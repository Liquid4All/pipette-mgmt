//! Job ↔ client eligibility matching, specified in `docs/plan-ingestion.md`.
//! Pure: every decision reads the body and one client, and the fleet reports go
//! through [`job_matches_client`] so they cannot disagree with the scheduler.

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

use pipette_plan_types::{
    ClientId, ClientPermission, NarrowingRung, RosterStanding, StarvedClauses, Tag,
};

use crate::client::{
    Client, ClientStatus, client_version_of, is_reserved_capability, is_tag_capability, tag_flag,
};
use crate::client_permission::ClientPermissionSet;
use crate::plan::StarvedGroup;

/// Which clients the matcher may offer work to: the fleet-wide counterpart of
/// `ClientRights` at the claim door. Counting every approved client would match
/// a plan to a fleet the door turns away. A client not named here holds nothing.
#[derive(Debug, Clone, Default)]
pub struct FleetRights(HashMap<ClientId, ClientPermissionSet>);

impl FleetRights {
    /// The rights of a whole fleet, from `ClientStore::list_client_permissions`.
    pub fn new(sets: impl IntoIterator<Item = (ClientId, ClientPermissionSet)>) -> Self {
        Self(sets.into_iter().collect())
    }

    /// Whether the matcher may offer work to `client`: approved, and holding
    /// `perform_work`.
    pub fn admits(&self, client: &Client) -> bool {
        client.status == ClientStatus::Approved && self.holds_perform_work(&client.client_id)
    }

    /// The second gate alone, so [`roster_standing`] can say which gate
    /// dropped a client.
    fn holds_perform_work(&self, client_id: &ClientId) -> bool {
        crate::client_permission::effective_holds(
            self.0.get(client_id),
            ClientPermission::PerformWork,
        )
    }

    pub fn workers<'a>(&self, fleet: impl IntoIterator<Item = &'a Client>) -> Vec<&'a Client> {
        fleet
            .into_iter()
            .filter(|client| self.admits(client))
            .collect()
    }
}

/// The fleet's tags, as `tag:<tag>` flags ([`tag_flag`]). Tags live in the tag
/// index, not on the client record. A client not named here has no tags.
#[derive(Debug, Clone, Default)]
pub struct FleetTags(HashMap<ClientId, BTreeSet<Tag>>);

impl FleetTags {
    /// The tags of a whole fleet, from `TagStore::list_forward_tag_markers`,
    /// or of one client.
    pub fn new(markers: impl IntoIterator<Item = (ClientId, Tag)>) -> Self {
        Self(
            markers
                .into_iter()
                .fold(HashMap::new(), |mut tags, (client_id, tag)| {
                    tags.entry(client_id).or_default().insert(tag);
                    tags
                }),
        )
    }

    /// A job's view of `client`: effective capabilities and tags. A `tag:` flag
    /// the client reported itself counts for nothing; only an operator assigns one.
    pub fn capabilities_of(&self, client: &Client) -> BTreeSet<String> {
        let mut capabilities = client.effective_capabilities();
        capabilities.retain(|flag| !is_tag_capability(flag));
        if let Some(tags) = self.0.get(&client.client_id) {
            capabilities.extend(tags.iter().map(tag_flag));
        }
        capabilities
    }
}

/// True if `client` is eligible for the job described by `job_body`.
///
/// ```text
/// eligible = NOT excluded
///            AND ( client_id ∈ job_body.clients                  (explicit list)
///                  OR ( effective_capabilities(client) ⊇ job_body.requires
///                       AND every job_body.any_of group shares at least
///                           one flag with effective_capabilities(client) ) )
///
/// excluded = client_id ∈ job_body.exclude_clients
///            OR effective_capabilities(client) shares a flag with
///               job_body.excludes
///            OR some job_body.not_all_of group is contained in
///               effective_capabilities(client)
/// ```
///
/// `effective_capabilities` includes `tags` as `tag:<tag>` flags. An empty
/// `requires` matches nobody through capabilities (zero requirements would
/// match everyone), and `any_of` only narrows that path. A malformed clause of
/// any kind makes the job match nobody, roster included: dropping a bad
/// exclusion would widen eligibility.
pub fn job_matches_client(job_body: &Value, client: &Client, tags: &FleetTags) -> bool {
    Clauses::parse(job_body).is_some_and(|clauses| {
        clauses.admit(job_body, &client.client_id, &tags.capabilities_of(client))
    })
}

/// The builds `client` may run the job on: each reported `client_version:<v>`
/// the job admits when it is the client's only version flag. Ascending, and
/// `[]` when the client reports none or none fits alone.
pub fn admitted_client_versions(
    job_body: &Value,
    client: &Client,
    tags: &FleetTags,
) -> Vec<String> {
    let Some(clauses) = Clauses::parse(job_body) else {
        return Vec::new();
    };
    let (versions, others): (BTreeSet<String>, BTreeSet<String>) = tags
        .capabilities_of(client)
        .into_iter()
        .partition(|flag| client_version_of(flag).is_some());
    versions
        .iter()
        .filter(|flag| {
            let mut capabilities = others.clone();
            capabilities.insert((*flag).clone());
            clauses.admit(job_body, &client.client_id, &capabilities)
        })
        .filter_map(|flag| client_version_of(flag))
        // `client_version:` names no build, so it is no answer.
        .filter(|version| !version.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether `flag` is in a namespace that picks where a job runs: a tag or a
/// client build.
pub fn is_routing_flag(flag: &str) -> bool {
    is_tag_capability(flag) || client_version_of(flag).is_some()
}

/// The suffix that marks a positive flag of a plan as required. Ingestion
/// takes it off and lists the plain flag in [`REQUIRED_FLAGS`].
pub const REQUIRED_SUFFIX: &str = ":required";

/// The body field listing the flags its plan marked [`REQUIRED_SUFFIX`]: what
/// a finished run must be shown to have held, routing namespace or not.
pub const REQUIRED_FLAGS: &str = "required_flags";

/// Whether a finished run under `before`'s clauses, whose record proves it held
/// `run` ([`crate::client::recorded_run_flags`]), still answers `after`'s,
/// routing flags and rosters aside. Unknown flags and new exclusions fail
/// closed.
pub fn run_answers(before: &Value, after: &Value, run: &BTreeSet<String>) -> bool {
    let (Some(was_clauses), Some(now_clauses), Some(was_required), Some(now_required)) = (
        Clauses::parse(before),
        Clauses::parse(after),
        required_flags(before),
        required_flags(after),
    ) else {
        return false;
    };
    let was = Measured::of(&was_clauses, &was_required);
    let now = Measured::of(&now_clauses, &now_required);
    // A run through the roster need not have held what `before` required, so
    // neither an unchanged body nor its `requires` vouches for it.
    let roster_run = !was_clauses.effective_roster_empty(before);
    if was == now && !roster_run {
        return true;
    }
    // A record names the device as its client reported it, not always as its
    // registration did, so a flag missing from `run` excludes nothing.
    if !now.excludes.is_subset(&was.excludes) || !now.not_all_of.is_subset(&was.not_all_of) {
        return false;
    }
    let held = if roster_run {
        BTreeSet::new()
    } else {
        was_clauses.requires
    };
    let recorded = |flag: &str| is_reserved_capability(flag) || client_version_of(flag).is_some();
    let known = now
        .positive_flags()
        .all(|flag| recorded(flag) || held.contains(flag));
    known && now.met_by(&run.union(&held).cloned().collect())
}

/// A body's [`REQUIRED_FLAGS`]: empty when absent, `None` when malformed.
fn required_flags(body: &Value) -> Option<BTreeSet<String>> {
    match body.get(REQUIRED_FLAGS) {
        None => Some(BTreeSet::new()),
        Some(flags) => flag_set(flags.as_array()?),
    }
}

/// A job's clauses on what its run measures: [`Clauses`] without the routing
/// flags it does not require, as sets so that order and repeats compare equal.
#[derive(Debug, PartialEq, Eq)]
struct Measured {
    requires: BTreeSet<String>,
    excludes: BTreeSet<String>,
    any_of: BTreeSet<BTreeSet<String>>,
    not_all_of: BTreeSet<BTreeSet<String>>,
}

impl Measured {
    fn of(clauses: &Clauses, required: &BTreeSet<String>) -> Self {
        let routes = |flag: &String| is_routing_flag(flag) && !required.contains(flag);
        let measured = |flags: &BTreeSet<String>| -> BTreeSet<String> {
            flags.iter().filter(|flag| !routes(flag)).cloned().collect()
        };
        // An all-routing group says nothing of the run; an empty or a mixed
        // one is a claim of its own and stays whole.
        let groups = |groups: &[BTreeSet<String>]| -> BTreeSet<BTreeSet<String>> {
            groups
                .iter()
                .filter(|group| group.is_empty() || !group.iter().all(routes))
                .cloned()
                .collect()
        };
        Self {
            requires: measured(&clauses.requires),
            excludes: measured(&clauses.excludes),
            any_of: groups(&clauses.any_of),
            not_all_of: groups(&clauses.not_all_of),
        }
    }

    fn positive_flags(&self) -> impl Iterator<Item = &String> {
        self.requires.iter().chain(self.any_of.iter().flatten())
    }

    /// [`capabilities_satisfy`] over `flags`, without the non-empty
    /// `requires` the scheduler demands: the run already happened.
    fn met_by(&self, flags: &BTreeSet<String>) -> bool {
        self.requires.is_subset(flags) && self.any_of.iter().all(|group| !group.is_disjoint(flags))
    }
}

/// The five fail-closed eligibility clauses of a job body, parsed once.
/// `clients` is absent: a malformed roster narrows rather than widens, so plan
/// ingestion alone checks it.
struct Clauses {
    requires: BTreeSet<String>,
    excludes: BTreeSet<String>,
    exclude_clients: BTreeSet<String>,
    any_of: Vec<BTreeSet<String>>,
    not_all_of: Vec<BTreeSet<String>>,
}

impl Clauses {
    /// The body's clauses, or `None` when one of them is malformed.
    fn parse(job_body: &Value) -> Option<Self> {
        // An absent `requires` is the ordinary shape of a `clients`-only job, so
        // it reads as the empty set. A present one must parse.
        let requires = match job_body.get("requires") {
            None => BTreeSet::new(),
            Some(_) => job_requires(job_body)?,
        };
        Some(Self {
            requires,
            excludes: job_excludes(job_body)?,
            exclude_clients: job_exclude_clients(job_body)?,
            any_of: job_any_of(job_body)?,
            not_all_of: job_not_all_of(job_body)?,
        })
    }

    /// The one evaluation every match runs: no negative names the client, and
    /// the roster or the capability clauses admit it.
    fn admit(
        &self,
        job_body: &Value,
        client_id: &ClientId,
        capabilities: &BTreeSet<String>,
    ) -> bool {
        !self.excludes_client(client_id, capabilities)
            && (client_listed(job_body, client_id)
                || capabilities_satisfy(&self.requires, &self.any_of, capabilities))
    }

    fn excludes_client(&self, client_id: &ClientId, capabilities: &BTreeSet<String>) -> bool {
        self.exclude_clients.contains(client_id.as_str())
            || !self.excludes.is_disjoint(capabilities)
            || self
                .not_all_of
                .iter()
                .any(|group| group.is_subset(capabilities))
    }

    /// The `clients` roster minus `exclude_clients`: the one place that
    /// subtraction is written.
    fn effective_roster<'b>(&self, job_body: &'b Value) -> Vec<&'b str> {
        job_body
            .get("clients")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|id| !self.exclude_clients.contains(*id))
            .collect()
    }

    fn effective_roster_empty(&self, job_body: &Value) -> bool {
        self.effective_roster(job_body).is_empty()
    }
}

