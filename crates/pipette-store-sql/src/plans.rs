//! Plans on SQL: a manifest is a `plans` row plus a `plan_jobs` row per job. A
//! marker's expiry is the `ExpiresAt` display string, truncated to the second
//! as the object key is.

use std::collections::{BTreeMap, HashMap};
use std::marker::PhantomData;
use std::num::NonZeroUsize;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::FromRow;
use sqlx::postgres::PgRow;

use pipette_core::plan::{PlanHeader, PlanManifest, PlanNotes, PlanSource, PlanSummary};
use pipette_plan_types::{
    Answered, CampaignId, ExpiresAt, JobId, PlanId, RestoredVariant, RevisionChange,
};
use pipette_store::{
    CellResult, CellScope, ContentKeyCas, ContentKeyClaim, ContentKeyRecord, ContentKeyVersion,
    CreateOutcome, JobPlanMarker, Listing, PLANS_PREFIX, PendingSourceMarker, PlanFilter,
    PlanHeaderPage, PlanListing, PlanPage, PlanStore, PlanSummaryListing, UnreadableObjectError,
    is_content_digest,
};

use crate::ClaimVanishedError;
use crate::db::SqlDatabase;
use crate::listing;
use crate::listing::{Keyspace, Rows};
use crate::retry::retry;

const PLANS: Keyspace = Keyspace {
    table: "plans",
    id_column: "plan_id",
    report_prefix: PLANS_PREFIX,
};

#[derive(Clone, Debug)]
pub(crate) struct SqlPlanStore {
    db: SqlDatabase,
}

impl SqlPlanStore {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }

    /// One statement for the ids in `$1`. A row that does not parse is
    /// reported, as a listing reports it; a failed statement fails the read.
    async fn read_batch<R, T, E>(
        &self,
        op: &'static str,
        sql: &'static str,
        plan_ids: &[PlanId],
        parse: impl Fn(R) -> (String, Result<T, E>),
    ) -> anyhow::Result<Listing<T>>
    where
        R: for<'r> FromRow<'r, PgRow> + Send + Unpin,
        E: std::fmt::Display,
    {
        if plan_ids.is_empty() {
            return Ok(Listing::default());
        }
        let ids: Vec<&str> = plan_ids.iter().map(PlanId::as_str).collect();
        let rows: Vec<R> = self
            .db
            .fetch_all(op, || sqlx::query_as(sql).bind(ids.as_slice()))
            .await?;
        Ok(rows
            .into_iter()
            .fold(Listing::default(), |mut listing, row| {
                match parse(row) {
                    (_, Ok(item)) => listing.items.push(item),
                    (plan_id, Err(e)) => listing.unreadable.push(UnreadableObjectError {
                        key: PLANS.report_key(&plan_id),
                        reason: e.to_string(),
                    }),
                }
                listing
            }))
    }
}

/// A racing duplicate fails with a conflict, and its retry then finds the
/// winner's row and changes none.
fn create_outcome(rows: u64) -> CreateOutcome {
    if rows == 0 {
        CreateOutcome::AlreadyExists
    } else {
        CreateOutcome::Created
    }
}

/// `None`, with a warning, for a row that no trait write made.
fn plan_id_of(raw: &str, what: &'static str) -> Option<PlanId> {
    PlanId::try_new(raw)
        .inspect_err(|e| tracing::warn!(key = %raw, error = %e, "skipping unparseable {what}"))
        .ok()
}

fn marker_of(plan_id: &str, expires_at: &str) -> Option<(PlanId, ExpiresAt)> {
    let parsed = PlanId::try_new(plan_id)
        .map_err(anyhow::Error::from)
        .and_then(|plan| Ok((plan, expires_at.parse::<ExpiresAt>()?)));
    parsed
        .inspect_err(|e| {
            tracing::warn!(key = %format!("{plan_id}.{expires_at}"), error = %e,
                "skipping unparseable job-plans marker");
        })
        .ok()
}

/// `None` for the token of another backend. The caller reads `None` as a lost
/// swap.
fn revision_of(version: &ContentKeyVersion) -> Option<i64> {
    match version {
        ContentKeyVersion::Revision(revision) => Some(*revision),
        ContentKeyVersion::Digest(_) | ContentKeyVersion::Object { .. } => None,
    }
}

/// The revision is random, so a digest that a caller deletes and claims again
/// never matches its earlier tokens.
fn first_revision() -> i64 {
    let (high, low) = uuid::Uuid::new_v4().as_u64_pair();
    // 62 bits keep the value positive and far from overflow, however often a
    // write increments it.
    i64::try_from((high ^ low) >> 2).unwrap_or(1)
}

fn content_key_entry(
    digest: &str,
    record: &str,
    revision: i64,
) -> anyhow::Result<(ContentKeyRecord, ContentKeyVersion)> {
    let record = serde_json::from_str(record)
        .with_context(|| format!("content-keys/v1/{digest} is not a content-key record"))?;
    Ok((record, ContentKeyVersion::Revision(revision)))
}

enum Claim {
    Created,
    /// `None` only if the refusing snapshot held no row, which `REPEATABLE
    /// READ` rules out: a later commit fails the insert with a conflict.
    Taken(Option<(String, i64)>),
}

/// A `plans` row, without its jobs.
macro_rules! header_select {
    () => {
        "SELECT p.plan_id, p.status, p.campaign_id, p.submitted_by, p.note,
                p.change::text AS change, p.body
         FROM plans p"
    };
}

/// A `plans` row with its `plan_jobs` rows. One statement reads both, so a read
/// sees one snapshot of the manifest.
macro_rules! plan_select {
    () => {
        "SELECT p.plan_id, p.status, p.campaign_id, p.submitted_by, p.body,
                (SELECT coalesce(json_agg(json_build_array(
                            j.job_id, j.ordinal, j.content_key, j.rerun_of, j.expired,
                            j.outcome)
                        ORDER BY j.ordinal NULLS LAST, j.job_id), '[]')::text
                 FROM plan_jobs j WHERE j.plan_id = p.plan_id) AS jobs
         FROM plans p"
    };
}

/// The [`PlanSummary`] fields, built in the query so neither body nor jobs
/// leave the database. `summary` is NULL for a non-object body, which the cast
/// would otherwise fail the statement on.
const SUMMARY_SELECT: &str = "
    SELECT p.plan_id,
           CASE WHEN p.body IS JSON OBJECT THEN (
               SELECT json_build_object(
                          'plan_id', p.plan_id, 'status', p.status,
                          'campaign_id', p.campaign_id, 'submitted_by', p.submitted_by,
                          'note', p.note,
                          'created_at', b->'created_at',
                          'counts', b->'progress_snapshot'->'counts')::text
               FROM (SELECT p.body::json AS b) body)
           END AS summary,
           p.change::text AS change
    FROM plans p WHERE p.plan_id = ANY($1)";

/// A [`SUMMARY_SELECT`] row.
#[derive(FromRow)]
struct SummaryRow {
    plan_id: String,
    summary: Option<String>,
    change: Option<String>,
}

/// Plans after id `$1` that the filter `$2` to `$6` admits, in `{id}.json`
/// order, at most `$7`. The reach `$5`/`$6` must read as `PlanReach::admits`.
macro_rules! matching_plans {
    () => {
        " WHERE ($1::text IS NULL OR (p.plan_id || '.json') > ($1::text || '.json'))
            AND ($2::text IS NULL OR p.status = $2)
            AND ($3::text IS NULL OR p.campaign_id = $3)
            AND ($4::text IS NULL OR p.submitted_by = $4)
            AND ($5::text[] IS NULL
                 OR p.campaign_id = ANY($5)
                 OR (p.campaign_id IS NULL AND p.submitted_by = $6::text))
          ORDER BY (p.plan_id || '.json')
          LIMIT $7"
    };
}

/// The `plans` columns that make a manifest's header.
#[derive(FromRow)]
struct PlanColumns {
    plan_id: String,
    status: String,
    campaign_id: Option<String>,
    submitted_by: Option<String>,
    body: String,
}

/// A plan as [`header_select!`] reads it: its columns and its notes.
#[derive(FromRow)]
struct HeaderRow {
    #[sqlx(flatten)]
    columns: PlanColumns,
    note: Option<String>,
    change: Option<String>,
}

/// A manifest as [`plan_select!`] reads it.
#[derive(FromRow)]
struct PlanRow {
    #[sqlx(flatten)]
    columns: PlanColumns,
    /// A JSON array with one `[job_id, ordinal, content_key, rerun_of,
    /// expired, outcome]` array for each `plan_jobs` row.
    jobs: String,
}

/// One element of [`PlanRow::jobs`].
type JobEntry = (
    String,
    Option<i32>,
    Option<String>,
    Option<String>,
    Option<Value>,
    Option<String>,
);

/// The `plan_jobs` columns of one job.
#[derive(Default)]
struct JobRow {
    ordinal: Option<i32>,
    content_key: Option<String>,
    rerun_of: Option<String>,
    /// The `ExpiredJob` as JSON text.
    expired: Option<String>,
    /// The `SettledOutcome` as its serialized name.
    outcome: Option<String>,
}

/// The `plan_jobs` rows of one plan, as one array for each column.
#[derive(Default)]
struct JobColumns {
    job_ids: Vec<String>,
    ordinals: Vec<Option<i32>>,
    content_keys: Vec<Option<String>>,
    rerun_of: Vec<Option<String>>,
    expired: Vec<Option<String>>,
    outcomes: Vec<Option<String>>,
}

/// A manifest as `put_plan` writes it.
struct StoredPlan {
    status: String,
    campaign_id: Option<String>,
    submitted_by: Option<String>,
    body: String,
    jobs: JobColumns,
}

/// A manifest that does not split into its rows.
#[derive(Debug, thiserror::Error)]
enum SplitError {
    #[error("the manifest's {field} is not a string: {value}")]
    NotText { field: String, value: Value },
    #[error("plan {plan_id} does not serialize to a JSON object")]
    NotAnObject { plan_id: PlanId },
    #[error("plan {plan_id} serializes no status")]
    NoStatus { plan_id: PlanId },
    #[error("plan {plan_id} names job {job_id} twice")]
    JobTwice { plan_id: PlanId, job_id: JobId },
}

/// Remove `field`, which a serialized manifest writes as a string or omits.
fn take_text(body: &mut Map<String, Value>, field: &str) -> Result<Option<String>, SplitError> {
    match body.remove(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(value) => Err(SplitError::NotText {
            field: field.to_owned(),
            value,
        }),
    }
}

/// Split `manifest` into its `plans` columns, body and `plan_jobs` rows.
fn split(manifest: &PlanManifest) -> anyhow::Result<StoredPlan> {
    let plan_id = &manifest.plan_id;
    let Value::Object(mut body) = serde_json::to_value(manifest)? else {
        return Err(SplitError::NotAnObject {
            plan_id: plan_id.clone(),
        }
        .into());
    };
    let status = take_text(&mut body, "status")?.ok_or_else(|| SplitError::NoStatus {
        plan_id: plan_id.clone(),
    })?;
    let campaign_id = take_text(&mut body, "campaign_id")?;
    let submitted_by = take_text(&mut body, "submitted_by")?;
    body.retain(|field, _| {
        ![
            "plan_id",
            "job_ids",
            "content_keys",
            "rerun_of",
            "expired_jobs",
            "settled_jobs",
        ]
        .contains(&field.as_str())
    });

    let mut jobs: BTreeMap<&JobId, JobRow> = BTreeMap::new();
    manifest.job_ids.iter().enumerate().try_for_each(
        |(ordinal, job_id)| -> anyhow::Result<()> {
            let row = jobs.entry(job_id).or_default();
            if row.ordinal.is_some() {
                return Err(SplitError::JobTwice {
                    plan_id: plan_id.clone(),
                    job_id: job_id.clone(),
                }
                .into());
            }
            row.ordinal = Some(i32::try_from(ordinal)?);
            Ok(())
        },
    )?;
    manifest
        .content_keys
        .iter()
        .for_each(|(job_id, content_key)| {
            jobs.entry(job_id).or_default().content_key = Some(content_key.clone());
        });
    manifest.rerun_of.iter().for_each(|(job_id, rerun_of)| {
        jobs.entry(job_id).or_default().rerun_of = Some(rerun_of.as_str().to_string());
    });
    manifest
        .expired_jobs
        .iter()
        .try_for_each(|(job_id, expired)| -> anyhow::Result<()> {
            jobs.entry(job_id).or_default().expired = Some(serde_json::to_string(expired)?);
            Ok(())
        })?;
    manifest.settled_jobs.iter().for_each(|(job_id, outcome)| {
        jobs.entry(job_id).or_default().outcome = Some(outcome.label().to_owned());
    });
    let jobs = jobs
        .into_iter()
        .fold(JobColumns::default(), |mut columns, (job_id, row)| {
            columns.job_ids.push(job_id.as_str().to_string());
            columns.ordinals.push(row.ordinal);
            columns.content_keys.push(row.content_key);
            columns.rerun_of.push(row.rerun_of);
            columns.expired.push(row.expired);
            columns.outcomes.push(row.outcome);
            columns
        });

    Ok(StoredPlan {
        status,
        campaign_id,
        submitted_by,
        body: serde_json::to_string(&body)?,
        jobs,
    })
}

