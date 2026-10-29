//! [`AuditStore`] on SQL: an event row, a row for each organization whose log
//! lists it and one for each record it names, written in one transaction. A
//! listing drives from the narrowest of those tables its filter allows.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::AssertSqlSafe;

use pipette_plan_types::{
    AuditAction, AuditActor, AuditEvent, AuditEventId, AuditOutcome, AuditTarget, CampaignId,
    ClientId, JobId, OrgId, PlanId, UserId, VaultResourceId,
};
use pipette_store::{AuditFilter, AuditPage, AuditStore, UnreadableObjectError};

use crate::db::SqlDatabase;
use crate::listing::Keyspace;
use crate::retry::retry;

/// The event rows; an unreadable one is reported under `audit/`. Only the
/// report key is read: the listing writes its own SQL.
const EVENTS: Keyspace = Keyspace {
    table: "audit_events",
    id_column: "event_id",
    report_prefix: "audit/",
};

/// The rows that one prune statement deletes, so no transaction holds every
/// expired row.
const PRUNE_BATCH: i64 = 10_000;

#[derive(Clone)]
pub(crate) struct SqlAuditStore {
    db: SqlDatabase,
}

impl SqlAuditStore {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }
}

/// An event in the shape earlier builds wrote to `audit_events.body`, kept
/// whatever the wire calls its fields so a rollback reads what this build writes.
#[derive(Serialize, Deserialize)]
struct StoredEvent {
    event_id: AuditEventId,
    at: DateTime<Utc>,
    actor: AuditActor,
    action: AuditAction,
    outcome: AuditOutcome,
    targets: Vec<StoredTarget>,
    org_ids: BTreeSet<OrgId>,
}

/// A target as `audit_events.body` keeps it.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
enum StoredTarget {
    Org(OrgId),
    User(UserId),
    Client(ClientId),
    Campaign(CampaignId),
    Plan(PlanId),
    Job(JobId),
    VaultResource(VaultResourceId),
}

impl From<AuditTarget> for StoredTarget {
    fn from(target: AuditTarget) -> Self {
        match target {
            AuditTarget::Org { org_id } => Self::Org(org_id),
            AuditTarget::User { user_id } => Self::User(user_id),
            AuditTarget::Client { client_id } => Self::Client(client_id),
            AuditTarget::Campaign { campaign_id } => Self::Campaign(campaign_id),
            AuditTarget::Plan { plan_id } => Self::Plan(plan_id),
            AuditTarget::Job { job_id } => Self::Job(job_id),
            AuditTarget::VaultResource { resource_id } => Self::VaultResource(resource_id),
        }
    }
}

impl From<StoredTarget> for AuditTarget {
    fn from(target: StoredTarget) -> Self {
        match target {
            StoredTarget::Org(org_id) => Self::Org { org_id },
            StoredTarget::User(user_id) => Self::User { user_id },
            StoredTarget::Client(client_id) => Self::Client { client_id },
            StoredTarget::Campaign(campaign_id) => Self::Campaign { campaign_id },
            StoredTarget::Plan(plan_id) => Self::Plan { plan_id },
            StoredTarget::Job(job_id) => Self::Job { job_id },
            StoredTarget::VaultResource(resource_id) => Self::VaultResource { resource_id },
        }
    }
}

/// The summary is the route table's, never stored.
impl From<AuditEvent> for StoredEvent {
    fn from(event: AuditEvent) -> Self {
        Self {
            event_id: event.event_id,
            at: event.created_at,
            actor: event.actor,
            action: event.action,
            outcome: event.outcome,
            targets: event.targets.into_iter().map(StoredTarget::from).collect(),
            org_ids: event.org_ids,
        }
    }
}

impl From<StoredEvent> for AuditEvent {
    fn from(event: StoredEvent) -> Self {
        Self {
            event_id: event.event_id,
            created_at: event.at,
            actor: event.actor,
            action: event.action,
            outcome: event.outcome,
            targets: event.targets.into_iter().map(AuditTarget::from).collect(),
            org_ids: event.org_ids,
            summary: None,
        }
    }
}