/// Whether `flags` holds every flag in `requires` and at least one flag of each
/// `any_of` group. An empty `requires` or an empty `any_of` group matches no
/// device.
pub fn capabilities_satisfy(
    requires: &BTreeSet<String>,
    any_of: &[BTreeSet<String>],
    flags: &BTreeSet<String>,
) -> bool {
    !requires.is_empty()
        && requires.is_subset(flags)
        && any_of.iter().all(|group| !group.is_disjoint(flags))
}

/// A job's six eligibility clauses, borrowed, as the body stores them. Named
/// fields, because six same-typed slices transpose without a compile error.
#[derive(Debug, Clone, Copy, Default)]
pub struct NarrowingClauses<'a> {
    pub clients: &'a [String],
    pub exclude_clients: &'a [String],
    pub requires: &'a [String],
    pub excludes: &'a [String],
    pub any_of: &'a [Vec<String>],
    pub not_all_of: &'a [Vec<String>],
}

impl<'a> From<&'a StarvedGroup> for NarrowingClauses<'a> {
    fn from(group: &'a StarvedGroup) -> Self {
        Self {
            clients: &group.clauses.clients,
            exclude_clients: &group.clauses.exclude_clients,
            requires: &group.clauses.requires,
            excludes: &group.clauses.excludes,
            any_of: &group.clauses.any_of,
            not_all_of: &group.clauses.not_all_of,
        }
    }
}

impl NarrowingClauses<'_> {
    /// These clauses as a job body, so [`job_matches_client`] itself answers a
    /// question about them. The one place that builds a synthetic body, so a new
    /// clause is one edit. An empty clause constrains nothing, except `requires`,
    /// where empty matches no client through capabilities.
    pub fn body(&self) -> Value {
        serde_json::json!({
            "clients": self.clients,
            "exclude_clients": self.exclude_clients,
            "requires": self.requires,
            "any_of": self.any_of,
            "excludes": self.excludes,
            "not_all_of": self.not_all_of,
        })
    }
}

/// Narrow `clients` one clause at a time, and report the survivors after each;
/// the rung that reads zero names the clause that blocks the match.
///
/// `exclude_clients` leads (its count is the fleet before any capability
/// clause), then the order [`describe_clauses`] emits. The order picks which
/// rung names the block, never the final count. An empty `requires` returns an
/// empty ladder: every rung would read zero and blame the wrong clause, and the
/// job matches through its roster alone ([`roster_standing`]).
pub fn narrow_fleet(
    clauses: NarrowingClauses<'_>,
    clients: &[&Client],
    tags: &FleetTags,
) -> Vec<NarrowingRung> {
    if clauses.requires.is_empty() {
        return Vec::new();
    }
    // The first rung's count only: the rungs below carry `exclude_clients` in
    // their bodies, so this set difference never feeds a capability count.
    let excluded: BTreeSet<&str> = clauses.exclude_clients.iter().map(String::as_str).collect();
    let admitted_ids = clients
        .iter()
        .filter(|client| !excluded.contains(client.client_id.as_str()))
        .count();
    let surviving = |body: &Value| {
        clients
            .iter()
            .filter(|client| job_matches_client(body, client, tags))
            .count()
    };

    // One prefix, widened a clause at a time, so a rung reports every clause
    // above it. `clients` stays empty: a roster is `roster_standing`'s question.
    let mut prefix = NarrowingClauses {
        exclude_clients: clauses.exclude_clients,
        requires: clauses.requires,
        ..Default::default()
    };
    let rung = |clause: String, prefix: &NarrowingClauses<'_>| NarrowingRung {
        clause,
        surviving_client_count: surviving(&prefix.body()),
    };

    let mut rungs = Vec::new();
    if !clauses.exclude_clients.is_empty() {
        // The one rung that is a set difference, for the reason
        // `NarrowingRung` gives: no body expresses "the fleet minus these ids".
        rungs.push(NarrowingRung {
            clause: format!("exclude_clients {}", flags_label(clauses.exclude_clients)),
            surviving_client_count: admitted_ids,
        });
    }
    rungs.push(rung(
        format!("requires {}", flags_label(clauses.requires)),
        &prefix,
    ));
    if !clauses.excludes.is_empty() {
        prefix.excludes = clauses.excludes;
        rungs.push(rung(
            format!("excludes {}", flags_label(clauses.excludes)),
            &prefix,
        ));
    }
    if !clauses.any_of.is_empty() {
        prefix.any_of = clauses.any_of;
        rungs.push(rung(
            format!("any_of {}", groups_label(clauses.any_of)),
            &prefix,
        ));
    }
    if !clauses.not_all_of.is_empty() {
        prefix.not_all_of = clauses.not_all_of;
        rungs.push(rung(
            format!("not_all_of {}", groups_label(clauses.not_all_of)),
            &prefix,
        ));
    }
    rungs
}

/// How `roster` — a job's `clients` array — stands against `fleet`, for the
/// job [`narrow_fleet`] declines: one with no `requires`.
///
/// Eligibility runs through [`job_matches_client`] over the working ids, so the
/// exclusion rules stay the scheduler's. `fleet` holds every client, so the
/// counts above `eligible` say why an id dropped. Duplicate ids count once.
pub fn roster_standing(
    clauses: NarrowingClauses<'_>,
    fleet: &[&Client],
    rights: &FleetRights,
    tags: &FleetTags,
) -> RosterStanding {
    let roster = clauses.clients;
    let by_id: HashMap<&str, &Client> = fleet
        .iter()
        .map(|client| (client.client_id.as_str(), *client))
        .collect();
    let listed: BTreeSet<&str> = roster.iter().map(String::as_str).collect();
    let registered: Vec<&Client> = listed
        .iter()
        .filter_map(|id| by_id.get(id).copied())
        .collect();
    let approved: Vec<&Client> = registered
        .iter()
        .copied()
        .filter(|client| client.status == ClientStatus::Approved)
        .collect();
    // The second gate on its own, so a roster of approved-but-ungranted ids
    // reads `approved: n, working: 0` and names the grant as the fix.
    let working: Vec<&Client> = approved
        .iter()
        .copied()
        .filter(|client| rights.holds_perform_work(&client.client_id))
        .collect();
    // One body with `requires` and `any_of` empty, so the count is the listed
    // clients that no exclusion names.
    let body = NarrowingClauses {
        clients: roster,
        exclude_clients: clauses.exclude_clients,
        excludes: clauses.excludes,
        not_all_of: clauses.not_all_of,
        ..Default::default()
    }
    .body();
    RosterStanding {
        listed: listed.len(),
        registered: registered.len(),
        approved: approved.len(),
        working: working.len(),
        eligible: working
            .iter()
            .copied()
            .filter(|client| job_matches_client(&body, client, tags))
            .count(),
    }
}

/// A flat flag or id list, as a ladder rung and a group header both spell it.
pub(crate) fn flags_label(flags: &[String]) -> String {
    flags.join(", ")
}

/// A list of clause groups, as a ladder rung and a group header both read it.
pub(crate) fn groups_label(groups: &[Vec<String>]) -> String {
    groups
        .iter()
        .map(|group| format!("[{}]", flags_label(group)))
        .collect::<Vec<_>>()
        .join(" & ")
}