/// A stored plan that does not make a manifest.
#[derive(Debug, thiserror::Error)]
enum AssembleError {
    #[error("body: {0}")]
    Body(#[source] serde_json::Error),
    #[error("plan_jobs: {0}")]
    PlanJobs(#[source] serde_json::Error),
    #[error(transparent)]
    Manifest(serde_json::Error),
}

/// A change in the shape earlier builds wrote to the `change` column, kept
/// whatever the wire calls its fields so a rollback reads what this build writes.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredChange {
    Variants {
        removed: usize,
        added: usize,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        carry_earlier: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inherit_from: Vec<PlanId>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        restored: Vec<StoredRestored>,
    },
    Document {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        carry_earlier: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        inherit_from: Vec<PlanId>,
    },
    Rerun {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answered: Option<Answered>,
        jobs: usize,
    },
    Recalculate {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all_evals: bool,
        jobs: usize,
    },
    Revert {
        to: PlanId,
    },
}

#[derive(Serialize, Deserialize)]
struct StoredRestored {
    plan: PlanId,
    index: usize,
}

impl From<RevisionChange> for StoredChange {
    fn from(change: RevisionChange) -> Self {
        match change {
            RevisionChange::Variants {
                removed_variant_count,
                added_variant_count,
                carry_earlier,
                inherit_from,
                restored,
            } => Self::Variants {
                removed: removed_variant_count,
                added: added_variant_count,
                carry_earlier,
                inherit_from,
                restored: restored
                    .into_iter()
                    .map(|variant| StoredRestored {
                        plan: variant.plan_id,
                        index: variant.variant_index,
                    })
                    .collect(),
            },
            RevisionChange::Document {
                carry_earlier,
                inherit_from,
            } => Self::Document {
                carry_earlier,
                inherit_from,
            },
            RevisionChange::Rerun {
                answered,
                job_count,
            } => Self::Rerun {
                answered,
                jobs: job_count,
            },
            RevisionChange::Recalculate {
                all_evals,
                job_count,
            } => Self::Recalculate {
                all_evals,
                jobs: job_count,
            },
            RevisionChange::Revert { to_plan_id } => Self::Revert { to: to_plan_id },
        }
    }
}

impl From<StoredChange> for RevisionChange {
    fn from(change: StoredChange) -> Self {
        match change {
            StoredChange::Variants {
                removed,
                added,
                carry_earlier,
                inherit_from,
                restored,
            } => Self::Variants {
                removed_variant_count: removed,
                added_variant_count: added,
                carry_earlier,
                inherit_from,
                restored: restored
                    .into_iter()
                    .map(|variant| RestoredVariant {
                        plan_id: variant.plan,
                        variant_index: variant.index,
                    })
                    .collect(),
            },
            StoredChange::Document {
                carry_earlier,
                inherit_from,
            } => Self::Document {
                carry_earlier,
                inherit_from,
            },
            StoredChange::Rerun { answered, jobs } => Self::Rerun {
                answered,
                job_count: jobs,
            },
            StoredChange::Recalculate { all_evals, jobs } => Self::Recalculate {
                all_evals,
                job_count: jobs,
            },
            StoredChange::Revert { to } => Self::Revert { to_plan_id: to },
        }
    }
}

fn write_change(change: &RevisionChange) -> serde_json::Result<String> {
    serde_json::to_string(&StoredChange::from(change.clone()))
}

/// The `change` column, read as text. A change this build cannot read, such as a
/// `kind` from a later build, reads as none, so the plan and its note still read.
fn read_change(plan_id: &str, text: Option<String>) -> Option<RevisionChange> {
    serde_json::from_str::<StoredChange>(&text?)
        .map(RevisionChange::from)
        .inspect_err(|e| {
            tracing::warn!(plan_id, error = %e, "plan change unreadable, read as none");
        })
        .ok()
}

/// A plan's notes from its `note` column and its `change` column's text.
fn read_notes(plan_id: &str, note: Option<String>, change: Option<String>) -> PlanNotes {
    PlanNotes {
        note,
        change: read_change(plan_id, change),
    }
}

/// A stored plan's body with its `plans` columns put back.
fn header_body(row: PlanColumns) -> Result<Map<String, Value>, AssembleError> {
    let mut body: Map<String, Value> =
        serde_json::from_str(&row.body).map_err(AssembleError::Body)?;
    body.insert("plan_id".into(), row.plan_id.into());
    body.insert("status".into(), row.status.into());
    if let Some(campaign_id) = row.campaign_id {
        body.insert("campaign_id".into(), campaign_id.into());
    }
    if let Some(submitted_by) = row.submitted_by {
        body.insert("submitted_by".into(), submitted_by.into());
    }
    Ok(body)
}

/// Rebuild a header from what [`header_select!`] reads.
fn assemble_header(row: HeaderRow) -> Result<PlanHeader, AssembleError> {
    let header: PlanHeader = serde_json::from_value(Value::Object(header_body(row.columns)?))
        .map_err(AssembleError::Manifest)?;
    Ok(PlanHeader {
        note: row.note,
        change: read_change(header.plan_id.as_str(), row.change),
        ..header
    })
}

/// Rebuild a manifest from what [`plan_select!`] reads.
fn assemble(row: PlanRow) -> Result<PlanManifest, AssembleError> {
    let mut body = header_body(row.columns)?;
    let jobs: Vec<JobEntry> = serde_json::from_str(&row.jobs).map_err(AssembleError::PlanJobs)?;

    // In `ordinal` order, so the members come in the order of `job_ids`.
    let job_ids: Vec<Value> = jobs
        .iter()
        .filter(|(_, ordinal, ..)| ordinal.is_some())
        .map(|(job_id, ..)| Value::from(job_id.as_str()))
        .collect();
    let content_keys: Map<String, Value> = jobs
        .iter()
        .filter_map(|(job_id, _, content_key, ..)| {
            Some((job_id.clone(), content_key.as_deref()?.into()))
        })
        .collect();
    let rerun_of: Map<String, Value> = jobs
        .iter()
        .filter_map(|(job_id, _, _, rerun, ..)| Some((job_id.clone(), rerun.as_deref()?.into())))
        .collect();
    let settled_jobs: Map<String, Value> = jobs
        .iter()
        .filter_map(|(job_id, .., outcome)| Some((job_id.clone(), outcome.as_deref()?.into())))
        .collect();
    let expired_jobs: Map<String, Value> = jobs
        .into_iter()
        .filter_map(|(job_id, _, _, _, expired, _)| Some((job_id, expired?)))
        .collect();
    body.insert("job_ids".into(), Value::Array(job_ids));
    body.insert("content_keys".into(), Value::Object(content_keys));
    body.insert("rerun_of".into(), Value::Object(rerun_of));
    body.insert("expired_jobs".into(), Value::Object(expired_jobs));
    body.insert("settled_jobs".into(), Value::Object(settled_jobs));
    serde_json::from_value(Value::Object(body)).map_err(AssembleError::Manifest)
}

/// A `plans` row that a filtered listing reads.
trait ListedRow: for<'r> FromRow<'r, PgRow> + Send + Unpin {
    /// The statement that reads the row, filtered by [`matching_plans!`].
    const LIST: &'static str;

    fn plan_id(&self) -> &str;
}

impl ListedRow for PlanRow {
    const LIST: &'static str = concat!(plan_select!(), matching_plans!());

    fn plan_id(&self) -> &str {
        &self.columns.plan_id
    }
}

impl ListedRow for HeaderRow {
    const LIST: &'static str = concat!(header_select!(), matching_plans!());

    fn plan_id(&self) -> &str {
        &self.columns.plan_id
    }
}

/// The plans that a filter admits, as [`listing::page_rows`] reads them into
/// `R`.
struct MatchingPlans<'a, R> {
    db: &'a SqlDatabase,
    status: Option<String>,
    campaign_id: Option<String>,
    submitted_by: Option<String>,
    reach_campaigns: Option<Vec<String>>,
    reach_filer: Option<String>,
    row: PhantomData<fn() -> R>,
}

impl<'a, R> MatchingPlans<'a, R> {
    fn new(db: &'a SqlDatabase, filter: &PlanFilter) -> Self {
        Self {
            db,
            status: filter.status.map(|s| s.label().to_owned()),
            campaign_id: filter.campaign_id.as_ref().map(|id| id.as_str().to_owned()),
            submitted_by: filter
                .submitted_by
                .as_ref()
                .map(|id| id.as_str().to_owned()),
            reach_campaigns: filter.reach.as_ref().map(|reach| {
                reach
                    .campaigns
                    .iter()
                    .map(|id| id.as_str().to_owned())
                    .collect()
            }),
            // A `NULL` filer equals no `submitted_by`, so it reads no
            // campaignless plan, as `PlanReach::admits` reads none.
            reach_filer: filter
                .reach
                .as_ref()
                .and_then(|reach| reach.filer.as_ref())
                .map(|filer| filer.as_str().to_owned()),
            row: PhantomData,
        }
    }

    /// `query` with the cursor and the filter bound, and `limit` last.
    fn bind<'q, T>(
        &'q self,
        query: PgQueryAs<'q, T>,
        after: Option<&'q str>,
        limit: i64,
    ) -> PgQueryAs<'q, T> {
        query
            .bind(after)
            .bind(self.status.as_deref())
            .bind(self.campaign_id.as_deref())
            .bind(self.submitted_by.as_deref())
            .bind(self.reach_campaigns.as_deref())
            .bind(self.reach_filer.as_deref())
            .bind(limit)
    }
}

type PgQueryAs<'q, T> = sqlx::query::QueryAs<'q, sqlx::Postgres, T, sqlx::postgres::PgArguments>;

#[async_trait]
impl<R: ListedRow> Rows for MatchingPlans<'_, R> {
    type Row = R;

    async fn rows_after(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<(String, R)>> {
        let rows: Vec<R> = self
            .db
            .fetch_all("list_plans", || {
                self.bind(sqlx::query_as(R::LIST), after, limit)
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.plan_id().to_owned(), row))
            .collect())
    }

    async fn ids_after(&self, after: Option<&str>, limit: i64) -> anyhow::Result<Vec<String>> {
        let ids: Vec<(String,)> = self
            .db
            .fetch_all("list_plans", || {
                self.bind(
                    sqlx::query_as(concat!("SELECT p.plan_id FROM plans p", matching_plans!())),
                    after,
                    limit,
                )
            })
            .await?;
        Ok(ids.into_iter().map(|(id,)| id).collect())
    }
}

#[async_trait]
impl PlanStore for SqlPlanStore {
    async fn put_plan(&self, manifest: &PlanManifest) -> anyhow::Result<()> {
        let stored = split(manifest)?;
        let stored = &stored;
        let plan_id = manifest.plan_id.as_str();
        let db = &self.db;
        retry("put_plan", || {
            Box::pin(async move {
                let mut tx = db.begin().await?;
                // Write this row first: a racing writer of the same plan waits
                // on it and fails with a conflict, which the retry reruns.
                sqlx::query(
                    "INSERT INTO plans (plan_id, status, campaign_id, submitted_by, body)
                     VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT (plan_id) DO UPDATE SET
                         status = EXCLUDED.status,
                         campaign_id = EXCLUDED.campaign_id,
                         submitted_by = EXCLUDED.submitted_by,
                         body = EXCLUDED.body",
                )
                .bind(plan_id)
                .bind(stored.status.as_str())
                .bind(stored.campaign_id.as_deref())
                .bind(stored.submitted_by.as_deref())
                .bind(stored.body.as_str())
                .execute(&mut *tx)
                .await?;
                let jobs = &stored.jobs;
                sqlx::query("DELETE FROM plan_jobs WHERE plan_id = $1 AND NOT (job_id = ANY($2))")
                    .bind(plan_id)
                    .bind(jobs.job_ids.as_slice())
                    .execute(&mut *tx)
                    .await?;
                // Unchanged jobs keep their rows. `job_ids` keep their order
                // for the plan's life, so no row moves onto another's ordinal.
                // An older build's statement omits `outcome`, so its write
                // keeps the row's.
                sqlx::query(
                    "INSERT INTO plan_jobs
                         (plan_id, job_id, ordinal, content_key, rerun_of, expired, outcome)
                     SELECT $1, job.job_id, job.ordinal, job.content_key, job.rerun_of,
                            job.expired::jsonb, job.outcome
                     FROM unnest($2::text[], $3::integer[], $4::text[], $5::text[], $6::text[],
                                 $7::text[])
                         AS job (job_id, ordinal, content_key, rerun_of, expired, outcome)
                     ON CONFLICT (plan_id, job_id) DO UPDATE SET
                         ordinal = EXCLUDED.ordinal,
                         content_key = EXCLUDED.content_key,
                         rerun_of = EXCLUDED.rerun_of,
                         expired = EXCLUDED.expired,
                         outcome = EXCLUDED.outcome
                     WHERE (plan_jobs.ordinal, plan_jobs.content_key, plan_jobs.rerun_of,
                            plan_jobs.expired, plan_jobs.outcome)
                         IS DISTINCT FROM (EXCLUDED.ordinal, EXCLUDED.content_key,
                            EXCLUDED.rerun_of, EXCLUDED.expired, EXCLUDED.outcome)",
                )
                .bind(plan_id)
                .bind(jobs.job_ids.as_slice())
                .bind(jobs.ordinals.as_slice())
                .bind(jobs.content_keys.as_slice())
                .bind(jobs.rerun_of.as_slice())
                .bind(jobs.expired.as_slice())
                .bind(jobs.outcomes.as_slice())
                .execute(&mut *tx)
                .await?;
                tx.commit().await
            })
        })
        .await?;
        Ok(())
    }

