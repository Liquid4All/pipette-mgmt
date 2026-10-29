//! Plan-manifest types: the durable record of an ingested plan's identity,
//! lifecycle and progress (`plans/{plan_id}.json`), and the
//! `plans status --explain` / `--leases` reports read against it.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use pipette_plan_types::{
    BenchmarkId, BenchmarkType, CampaignId, Counts, ExpiresAt, FleetDetail, FleetStanding,
    JobCensus, JobId, NarrowingRung, NonEmptyTrimmedString, PlanId, PlanStatus, RosterStanding,
    SchedulingState, Sha256, Sha256Error, StarvedClauses, UserId, Warning,
};

#[cfg(any(test, feature = "testkit"))]
use pipette_plan_types::WarningKind;

use crate::canonical_json::sha256_hex;
use crate::todo_filename::Lease;

/// Refreshed whole by each `queue-maintenance` run, never by ingestion, so its
/// two writers never collide over the rest of the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSnapshot {
    /// Dates `counts` and `starved` together.
    pub computed_at: DateTime<Utc>,
    /// Six buckets that partition `job_ids`; an `expired_jobs` entry counts `failed` first.
    pub counts: Counts,
    /// Outstanding jobs matching no working client, grouped by identical
    /// requirement set: the refreshed form of the ingestion-time [`Warning`]s.
    pub starved: Vec<StarvedGroup>,
}

/// Earliest and latest deadline across a plan's jobs, frozen at ingestion, under
/// the names a manifest stores; the body adds `_expires_at` to each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobExpiry {
    pub earliest: ExpiresAt,
    pub latest: ExpiresAt,
}

/// A plan's own lapse record for a job: its horizon passed with no answer.
/// Stored as `horizon`, which the body names `expires_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpiredJob {
    #[serde(with = "past_horizon")]
    pub horizon: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
}

/// A lapse's horizon in [`ExpiresAt`]'s compact form; `never` cannot have passed.
mod past_horizon {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};

    use pipette_plan_types::ExpiresAt;

    pub(super) fn serialize<S: Serializer>(
        at: &DateTime<Utc>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        ExpiresAt::At(*at).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DateTime<Utc>, D::Error> {
        match ExpiresAt::deserialize(deserializer)? {
            ExpiresAt::At(at) => Ok(at),
            ExpiresAt::Never => Err(D::Error::custom("a lapse's horizon cannot be `never`")),
        }
    }
}

/// Outstanding, unmatched jobs sharing an identical requirement set, echoed as
/// stored. All six clauses are part of the identity: jobs that differ only in
/// what they exclude are starved for different reasons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarvedGroup {
    #[serde(flatten)]
    pub clauses: StarvedClauses,
    pub job_ids: Vec<JobId>,
}

/// The outcome of a plan job, from its submission record's `message_type`.
/// Final once read, as moving the record to `processed/` does not change it;
/// only `unverified promote` writes over one, and the counts keep the first read.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumIter,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SettledOutcome {
    /// A record whose `message_type` is not `failure`, or that has none.
    Finished,
    /// A record whose `message_type` is `failure`.
    Failed,
}

impl SettledOutcome {
    /// Every reader of an outcome uses this rule. A record with no
    /// `message_type` predates the field, and is a success.
    pub fn from_message_type(message_type: Option<&str>) -> Self {
        match message_type {
            Some("failure") => Self::Failed,
            _ => Self::Finished,
        }
    }

    /// The name of the outcome, the same as its serde name.
    pub fn label(self) -> &'static str {
        self.into()
    }
}

/// The durable plan manifest. Its `job_ids` list is the **only** record of
/// which jobs belong to the plan: job bodies carry no `plan_id`.
///
/// Deliberately open (no `deny_unknown_fields`): ingestion and maintenance own
/// disjoint parts and tolerate each other's. Reads only: `put_plan` writes the
/// whole object, so deploy every writer at a build that knows each field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanManifest {
    pub plan_id: PlanId,
    pub status: PlanStatus,
    pub created_at: DateTime<Utc>,
    /// The **user account** a plan is attributed to, or `None`: the HTTP routes
    /// record the signer, the `campaigns` commands record `None`. A record, not a
    /// proof: an older build let a `submit_any_plans` holder name any account. A
    /// stored value that is not a [`UserId`] reads as absent ([`lenient_submitter`]).
    /// It grants reach only over a plan outside a campaign; for a campaign's plan,
    /// access to the campaign decides.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_submitter"
    )]
    pub submitted_by: Option<UserId>,
    /// The campaign this plan was filed under, or `None` for a plan filed
    /// outside one (before campaigns, or a one-off from an older build).
    ///
    /// **Written at ingestion and never changed**: no route, command or store
    /// method moves it, which is what makes "no two campaigns hold one plan"
    /// true. Jobs still merge across campaigns; only the plan is exclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign_id: Option<CampaignId>,
    /// The plan this one supersedes in its campaign; `None` for the first, and
    /// outside a campaign. Points backwards because that direction is immutable:
    /// a forward list on the campaign would be rewritten on every revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_plan_id: Option<PlanId>,
    /// The digest of the plan document at `plan-sources/{plan_id}.toml`; `None`
    /// for a plan filed as job bodies. Never changed after `creating`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sha256: Option<Sha256>,
    /// The digest of the document as submitted, when the store keeps another
    /// text (a model carried a token, stored as its vault arm). A resend matches
    /// it; the submitted text is not stored, because it holds the token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_sha256: Option<Sha256>,
    /// The planner's warnings on the plan document, in order. Empty for a plan
    /// filed as job bodies. Never changed after `creating`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan_warnings: Vec<String>,
    /// Each benchmark's type, resolved from the filer's catalog at ingestion and
    /// never changed: every later parse of the document types its cells by it.
    /// Empty only on a plan filed before the field, which reads typed by each
    /// id's prefix, as it was filed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub benchmark_types: BTreeMap<BenchmarkId, BenchmarkType>,
    /// Authoritative plan membership (see the struct doc). A job may sit on several
    /// manifests; the `job-plans/` markers give the job→plans direction.
    pub job_ids: Vec<JobId>,
    /// The content digest each job was placed under, so a variant edit can
    /// carry a finished job over: its body left the queue with it. Recorded at
    /// ingestion, for a campaign's plans filed from a document only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub content_keys: BTreeMap<JobId, String>,
    /// The job each re-run job of this plan runs again, keyed by the re-run
    /// job. Written at ingestion, and kept by a revision that carries the job.
    /// The scorer copies it onto the job's warehouse rows.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rerun_of: BTreeMap<JobId, JobId>,
    /// Frozen ingestion-time warnings, grouped by [`WarningKind`] and then by
    /// requirement set or model.
    #[serde(default)]
    pub warnings: Vec<Warning>,
    /// Frozen ingestion-time job expiry range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_expiry: Option<JobExpiry>,
    /// The jobs whose horizon for **this plan** lapsed, written by the lapse pass
    /// and never removed: the plan counts each `failed` and stops waiting on it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub expired_jobs: BTreeMap<JobId, ExpiredJob>,
    /// The outcome of each job whose record `queue-maintenance` read. A cache: a
    /// writer that drops it costs reads, not correctness.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settled_jobs: BTreeMap<JobId, SettledOutcome>,
    /// Progress snapshot; absent until the first `queue-maintenance` run writes
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_snapshot: Option<ProgressSnapshot>,
    /// When the plan latched a terminal status (`complete` / `cancelled`); set
    /// by `queue-maintenance`. `None` while non-terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_at: Option<DateTime<Utc>>,
}

/// The note of a plan and its revision change. A store keeps them apart from
/// the manifest (`PlanStore::put_plan_notes`). A rewrite of the manifest cannot
/// change them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanNotes {
    /// Free text from the filer that tells why the plan was filed. Only the
    /// `plan_id` identifies the plan. The note is the only field of a plan that
    /// a reader can change after filing (`PlanStore::set_plan_note`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The change that a campaign revision made. The server writes it at filing,
    /// and the value does not change after that. The value is `None` for a plan
    /// filed whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<pipette_plan_types::RevisionChange>,
}

impl PlanNotes {
    pub fn is_empty(&self) -> bool {
        self.note.is_none() && self.change.is_none()
    }
}