/// One-line description of the eligibility every job in a starved group shares.
/// The ingestion warning and `plans show` both render through it, so they
/// cannot drift.
pub fn describe_clauses(clauses: &StarvedClauses) -> String {
    let flags = flags_label;
    let groups = groups_label;
    let clauses: Vec<String> = [
        (!clauses.requires.is_empty()).then(|| format!("requiring {}", flags(&clauses.requires))),
        (!clauses.excludes.is_empty())
            .then(|| format!("excluding flags {}", flags(&clauses.excludes))),
        (!clauses.any_of.is_empty()).then(|| format!("with any-of {}", groups(&clauses.any_of))),
        (!clauses.not_all_of.is_empty())
            .then(|| format!("with not-all-of {}", groups(&clauses.not_all_of))),
        (!clauses.clients.is_empty()).then(|| format!("pinned to {}", flags(&clauses.clients))),
        (!clauses.exclude_clients.is_empty())
            .then(|| format!("excluding clients {}", flags(&clauses.exclude_clients))),
    ]
    .into_iter()
    .flatten()
    .collect();
    match clauses.is_empty() {
        true => "with no eligibility".to_string(),
        false => clauses.join(" "),
    }
}

/// The **effective** roster of a `clients`-only job (non-empty `clients` minus
/// `exclude_clients`, no non-empty `requires`), or `None` for any other shape.
///
/// Such a job's eligible set is closed, so once every listed client is denied
/// it escalates to a `"system"` failure; a `requires` job stays open to new
/// clients. Only the id exclusion counts: `excludes` and `not_all_of` read
/// capabilities a device can change back, which would retire a job on a
/// reversible condition. An empty effective roster or a malformed body answers
/// `None`: the retirement pass owns those.
pub fn clients_only_roster(job_body: &Value) -> Option<Vec<&str>> {
    let clauses = Clauses::parse(job_body)?;
    let roster = clauses.effective_roster(job_body);
    (!roster.is_empty() && clauses.requires.is_empty()).then_some(roster)
}

/// Exact equality: client ids are opaque.
fn client_listed(job_body: &Value, client_id: &ClientId) -> bool {
    job_body
        .get("clients")
        .and_then(Value::as_array)
        .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some(client_id.as_str())))
}

/// A JSON array of flags as a set, or `None` if any element is not a string:
/// dropping one would leave a smaller set, easier to satisfy.
fn flag_set(flags: &[Value]) -> Option<BTreeSet<String>> {
    flags
        .iter()
        .map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// `None` when `requires` is absent or malformed.
fn job_requires(job_body: &Value) -> Option<BTreeSet<String>> {
    flag_set(job_body.get("requires")?.as_array()?)
}

/// An absent `excludes` is the empty set; a malformed one is `None`.
fn job_excludes(job_body: &Value) -> Option<BTreeSet<String>> {
    let Some(excludes) = job_body.get("excludes") else {
        return Some(BTreeSet::new());
    };
    flag_set(excludes.as_array()?)
}

/// An absent `exclude_clients` is the empty set; a malformed one is `None`.
fn job_exclude_clients(job_body: &Value) -> Option<BTreeSet<String>> {
    let Some(ids) = job_body.get("exclude_clients") else {
        return Some(BTreeSet::new());
    };
    flag_set(ids.as_array()?)
}

/// `None` when malformed, no groups when absent. An empty group is well-formed
/// but unsatisfiable, so it stays and matches nobody.
fn job_any_of(job_body: &Value) -> Option<Vec<BTreeSet<String>>> {
    flag_groups(job_body, "any_of")
}

/// As [`job_any_of`]; an empty group excludes every client.
fn job_not_all_of(job_body: &Value) -> Option<Vec<BTreeSet<String>>> {
    flag_groups(job_body, "not_all_of")
}

fn flag_groups(job_body: &Value, field: &str) -> Option<Vec<BTreeSet<String>>> {
    let Some(groups) = job_body.get(field) else {
        return Some(Vec::new());
    };
    groups
        .as_array()?
        .iter()
        .map(|group| group.as_array().and_then(|flags| flag_set(flags)))
        .collect()
}

/// True when a **fail-closed** clause (every eligibility clause but `clients`)
/// is malformed; an absent clause never is. Such a job silently matches nobody,
/// so ingestion rejects it and the retirement pass retires a queued one.
pub fn capability_clauses_malformed(job_body: &Value) -> bool {
    Clauses::parse(job_body).is_none()
}

/// The rule that proves **no possible client** can match `job_body`, as a
/// complete operator-facing reason, or `None`. A property of the body alone,
/// never of the fleet. A malformed body answers `None`: call
/// [`capability_clauses_malformed`] first.
///
/// An empty `not_all_of` group excludes everyone. Otherwise both branches must
/// be dead: the roster when `clients` minus `exclude_clients` is empty (a flag
/// exclusion cannot kill it), the capability path when `requires` is empty, a
/// required flag is excluded, a `not_all_of` group is within `requires`, or an
/// `any_of` group is empty or wholly excluded.
///
/// Sound but deliberately incomplete: the general question is monotone CNF
/// satisfiability, which is NP-complete.
pub fn unmatchable_reason(job_body: &Value) -> Option<String> {
    let clauses = Clauses::parse(job_body)?;
    if clauses.not_all_of.iter().any(BTreeSet::is_empty) {
        return Some(
            "an empty `not_all_of` group excludes every client, whatever its id or flags"
                .to_owned(),
        );
    }
    let branch = clauses
        .effective_roster_empty(job_body)
        .then(|| clauses.capability_branch_reason())
        .flatten()?;
    Some(format!("{}, and {branch}", dead_roster_reason(job_body)))
}

/// The head of an [`unmatchable_reason`] with a dead roster branch. A `clients`
/// value holding no id string is a shape fault, so the reason must not blame
/// an `exclude_clients` the body never carried.
fn dead_roster_reason(job_body: &Value) -> &'static str {
    let names_no_id = match job_body.get("clients") {
        None => false,
        Some(Value::Array(ids)) => !ids.is_empty() && !ids.iter().any(Value::is_string),
        Some(_) => true,
    };
    if names_no_id {
        "the `clients` value is not an array of client ids"
    } else {
        "the `clients` roster is empty or wholly excluded"
    }
}

impl Clauses {
    /// The rule that kills the capability path, or `None`; see
    /// [`unmatchable_reason`].
    fn capability_branch_reason(&self) -> Option<&'static str> {
        if self.requires.is_empty() {
            return Some("`requires` is empty, so the capability path matches nobody");
        }
        if !self.requires.is_disjoint(&self.excludes) {
            return Some("`excludes` names a flag `requires` demands");
        }
        if self
            .not_all_of
            .iter()
            .any(|group| group.is_subset(&self.requires))
        {
            return Some("a `not_all_of` group holds only flags `requires` demands");
        }
        if self.any_of.iter().any(BTreeSet::is_empty) {
            return Some("an `any_of` group is empty, so nothing satisfies it");
        }
        self.any_of
            .iter()
            .any(|group| group.is_subset(&self.excludes))
            .then_some("`excludes` covers every member of an `any_of` group")
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;
    use serde_json::json;

    use pipette_plan_types::{
        ClientId, ContactEmail, DeviceFormFactor, DeviceProfile, NonEmptyTrimmedString,
        PublicKeyHex,
    };

    use super::*;
    use crate::client::Client;

    /// A client with the given device profile and reported capabilities.
    fn client_with(
        id: &str,
        profile: DeviceProfile,
        capabilities: &[&str],
    ) -> anyhow::Result<Client> {
        Ok(Client {
            client_id: ClientId::try_new(id)?,
            public_key: PublicKeyHex::try_new(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            )?,
            organization: NonEmptyTrimmedString::try_new("org")?,
            client_details: NonEmptyTrimmedString::try_new("details")?,
            contact_email: ContactEmail::try_new("a@b.com")?,
            status: crate::client::ClientStatus::Approved,
            registered_at: "2026-01-01T00:00:00Z".parse()?,
            device_profile: profile,
            capabilities: capabilities.iter().map(|s| s.to_string()).collect(),
        })
    }

    /// Written with `ok()` so a `#[case]` can call it; a literal that stopped
    /// validating reads as an absent field, a visible failure.
    fn nz(s: &str) -> Option<NonEmptyTrimmedString> {
        NonEmptyTrimmedString::try_new(s).ok()
    }

    /// The tags `ev1_a` carries in the tests below.
    fn tagged_a(tags: &[&str]) -> anyhow::Result<FleetTags> {
        let client_id = ClientId::try_new("ev1_a")?;
        Ok(FleetTags::new(
            tags.iter()
                .map(|tag| Ok((client_id.clone(), Tag::try_new(*tag)?)))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ))
    }

    /// [`job_matches_client`] over a fleet that carries no tags.
    fn matches_untagged(job: &Value, client: &Client) -> bool {
        job_matches_client(job, client, &FleetTags::default())
    }

    /// A vLLM or SGLang job as the planner expands it, routed by a tag: the
    /// author's `requires`, and a GPU or an AVX-512 CPU (PIP-504).
    #[rstest]
    #[case::gpu(&["accel:gpu"], true)]
    #[case::avx512(&["cpu_feature:avx512f"], true)]
    #[case::both(&["accel:gpu", "cpu_feature:avx512f"], true)]
    #[case::neither(&[], false)]
    fn server_runtime_job_needs_a_gpu_or_avx512(
        #[case] reported: &[&str],
        #[case] eligible: bool,
    ) -> anyhow::Result<()> {
        let client = client_with("ev1_a", DeviceProfile::default(), reported)?;
        let job = json!({
            "requires": ["tag:lab-h100-eval"],
            "any_of": [["accel:gpu", "cpu_feature:avx512f"]],
        });
        let tags = tagged_a(&["lab-h100-eval"])?;
        assert_eq!(job_matches_client(&job, &client, &tags), eligible);
        Ok(())
    }