    async fn get_plan_notes_of(
        &self,
        plan_ids: &[PlanId],
    ) -> anyhow::Result<HashMap<PlanId, PlanNotes>> {
        let ids: Vec<&str> = plan_ids.iter().map(PlanId::as_str).collect();
        let rows: Vec<(String, Option<String>, Option<String>)> = self
            .db
            .fetch_all("get_plan_notes_of", || {
                sqlx::query_as(
                    "SELECT plan_id, note, change::text FROM plans
                     WHERE plan_id = ANY($1) AND (note IS NOT NULL OR change IS NOT NULL)",
                )
                .bind(ids.as_slice())
            })
            .await?;
        rows.into_iter()
            .map(|(plan_id, note, change)| {
                let notes = read_notes(&plan_id, note, change);
                Ok((PlanId::try_new(plan_id)?, notes))
            })
            .collect()
    }

    async fn put_plan_notes(&self, plan_id: &PlanId, notes: &PlanNotes) -> anyhow::Result<bool> {
        let change = notes.change.as_ref().map(write_change).transpose()?;
        let written = self
            .db
            .execute("put_plan_notes", || {
                sqlx::query("UPDATE plans SET note = $2, change = $3::jsonb WHERE plan_id = $1")
                    .bind(plan_id.as_str())
                    .bind(notes.note.as_deref())
                    .bind(change.as_deref())
            })
            .await?;
        Ok(written > 0)
    }

    async fn set_plan_note(&self, plan_id: &PlanId, note: Option<&str>) -> anyhow::Result<bool> {
        let written = self
            .db
            .execute("set_plan_note", || {
                sqlx::query("UPDATE plans SET note = $2 WHERE plan_id = $1")
                    .bind(plan_id.as_str())
                    .bind(note)
            })
            .await?;
        Ok(written > 0)
    }

