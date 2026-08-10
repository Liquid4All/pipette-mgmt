//! Plan-manifest types — the durable record of an ingested plan's identity,
//! lifecycle, and progress, stored at `plans/{plan_id}.json` in the `[storage]`
//! backend by [`crate::stores::PlanStore`].
//!
//! These types are **owned here**: the ingestion pipeline (which writes the
//! initial manifest) and `queue-maintenance` (which reconciles `status` and
//! refreshes `progress_snapshot`) are pure consumers and add no types of their
//! own. See `docs/plan-ingestion.md` §9.
//!
//! [`PlanStatusView`] — the client-facing projection of a manifest — lives here
//! too, beside the record it projects, so the `plans status` CLI and
//! `GET /plans/{plan_id}` serve the same shape from one definition.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{JobId, PlanId};

/// Lifecycle state of a plan (`docs/plan-ingestion.md` §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Manifest written with the full job-id list; jobs are being staged.
    /// Nothing is claimable yet.
    Creating,
    /// All jobs promoted into `avail/`; at least one outstanding job is eligible
    /// to some registered, approved client.
    Active,
    /// Jobs promoted and outstanding, but **none** currently matches any
    /// registered, approved client — the honest state for a plan queued ahead
    /// of the fleet. Not an error.
    PendingClients,
    /// Every job in the manifest reached a terminal state.
    Complete,
    /// An operator cancelled the plan, or ingestion aborted before completing.
    Cancelled,
}

impl PlanStatus {
    /// Whether this is a terminal latch the maintenance pass never leaves
    /// (`complete` / `cancelled`).
    pub fn is_terminal(self) -> bool {
        matches!(self, PlanStatus::Complete | PlanStatus::Cancelled)
    }

    /// The `snake_case` spelling — the same string the serde form uses, for logs
    /// and operator-facing display without round-tripping through `serde_json`.
    pub fn label(self) -> &'static str {
        match self {
            PlanStatus::Creating => "creating",
            PlanStatus::Active => "active",
            PlanStatus::PendingClients => "pending_clients",
            PlanStatus::Complete => "complete",
            PlanStatus::Cancelled => "cancelled",
        }
    }
}

/// A group of jobs sharing an identical requirement set that matched no
/// registered, approved client at ingestion — the **frozen** ingestion-time
/// record (`docs/plan-ingestion.md` §6.2, §9). Grouped rather than emitted
/// per-job so a large plan stays readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    /// Human-readable summary, e.g. "2 jobs requiring os:macos match no
    /// registered, approved client".
    pub message: String,
    /// The minted job ids in this group, so the operator can see exactly which
    /// jobs are affected.
    pub job_ids: Vec<JobId>,
}

/// Progress snapshot, refreshed whole by each `queue-maintenance` run (never by
/// ingestion). Optional on the manifest — absent before the first maintenance
/// run — so its two writers never collide over the rest of the record. See
/// `docs/plan-ingestion.md` §9.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressSnapshot {
    /// When this snapshot was computed — dates `counts` and `starved` together,
    /// since the pass produces the whole snapshot in one run.
    pub computed_at: DateTime<Utc>,
    pub counts: Counts,
    /// Outstanding jobs that currently match no registered, approved client,
    /// grouped by identical requirement set — the ongoing, refreshed form of
    /// the ingestion-time [`Warning`]s, and what surfaces *partial* starvation
    /// while the plan is still `active`.
    pub starved: Vec<StarvedGroup>,
}

/// Job counts across the plan at the moment the snapshot was computed
/// (`total == finished + running + available + failed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub total: u64,
    pub finished: u64,
    pub running: u64,
    pub available: u64,
    pub failed: u64,
}

/// A group of outstanding, unmatched jobs sharing an identical requirement set.
/// Derived from the job bodies at snapshot time; not a plan-structure concept.
/// The requirement fields are opaque capability-flag strings (the server never
/// interprets them), echoed as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarvedGroup {
    /// Flat required flags — all must be present (`docs/plan-ingestion.md` §5).
    pub requires: Vec<String>,
    /// Disjunction clauses — each inner group is satisfied by at least one of
    /// its members.
    pub any_of: Vec<Vec<String>>,
    /// Explicitly listed client ids (the `clients` eligibility path).
    pub clients: Vec<String>,
    /// The minted job ids in this group.
    pub job_ids: Vec<JobId>,
}