    /// A tag is a flag of the client that carries it, in every clause, and an
    /// operator assigns it: one the client reported itself matches nothing.
    #[rstest]
    #[case::required(json!({"requires": ["tag:lab-h100-eval"]}), &["lab-h100-eval"], &[], true)]
    #[case::required_missing(json!({"requires": ["tag:lab-h100-eval"]}), &["other"], &[], false)]
    #[case::required_untagged(json!({"requires": ["tag:lab-h100-eval"]}), &[], &[], false)]
    #[case::required_beside_flags(json!({"requires": ["tag:lab-h100-eval", "runtime:vllm"]}), &["lab-h100-eval"], &["runtime:vllm"], true)]
    #[case::reported_not_assigned(json!({"requires": ["tag:lab-h100-eval"]}), &[], &["tag:lab-h100-eval"], false)]
    #[case::any_of(json!({"requires": ["runtime:vllm"], "any_of": [["tag:a", "tag:b"]]}), &["b"], &["runtime:vllm"], true)]
    #[case::excluded(json!({"requires": ["runtime:vllm"], "excludes": ["tag:busy"]}), &["busy"], &["runtime:vllm"], false)]
    #[case::excluded_other(json!({"requires": ["runtime:vllm"], "excludes": ["tag:busy"]}), &["idle"], &["runtime:vllm"], true)]
    #[case::not_all_of_whole_group(json!({"requires": ["runtime:vllm"], "not_all_of": [["tag:a", "tag:b"]]}), &["a", "b"], &["runtime:vllm"], false)]
    #[case::not_all_of_partial_group(json!({"requires": ["runtime:vllm"], "not_all_of": [["tag:a", "tag:b"]]}), &["a"], &["runtime:vllm"], true)]
    // The roster branch reads no tag, and an excluded tag still beats it.
    #[case::roster_needs_no_tag(json!({"clients": ["ev1_a"], "requires": ["tag:lab-h100-eval"]}), &[], &[], true)]
    #[case::excluded_tag_beats_the_roster(json!({"clients": ["ev1_a"], "excludes": ["tag:busy"]}), &["busy"], &[], false)]
    // A reported tag is dropped from the negatives too: it excludes nothing.
    #[case::reported_tag_excludes_nothing(json!({"requires": ["runtime:vllm"], "excludes": ["tag:busy"]}), &[], &["runtime:vllm", "tag:busy"], true)]
    fn tags_match_as_tag_flags(
        #[case] job: Value,
        #[case] tags: &[&str],
        #[case] reported: &[&str],
        #[case] eligible: bool,
    ) -> anyhow::Result<()> {
        let client = client_with("ev1_a", DeviceProfile::default(), reported)?;
        assert_eq!(
            job_matches_client(&job, &client, &tagged_a(tags)?),
            eligible
        );
        Ok(())
    }

    /// A tag counts only for the client that carries it.
    #[test]
    fn another_clients_tag_matches_nothing() -> anyhow::Result<()> {
        let client = client_with("ev1_a", DeviceProfile::default(), &[])?;
        let tags = FleetTags::new([(ClientId::try_new("ev1_b")?, Tag::try_new("lab")?)]);
        assert!(!job_matches_client(
            &json!({"requires": ["tag:lab"]}),
            &client,
            &tags
        ));
        Ok(())
    }

    // ── client versions ──────────────────────────────────────────────────────

    /// The builds a phone reporting three versions may run each job on; a
    /// version group composes like any `any_of` group.
    #[rstest]
    #[case::one_version(json!({"requires": ["os:ios"], "any_of": [["client_version:2.0"]]}), &[], &["2.0"])]
    #[case::a_set(json!({"requires": ["os:ios"], "any_of": [["client_version:1.0", "client_version:3.0", "client_version:9.0"]]}), &[], &["1.0", "3.0"])]
    #[case::no_version_clause(json!({"requires": ["os:ios"]}), &[], &["1.0", "2.0", "3.0"])]
    #[case::tag_and_version(json!({"requires": ["os:ios", "tag:lab-phone"], "any_of": [["client_version:2.0"]]}), &["lab-phone"], &["2.0"])]
    #[case::version_without_the_tag(json!({"requires": ["os:ios", "tag:lab-phone"], "any_of": [["client_version:2.0"]]}), &[], &[])]
    // The roster reads no capability, so it admits every reported version.
    #[case::roster_and_version(json!({"clients": ["ev1_a"], "requires": ["os:ios"], "any_of": [["client_version:9.0"]]}), &[], &["1.0", "2.0", "3.0"])]
    // A negative still beats the roster, one version at a time.
    #[case::roster_and_an_excluded_version(json!({"clients": ["ev1_a"], "excludes": ["client_version:1.0"]}), &[], &["2.0", "3.0"])]
    #[case::excludes(json!({"requires": ["os:ios"], "excludes": ["client_version:1.0"]}), &[], &["2.0", "3.0"])]
    #[case::not_all_of(json!({"requires": ["os:ios"], "not_all_of": [["client_version:2.0", "runtime:llama_cpp"]]}), &[], &["1.0", "3.0"])]
    // One version at a time, so a version in `requires` admits that one alone.
    #[case::required_version(json!({"requires": ["os:ios", "client_version:3.0"]}), &[], &["3.0"])]
    #[case::job_admits_the_client_at_no_version(json!({"requires": ["os:android"], "any_of": [["client_version:1.0"]]}), &[], &[])]
    #[case::malformed(json!({"requires": "os:ios"}), &[], &[])]
    fn client_versions_name_each_admitted_build(
        #[case] job: Value,
        #[case] tags: &[&str],
        #[case] expected: &[&str],
    ) -> anyhow::Result<()> {
        let reported = [
            "runtime:llama_cpp",
            "client_version:3.0",
            "client_version:1.0",
            "client_version:2.0",
        ];
        let tags = tagged_a(tags)?;
        assert_eq!(
            admitted_client_versions(&job, &phone(&reported)?, &tags),
            expected
        );
        // The rule as stated: a version is admitted exactly when the job matches
        // a client whose only version flag is that one.
        ["1.0", "2.0", "3.0"]
            .into_iter()
            .try_for_each(|version| -> anyhow::Result<()> {
                let only = format!("client_version:{version}");
                let client = phone(&["runtime:llama_cpp", &only])?;
                assert_eq!(
                    job_matches_client(&job, &client, &tags),
                    expected.contains(&version),
                    "{version}"
                );
                Ok(())
            })
    }

    /// A client that reports no version gets `[]`, whether or not the job names
    /// one, and `client_version:` names no build.
    #[rstest]
    #[case::versioned_job(json!({"requires": ["os:ios"], "any_of": [["client_version:1.0"]]}), &["runtime:llama_cpp"])]
    #[case::unversioned_job(json!({"requires": ["os:ios"]}), &["runtime:llama_cpp"])]
    #[case::empty_version(json!({"requires": ["os:ios"]}), &["client_version:"])]
    fn no_reported_version_admits_nothing(
        #[case] job: Value,
        #[case] reported: &[&str],
    ) -> anyhow::Result<()> {
        assert!(
            admitted_client_versions(&job, &phone(reported)?, &FleetTags::default()).is_empty()
        );
        Ok(())
    }

    /// An iOS client `ev1_a` reporting `capabilities`.
    fn phone(capabilities: &[&str]) -> anyhow::Result<Client> {
        client_with(
            "ev1_a",
            DeviceProfile {
                device_os_name: nz("iOS"),
                ..Default::default()
            },
            capabilities,
        )
    }

    /// A representative lab device: normalizes to
    /// `{os:ios, os_version:26.1, device:iphone17pro, chip:applea19pro,
    /// form_factor:phone, ram_bytes:8000000000}`, plus a reported
    /// `runtime:llama_cpp` capability.
    fn iphone() -> anyhow::Result<Client> {
        client_with(
            "ev1_a",
            DeviceProfile {
                device_name: nz("iPhone 17 Pro"),
                device_os_name: nz("iOS"),
                device_os_version: nz("26.1"),
                device_chip_model: nz("Apple A19 Pro"),
                device_form_factor: Some(DeviceFormFactor::Phone),
                device_ram_bytes: Some(8_000_000_000),
                ..Default::default()
            },
            &["runtime:llama_cpp"],
        )
    }

    // ── clients array ────────────────────────────────────────────────────────

    /// A roster names a client by its exact id, whatever its capabilities.
    #[rstest]
    #[case::listed(json!({"clients": ["ev1_a", "ev1_b"]}), true)]
    #[case::unlisted(json!({"clients": ["ev1_b"]}), false)]
    // No partial / prefix matching on client ids.
    #[case::prefix_only(json!({"clients": ["ev1_"]}), false)]
    #[case::listed_without_capabilities(json!({"clients": ["ev1_a"]}), true)]
    // …and even when it also fails the requires path.
    #[case::listed_failing_requires(json!({"clients": ["ev1_a"], "requires": ["os:android"]}), true)]
    fn clients_array_exact_membership(
        #[case] job: Value,
        #[case] eligible: bool,
    ) -> anyhow::Result<()> {
        let client = client_with("ev1_a", DeviceProfile::default(), &[])?;
        assert_eq!(matches_untagged(&job, &client), eligible);
        Ok(())
    }

    // ── capability containment ────────────────────────────────────────────────

