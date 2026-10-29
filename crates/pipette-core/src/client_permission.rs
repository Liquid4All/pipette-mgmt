//! What a **client** may do: stored members, the set and its two defaults.
//! Closed on input (a [`ClientPermission`]), open at rest (any
//! [`PermissionName`], sorted by [`StoredClientPermission`]).

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::LazyLock;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use strum::{AsRefStr, Display};

use pipette_plan_types::ClientPermission;

use pipette_plan_types::PermissionName;

/// One member of a client's stored set. An `Unknown` name (a user's included)
/// grants nothing and survives a rewrite: dropping it would tell an operator on
/// an older replica that a newer release's grant is gone.
#[derive(Debug, Clone, Display, AsRefStr)]
pub enum StoredClientPermission {
    #[strum(transparent)]
    Known(ClientPermission),
    #[strum(transparent)]
    Unknown(PermissionName),
}

impl StoredClientPermission {
    /// Sort a stored name into its reading, here and nowhere else: a member built
    /// in the wrong variant by hand would grant less than it names.
    pub fn classify(name: PermissionName) -> Self {
        match ClientPermission::from_str(name.as_str()) {
            Ok(permission) => Self::Known(permission),
            Err(_) => Self::Unknown(name),
        }
    }

    /// The name this member carries on the wire and in storage.
    pub fn wire_name(&self) -> &str {
        self.as_ref()
    }
}

// Identity and order are both the wire name, so a [`ClientPermissionSet`] holds
// one member per name, in the order the JSON array needs.
impl PartialEq for StoredClientPermission {
    fn eq(&self, other: &Self) -> bool {
        self.wire_name() == other.wire_name()
    }
}

impl Eq for StoredClientPermission {}

impl Ord for StoredClientPermission {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.wire_name().cmp(other.wire_name())
    }
}

impl PartialOrd for StoredClientPermission {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl From<ClientPermission> for StoredClientPermission {
    fn from(permission: ClientPermission) -> Self {
        Self::Known(permission)
    }
}

impl Serialize for StoredClientPermission {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.wire_name())
    }
}

impl<'de> Deserialize<'de> for StoredClientPermission {
    /// A name that is not a [`PermissionName`] fails the whole object. The name
    /// normalizes before the match: a raw `PERFORM_WORK` would read as an inert
    /// `Unknown` and grant `perform_work` after the next rewrite.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let name =
            PermissionName::try_new(String::deserialize(d)?).map_err(serde::de::Error::custom)?;
        Ok(Self::classify(name))
    }
}

/// A client's permission set: the names it holds, sorted and each held once.
/// One type for grants, the stored object, key templates and read surfaces.
///
/// Serializes as its JSON array; the stored `{"permissions": [...]}` envelope
/// belongs to the layout, as an older replica cannot read a bare array.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientPermissionSet(BTreeSet<StoredClientPermission>);

impl ClientPermissionSet {
    pub fn iter(&self) -> impl Iterator<Item = &StoredClientPermission> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set holds `member`. Identity is the wire name, so this
    /// matches a name however it was classified.
    pub fn contains(&self, member: &StoredClientPermission) -> bool {
        self.0.contains(member)
    }

    pub fn insert(&mut self, member: StoredClientPermission) -> bool {
        self.0.insert(member)
    }

    /// The set a read surface renders: as stored, since nothing in this
    /// vocabulary implies anything else.
    pub fn resolved(&self) -> ClientPermissionSet {
        self.clone()
    }

    /// The wire names this set holds, in order.
    pub fn names(&self) -> Vec<&str> {
        self.iter().map(StoredClientPermission::wire_name).collect()
    }

    /// Whether this set grants `permission`. The entry point every route check
    /// uses, through [`effective_holds`].
    pub fn holds(&self, permission: ClientPermission) -> bool {
        self.0.iter().any(
            |member| matches!(member, StoredClientPermission::Known(held) if *held == permission),
        )
    }

    /// Add each of `permissions`, reporting whether the set changed. An
    /// already-held name is skipped, so an `add` that grants nothing new writes
    /// nothing.
    pub fn insert_all(&mut self, permissions: impl IntoIterator<Item = ClientPermission>) -> bool {
        let before = self.0.len();
        self.0
            .extend(permissions.into_iter().map(StoredClientPermission::from));
        self.0.len() != before
    }