/// The durable plan manifest, stored at `plans/{plan_id}.json` in the
/// `[storage]` backend. Its `job_ids` list is the **only** record of which jobs
/// belong to the plan — job bodies carry no `plan_id`, so every server path
/// runs plan→jobs through this list. See `docs/plan-ingestion.md` §9.
///
/// The schema is deliberately **open** — `#[serde(deny_unknown_fields)]` is
/// *not* set. The creation writer (ingestion) and the maintenance writer own
/// disjoint parts of the record and must tolerate each other's additions,
/// chiefly `progress_snapshot`, which only maintenance writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanManifest {
    pub plan_id: PlanId,
    /// Optional operator-supplied label (`--plan-name`); a human reference, not
    /// an identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_name: Option<String>,
    pub status: PlanStatus,
    pub created_at: DateTime<Utc>,
    /// Authoritative plan membership (see the struct doc).
    pub job_ids: Vec<JobId>,
    /// Frozen ingestion-time fleet-match warnings, grouped by requirement set.
    #[serde(default)]
    pub warnings: Vec<Warning>,
    /// Progress snapshot; absent until the first `queue-maintenance` run writes
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_snapshot: Option<ProgressSnapshot>,
    /// When the plan latched a terminal status (`complete` / `cancelled`); set
    /// by `queue-maintenance`, read by the retention GC. `None` while
    /// non-terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_at: Option<DateTime<Utc>>,
}

/// The client-facing projection of a [`PlanManifest`] — what `plans status`
/// renders and what `GET /plans/{plan_id}` will serialize
/// (`docs/plan-ingestion.md` §11).
///
/// Deliberately omits **exactly one** manifest field, `job_ids`: it is internal
/// plan↔job bookkeeping, and a plan's membership list is of no use to a caller
/// asking after progress. Everything else is passed through, plus
/// `cancel_requested`, which is read separately from the cancel marker rather
/// than stored on the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanStatusView {
    pub plan_id: PlanId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_name: Option<String>,
    pub status: PlanStatus,
    pub created_at: DateTime<Utc>,
    /// Whether a cancel has been requested but not yet latched into `status`.
    ///
    /// `plans cancel` only writes the out-of-band marker; `queue-maintenance`
    /// performs the latch and the teardown, so a cancel is invisible in `status`
    /// for up to one cron interval. Surfacing the marker makes that window
    /// legible instead of looking like the cancel was dropped.
    pub cancel_requested: bool,
    /// The **frozen ingestion-time** fleet-match warnings. Distinct from
    /// `progress_snapshot.starved`, which is the live, refreshed form: these two
    /// can legitimately disagree once clients register after ingestion. Kept
    /// because until the first maintenance pass runs they are the only
    /// starvation signal available.
    pub warnings: Vec<Warning>,
    /// `None` until `queue-maintenance` writes the first snapshot.
    ///
    /// Unlike [`PlanManifest::progress_snapshot`] this is **not** skipped when
    /// absent: the manifest skips it so its two writers never collide over the
    /// record, but a read-only projection should say `null` explicitly, so a
    /// caller can tell "not computed yet" from "computed, and empty".
    pub progress_snapshot: Option<ProgressSnapshot>,
    /// When the plan latched `complete` / `cancelled`; `None` while it is still
    /// live. Not derivable by a caller from anything else the projection
    /// carries — `expires_at` is a per-job future deadline, whereas this is the
    /// past moment the plan ended — so a caller asking after a finished plan
    /// would otherwise have no way to learn *when* it finished.
    ///
    /// Like `progress_snapshot`, not skipped when absent: `null` states "still
    /// live" rather than leaving the field's meaning to inference.
    pub terminal_at: Option<DateTime<Utc>>,
}

impl PlanStatusView {
    /// Project a manifest, folding in the separately-read cancel-marker state.
    pub fn new(manifest: PlanManifest, cancel_requested: bool) -> Self {
        Self {
            plan_id: manifest.plan_id,
            plan_name: manifest.plan_name,
            status: manifest.status,
            created_at: manifest.created_at,
            cancel_requested,
            warnings: manifest.warnings,
            progress_snapshot: manifest.progress_snapshot,
            terminal_at: manifest.terminal_at,
        }
    }
}