    /// Eligibility of a job body against [`iphone`] (client id `ev1_a`).
    #[rstest]
    // A subset drawn from both normalized device flags and a reported flag.
    #[case::subset_mixed(json!({"requires": ["os:ios", "device:iphone17pro", "runtime:llama_cpp"]}), true)]
    #[case::subset_single_normalized(json!({"requires": ["chip:applea19pro"]}), true)]
    #[case::exact_ram(json!({"requires": ["ram_bytes:8000000000"]}), true)]
    // Any unmet flag fails the whole requirement (superset containment).
    #[case::missing_flag(json!({"requires": ["os:android"]}), false)]
    #[case::partial_match(json!({"requires": ["os:ios", "runtime:mlx"]}), false)]
    #[case::exact_ram_mismatch(json!({"requires": ["ram_bytes:16000000000"]}), false)]
    // Empty requires matches nobody (zero conditions would match everyone);
    // the explicit list still matches such a job.
    #[case::empty_requires(json!({"requires": []}), false)]
    #[case::empty_requires_but_listed(json!({"clients": ["ev1_a"], "requires": []}), true)]
    // Neither clients nor requires → nobody.
    #[case::absent(json!({}), false)]
    // Malformed requires fails closed: not an array, or a non-string element.
    #[case::malformed_string(json!({"requires": "os:ios"}), false)]
    #[case::malformed_object(json!({"requires": {}}), false)]
    #[case::malformed_null(json!({"requires": null}), false)]
    #[case::malformed_non_string_element(json!({"requires": ["os:ios", 42]}), false)]
    // any_of: each group must share at least one flag with the client.
    #[case::any_of_group_satisfied(json!({"requires": ["os:ios"], "any_of": [["device:iphone17pro", "device:iphone18"]]}), true)]
    #[case::any_of_group_unmet(json!({"requires": ["os:ios"], "any_of": [["device:iphone18", "device:iphone18pro"]]}), false)]
    #[case::any_of_every_group_must_intersect(json!({"requires": ["os:ios"], "any_of": [["device:iphone17pro"], ["runtime:mlx"]]}), false)]
    #[case::any_of_multiple_groups_satisfied(json!({"requires": ["os:ios"], "any_of": [["device:iphone17pro"], ["runtime:llama_cpp"]]}), true)]
    // An empty group list is the default — no constraint; an empty *group* is
    // an at-least-one-of over nothing, unsatisfiable.
    #[case::any_of_no_groups(json!({"requires": ["os:ios"], "any_of": []}), true)]
    #[case::any_of_empty_group(json!({"requires": ["os:ios"], "any_of": [[]]}), false)]
    // any_of only narrows: without a non-empty requires, the capability path
    // matches nobody, however satisfiable the groups.
    #[case::any_of_without_requires(json!({"any_of": [["device:iphone17pro"]]}), false)]
    #[case::any_of_with_empty_requires(json!({"requires": [], "any_of": [["device:iphone17pro"]]}), false)]
    // Malformed any_of fails closed: not an array, a group that is not an
    // array, a non-string member, or null (not an array either).
    #[case::any_of_malformed_string(json!({"requires": ["os:ios"], "any_of": "device:iphone17pro"}), false)]
    #[case::any_of_malformed_flat_group(json!({"requires": ["os:ios"], "any_of": ["device:iphone17pro"]}), false)]
    #[case::any_of_malformed_null_group(json!({"requires": ["os:ios"], "any_of": [null]}), false)]
    #[case::any_of_malformed_non_string_member(json!({"requires": ["os:ios"], "any_of": [["device:iphone17pro", 42]]}), false)]
    #[case::any_of_malformed_null(json!({"requires": ["os:ios"], "any_of": null}), false)]
    // An unmet positive clause never blocks a listed client: the roster is the
    // other half of the union. A *malformed* one does — every clause fails
    // closed over the whole rule, roster included.
    #[case::any_of_unmet_but_listed(json!({"clients": ["ev1_a"], "requires": ["os:ios"], "any_of": [[]]}), true)]
    #[case::any_of_malformed_blocks_listed(json!({"clients": ["ev1_a"], "any_of": "bogus"}), false)]
    #[case::requires_malformed_blocks_listed(json!({"clients": ["ev1_a"], "requires": "os:ios"}), false)]
    // ── exclusion clauses ───────────────────────────────────────────────────
    // exclude_clients names an id, and beats the roster: a denial overrides a
    // match.
    #[case::exclude_clients_beats_roster(json!({"clients": ["ev1_a"], "exclude_clients": ["ev1_a"]}), false)]
    #[case::exclude_clients_beats_capabilities(json!({"requires": ["os:ios"], "exclude_clients": ["ev1_a"]}), false)]
    #[case::exclude_clients_other_id(json!({"requires": ["os:ios"], "exclude_clients": ["ev1_b"]}), true)]
    // Ids are opaque, so membership is exact equality: an id prefix excludes
    // nobody, and the client stays eligible.
    #[case::exclude_clients_prefix(json!({"requires": ["os:ios"], "exclude_clients": ["ev1_"]}), true)]
    // excludes is a disjunction: any one listed flag the client carries denies it.
    #[case::excludes_single_flag(json!({"requires": ["os:ios"], "excludes": ["chip:applea19pro"]}), false)]
    #[case::excludes_one_of_several(json!({"requires": ["os:ios"], "excludes": ["chip:a15", "chip:applea19pro"]}), false)]
    #[case::excludes_no_flag_carried(json!({"requires": ["os:ios"], "excludes": ["chip:a15", "chip:a16"]}), true)]
    #[case::excludes_beats_roster(json!({"clients": ["ev1_a"], "excludes": ["os:ios"]}), false)]
    // not_all_of is a conjunction: only the whole group denies, so each flag
    // stays usable on its own.
    #[case::not_all_of_whole_group(json!({"requires": ["os:ios"], "not_all_of": [["runtime:llama_cpp", "chip:applea19pro"]]}), false)]
    #[case::not_all_of_partial_group(json!({"requires": ["os:ios"], "not_all_of": [["runtime:llama_cpp", "chip:a15"]]}), true)]
    #[case::not_all_of_second_group_matches(json!({"requires": ["os:ios"], "not_all_of": [["chip:a15"], ["os:ios"]]}), false)]
    #[case::not_all_of_beats_roster(json!({"clients": ["ev1_a"], "not_all_of": [["os:ios"]]}), false)]
    // Empty-list forms follow from the rule: an empty negative list excludes
    // nobody, while an empty not_all_of *group* is contained in every capability
    // set and so excludes everybody.
    #[case::empty_excludes(json!({"requires": ["os:ios"], "excludes": []}), true)]
    #[case::empty_exclude_clients(json!({"clients": ["ev1_a"], "exclude_clients": []}), true)]
    #[case::empty_not_all_of_group_list(json!({"requires": ["os:ios"], "not_all_of": []}), true)]
    #[case::empty_not_all_of_group(json!({"clients": ["ev1_a"], "not_all_of": [[]]}), false)]
    // Several clauses at once all apply — they compose by union, so a body that
    // carries an authored clause beside an injected one needs no precedence rule.
    #[case::composed_negatives_all_pass(json!({"requires": ["os:ios"], "exclude_clients": ["ev1_b"], "excludes": ["chip:a15"], "not_all_of": [["os:ios", "chip:a15"]]}), true)]
    #[case::composed_negatives_one_denies(json!({"requires": ["os:ios"], "exclude_clients": ["ev1_b"], "excludes": ["chip:a15"], "not_all_of": [["os:ios", "runtime:llama_cpp"]]}), false)]
    // A malformed negative excludes everybody: skipping it would widen
    // eligibility, the one direction the matcher must never take.
    #[case::malformed_excludes(json!({"requires": ["os:ios"], "excludes": "chip:a15"}), false)]
    #[case::malformed_exclude_clients(json!({"clients": ["ev1_a"], "exclude_clients": "ev1_b"}), false)]
    #[case::malformed_exclude_clients_element(json!({"clients": ["ev1_a"], "exclude_clients": ["ev1_b", 42]}), false)]
    #[case::malformed_not_all_of(json!({"requires": ["os:ios"], "not_all_of": ["chip:a15"]}), false)]
    #[case::malformed_not_all_of_member(json!({"requires": ["os:ios"], "not_all_of": [["chip:a15", 42]]}), false)]
    fn job_matches_iphone(
        #[case] job_body: serde_json::Value,
        #[case] expected: bool,
    ) -> anyhow::Result<()> {
        assert_eq!(matches_untagged(&job_body, &iphone()?), expected);
        Ok(())
    }