/// The part of a plan that a listing row shows. It holds no job, so a store
/// cannot write it back. A store can select these fields without the rest of
/// the plan.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PlanSummary {
    pub plan_id: PlanId,
    pub note: Option<String>,
    /// Not in the summary JSON: the SQL store reads the column as it does a header's.
    #[serde(skip)]
    pub change: Option<pipette_plan_types::RevisionChange>,
    pub status: PlanStatus,
    pub created_at: DateTime<Utc>,
    /// Read as [`PlanManifest::submitted_by`] reads it.
    #[serde(default, deserialize_with = "lenient_submitter")]
    pub submitted_by: Option<UserId>,
    pub campaign_id: Option<CampaignId>,
    /// `None` until `queue-maintenance` counts the plan.
    pub counts: Option<Counts>,
}

/// A stored note is free text. Return the note with its whitespace trimmed, or
/// `None` if the note is blank.
fn label(stored: Option<String>) -> Option<NonEmptyTrimmedString> {
    stored.and_then(|note| NonEmptyTrimmedString::try_new(note).ok())
}

/// A manifest holds no note or change, so the summary has none. The SQL store
/// selects them from their own columns.
impl From<PlanManifest> for PlanSummary {
    fn from(manifest: PlanManifest) -> Self {
        Self {
            plan_id: manifest.plan_id,
            note: None,
            change: None,
            status: manifest.status,
            created_at: manifest.created_at,
            submitted_by: manifest.submitted_by,
            campaign_id: manifest.campaign_id,
            counts: manifest.progress_snapshot.map(|snapshot| snapshot.counts),
        }
    }
}

impl PlanSummary {
    /// The body twin, with the separately read cancel marker folded in; a
    /// `From` could not take the marker.
    pub fn view(self, cancel_requested: bool) -> pipette_plan_types::PlanSummary {
        pipette_plan_types::PlanSummary {
            plan_id: self.plan_id,
            note: label(self.note),
            change: self.change,
            status: self.status,
            created_at: self.created_at,
            submitted_by: self.submitted_by,
            campaign_id: self.campaign_id,
            cancel_requested,
            counts: self.counts,
        }
    }
}

/// A manifest without its per-job fields: what a plan read shows
/// ([`PlanView`](pipette_plan_types::PlanView)). Like [`PlanSummary`], it
/// cannot be written back.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PlanHeader {
    pub plan_id: PlanId,
    /// Not in the stored JSON: the SQL store reads its column.
    #[serde(skip)]
    pub note: Option<String>,
    /// Not in the stored JSON: the SQL store reads its column.
    #[serde(skip)]
    pub change: Option<pipette_plan_types::RevisionChange>,
    pub status: PlanStatus,
    pub created_at: DateTime<Utc>,
    #[serde(default, deserialize_with = "lenient_submitter")]
    pub submitted_by: Option<UserId>,
    #[serde(default)]
    pub campaign_id: Option<CampaignId>,
    #[serde(default)]
    pub previous_plan_id: Option<PlanId>,
    #[serde(default)]
    pub plan_warnings: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<Warning>,
    #[serde(default)]
    pub job_expiry: Option<JobExpiry>,
    #[serde(default)]
    pub progress_snapshot: Option<ProgressSnapshot>,
    #[serde(default)]
    pub terminal_at: Option<DateTime<Utc>>,
}

impl From<PlanManifest> for PlanHeader {
    fn from(manifest: PlanManifest) -> Self {
        Self::new(manifest, PlanNotes::default())
    }
}

impl PlanHeader {
    pub fn new(manifest: PlanManifest, notes: PlanNotes) -> Self {
        Self {
            plan_id: manifest.plan_id,
            note: notes.note,
            change: notes.change,
            status: manifest.status,
            created_at: manifest.created_at,
            submitted_by: manifest.submitted_by,
            campaign_id: manifest.campaign_id,
            previous_plan_id: manifest.previous_plan_id,
            plan_warnings: manifest.plan_warnings,
            warnings: manifest.warnings,
            job_expiry: manifest.job_expiry,
            progress_snapshot: manifest.progress_snapshot,
            terminal_at: manifest.terminal_at,
        }
    }

    /// The plan as a reader at `detail` reads it, with the separately read
    /// cancel marker folded in.
    pub fn view(self, cancel_requested: bool, detail: FleetDetail) -> pipette_plan_types::PlanView {
        pipette_plan_types::PlanView {
            fleet_detail: FleetDetail::Full,
            plan_id: self.plan_id,
            note: label(self.note),
            change: self.change,
            status: self.status,
            created_at: self.created_at,
            submitted_by: self.submitted_by,
            campaign_id: self.campaign_id,
            previous_plan_id: self.previous_plan_id,
            job_expiry: self.job_expiry.map(Into::into),
            cancel_requested,
            warnings: self.warnings,
            plan_warnings: self.plan_warnings,
            progress_snapshot: self.progress_snapshot.map(Into::into),
            terminal_at: self.terminal_at,
        }
        .at(detail)
    }
}

impl PlanManifest {
    /// [`PlanHeader::view`] of this manifest: no job list or per-job maps, no
    /// `source_sha256` (the document's storage record), and no `submitted_sha256`,
    /// which digests a text that held a token.
    pub fn view(
        self,
        notes: PlanNotes,
        cancel_requested: bool,
        detail: FleetDetail,
    ) -> pipette_plan_types::PlanView {
        PlanHeader::new(self, notes).view(cancel_requested, detail)
    }

    /// [`PlanSummary::view`] of this manifest, its note and change taken from
    /// `notes`.
    pub fn summary(
        self,
        notes: PlanNotes,
        cancel_requested: bool,
    ) -> pipette_plan_types::PlanSummary {
        PlanSummary {
            note: notes.note,
            change: notes.change,
            ..self.into()
        }
        .view(cancel_requested)
    }

    /// The plan a campaign revision is read from, as its readers name it.
    pub fn revision_base(&self, notes: PlanNotes) -> pipette_plan_types::RevisionBase {
        pipette_plan_types::RevisionBase {
            plan_id: self.plan_id.clone(),
            note: label(notes.note),
            change: notes.change,
        }
    }
}

impl From<JobExpiry> for pipette_plan_types::JobExpiry {
    fn from(expiry: JobExpiry) -> Self {
        Self {
            earliest_expires_at: expiry.earliest.instant(),
            latest_expires_at: expiry.latest.instant(),
        }
    }
}

impl From<ExpiredJob> for pipette_plan_types::ExpiredJob {
    fn from(expired: ExpiredJob) -> Self {
        Self {
            expires_at: expired.horizon,
            recorded_at: expired.recorded_at,
        }
    }
}

impl From<ProgressSnapshot> for pipette_plan_types::ProgressSnapshot {
    fn from(snapshot: ProgressSnapshot) -> Self {
        Self {
            computed_at: snapshot.computed_at,
            counts: snapshot.counts,
            starved_groups: snapshot
                .starved
                .into_iter()
                .map(|group| pipette_plan_types::StarvedGroup {
                    clauses: Some(group.clauses),
                    job_ids: group.job_ids,
                })
                .collect(),
        }
    }
}

/// The key of a model token in a plan document.
const TOKEN_KEY: &str = "auth_token";

/// Whether `text` names an `auth_token` anywhere, comments and strings too: a
/// stored document holds no token.
pub fn names_a_token(text: &str) -> bool {
    text.contains(TOKEN_KEY)
}

/// A plan document, stored at `plan-sources/{plan_id}.toml`. Ingestion may store
/// a changed text (imported models as vault arms, no tokens) instead of the
/// submitted one; [`sha256`](Self::sha256) digests exactly the stored text.
#[derive(Clone, PartialEq, Eq)]
pub struct PlanSource {
    text: String,
    sha256: Sha256,
}

impl PlanSource {
    pub fn new(text: String) -> Result<Self, Sha256Error> {
        let sha256 = Sha256::try_new(sha256_hex(&text))?;
        Ok(Self { text, sha256 })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn names_a_token(&self) -> bool {
        names_a_token(&self.text)
    }

    pub fn sha256(&self) -> &Sha256 {
        &self.sha256
    }
}

/// Shows the digest and the length only. A document can be large, and a log
/// line that prints a submission must not print the whole document.
impl std::fmt::Debug for PlanSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanSource")
            .field("sha256", &self.sha256)
            .field("len", &self.text.len())
            .finish()
    }
}