    /// Remove every member spelled like one of `names`, reporting whether the
    /// set changed. Takes names, not [`ClientPermission`]s: removal is how an
    /// operator clears an `Unknown` one.
    pub fn remove_all<S: AsRef<str>>(&mut self, names: impl IntoIterator<Item = S>) -> bool {
        let removing: BTreeSet<String> = names
            .into_iter()
            .map(|name| name.as_ref().to_string())
            .collect();
        let before = self.0.len();
        self.0
            .retain(|member| !removing.contains(member.wire_name()));
        self.0.len() != before
    }

    /// Replace the whole set, reporting whether the set changed. An `Unknown`
    /// name the object carried is dropped, because the operator states the
    /// whole set.
    pub fn replace(&mut self, permissions: impl IntoIterator<Item = ClientPermission>) -> bool {
        let replacement: BTreeSet<StoredClientPermission> =
            permissions.into_iter().map(Into::into).collect();
        let changed = replacement != self.0;
        self.0 = replacement;
        changed
    }
}

impl From<&ClientPermissionSet> for pipette_plan_types::ClientPermissionSet {
    /// The set's names as stored, a name this build does not know included.
    fn from(set: &ClientPermissionSet) -> Self {
        Self(set.names().into_iter().map(str::to_string).collect())
    }
}