/// A bound value of a listing statement, in placeholder order.
enum Param {
    Text(String),
    Texts(Vec<String>),
    Int(i64),
}

/// The statement for one round of a listing and its parameters. It walks the
/// narrowest table that the filter allows, newest first. On the org and event
/// rows, each requested outcome is its own branch; with no outcome, each
/// requested actor kind is its own branch. A branch is an equality that an
/// index walks in `event_id` order, and one limit merges the branches. `= ANY`
/// would read every match. The record rows carry no filter columns, so a
/// record's history is one walk that checks each row as it reads it.
fn listing(filter: &AuditFilter, before: Option<&str>, limit: i64) -> (String, Vec<Param>) {
    let mut params = Vec::new();
    let mut bind = |param: Param| {
        params.push(param);
        format!("${}", params.len())
    };
    let mut joins = String::new();
    let mut conditions = Vec::new();
    // The org and event rows carry the filter columns; the record rows do not.
    let (driver, columns) = match (&filter.record, &filter.org_id) {
        (Some(record), org_id) => {
            let kind = bind(Param::Text(record.kind().into()));
            let id = bind(Param::Text(record.id().into()));
            conditions.push(format!("d.record_type = {kind} AND d.record_id = {id}"));
            if let Some(org_id) = org_id {
                let org = bind(Param::Text(org_id.as_str().into()));
                joins.push_str(&format!(
                    " JOIN audit_event_orgs o ON o.event_id = d.event_id AND o.org_id = {org}"
                ));
            }
            ("audit_event_records", "e")
        }
        (None, Some(org_id)) => {
            let org = bind(Param::Text(org_id.as_str().into()));
            conditions.push(format!("d.org_id = {org}"));
            ("audit_event_orgs", "d")
        }
        (None, None) => ("audit_events", "d"),
    };
    if let Some(before) = before {
        conditions.push(format!("d.event_id < {}", bind(Param::Text(before.into()))));
    }
    let outcomes: Vec<String> = filter
        .outcomes
        .iter()
        .map(|o| o.as_ref().to_owned())
        .collect();
    let kinds: Vec<String> = filter
        .actor_kinds
        .iter()
        .map(|k| k.as_ref().to_owned())
        .collect();
    let (split_column, split_values) = if driver == "audit_event_records" {
        conditions.extend(
            [("outcome", outcomes), ("actor_type", kinds)]
                .into_iter()
                .filter(|(_, values)| !values.is_empty())
                .map(|(column, values)| {
                    format!("{columns}.{column} = ANY({})", bind(Param::Texts(values)))
                }),
        );
        ("outcome", Vec::new())
    } else if outcomes.is_empty() {
        ("actor_type", kinds)
    } else {
        if !kinds.is_empty() {
            let kinds = bind(Param::Texts(kinds));
            conditions.push(format!("{columns}.actor_type = ANY({kinds})"));
        }
        ("outcome", outcomes)
    };
    let branches: Vec<Option<String>> = if split_values.is_empty() {
        vec![None]
    } else {
        split_values
            .into_iter()
            .map(|value| {
                Some(format!(
                    "{columns}.{split_column} = {}",
                    bind(Param::Text(value))
                ))
            })
            .collect()
    };
    let limit = bind(Param::Int(limit));
    let (from, body) = if driver == "audit_events" {
        ("audit_events d".to_owned(), "d.body")
    } else {
        (
            format!("{driver} d JOIN audit_events e ON e.event_id = d.event_id"),
            "e.body",
        )
    };
    let branch = |split: Option<String>| {
        let mut all = conditions.clone();
        all.extend(split);
        let filter = if all.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", all.join(" AND "))
        };
        format!(
            "SELECT d.event_id AS event_id, {body} AS body FROM {from}{joins}{filter}
             ORDER BY d.event_id DESC LIMIT {limit}"
        )
    };
    let sql = match branches.as_slice() {
        [single] => branch(single.clone()),
        many => format!(
            "SELECT event_id, body FROM ({}) AS branch ORDER BY event_id DESC LIMIT {limit}",
            many.iter()
                .map(|split| format!("({})", branch(split.clone())))
                .collect::<Vec<_>>()
                .join(" UNION ALL ")
        ),
    };
    (sql, params)
}