    /// A capability query uses the same body shape and predicate as a real job.
    #[rstest]
    #[case::subset(vec!["os:ios"], vec![], vec![], vec![], true)]
    #[case::superset(vec!["os:ios", "os:android"], vec![], vec![], vec![], false)]
    #[case::any_of_intersects(
        vec!["os:ios"],
        vec![vec!["device:android", "device:iphone17pro"]],
        vec![],
        vec![],
        true
    )]
    #[case::any_of_does_not_intersect(
        vec!["os:ios"],
        vec![vec!["device:android", "device:iphone18"]],
        vec![],
        vec![],
        false
    )]
    #[case::any_of_without_requires(vec![], vec![vec!["os:ios", "os:android"]], vec![], vec![], false)]
    #[case::excludes_carried_flag(vec!["os:ios"], vec![], vec!["chip:applea19pro"], vec![], false)]
    #[case::excludes_unheld_flag(vec!["os:ios"], vec![], vec!["chip:a15"], vec![], true)]
    #[case::not_all_of_whole_group(vec!["os:ios"], vec![], vec![], vec![vec!["os:ios", "runtime:llama_cpp"]], false)]
    #[case::not_all_of_partial_group(vec!["os:ios"], vec![], vec![], vec![vec!["os:ios", "runtime:mlx"]], true)]
    // A negatives-only query matches nobody, for the same reason `any_of` alone
    // does: the capability path needs a non-empty `requires`.
    #[case::excludes_without_requires(vec![], vec![], vec!["chip:a15"], vec![], false)]
    fn capability_query_matches_like_a_job(
        #[case] requires: Vec<&str>,
        #[case] any_of: Vec<Vec<&str>>,
        #[case] excludes: Vec<&str>,
        #[case] not_all_of: Vec<Vec<&str>>,
        #[case] expected: bool,
    ) -> anyhow::Result<()> {
        let flags = |f: Vec<&str>| f.into_iter().map(str::to_string).collect::<Vec<_>>();
        let groups = |g: Vec<Vec<&str>>| g.into_iter().map(flags).collect::<Vec<_>>();
        let body = NarrowingClauses {
            requires: &flags(requires),
            any_of: &groups(any_of),
            excludes: &flags(excludes),
            not_all_of: &groups(not_all_of),
            ..Default::default()
        }
        .body();
        assert_eq!(matches_untagged(&body, &iphone()?), expected);
        Ok(())
    }

    #[test]
    fn runtime_flags_match_as_whole_strings() -> anyhow::Result<()> {
        // Each flag is compared as a whole, opaque string: a versioned flag and
        // its unversioned prefix are two distinct, independent flags.
        let general = json!({"requires": ["runtime:llama_cpp"]});
        let pinned = json!({"requires": ["runtime:llama_cpp:b9999"]});

        // A client advertising only the general flag does not match a build-pinned
        // job; one advertising only the versioned flag does not match a general job.
        let general_only = client_with("ev1_a", DeviceProfile::default(), &["runtime:llama_cpp"])?;
        let versioned_only = client_with(
            "ev1_b",
            DeviceProfile::default(),
            &["runtime:llama_cpp:b9999"],
        )?;
        assert!(!matches_untagged(&pinned, &general_only));
        assert!(!matches_untagged(&general, &versioned_only));

        // The recommended pattern: a client reports *every level it supports*, so
        // a build-pinned client advertising both flags matches jobs at either
        // granularity.
        let both = client_with(
            "ev1_c",
            DeviceProfile::default(),
            &["runtime:llama_cpp", "runtime:llama_cpp:b9999"],
        )?;
        assert!(matches_untagged(&general, &both));
        assert!(matches_untagged(&pinned, &both));
        Ok(())
    }

    // ── roster classification ──────────────────────────────────────────────────

    /// [`clients_only_roster`] answers only for a `clients`-only job; an empty
    /// `requires` still counts, and a roster emptied by `exclude_clients` is
    /// `None`.
    #[rstest]
    #[case::clients_only(json!({"clients": ["ev1_a", "ev1_b"]}), Some(vec!["ev1_a", "ev1_b"]))]
    #[case::clients_with_requires(json!({"clients": ["ev1_a"], "requires": ["os:ios"]}), None)]
    #[case::clients_with_empty_requires(json!({"clients": ["ev1_a"], "requires": []}), Some(vec!["ev1_a"]))]
    #[case::clients_with_any_of(json!({"clients": ["ev1_a"], "any_of": [["os:ios"]]}), Some(vec!["ev1_a"]))]
    #[case::requires_only(json!({"requires": ["os:ios"]}), None)]
    #[case::empty(json!({}), None)]
    #[case::excluded_member_subtracted(
        json!({"clients": ["ev1_a", "ev1_b"], "exclude_clients": ["ev1_b"]}),
        Some(vec!["ev1_a"])
    )]
    #[case::roster_excluded_away(json!({"clients": ["ev1_a"], "exclude_clients": ["ev1_a"]}), None)]
    // Capability exclusions are not subtracted: they read a profile a client can
    // change back, so they never close a roster.
    #[case::capability_exclusions_ignored(
        json!({"clients": ["ev1_a"], "excludes": ["os:ios"], "not_all_of": [["os:ios"]]}),
        Some(vec!["ev1_a"])
    )]
    fn clients_only_roster_classification(
        #[case] job_body: serde_json::Value,
        #[case] expected: Option<Vec<&str>>,
    ) {
        assert_eq!(clients_only_roster(&job_body), expected);
    }

    // ── malformed-clause classification ────────────────────────────────────

    /// Shape errors only: an absent clause and an empty `any_of` group are legal.
    #[rstest]
    #[case::malformed_requires(json!({"requires": "os:ios"}), true)]
    #[case::malformed_requires_element(json!({"requires": ["os:ios", 42]}), true)]
    #[case::malformed_any_of(json!({"requires": ["os:ios"], "any_of": "bogus"}), true)]
    #[case::malformed_any_of_group(json!({"requires": ["os:ios"], "any_of": [null]}), true)]
    #[case::malformed_excludes(json!({"requires": ["os:ios"], "excludes": "chip:a15"}), true)]
    #[case::malformed_excludes_element(json!({"requires": ["os:ios"], "excludes": [42]}), true)]
    #[case::malformed_exclude_clients(json!({"clients": ["ev1_a"], "exclude_clients": {}}), true)]
    #[case::malformed_not_all_of(json!({"requires": ["os:ios"], "not_all_of": "chip:a15"}), true)]
    #[case::malformed_not_all_of_group(json!({"requires": ["os:ios"], "not_all_of": ["chip:a15"]}), true)]
    #[case::clients_only_absent_requires(json!({"clients": ["ev1_a"]}), false)]
    #[case::well_formed(json!({"requires": ["os:ios"], "any_of": [["device:iphone17pro"]]}), false)]
    #[case::well_formed_negatives(json!({"requires": ["os:ios"], "excludes": ["chip:a15"], "exclude_clients": ["ev1_b"], "not_all_of": [["chip:a15", "os:ios"]]}), false)]
    #[case::empty_group_not_malformed(json!({"requires": ["os:ios"], "any_of": [[]]}), false)]
    #[case::empty_not_all_of_group_not_malformed(json!({"requires": ["os:ios"], "not_all_of": [[]]}), false)]
    fn malformed_clause_classification(
        #[case] job_body: serde_json::Value,
        #[case] expected: bool,
    ) {
        assert_eq!(capability_clauses_malformed(&job_body), expected);
    }

    // ── static unmatchability ──────────────────────────────────────────────

    /// [`unmatchable_reason`] proves from the body alone that no client can ever
    /// match. Each rule it states gets a case, alongside the bodies that match
    /// nobody in *today's* fleet and are therefore not unmatchable.
    #[rstest]
    // Both branches alive.
    #[case::plain_requires(json!({"requires": ["os:ios"]}), false)]
    #[case::plain_roster(json!({"clients": ["ev1_a"]}), false)]
    // Fleet-dependent, not unmatchable: a conforming client can register later.
    #[case::excluded_id_leaves_capability_path(json!({"requires": ["os:ios"], "exclude_clients": ["ev1_a"]}), false)]
    #[case::unheld_exclude_flag(json!({"requires": ["os:ios"], "excludes": ["chip:a15"]}), false)]
    #[case::partial_not_all_of_group(json!({"requires": ["os:ios"], "not_all_of": [["os:ios", "chip:a15"]]}), false)]
    // A live roster survives a flag exclusion: an id does not determine the
    // client's flags.
    #[case::roster_survives_flag_exclusion(json!({"clients": ["ev1_a"], "excludes": ["os:ios"]}), false)]
    // Roster dead and capability branch dead.
    #[case::roster_excluded_away(json!({"clients": ["ev1_a"], "exclude_clients": ["ev1_a"]}), true)]
    #[case::required_flag_excluded(json!({"requires": ["os:ios"], "excludes": ["os:ios"]}), true)]
    #[case::not_all_of_group_inside_requires(json!({"requires": ["os:ios", "chip:a15"], "not_all_of": [["os:ios", "chip:a15"]]}), true)]
    #[case::any_of_group_wholly_excluded(json!({"requires": ["os:ios"], "any_of": [["chip:a15", "chip:a16"]], "excludes": ["chip:a15", "chip:a16"]}), true)]
    // An empty `any_of` group is an at-least-one-of over nothing.
    #[case::empty_any_of_group(json!({"requires": ["os:ios"], "any_of": [[]]}), true)]
    // An empty `not_all_of` group is contained in every capability set, so it
    // kills the roster branch too — the one exclusion a live roster survives.
    #[case::empty_not_all_of_group(json!({"requires": ["os:ios"], "not_all_of": [[]]}), true)]
    #[case::empty_not_all_of_group_with_roster(json!({"clients": ["ev1_a"], "not_all_of": [[]]}), true)]
    // A live roster keeps such a body matchable — only the capability branch died.
    #[case::empty_any_of_group_with_roster(json!({"clients": ["ev1_a"], "requires": ["os:ios"], "any_of": [[]]}), false)]
    // A malformed body matches nobody too, but `capability_clauses_malformed`
    // owns that answer, so a shape fault is named as one.
    #[case::malformed_requires(json!({"clients": ["ev1_a"], "requires": "os:ios"}), false)]
    #[case::malformed_excludes(json!({"clients": ["ev1_a"], "excludes": "chip:a15"}), false)]
    fn static_unmatchability_classification(
        #[case] job_body: serde_json::Value,
        #[case] expected: bool,
    ) {
        assert_eq!(unmatchable_reason(&job_body).is_some(), expected);
    }

    /// The head of a two-branch reason names *why* the roster branch is dead. A
    /// `clients` value that names no id is a shape fault, so the reason must not
    /// send its author looking for an exclusion.
    #[rstest]
    #[case::scalar_clients(json!({"clients": "ev1_a"}), "the `clients` value is not an array of client ids")]
    #[case::non_string_clients(json!({"clients": [42]}), "the `clients` value is not an array of client ids")]
    #[case::null_clients(json!({"clients": null}), "the `clients` value is not an array of client ids")]
    #[case::wholly_excluded(json!({"clients": ["ev1_a"], "exclude_clients": ["ev1_a"]}), "the `clients` roster is empty or wholly excluded")]
    #[case::empty_clients(json!({"clients": []}), "the `clients` roster is empty or wholly excluded")]
    #[case::absent_clients(json!({"requires": []}), "the `clients` roster is empty or wholly excluded")]
    fn dead_roster_reason_names_the_fault(
        #[case] job_body: serde_json::Value,
        #[case] expected_head: &str,
    ) -> anyhow::Result<()> {
        let reason = unmatchable_reason(&job_body).context("body should be unmatchable")?;
        assert!(reason.starts_with(expected_head), "{reason}");
        Ok(())
    }

    // ── fleet narrowing ──────────────────────────────────────────────────────

    /// A fleet spanning the clauses the ladder tests: two iOS phones on
    /// different chips, one Android phone, and one client pending approval.
    fn narrowing_fleet() -> anyhow::Result<Vec<Client>> {
        let ios = |id: &str, chip: &str| {
            client_with(
                id,
                DeviceProfile {
                    device_os_name: nz("iOS"),
                    device_chip_model: nz(chip),
                    device_form_factor: Some(DeviceFormFactor::Phone),
                    ..Default::default()
                },
                &["runtime:llama_cpp"],
            )
        };
        let mut pending = ios("ev1_pending", "A17 Pro")?;
        pending.status = ClientStatus::Pending;
        Ok(vec![
            ios("ev1_new", "A17 Pro")?,
            ios("ev1_old", "A15")?,
            client_with(
                "ev1_droid",
                DeviceProfile {
                    device_os_name: nz("Android"),
                    ..Default::default()
                },
                &[],
            )?,
            pending,
        ])
    }

    /// The tags [`narrowing_fleet`] carries: `ev1_new` alone is tagged `lab`.
    fn narrowing_tags() -> anyhow::Result<FleetTags> {
        Ok(FleetTags::new([(
            ClientId::try_new("ev1_new")?,
            Tag::try_new("lab")?,
        )]))
    }

    /// The count falls to zero at the clause the operator has to fix. The fleet
    /// is [`narrowing_fleet`] minus its pending client.
    #[rstest]
    // `requires` alone: two iOS clients survive, and nothing blocks them.
    #[case::requires_survives(
        NarrowingClauses { requires: &["os:ios".into()], ..Default::default() },
        vec![("requires os:ios", 2)]
    )]
    // A `requires` no client meets empties the fleet at its own rung.
    #[case::requires_blocks(
        NarrowingClauses { requires: &["os:windows".into()], ..Default::default() },
        vec![("requires os:windows", 0)]
    )]
    // `excludes` drops the rung below `requires`.
    #[case::excludes_narrows(
        NarrowingClauses {
            requires: &["os:ios".into()],
            excludes: &["chip:a15".into()],
            ..Default::default()
        },
        vec![("requires os:ios", 2), ("excludes chip:a15", 1)]
    )]
    #[case::excludes_blocks(
        NarrowingClauses {
            requires: &["os:ios".into()],
            excludes: &["os:ios".into()],
            ..Default::default()
        },
        vec![("requires os:ios", 2), ("excludes os:ios", 0)]
    )]
    // `any_of` is the rung that empties the fleet here, not `requires`.
    #[case::any_of_blocks(
        NarrowingClauses {
            requires: &["os:ios".into()],
            any_of: &[vec!["gpu:none".into()]],
            ..Default::default()
        },
        vec![("requires os:ios", 2), ("any_of [gpu:none]", 0)]
    )]
    // `not_all_of` excludes a whole combination, and reads last.
    #[case::not_all_of_blocks(
        NarrowingClauses {
            requires: &["os:ios".into()],
            not_all_of: &[vec!["os:ios".into(), "runtime:llama_cpp".into()]],
            ..Default::default()
        },
        vec![
            ("requires os:ios", 2),
            ("not_all_of [os:ios, runtime:llama_cpp]", 0),
        ]
    )]
    // `exclude_clients` leads the ladder, and the rungs below it carry the same
    // exclusion in their bodies — so the `requires` rung counts one client, not
    // the two the fleet holds.
    #[case::exclude_clients_leads(
        NarrowingClauses {
            exclude_clients: &["ev1_old".into()],
            requires: &["os:ios".into()],
            ..Default::default()
        },
        vec![("exclude_clients ev1_old", 2), ("requires os:ios", 1)]
    )]
    // Every clause at once, in the documented rung order.
    #[case::full_ladder(
        NarrowingClauses {
            exclude_clients: &["ev1_droid".into()],
            requires: &["os:ios".into()],
            excludes: &["chip:a15".into()],
            any_of: &[vec!["runtime:llama_cpp".into(), "runtime:mlx".into()]],
            not_all_of: &[vec!["chip:a17pro".into()]],
            ..Default::default()
        },
        vec![
            ("exclude_clients ev1_droid", 2),
            ("requires os:ios", 2),
            ("excludes chip:a15", 1),
            ("any_of [runtime:llama_cpp, runtime:mlx]", 1),
            ("not_all_of [chip:a17pro]", 0),
        ]
    )]
    // A tag is a flag of the client that carries it, on any rung.
    #[case::requires_a_tag(
        NarrowingClauses { requires: &["tag:lab".into()], ..Default::default() },
        vec![("requires tag:lab", 1)]
    )]
    #[case::excludes_a_tag(
        NarrowingClauses {
            requires: &["os:ios".into()],
            excludes: &["tag:lab".into()],
            ..Default::default()
        },
        vec![("requires os:ios", 2), ("excludes tag:lab", 1)]
    )]
    // A job with no `requires` matches through its roster alone, so the ladder is
    // empty rather than a column of zeroes blaming the last clause.
    #[case::declines_without_requires(
        NarrowingClauses { excludes: &["chip:a15".into()], ..Default::default() },
        vec![]
    )]
    fn narrow_fleet_names_the_blocking_clause(
        #[case] clauses: NarrowingClauses<'_>,
        #[case] expected: Vec<(&str, usize)>,
    ) -> anyhow::Result<()> {
        let fleet = narrowing_fleet()?;
        let approved: Vec<&Client> = fleet
            .iter()
            .filter(|c| c.status == ClientStatus::Approved)
            .collect();
        let rungs = narrow_fleet(clauses, &approved, &narrowing_tags()?);
        let ladder: Vec<(&str, usize)> = rungs
            .iter()
            .map(|rung| (rung.clause.as_str(), rung.surviving_client_count))
            .collect();
        assert_eq!(ladder, expected);
        Ok(())
    }

    /// The rights of a fleet where every named client holds `perform_work`.
    /// Stated per case, so a case about an exclusion grants everyone and the
    /// count it asserts is the exclusion's doing alone.
    fn granting(ids: &[&str]) -> anyhow::Result<FleetRights> {
        let set = ClientPermissionSet::from_iter([ClientPermission::PerformWork]);
        Ok(FleetRights::new(
            ids.iter()
                .map(|id| anyhow::Ok((ClientId::try_new(*id)?, set.clone())))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ))
    }

    /// The matcher offers work only to an approved client holding
    /// `perform_work`; the ungranted approved one is what a registration leaves.
    #[rstest]
    #[case::approved_and_granted("ev1_new", &["ev1_new"], true)]
    #[case::approved_but_ungranted("ev1_new", &[], false)]
    #[case::granted_but_pending("ev1_pending", &["ev1_pending"], false)]
    #[case::neither("ev1_pending", &[], false)]
    fn only_an_approved_holder_of_perform_work_is_offered_work(
        #[case] client_id: &str,
        #[case] granted: &[&str],
        #[case] expected: bool,
    ) -> anyhow::Result<()> {
        let fleet = narrowing_fleet()?;
        let client = fleet
            .iter()
            .find(|c| c.client_id.as_str() == client_id)
            .ok_or_else(|| anyhow::anyhow!("{client_id} is not in the fixture fleet"))?;
        let rights = granting(granted)?;

        assert_eq!(rights.admits(client), expected);
        assert_eq!(
            rights.workers([client]).len(),
            usize::from(expected),
            "`workers` and `admits` disagree about {client_id}"
        );
        Ok(())
    }

    /// A set that holds another name grants nothing here: `submit_results` is
    /// what a registration writes, and it is not the right to be handed work.
    #[test]
    fn submit_results_alone_is_not_a_worker() -> anyhow::Result<()> {
        let fleet = narrowing_fleet()?;
        let rights = FleetRights::new([(
            ClientId::try_new("ev1_new")?,
            ClientPermissionSet::from_iter([ClientPermission::SubmitResults]),
        )]);
        assert!(rights.workers(&fleet).is_empty());
        Ok(())
    }

    /// The roster standing reports each step of the roster's journey. The count
    /// that drops names the fix: register the id, approve the client, grant it
    /// `perform_work`, or drop the exclusion that denies it.
    #[rstest]
    // Two listed ids: one approved, one still pending.
    #[case::mixed_standing(
        &["ev1_new", "ev1_pending"],
        &["ev1_new", "ev1_pending"],
        NarrowingClauses::default(),
        RosterStanding { listed: 2, registered: 2, approved: 1, working: 1, eligible: 1 }
    )]
    // An id nobody has registered.
    #[case::unregistered(
        &["ev1_ghost"],
        &["ev1_ghost"],
        NarrowingClauses::default(),
        RosterStanding { listed: 1, registered: 0, approved: 0, working: 0, eligible: 0 }
    )]
    // The standing of a machine that just registered: approved, granted
    // nothing. The drop is at `working`, and the fix is a grant — not a clause,
    // and not an approval it already has.
    #[case::approved_but_ungranted(
        &["ev1_new"],
        &[],
        NarrowingClauses::default(),
        RosterStanding { listed: 1, registered: 1, approved: 1, working: 0, eligible: 0 }
    )]
    // A flag exclusion denies a listed, approved client — the roster is not a
    // way past the negatives.
    #[case::excluded_by_flag(
        &["ev1_new"],
        &["ev1_new"],
        NarrowingClauses { excludes: &["os:ios".into()], ..Default::default() },
        RosterStanding { listed: 1, registered: 1, approved: 1, working: 1, eligible: 0 }
    )]
    // An id exclusion denies it too.
    #[case::excluded_by_id(
        &["ev1_new"],
        &["ev1_new"],
        NarrowingClauses { exclude_clients: &["ev1_new".into()], ..Default::default() },
        RosterStanding { listed: 1, registered: 1, approved: 1, working: 1, eligible: 0 }
    )]
    // So does a tag exclusion: `ev1_new` carries `lab`.
    #[case::excluded_by_tag(
        &["ev1_new"],
        &["ev1_new"],
        NarrowingClauses { excludes: &["tag:lab".into()], ..Default::default() },
        RosterStanding { listed: 1, registered: 1, approved: 1, working: 1, eligible: 0 }
    )]
    // A repeated id names one client, so it counts once.
    #[case::duplicate_id(
        &["ev1_new", "ev1_new"],
        &["ev1_new"],
        NarrowingClauses::default(),
        RosterStanding { listed: 1, registered: 1, approved: 1, working: 1, eligible: 1 }
    )]
    fn roster_standing_reports_each_step(
        #[case] roster: &[&str],
        #[case] granted: &[&str],
        #[case] clauses: NarrowingClauses<'_>,
        #[case] expected: RosterStanding,
    ) -> anyhow::Result<()> {
        let fleet = narrowing_fleet()?;
        let registered: Vec<&Client> = fleet.iter().collect();
        let roster: Vec<String> = roster.iter().map(|id| (*id).to_string()).collect();
        // The roster is the group's `clients` clause, which is where the one
        // production caller reads it from too.
        let clauses = NarrowingClauses {
            clients: &roster,
            ..clauses
        };
        assert_eq!(
            roster_standing(
                clauses,
                &registered,
                &granting(granted)?,
                &narrowing_tags()?
            ),
            expected
        );
        Ok(())
    }

    const S26: &[&str] = &[
        "device:sm-s948u1",
        "chip:sm8850",
        "os:android",
        "os_version:16",
        "form_factor:phone",
    ];

    /// The first cases are edits filed in production.
    #[rstest]
    #[case::client_version_to_a_lab_tag(
        json!({"requires": ["client_version:2026.10.05-438185c", "device:sm-s948u1"]}),
        json!({"requires": ["device:sm-s948u1", "tag:lab-east"]}),
        true
    )]
    #[case::a_chip_the_device_has(
        json!({"requires": ["client_version:2026.10.05-438185c", "device:sm-s948u1"]}),
        json!({"requires": ["chip:sm8850", "device:sm-s948u1", "tag:lab-east"]}),
        true
    )]
    #[case::an_any_of_of_client_versions_dropped(
        json!({"requires": ["device:sm-s948u1"], "any_of": [["client_version:a", "client_version:b"]]}),
        json!({"requires": ["device:sm-s948u1", "tag:lab-east"]}),
        true
    )]
    #[case::a_roster_and_its_exclusions(
        json!({"requires": ["device:sm-s948u1"], "clients": ["ev1_a"]}),
        json!({"requires": ["device:sm-s948u1"], "exclude_clients": ["ev1_a"]}),
        true
    )]
    #[case::another_device(
        json!({"requires": ["device:sm-s948u1"]}),
        json!({"requires": ["device:pixel10", "tag:lab-east"]}),
        false
    )]
    #[case::the_chip_now_excluded(
        json!({"requires": ["device:sm-s948u1"]}),
        json!({"requires": ["device:sm-s948u1"], "excludes": ["chip:sm8850"]}),
        false
    )]
    // iOS records a phone's own name (`bos-17pro-1`), so a flag the record
    // lacks proves nothing against an exclusion.
    #[case::any_new_exclusion(
        json!({"requires": ["os:android"]}),
        json!({"requires": ["os:android"], "excludes": ["device:pixel10"]}),
        false
    )]
    #[case::an_exclusion_dropped(
        json!({"requires": ["os:android"], "excludes": ["chip:a15"], "not_all_of": [["chip:a16", "os:ios"]]}),
        json!({"requires": ["os:android", "chip:sm8850"]}),
        true
    )]
    #[case::a_device_group_it_is_in(
        json!({"requires": ["os:android"]}),
        json!({"requires": ["os:android"], "any_of": [["device:sm-s948u1", "device:pixel10"]]}),
        true
    )]
    #[case::a_flag_it_was_required_to_hold(
        json!({"requires": ["device:sm-s948u1", "runtime:llama_cpp"]}),
        json!({"requires": ["device:sm-s948u1", "runtime:llama_cpp", "chip:sm8850"]}),
        true
    )]
    #[case::a_flag_no_one_knows_it_held(
        json!({"requires": ["device:sm-s948u1"]}),
        json!({"requires": ["device:sm-s948u1", "runtime:llama_cpp"]}),
        false
    )]
    #[case::a_flag_held_through_the_roster_only(
        json!({"clients": ["ev1_a"], "requires": ["runtime:llama_cpp"]}),
        json!({"requires": ["runtime:llama_cpp", "device:sm-s948u1"]}),
        false
    )]
    #[case::a_malformed_clause(
        json!({"requires": ["device:sm-s948u1"]}),
        json!({"requires": "device:sm-s948u1"}),
        false
    )]
    // A mixed group keeps its routing flags: dropping the tag widens the
    // exclusion to every S26, or narrows the group to a chip this one lacks.
    #[case::a_mixed_exclusion_widened(
        json!({"requires": ["os:android"], "not_all_of": [["tag:lab-east", "chip:sm8850"]]}),
        json!({"requires": ["os:android"], "not_all_of": [["chip:sm8850"]]}),
        false
    )]
    #[case::a_mixed_group_narrowed(
        json!({"requires": ["os:android"], "any_of": [["tag:lab-east", "chip:tensorg5"]]}),
        json!({"requires": ["os:android"], "any_of": [["chip:tensorg5"]]}),
        false
    )]
    #[case::a_mixed_group_unchanged(
        json!({"requires": ["os:android", "client_version:a"], "any_of": [["tag:lab-east", "chip:tensorg5"]]}),
        json!({"requires": ["os:android"], "any_of": [["tag:lab-east", "chip:tensorg5"]]}),
        true
    )]
    // The roster ran this S26 on a Pixel job; dropping the roster leaves the
    // clauses as they were, but the run never met them.
    #[case::a_roster_run_under_unchanged_clauses(
        json!({"clients": ["ev1_a"], "requires": ["device:pixel10"]}),
        json!({"requires": ["device:pixel10"]}),
        false
    )]
    // A record with no `client_version` proves no build.
    #[case::a_required_pin_of_unknown_build(
        json!({"requires": ["os:android"]}),
        json!({"requires": ["os:android"], "any_of": [["client_version:a"]], "required_flags": ["client_version:a"]}),
        false
    )]
    fn a_finished_run_answers_what_it_still_meets(
        #[case] before: Value,
        #[case] after: Value,
        #[case] answers: bool,
    ) {
        let device: BTreeSet<String> = S26.iter().map(|flag| (*flag).to_string()).collect();
        assert_eq!(run_answers(&before, &after, &device), answers);
    }

    /// A required flag counts as measured: a build off the record's
    /// `client_version`, a tag only where the old body required it. The run is
    /// an S26 on build `a`.
    #[rstest]
    #[case::an_unrequired_pin_moved(
        json!({"requires": ["os:android", "client_version:a"]}),
        json!({"requires": ["os:android", "client_version:b"]}),
        true
    )]
    #[case::a_required_pin_moved(
        json!({"requires": ["os:android", "client_version:a"]}),
        json!({"requires": ["os:android"], "any_of": [["client_version:b"]], "required_flags": ["client_version:b"]}),
        false
    )]
    #[case::a_required_pin_naming_its_build(
        json!({"requires": ["os:android"]}),
        json!({"requires": ["os:android", "tag:lab-east"], "any_of": [["client_version:a", "client_version:b"]], "required_flags": ["client_version:a", "client_version:b"]}),
        true
    )]
    #[case::a_pin_marked_required_later(
        json!({"requires": ["os:android"], "any_of": [["client_version:b"]]}),
        json!({"requires": ["os:android"], "any_of": [["client_version:b"]], "required_flags": ["client_version:b"]}),
        false
    )]
    #[case::a_required_pin_dropped(
        json!({"requires": ["os:android"], "any_of": [["client_version:b"]], "required_flags": ["client_version:b"]}),
        json!({"requires": ["os:android"]}),
        true
    )]
    #[case::a_required_tag_it_held(
        json!({"requires": ["os:android", "tag:lab-east"]}),
        json!({"requires": ["os:android", "tag:lab-east"], "required_flags": ["tag:lab-east"]}),
        true
    )]
    #[case::a_new_required_tag(
        json!({"requires": ["os:android"]}),
        json!({"requires": ["os:android", "tag:lab-east"], "required_flags": ["tag:lab-east"]}),
        false
    )]
    #[case::malformed_required_flags(
        json!({"requires": ["os:android"]}),
        json!({"requires": ["os:android"], "required_flags": "tag:lab-east"}),
        false
    )]
    fn a_required_flag_is_measured(
        #[case] before: Value,
        #[case] after: Value,
        #[case] answers: bool,
    ) {
        let run: BTreeSet<String> = S26
            .iter()
            .chain(&["client_version:a"])
            .map(|flag| (*flag).to_string())
            .collect();
        assert_eq!(run_answers(&before, &after, &run), answers);
    }

    /// A change of routing alone needs no record of the device: a server-written
    /// failure, or a run recorded before `device_*` existed, still answers.
    #[test]
    fn a_routing_change_answers_without_a_device() {
        let before =
            json!({"requires": ["os:ios", "client_version:a"], "any_of": [["device:iphone17pro"]]});
        let after =
            json!({"requires": ["tag:lab-east", "os:ios"], "any_of": [["device:iphone17pro"]]});
        assert!(run_answers(&before, &after, &BTreeSet::new()));
    }
}