impl FromIterator<StoredClientPermission> for ClientPermissionSet {
    fn from_iter<I: IntoIterator<Item = StoredClientPermission>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// Build a set from the closed vocabulary, which is how a caller states an
/// expected set: every member is [`StoredClientPermission::Known`], the only
/// variant a name from this binary can be.
impl FromIterator<ClientPermission> for ClientPermissionSet {
    fn from_iter<I: IntoIterator<Item = ClientPermission>>(iter: I) -> Self {
        Self(iter.into_iter().map(Into::into).collect())
    }
}

impl Extend<StoredClientPermission> for ClientPermissionSet {
    fn extend<I: IntoIterator<Item = StoredClientPermission>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

impl IntoIterator for ClientPermissionSet {
    type Item = StoredClientPermission;
    type IntoIter = std::collections::btree_set::IntoIter<StoredClientPermission>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a ClientPermissionSet {
    type Item = &'a StoredClientPermission;
    type IntoIter = std::collections::btree_set::Iter<'a, StoredClientPermission>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// What a client holds when it registers without a pre-auth key template: an
/// external device's owner benchmarks it and submits the result, nothing more.
///
/// **The only default there is**, written into the object once at
/// registration; a change here reaches new registrations only.
pub static REGISTRATION_DEFAULT: LazyLock<ClientPermissionSet> =
    LazyLock::new(|| ClientPermissionSet::from_iter([ClientPermission::SubmitResults]));

/// The set a read surface renders for a client: what its object holds, or
/// nothing at all when it holds none.
///
/// **An absent object is never filled in from a default.** Registration writes
/// the object, so its absence is the evidence nobody granted anything; an
/// operator grants such a record by hand (`clients permission add`).
pub fn effective(stored: Option<ClientPermissionSet>) -> ClientPermissionSet {
    stored.unwrap_or_default()
}

/// Whether a client whose stored object is `stored` may perform `permission`:
/// the authorization question every gated route asks. Takes the `Option` so no
/// caller picks the absent-object default itself; it agrees with [`effective`].
pub fn effective_holds(stored: Option<&ClientPermissionSet>, permission: ClientPermission) -> bool {
    // Over a borrow rather than `effective`'s clone: a batch asks this per item.
    stored.is_some_and(|set| set.holds(permission))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use strum::IntoEnumIterator;
    use strum::VariantNames as _;

    use super::*;

    /// Every member's wire form, in both directions. The names are the storage
    /// and wire contract, so a rename has to break here.
    #[rstest]
    #[case(ClientPermission::SubmitResults, "submit_results")]
    #[case(ClientPermission::PerformWork, "perform_work")]
    fn a_permission_round_trips_through_its_wire_name(
        #[case] permission: ClientPermission,
        #[case] name: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(permission.as_ref(), name);
        assert_eq!(serde_json::to_value(permission)?, serde_json::json!(name));
        assert_eq!(ClientPermission::from_str(name)?, permission);
        Ok(())
    }

    /// Each name belongs to one kind, and the two vocabularies partition the
    /// keyspace: eighteen names, two and sixteen, none spelled by both.
    #[test]
    fn the_keyspace_is_eighteen_names_partitioned_in_two() {
        use pipette_plan_types::UserPermission;

        assert_eq!(ClientPermission::VARIANTS.len(), 2);
        assert_eq!(UserPermission::VARIANTS.len(), 16);

        let names: std::collections::BTreeSet<&str> = ClientPermission::VARIANTS
            .iter()
            .chain(UserPermission::VARIANTS)
            .copied()
            .collect();
        assert_eq!(
            names.len(),
            18,
            "one keyspace: no name in both vocabularies"
        );
    }

    /// The vocabulary is two names and no more.
    #[test]
    fn the_vocabulary_holds_two_names() {
        assert_eq!(ClientPermission::iter().count(), 2);
        assert_eq!(ClientPermission::VARIANTS.len(), 2);
    }

    /// A name outside this vocabulary, a user's included, is refused; the wire
    /// and the CLI both parse through here.
    #[rstest]
    #[case::user_name("submit_plans")]
    #[case::user_any_name("read_any_clients")]
    #[case::unknown("rescore_plans")]
    #[case::typo("perform_wrok")]
    #[case::empty("")]
    #[case::wrong_case("PERFORM_WORK")]
    #[case::padded(" perform_work ")]
    fn a_name_outside_the_vocabulary_is_not_a_client_permission(#[case] name: &str) {
        assert!(
            ClientPermission::from_str(name).is_err(),
            "{name:?} must not parse as input"
        );
    }

    /// Nothing implies anything here, so a client's set grants exactly what it
    /// names.
    #[rstest]
    #[case(ClientPermission::PerformWork, ClientPermission::SubmitResults)]
    #[case(ClientPermission::SubmitResults, ClientPermission::PerformWork)]
    fn a_permission_implies_nothing(
        #[case] held: ClientPermission,
        #[case] other: ClientPermission,
    ) {
        let set = ClientPermissionSet::from_iter([held]);
        assert!(set.holds(held));
        assert!(!set.holds(other));
        assert_eq!(set.resolved().names(), [held.as_ref()]);
    }

    /// A misplaced user name grants nothing, and is rendered like any other
    /// name this vocabulary does not define: one an operator cannot see is one
    /// they cannot remove.
    #[test]
    fn a_user_name_in_a_client_object_grants_nothing() -> anyhow::Result<()> {
        let body = serde_json::json!(["perform_work", "submit_plans"]);
        let set: ClientPermissionSet = serde_json::from_value(body.clone())?;

        assert!(set.holds(ClientPermission::PerformWork));
        assert_eq!(
            set.resolved().names(),
            ["perform_work", "submit_plans"],
            "the misplaced name is rendered, not hidden"
        );
        assert_eq!(
            serde_json::to_value(&set)?,
            body,
            "the stored object is untouched by a read"
        );
        Ok(())
    }

    /// An unknown name is cleared by removal, which is why removal takes a name
    /// rather than a [`ClientPermission`].
    #[test]
    fn a_name_a_client_cannot_hold_can_still_be_removed() -> anyhow::Result<()> {
        let mut set: ClientPermissionSet = serde_json::from_value(serde_json::json!([
            "perform_work",
            "submit_plans",
            "rescore_plans"
        ]))?;

        assert!(set.remove_all(["submit_plans", "rescore_plans"]));
        assert_eq!(
            serde_json::to_value(&set)?,
            serde_json::json!(["perform_work"])
        );
        Ok(())
    }

    /// A client with no object holds nothing: it is not filled in from a
    /// default.
    #[test]
    fn an_absent_object_grants_nothing() {
        assert!(effective(None).is_empty(), "an absent object holds nothing");
        ClientPermission::iter()
            .for_each(|permission| assert!(!effective_holds(None, permission), "{permission}"));
    }

    /// What a route authorizes and what a read surface renders are the same
    /// answer over the same object, for every name and stored reading.
    #[rstest]
    #[case::absent(None)]
    #[case::cleared(Some(&[][..]))]
    #[case::granted(Some(&["perform_work"][..]))]
    #[case::both(Some(&["perform_work", "submit_results"][..]))]
    #[case::misplaced(Some(&["submit_plans"][..]))]
    #[case::unknown(Some(&["operate_centrifuge"][..]))]
    fn a_route_authorizes_exactly_what_a_read_surface_renders(
        #[case] stored: Option<&[&str]>,
    ) -> anyhow::Result<()> {
        let stored: Option<ClientPermissionSet> = stored
            .map(|names| serde_json::from_value(serde_json::json!(names)))
            .transpose()?;
        let rendered = effective(stored.clone());
        ClientPermission::iter().try_for_each(|permission| {
            assert_eq!(
                effective_holds(stored.as_ref(), permission),
                rendered.contains(&StoredClientPermission::Known(permission)),
                "{permission} is authorized and rendered differently"
            );
            anyhow::Ok(())
        })
    }

    /// A name this binary does not know survives the round trip byte for byte
    /// and grants nothing — the property that lets an older binary read an
    /// object a newer one wrote.
    #[test]
    fn an_unknown_name_round_trips_and_grants_nothing() -> anyhow::Result<()> {
        let body = serde_json::json!(["perform_work", "rescore_plans"]);
        let set: ClientPermissionSet = serde_json::from_value(body.clone())?;

        assert_eq!(serde_json::to_value(&set)?, body);
        assert_eq!(
            serde_json::to_value(set.resolved())?,
            body,
            "an unknown name is rendered as well as preserved"
        );
        assert!(set.holds(ClientPermission::PerformWork));
        assert!(!set.holds(ClientPermission::SubmitResults));
        Ok(())
    }

    /// A stored name differing from its canonical spelling only in case or
    /// padding reads as the permission it names, never as an inert `Unknown`.
    #[rstest]
    #[case::upper("PERFORM_WORK")]
    #[case::mixed("Perform_Work")]
    #[case::padded(" perform_work ")]
    fn a_non_canonical_stored_name_reads_as_the_permission_it_names(
        #[case] stored: &str,
    ) -> anyhow::Result<()> {
        let set: ClientPermissionSet = serde_json::from_value(serde_json::json!([stored]))?;

        assert!(
            set.holds(ClientPermission::PerformWork),
            "{stored:?} must grant it"
        );
        assert_eq!(
            serde_json::to_value(&set)?,
            serde_json::json!(["perform_work"])
        );
        Ok(())
    }

    /// Read, write back, read again: the same rights and the same rendering,
    /// so an unrelated mutation never gains or loses a permission.
    #[rstest]
    #[case::canonical("perform_work")]
    #[case::upper("PERFORM_WORK")]
    #[case::misplaced("submit_plans")]
    #[case::unknown("rescore_plans")]
    fn a_stored_set_grants_the_same_rights_after_a_rewrite(
        #[case] stored: &str,
    ) -> anyhow::Result<()> {
        let first: ClientPermissionSet = serde_json::from_value(serde_json::json!([stored]))?;
        let rewritten: ClientPermissionSet = serde_json::from_slice(&serde_json::to_vec(&first)?)?;

        let held = |set: &ClientPermissionSet| -> Vec<ClientPermission> {
            ClientPermission::iter().filter(|p| set.holds(*p)).collect()
        };
        assert_eq!(
            held(&first),
            held(&rewritten),
            "{stored:?} changed on rewrite"
        );
        assert_eq!(
            first.resolved(),
            rewritten.resolved(),
            "{stored:?} renders differently after a rewrite"
        );
        Ok(())
    }

    /// A stored name outside `[a-z0-9_]{1,64}` fails the object rather than
    /// being carried, which is what keeps a stored set well-formed whatever
    /// wrote it.
    #[test]
    fn a_malformed_stored_name_fails_the_object() {
        let body = serde_json::json!(["../../escape"]);
        assert!(serde_json::from_value::<ClientPermissionSet>(body).is_err());
    }

    /// `insert_all` / `remove_all` / `replace` each report whether they changed
    /// anything, which is what lets the CLI skip a write that grants nothing.
    #[test]
    fn a_mutation_reports_whether_it_changed_the_set() {
        let mut set = ClientPermissionSet::from_iter([ClientPermission::SubmitResults]);

        assert!(set.insert_all([ClientPermission::PerformWork]));
        assert!(!set.insert_all([ClientPermission::PerformWork]));
        assert!(set.remove_all([ClientPermission::PerformWork]));
        assert!(!set.remove_all([ClientPermission::PerformWork]));
        assert!(!set.replace([ClientPermission::SubmitResults]));
        assert!(set.replace([]));
        assert!(set.is_empty());
    }

    /// The registration default is `submit_results` and nothing else, and a read
    /// never applies one ([`an_absent_object_grants_nothing`]).
    #[test]
    fn the_registration_default_grants_submit_results_alone() {
        assert_eq!(REGISTRATION_DEFAULT.len(), 1);
        assert!(REGISTRATION_DEFAULT.holds(ClientPermission::SubmitResults));
        assert!(!REGISTRATION_DEFAULT.holds(ClientPermission::PerformWork));
    }
}