/// Shared test fixture: a manifest with a fixed `created_at`, optionally
/// carrying a `progress_snapshot`. Used by the `PlanStore` round-trip tests in
/// both backends so they exercise identical shapes.
#[cfg(test)]
pub(crate) fn sample_manifest(
    plan_id: PlanId,
    status: PlanStatus,
    with_snapshot: bool,
) -> PlanManifest {
    let created_at: DateTime<Utc> = "2026-07-20T17:55:00Z".parse().unwrap();
    PlanManifest {
        plan_id,
        plan_name: Some("afm-smoke".to_string()),
        status,
        created_at,
        job_ids: vec![JobId::new_unchecked("job-1"), JobId::new_unchecked("job-2")],
        warnings: vec![Warning {
            message: "2 jobs requiring os:macos match no registered, approved client".to_string(),
            job_ids: vec![JobId::new_unchecked("job-2")],
        }],
        progress_snapshot: with_snapshot.then(|| ProgressSnapshot {
            computed_at: created_at,
            counts: Counts {
                total: 2,
                finished: 1,
                running: 0,
                available: 1,
                failed: 0,
            },
            starved: vec![StarvedGroup {
                requires: vec!["os:macos".to_string()],
                any_of: vec![],
                clients: vec![],
                job_ids: vec![JobId::new_unchecked("job-2")],
            }],
        }),
        // A terminal plan carries the timestamp `queue-maintenance` stamps when
        // it latches the plan terminal, so the store round-trips exercise
        // `terminal_at` populated as well as absent.
        terminal_at: status.is_terminal().then_some(created_at),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plan_status_serializes_snake_case() -> anyhow::Result<()> {
        assert_eq!(json!(PlanStatus::PendingClients), json!("pending_clients"));
        assert_eq!(json!(PlanStatus::Creating), json!("creating"));
        assert_eq!(
            serde_json::from_value::<PlanStatus>(json!("complete"))?,
            PlanStatus::Complete
        );
        Ok(())
    }

    #[test]
    fn plan_status_is_terminal() {
        assert!(PlanStatus::Complete.is_terminal());
        assert!(PlanStatus::Cancelled.is_terminal());
        assert!(!PlanStatus::Active.is_terminal());
        assert!(!PlanStatus::Creating.is_terminal());
        assert!(!PlanStatus::PendingClients.is_terminal());
    }

    /// A minimal `creating` manifest (no snapshot, no terminal_at) round-trips,
    /// and the optional fields are omitted from the serialized form.
    #[test]
    fn manifest_minimal_roundtrip_omits_optionals() -> anyhow::Result<()> {
        let manifest = PlanManifest {
            plan_id: PlanId::from_uuid(uuid::Uuid::nil()),
            plan_name: None,
            status: PlanStatus::Creating,
            created_at: Utc::now(),
            job_ids: vec![JobId::from_uuid(uuid::Uuid::nil())],
            warnings: vec![],
            progress_snapshot: None,
            terminal_at: None,
        };
        let value = serde_json::to_value(&manifest)?;
        assert!(value.get("progress_snapshot").is_none());
        assert!(value.get("terminal_at").is_none());
        assert!(value.get("plan_name").is_none());
        let back: PlanManifest = serde_json::from_value(value)?;
        assert_eq!(back, manifest);
        Ok(())
    }

    /// A full, terminal manifest (snapshot + terminal_at + warnings) round-trips,
    /// and `terminal_at` — the field the retention GC keys off — actually
    /// serializes when populated.
    #[test]
    fn manifest_full_roundtrip() -> anyhow::Result<()> {
        let now = Utc::now();
        let manifest = PlanManifest {
            plan_id: PlanId::from_uuid(uuid::Uuid::nil()),
            plan_name: Some("afm-smoke".to_string()),
            status: PlanStatus::Complete,
            created_at: now,
            job_ids: vec![JobId::from_uuid(uuid::Uuid::nil())],
            warnings: vec![Warning {
                message: "2 jobs requiring os:macos match no registered, approved client"
                    .to_string(),
                job_ids: vec![JobId::from_uuid(uuid::Uuid::nil())],
            }],
            progress_snapshot: Some(ProgressSnapshot {
                computed_at: now,
                counts: Counts {
                    total: 3,
                    finished: 1,
                    running: 1,
                    available: 1,
                    failed: 0,
                },
                starved: vec![StarvedGroup {
                    requires: vec!["os:macos".to_string()],
                    any_of: vec![],
                    clients: vec![],
                    job_ids: vec![JobId::from_uuid(uuid::Uuid::nil())],
                }],
            }),
            terminal_at: Some(now),
        };
        let value = serde_json::to_value(&manifest)?;
        assert!(value.get("terminal_at").is_some());
        let back: PlanManifest = serde_json::from_value(value)?;
        assert_eq!(back, manifest);
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
}