/// The records an event names, as `(record_type, record_id)`: its targets and
/// the account or device that acted.
fn named_records(event: &AuditEvent) -> (Vec<String>, Vec<String>) {
    let actor = event.actor.record();
    let mut records: Vec<&AuditTarget> = event.targets.iter().collect();
    if let Some(actor) = actor
        .as_ref()
        .filter(|actor| !event.targets.contains(actor))
    {
        records.push(actor);
    }
    records
        .into_iter()
        .map(|record| (record.kind().to_owned(), record.id().to_owned()))
        .unzip()
}

#[async_trait]
impl AuditStore for SqlAuditStore {
    async fn append(&self, event: &AuditEvent) -> anyhow::Result<()> {
        let body = serde_json::to_string(&StoredEvent::from(event.clone()))?;
        let (body, event_id) = (body.as_str(), event.event_id.as_str());
        let (outcome, actor_type) = (event.outcome.as_ref(), event.actor.kind());
        let actor_type = actor_type.as_ref();
        let orgs: Vec<&str> = event.org_ids.iter().map(OrgId::as_str).collect();
        let orgs = orgs.as_slice();
        let (record_types, record_ids) = named_records(event);
        let (record_types, record_ids) = (record_types.as_slice(), record_ids.as_slice());
        let db = &self.db;
        retry("append_audit_event", || {
            Box::pin(async move {
                let mut tx = db.begin().await?;
                sqlx::query(
                    "INSERT INTO audit_events (event_id, at, outcome, actor_type, body)
                     VALUES ($1, $2, $3, $4, $5)",
                )
                .bind(event_id)
                .bind(event.created_at)
                .bind(outcome)
                .bind(actor_type)
                .bind(body)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "INSERT INTO audit_event_orgs (org_id, event_id, outcome, actor_type)
                     SELECT org_id, $2, $3, $4 FROM unnest($1::text[]) AS org (org_id)",
                )
                .bind(orgs)
                .bind(event_id)
                .bind(outcome)
                .bind(actor_type)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "INSERT INTO audit_event_records (record_type, record_id, event_id)
                     SELECT record_type, record_id, $3
                     FROM unnest($1::text[], $2::text[]) AS record (record_type, record_id)",
                )
                .bind(record_types)
                .bind(record_ids)
                .bind(event_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await
            })
        })
        .await?;
        Ok(())
    }

    async fn list_events(
        &self,
        filter: &AuditFilter,
        before: Option<&AuditEventId>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<AuditPage> {
        let mut page = AuditPage::default();
        let mut cursor = before.map(|id| id.as_str().to_string());
        loop {
            let remaining = limit.get().saturating_sub(page.items.len());
            // One row past the page says whether another page follows.
            let (sql, params) = listing(
                filter,
                cursor.as_deref(),
                i64::try_from(remaining.saturating_add(1))?,
            );
            let rows: Vec<(String, String)> = self
                .db
                .fetch_all("list_audit_events", || {
                    params.iter().fold(
                        sqlx::query_as(AssertSqlSafe(sql.clone())),
                        |query, param| match param {
                            Param::Text(value) => query.bind(value.as_str()),
                            Param::Texts(values) => query.bind(values.as_slice()),
                            Param::Int(value) => query.bind(*value),
                        },
                    )
                })
                .await?;
            let mut rows = rows;
            let more = rows.len() > remaining;
            rows.truncate(remaining);
            for (event_id, body) in rows {
                match serde_json::from_str::<StoredEvent>(&body).map(AuditEvent::from) {
                    Ok(event) if event.event_id.as_str() == event_id => page.items.push(event),
                    Ok(_) => page.unreadable.push(UnreadableObjectError {
                        key: EVENTS.report_key(&event_id),
                        reason: "the body names another event".into(),
                    }),
                    Err(e) => page.unreadable.push(UnreadableObjectError {
                        key: EVENTS.report_key(&event_id),
                        reason: e.to_string(),
                    }),
                }
                cursor = Some(event_id);
            }
            if !more {
                return Ok(page);
            }
            if page.items.len() == limit.get() {
                page.resume_after = cursor.map(AuditEventId::try_new).transpose()?;
                return Ok(page);
            }
        }
    }

    async fn prune_before(&self, cutoff: DateTime<Utc>) -> anyhow::Result<u64> {
        let mut pruned = 0u64;
        loop {
            let deleted = self
                .db
                .execute("prune_audit_events", || {
                    sqlx::query(
                        "DELETE FROM audit_events WHERE event_id IN (
                             SELECT event_id FROM audit_events WHERE at < $1
                             ORDER BY at LIMIT $2)",
                    )
                    .bind(cutoff)
                    .bind(PRUNE_BATCH)
                })
                .await?;
            pruned = pruned.saturating_add(deleted);
            if deleted == 0 {
                return Ok(pruned);
            }
        }
    }

    async fn count_before(&self, cutoff: DateTime<Utc>) -> anyhow::Result<u64> {
        let (count,): (i64,) = self
            .db
            .fetch_one("count_audit_events", || {
                sqlx::query_as("SELECT count(*) FROM audit_events WHERE at < $1").bind(cutoff)
            })
            .await?;
        Ok(u64::try_from(count)?)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;
    use serde_json::{Value, json};

    use pipette_plan_types::{
        AuditActorKind, NonEmptyTrimmedString, PublicKeyHex, UserKey, UserKeyId,
    };
    use pipette_store_testkit::TEST_LIST_LIMIT;

    use super::*;

    const USER: &str = "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";

    /// A body as every build writes it: what a rollback reads.
    const STORED_BODY: &str = concat!(
        r#"{"event_id":"audit_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e","at":"2026-10-17T12:00:00Z","#,
        r#""actor":{"type":"user","user_id":"user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e","#,
        r#""key_id":"userkey_0195c3e2a4f37b8c"},"action":{"type":"http","method":"PATCH","#,
        r#""route":"/api/v1/campaigns/{campaign_id}","#,
        r#""path":"/api/v1/campaigns/campaign_0195c3e2","#,
        r#""status":200},"outcome":"succeeded","targets":[{"type":"org","id":"org_a"},"#,
        r#"{"type":"campaign","id":"campaign_0195c3e2"},"#,
        r#"{"type":"vault_resource","id":"vault_0195c3e2"}],"org_ids":["org_a"]}"#,
    );

    /// The event [`STORED_BODY`] keeps.
    fn stored_event() -> anyhow::Result<AuditEvent> {
        Ok(AuditEvent {
            event_id: AuditEventId::try_new("audit_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?,
            created_at: "2026-10-17T12:00:00Z".parse()?,
            actor: AuditActor::User {
                user_id: UserId::try_new(USER)?,
                key_id: UserKeyId::try_new("userkey_0195c3e2a4f37b8c")?,
            },
            action: AuditAction::Http {
                method: "PATCH".into(),
                route: "/api/v1/campaigns/{campaign_id}".into(),
                path: "/api/v1/campaigns/campaign_0195c3e2".into(),
                status: 200,
            },
            outcome: AuditOutcome::Succeeded,
            targets: [
                AuditTarget::VaultResource {
                    resource_id: VaultResourceId::try_new("vault_0195c3e2")?,
                },
                AuditTarget::Campaign {
                    campaign_id: CampaignId::try_new("campaign_0195c3e2")?,
                },
                AuditTarget::Org {
                    org_id: OrgId::try_new("org_a")?,
                },
            ]
            .into(),
            org_ids: [OrgId::try_new("org_a")?].into(),
            summary: None,
        })
    }

    /// The part of [`stored_event`] a case of
    /// `every_part_is_stored_in_its_shape` replaces.
    enum Part {
        Actor(AuditActor),
        Action(AuditAction),
        Outcome(AuditOutcome),
        Target(AuditTarget),
    }

    /// Each actor, action, outcome and target as `audit_events.body` keeps it
    /// and the index columns name it, read back to its event. The body embeds
    /// the wire actor, action and outcome enums, so renaming one would change it.
    #[rstest]
    #[case::user(
        Part::Actor(AuditActor::User {
            user_id: UserId::try_new(USER)?,
            key_id: UserKeyId::try_new("userkey_0195c3e2a4f37b8c")?,
        }),
        json!({"type": "user", "user_id": USER, "key_id": "userkey_0195c3e2a4f37b8c"}),
        "user",
        &[("user", USER)]
    )]
    #[case::client(
        Part::Actor(AuditActor::Client { client_id: ClientId::try_new("ev1_abc")? }),
        json!({"type": "client", "client_id": "ev1_abc"}),
        "client",
        &[("client", "ev1_abc")]
    )]
    #[case::unverified_without_a_claim(
        Part::Actor(AuditActor::Unverified { claimed_id: None }),
        json!({"type": "unverified", "claimed_id": null}),
        "unverified",
        &[]
    )]
    #[case::unverified_with_a_claim(
        Part::Actor(AuditActor::Unverified { claimed_id: Some(USER.into()) }),
        json!({"type": "unverified", "claimed_id": USER}),
        "unverified",
        &[]
    )]
    #[case::host(
        Part::Actor(AuditActor::Host { os_user: "ops".into(), hostname: "mgmt-1".into() }),
        json!({"type": "host", "os_user": "ops", "hostname": "mgmt-1"}),
        "host",
        &[]
    )]
    #[case::http(
        Part::Action(AuditAction::Http {
            method: "PATCH".into(),
            route: "/api/v1/campaigns/{campaign_id}".into(),
            path: "/api/v1/campaigns/campaign_0195c3e2".into(),
            status: 200,
        }),
        json!({
            "type": "http",
            "method": "PATCH",
            "route": "/api/v1/campaigns/{campaign_id}",
            "path": "/api/v1/campaigns/campaign_0195c3e2",
            "status": 200,
        }),
        "user",
        &[("user", USER)]
    )]
    #[case::cli(
        Part::Action(AuditAction::Cli {
            command: "users create".into(),
            args: vec!["ops@example.test".into()],
        }),
        json!({"type": "cli", "command": "users create", "args": ["ops@example.test"]}),
        "user",
        &[("user", USER)]
    )]
    #[case::cli_without_args(
        Part::Action(AuditAction::Cli { command: "orgs list".into(), args: vec![] }),
        json!({"type": "cli", "command": "orgs list", "args": []}),
        "user",
        &[("user", USER)]
    )]
    #[case::first_clerk_sign_in(
        Part::Action(AuditAction::FirstClerkSignIn {
            prior_keys: vec![UserKey {
                key_id: UserKeyId::try_new("userkey_0195c3e2a4f37b8c")?,
                public_key: PublicKeyHex::try_new("ab".repeat(PublicKeyHex::BYTE_LEN))?,
                key_details: NonEmptyTrimmedString::try_new("build box")?,
                added_at: "2026-10-16T12:00:00Z".parse()?,
                expires_at: Some("2026-10-23T12:00:00Z".parse()?),
            }],
        }),
        json!({
            "type": "first_clerk_sign_in",
            "prior_keys": [{
                "key_id": "userkey_0195c3e2a4f37b8c",
                "public_key": "ab".repeat(32),
                "key_details": "build box",
                "added_at": "2026-10-16T12:00:00Z",
                "expires_at": "2026-10-23T12:00:00Z",
            }],
        }),
        "user",
        &[("user", USER)]
    )]
    #[case::succeeded(
        Part::Outcome(AuditOutcome::Succeeded),
        json!("succeeded"),
        "user",
        &[("user", USER)]
    )]
    #[case::refused(
        Part::Outcome(AuditOutcome::Refused),
        json!("refused"),
        "user",
        &[("user", USER)]
    )]
    #[case::failed(
        Part::Outcome(AuditOutcome::Failed),
        json!("failed"),
        "user",
        &[("user", USER)]
    )]
    #[case::org_target(
        Part::Target(AuditTarget::Org { org_id: OrgId::try_new("org_a")? }),
        json!({"type": "org", "id": "org_a"}),
        "user",
        &[("org", "org_a"), ("user", USER)]
    )]
    #[case::acting_user_target(
        Part::Target(AuditTarget::User { user_id: UserId::try_new(USER)? }),
        json!({"type": "user", "id": USER}),
        "user",
        &[("user", USER)]
    )]
    #[case::client_target(
        Part::Target(AuditTarget::Client { client_id: ClientId::try_new("ev1_abc")? }),
        json!({"type": "client", "id": "ev1_abc"}),
        "user",
        &[("client", "ev1_abc"), ("user", USER)]
    )]
    #[case::campaign_target(
        Part::Target(AuditTarget::Campaign {
            campaign_id: CampaignId::try_new("campaign_0195c3e2")?,
        }),
        json!({"type": "campaign", "id": "campaign_0195c3e2"}),
        "user",
        &[("campaign", "campaign_0195c3e2"), ("user", USER)]
    )]
    #[case::plan_target(
        Part::Target(AuditTarget::Plan { plan_id: PlanId::try_new("plan-a")? }),
        json!({"type": "plan", "id": "plan-a"}),
        "user",
        &[("plan", "plan-a"), ("user", USER)]
    )]
    #[case::job_target(
        Part::Target(AuditTarget::Job { job_id: JobId::try_new("job-1")? }),
        json!({"type": "job", "id": "job-1"}),
        "user",
        &[("job", "job-1"), ("user", USER)]
    )]
    #[case::vault_resource_target(
        Part::Target(AuditTarget::VaultResource {
            resource_id: VaultResourceId::try_new("vault_0195c3e2")?,
        }),
        json!({"type": "vault_resource", "id": "vault_0195c3e2"}),
        "user",
        &[("vault_resource", "vault_0195c3e2"), ("user", USER)]
    )]
    fn every_part_is_stored_in_its_shape(
        #[case] part: Part,
        #[case] stored: Value,
        #[case] actor_type: &str,
        #[case] records: &[(&str, &str)],
    ) -> anyhow::Result<()> {
        let mut event = AuditEvent {
            targets: BTreeSet::new(),
            ..stored_event()?
        };
        let (key, stored) = match part {
            Part::Actor(actor) => {
                event.actor = actor;
                ("actor", stored)
            }
            Part::Action(action) => {
                event.action = action;
                ("action", stored)
            }
            Part::Outcome(outcome) => {
                event.outcome = outcome;
                ("outcome", stored)
            }
            Part::Target(target) => {
                event.targets = [target].into();
                ("targets", json!([stored]))
            }
        };
        let mut expected: Value = serde_json::from_str(STORED_BODY)?;
        let fields = expected
            .as_object_mut()
            .context("a stored body is an object")?;
        fields.insert("targets".into(), json!([]));
        fields.insert(key.into(), stored);

        let body = serde_json::to_value(StoredEvent::from(event.clone()))?;
        assert_eq!(body, expected);
        // The columns `append` writes beside the body.
        assert_eq!(event.actor.kind().as_ref(), actor_type);
        assert_eq!(body.get("outcome"), Some(&json!(event.outcome.as_ref())));
        let (types, ids) = named_records(&event);
        let named: Vec<(&str, &str)> = types
            .iter()
            .map(String::as_str)
            .zip(ids.iter().map(String::as_str))
            .collect();
        assert_eq!(named, records);
        assert_eq!(
            AuditEvent::from(serde_json::from_value::<StoredEvent>(body)?),
            event
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_body_in_the_stored_shape_reads_back() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlAuditStore::new(db.clone());
        sqlx::query(
            "INSERT INTO audit_events (event_id, at, outcome, actor_type, body)
             VALUES ('audit_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e', now(), 'succeeded', 'user', $1)",
        )
        .bind(STORED_BODY)
        .execute(db.pool())
        .await?;

        let page = store
            .list_events(&AuditFilter::default(), None, TEST_LIST_LIMIT)
            .await?;
        assert!(page.unreadable.is_empty(), "{:?}", page.unreadable);
        assert_eq!(page.items, vec![stored_event()?]);
        Ok(())
    }

    #[tokio::test]
    async fn an_append_writes_the_stored_shape() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlAuditStore::new(db.clone());
        store.append(&stored_event()?).await?;

        let body: String = sqlx::query_scalar("SELECT body FROM audit_events")
            .fetch_one(db.pool())
            .await?;
        assert_eq!(body, STORED_BODY);
        Ok(())
    }

    #[tokio::test]
    async fn meets_the_shared_contract() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlAuditStore::new(test_db.db().clone());
        pipette_store_testkit::assert_audit_store_contract(&store).await
    }

    /// A pruned event takes its organization and record rows with it.
    #[tokio::test]
    async fn a_prune_leaves_no_index_rows() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlAuditStore::new(db.clone());
        let event = pipette_store_testkit::an_audit_event(&["org_a", "org_b"])?;
        store.append(&event).await?;
        let cutoff = event.created_at + chrono::Duration::seconds(1);
        assert_eq!(store.prune_before(cutoff).await?, 1);
        for table in ["audit_event_orgs", "audit_event_records"] {
            let left: i64 =
                sqlx::query_scalar(AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(db.pool())
                    .await?;
            assert_eq!(left, 0, "{table}");
        }
        Ok(())
    }

    /// Every driver and filter combination is valid SQL with its parameters in
    /// order: the contract runs each against the database. A record's history
    /// is one walk; an organization's log walks a branch per outcome.
    #[rstest]
    #[case::a_record_in_an_org_log(true, &[AuditOutcome::Refused], Some("audit_b"), 7, 0)]
    #[case::a_record_with_two_outcomes(true, &[AuditOutcome::Refused, AuditOutcome::Failed], None, 6, 0)]
    #[case::an_org_log_with_two_outcomes(false, &[AuditOutcome::Refused, AuditOutcome::Failed], None, 5, 1)]
    fn a_listing_binds_one_parameter_per_placeholder(
        #[case] record: bool,
        #[case] outcomes: &[AuditOutcome],
        #[case] before: Option<&str>,
        #[case] placeholders: usize,
        #[case] unions: usize,
    ) -> anyhow::Result<()> {
        let filter = AuditFilter {
            org_id: Some(OrgId::try_new("org_a")?),
            outcomes: outcomes.iter().copied().collect(),
            actor_kinds: [AuditActorKind::User].into(),
            record: record
                .then(|| OrgId::try_new("org_a").map(|org_id| AuditTarget::Org { org_id }))
                .transpose()?,
        };
        let (sql, params) = listing(&filter, before, 5);
        assert_eq!(params.len(), placeholders, "{sql}");
        let last = format!("${placeholders}");
        let past = format!("${}", placeholders + 1);
        assert!(sql.contains(&last) && !sql.contains(&past), "{sql}");
        assert_eq!(sql.matches("UNION ALL").count(), unions, "{sql}");
        Ok(())
    }

    #[tokio::test]
    async fn an_unreadable_row_is_reported_by_its_object_key() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlAuditStore::new(db.clone());
        let good = pipette_store_testkit::an_audit_event(&[])?;
        store.append(&good).await?;
        sqlx::query(
            "INSERT INTO audit_events (event_id, at, outcome, actor_type, body)
             VALUES ('audit_broken', now(), 'succeeded', 'user', 'not json')",
        )
        .execute(db.pool())
        .await?;

        let page = store
            .list_events(&AuditFilter::default(), None, TEST_LIST_LIMIT)
            .await?;
        assert_eq!(page.items, vec![good]);
        let reported: Vec<&str> = page.unreadable.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(reported, vec!["audit/audit_broken.json"]);
        Ok(())
    }
}
