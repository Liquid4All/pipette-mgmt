//! The audit log: one event for each write request and each host command that
//! changes records, refused ones included.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    AuditEventId, CampaignId, ClientId, JobId, OrgId, PlanId, UserId, UserKey, UserKeyId,
    VaultResourceId, empty_as_none,
};

/// Who acted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuditActor {
    /// An account, and the key it signed with.
    User { user_id: UserId, key_id: UserKeyId },
    /// A device.
    Client { client_id: ClientId },
    /// A request whose signature did not verify: the id it claimed, if any.
    Unverified { claimed_id: Option<String> },
    /// An operator running `pipette-mgmt` on a host.
    Host { os_user: String, hostname: String },
}

impl AuditActor {
    pub fn kind(&self) -> AuditActorKind {
        match self {
            Self::User { .. } => AuditActorKind::User,
            Self::Client { .. } => AuditActorKind::Client,
            Self::Unverified { .. } => AuditActorKind::Unverified,
            Self::Host { .. } => AuditActorKind::Host,
        }
    }

    /// The record of the account or device that acted, when the request proved
    /// it.
    pub fn record(&self) -> Option<AuditTarget> {
        match self {
            Self::User { user_id, .. } => Some(AuditTarget::User {
                user_id: user_id.clone(),
            }),
            Self::Client { client_id } => Some(AuditTarget::Client {
                client_id: client_id.clone(),
            }),
            Self::Unverified { .. } | Self::Host { .. } => None,
        }
    }
}

/// An actor's kind, as the listings filter by it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    strum::AsRefStr,
    strum::EnumString,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AuditActorKind {
    User,
    Client,
    Unverified,
    Host,
}

/// What was done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuditAction {
    /// A request to a write route: `route` is the pattern, `path` as sent
    /// without the query.
    Http {
        method: String,
        route: String,
        path: String,
        /// `499` for a caller that left before the answer.
        status: u16,
    },
    /// A `pipette-mgmt` command, such as `users create`, and its arguments.
    Cli { command: String, args: Vec<String> },
    /// An account's first Clerk sign-in, when it already held keys: each was
    /// registered before its owner signed in.
    FirstClerkSignIn { prior_keys: Vec<UserKey> },
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    strum::AsRefStr,
    strum::EnumString,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AuditOutcome {
    Succeeded,
    /// Refused for its credentials or its scope: a `401`, a `403`, or the
    /// `404` that hides a record outside the caller's scope.
    Refused,
    Failed,
}

/// A record the action named or made.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, strum::IntoStaticStr,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AuditTarget {
    Org { org_id: OrgId },
    User { user_id: UserId },
    Client { client_id: ClientId },
    Campaign { campaign_id: CampaignId },
    Plan { plan_id: PlanId },
    Job { job_id: JobId },
    VaultResource { resource_id: VaultResourceId },
}

/// One audit event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct AuditEvent {
    pub event_id: AuditEventId,
    pub created_at: DateTime<Utc>,
    pub actor: AuditActor,
    pub action: AuditAction,
    pub outcome: AuditOutcome,
    pub targets: BTreeSet<AuditTarget>,
    /// The organizations whose audit log lists the event. These are the
    /// organization of a verified actor and the organization of each target,
    /// before and after the action. An unverified event is in no
    /// organization's log.
    pub org_ids: BTreeSet<OrgId>,
    /// What the route does, in words; read from the route table, never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl AuditTarget {
    /// The record's kind: the `type` tag that serde writes.
    pub fn kind(&self) -> &'static str {
        self.into()
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Org { org_id } => org_id.as_str(),
            Self::User { user_id } => user_id.as_str(),
            Self::Client { client_id } => client_id.as_str(),
            Self::Campaign { campaign_id } => campaign_id.as_str(),
            Self::Plan { plan_id } => plan_id.as_str(),
            Self::Job { job_id } => job_id.as_str(),
            Self::VaultResource { resource_id } => resource_id.as_str(),
        }
    }
}

/// Query parameters for the audit listings, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AuditQuery {
    /// Rows a page holds: 50 by default, at most 200 (the server's
    /// `list_default_limit` and `list_max_limit`). `0` or more than that is a
    /// `400`, not clamped.
    #[cfg_attr(feature = "schemars", schemars(range(min = 1)))]
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub limit: Option<usize>,
    /// The `next_cursor` of the page before: events older than it.
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub cursor: Option<String>,
    /// The outcomes to keep, comma-separated; every outcome when absent.
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub outcome: Option<String>,
    /// The actor kinds to keep, comma-separated; every kind when absent.
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub actor: Option<String>,
    /// Only the events that name this record by id, as a target or as the
    /// account or device that acted.
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub target: Option<String>,
    /// A readable name for each id the page shows that the reader may see.
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub names: Option<bool>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The tags the app and `pipette-ctl` switch on.
    #[test]
    fn an_event_reads_as_tagged_objects() -> anyhow::Result<()> {
        let event = AuditEvent {
            event_id: AuditEventId::try_new("audit_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?,
            created_at: DateTime::from_timestamp(0, 0).unwrap_or_default(),
            actor: AuditActor::Client {
                client_id: ClientId::try_new("ev1_abc")?,
            },
            action: AuditAction::Http {
                method: "PUT".into(),
                route: "/plans/{job_id}/heartbeat".into(),
                path: "/plans/job-1/heartbeat".into(),
                status: 200,
            },
            outcome: AuditOutcome::Succeeded,
            targets: BTreeSet::from([AuditTarget::Job {
                job_id: JobId::try_new("job-1")?,
            }]),
            org_ids: BTreeSet::from([OrgId::try_new("org_a")?]),
            summary: None,
        };
        let value = serde_json::to_value(&event)?;
        assert_eq!(value["created_at"], "1970-01-01T00:00:00Z");
        assert_eq!(
            value["actor"],
            json!({"type": "client", "client_id": "ev1_abc"})
        );
        assert_eq!(value["action"]["type"], "http");
        assert_eq!(
            value["targets"],
            json!([{"type": "job", "job_id": "job-1"}])
        );
        assert_eq!(serde_json::from_value::<AuditEvent>(value)?, event);
        Ok(())
    }

    /// The `record_type` column and the JSON `type` tag are one spelling, and the
    /// id keeps the field name the other bodies give it.
    #[rstest::rstest]
    #[case::org(json!({"type": "org", "org_id": "org_a"}))]
    #[case::user(json!({"type": "user", "user_id": "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e"}))]
    #[case::client(json!({"type": "client", "client_id": "ev1_abc"}))]
    #[case::campaign(json!({"type": "campaign", "campaign_id": "campaign_0195c3e2"}))]
    #[case::plan(json!({"type": "plan", "plan_id": "plan-1"}))]
    #[case::job(json!({"type": "job", "job_id": "job-1"}))]
    #[case::vault_resource(json!({"type": "vault_resource", "resource_id": "vault_0195c3e2"}))]
    fn a_target_is_its_type_and_typed_id(#[case] wire: serde_json::Value) -> anyhow::Result<()> {
        let target: AuditTarget = serde_json::from_value(wire.clone())?;
        assert_eq!(wire["type"], target.kind());
        let ids: Vec<&serde_json::Value> = wire
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, _)| *key != "type")
            .map(|(_, id)| id)
            .collect();
        assert_eq!(ids, [target.id()]);
        assert_eq!(serde_json::to_value(&target)?, wire);
        Ok(())
    }
}