    async fn get_plan(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanManifest>> {
        let row: Option<PlanRow> = self
            .db
            .fetch_optional("get_plan", || {
                sqlx::query_as(concat!(plan_select!(), " WHERE p.plan_id = $1"))
                    .bind(plan_id.as_str())
            })
            .await?;
        row.map(|row| {
            assemble(row).map_err(|e| {
                UnreadableObjectError {
                    key: PLANS.report_key(plan_id.as_str()),
                    reason: e.to_string(),
                }
                .into()
            })
        })
        .transpose()
    }

    /// One indexed query; a row that does not assemble is left for the walk's
    /// own read, which reports it.
    async fn get_campaign_plans(
        &self,
        campaign_id: &CampaignId,
    ) -> anyhow::Result<Option<Vec<PlanManifest>>> {
        let rows: Vec<PlanRow> = self
            .db
            .fetch_all("get_campaign_plans", || {
                sqlx::query_as(concat!(plan_select!(), " WHERE p.campaign_id = $1"))
                    .bind(campaign_id.as_str())
            })
            .await?;
        Ok(Some(
            rows.into_iter()
                .filter_map(|row| assemble(row).ok())
                .collect(),
        ))
    }

    async fn get_plans(&self, plan_ids: &[PlanId]) -> anyhow::Result<PlanListing> {
        self.read_batch(
            "get_plans",
            concat!(plan_select!(), " WHERE p.plan_id = ANY($1)"),
            plan_ids,
            |row: PlanRow| (row.columns.plan_id.clone(), assemble(row)),
        )
        .await
    }

    /// Reads the summary fields alone, so a summary does not hang on the rest
    /// of the manifest reading.
    async fn get_plan_summaries(&self, plan_ids: &[PlanId]) -> anyhow::Result<PlanSummaryListing> {
        self.read_batch(
            "get_plan_summaries",
            SUMMARY_SELECT,
            plan_ids,
            |row: SummaryRow| {
                let summary = row
                    .summary
                    .ok_or_else(|| "body is not a JSON object".to_owned())
                    .and_then(|summary| {
                        serde_json::from_str::<PlanSummary>(&summary).map_err(|e| e.to_string())
                    })
                    .map(|parsed| PlanSummary {
                        change: read_change(&row.plan_id, row.change),
                        ..parsed
                    });
                (row.plan_id, summary)
            },
        )
        .await
    }

    /// The filter runs on the `plans` columns, so a plan whose body does not
    /// read is reported only if its columns match.
    async fn list_matching_plans(
        &self,
        filter: &PlanFilter,
        after: Option<&PlanId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<PlanPage> {
        listing::page_rows(
            &MatchingPlans::<PlanRow>::new(&self.db, filter),
            PLANS,
            after,
            limit,
            PlanId::try_new,
            |_, row| assemble(row),
            |_| true,
        )
        .await
    }

    /// Reads no `plan_jobs` row.
    async fn list_matching_plan_headers(
        &self,
        filter: &PlanFilter,
        after: Option<&PlanId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<PlanHeaderPage> {
        listing::page_rows(
            &MatchingPlans::<HeaderRow>::new(&self.db, filter),
            PLANS,
            after,
            limit,
            PlanId::try_new,
            |_, row| assemble_header(row),
            |_| true,
        )
        .await
    }

    async fn write_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        self.db
            .execute("write_cancel_marker", || {
                sqlx::query(
                    "INSERT INTO cancelled_plans (plan_id) VALUES ($1)
                     ON CONFLICT (plan_id) DO NOTHING",
                )
                .bind(plan_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn has_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<bool> {
        let row: Option<(i32,)> = self
            .db
            .fetch_optional("has_cancel_marker", || {
                sqlx::query_as("SELECT 1 FROM cancelled_plans WHERE plan_id = $1")
                    .bind(plan_id.as_str())
            })
            .await?;
        Ok(row.is_some())
    }

    async fn list_cancel_markers(&self) -> anyhow::Result<Vec<PlanId>> {
        let rows: Vec<(String,)> = self
            .db
            .fetch_all("list_cancel_markers", || {
                sqlx::query_as("SELECT plan_id FROM cancelled_plans ORDER BY plan_id")
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(raw,)| plan_id_of(&raw, "cancel marker"))
            .collect())
    }

    async fn delete_cancel_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        self.db
            .execute("delete_cancel_marker", || {
                sqlx::query("DELETE FROM cancelled_plans WHERE plan_id = $1").bind(plan_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn get_content_key(
        &self,
        digest: &str,
    ) -> anyhow::Result<Option<(ContentKeyRecord, ContentKeyVersion)>> {
        let row: Option<(String, i64)> = self
            .db
            .fetch_optional("get_content_key", || {
                sqlx::query_as("SELECT record, revision FROM content_keys WHERE digest = $1")
                    .bind(digest)
            })
            .await?;
        row.map(|(record, revision)| content_key_entry(digest, &record, revision))
            .transpose()
    }

    async fn create_content_key(
        &self,
        digest: &str,
        record: &ContentKeyRecord,
    ) -> anyhow::Result<ContentKeyClaim> {
        let body = serde_json::to_string_pretty(record)?;
        let body = body.as_str();
        let db = &self.db;
        let revision = first_revision();
        // The insert and the read share a snapshot, so a refused insert reads
        // its incumbent.
        let claim = retry("create_content_key", || {
            Box::pin(async move {
                let mut tx = db.begin().await?;
                let inserted = sqlx::query(
                    "INSERT INTO content_keys (digest, record, revision) VALUES ($1, $2, $3)
                     ON CONFLICT (digest) DO NOTHING",
                )
                .bind(digest)
                .bind(body)
                .bind(revision)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                let claim = if inserted == 0 {
                    Claim::Taken(
                        sqlx::query_as(
                            "SELECT record, revision FROM content_keys WHERE digest = $1",
                        )
                        .bind(digest)
                        .fetch_optional(&mut *tx)
                        .await?,
                    )
                } else {
                    Claim::Created
                };
                tx.commit().await?;
                Ok(claim)
            })
        })
        .await?;
        match claim {
            Claim::Created => Ok(ContentKeyClaim::Created),
            Claim::Taken(Some((record, revision))) => {
                let (record, version) = content_key_entry(digest, &record, revision)?;
                Ok(ContentKeyClaim::AlreadyExists(record, version))
            }
            Claim::Taken(None) => Err(ClaimVanishedError {
                claim: "content-key",
                name: digest.to_owned(),
            }
            .into()),
        }
    }

    async fn replace_content_key(
        &self,
        digest: &str,
        version: &ContentKeyVersion,
        record: &ContentKeyRecord,
    ) -> anyhow::Result<ContentKeyCas> {
        let Some(revision) = revision_of(version) else {
            return Ok(ContentKeyCas::Lost);
        };
        let body = serde_json::to_string_pretty(record)?;
        // A conflicting writer increments the revision first. The retry then
        // matches no row and reports the swap as lost.
        let replaced = self
            .db
            .execute("replace_content_key", || {
                sqlx::query(
                    "UPDATE content_keys SET record = $3, revision = revision + 1
                     WHERE digest = $1 AND revision = $2",
                )
                .bind(digest)
                .bind(revision)
                .bind(body.as_str())
            })
            .await?;
        Ok(if replaced == 0 {
            ContentKeyCas::Lost
        } else {
            ContentKeyCas::Applied
        })
    }

    async fn delete_content_key(
        &self,
        digest: &str,
        version: &ContentKeyVersion,
    ) -> anyhow::Result<ContentKeyCas> {
        let Some(revision) = revision_of(version) else {
            return Ok(ContentKeyCas::Lost);
        };
        // One conditional statement checks and deletes, so no replace lands
        // between them.
        let deleted = self
            .db
            .execute("delete_content_key", || {
                sqlx::query("DELETE FROM content_keys WHERE digest = $1 AND revision = $2")
                    .bind(digest)
                    .bind(revision)
            })
            .await?;
        Ok(if deleted == 0 {
            ContentKeyCas::Lost
        } else {
            ContentKeyCas::Applied
        })
    }

    async fn list_content_keys(
        &self,
    ) -> anyhow::Result<Vec<(String, ContentKeyRecord, ContentKeyVersion)>> {
        let rows: Vec<(String, String, i64)> = self
            .db
            .fetch_all("list_content_keys", || {
                sqlx::query_as("SELECT digest, record, revision FROM content_keys ORDER BY digest")
            })
            .await?;
        rows.into_iter()
            .filter(|(digest, _, _)| is_content_digest(digest))
            .map(|(digest, record, revision)| {
                let (record, version) = content_key_entry(&digest, &record, revision)?;
                Ok((digest, record, version))
            })
            .collect()
    }

    async fn write_job_plan_marker(
        &self,
        job_id: &JobId,
        plan_id: &PlanId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        let expires_at = expires_at.to_string();
        // A rewrite refreshes `written_at`, as an HTTP PUT request refreshes
        // the modification time of an object.
        self.db
            .execute("write_job_plan_marker", || {
                sqlx::query(
                    "INSERT INTO job_plans (job_id, plan_id, expires_at, written_at)
                     VALUES ($1, $2, $3, now())
                     ON CONFLICT (job_id, plan_id, expires_at)
                     DO UPDATE SET written_at = EXCLUDED.written_at",
                )
                .bind(job_id.as_str())
                .bind(plan_id.as_str())
                .bind(expires_at.as_str())
            })
            .await?;
        Ok(())
    }

    async fn list_job_plan_markers(
        &self,
        job_id: &JobId,
    ) -> anyhow::Result<Vec<(PlanId, ExpiresAt)>> {
        // In the order of the `{plan_id}.{expires_at}` leaves an object listing
        // returns.
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("list_job_plan_markers", || {
                sqlx::query_as(
                    "SELECT plan_id, expires_at FROM job_plans WHERE job_id = $1
                     ORDER BY plan_id || '.' || expires_at",
                )
                .bind(job_id.as_str())
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(plan_id, expires_at)| marker_of(&plan_id, &expires_at))
            .collect())
    }

    async fn list_all_job_plan_markers(&self) -> anyhow::Result<Vec<JobPlanMarker>> {
        let rows: Vec<(String, String, String, DateTime<Utc>)> = self
            .db
            .fetch_all("list_all_job_plan_markers", || {
                sqlx::query_as(
                    "SELECT job_id, plan_id, expires_at, written_at FROM job_plans
                     ORDER BY job_id || '/' || plan_id || '.' || expires_at",
                )
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(job_id, plan_id, expires_at, written_at)| {
                let job_id = JobId::try_new(job_id.as_str())
                    .inspect_err(|e| {
                        tracing::warn!(key = %job_id, error = %e,
                            "skipping unparseable job-plans marker");
                    })
                    .ok()?;
                let (plan_id, expires_at) = marker_of(&plan_id, &expires_at)?;
                Some(JobPlanMarker {
                    job_id,
                    plan_id,
                    expires_at,
                    written_at,
                })
            })
            .collect())
    }

    async fn delete_job_plan_marker(
        &self,
        job_id: &JobId,
        plan_id: &PlanId,
        expires_at: ExpiresAt,
    ) -> anyhow::Result<()> {
        let expires_at = expires_at.to_string();
        self.db
            .execute("delete_job_plan_marker", || {
                sqlx::query(
                    "DELETE FROM job_plans WHERE job_id = $1 AND plan_id = $2 AND expires_at = $3",
                )
                .bind(job_id.as_str())
                .bind(plan_id.as_str())
                .bind(expires_at.as_str())
            })
            .await?;
        Ok(())
    }

    async fn put_plan_source(
        &self,
        plan_id: &PlanId,
        source: &PlanSource,
    ) -> anyhow::Result<CreateOutcome> {
        if source.names_a_token() {
            anyhow::bail!(
                "refused to store the source of plan {plan_id}: it names an `auth_token`"
            );
        }
        let created = self
            .db
            .execute("put_plan_source", || {
                sqlx::query(
                    "INSERT INTO plan_sources (plan_id, source) VALUES ($1, $2)
                     ON CONFLICT (plan_id) DO NOTHING",
                )
                .bind(plan_id.as_str())
                .bind(source.text())
            })
            .await?;
        Ok(create_outcome(created))
    }

    async fn get_plan_source(&self, plan_id: &PlanId) -> anyhow::Result<Option<PlanSource>> {
        let row: Option<(String,)> = self
            .db
            .fetch_optional("get_plan_source", || {
                sqlx::query_as("SELECT source FROM plan_sources WHERE plan_id = $1")
                    .bind(plan_id.as_str())
            })
            .await?;
        Ok(row.map(|(text,)| PlanSource::new(text)).transpose()?)
    }

    async fn delete_plan_source(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        self.db
            .execute("delete_plan_source", || {
                sqlx::query("DELETE FROM plan_sources WHERE plan_id = $1").bind(plan_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn write_pending_source_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        // Garbage collection's age gate reads `written_at`; a rewrite refreshes
        // it.
        self.db
            .execute("write_pending_source_marker", || {
                sqlx::query(
                    "INSERT INTO plan_sources_pending (plan_id, written_at) VALUES ($1, now())
                     ON CONFLICT (plan_id) DO UPDATE SET written_at = EXCLUDED.written_at",
                )
                .bind(plan_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn list_pending_source_markers(&self) -> anyhow::Result<Vec<PendingSourceMarker>> {
        let rows: Vec<(String, DateTime<Utc>)> = self
            .db
            .fetch_all("list_pending_source_markers", || {
                sqlx::query_as(
                    "SELECT plan_id, written_at FROM plan_sources_pending ORDER BY plan_id",
                )
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(raw, written_at)| {
                Some(PendingSourceMarker {
                    plan_id: plan_id_of(&raw, "pending source marker")?,
                    written_at,
                })
            })
            .collect())
    }

    async fn delete_pending_source_marker(&self, plan_id: &PlanId) -> anyhow::Result<()> {
        self.db
            .execute("delete_pending_source_marker", || {
                sqlx::query("DELETE FROM plan_sources_pending WHERE plan_id = $1")
                    .bind(plan_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn put_job_source(
        &self,
        job_id: &JobId,
        body: &serde_json::Value,
    ) -> anyhow::Result<CreateOutcome> {
        // `to_string` writes the bytes `to_vec` does, key order included.
        let body = serde_json::to_string(body)?;
        let created = self
            .db
            .execute("put_job_source", || {
                sqlx::query(
                    "INSERT INTO job_sources (job_id, body) VALUES ($1, $2)
                     ON CONFLICT (job_id) DO NOTHING",
                )
                .bind(job_id.as_str())
                .bind(body.as_str())
            })
            .await?;
        Ok(create_outcome(created))
    }

    async fn get_job_source(&self, job_id: &JobId) -> anyhow::Result<Option<serde_json::Value>> {
        let row: Option<(String,)> = self
            .db
            .fetch_optional("get_job_source", || {
                sqlx::query_as("SELECT body FROM job_sources WHERE job_id = $1")
                    .bind(job_id.as_str())
            })
            .await?;
        let Some((body,)) = row else {
            return Ok(None);
        };
        serde_json::from_str(&body).map(Some).map_err(|e| {
            UnreadableObjectError {
                key: format!("job-sources/{job_id}.json"),
                reason: e.to_string(),
            }
            .into()
        })
    }

    /// One statement for every job, where the default makes one per job: a
    /// plan's page reads all of its sources at once.
    async fn get_job_sources(
        &self,
        job_ids: &[JobId],
    ) -> anyhow::Result<Listing<(JobId, serde_json::Value)>> {
        if job_ids.is_empty() {
            return Ok(Listing::default());
        }
        let asked: HashMap<&str, &JobId> = job_ids.iter().map(|id| (id.as_str(), id)).collect();
        let ids: Vec<&str> = asked.keys().copied().collect();
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("get_job_sources", || {
                sqlx::query_as("SELECT job_id, body FROM job_sources WHERE job_id = ANY($1)")
                    .bind(ids.as_slice())
            })
            .await?;
        Ok(rows
            .into_iter()
            .fold(Listing::default(), |mut listing, (job_id, body)| {
                // The statement answers only the ids it was given.
                let Some(&job_id) = asked.get(job_id.as_str()) else {
                    return listing;
                };
                match serde_json::from_str(&body) {
                    Ok(body) => listing.items.push((job_id.clone(), body)),
                    Err(e) => listing.unreadable.push(UnreadableObjectError {
                        key: format!("job-sources/{job_id}.json"),
                        reason: e.to_string(),
                    }),
                }
                listing
            }))
    }

    async fn put_job_cell(
        &self,
        job_id: &JobId,
        cell_digest: &str,
        scope: &CellScope,
    ) -> anyhow::Result<()> {
        crate::job_cells::insert_job_cell(
            &self.db,
            job_id.as_str(),
            cell_digest,
            Some(&scope.key()),
        )
        .await
    }

    /// The method logs a warning for a row whose job id or source does not
    /// parse, and leaves the row out. The cell then runs again. That costs work
    /// and gives no wrong answer.
    async fn find_cell_results(
        &self,
        cell_digests: &[String],
        scope: &CellScope,
    ) -> anyhow::Result<Vec<CellResult>> {
        if cell_digests.is_empty() {
            return Ok(Vec::new());
        }
        let scope = scope.key();
        let per_cell = i64::try_from(pipette_store::CELL_RESULTS_PER_CELL)?;
        let rows: Vec<(String, String, String, Vec<String>)> = self
            .db
            .fetch_all("find_cell_results", || {
                sqlx::query_as(
                    "SELECT cell_digest, job_id, body, run_flags FROM (
                         SELECT c.cell_digest, c.job_id, s.body, o.run_flags,
                                ROW_NUMBER() OVER (
                                    PARTITION BY c.cell_digest ORDER BY c.job_id DESC
                                ) AS newest
                         FROM job_cells c
                         JOIN submission_outcomes o ON o.job_id = c.job_id
                         JOIN job_sources s ON s.job_id = c.job_id
                         WHERE c.cell_digest = ANY($1)
                           AND c.scope = $2
                           AND o.outcome = 'finished'
                           AND o.run_flags IS NOT NULL
                           AND NOT EXISTS (SELECT 1 FROM plan_jobs p WHERE p.rerun_of = c.job_id)
                     ) ranked
                     WHERE newest <= $3
                     ORDER BY job_id DESC",
                )
                .bind(cell_digests)
                .bind(&scope)
                .bind(per_cell)
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(cell_digest, job_id, body, run_flags)| {
                let job_id = JobId::try_new(&job_id)
                    .inspect_err(|e| {
                        tracing::warn!(key = %job_id, error = %e, "skipping unparseable job-cells key");
                    })
                    .ok()?;
                let body = serde_json::from_str(&body)
                    .inspect_err(|e| {
                        tracing::warn!(
                            job_id = %job_id,
                            error = %e,
                            "skipping a finished run of a cell whose job source does not parse"
                        );
                    })
                    .ok()?;
                Some(CellResult {
                    cell_digest,
                    job_id,
                    body,
                    run_flags: run_flags.into_iter().collect(),
                })
            })
            .collect())
    }

    async fn put_job_plan_link(
        &self,
        job_id: &JobId,
        plan_id: &PlanId,
    ) -> anyhow::Result<CreateOutcome> {
        let created = self
            .db
            .execute("put_job_plan_link", || {
                sqlx::query(
                    "INSERT INTO job_plan_links (job_id, plan_id) VALUES ($1, $2)
                     ON CONFLICT (job_id, plan_id) DO NOTHING",
                )
                .bind(job_id.as_str())
                .bind(plan_id.as_str())
            })
            .await?;
        Ok(create_outcome(created))
    }

    async fn list_job_plan_links(&self, job_id: &JobId) -> anyhow::Result<Vec<PlanId>> {
        let rows: Vec<(String,)> = self
            .db
            .fetch_all("list_job_plan_links", || {
                sqlx::query_as(
                    "SELECT plan_id FROM job_plan_links WHERE job_id = $1 ORDER BY plan_id",
                )
                .bind(job_id.as_str())
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(raw,)| plan_id_of(&raw, "job-plan-links key"))
            .collect())
    }

    /// One statement for every job, where the default makes one per job.
    async fn list_job_plan_links_of(
        &self,
        job_ids: &[JobId],
    ) -> anyhow::Result<Vec<(JobId, Vec<PlanId>)>> {
        if job_ids.is_empty() {
            return Ok(Vec::new());
        }
        let asked: HashMap<&str, &JobId> = job_ids.iter().map(|id| (id.as_str(), id)).collect();
        let ids: Vec<&str> = asked.keys().copied().collect();
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("list_job_plan_links_of", || {
                sqlx::query_as("SELECT job_id, plan_id FROM job_plan_links WHERE job_id = ANY($1)")
                    .bind(ids.as_slice())
            })
            .await?;
        let linked = rows.into_iter().fold(
            HashMap::<&JobId, Vec<PlanId>>::new(),
            |mut linked, (job_id, plan_id)| {
                if let (Some(&job_id), Some(plan_id)) = (
                    asked.get(job_id.as_str()),
                    plan_id_of(&plan_id, "job-plan-links key"),
                ) {
                    linked.entry(job_id).or_default().push(plan_id);
                }
                linked
            },
        );
        Ok(linked
            .into_iter()
            .map(|(job_id, plan_ids)| (job_id.clone(), plan_ids))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::Context;
    use futures::TryStreamExt;

    use pipette_core::config::StorageConfig;
    use pipette_core::plan::{ExpiredJob, sample_manifest};
    use pipette_core::types::Mint;
    use pipette_plan_types::{CampaignId, PlanStatus};
    use pipette_store::drain_plans;

    use super::*;
    use crate::scratch::TestDb;
    use crate::testing::race;

    /// A `plan_jobs` row as a tuple of the job, its ordinal, its content key,
    /// its rerun source, whether it holds an expiry, and its outcome.
    type StoredJob = (
        String,
        Option<i32>,
        Option<String>,
        Option<String>,
        bool,
        Option<String>,
    );

    fn store(test_db: &TestDb) -> SqlPlanStore {
        SqlPlanStore::new(test_db.db().clone())
    }

    fn plan(n: u128) -> PlanId {
        PlanId::from_uuid(uuid::Uuid::from_u128(n))
    }

    fn job(n: u128) -> JobId {
        JobId::from_uuid(uuid::Uuid::from_u128(n))
    }

    /// A record whose `created_at` carries nanoseconds, which a `timestamptz`
    /// would drop.
    fn record(n: u128) -> anyhow::Result<ContentKeyRecord> {
        let created_at = DateTime::from_timestamp(1_780_000_000 + i64::try_from(n)?, 123_456_789)
            .context("timestamp")?;
        Ok(ContentKeyRecord {
            job_id: job(n),
            plan_id: plan(n),
            created_at,
        })
    }

    fn source(n: u128) -> anyhow::Result<PlanSource> {
        Ok(PlanSource::new(format!(
            "# writer {n}\nbenchmarks = [\"b{n}\"]\n"
        ))?)
    }

    /// Write a row past the store, as an operator or another build might.
    async fn plant(db: &SqlDatabase, sql: &'static str, values: &[&str]) -> anyhow::Result<()> {
        values
            .iter()
            .fold(sqlx::query(sql), |query, value| query.bind(*value))
            .execute(db.pool())
            .await?;
        Ok(())
    }

    /// The pagination contract's `write_raw`: a `.json` leaf is the row its
    /// stem names, `complete` so the filtered scan reaches it; others are none.
    async fn plant_manifest(db: SqlDatabase, leaf: &str, body: Vec<u8>) -> anyhow::Result<()> {
        let Some(stem) = leaf.strip_suffix(".json") else {
            return Ok(());
        };
        let body = String::from_utf8(body)?;
        plant(
            &db,
            "INSERT INTO plans (plan_id, status, body) VALUES ($1, 'complete', $2)
             ON CONFLICT (plan_id) DO UPDATE SET body = EXCLUDED.body",
            &[stem, body.as_str()],
        )
        .await
    }

    /// A `plan_max_jobs`-sized plan with every job starved, past 1 MiB
    /// pretty-printed. `writer` picks the job ids, so each writer's differs.
    fn large_manifest(plan_id: PlanId, writer: u128) -> anyhow::Result<PlanManifest> {
        let mut manifest = sample_manifest(plan_id, PlanStatus::Active, true)?;
        let job_ids: Vec<JobId> = (0..5_000).map(|n| job((writer << 32) | n)).collect();
        manifest.content_keys = job_ids
            .iter()
            .zip(0_u128..)
            .map(|(id, n)| (id.clone(), format!("{n:064x}")))
            .collect();
        manifest
            .progress_snapshot
            .as_mut()
            .and_then(|snapshot| snapshot.starved.first_mut())
            .context("the sample's starved group")?
            .job_ids
            .clone_from(&job_ids);
        manifest.job_ids = job_ids;
        Ok(manifest)
    }

    async fn stored_text(
        db: &SqlDatabase,
        sql: &'static str,
        id: &str,
    ) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as(sql)
            .bind(id)
            .fetch_optional(db.pool())
            .await?;
        Ok(row.map(|(text,)| text))
    }

    #[tokio::test]
    async fn content_key_index_meets_the_shared_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        pipette_store_testkit::assert_content_key_roundtrip(&store(&test_db)).await
    }

    #[tokio::test]
    async fn job_plan_markers_meet_the_shared_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        pipette_store_testkit::assert_job_plan_marker_roundtrip(&store(&test_db)).await
    }

    /// The store keeps the notes of a plan apart from its manifest. The test
    /// checks these rules:
    /// - A rewrite of the manifest keeps the notes.
    /// - A note edit keeps the change.
    /// - A write for a plan that does not exist stores nothing.
    #[tokio::test]
    async fn plan_notes_round_trip_beside_the_manifest() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let plan_id = PlanId::from_uuid(uuid::Uuid::from_u128(60));
        let other = PlanId::from_uuid(uuid::Uuid::from_u128(61));
        let filed = PlanNotes {
            note: Some("as filed".to_owned()),
            change: Some(pipette_plan_types::RevisionChange::Rerun {
                answered: Some(pipette_plan_types::Answered::Failed),
                job_count: 2,
            }),
        };
        assert!(!store.put_plan_notes(&plan_id, &filed).await?);
        assert!(!store.set_plan_note(&plan_id, Some("absent")).await?);
        assert_eq!(store.get_plan_notes(&plan_id).await?, PlanNotes::default());

        let manifest = sample_manifest(plan_id.clone(), PlanStatus::Active, true)?;
        let untouched = sample_manifest(other.clone(), PlanStatus::Active, true)?;
        store.put_plan(&manifest).await?;
        store.put_plan(&untouched).await?;
        assert!(store.put_plan_notes(&plan_id, &filed).await?);

        let note = "Thermals,\nsee bos-17pro-1";
        assert!(store.set_plan_note(&plan_id, Some(note)).await?);
        let noted = PlanNotes {
            note: Some(note.to_owned()),
            ..filed.clone()
        };
        // A rewrite of the manifest, as `queue-maintenance` makes, keeps the notes.
        store
            .put_plan(&PlanManifest {
                status: PlanStatus::Complete,
                ..manifest.clone()
            })
            .await?;
        assert_eq!(store.get_plan_notes(&plan_id).await?, noted);
        assert_eq!(
            store
                .get_plan_notes_of(&[plan_id.clone(), other.clone()])
                .await?,
            HashMap::from([(plan_id.clone(), noted.clone())])
        );
        let summaries = store
            .get_plan_summaries(std::slice::from_ref(&plan_id))
            .await?;
        assert_eq!(
            summaries
                .items
                .first()
                .map(|summary| (summary.note.as_deref(), summary.change.as_ref())),
            Some((Some(note), filed.change.as_ref()))
        );
        let headers = store
            .list_matching_plan_headers(
                &PlanFilter::default(),
                None,
                pipette_store_testkit::TEST_LIST_LIMIT,
            )
            .await?;
        let rewritten = PlanManifest {
            status: PlanStatus::Complete,
            ..manifest
        };
        assert_eq!(
            headers.items,
            vec![
                PlanHeader::new(rewritten.clone(), noted),
                PlanHeader::new(untouched, PlanNotes::default())
            ]
        );
        assert_eq!(store.get_plan(&plan_id).await?, Some(rewritten));

        assert!(store.set_plan_note(&plan_id, None).await?);
        assert_eq!(
            store.get_plan_notes(&plan_id).await?,
            PlanNotes {
                note: None,
                ..filed
            }
        );
        Ok(())
    }

    /// A change this build cannot read, such as a `kind` from a later build,
    /// reads as none. The plan, its note, its header and its summary still read.
    #[tokio::test]
    async fn an_unreadable_change_reads_as_none() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let plan_id = PlanId::from_uuid(uuid::Uuid::from_u128(62));
        let manifest = sample_manifest(plan_id.clone(), PlanStatus::Active, true)?;
        store.put_plan(&manifest).await?;
        assert!(store.set_plan_note(&plan_id, Some("kept")).await?);
        plant(
            test_db.db(),
            r#"UPDATE plans SET change = '{"kind": "from_a_later_build"}' WHERE plan_id = $1"#,
            &[plan_id.as_str()],
        )
        .await?;

        let notes = PlanNotes {
            note: Some("kept".to_owned()),
            change: None,
        };
        assert_eq!(store.get_plan_notes(&plan_id).await?, notes);
        assert_eq!(
            store
                .get_plan_notes_of(std::slice::from_ref(&plan_id))
                .await?,
            HashMap::from([(plan_id.clone(), notes.clone())])
        );
        let headers = store
            .list_matching_plan_headers(
                &PlanFilter::default(),
                None,
                pipette_store_testkit::TEST_LIST_LIMIT,
            )
            .await?;
        assert_eq!(
            headers.items,
            vec![PlanHeader::new(manifest.clone(), notes)]
        );
        assert!(headers.unreadable.is_empty(), "{:?}", headers.unreadable);
        let summaries = store
            .get_plan_summaries(std::slice::from_ref(&plan_id))
            .await?;
        let summary = PlanSummary {
            note: Some("kept".to_owned()),
            ..manifest.into()
        };
        assert_eq!(summaries.items, vec![summary]);
        assert!(
            summaries.unreadable.is_empty(),
            "{:?}",
            summaries.unreadable
        );
        Ok(())
    }

    /// Each kind of change as every build writes the column: what a rollback reads.
    fn stored_changes() -> anyhow::Result<Vec<(&'static str, RevisionChange)>> {
        let document = |carry_earlier| RevisionChange::Document {
            carry_earlier,
            inherit_from: Vec::new(),
        };
        let rerun = |answered, job_count| RevisionChange::Rerun {
            answered,
            job_count,
        };
        let recalculate = |all_evals, job_count| RevisionChange::Recalculate {
            all_evals,
            job_count,
        };
        Ok(vec![
            (
                r#"{"kind":"variants","removed":0,"added":1}"#,
                RevisionChange::Variants {
                    removed_variant_count: 0,
                    added_variant_count: 1,
                    carry_earlier: false,
                    inherit_from: Vec::new(),
                    restored: Vec::new(),
                },
            ),
            (
                r#"{"kind":"variants","removed":2,"added":3,"carry_earlier":true,"inherit_from":["plan-a","plan-b"]}"#,
                RevisionChange::Variants {
                    removed_variant_count: 2,
                    added_variant_count: 3,
                    carry_earlier: true,
                    inherit_from: vec![PlanId::try_new("plan-a")?, PlanId::try_new("plan-b")?],
                    restored: Vec::new(),
                },
            ),
            (
                r#"{"kind":"variants","removed":1,"added":2,"inherit_from":["plan-a"],"restored":[{"plan":"plan-a","index":2},{"plan":"plan-a","index":0}]}"#,
                RevisionChange::Variants {
                    removed_variant_count: 1,
                    added_variant_count: 2,
                    carry_earlier: false,
                    inherit_from: vec![PlanId::try_new("plan-a")?],
                    restored: vec![
                        RestoredVariant {
                            plan_id: PlanId::try_new("plan-a")?,
                            variant_index: 2,
                        },
                        RestoredVariant {
                            plan_id: PlanId::try_new("plan-a")?,
                            variant_index: 0,
                        },
                    ],
                },
            ),
            (r#"{"kind":"document"}"#, document(false)),
            (
                r#"{"kind":"document","carry_earlier":true}"#,
                document(true),
            ),
            (r#"{"kind":"rerun","jobs":1}"#, rerun(None, 1)),
            (
                r#"{"kind":"rerun","answered":"failed","jobs":12}"#,
                rerun(Some(Answered::Failed), 12),
            ),
            (
                r#"{"kind":"rerun","answered":"all","jobs":3}"#,
                rerun(Some(Answered::All), 3),
            ),
            (r#"{"kind":"recalculate","jobs":2}"#, recalculate(false, 2)),
            (
                r#"{"kind":"recalculate","all_evals":true,"jobs":5}"#,
                recalculate(true, 5),
            ),
            (
                r#"{"kind":"revert","to":"plan-a"}"#,
                RevisionChange::Revert {
                    to_plan_id: PlanId::try_new("plan-a")?,
                },
            ),
        ])
    }

    #[test]
    fn a_change_reads_and_writes_in_its_stored_shape() -> anyhow::Result<()> {
        stored_changes()?
            .into_iter()
            .try_for_each(|(stored, change)| {
                assert_eq!(
                    read_change("plan", Some(stored.to_owned())).as_ref(),
                    Some(&change),
                    "{stored}"
                );
                assert_eq!(write_change(&change)?, stored);
                anyhow::Ok(())
            })
    }

    /// The column itself: a planted stored change reads, and a write leaves
    /// the same `jsonb`.
    #[tokio::test]
    async fn the_change_column_keeps_its_stored_shape() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let plan_id = PlanId::from_uuid(uuid::Uuid::from_u128(63));
        store
            .put_plan(&sample_manifest(plan_id.clone(), PlanStatus::Active, true)?)
            .await?;
        // One at a time: every row rewrites the same plan row.
        let (test_db, store, plan_id) = (&test_db, &store, &plan_id);
        futures::stream::iter(stored_changes()?.into_iter().map(anyhow::Ok))
            .try_for_each(|(stored, change)| async move {
                plant(
                    test_db.db(),
                    "UPDATE plans SET change = $2::jsonb WHERE plan_id = $1",
                    &[plan_id.as_str(), stored],
                )
                .await?;
                let read = store.get_plan_notes(plan_id).await?.change;
                assert_eq!(read.as_ref(), Some(&change), "{stored}");

                plant(
                    test_db.db(),
                    "UPDATE plans SET change = NULL WHERE plan_id = $1",
                    &[plan_id.as_str()],
                )
                .await?;
                let notes = PlanNotes {
                    note: None,
                    change: Some(change),
                };
                assert!(store.put_plan_notes(plan_id, &notes).await?);
                let same: bool =
                    sqlx::query_scalar("SELECT change = $2::jsonb FROM plans WHERE plan_id = $1")
                        .bind(plan_id.as_str())
                        .bind(stored)
                        .fetch_one(test_db.db().pool())
                        .await?;
                assert!(same, "{stored}");
                Ok(())
            })
            .await
    }

    /// The notes are columns that no manifest write changes. A rewrite from a
    /// manifest that the store read before an edit keeps the edit. The body
    /// holds neither the note nor the change.
    #[tokio::test]
    async fn a_note_outlives_a_rewrite_from_an_older_read() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
        store
            .put_plan(&sample_manifest(
                plan_id.clone(),
                PlanStatus::Active,
                false,
            )?)
            .await?;
        let filed = PlanNotes {
            note: Some("as filed".into()),
            change: Some(RevisionChange::Document {
                carry_earlier: true,
                inherit_from: Vec::new(),
            }),
        };
        assert!(store.put_plan_notes(&plan_id, &filed).await?);
        let stale = store
            .get_plan(&plan_id)
            .await?
            .context("the plan is stored")?;
        assert!(store.set_plan_note(&plan_id, Some("edited")).await?);

        store
            .put_plan(&PlanManifest {
                status: PlanStatus::Complete,
                ..stale
            })
            .await?;
        assert_eq!(
            store.get_plan_notes(&plan_id).await?,
            PlanNotes {
                note: Some("edited".into()),
                ..filed
            }
        );
        let (body,): (String,) = sqlx::query_as("SELECT body FROM plans WHERE plan_id = $1")
            .bind(plan_id.as_str())
            .fetch_one(test_db.db().pool())
            .await?;
        ["edited", "note", "carry_earlier", "change"]
            .iter()
            .for_each(|absent| assert!(!body.contains(absent), "{body}"));
        Ok(())
    }

    #[tokio::test]
    async fn plan_sources_meet_the_shared_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        pipette_store_testkit::assert_plan_source_roundtrip(&store).await?;
        pipette_store_testkit::assert_pending_source_marker_roundtrip(&store).await
    }

    #[tokio::test]
    async fn job_sources_and_links_meet_the_shared_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        pipette_store_testkit::assert_job_source_roundtrip(&store).await?;
        pipette_store_testkit::assert_job_plan_link_roundtrip(&store).await
    }

    #[tokio::test]
    async fn plan_listing_meets_the_shared_pagination_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db().clone();
        pipette_store_testkit::assert_plan_listing_pagination(&store(&test_db), |leaf, bytes| {
            plant_manifest(db.clone(), leaf, bytes)
        })
        .await
    }

    #[tokio::test]
    async fn get_plans_meets_the_shared_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db().clone();
        pipette_store_testkit::assert_get_plans_contract(&store(&test_db), |leaf, bytes| {
            plant_manifest(db.clone(), leaf, bytes)
        })
        .await
    }

    /// A 5,000-job plan reads whole beside a small one, and summarizes alike.
    #[tokio::test]
    async fn get_plans_reads_a_large_batch_whole_and_summarized() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let large = large_manifest(plan(1), 0)?;
        let small = sample_manifest(plan(2), PlanStatus::Complete, false)?;
        store.put_plan(&large).await?;
        store.put_plan(&small).await?;
        let ids = [plan(1), plan(2)];

        let mut whole = store.get_plans(&ids).await?.items;
        whole.sort_by(|a, b| a.plan_id.cmp(&b.plan_id));
        assert_eq!(whole, vec![large.clone(), small.clone()]);

        let mut summaries = store.get_plan_summaries(&ids).await?.items;
        summaries.sort_by(|a, b| a.plan_id.cmp(&b.plan_id));
        assert_eq!(summaries, vec![large.into(), small.into()]);
        Ok(())
    }

    /// A body field the summary does not show fails the manifest, not the
    /// summary.
    #[tokio::test]
    async fn get_plan_summaries_reads_past_a_field_it_does_not_show() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let manifest = sample_manifest(plan(1), PlanStatus::Active, true)?;
        store.put_plan(&manifest).await?;
        plant(
            test_db.db(),
            "UPDATE plans SET body = jsonb_set(body::jsonb, '{warnings}', '\"broken\"')::text
             WHERE plan_id = $1",
            &[plan(1).as_str()],
        )
        .await?;
        let ids = [plan(1)];

        let whole = store.get_plans(&ids).await?;
        assert!(whole.items.is_empty());
        assert_eq!(whole.unreadable.len(), 1);
        let summaries = store.get_plan_summaries(&ids).await?;
        assert_eq!(summaries.items, vec![manifest.into()]);
        assert!(summaries.unreadable.is_empty());
        Ok(())
    }

    /// Against a queue on the object store, the split the SQL backend runs in.
    #[tokio::test]
    async fn each_job_has_its_source_and_link() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let dir = tempfile::tempdir()?;
        let todo = pipette_store_fs::build_local_fs_todo_store(&StorageConfig::local_fs(
            dir.path().to_path_buf(),
        ))?;
        let plan_id = plan(1);
        let job_ids = vec![job(10), job(11)];
        for job_id in &job_ids {
            let body = serde_json::json!({
                "job_id": job_id.as_str(),
                "spec": { "benchmark": "decode_throughput_512_100" },
            });
            todo.write_tmp(job_id, &body).await?;
            todo.promote_avail(job_id, ExpiresAt::Never).await?;
            store.put_job_source(job_id, &body).await?;
            store.put_job_plan_link(job_id, &plan_id).await?;
        }
        pipette_store_testkit::assert_each_job_has_its_source_and_link(
            &store,
            todo.as_ref(),
            &plan_id,
            &job_ids,
        )
        .await
    }

    #[tokio::test]
    async fn manifests_round_trip_filter_and_overwrite() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let active = sample_manifest(plan(1), PlanStatus::Active, true)?;
        let done = sample_manifest(plan(2), PlanStatus::Complete, false)?;
        store.put_plan(&active).await?;
        store.put_plan(&done).await?;

        assert_eq!(store.get_plan(&plan(1)).await?.as_ref(), Some(&active));
        assert_eq!(store.get_plan(&plan(2)).await?.as_ref(), Some(&done));
        assert_eq!(
            drain_plans(&store, None).await?.items,
            vec![active.clone(), done.clone()]
        );
        assert_eq!(
            drain_plans(&store, Some(PlanStatus::Complete)).await?.items,
            vec![done]
        );
        assert!(
            drain_plans(&store, Some(PlanStatus::Cancelled))
                .await?
                .items
                .is_empty()
        );

        let mut cancelled = active;
        cancelled.status = PlanStatus::Cancelled;
        store.put_plan(&cancelled).await?;
        assert_eq!(store.get_plan(&plan(1)).await?.as_ref(), Some(&cancelled));
        assert_eq!(
            stored_text(
                test_db.db(),
                "SELECT status FROM plans WHERE plan_id = $1",
                plan(1).as_str(),
            )
            .await?
            .as_deref(),
            Some("cancelled")
        );
        Ok(())
    }

    /// The filter columns and the per-job fields leave the body, so each fact
    /// has one place.
    #[tokio::test]
    async fn a_manifest_splits_into_columns_jobs_and_the_rest() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = store(&test_db);
        let mut manifest = sample_manifest(plan(1), PlanStatus::Active, true)?;
        manifest.expired_jobs.insert(
            JobId::new_unchecked("job-1"),
            ExpiredJob {
                horizon: "2026-10-07T09:00:00Z".parse()?,
                recorded_at: DateTime::from_timestamp(1_780_000_000, 123_456_789)
                    .context("timestamp")?,
            },
        );
        manifest
            .rerun_of
            .insert(JobId::new_unchecked("job-9"), JobId::new_unchecked("job-1"));
        store.put_plan(&manifest).await?;
        assert_eq!(store.get_plan(&plan(1)).await?.as_ref(), Some(&manifest));

        let (status, campaign_id, submitted_by, body): (
            String,
            Option<String>,
            Option<String>,
            String,
        ) = sqlx::query_as(
            "SELECT status, campaign_id, submitted_by, body FROM plans WHERE plan_id = $1",
        )
        .bind(plan(1).as_str())
        .fetch_one(db.pool())
        .await?;
        assert_eq!(status, "active");
        assert_eq!(
            campaign_id.as_deref(),
            manifest.campaign_id.as_ref().map(|id| id.as_str())
        );
        assert_eq!(
            submitted_by.as_deref(),
            manifest.submitted_by.as_ref().map(|id| id.as_str())
        );
        let body: serde_json::Map<String, Value> = serde_json::from_str(&body)?;
        // The body keeps the stored names, which the plan read renames.
        assert_eq!(
            body.get("job_expiry"),
            Some(
                &serde_json::json!({"earliest": "20260819T175500Z", "latest": "20260918T175500Z"})
            )
        );
        for moved in [
            "plan_id",
            "status",
            "campaign_id",
            "submitted_by",
            "job_ids",
            "content_keys",
            "rerun_of",
            "expired_jobs",
            "settled_jobs",
        ] {
            assert!(!body.contains_key(moved), "{moved} stays in the body");
        }

        // `job-9` is in no `job_ids`, so its row has no ordinal.
        let jobs: Vec<StoredJob> = sqlx::query_as(
            "SELECT job_id, ordinal, content_key, rerun_of, expired IS NOT NULL, outcome
                 FROM plan_jobs WHERE plan_id = $1 ORDER BY job_id",
        )
        .bind(plan(1).as_str())
        .fetch_all(db.pool())
        .await?;
        assert_eq!(
            jobs,
            vec![
                (
                    "job-1".into(),
                    Some(0),
                    Some("1".repeat(64)),
                    None,
                    true,
                    Some("finished".into())
                ),
                (
                    "job-2".into(),
                    Some(1),
                    Some("2".repeat(64)),
                    Some("job-0".into()),
                    true,
                    None
                ),
                (
                    "job-9".into(),
                    None,
                    None,
                    Some("job-1".into()),
                    false,
                    None
                ),
            ]
        );
        Ok(())
    }

    /// `plan_jobs.expired` holds a lapse under its stored names, which the jobs
    /// page renames: a write produces them, and a row in them reads back.
    #[tokio::test]
    async fn the_expired_column_keeps_the_stored_names() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = store(&test_db);
        let stored = serde_json::json!({
            "horizon": "20261007T090000Z",
            "recorded_at": "2026-10-07T11:00:00Z",
        });
        let lapse = ExpiredJob {
            horizon: "2026-10-07T09:00:00Z".parse()?,
            recorded_at: "2026-10-07T11:00:00Z".parse()?,
        };
        let mut manifest = sample_manifest(plan(1), PlanStatus::Active, false)?;
        manifest.expired_jobs = BTreeMap::from([(JobId::new_unchecked("job-1"), lapse.clone())]);
        store.put_plan(&manifest).await?;
        let written: String = sqlx::query_scalar(
            "SELECT expired::text FROM plan_jobs WHERE plan_id = $1 AND job_id = 'job-1'",
        )
        .bind(plan(1).as_str())
        .fetch_one(db.pool())
        .await?;
        assert_eq!(serde_json::from_str::<Value>(&written)?, stored);

        sqlx::query(
            "UPDATE plan_jobs SET expired = $2::jsonb WHERE plan_id = $1 AND job_id = 'job-2'",
        )
        .bind(plan(1).as_str())
        .bind(stored.to_string())
        .execute(db.pool())
        .await?;
        let read = store.get_plan(&plan(1)).await?.context("the plan")?;
        assert_eq!(
            read.expired_jobs,
            BTreeMap::from([
                (JobId::new_unchecked("job-1"), lapse.clone()),
                (JobId::new_unchecked("job-2"), lapse),
            ])
        );
        Ok(())
    }

    /// An older build's migration-0001 statement names no `outcome`, so its
    /// write keeps the outcome a newer build recorded.
    #[tokio::test]
    async fn an_older_build_keeps_the_outcome() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = store(&test_db);
        let manifest = sample_manifest(plan(1), PlanStatus::Active, false)?;
        store.put_plan(&manifest).await?;

        sqlx::query(
            "INSERT INTO plan_jobs (plan_id, job_id, ordinal, content_key, rerun_of, expired)
             VALUES ($1, 'job-1', 0, $2, NULL, NULL)
             ON CONFLICT (plan_id, job_id) DO UPDATE SET
                 ordinal = EXCLUDED.ordinal,
                 content_key = EXCLUDED.content_key,
                 rerun_of = EXCLUDED.rerun_of,
                 expired = EXCLUDED.expired",
        )
        .bind(plan(1).as_str())
        .bind("3".repeat(64))
        .execute(db.pool())
        .await?;

        let read = store
            .get_plan(&plan(1))
            .await?
            .context("the plan is stored")?;
        assert_eq!(read.settled_jobs, manifest.settled_jobs);
        assert_eq!(
            read.content_keys.get(&JobId::new_unchecked("job-1")),
            Some(&"3".repeat(64)),
            "the older write landed"
        );
        Ok(())
    }

    /// A status refresh rewrites the `plans` row and leaves each unchanged job
    /// row as it was; a job that leaves the manifest loses its row.
    #[tokio::test]
    async fn a_rewrite_touches_only_the_jobs_that_changed() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = store(&test_db);
        let versions = || async {
            let rows: Vec<(String, String)> = sqlx::query_as(
                "SELECT job_id, xmin::text FROM plan_jobs WHERE plan_id = $1 ORDER BY job_id",
            )
            .bind(plan(1).as_str())
            .fetch_all(db.pool())
            .await?;
            anyhow::Ok(rows.into_iter().collect::<BTreeMap<_, _>>())
        };
        let mut manifest = sample_manifest(plan(1), PlanStatus::Active, false)?;
        manifest.job_ids = (1..=3).map(job).collect();
        manifest.content_keys.clear();
        manifest.rerun_of.clear();
        store.put_plan(&manifest).await?;
        let before = versions().await?;

        manifest.status = PlanStatus::Complete;
        manifest.content_keys.insert(job(2), "2".repeat(64));
        manifest.job_ids.retain(|id| *id != job(3));
        store.put_plan(&manifest).await?;
        let after = versions().await?;

        assert_eq!(after.get(job(1).as_str()), before.get(job(1).as_str()));
        assert_ne!(after.get(job(2).as_str()), before.get(job(2).as_str()));
        assert!(!after.contains_key(job(3).as_str()));
        assert_eq!(store.get_plan(&plan(1)).await?.as_ref(), Some(&manifest));
        Ok(())
    }

    #[tokio::test]
    async fn a_manifest_that_names_a_job_twice_is_refused() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let mut manifest = sample_manifest(plan(1), PlanStatus::Active, false)?;
        manifest.job_ids = vec![job(1), job(1)];
        let refused = store.put_plan(&manifest).await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e.to_string().contains("twice")),
            "{refused:?}"
        );
        assert!(store.get_plan(&plan(1)).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn plan_filter_meets_the_shared_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        pipette_store_testkit::assert_plan_filter_contract(&store(&test_db)).await
    }

    #[tokio::test]
    async fn plan_reach_meets_the_shared_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        pipette_store_testkit::assert_plan_reach_contract(&store(&test_db)).await
    }

    /// The filter reads columns, which a broken body leaves intact, so only a
    /// broken plan that the filter admits is reported.
    #[tokio::test]
    async fn a_filtered_listing_reports_only_the_broken_plans_it_admits() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let mine = CampaignId::mint();
        let other = CampaignId::mint();
        for (n, campaign) in [(1, &mine), (2, &other)] {
            sqlx::query(
                "INSERT INTO plans (plan_id, status, campaign_id, body)
                 VALUES ($1, 'active', $2, '{not json')",
            )
            .bind(plan(n).as_str())
            .bind(campaign.as_str())
            .execute(test_db.db().pool())
            .await?;
        }
        let filter = |campaign: &CampaignId| PlanFilter {
            campaign_id: Some(campaign.clone()),
            ..PlanFilter::default()
        };
        let listing = pipette_store::drain_matching_plans(&store, &filter(&mine)).await?;
        assert!(listing.items.is_empty());
        let reported: Vec<&str> = listing
            .unreadable
            .iter()
            .map(|bad| bad.key.as_str())
            .collect();
        assert_eq!(reported, vec![format!("plans/{}.json", plan(1))]);
        assert_eq!(
            drain_plans(&store, None).await?.unreadable.len(),
            2,
            "an unfiltered listing reports both"
        );
        Ok(())
    }

    /// A campaign's plans in one read: its own alone, and a broken one left for
    /// the chain walk's own read to report.
    #[tokio::test]
    async fn a_campaigns_plans_read_at_once() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let mine = CampaignId::mint();
        let other = CampaignId::mint();
        for (n, campaign) in [(1, &mine), (2, &mine), (3, &other)] {
            let mut manifest = sample_manifest(plan(n), PlanStatus::Active, false)?;
            manifest.campaign_id = Some(campaign.clone());
            store.put_plan(&manifest).await?;
        }
        plant(
            test_db.db(),
            "INSERT INTO plans (plan_id, status, campaign_id, body) VALUES ($1, 'active', $2, '{not json')",
            &[plan(4).as_str(), mine.as_str()],
        )
        .await?;
        let mut read: Vec<PlanId> = store
            .get_campaign_plans(&mine)
            .await?
            .context("the SQL store reads a campaign's plans at once")?
            .into_iter()
            .map(|manifest| manifest.plan_id)
            .collect();
        read.sort();
        assert_eq!(read, vec![plan(1), plan(2)]);
        Ok(())
    }