/// Read a manifest's `submitted_by`, treating a value that is not a [`UserId`]
/// as absent: older manifests carry the `ev1_…` of the filing client.
///
/// Absent rather than an error, because a refusal costs the whole manifest: the
/// plan drops out of listings and every maintenance pass skips it. The value is
/// lost on the next rewrite. A non-string still fails: nothing writes one.
fn lenient_submitter<'de, D: Deserializer<'de>>(d: D) -> Result<Option<UserId>, D::Error> {
    let Some(raw) = Option::<String>::deserialize(d)? else {
        return Ok(None);
    };
    match UserId::try_new(raw.as_str()) {
        Ok(user_id) => Ok(Some(user_id)),
        // `debug!`: every read of a dated manifest reaches this, and a
        // `deserialize_with` cannot see the sibling `plan_id`.
        Err(_) => {
            tracing::debug!(
                submitted_by = %raw,
                "plan manifest submitter is not a user id, read as absent"
            );
            Ok(None)
        }
    }
}

/// One cached [`StarvedGroup`], explained against the live fleet. The clauses
/// date from the snapshot and every count is live; a live match means a client
/// registered since, and the next `queue-maintenance` run clears the group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StarvedExplanation {
    /// The clause set the group's jobs share
    /// ([`describe_clauses`](crate::matching::describe_clauses)).
    pub description: String,
    pub job_count: usize,
    /// The rule that proves no client can ever match, or `None`. Read from a
    /// real body, not the cached group: `requirement_key` records a malformed
    /// clause as an absent one, so a rebuilt body would name the wrong rule.
    pub never_matches: Option<String>,
    /// The clause ladder over the working fleet. Empty when the group carries
    /// no `requires`, and when `never_matches` settles the question.
    pub ladder: Vec<NarrowingRung>,
    /// Roster standing, for a group whose jobs name clients by id.
    pub roster: Option<RosterStanding>,
    /// Working clients the whole clause set matches now — the ones the claim
    /// path would admit, so this is the count that clears the group.
    pub live_working_client_count: usize,
    /// Approved clients the whole clause set matches now, granted or not. A
    /// value above `live_working_client_count` means a `perform_work` grant
    /// clears the group, where editing a clause would not.
    pub live_approved_client_count: usize,
    /// Registered clients the whole clause set matches now, approved or not. A
    /// value above `live_approved_client_count` means an approval clears the
    /// group.
    pub live_registered_client_count: usize,
}

/// Why a plan's jobs are not scheduled — the `plans status --explain` report
/// (`docs/cli.md`). Status and cancellation are inputs to the verdict, not
/// fields: every caller already holds the plan's read projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanExplanation {
    pub plan_id: PlanId,
    /// Every count below dates from this instant, so the snapshot's age is
    /// measured against it, not the render-time clock.
    pub read_at: DateTime<Utc>,
    /// When `queue-maintenance` last computed the plan's snapshot. `None` when
    /// no pass ran for this plan, which by itself explains a plan that reads
    /// `active` and runs nothing.
    pub snapshot_computed_at: Option<DateTime<Utc>>,
    pub census: JobCensus,
    pub fleet: FleetStanding,
    /// One entry per group of the cached snapshot, whatever the verdict: a
    /// partially starved plan clears the chain and still holds groups worth
    /// reading.
    pub groups: Vec<StarvedExplanation>,
    /// True when the live body scan settled [`SchedulingState::Starved`] past a
    /// cached starvation set that names none of the jobs: the snapshot is stale
    /// or absent.
    pub starved_by_live_scan: bool,
    pub verdict: SchedulingState,
    /// Which clients hold the plan's jobs, from the `leased/` listing the census
    /// counts.
    pub leases: PlanLeases,
}

impl PlanExplanation {
    /// The report as the explain route serves it to a reader at `detail`.
    pub fn view(self, detail: FleetDetail) -> pipette_plan_types::PlanExplanation {
        pipette_plan_types::PlanExplanation {
            plan_id: self.plan_id.clone(),
            scheduling_state: self.verdict,
            headline: self.headline(detail),
            read_at: self.read_at,
            snapshot_computed_at: self.snapshot_computed_at,
            census: self.census,
            starved_by_live_scan: self.starved_by_live_scan,
            fleet_detail: FleetDetail::Full,
            fleet: Some(self.fleet),
            groups: self
                .groups
                .into_iter()
                .map(|group| pipette_plan_types::StarvedGroupExplanation {
                    description: Some(group.description),
                    job_count: group.job_count,
                    never_matches: group.never_matches,
                    ladder: Some(group.ladder),
                    roster: group.roster,
                    live_working_client_count: Some(group.live_working_client_count),
                    live_approved_client_count: Some(group.live_approved_client_count),
                    live_registered_client_count: Some(group.live_registered_client_count),
                })
                .collect(),
        }
        .at(detail)
    }

    /// The verdict as one operator-facing sentence with its supporting counts.
    /// Written here, not in the renderer, so every surface states one answer.
    pub fn headline(&self, detail: FleetDetail) -> String {
        let c = &self.census;
        match self.verdict {
            SchedulingState::StillCreating => format!(
                "the plan is still `creating`, so its {} job(s) sit in `todo/tmp/` and no client can see them",
                c.total
            ),
            SchedulingState::CancelPending => {
                "teardown owns the plan; `queue-maintenance` retires its jobs".to_string()
            }
            SchedulingState::LeasesLapsed => format!(
                "all {} outstanding job(s) sit behind a lapsed lease. The holders stopped heartbeating. A `queue-maintenance` run returns the jobs to the queue.",
                c.lapsed_leases
            ),
            SchedulingState::NothingOutstanding => {
                // A live lease decided this gate, so a strand beside it is the
                // detail the verdict does not carry.
                let strand = match c.lapsed_leases {
                    0 => String::new(),
                    n => format!(" ({n} behind a lapsed lease)"),
                };
                format!(
                    "no job is in `avail/`: {} leased{strand}, {} finished or retired",
                    c.leased, c.gone
                )
            }
            SchedulingState::Expired => format!(
                "all {} available job(s) are past their deadline, so the claim path refuses them",
                c.expired
            ),
            SchedulingState::NotIndexed => format!(
                "0 of {} claimable job(s) are indexed, so no client carries an `eligible/` marker. Run `queue-maintenance`",
                c.claimable
            ),
            // One fixed sentence ahead of the arms below: they read the live
            // fleet, which a caller below `read_clients` may not see.
            SchedulingState::Starved if detail == FleetDetail::Withheld => format!(
                "all {} claimable job(s) match no working client. Whether the snapshot is current, and whether any group matches a working client now, is withheld: reading it needs read_clients or read_any_clients. A queue-maintenance run clears a group that recovered",
                c.claimable
            ),
            // The live scan reached this past a cached set naming only some of
            // these jobs, so the stale cache is part of the answer.
            SchedulingState::Starved if self.starved_by_live_scan && self.groups.is_empty() => format!(
                "all {} claimable job(s) match no working client. No cached starved group names them, so the snapshot is stale or absent. Run `queue-maintenance` to refresh it and name the clause to fix",
                c.claimable
            ),
            SchedulingState::Starved if self.starved_by_live_scan => format!(
                "all {} claimable job(s) match no working client. The {} starved group(s) below name only some of them, so the snapshot is stale. Run `queue-maintenance` to refresh it",
                c.claimable,
                self.groups.len()
            ),
            // Without a snapshot there is no group to point at; the frozen
            // ingest warnings take the past tense, as no pass re-tested them.
            SchedulingState::Starved if self.groups.is_empty() => format!(
                "all {} claimable job(s) matched no working client when ingestion ran, by the warnings that it froze. No `queue-maintenance` pass re-tested them against the fleet. Run one",
                c.claimable
            ),
            // The count is the snapshot's and the group notes the live fleet's,
            // so the tense follows the source.
            SchedulingState::Starved => match self
                .groups
                .iter()
                .filter(|group| group.live_working_client_count > 0)
                .count()
            {
                0 => format!(
                    "all {} claimable job(s) match no working client. Read the starved groups below",
                    c.claimable
                ),
                recovered if recovered == self.groups.len() => format!(
                    "all {} claimable job(s) matched no working client when the snapshot was computed. Every group matches a working client now. The next `queue-maintenance` run clears them.",
                    c.claimable
                ),
                recovered => format!(
                    "all {} claimable job(s) matched no working client when the snapshot was computed. {recovered} of {} group(s) match a working client now. The next `queue-maintenance` run clears those.",
                    c.claimable,
                    self.groups.len()
                ),
            },
            SchedulingState::ClientsBlocked if detail == FleetDetail::Withheld =>
                "no client can take a claimable job. Which fleet gate blocks them is withheld: reading it needs read_clients or read_any_clients".to_string(),
            SchedulingState::ClientsBlocked if self.fleet.approved == 0 => format!(
                "no approved client is registered ({} client(s) await approval)",
                self.fleet.registered
            ),
            // The gate an approved fleet fails by default: a registration
            // grants `submit_results` alone. The fix is a grant, so the
            // sentence names it.
            SchedulingState::ClientsBlocked if self.fleet.working == 0 => format!(
                "no approved client holds `perform_work`, so the claim path refuses all {} of them: `pipette-mgmt clients permission add <client_id> perform_work`",
                self.fleet.approved
            ),
            // Only the causes that apply: a fleet blocked by suspension alone
            // must not read as one that also holds denials.
            SchedulingState::ClientsBlocked => {
                let causes = [
                    (self.fleet.suspended > 0)
                        .then(|| format!("{} suspended", self.fleet.suspended)),
                    (self.fleet.pending_reindex > 0).then(|| {
                        format!(
                            "{} pending an eligible-index rebuild",
                            self.fleet.pending_reindex
                        )
                    }),
                    // The census cannot tell "matches none" from "denied the
                    // ones it matches", so the cause names the standing.
                    (self.fleet.unblocked > 0).then(|| {
                        format!("{} unblocked but able to take none", self.fleet.unblocked)
                    }),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ");
                format!("no client can take a claimable job: {causes}")
            }
            SchedulingState::QueueMoved => format!(
                "all {} claimable job(s) left `avail/` while the report read them. The scan reached no evidence, so re-run the report.",
                c.claimable
            ),
            SchedulingState::Schedulable if detail == FleetDetail::Withheld => format!(
                "a claimable job can reach a client: {} claimable job(s). The count of unblocked clients is withheld: reading it needs read_clients or read_any_clients. The server cannot see whether a client polls",
                c.claimable
            ),
            SchedulingState::Schedulable => format!(
                "a claimable job can reach a client: {} claimable job(s) and {} unblocked client(s). The server cannot see whether a client polls.",
                c.claimable, self.fleet.unblocked
            ),
        }
    }
}

/// Which clients hold a plan's jobs — the `plans status --leases` report
/// (`docs/cli.md`). A live reading, unlike the cached [`ProgressSnapshot`]'s
/// `running` count, so the two can disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanLeases {
    /// Leases whose holder still heartbeats — the plan's work in flight. One
    /// entry per job, ordered by client id, then job id.
    pub live: Vec<Lease>,
    /// Leases past their deadline, in the same order. Each one strands its job:
    /// the job sits outside `avail/`, where `claim` cannot reach it, until a
    /// `queue-maintenance` run recycles the lease.
    pub lapsed: Vec<Lease>,
    /// `leased/` keys that name no job, which the lists above cannot account
    /// for; the same caveat as [`JobCensus::unparseable_leased_keys`].
    pub unparseable_keys: usize,
    /// The instant every deadline in the two lists is measured against: a lease
    /// lapsed by seconds and one lapsed by an hour mean different things.
    pub read_at: DateTime<Utc>,
}

