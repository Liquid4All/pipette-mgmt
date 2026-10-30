//! An audit event: who changed what, when, and which organizations' logs list
//! it. Written once and never edited; only the retention prune removes one.

use std::collections::BTreeSet;

use chrono::Utc;

use pipette_plan_types::{AuditAction, AuditActor, AuditEvent, AuditEventId, AuditOutcome};

use crate::types::Mint;

/// How long an event is kept before `audit prune` removes it.
pub const RETENTION_DAYS: u32 = 730;

/// A new event with a fresh id, dated now. Its targets and organizations are
/// not yet known.
pub fn new_event(actor: AuditActor, action: AuditAction, outcome: AuditOutcome) -> AuditEvent {
    AuditEvent {
        event_id: AuditEventId::mint(),
        created_at: Utc::now(),
        actor,
        action,
        outcome,
        targets: BTreeSet::new(),
        org_ids: BTreeSet::new(),
        summary: None,
    }
}

pub fn outcome_of_status(status: u16) -> AuditOutcome {
    match status {
        200..=399 => AuditOutcome::Succeeded,
        401 | 403 => AuditOutcome::Refused,
        _ => AuditOutcome::Failed,
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(200, AuditOutcome::Succeeded)]
    #[case(204, AuditOutcome::Succeeded)]
    #[case(304, AuditOutcome::Succeeded)]
    #[case(401, AuditOutcome::Refused)]
    #[case(403, AuditOutcome::Refused)]
    #[case(400, AuditOutcome::Failed)]
    #[case(404, AuditOutcome::Failed)]
    #[case(500, AuditOutcome::Failed)]
    fn a_status_reads_as_an_outcome(#[case] status: u16, #[case] outcome: AuditOutcome) {
        assert_eq!(outcome_of_status(status), outcome);
    }

    /// Minted ids sort in the order the events were made, which paging relies on.
    #[test]
    fn events_sort_by_their_ids() {
        let actor = AuditActor::Unverified { claimed_id: None };
        let action = AuditAction::Cli {
            command: "audit prune".into(),
            args: vec![],
        };
        let first = new_event(actor.clone(), action.clone(), AuditOutcome::Succeeded);
        let second = new_event(actor, action, AuditOutcome::Succeeded);
        assert!(first.event_id < second.event_id);
    }
}