    #[tokio::test]
    async fn a_full_page_ends_the_listing_past_rows_that_name_no_plan() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let two = NonZeroUsize::new(2).context("two")?;
        for n in [1, 2] {
            store
                .put_plan(&sample_manifest(plan(n), PlanStatus::Active, false)?)
                .await?;
        }
        // Both sort after every `plan-{uuid}`: `x` past the hex digits, `.` past `-`.
        for stray in ["plan-x.bak", "plan.bad"] {
            plant(
                test_db.db(),
                "INSERT INTO plans (plan_id, status, body) VALUES ($1, 'active', '{not json')",
                &[stray],
            )
            .await?;
        }
        let page = store.list_plans(None, None, two).await?;
        let ids = |page: &PlanPage| {
            page.items
                .iter()
                .map(|p| p.plan_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&page), vec![plan(1), plan(2)]);
        assert!(page.unreadable.is_empty());
        assert_eq!(page.resume_after, None);

        let later = PlanId::try_new("planz")?;
        store
            .put_plan(&sample_manifest(later.clone(), PlanStatus::Active, false)?)
            .await?;
        let page = store.list_plans(None, None, two).await?;
        assert_eq!(page.resume_after, Some(plan(2)));
        let rest = store.list_plans(None, Some(&plan(2)), two).await?;
        assert_eq!(ids(&rest), vec![later]);
        assert!(rest.unreadable.is_empty());
        assert_eq!(rest.resume_after, None);
        Ok(())
    }

    /// A 5,000-job manifest with a full snapshot runs past 1 MiB, and is one
    /// `plans` row and a `plan_jobs` row for each job.
    #[tokio::test]
    async fn a_large_manifest_round_trips() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = store(&test_db);
        let large = large_manifest(plan(1), 0)?;
        let text = serde_json::to_string_pretty(&large)?;
        assert!(text.len() > 1 << 20, "{} bytes", text.len());
        store.put_plan(&large).await?;
        let small = sample_manifest(plan(2), PlanStatus::Complete, false)?;
        store.put_plan(&small).await?;

        let jobs: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM plan_jobs WHERE plan_id = $1 AND ordinal IS NOT NULL",
        )
        .bind(plan(1).as_str())
        .fetch_one(db.pool())
        .await?;
        assert_eq!(jobs, 5_000);
        assert_eq!(store.get_plan(&plan(1)).await?.as_ref(), Some(&large));
        assert_eq!(drain_plans(&store, None).await?.items, vec![large, small]);
        Ok(())
    }

    #[tokio::test]
    async fn a_missing_manifest_is_none_and_a_corrupt_one_fails() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        assert!(store.get_plan(&plan(7)).await?.is_none());
        plant(
            test_db.db(),
            "INSERT INTO plans (plan_id, status, body) VALUES ($1, 'active', '{not json')",
            &[plan(7).as_str()],
        )
        .await?;
        let Err(e) = store.get_plan(&plan(7)).await else {
            anyhow::bail!("a corrupt manifest must not read");
        };
        let unreadable = e
            .root_cause()
            .downcast_ref::<UnreadableObjectError>()
            .context("the root cause is an unreadable object")?;
        assert!(unreadable.key.contains(plan(7).as_str()), "{unreadable}");
        Ok(())
    }

    #[tokio::test]
    async fn cancel_marker_roundtrip() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let (a, b) = (plan(1), plan(2));

        assert!(!store.has_cancel_marker(&a).await?);
        assert!(store.list_cancel_markers().await?.is_empty());

        store.write_cancel_marker(&a).await?;
        store.write_cancel_marker(&a).await?;
        assert!(store.has_cancel_marker(&a).await?);
        assert!(!store.has_cancel_marker(&b).await?, "marker is per-plan");
        assert_eq!(store.list_cancel_markers().await?, vec![a.clone()]);

        store
            .put_plan(&sample_manifest(a.clone(), PlanStatus::Active, false)?)
            .await?;
        assert_eq!(drain_plans(&store, None).await?.items.len(), 1);
        assert_eq!(store.list_cancel_markers().await?.len(), 1);

        store.delete_cancel_marker(&a).await?;
        store.delete_cancel_marker(&a).await?;
        assert!(!store.has_cancel_marker(&a).await?);
        assert!(store.get_plan(&a).await?.is_some(), "manifest survives");
        assert!(store.list_cancel_markers().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn marker_listings_skip_rows_no_write_could_make() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = store(&test_db);
        let (good, job_id) = (plan(1), job(1));

        store.write_cancel_marker(&good).await?;
        plant(
            db,
            "INSERT INTO cancelled_plans (plan_id) VALUES ($1)",
            &["not.a.plan.id"],
        )
        .await?;
        assert_eq!(store.list_cancel_markers().await?, vec![good.clone()]);

        store.write_pending_source_marker(&good).await?;
        plant(
            db,
            "INSERT INTO plan_sources_pending (plan_id, written_at) VALUES ($1, now())",
            &["not.a.plan.id"],
        )
        .await?;
        let pending: Vec<PlanId> = store
            .list_pending_source_markers()
            .await?
            .into_iter()
            .map(|m| m.plan_id)
            .collect();
        assert_eq!(pending, vec![good.clone()]);

        store.put_job_plan_link(&job_id, &good).await?;
        plant(
            db,
            "INSERT INTO job_plan_links (job_id, plan_id) VALUES ($1, $2)",
            &[job_id.as_str(), "stray.tmp"],
        )
        .await?;
        assert_eq!(
            store.list_job_plan_links(&job_id).await?,
            vec![good.clone()]
        );

        store
            .write_job_plan_marker(&job_id, &good, ExpiresAt::Never)
            .await?;
        for row in [
            [job_id.as_str(), "not.a.plan", "never"],
            [job_id.as_str(), good.as_str(), "tomorrow"],
            ["not a job", good.as_str(), "never"],
        ] {
            plant(
                db,
                "INSERT INTO job_plans (job_id, plan_id, expires_at, written_at)
                 VALUES ($1, $2, $3, now())",
                &row,
            )
            .await?;
        }
        assert_eq!(
            store.list_job_plan_markers(&job_id).await?,
            vec![(good.clone(), ExpiresAt::Never)]
        );
        let all = store.list_all_job_plan_markers().await?;
        assert_eq!(
            all.into_iter()
                .map(|m| (m.job_id, m.plan_id, m.expires_at))
                .collect::<Vec<_>>(),
            vec![(job_id, good, ExpiresAt::Never)]
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_membership_is_keyed_to_the_second_and_lists_in_key_order() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let at = |nanos: u32| -> anyhow::Result<ExpiresAt> {
            Ok(ExpiresAt::At(
                DateTime::from_timestamp(1_780_000_000, nanos).context("timestamp")?,
            ))
        };
        let (shared, other) = (JobId::try_new("job-a")?, JobId::try_new("job-a-1")?);
        let (a, a1) = (PlanId::try_new("plan-a")?, PlanId::try_new("plan-a-1")?);

        store
            .write_job_plan_marker(&shared, &a, at(250_000_000)?)
            .await?;
        store
            .write_job_plan_marker(&shared, &a, at(750_000_000)?)
            .await?;
        assert_eq!(
            store.list_job_plan_markers(&shared).await?,
            vec![(a.clone(), at(0)?)]
        );

        // `plan-a-1.never` sorts before `plan-a.…`, and a dated horizon before
        // `never`.
        store
            .write_job_plan_marker(&shared, &a, ExpiresAt::Never)
            .await?;
        store
            .write_job_plan_marker(&shared, &a1, ExpiresAt::Never)
            .await?;
        assert_eq!(
            store.list_job_plan_markers(&shared).await?,
            vec![
                (a1.clone(), ExpiresAt::Never),
                (a.clone(), at(0)?),
                (a.clone(), ExpiresAt::Never),
            ]
        );
        // `job-a-1/…` sorts before `job-a/…`.
        store
            .write_job_plan_marker(&other, &a, ExpiresAt::Never)
            .await?;
        let all: Vec<(JobId, PlanId)> = store
            .list_all_job_plan_markers()
            .await?
            .into_iter()
            .map(|m| (m.job_id, m.plan_id))
            .collect();
        assert_eq!(
            all,
            vec![
                (other.clone(), a.clone()),
                (shared.clone(), a1),
                (shared.clone(), a.clone()),
                (shared.clone(), a.clone()),
            ]
        );

        // A delete names the marker by its key, whatever fraction it carries.
        store
            .delete_job_plan_marker(&shared, &a, at(999_999_999)?)
            .await?;
        assert!(
            !store
                .list_job_plan_markers(&shared)
                .await?
                .contains(&(a, at(0)?))
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_rewritten_marker_carries_its_latest_write_time() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let pending_at = async || -> anyhow::Result<DateTime<Utc>> {
            Ok(store
                .list_pending_source_markers()
                .await?
                .first()
                .context("one pending marker")?
                .written_at)
        };
        let member_at = async || -> anyhow::Result<DateTime<Utc>> {
            Ok(store
                .list_all_job_plan_markers()
                .await?
                .first()
                .context("one membership")?
                .written_at)
        };

        store.write_pending_source_marker(&plan(1)).await?;
        store
            .write_job_plan_marker(&job(1), &plan(1), ExpiresAt::Never)
            .await?;
        let (pending, member) = (pending_at().await?, member_at().await?);
        let now = Utc::now();
        assert!(
            (now - pending).num_seconds().abs() < 60,
            "a server time near the app's"
        );

        tokio::time::sleep(Duration::from_millis(20)).await;
        store.write_pending_source_marker(&plan(1)).await?;
        store
            .write_job_plan_marker(&job(1), &plan(1), ExpiresAt::Never)
            .await?;
        assert!(pending_at().await? > pending);
        assert!(member_at().await? > member);
        Ok(())
    }

    /// Key order included, which a `jsonb` column would not keep.
    #[tokio::test]
    async fn a_job_source_keeps_the_bytes_the_queue_holds() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let body = serde_json::json!({
            "zeta": 1,
            "alpha": { "nested": [1, 2] },
            "control": "a\u{0}b",
        });
        assert_eq!(
            store.put_job_source(&job(1), &body).await?,
            CreateOutcome::Created
        );
        let stored = stored_text(
            test_db.db(),
            "SELECT body FROM job_sources WHERE job_id = $1",
            job(1).as_str(),
        )
        .await?
        .context("the job source row")?;
        assert_eq!(stored.as_bytes(), serde_json::to_vec(&body)?);
        assert_eq!(store.get_job_source(&job(1)).await?, Some(body));
        Ok(())
    }

    /// The root cause must be [`UnreadableObjectError`]: the console degrades a
    /// row on it.
    #[tokio::test]
    async fn a_corrupt_job_source_is_unreadable_not_absent() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        plant(
            test_db.db(),
            "INSERT INTO job_sources (job_id, body) VALUES ($1, '{not json')",
            &[job(1).as_str()],
        )
        .await?;
        let Err(e) = store.get_job_source(&job(1)).await else {
            anyhow::bail!("a corrupt job source must not read");
        };
        let unreadable = e
            .root_cause()
            .downcast_ref::<UnreadableObjectError>()
            .context("the root cause is an unreadable object")?;
        assert!(unreadable.key.contains(job(1).as_str()), "{unreadable}");
        Ok(())
    }

    /// Read with others, a corrupt source is reported and the rest still read.
    #[tokio::test]
    async fn a_corrupt_job_source_does_not_cost_the_batch() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        plant(
            test_db.db(),
            "INSERT INTO job_sources (job_id, body) VALUES ($1, '{not json')",
            &[job(1).as_str()],
        )
        .await?;
        let body = serde_json::json!({ "job_id": job(2).as_str() });
        store.put_job_source(&job(2), &body).await?;
        let read = store.get_job_sources(&[job(1), job(2)]).await?;
        assert_eq!(read.items, vec![(job(2), body)]);
        let [unreadable] = read.unreadable.as_slice() else {
            anyhow::bail!("one unreadable source, got {:?}", read.unreadable);
        };
        assert!(unreadable.key.contains(job(1).as_str()), "{unreadable}");
        Ok(())
    }

    #[tokio::test]
    async fn a_content_key_keeps_nanoseconds_and_refuses_foreign_tokens() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let digest = "a".repeat(64);
        let mine = record(1)?;
        assert!(matches!(
            store.create_content_key(&digest, &mine).await?,
            ContentKeyClaim::Created
        ));
        let read = store
            .get_content_key(&digest)
            .await?
            .context("claimed digest")?;
        assert_eq!(read.0, mine);
        let ContentKeyClaim::AlreadyExists(incumbent, version) =
            store.create_content_key(&digest, &record(2)?).await?
        else {
            anyhow::bail!("a second claim must not create");
        };
        assert_eq!((incumbent, version), read.clone());
        assert_eq!(
            store.list_content_keys().await?,
            vec![(digest.clone(), read.0.clone(), read.1.clone())]
        );

        for foreign in [
            ContentKeyVersion::Digest("not-a-revision".to_string()),
            ContentKeyVersion::Object {
                e_tag: Some("\"etag\"".to_string()),
                version: None,
            },
        ] {
            assert_eq!(
                store
                    .replace_content_key(&digest, &foreign, &record(3)?)
                    .await?,
                ContentKeyCas::Lost
            );
            assert_eq!(
                store.delete_content_key(&digest, &foreign).await?,
                ContentKeyCas::Lost
            );
        }
        assert_eq!(store.get_content_key(&digest).await?, Some(read));
        Ok(())
    }

    #[tokio::test]
    async fn a_reclaimed_digest_refuses_its_earlier_tokens() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = store(&test_db);
        let digest = "a".repeat(64);
        store.create_content_key(&digest, &record(1)?).await?;
        let (_, first) = store
            .get_content_key(&digest)
            .await?
            .context("first life")?;
        assert_eq!(
            store.delete_content_key(&digest, &first).await?,
            ContentKeyCas::Applied
        );
        assert!(matches!(
            store.create_content_key(&digest, &record(2)?).await?,
            ContentKeyClaim::Created
        ));
        let (_, second) = store
            .get_content_key(&digest)
            .await?
            .context("second life")?;
        assert_ne!(first, second);
        assert_eq!(
            store
                .replace_content_key(&digest, &first, &record(3)?)
                .await?,
            ContentKeyCas::Lost
        );
        assert_eq!(
            store.delete_content_key(&digest, &first).await?,
            ContentKeyCas::Lost
        );
        assert_eq!(
            store.get_content_key(&digest).await?,
            Some((record(2)?, second))
        );
        Ok(())
    }

    #[tokio::test]
    async fn content_key_reads_skip_foreign_names_and_fail_on_a_corrupt_record()
    -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = store(&test_db);
        let good = "a".repeat(64);
        let corrupt = "b".repeat(64);
        store.create_content_key(&good, &record(1)?).await?;
        plant(
            db,
            "INSERT INTO content_keys (digest, record, revision) VALUES ($1, $2, 1)",
            &[
                "not-a-digest",
                serde_json::to_string_pretty(&record(2)?)?.as_str(),
            ],
        )
        .await?;
        let listed: Vec<String> = store
            .list_content_keys()
            .await?
            .into_iter()
            .map(|(digest, _, _)| digest)
            .collect();
        assert_eq!(listed, vec![good]);

        plant(
            db,
            "INSERT INTO content_keys (digest, record, revision) VALUES ($1, '{not json', 1)",
            &[corrupt.as_str()],
        )
        .await?;
        assert!(store.list_content_keys().await.is_err());
        assert!(store.get_content_key(&corrupt).await.is_err());
        assert!(
            store
                .create_content_key(&corrupt, &record(3)?)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_content_key_claims_admit_one_winner() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = Arc::new(store(&test_db));
        let digest: Arc<str> = Arc::from("c".repeat(64));
        let outcomes = race(|n| {
            let (store, digest) = (Arc::clone(&store), Arc::clone(&digest));
            async move {
                let mine = record(u128::try_from(n)?)?;
                let claim = store.create_content_key(&digest, &mine).await?;
                Ok((mine, claim))
            }
        })
        .await?;
        let (stored, version) = store
            .get_content_key(&digest)
            .await?
            .context("claimed digest")?;
        let mut created = 0;
        for (mine, claim) in outcomes {
            match claim {
                ContentKeyClaim::Created => {
                    created += 1;
                    assert_eq!(mine, stored, "the winner's record is the one stored");
                }
                ContentKeyClaim::AlreadyExists(seen, seen_version) => {
                    assert_eq!((seen, seen_version), (stored.clone(), version.clone()));
                }
            }
        }
        assert_eq!(created, 1);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_swaps_on_one_version_apply_once() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = Arc::new(store(&test_db));
        let digest: Arc<str> = Arc::from("c".repeat(64));
        store.create_content_key(&digest, &record(100)?).await?;
        let (_, read) = store.get_content_key(&digest).await?.context("claimed")?;
        let read = Arc::new(read);
        let outcomes = race(|n| {
            let (store, digest, read) =
                (Arc::clone(&store), Arc::clone(&digest), Arc::clone(&read));
            async move {
                let mine = record(u128::try_from(n)?)?;
                let cas = store.replace_content_key(&digest, &read, &mine).await?;
                Ok((mine, cas))
            }
        })
        .await?;
        let applied: Vec<ContentKeyRecord> = outcomes
            .into_iter()
            .filter_map(|(mine, cas)| (cas == ContentKeyCas::Applied).then_some(mine))
            .collect();
        let (stored, version) = store.get_content_key(&digest).await?.context("swapped")?;
        assert_eq!(
            applied,
            vec![stored],
            "one swap lands, and it is the one stored"
        );
        assert_ne!(version, *read);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_deletes_on_one_version_apply_once() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = Arc::new(store(&test_db));
        let digest: Arc<str> = Arc::from("c".repeat(64));
        store.create_content_key(&digest, &record(100)?).await?;
        let (_, read) = store.get_content_key(&digest).await?.context("claimed")?;
        let read = Arc::new(read);
        let outcomes = race(|_| {
            let (store, digest, read) =
                (Arc::clone(&store), Arc::clone(&digest), Arc::clone(&read));
            async move { store.delete_content_key(&digest, &read).await }
        })
        .await?;
        assert_eq!(
            outcomes
                .iter()
                .filter(|&&cas| cas == ContentKeyCas::Applied)
                .count(),
            1
        );
        assert!(store.get_content_key(&digest).await?.is_none());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_swap_racing_a_delete_lets_one_land() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = Arc::new(store(&test_db));
        let digest: Arc<str> = Arc::from("c".repeat(64));
        store.create_content_key(&digest, &record(100)?).await?;
        let (_, read) = store.get_content_key(&digest).await?.context("claimed")?;
        let read = Arc::new(read);
        let outcomes = race(|n| {
            let (store, digest, read) =
                (Arc::clone(&store), Arc::clone(&digest), Arc::clone(&read));
            async move {
                if n % 2 == 0 {
                    let mine = record(u128::try_from(n)?)?;
                    let cas = store.replace_content_key(&digest, &read, &mine).await?;
                    Ok((Some(mine), cas))
                } else {
                    Ok((None, store.delete_content_key(&digest, &read).await?))
                }
            }
        })
        .await?;
        let landed: Vec<Option<ContentKeyRecord>> = outcomes
            .into_iter()
            .filter_map(|(swap, cas)| (cas == ContentKeyCas::Applied).then_some(swap))
            .collect();
        let stored = store
            .get_content_key(&digest)
            .await?
            .map(|(record, _)| record);
        assert_eq!(landed, vec![stored]);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_plan_source_puts_keep_the_winner() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = Arc::new(store(&test_db));
        let outcomes = race(|n| {
            let store = Arc::clone(&store);
            async move {
                let mine = source(u128::try_from(n)?)?;
                let outcome = store.put_plan_source(&plan(1), &mine).await?;
                Ok((mine, outcome))
            }
        })
        .await?;
        let created: Vec<PlanSource> = outcomes
            .into_iter()
            .filter_map(|(mine, outcome)| (outcome == CreateOutcome::Created).then_some(mine))
            .collect();
        let stored = store.get_plan_source(&plan(1)).await?;
        assert_eq!(created, stored.into_iter().collect::<Vec<_>>());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_job_source_puts_keep_the_winner() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = Arc::new(store(&test_db));
        let outcomes = race(|n| {
            let store = Arc::clone(&store);
            async move {
                let mine =
                    serde_json::json!({ "job_id": job(1).as_str(), "writer": n.to_string() });
                let outcome = store.put_job_source(&job(1), &mine).await?;
                Ok((mine, outcome))
            }
        })
        .await?;
        let created: Vec<serde_json::Value> = outcomes
            .into_iter()
            .filter_map(|(mine, outcome)| (outcome == CreateOutcome::Created).then_some(mine))
            .collect();
        let stored = store.get_job_source(&job(1)).await?;
        assert_eq!(created, stored.into_iter().collect::<Vec<_>>());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_link_puts_create_one() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = Arc::new(store(&test_db));
        let outcomes = race(|_| {
            let store = Arc::clone(&store);
            async move { store.put_job_plan_link(&job(1), &plan(1)).await }
        })
        .await?;
        assert_eq!(
            outcomes
                .iter()
                .filter(|&&outcome| outcome == CreateOutcome::Created)
                .count(),
            1
        );
        assert_eq!(store.list_job_plan_links(&job(1)).await?, vec![plan(1)]);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_marker_writes_leave_one_marker() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = Arc::new(store(&test_db));
        race(|_| {
            let store = Arc::clone(&store);
            async move {
                store.write_cancel_marker(&plan(1)).await?;
                store.write_pending_source_marker(&plan(1)).await?;
                store
                    .write_job_plan_marker(&job(1), &plan(1), ExpiresAt::Never)
                    .await
            }
        })
        .await?;
        assert_eq!(store.list_cancel_markers().await?, vec![plan(1)]);
        assert_eq!(store.list_pending_source_markers().await?.len(), 1);
        assert_eq!(
            store.list_job_plan_markers(&job(1)).await?,
            vec![(plan(1), ExpiresAt::Never)]
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_manifest_writes_leave_one_whole_manifest() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = Arc::new(store(&test_db));
        let written = race(|n| {
            let store = Arc::clone(&store);
            async move {
                let mut mine = large_manifest(plan(1), u128::try_from(n)?)?;
                mine.plan_warnings = vec![format!("writer {n}")];
                store.put_plan(&mine).await?;
                Ok(mine)
            }
        })
        .await?;
        let stored = store.get_plan(&plan(1)).await?.context("the manifest")?;
        assert!(written.contains(&stored));
        Ok(())
    }

    /// The method gives the finished runs of each cell asked, in descending
    /// `job_id` order. A
    /// run with empty flags counts. The method leaves out a failed run, a
    /// finished row with no flags, a job that a plan reran, another cell, a job
    /// of another scope, and a job with no scope.
    #[tokio::test]
    async fn find_cell_results_gives_the_finished_runs_of_a_cell() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let (db, store) = (test_db.db(), store(&test_db));
        let scope = CellScope::Org(pipette_plan_types::OrgId::try_new("org_a")?);
        let other = CellScope::User(pipette_plan_types::UserId::try_new("user_b")?);
        let jobs = [
            (1, "a", Some(("finished", Some(&["os:android"][..])))),
            (2, "a", Some(("failed", None))),
            (3, "a", Some(("finished", None))),
            (4, "a", Some(("finished", Some(&["os:android"][..])))),
            (5, "b", Some(("finished", Some(&["os:ios"][..])))),
            (6, "b", Some(("finished", Some(&[][..])))),
            (
                7,
                "a",
                Some(("finished", Some(&["chip:sm8850", "os:android"][..]))),
            ),
            (8, "a", None),
            (10, "c", Some(("finished", Some(&["os:ios"][..])))),
        ];
        for (n, cell, outcome) in jobs {
            store
                .put_job_source(&job(n), &serde_json::json!({ "spec": n }))
                .await?;
            store.put_job_cell(&job(n), cell, &scope).await?;
            if let Some((outcome, run_flags)) = outcome {
                crate::testing::put_outcome(db, job(n).as_str(), outcome, run_flags).await?;
            }
        }
        // A job keeps the cell and the scope it was minted with.
        store.put_job_cell(&job(1), "c", &other).await?;
        let os = Some(&["os:android"][..]);
        store
            .put_job_source(&job(11), &serde_json::json!({ "spec": 11 }))
            .await?;
        store.put_job_cell(&job(11), "a", &other).await?;
        crate::testing::put_outcome(db, job(11).as_str(), "finished", os).await?;
        store
            .put_job_source(&job(12), &serde_json::json!({ "spec": 12 }))
            .await?;
        crate::job_cells::insert_job_cell(db, job(12).as_str(), "a", None).await?;
        crate::testing::put_outcome(db, job(12).as_str(), "finished", os).await?;
        let mut rerun = sample_manifest(plan(1), PlanStatus::Active, false)?;
        rerun.rerun_of.insert(job(9), job(4));
        store.put_plan(&rerun).await?;

        let found = store
            .find_cell_results(&["a".to_string(), "b".to_string()], &scope)
            .await?;
        let result = |n: u128, cell: &str, flags: &[&str]| CellResult {
            cell_digest: cell.to_string(),
            job_id: job(n),
            body: serde_json::json!({ "spec": n }),
            run_flags: flags.iter().map(|f| f.to_string()).collect(),
        };
        assert_eq!(
            found,
            [
                result(7, "a", &["chip:sm8850", "os:android"]),
                result(6, "b", &[]),
                result(5, "b", &["os:ios"]),
                result(1, "a", &["os:android"]),
            ]
        );
        assert!(store.find_cell_results(&[], &scope).await?.is_empty());
        Ok(())
    }

    /// The method gives the newest [`pipette_store::CELL_RESULTS_PER_CELL`]
    /// runs of a cell, and leaves out the older ones.
    #[tokio::test]
    async fn find_cell_results_gives_the_newest_runs_of_a_cell() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let (db, store) = (test_db.db(), store(&test_db));
        let scope = CellScope::Org(pipette_plan_types::OrgId::try_new("org_a")?);
        let limit = u128::try_from(pipette_store::CELL_RESULTS_PER_CELL)?;
        for n in 1..=limit + 2 {
            store
                .put_job_source(&job(n), &serde_json::json!({ "spec": n }))
                .await?;
            store.put_job_cell(&job(n), "a", &scope).await?;
            crate::testing::put_outcome(db, job(n).as_str(), "finished", Some(&[][..])).await?;
        }

        let found: Vec<JobId> = store
            .find_cell_results(&["a".to_string()], &scope)
            .await?
            .into_iter()
            .map(|result| result.job_id)
            .collect();

        let newest: Vec<JobId> = (3..=limit + 2).rev().map(job).collect();
        assert_eq!(found, newest);
        Ok(())
    }
}