impl PlanLeases {
    /// Collapse `leases` to one entry per job (latest `lease_expiry`), then
    /// split on the deadline at `now`.
    ///
    /// Collapse first: a renewal renames the key, so one listing can hold both
    /// keys, and a job with one live lease must read as held, not stranded. Key
    /// on `job_id` alone: a recycle and re-claim between two partition listings
    /// put two clients on one job, and the later deadline names the holder.
    pub fn new(leases: Vec<Lease>, unparseable_keys: usize, now: DateTime<Utc>) -> Self {
        let latest: BTreeMap<JobId, Lease> =
            leases
                .into_iter()
                .fold(BTreeMap::new(), |mut latest, lease| {
                    match latest.get(&lease.job_id) {
                        Some(held) if held.lease_expiry >= lease.lease_expiry => latest,
                        _ => {
                            latest.insert(lease.job_id.clone(), lease);
                            latest
                        }
                    }
                });
        let (mut live, mut lapsed): (Vec<Lease>, Vec<Lease>) = latest
            .into_values()
            .partition(|lease| lease.lease_expiry > now);
        // Sorted so repeated runs print the same order: neither the backend walk
        // nor the fold fixes the holder order.
        let by_holder = |lease: &Lease| (lease.client_id.clone(), lease.job_id.clone());
        live.sort_by_key(by_holder);
        lapsed.sort_by_key(by_holder);
        Self {
            live,
            lapsed,
            unparseable_keys,
            read_at: now,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty() && self.lapsed.is_empty()
    }
}

/// Whether `stamp` is at least `gate` (`plan_creating_max_age_secs`) old at
/// `now`: the one age rule ingestion and `queue-maintenance` share. A future
/// stamp reads as young, the safe direction.
pub fn past_gate(stamp: DateTime<Utc>, now: DateTime<Utc>, gate: std::time::Duration) -> bool {
    now.signed_duration_since(stamp)
        .to_std()
        .is_ok_and(|age| age >= gate)
}

/// A well-formed `UserId` for the fixtures below: the type enforces the
/// prefix and the UUID shape, so a fixture cannot use a bare label.
#[cfg(any(test, feature = "testkit"))]
const TEST_USER: &str = "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";

/// A well-formed `CampaignId` and the plan a fixture supersedes, on the same
/// rule.
#[cfg(any(test, feature = "testkit"))]
const TEST_CAMPAIGN: &str = "campaign_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";
#[cfg(any(test, feature = "testkit"))]
const TEST_PREVIOUS_PLAN: &str = "plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";
/// A well-formed source digest for a fixture that records a plan document.
#[cfg(any(test, feature = "testkit"))]
const TEST_SOURCE_SHA256: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

/// A manifest with a fixed `created_at`, shared by the `PlanStore` round-trip
/// tests of both backends.
#[cfg(any(test, feature = "testkit"))]
pub fn sample_manifest(
    plan_id: PlanId,
    status: PlanStatus,
    with_snapshot: bool,
) -> anyhow::Result<PlanManifest> {
    let created_at: DateTime<Utc> = "2026-07-20T17:55:00Z".parse()?;
    Ok(PlanManifest {
        plan_id,
        status,
        created_at,
        // Populated, not `None`: `a_plan_view_withholds_only_the_manifest_fields`
        // diffs the serialized key sets, and a skipped key would hide a field
        // missing from `PlanView`.
        submitted_by: Some(UserId::try_new(TEST_USER)?),
        // Populated for `submitted_by`'s reason.
        campaign_id: Some(CampaignId::try_new(TEST_CAMPAIGN)?),
        previous_plan_id: Some(PlanId::try_new(TEST_PREVIOUS_PLAN)?),
        source_sha256: Some(Sha256::try_new(TEST_SOURCE_SHA256)?),
        // Populated for `submitted_by`'s reason.
        submitted_sha256: Some(Sha256::try_new("e".repeat(64))?),
        // Populated for `submitted_by`'s reason: an empty list is skipped.
        plan_warnings: vec!["variant 0: the roster adds nobody".to_string()],
        benchmark_types: Default::default(),
        job_ids: vec![JobId::new_unchecked("job-1"), JobId::new_unchecked("job-2")],
        // Populated for `submitted_by`'s reason: an empty map is skipped.
        content_keys: BTreeMap::from([
            (JobId::new_unchecked("job-1"), "1".repeat(64)),
            (JobId::new_unchecked("job-2"), "2".repeat(64)),
        ]),
        // Populated for `submitted_by`'s reason: an empty map is skipped.
        rerun_of: BTreeMap::from([(JobId::new_unchecked("job-2"), JobId::new_unchecked("job-0"))]),
        warnings: vec![Warning {
            kind: WarningKind::Fleet,
            message: "2 jobs requiring os:macos match no working client".to_string(),
            job_ids: vec![JobId::new_unchecked("job-2")],
        }],
        // Populated for `submitted_by`'s reason: an empty map is skipped.
        expired_jobs: BTreeMap::from([(
            JobId::new_unchecked("job-2"),
            ExpiredJob {
                horizon: "2026-08-19T17:55:00Z".parse()?,
                recorded_at: created_at,
            },
        )]),
        job_expiry: Some(JobExpiry {
            earliest: ExpiresAt::At("2026-08-19T17:55:00Z".parse()?),
            latest: ExpiresAt::At("2026-09-18T17:55:00Z".parse()?),
        }),
        // Populated for `submitted_by`'s reason: an empty map is skipped.
        settled_jobs: BTreeMap::from([(JobId::new_unchecked("job-1"), SettledOutcome::Finished)]),
        progress_snapshot: with_snapshot.then(|| ProgressSnapshot {
            computed_at: created_at,
            counts: Counts {
                total: 2,
                finished: 1,
                running: 0,
                available: 1,
                failed: 0,
                cancelled: 0,
            },
            starved: vec![StarvedGroup {
                clauses: StarvedClauses {
                    requires: vec!["os:macos".to_string()],
                    any_of: vec![],
                    clients: vec![],
                    exclude_clients: vec!["ev1_reserved".to_string()],
                    excludes: vec!["chip:a15".to_string()],
                    not_all_of: vec![vec![
                        "runtime:llama_cpp".to_string(),
                        "chip:a16".to_string(),
                    ]],
                },
                job_ids: vec![JobId::new_unchecked("job-2")],
            }],
        }),
        // A terminal plan carries `terminal_at`, so round-trips cover both ways.
        terminal_at: status.is_terminal().then_some(created_at),
    })
}

/// A starved-verdict report, one group per `live_matches` entry, which sets its
/// counts; the headline reads only `live_working_client_count`.
#[cfg(any(test, feature = "testkit"))]
pub fn starved_report(live_matches: &[usize], claimable: usize) -> PlanExplanation {
    PlanExplanation {
        plan_id: PlanId::from_uuid(uuid::Uuid::nil()),
        read_at: Utc::now(),
        snapshot_computed_at: None,
        census: JobCensus {
            total: claimable,
            available: claimable,
            expired: 0,
            claimable,
            indexed: claimable,
            leased: 0,
            lapsed_leases: 0,
            gone: 0,
            denied: 0,
            unparseable_avail_keys: 0,
            unparseable_leased_keys: 0,
        },
        fleet: FleetStanding {
            registered: 1,
            approved: 1,
            working: 1,
            suspended: 0,
            pending_reindex: 0,
            unblocked: 1,
        },
        groups: live_matches
            .iter()
            .map(|live| StarvedExplanation {
                description: "requiring os:ios".to_string(),
                job_count: 1,
                never_matches: None,
                ladder: vec![],
                roster: None,
                live_working_client_count: *live,
                live_approved_client_count: *live,
                live_registered_client_count: *live,
            })
            .collect(),
        starved_by_live_scan: false,
        verdict: SchedulingState::Starved,
        leases: PlanLeases::new(vec![], 0, Utc::now()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rstest::rstest;
    use serde_json::{Value, json};

    use pipette_plan_types::ClientId;

    use super::*;

    #[rstest]
    #[case::blank("  ", None)]
    #[case::padded(" smoke ", Some("smoke"))]
    fn a_stored_note_reads_trimmed_and_blank_as_none(
        #[case] stored: &str,
        #[case] shown: Option<&str>,
    ) -> anyhow::Result<()> {
        let note = label(Some(stored.to_owned()));
        assert_eq!(note.as_ref().map(NonEmptyTrimmedString::as_str), shown);
        Ok(())
    }

    /// A blocked fleet names the gate that blocked it: `approved: 0` sends the
    /// reader to approval, `approved: n, working: 0` to `clients permission add`,
    /// since a registration grants `submit_results` alone.
    #[rstest]
    #[case::nobody_approved(0, 0, "await approval")]
    #[case::nobody_granted(3, 0, "no approved client holds `perform_work`")]
    #[case::blocked_by_suspension(3, 3, "no client can take a claimable job")]
    fn clients_blocked_headline_names_the_gate(
        #[case] approved: usize,
        #[case] working: usize,
        #[case] expected: &str,
    ) {
        let mut report = starved_report(&[], 4);
        report.verdict = SchedulingState::ClientsBlocked;
        report.fleet = FleetStanding {
            registered: 4,
            approved,
            working,
            suspended: working,
            pending_reindex: 0,
            unblocked: 0,
        };
        let headline = report.headline(FleetDetail::Full);
        assert!(headline.contains(expected), "{headline}");
        assert_eq!(
            report.headline(FleetDetail::Withheld),
            "no client can take a claimable job. Which fleet gate blocks them is withheld: reading it needs read_clients or read_any_clients"
        );
    }

    /// A scan that read no body says so and asks for a re-run, rather than state
    /// a fact about jobs it never saw.
    #[test]
    fn queue_moved_headline_asks_for_a_re_run() {
        let mut report = starved_report(&[], 4);
        report.verdict = SchedulingState::QueueMoved;
        let headline = report.headline(FleetDetail::Full);
        assert!(headline.contains("left `avail/`"), "{headline}");
        assert!(headline.contains("re-run the report"), "{headline}");
        assert!(
            !headline.contains("match no working client"),
            "it must not borrow the starved wording: {headline}"
        );
    }

    /// The headline never states a frozen count in the present tense once a
    /// group matches a client now; the next `queue-maintenance` run clears it.
    #[rstest]
    // The snapshot and the fleet agree, so the present tense is correct.
    #[case::none_recovered(&[0, 0], "match no working client")]
    // Every group recovered.
    #[case::all_recovered(&[1, 2], "Every group matches a working client now")]
    // Some recovered, and the headline says which.
    #[case::partly_recovered(&[1, 0, 0], "1 of 3 group(s) match a working client now")]
    fn starved_headline_follows_the_live_fleet(
        #[case] live_matches: &[usize],
        #[case] expected: &str,
    ) {
        let report = starved_report(live_matches, 4);
        assert_eq!(
            report.headline(FleetDetail::Withheld),
            "all 4 claimable job(s) match no working client. Whether the snapshot is current, and whether any group matches a working client now, is withheld: reading it needs read_clients or read_any_clients. A queue-maintenance run clears a group that recovered"
        );
        let headline = report.headline(FleetDetail::Full);
        assert!(headline.contains(expected), "{headline}");
        // A recovered group is a past fact, so the frozen claim never reads as
        // the present one.
        if live_matches.iter().any(|live| *live > 0) {
            assert!(
                headline.contains("matched no working client when the snapshot was computed"),
                "{headline}"
            );
        }
    }

    /// The withheld tier states one fixed sentence for the whole `Starved`
    /// verdict, whatever the snapshot's age: every full-tier arm reads the fleet.
    #[rstest]
    #[case::snapshot_with_groups(false, &[0, 0])]
    #[case::snapshot_without_groups(false, &[])]
    #[case::live_scan_with_groups(true, &[0, 0])]
    #[case::live_scan_without_groups(true, &[])]
    fn withheld_starved_headline_ignores_the_snapshot(
        #[case] starved_by_live_scan: bool,
        #[case] live_matches: &[usize],
    ) {
        let mut report = starved_report(live_matches, 4);
        report.starved_by_live_scan = starved_by_live_scan;
        assert_eq!(
            report.headline(FleetDetail::Withheld),
            "all 4 claimable job(s) match no working client. Whether the snapshot is current, and whether any group matches a working client now, is withheld: reading it needs read_clients or read_any_clients. A queue-maintenance run clears a group that recovered"
        );
    }

    #[test]
    fn schedulable_withheld_headline_ignores_fleet_count() {
        let mut report = starved_report(&[], 4);
        report.verdict = SchedulingState::Schedulable;
        let expected = "a claimable job can reach a client: 4 claimable job(s). The count of unblocked clients is withheld: reading it needs read_clients or read_any_clients. The server cannot see whether a client polls";
        for unblocked in [0, 1, 99] {
            report.fleet.unblocked = unblocked;
            assert_eq!(report.headline(FleetDetail::Withheld), expected);
        }
    }

    #[test]
    fn plan_status_serializes_snake_case() -> anyhow::Result<()> {
        assert_eq!(json!(PlanStatus::PendingClients), json!("pending_clients"));
        assert_eq!(json!(PlanStatus::Creating), json!("creating"));
        assert_eq!(json!(PlanStatus::Cancelling), json!("cancelling"));
        assert_eq!(
            serde_json::from_value::<PlanStatus>(json!("complete"))?,
            PlanStatus::Complete
        );
        assert_eq!(
            serde_json::from_value::<PlanStatus>(json!("cancelling"))?,
            PlanStatus::Cancelling
        );
        Ok(())
    }

    /// Every variant's `label` matches its serde spelling, so a log line and a
    /// stored manifest name the same state identically.
    #[test]
    fn plan_status_label_matches_the_serde_spelling() -> anyhow::Result<()> {
        // Over `EnumIter` rather than a hand-listed array: a variant added
        // later is covered without anyone remembering to add it here.
        <PlanStatus as strum::IntoEnumIterator>::iter()
            .for_each(|status| assert_eq!(json!(status), json!(status.label())));
        Ok(())
    }

    /// Every outcome's `label` matches its serde name, so the SQL column and the
    /// manifest JSON name the same outcome identically. A label parses back to
    /// its outcome, as a SQL column reads it.
    #[test]
    fn settled_outcome_label_matches_the_serde_spelling() -> anyhow::Result<()> {
        <SettledOutcome as strum::IntoEnumIterator>::iter().try_for_each(|outcome| {
            assert_eq!(json!(outcome), json!(outcome.label()));
            assert_eq!(outcome.label().parse::<SettledOutcome>()?, outcome);
            anyhow::Ok(())
        })
    }

    /// Only a `failure` record fails. Any other `message_type`, or none, is a
    /// finished job.
    #[rstest]
    #[case::failure(Some("failure"), SettledOutcome::Failed)]
    #[case::success(Some("success"), SettledOutcome::Finished)]
    #[case::other(Some("other"), SettledOutcome::Finished)]
    #[case::absent(None, SettledOutcome::Finished)]
    fn settled_outcome_from_message_type(
        #[case] message_type: Option<&str>,
        #[case] expected: SettledOutcome,
    ) {
        assert_eq!(SettledOutcome::from_message_type(message_type), expected);
    }

    #[test]
    fn plan_status_is_terminal() {
        assert!(PlanStatus::Complete.is_terminal());
        assert!(PlanStatus::Cancelled.is_terminal());
        assert!(!PlanStatus::Active.is_terminal());
        assert!(!PlanStatus::Creating.is_terminal());
        assert!(!PlanStatus::PendingClients.is_terminal());
        // Teardown is still in flight, so the pass must keep visiting the plan.
        assert!(!PlanStatus::Cancelling.is_terminal());
    }

    /// `counts.cancelled` defaults to zero, so a snapshot written without the
    /// field still deserializes.
    #[test]
    fn counts_defaults_cancelled_when_absent() -> anyhow::Result<()> {
        let counts: Counts = serde_json::from_value(json!({
            "total": 3, "finished": 1, "running": 1, "available": 1, "failed": 0,
        }))?;
        assert_eq!(counts.cancelled, 0);
        Ok(())
    }

    /// A source keeps its text byte for byte, and its digest is the SHA-256 of
    /// that text. `TEST_SOURCE_SHA256` is the published digest of `"test"`.
    #[test]
    fn plan_source_digests_its_exact_text() -> anyhow::Result<()> {
        let source = PlanSource::new("test".to_string())?;
        assert_eq!(source.text(), "test");
        assert_eq!(source.sha256().as_ref(), TEST_SOURCE_SHA256);
        Ok(())
    }

    /// The `Debug` form names the digest and the length, and leaves the
    /// document out of a log line.
    #[test]
    fn plan_source_debug_omits_the_document() -> anyhow::Result<()> {
        let source = PlanSource::new("benchmarks = [\"secret-looking\"]".to_string())?;
        let rendered = format!("{source:?}");
        assert!(!rendered.contains("secret-looking"), "{rendered}");
        assert!(rendered.contains(source.sha256().as_ref()), "{rendered}");
        Ok(())
    }

    /// A minimal `creating` manifest (no snapshot, no terminal_at) round-trips,
    /// and the optional fields are omitted from the serialized form.
    #[test]
    fn manifest_minimal_roundtrip_omits_optionals() -> anyhow::Result<()> {
        let manifest = PlanManifest {
            plan_id: PlanId::from_uuid(uuid::Uuid::nil()),
            status: PlanStatus::Creating,
            created_at: Utc::now(),
            submitted_by: None,
            campaign_id: None,
            previous_plan_id: None,
            source_sha256: None,
            submitted_sha256: None,
            plan_warnings: Vec::new(),
            benchmark_types: Default::default(),
            job_ids: vec![JobId::from_uuid(uuid::Uuid::nil())],
            content_keys: BTreeMap::new(),
            rerun_of: BTreeMap::new(),
            warnings: vec![],
            expired_jobs: BTreeMap::new(),
            job_expiry: None,
            settled_jobs: Default::default(),
            progress_snapshot: None,
            terminal_at: None,
        };
        let value = serde_json::to_value(&manifest)?;
        assert!(value.get("progress_snapshot").is_none());
        assert!(value.get("terminal_at").is_none());
        assert!(value.get("job_expiry").is_none());
        assert!(value.get("note").is_none());
        assert!(value.get("expired_jobs").is_none());
        assert!(value.get("submitted_by").is_none());
        assert!(value.get("source_sha256").is_none());
        assert!(value.get("plan_warnings").is_none());
        assert!(value.get("content_keys").is_none());
        assert!(value.get("rerun_of").is_none());
        let back: PlanManifest = serde_json::from_value(value)?;
        assert_eq!(back, manifest);
        Ok(())
    }

    /// A full, terminal manifest (snapshot + terminal_at + warnings) round-trips,
    /// and `terminal_at` actually serializes when populated.
    #[test]
    fn manifest_full_roundtrip() -> anyhow::Result<()> {
        let now = Utc::now();
        let manifest = PlanManifest {
            plan_id: PlanId::from_uuid(uuid::Uuid::nil()),
            status: PlanStatus::Complete,
            created_at: now,
            submitted_by: Some(UserId::try_new(TEST_USER)?),
            campaign_id: Some(CampaignId::try_new(TEST_CAMPAIGN)?),
            previous_plan_id: Some(PlanId::try_new(TEST_PREVIOUS_PLAN)?),
            source_sha256: Some(Sha256::try_new(TEST_SOURCE_SHA256)?),
            submitted_sha256: None,
            plan_warnings: vec!["variant 0: the roster adds nobody".to_string()],
            benchmark_types: Default::default(),
            job_ids: vec![JobId::from_uuid(uuid::Uuid::nil())],
            content_keys: BTreeMap::from([(JobId::from_uuid(uuid::Uuid::nil()), "a".repeat(64))]),
            rerun_of: BTreeMap::from([(
                JobId::from_uuid(uuid::Uuid::nil()),
                JobId::from_uuid(uuid::Uuid::max()),
            )]),
            warnings: vec![Warning {
                kind: WarningKind::Fleet,
                message: "2 jobs requiring os:macos match no working client".to_string(),
                job_ids: vec![JobId::from_uuid(uuid::Uuid::nil())],
            }],
            expired_jobs: BTreeMap::from([(
                JobId::from_uuid(uuid::Uuid::nil()),
                ExpiredJob {
                    horizon: "2026-08-19T17:55:00Z".parse()?,
                    recorded_at: now,
                },
            )]),
            job_expiry: None,
            settled_jobs: Default::default(),
            progress_snapshot: Some(ProgressSnapshot {
                computed_at: now,
                counts: Counts {
                    total: 3,
                    finished: 1,
                    running: 1,
                    available: 1,
                    failed: 0,
                    cancelled: 0,
                },
                // No negatives, so the three exclusion fields must be absent
                // from the serialized group and default back on the way in.
                starved: vec![StarvedGroup {
                    clauses: StarvedClauses {
                        requires: vec!["os:macos".to_string()],
                        any_of: vec![],
                        clients: vec![],
                        exclude_clients: vec![],
                        excludes: vec![],
                        not_all_of: vec![],
                    },
                    job_ids: vec![JobId::from_uuid(uuid::Uuid::nil())],
                }],
            }),
            terminal_at: Some(now),
        };
        let value = serde_json::to_value(&manifest)?;
        assert!(value.get("terminal_at").is_some());
        // The submitter serializes as a bare string, the shape `UserId`'s
        // `into = "String"` gives it.
        assert_eq!(value["submitted_by"], json!(TEST_USER));
        // Keyed by job id, and the horizon keeps its filename spelling.
        assert_eq!(
            value["expired_jobs"][JobId::from_uuid(uuid::Uuid::nil()).as_str()]["horizon"],
            json!("20260819T175500Z")
        );
        let back: PlanManifest = serde_json::from_value(value)?;
        assert_eq!(back, manifest);
        Ok(())
    }

    /// A manifest with only the four required fields reads back with defaults,
    /// so a new optional field without a serde default fails here, not on a
    /// live manifest.
    #[test]
    fn manifest_without_optional_fields_reads_with_defaults() -> anyhow::Result<()> {
        let manifest: PlanManifest = serde_json::from_value(json!({
            "plan_id": "plan-1",
            "status": "active",
            "created_at": "2026-07-20T17:55:00Z",
            "job_ids": ["job-1"],
        }))?;
        assert!(manifest.submitted_by.is_none());
        assert!(manifest.warnings.is_empty());
        assert!(manifest.plan_warnings.is_empty());
        assert!(manifest.job_expiry.is_none());
        assert!(manifest.expired_jobs.is_empty());
        assert!(manifest.progress_snapshot.is_none());
        assert!(manifest.terminal_at.is_none());
        Ok(())
    }

    /// A submitter reads back only as a [`UserId`]; anything else, such as an
    /// older manifest's `ev1_…`, reads as absent rather than failing the object.
    #[rstest]
    #[case::a_user(TEST_USER, Some(TEST_USER))]
    #[case::a_legacy_client_id("ev1_filer", None)]
    #[case::unprefixed("operator", None)]
    #[case::blank("", None)]
    fn manifest_submitted_by_reads_only_a_user_id(
        #[case] submitted_by: &str,
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let manifest: PlanManifest = serde_json::from_value(json!({
            "plan_id": "plan-1",
            "status": "active",
            "created_at": "2026-07-20T17:55:00Z",
            "job_ids": ["job-1"],
            "submitted_by": submitted_by,
        }))?;

        assert_eq!(
            manifest.submitted_by,
            expected.map(UserId::try_new).transpose()?,
            "{submitted_by:?}"
        );
        Ok(())
    }

    /// Leniency covers the submitter's *value*, not the field's shape: a
    /// `submitted_by` that is not a string fails the manifest. Nothing writes
    /// one, so such a value means the object is damaged rather than dated.
    #[test]
    fn manifest_submitted_by_must_still_be_a_string() {
        let parsed = serde_json::from_value::<PlanManifest>(json!({
            "plan_id": "plan-1",
            "status": "active",
            "created_at": "2026-07-20T17:55:00Z",
            "job_ids": ["job-1"],
            "submitted_by": 42,
        }));
        assert!(parsed.is_err());
    }

    /// A submitter the read drops does not come back: `put_plan` writes the
    /// whole object, so the next rewrite stores no `submitted_by` key.
    #[test]
    fn a_dropped_submitter_does_not_survive_a_rewrite() -> anyhow::Result<()> {
        let manifest: PlanManifest = serde_json::from_value(json!({
            "plan_id": "plan-1",
            "status": "active",
            "created_at": "2026-07-20T17:55:00Z",
            "job_ids": ["job-1"],
            "submitted_by": "ev1_filer",
        }))?;

        let rewritten = serde_json::to_value(&manifest)?;
        assert!(rewritten.get("submitted_by").is_none());
        Ok(())
    }

    /// An unknown field is tolerated (schema is open, not closed).
    #[test]
    fn manifest_tolerates_unknown_fields() -> anyhow::Result<()> {
        let value = json!({
            "plan_id": "plan-1",
            "status": "creating",
            "created_at": "2026-07-20T17:55:00Z",
            "job_ids": ["job-1"],
            "some_future_field": 42,
        });
        let manifest: PlanManifest = serde_json::from_value(value)?;
        assert_eq!(manifest.status, PlanStatus::Creating);
        assert!(manifest.warnings.is_empty());
        Ok(())
    }
    // ── PlanLeases ──────────────────────────────────────────────────────────

    /// A lease on `job` held by `client`, its deadline `minutes` from `now`. A
    /// negative offset seeds the strand that a stalled `queue-maintenance`
    /// leaves.
    fn lease(client: &str, job: &str, minutes: i64, now: DateTime<Utc>) -> anyhow::Result<Lease> {
        Ok(Lease {
            job_id: JobId::new_unchecked(job),
            expires_at: ExpiresAt::Never,
            client_id: ClientId::try_new(client)?,
            lease_expiry: now + chrono::Duration::minutes(minutes),
        })
    }

    /// The holder and job of each row, in order — what the renderer prints.
    fn rows(held: &[Lease]) -> Vec<(String, String)> {
        held.iter()
            .map(|lease| (lease.client_id.to_string(), lease.job_id.to_string()))
            .collect()
    }

    /// An expected row list in the shape that [`rows`] returns.
    fn expected(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(client, job)| (client.to_string(), job.to_string()))
            .collect()
    }

    /// `expect_retained` is the deadline a one-row collapse keeps, in minutes
    /// from `now`; `None` skips the check.
    #[rstest]
    // A deadline ahead of `now` is work in flight.
    #[case::live(&[("ev1_a", "job-1", 30)], &[("ev1_a", "job-1")], &[], 0, None)]
    // Past its deadline the holder stopped heartbeating, so the job is stranded.
    #[case::lapsed(&[("ev1_a", "job-1", -30)], &[], &[("ev1_a", "job-1")], 0, None)]
    #[case::one_of_each(
        &[("ev1_a", "job-1", 30), ("ev1_b", "job-2", -30)],
        &[("ev1_a", "job-1")],
        &[("ev1_b", "job-2")],
        0,
        None
    )]
    // A renewal renames the key, so one listing can return both. The job is
    // held, not stranded, and it appears once.
    #[case::renewal_collapses(
        &[("ev1_a", "job-1", -30), ("ev1_a", "job-1", 30)],
        &[("ev1_a", "job-1")],
        &[],
        0,
        None
    )]
    // The same pair in the other listing order. `list_leased` fixes no order,
    // so the collapse must not depend on one.
    #[case::renewal_collapses_either_order(
        &[("ev1_a", "job-1", 30), ("ev1_a", "job-1", -30)],
        &[("ev1_a", "job-1")],
        &[],
        0,
        None
    )]
    // Two clients can hold two of the plan's jobs, and one client can hold
    // several. Rows sort by client id, then job id.
    #[case::sorted_by_holder(
        &[("ev1_c", "job-3", 30), ("ev1_a", "job-2", 30), ("ev1_a", "job-1", 30)],
        &[("ev1_a", "job-1"), ("ev1_a", "job-2"), ("ev1_c", "job-3")],
        &[],
        0,
        None
    )]
    #[case::nothing_held(&[], &[], &[], 0, None)]
    // A deadline exactly at `now` is past: the split keeps a lease live only
    // while its deadline is ahead. `JobCensus::lapsed_leases` reads these same
    // lists, so the two agree at the boundary.
    #[case::due_now(&[("ev1_a", "job-1", 0)], &[], &[("ev1_a", "job-1")], 0, None)]
    // The collapse keeps the renewal, not the key that it replaced, so the row
    // reports the deadline that the holder currently heartbeats against.
    #[case::renewal_keeps_the_later_deadline(
        &[("ev1_a", "job-1", 5), ("ev1_a", "job-1", 45)],
        &[("ev1_a", "job-1")],
        &[],
        0,
        Some(45)
    )]
    // A key that names no job joins no list. The count carries it instead, so
    // the report can say what the lists do not cover.
    #[case::unparseable_keys_are_not_rows(&[], &[], &[], 2, None)]
    fn plan_leases_collapses_splits_and_sorts(
        #[case] held: &[(&str, &str, i64)],
        #[case] expect_live: &[(&str, &str)],
        #[case] expect_lapsed: &[(&str, &str)],
        #[case] unparseable: usize,
        #[case] expect_retained: Option<i64>,
    ) -> anyhow::Result<()> {
        let now: DateTime<Utc> = "2026-09-04T12:00:00Z".parse()?;
        let leases = PlanLeases::new(
            held.iter()
                .map(|(client, job, minutes)| lease(client, job, *minutes, now))
                .collect::<anyhow::Result<Vec<_>>>()?,
            unparseable,
            now,
        );
        assert_eq!(rows(&leases.live), expected(expect_live), "live rows");
        assert_eq!(rows(&leases.lapsed), expected(expect_lapsed), "lapsed rows");
        assert_eq!(
            leases.is_empty(),
            expect_live.is_empty() && expect_lapsed.is_empty(),
            "`is_empty` agrees with the two lists"
        );
        assert_eq!(
            leases.unparseable_keys, unparseable,
            "the count reaches the report"
        );
        assert_eq!(
            leases.read_at, now,
            "the report holds the instant it split on"
        );
        if let Some(minutes) = expect_retained {
            let held: Vec<&Lease> = leases.live.iter().chain(&leases.lapsed).collect();
            assert_eq!(
                held.len(),
                1,
                "`expect_retained` describes a case that collapses to one row"
            );
            assert_eq!(
                held[0].lease_expiry,
                now + chrono::Duration::minutes(minutes),
                "the collapse keeps the latest deadline"
            );
        }
        Ok(())
    }

    #[rstest]
    #[case::withheld(FleetDetail::Withheld)]
    #[case::full(FleetDetail::Full)]
    fn status_projects_frozen_warnings_and_all_clauses(
        #[case] detail: FleetDetail,
    ) -> anyhow::Result<()> {
        let manifest = sample_manifest(
            PlanId::from_uuid(uuid::Uuid::nil()),
            PlanStatus::Active,
            true,
        )?;
        let stored = serde_json::to_value(&manifest)?;
        let view = serde_json::to_value(manifest.view(PlanNotes::default(), false, detail))?;
        assert_eq!(view["fleet_detail"], detail.label());
        // The stored record keeps `starved`; only the body says `starved_groups`.
        let snapshot = &view["progress_snapshot"];
        assert!(stored["progress_snapshot"].get("starved_groups").is_none());
        assert!(snapshot.get("starved").is_none());
        assert_eq!(snapshot["counts"], stored["progress_snapshot"]["counts"]);
        if detail == FleetDetail::Full {
            assert_eq!(view["warnings"], stored["warnings"]);
            assert_eq!(view["plan_warnings"], stored["plan_warnings"]);
            assert_eq!(
                snapshot["starved_groups"],
                stored["progress_snapshot"]["starved"]
            );
        } else {
            assert_eq!(view["plan_warnings"], json!([]));
            assert_eq!(
                view["warnings"][0]["message"],
                "1 job matches no working client"
            );
            assert_eq!(snapshot["starved_groups"], json!([{"job_ids": ["job-2"]}]));
        }
        Ok(())
    }

    /// A warning that describes the caller's own jobs keeps its message for a
    /// caller that may not read the fleet. Only a fleet warning names clauses,
    /// so only a fleet warning is rewritten.
    #[test]
    fn status_keeps_a_non_fleet_warning_under_withheld() -> anyhow::Result<()> {
        let mut manifest = sample_manifest(
            PlanId::from_uuid(uuid::Uuid::nil()),
            PlanStatus::Active,
            false,
        )?;
        let message = "1 job names a model with no parameter-count entry".to_string();
        manifest.warnings = vec![Warning {
            kind: WarningKind::UncataloguedModel,
            message: message.clone(),
            job_ids: vec![JobId::new_unchecked("job-2")],
        }];
        let view = manifest.view(PlanNotes::default(), false, FleetDetail::Withheld);
        assert_eq!(
            view.warnings
                .iter()
                .map(|w| (w.kind.clone(), w.message.as_str()))
                .collect::<Vec<_>>(),
            vec![(WarningKind::UncataloguedModel, message.as_str())]
        );
        Ok(())
    }

    /// The view drops `job_ids`, keeps the ingestion-time warnings, and states
    /// each uncomputed or absent field as `null` rather than omitting it, so a
    /// caller can tell "not computed yet" from "computed, and empty".
    #[test]
    fn a_plan_view_says_what_is_absent() -> anyhow::Result<()> {
        let mut manifest = sample_manifest(
            PlanId::from_uuid(uuid::Uuid::from_u128(1)),
            PlanStatus::Active,
            false,
        )?;
        manifest.progress_snapshot = None;
        manifest.terminal_at = None;
        manifest.job_expiry = None;
        manifest.submitted_by = None;
        let value =
            serde_json::to_value(manifest.view(PlanNotes::default(), false, FleetDetail::Full))?;
        assert!(value.get("job_ids").is_none(), "job_ids stays internal");
        for field in ["job_expiry", "progress_snapshot", "terminal_at"] {
            assert_eq!(
                value.get(field),
                Some(&Value::Null),
                "{field} is an explicit null"
            );
        }
        // Absence is a final answer for the submitter, so the key goes away.
        assert!(value.get("submitted_by").is_none());
        Ok(())
    }

    /// A manifest's summary takes its note and change from the notes and its
    /// counts from the snapshot, as a listing row selects them.
    #[test]
    fn a_manifest_summary_carries_its_notes_and_counts() -> anyhow::Result<()> {
        let manifest = sample_manifest(
            PlanId::from_uuid(uuid::Uuid::from_u128(1)),
            PlanStatus::Active,
            true,
        )?;
        let counts = manifest
            .progress_snapshot
            .as_ref()
            .map(|snapshot| snapshot.counts);
        let change = pipette_plan_types::RevisionChange::Rerun {
            answered: None,
            job_count: 1,
        };
        let notes = PlanNotes {
            note: Some(" nightly ".to_owned()),
            change: Some(change.clone()),
        };
        let summary = manifest.clone().summary(notes, true);
        assert_eq!(
            summary.note.as_ref().map(NonEmptyTrimmedString::as_str),
            Some("nightly")
        );
        assert_eq!(summary.change, Some(change));
        assert_eq!(summary.counts, counts);
        assert!(summary.cancel_requested);
        assert_eq!(summary.submitted_by, manifest.submitted_by);
        assert_eq!(summary.campaign_id, manifest.campaign_id);
        Ok(())
    }

    /// A manifest keeps the expiry range and a lapse under their stored names
    /// and compact form; the plan and jobs reads rename them and write RFC 3339,
    /// `null` for never.
    #[test]
    fn the_expiry_names_are_stored_and_renamed_on_the_read() -> anyhow::Result<()> {
        let manifest = sample_manifest(
            PlanId::from_uuid(uuid::Uuid::from_u128(1)),
            PlanStatus::Active,
            false,
        )?;
        let stored = serde_json::to_value(&manifest)?;
        assert_eq!(
            stored["job_expiry"],
            json!({"earliest": "20260819T175500Z", "latest": "20260918T175500Z"})
        );
        let lapse = json!({"horizon": "20260819T175500Z", "recorded_at": "2026-07-20T17:55:00Z"});
        assert_eq!(stored["expired_jobs"]["job-2"], lapse);

        let expired: ExpiredJob = serde_json::from_value(lapse)?;
        assert_eq!(
            serde_json::to_value(pipette_plan_types::ExpiredJob::from(expired))?,
            json!({"expires_at": "2026-08-19T17:55:00Z", "recorded_at": "2026-07-20T17:55:00Z"})
        );
        let view =
            serde_json::to_value(manifest.view(PlanNotes::default(), false, FleetDetail::Full))?;
        assert_eq!(
            view["job_expiry"],
            json!({
                "earliest_expires_at": "2026-08-19T17:55:00Z",
                "latest_expires_at": "2026-09-18T17:55:00Z",
            })
        );
        let never = json!({"horizon": "never", "recorded_at": "2026-07-20T17:55:00Z"});
        assert!(serde_json::from_value::<ExpiredJob>(never).is_err());
        let unbounded: JobExpiry =
            serde_json::from_value(json!({"earliest": "20260819T175500Z", "latest": "never"}))?;
        assert_eq!(
            serde_json::to_value(pipette_plan_types::JobExpiry::from(unbounded))?,
            json!({"earliest_expires_at": "2026-08-19T17:55:00Z", "latest_expires_at": null})
        );
        Ok(())
    }

    /// The view withholds exactly the manifest-only fields, pinned against a
    /// manifest with every field populated, so a field added to `PlanManifest`
    /// and forgotten here fails rather than silently disappearing.
    #[test]
    fn a_plan_view_withholds_only_the_manifest_fields() -> anyhow::Result<()> {
        let manifest = sample_manifest(
            PlanId::from_uuid(uuid::Uuid::from_u128(1)),
            PlanStatus::Complete,
            true,
        )?;
        let keys = |value: Value| -> BTreeSet<String> {
            value
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default()
        };
        let manifest_keys = keys(serde_json::to_value(&manifest)?);
        let view_keys = keys(serde_json::to_value(manifest.view(
            PlanNotes::default(),
            false,
            FleetDetail::Full,
        ))?);
        assert_eq!(
            manifest_keys.difference(&view_keys).collect::<Vec<_>>(),
            vec![
                "content_keys",
                "expired_jobs",
                "job_ids",
                "rerun_of",
                "settled_jobs",
                "source_sha256",
                "submitted_sha256"
            ]
        );
        assert_eq!(
            view_keys.difference(&manifest_keys).collect::<Vec<_>>(),
            vec!["cancel_requested", "fleet_detail"]
        );
        Ok(())
    }

    /// The withheld projection drops every clause-bearing diagnostic, so a
    /// `never_matches` string cannot reach a client through it.
    #[test]
    fn withheld_explanation_omits_clause_bearing_diagnostics() -> anyhow::Result<()> {
        let mut report = starved_report(&[0], 1);
        report.groups[0].never_matches = Some("requires secret:clause can never match".into());
        let json = serde_json::to_value(report.view(FleetDetail::Withheld))?;
        assert!(json["groups"][0].get("never_matches").is_none());
        assert!(json["groups"][0].get("description").is_none());
        assert!(!json.to_string().contains("secret:clause"));
        Ok(())
    }
}
