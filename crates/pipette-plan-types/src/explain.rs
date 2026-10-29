//! `GET /api/v1/plans/{plan_id}/explain`: why a plan's jobs are not scheduled,
//! the HTTP form of `plans status --explain`.
//!
//! Client counts and the clause ladder disclose the fleet, so a reader who may not
//! read it gets the scheduling state, its own job counts and fixed reason strings,
//! with the rest absent.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{FleetDetail, PlanId};

/// Where a plan's outstanding jobs stand on the way to a client: the first gate
/// that blocks every one of them, or `schedulable` when none does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SchedulingState {
    StillCreating,
    CancelPending,
    LeasesLapsed,
    NothingOutstanding,
    Expired,
    NotIndexed,
    Starved,
    ClientsBlocked,
    Schedulable,
    QueueMoved,
}

/// The plan's own jobs across the queue. Counts of the caller's jobs, not of
/// clients, so never withheld.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct JobCensus {
    pub total: usize,
    pub available: usize,
    pub expired: usize,
    pub claimable: usize,
    pub indexed: usize,
    pub leased: usize,
    pub lapsed_leases: usize,
    pub gone: usize,
    pub denied: usize,
    pub unparseable_avail_keys: usize,
    pub unparseable_leased_keys: usize,
}

/// The fleet's standing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct FleetStanding {
    pub registered: usize,
    pub approved: usize,
    pub working: usize,
    pub suspended: usize,
    pub pending_reindex: usize,
    pub unblocked: usize,
}

/// One rung of the clause ladder: the working clients left once `clause` is
/// added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct NarrowingRung {
    pub clause: String,
    pub surviving_client_count: usize,
}

/// How a group's `clients` roster stands against the live fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct RosterStanding {
    pub listed: usize,
    pub registered: usize,
    pub approved: usize,
    pub working: usize,
    pub eligible: usize,
}

/// One starved group, at the caller's tier. Every field but `job_count` is
/// absent at [`FleetDetail::Withheld`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct StarvedGroupExplanation {
    /// The clause set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Jobs in the group: the caller's own.
    pub job_count: usize,
    /// A job-body contradiction, which can quote the group's clauses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub never_matches: Option<String>,
    /// The clause ladder over the working fleet, narrowing one clause at a time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ladder: Option<Vec<NarrowingRung>>,
    /// Absent too for a group whose jobs name no client ids.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roster: Option<RosterStanding>,
    /// Working clients the whole clause set matches now: the count that clears
    /// the group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_working_client_count: Option<usize>,
    /// Approved clients it matches, granted or not. Above
    /// `live_working_client_count` means a `perform_work` grant clears the
    /// group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_approved_client_count: Option<usize>,
    /// Registered clients it matches, approved or not. Above
    /// `live_approved_client_count` means an approval clears the group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_registered_client_count: Option<usize>,
}

/// The explain route's body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct PlanExplanation {
    pub plan_id: PlanId,
    pub scheduling_state: SchedulingState,
    /// The scheduling state as one sentence at the caller's tier.
    pub headline: String,
    /// When the report read the queue and the fleet; every count dates from it.
    pub read_at: DateTime<Utc>,
    /// When maintenance last computed the plan's snapshot. `null` when none
    /// has, which by itself explains a plan that reads `active` and runs
    /// nothing.
    pub snapshot_computed_at: Option<DateTime<Utc>>,
    pub census: JobCensus,
    /// `true` when the live scan found the plan starved but the cached
    /// starvation set names none of its jobs, so the groups carry no ladder.
    pub starved_by_live_scan: bool,
    /// Which tier built this body, so an absent count reads as "not yours to
    /// see" rather than zero.
    pub fleet_detail: FleetDetail,
    /// Absent at [`FleetDetail::Withheld`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fleet: Option<FleetStanding>,
    pub groups: Vec<StarvedGroupExplanation>,
}

impl PlanExplanation {
    /// The report a reader at `detail` sees. The builder sets the headline,
    /// which is written for one tier.
    pub fn at(mut self, detail: FleetDetail) -> Self {
        self.fleet_detail = detail;
        if detail == FleetDetail::Withheld {
            self.fleet = None;
            for group in &mut self.groups {
                // A withheld roster and an absent one both read as "nothing
                // about the roster"; telling them apart would itself be a bit
                // about the group's clauses.
                *group = StarvedGroupExplanation {
                    description: None,
                    job_count: group.job_count,
                    never_matches: None,
                    ladder: None,
                    roster: None,
                    live_working_client_count: None,
                    live_approved_client_count: None,
                    live_registered_client_count: None,
                };
            }
        }
        self
    }
}
