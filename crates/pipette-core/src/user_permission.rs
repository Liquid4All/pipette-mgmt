//! What a **user** may do: permission groups, stored members and the set.
//! Stored minimal, resolved at read time. Deliberately not one generic harness
//! with [`crate::client_permission`]: the lattices and defaults differ.

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::LazyLock;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use strum::{AsRefStr, Display, EnumIter};

use pipette_plan_types::{UserAccountType, UserPermission};

use pipette_plan_types::PermissionName;

use crate::user::User;

/// A named group of grants an operator can ask for by one word: a **spelling
/// of a set, not another grant**. It expands at grant time and nothing records
/// it, so widening a group never widens a set already written.
#[derive(Debug, Clone, Copy, Display, EnumIter)]
#[strum(serialize_all = "snake_case")]
pub enum UserPermissionGroup {
    /// A member of an organization: files and cancels the plans of its own
    /// campaigns, reads its organization's too, reads the fleet, and changes
    /// the org entries of the descriptor catalog. No roster name: who belongs
    /// where is an administrator's question.
    OrgMember,
    /// Every name, through the `_any_` verbs; still no work verb, which is a
    /// client's. `server_admin`, not `admin`: the scope is this server, not the
    /// organizations as tenants.
    ServerAdmin,
    /// Everything inside **one** organization, its own: a member's plans, and
    /// its people, devices, campaigns, benchmarks and vault. Only bare verbs,
    /// so it never reaches another organization; an account holding it in no
    /// organization administers nothing.
    OrgAdmin,
}

impl UserPermissionGroup {
    /// The names this group grants, as its documentation states them. Redundant
    /// names are fine: normalization decides how few hold the grant.
    pub fn permissions(self) -> &'static [UserPermission] {
        match self {
            Self::OrgMember => &[
                UserPermission::SubmitPlans,
                UserPermission::ReadPlans,
                UserPermission::CancelPlans,
                UserPermission::ReadClients,
                UserPermission::EditCatalog,
            ],
            // No `_any_` verb, which keeps one organization's admin out of
            // another's.
            Self::OrgAdmin => &[
                UserPermission::SubmitPlans,
                UserPermission::ReadPlans,
                UserPermission::CancelPlans,
                UserPermission::ReadOrgs,
                UserPermission::ManageOrgs,
                UserPermission::ReadClients,
                UserPermission::ManageClients,
                UserPermission::EditCatalog,
            ],
            Self::ServerAdmin => &[
                UserPermission::SubmitAnyPlans,
                UserPermission::ReadAnyPlans,
                UserPermission::CancelAnyPlans,
                UserPermission::ReadAnyClients,
                UserPermission::ManageAnyOrgs,
                UserPermission::ManageAnyClients,
                UserPermission::EditAnyCatalog,
            ],
        }
    }
}

/// One member of a user's stored set. An `Unknown` name (a client's included)
/// grants nothing and survives a rewrite: dropping it would tell an operator on
/// an older replica that a newer release's grant is gone.
#[derive(Debug, Clone, Display, AsRefStr)]
pub enum StoredUserPermission {
    #[strum(transparent)]
    Known(UserPermission),
    #[strum(transparent)]
    Unknown(PermissionName),
}

impl StoredUserPermission {
    /// Sort a stored name into its reading, here and nowhere else: a member built
    /// in the wrong variant by hand would grant less than it names.
    pub fn classify(name: PermissionName) -> Self {
        match UserPermission::from_str(name.as_str()) {
            Ok(permission) => Self::Known(permission),
            Err(_) => Self::Unknown(name),
        }
    }

    /// The name this member carries on the wire and in storage.
    pub fn wire_name(&self) -> &str {
        self.as_ref()
    }
}

// Identity and order are both the wire name, so a [`UserPermissionSet`] holds
// one member per name, in the order the JSON array needs.
impl PartialEq for StoredUserPermission {
    fn eq(&self, other: &Self) -> bool {
        self.wire_name() == other.wire_name()
    }
}

impl Eq for StoredUserPermission {}

impl Ord for StoredUserPermission {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.wire_name().cmp(other.wire_name())
    }
}

impl PartialOrd for StoredUserPermission {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl From<UserPermission> for StoredUserPermission {
    fn from(permission: UserPermission) -> Self {
        Self::Known(permission)
    }
}

impl Serialize for StoredUserPermission {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.wire_name())
    }
}

impl<'de> Deserialize<'de> for StoredUserPermission {
    /// A name that is not a [`PermissionName`] fails the whole object. The name
    /// normalizes before the match: a raw `SUBMIT_PLANS` would read as an inert
    /// `Unknown` and grant `submit_plans` after the next rewrite.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let name =
            PermissionName::try_new(String::deserialize(d)?).map_err(serde::de::Error::custom)?;
        Ok(Self::classify(name))
    }
}

/// A user's permission set: the names it holds, sorted and each held once.
///
/// Serializes as its JSON array; the stored `{"permissions": [...]}` envelope
/// belongs to the layout, as an older replica cannot read a bare array.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserPermissionSet(BTreeSet<StoredUserPermission>);

impl UserPermissionSet {
    pub fn iter(&self) -> impl Iterator<Item = &StoredUserPermission> {
        self.0.iter()
    }

    /// The known names: an unknown one grants nothing, so no check asks for it.
    pub fn known(&self) -> impl Iterator<Item = UserPermission> + '_ {
        self.0.iter().filter_map(|member| match member {
            StoredUserPermission::Known(permission) => Some(*permission),
            StoredUserPermission::Unknown(_) => None,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set holds `member`. Identity is the wire name, so this
    /// matches a name however it was classified.
    pub fn contains(&self, member: &StoredUserPermission) -> bool {
        self.0.contains(member)
    }

    pub fn insert(&mut self, member: StoredUserPermission) -> bool {
        self.0.insert(member)
    }

    /// The set a read surface renders: every member a user may hold plus what
    /// each implies. A read never rewrites a set.
    pub fn resolved(&self) -> UserPermissionSet {
        self.0
            .iter()
            .flat_map(|member| {
                let implied = match member {
                    StoredUserPermission::Known(permission) => permission.implied().collect(),
                    StoredUserPermission::Unknown(_) => Vec::new(),
                };
                std::iter::once(member.clone()).chain(implied.into_iter().map(Into::into))
            })
            .collect()
    }

    /// The same grant in the fewest names, the shape a set is **written** in; the
    /// dual of [`Self::resolved`], so it never narrows a grant. Without it,
    /// revoking `submit_any_plans` would leave a stored `submit_plans` granting
    /// what the operator meant to revoke. An `Unknown` name is never dropped.
    pub fn normalized(&self) -> UserPermissionSet {
        let implied: BTreeSet<UserPermission> =
            self.known().flat_map(UserPermission::implied).collect();
        self.0
            .iter()
            .filter(|member| match member {
                StoredUserPermission::Known(permission) => !implied.contains(permission),
                StoredUserPermission::Unknown(_) => true,
            })
            .cloned()
            .collect()
    }

    /// The wire names this set holds, in order.
    pub fn names(&self) -> Vec<&str> {
        self.iter().map(StoredUserPermission::wire_name).collect()
    }

    /// Whether this set grants `permission`, following the implication rules.
    /// The entry point every route check uses.
    pub fn holds(&self, permission: UserPermission) -> bool {
        self.0.iter().any(|member| match member {
            StoredUserPermission::Known(held) => {
                *held == permission || held.implied().any(|implied| implied == permission)
            }
            StoredUserPermission::Unknown(_) => false,
        })
    }

    /// Every name in `wanted` this set does **not** authorize handing out: a
    /// caller may grant only what it holds, read resolved. This is what stops an
    /// org admin granting itself an `_any_` verb.
    pub fn refused_grants(
        &self,
        wanted: impl IntoIterator<Item = UserPermission>,
    ) -> Vec<UserPermission> {
        let held = self.resolved();
        wanted
            .into_iter()
            .filter(|permission| !held.holds(*permission))
            .collect()
    }

    /// The server-wide names this set holds and `other` does not, both read
    /// resolved. An unknown name grants nothing here, so it does not count.
    pub fn server_wide_beyond(&self, other: &UserPermissionSet) -> Vec<UserPermission> {
        self.resolved()
            .known()
            .filter(|permission| permission.is_server_wide() && !other.holds(*permission))
            .collect()
    }

    /// The members `holder` may hold. A name it may not is dropped, so one
    /// granted before the rule grants nothing.
    pub fn held_by(&self, holder: Holder) -> UserPermissionSet {
        self.0
            .iter()
            .filter(|member| match member {
                StoredUserPermission::Known(permission) => holder.may_hold(*permission),
                StoredUserPermission::Unknown(_) => true,
            })
            .cloned()
            .collect()
    }

    /// Add each of `permissions`, reporting whether the set changed. An
    /// already-held name is skipped, so an `add` that grants nothing new writes
    /// nothing.
    pub fn insert_all(&mut self, permissions: impl IntoIterator<Item = UserPermission>) -> bool {
        let before = self.0.len();
        self.0
            .extend(permissions.into_iter().map(StoredUserPermission::from));
        self.0.len() != before
    }

    /// Remove every member spelled like one of `names`, reporting whether the
    /// set changed. Takes names, not [`UserPermission`]s: removal is how an
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
    pub fn replace(&mut self, permissions: impl IntoIterator<Item = UserPermission>) -> bool {
        let replacement: BTreeSet<StoredUserPermission> =
            permissions.into_iter().map(Into::into).collect();
        let changed = replacement != self.0;
        self.0 = replacement;
        changed
    }
}

impl From<&UserPermissionSet> for pipette_plan_types::UserPermissionSet {
    /// The set's names as stored, a name this build does not know included.
    fn from(set: &UserPermissionSet) -> Self {
        Self(set.names().into_iter().map(str::to_string).collect())
    }
}

impl FromIterator<StoredUserPermission> for UserPermissionSet {
    fn from_iter<I: IntoIterator<Item = StoredUserPermission>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// Build a set from the closed vocabulary, which is how a caller states an
/// expected set: every member is [`StoredUserPermission::Known`], the only
/// variant a name from this binary can be.
impl FromIterator<UserPermission> for UserPermissionSet {
    fn from_iter<I: IntoIterator<Item = UserPermission>>(iter: I) -> Self {
        Self(iter.into_iter().map(Into::into).collect())
    }
}

impl Extend<StoredUserPermission> for UserPermissionSet {
    fn extend<I: IntoIterator<Item = StoredUserPermission>>(&mut self, iter: I) {
        self.0.extend(iter);
    }
}

impl IntoIterator for UserPermissionSet {
    type Item = StoredUserPermission;
    type IntoIter = std::collections::btree_set::IntoIter<StoredUserPermission>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a UserPermissionSet {
    type Item = &'a StoredUserPermission;
    type IntoIter = std::collections::btree_set::Iter<'a, StoredUserPermission>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// What bounds the names an account may hold: its type, and whether it is a
/// campaign reader ([`User::campaign_scope`]), which holds `read_plans` alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holder {
    pub account_type: UserAccountType,
    pub campaign_reader: bool,
}

impl Holder {
    pub fn of(user: &User) -> Self {
        Self {
            account_type: user.account_type,
            campaign_reader: user.campaign_scope.is_some(),
        }
    }

    pub fn unscoped(account_type: UserAccountType) -> Self {
        Self {
            account_type,
            campaign_reader: false,
        }
    }

    /// A campaign reader is always a service account.
    pub fn campaign_reader() -> Self {
        Self {
            account_type: UserAccountType::Service,
            campaign_reader: true,
        }
    }

    /// The holder's name in a refusal.
    pub fn noun(self) -> &'static str {
        match (self.campaign_reader, self.account_type) {
            (true, _) => "a campaign reader",
            (false, UserAccountType::Service) => "a service account",
            (false, UserAccountType::Human) => "a person's account",
        }
    }

    pub fn may_hold(self, permission: UserPermission) -> bool {
        self.account_type.may_hold(permission)
            && (!self.campaign_reader || permission == UserPermission::ReadPlans)
    }
}

/// What a user holds with no permissions object: nothing. A user has no
/// registration default, so no object means granted nothing.
pub static ABSENT_DEFAULT: LazyLock<UserPermissionSet> = LazyLock::new(UserPermissionSet::default);

/// The set a read surface renders for a user: the closure of its stored object,
/// filtered by `holder` first so a service account's stray `manage_orgs`
/// leaves no implied `read_orgs` behind.
///
/// The absent default is per kind: resolving it the client way would hand every
/// new account `submit_results`, which a user cannot even be granted.
pub fn effective(stored: Option<UserPermissionSet>, holder: Holder) -> UserPermissionSet {
    stored
        .unwrap_or_else(|| ABSENT_DEFAULT.clone())
        .held_by(holder)
        .resolved()
}

/// The stored set resolved without the account-type filter: what rank and
/// ceiling checks compare, since retyping a service account as a person's
/// restores it all.
pub fn stored_closure(stored: Option<UserPermissionSet>) -> UserPermissionSet {
    stored.unwrap_or_else(|| ABSENT_DEFAULT.clone()).resolved()
}

/// The names in `wanted` `holder` may not be granted.
pub fn refused_for(
    holder: Holder,
    wanted: impl IntoIterator<Item = UserPermission>,
) -> Vec<UserPermission> {
    wanted
        .into_iter()
        .filter(|permission| !holder.may_hold(*permission))
        .collect()
}

/// The set an operator stated, in its stored shape: normalized
/// ([`UserPermissionSet::normalized`]). Every surface that assigns a user
/// permission goes through here, so "stored minimal, read resolved" holds.
pub fn normalize(permissions: &[UserPermission]) -> BTreeSet<UserPermission> {
    let normalized = UserPermissionSet::from_iter(permissions.iter().copied()).normalized();
    permissions
        .iter()
        .copied()
        .filter(|permission| normalized.contains(&StoredUserPermission::from(*permission)))
        .collect()
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
    #[case(UserPermission::SubmitPlans, "submit_plans")]
    #[case(UserPermission::SubmitAnyPlans, "submit_any_plans")]
    #[case(UserPermission::ReadPlans, "read_plans")]
    #[case(UserPermission::ReadAnyPlans, "read_any_plans")]
    #[case(UserPermission::CancelPlans, "cancel_plans")]
    #[case(UserPermission::CancelAnyPlans, "cancel_any_plans")]
    #[case(UserPermission::ReadClients, "read_clients")]
    #[case(UserPermission::ReadAnyClients, "read_any_clients")]
    #[case(UserPermission::ReadOrgs, "read_orgs")]
    #[case(UserPermission::ManageOrgs, "manage_orgs")]
    #[case(UserPermission::ManageClients, "manage_clients")]
    #[case(UserPermission::ManageAnyClients, "manage_any_clients")]
    #[case(UserPermission::ManageAnyOrgs, "manage_any_orgs")]
    #[case(UserPermission::ReadAnyOrgs, "read_any_orgs")]
    #[case(UserPermission::EditCatalog, "edit_catalog")]
    #[case(UserPermission::EditAnyCatalog, "edit_any_catalog")]
    fn a_permission_round_trips_through_its_wire_name(
        #[case] permission: UserPermission,
        #[case] name: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(permission.as_ref(), name);
        assert_eq!(serde_json::to_value(permission)?, serde_json::json!(name));
        assert_eq!(UserPermission::from_str(name)?, permission);
        Ok(())
    }

    /// A caller may grant only what it holds, and the check reads the
    /// **resolved** set so a wider name authorizes the narrower ones it
    /// implies. Without this an org admin could hand itself a `_any_` verb.
    #[test]
    fn a_grant_is_bounded_by_what_the_caller_holds() -> anyhow::Result<()> {
        let caller = UserPermissionSet::from_iter([
            UserPermission::ManageOrgs,
            UserPermission::ManageClients,
        ]);

        // Names it holds, and names those imply, pass.
        assert!(
            caller
                .refused_grants([UserPermission::ManageOrgs, UserPermission::ReadOrgs])
                .is_empty(),
            "a held name and one it implies are both grantable"
        );

        // A wider verb it does not hold is refused, and named.
        assert_eq!(
            caller.refused_grants([
                UserPermission::ManageOrgs,
                UserPermission::ManageAnyOrgs,
                UserPermission::ManageAnyClients,
            ]),
            vec![
                UserPermission::ManageAnyOrgs,
                UserPermission::ManageAnyClients
            ],
            "every refused name is reported, not just the first"
        );

        // A server admin may grant anything, because it holds everything.
        let server_admin =
            UserPermissionSet::from_iter(UserPermissionGroup::ServerAdmin.permissions().to_vec());
        assert!(
            server_admin
                .refused_grants(UserPermission::iter())
                .is_empty(),
            "the widest group may delegate the whole vocabulary"
        );

        // An empty set may grant nothing at all.
        assert_eq!(
            UserPermissionSet::default()
                .refused_grants([UserPermission::ReadOrgs])
                .len(),
            1
        );
        Ok(())
    }

    /// The server-wide names are exactly the `_any_` forms, the spelling the
    /// app's `outranking` reads.
    #[test]
    fn server_wide_names_are_the_any_forms() {
        UserPermission::iter().for_each(|permission| {
            assert_eq!(
                permission.is_server_wide(),
                permission.as_ref().contains("_any_"),
                "{permission}"
            );
        });
    }

    /// An account is beyond a caller by the server-wide names it holds and the
    /// caller does not, both read resolved; a plan name never counts.
    #[rstest]
    #[case::server_admin_over_org_admin(
        UserPermissionGroup::ServerAdmin.permissions(),
        UserPermissionGroup::OrgAdmin.permissions(),
        // Every `_any_` name, in wire-name order.
        &[
            UserPermission::CancelAnyPlans,
            UserPermission::EditAnyCatalog,
            UserPermission::ManageAnyClients,
            UserPermission::ManageAnyOrgs,
            UserPermission::ReadAnyClients,
            UserPermission::ReadAnyOrgs,
            UserPermission::ReadAnyPlans,
            UserPermission::SubmitAnyPlans,
        ]
    )]
    #[case::over_itself(
        UserPermissionGroup::ServerAdmin.permissions(),
        UserPermissionGroup::ServerAdmin.permissions(),
        &[]
    )]
    #[case::plan_names_are_not_server_wide(
        UserPermissionGroup::OrgMember.permissions(),
        UserPermissionGroup::OrgAdmin.permissions(),
        &[]
    )]
    // `manage_any_orgs` implies `read_any_orgs`, and the caller holds that one.
    #[case::implied_read_any_orgs_is_held(
        &[UserPermission::ManageAnyOrgs],
        &[UserPermission::ManageOrgs, UserPermission::ReadAnyOrgs],
        &[UserPermission::ManageAnyOrgs]
    )]
    #[case::held_by_both(
        &[UserPermission::ReadAnyPlans, UserPermission::SubmitPlans],
        &[UserPermission::ManageOrgs, UserPermission::ReadAnyPlans],
        &[]
    )]
    fn an_account_is_beyond_a_caller_by_the_server_wide_names_it_lacks(
        #[case] account: &[UserPermission],
        #[case] caller: &[UserPermission],
        #[case] expected: &[UserPermission],
    ) {
        let set = |names: &[UserPermission]| UserPermissionSet::from_iter(names.iter().copied());
        assert_eq!(set(account).server_wide_beyond(&set(caller)), expected);
    }

    /// An unknown name grants nothing, so it is beyond nobody, not even a
    /// caller holding nothing.
    #[test]
    fn an_unknown_server_wide_name_is_not_beyond_anyone() -> anyhow::Result<()> {
        let unknown: UserPermissionSet =
            serde_json::from_value(serde_json::json!(["rescore_any_plans"]))?;
        assert!(
            unknown
                .server_wide_beyond(&UserPermissionSet::default())
                .is_empty()
        );
        Ok(())
    }

    /// The vocabulary is sixteen names and no more. The count is the point: a
    /// hard number makes the next addition a decision.
    #[test]
    fn the_vocabulary_holds_sixteen_names() {
        assert_eq!(UserPermission::iter().count(), 16);
        assert_eq!(UserPermission::VARIANTS.len(), 16);
    }

    /// A name outside this vocabulary, a client's included, is refused; the
    /// wire and the CLI both parse through here.
    #[rstest]
    #[case::client_name("perform_work")]
    #[case::client_name_2("submit_results")]
    #[case::unknown("rescore_plans")]
    #[case::typo("submit_plnas")]
    #[case::empty("")]
    #[case::wrong_case("SUBMIT_PLANS")]
    #[case::padded(" submit_plans ")]
    fn a_name_outside_the_vocabulary_is_not_a_user_permission(#[case] name: &str) {
        assert!(
            UserPermission::from_str(name).is_err(),
            "{name:?} must not parse as input"
        );
    }

    /// The implication lattice. Each case names the permission and the set it
    /// resolves to, itself included.
    #[rstest]
    #[case::submit_any_plans(
        UserPermission::SubmitAnyPlans,
        &["read_plans", "submit_any_plans", "submit_plans"]
    )]
    #[case::submit_plans(UserPermission::SubmitPlans, &["read_plans", "submit_plans"])]
    #[case::read_any_plans(UserPermission::ReadAnyPlans, &["read_any_plans", "read_plans"])]
    #[case::cancel_any_plans(
        UserPermission::CancelAnyPlans,
        &["cancel_any_plans", "cancel_plans"]
    )]
    // The roster closure: the widest name reaches both the narrower write and
    // the wider read, so a holder can see everything it may change.
    #[case::manage_any_orgs(
        UserPermission::ManageAnyOrgs,
        &["manage_any_orgs", "manage_orgs", "read_any_orgs", "read_orgs"]
    )]
    #[case::manage_orgs(UserPermission::ManageOrgs, &["manage_orgs", "read_orgs"])]
    #[case::read_any_clients(
        UserPermission::ReadAnyClients,
        &["read_any_clients", "read_clients"]
    )]
    #[case::manage_any_clients(
        UserPermission::ManageAnyClients,
        &["manage_any_clients", "manage_clients", "read_clients"]
    )]
    // A cancellation is destructive and is granted on its own.
    #[case::cancel_plans(UserPermission::CancelPlans, &["cancel_plans"])]
    #[case::read_plans(UserPermission::ReadPlans, &["read_plans"])]
    #[case::read_clients(UserPermission::ReadClients, &["read_clients"])]
    #[case::edit_catalog(UserPermission::EditCatalog, &["edit_catalog"])]
    #[case::edit_any_catalog(UserPermission::EditAnyCatalog, &["edit_any_catalog", "edit_catalog"])]
    fn a_permission_resolves_to_its_closure(
        #[case] permission: UserPermission,
        #[case] expected: &[&str],
    ) -> anyhow::Result<()> {
        let set = UserPermissionSet::from_iter([permission]);
        let resolved: Vec<String> = set
            .resolved()
            .iter()
            .map(|member| member.wire_name().to_string())
            .collect();
        assert_eq!(resolved, expected);
        // `holds` reads the same lattice the closure does.
        expected.iter().try_for_each(|name| {
            assert!(
                set.holds(UserPermission::from_str(name)?),
                "{permission} must hold {name}"
            );
            anyhow::Ok(())
        })?;
        Ok(())
    }

    /// Each `_any_` scope grants its own scope. Stated over the vocabulary
    /// rather than case by case, so a new `_any_` member is covered the day it
    /// is added.
    #[test]
    fn every_any_scope_implies_its_own_scope() -> anyhow::Result<()> {
        UserPermission::iter()
            .filter_map(|permission| {
                let own = permission.as_ref().replace("_any_", "_");
                (own != permission.as_ref()).then_some((permission, own))
            })
            .try_for_each(|(permission, own)| {
                let own = UserPermission::from_str(&own)?;
                assert!(
                    permission.implied().any(|implied| implied == own),
                    "{permission} must imply {own}"
                );
                anyhow::Ok(())
            })?;
        Ok(())
    }

    /// A misplaced client name grants nothing and implies nothing, and is
    /// rendered beside the closure: one an operator cannot see is one they
    /// cannot remove.
    #[test]
    fn a_client_name_in_a_user_object_grants_nothing() -> anyhow::Result<()> {
        let body = serde_json::json!(["perform_work", "submit_plans"]);
        let set: UserPermissionSet = serde_json::from_value(body.clone())?;

        assert!(set.holds(UserPermission::SubmitPlans));
        assert!(
            set.holds(UserPermission::ReadPlans),
            "the closure expands from the name this vocabulary does define"
        );
        assert_eq!(
            set.resolved().names(),
            ["perform_work", "read_plans", "submit_plans"],
            "the misplaced name is rendered beside the closure, not hidden"
        );
        assert_eq!(
            serde_json::to_value(&set)?,
            body,
            "the stored object is untouched by a read"
        );
        Ok(())
    }

    /// An unknown name is cleared by removal, which is why removal takes a name
    /// rather than a [`UserPermission`].
    #[test]
    fn a_name_a_user_cannot_hold_can_still_be_removed() -> anyhow::Result<()> {
        let mut set: UserPermissionSet = serde_json::from_value(serde_json::json!([
            "submit_plans",
            "perform_work",
            "rescore_plans"
        ]))?;

        assert!(set.remove_all(["perform_work", "rescore_plans"]));
        assert_eq!(
            serde_json::to_value(&set)?,
            serde_json::json!(["submit_plans"])
        );
        Ok(())
    }

    /// A group grants what its name promises and nothing beyond the
    /// vocabulary, asserted resolved.
    #[rstest]
    #[case(
        UserPermissionGroup::OrgMember,
        &["cancel_plans", "edit_catalog", "read_clients", "read_plans", "submit_plans"]
    )]
    #[case(
        UserPermissionGroup::OrgAdmin,
        &["cancel_plans", "edit_catalog", "manage_clients", "manage_orgs", "read_clients", "read_orgs",
          "read_plans", "submit_plans"]
    )]
    #[case(
        UserPermissionGroup::ServerAdmin,
        &["cancel_any_plans", "cancel_plans", "edit_any_catalog", "edit_catalog", "manage_any_clients",
          "manage_any_orgs", "manage_clients", "manage_orgs", "read_any_clients", "read_any_orgs",
          "read_any_plans", "read_clients", "read_orgs", "read_plans", "submit_any_plans",
          "submit_plans"]
    )]
    fn a_group_resolves_to_the_names_it_promises(
        #[case] group: UserPermissionGroup,
        #[case] expected: &[&str],
    ) {
        let resolved = UserPermissionSet::from_iter(group.permissions().iter().copied()).resolved();
        assert_eq!(
            resolved.iter().map(|p| p.wire_name()).collect::<Vec<_>>(),
            expected
        );
    }

    /// Normalization drops what the lattice already grants, and only that: the
    /// resolved set is identical before and after, which is the property that
    /// makes writing the smaller object safe.
    #[rstest]
    #[case::redundant_bare_names(
        &["submit_any_plans", "submit_plans", "read_plans"],
        &["submit_any_plans"]
    )]
    #[case::already_minimal(
        &["cancel_any_plans", "read_any_clients"],
        &["cancel_any_plans", "read_any_clients"]
    )]
    #[case::whole_plan_vocabulary_is_three_names(
        &["cancel_any_plans", "cancel_plans", "read_any_plans", "read_plans",
          "submit_any_plans", "submit_plans"],
        &["cancel_any_plans", "read_any_plans", "submit_any_plans"]
    )]
    // The roster and fleet halves collapse the same way, each to its widest
    // name.
    #[case::whole_roster_vocabulary_is_one_name(
        &["manage_any_orgs", "manage_orgs", "read_any_orgs", "read_orgs"],
        &["manage_any_orgs"]
    )]
    #[case::whole_fleet_vocabulary_is_one_name(
        &["manage_any_clients", "manage_clients", "read_clients"],
        &["manage_any_clients"]
    )]
    #[case::catalog_vocabulary_is_one_name(
        &["edit_any_catalog", "edit_catalog"],
        &["edit_any_catalog"]
    )]
    // A retired name is an unknown one, so it is carried through and implies
    // nothing: `read_users` survives beside `read_orgs`.
    #[case::a_retired_name_implies_nothing(
        &["read_orgs", "read_users"],
        &["read_orgs", "read_users"]
    )]
    // Nothing implies an unknown name and it implies nothing, so it is carried
    // through whatever else the set holds.
    #[case::an_unknown_name_survives(
        &["submit_any_plans", "submit_plans", "zzz_from_a_newer_release"],
        &["submit_any_plans", "zzz_from_a_newer_release"]
    )]
    fn normalizing_drops_only_what_another_name_implies(
        #[case] stored: &[&str],
        #[case] expected: &[&str],
    ) -> anyhow::Result<()> {
        let set: UserPermissionSet = serde_json::from_value(serde_json::json!(stored))?;

        let normalized = set.normalized();

        assert_eq!(normalized.names(), expected);
        assert_eq!(
            normalized.resolved().names(),
            set.resolved().names(),
            "normalizing must not narrow what the set grants"
        );
        Ok(())
    }

    /// Every group normalizes to a set no member of which implies another, so
    /// no group can write a name the lattice already grants.
    #[test]
    fn a_group_normalizes_to_names_none_of_which_imply_another() {
        // Collected rather than asserted one at a time, so a group that names
        // two redundant permissions reports both.
        let redundant: Vec<String> = UserPermissionGroup::iter()
            .flat_map(|group| {
                let normalized =
                    UserPermissionSet::from_iter(group.permissions().iter().copied()).normalized();
                normalized
                    .iter()
                    .filter_map(|member| match member {
                        StoredUserPermission::Known(permission) => Some(*permission),
                        StoredUserPermission::Unknown(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .flat_map(move |permission| {
                        let normalized = normalized.clone();
                        permission
                            .implied()
                            .filter(move |implied| {
                                normalized.contains(&StoredUserPermission::from(*implied))
                            })
                            .map(move |implied| {
                                format!("{group} stores {implied}, which {permission} implies")
                            })
                    })
            })
            .collect();
        assert!(redundant.is_empty(), "{redundant:?}");
    }

    /// A user with no object holds nothing, the answer a client's absent object
    /// gives too: neither kind invents a grant for a principal that has none.
    #[test]
    fn an_absent_object_grants_nothing() {
        assert!(
            effective(None, Holder::unscoped(UserAccountType::Human)).is_empty(),
            "an absent object holds nothing"
        );
        assert!(ABSENT_DEFAULT.is_empty());
    }

    /// A service account holds the work names and none that administer or
    /// reach past its organization, nor what those imply.
    #[test]
    fn a_service_account_holds_no_admin_name() {
        let stored = UserPermissionSet::from_iter([
            UserPermission::SubmitPlans,
            UserPermission::ManageOrgs,
            UserPermission::ReadAnyClients,
            UserPermission::EditCatalog,
            UserPermission::EditAnyCatalog,
        ]);
        let service = effective(
            Some(stored.clone()),
            Holder::unscoped(UserAccountType::Service),
        );
        assert!(service.holds(UserPermission::SubmitPlans));
        assert!(service.holds(UserPermission::ReadPlans));
        assert!(service.holds(UserPermission::EditCatalog));
        assert!(!service.holds(UserPermission::EditAnyCatalog));
        let leaked: Vec<_> = [
            UserPermission::ManageOrgs,
            UserPermission::ReadOrgs,
            UserPermission::ReadAnyClients,
            UserPermission::ReadClients,
        ]
        .into_iter()
        .filter(|permission| service.holds(*permission))
        .collect();
        assert!(leaked.is_empty(), "{leaked:?}");
        let human = effective(Some(stored), Holder::unscoped(UserAccountType::Human));
        assert!(human.holds(UserPermission::ManageOrgs));
        assert!(human.holds(UserPermission::ReadAnyClients));
        assert_eq!(
            refused_for(
                Holder::unscoped(UserAccountType::Service),
                UserPermission::iter()
            ),
            UserPermission::iter()
                .filter(|p| p.is_admin())
                .collect::<Vec<_>>()
        );
        assert!(
            refused_for(
                Holder::unscoped(UserAccountType::Human),
                UserPermission::iter()
            )
            .is_empty()
        );
    }

    /// A campaign reader holds `read_plans` and nothing else, whatever it was
    /// granted before it was scoped, and nothing implied by a wider name.
    #[test]
    fn a_campaign_reader_holds_read_plans_alone() {
        let reader = Holder::campaign_reader();
        let stored = UserPermissionSet::from_iter([
            UserPermission::ReadPlans,
            UserPermission::SubmitPlans,
            UserPermission::ReadClients,
        ]);
        let held = effective(Some(stored), reader);
        assert_eq!(held.names(), vec!["read_plans"]);
        assert!(
            !effective(
                Some(UserPermissionSet::from_iter([UserPermission::ReadAnyPlans])),
                reader
            )
            .holds(UserPermission::ReadPlans),
            "read_any_plans is refused whole, not read down to read_plans"
        );
        assert_eq!(
            refused_for(reader, UserPermission::iter()),
            UserPermission::iter()
                .filter(|p| *p != UserPermission::ReadPlans)
                .collect::<Vec<_>>()
        );
    }

    /// A name this binary does not know survives the round trip byte for byte
    /// and grants nothing — the property that lets an older binary read an
    /// object a newer one wrote.
    #[test]
    fn an_unknown_name_round_trips_and_grants_nothing() -> anyhow::Result<()> {
        let body = serde_json::json!(["rescore_plans", "submit_plans"]);
        let set: UserPermissionSet = serde_json::from_value(body.clone())?;

        assert_eq!(serde_json::to_value(&set)?, body);
        assert_eq!(
            set.resolved().names(),
            ["read_plans", "rescore_plans", "submit_plans"],
            "an unknown name is rendered as well as preserved"
        );
        UserPermission::iter().for_each(|permission| {
            assert_eq!(
                set.holds(permission),
                matches!(
                    permission,
                    UserPermission::SubmitPlans | UserPermission::ReadPlans
                ),
                "an unknown name must not grant {permission}"
            );
        });
        Ok(())
    }

    /// A stored name differing from its canonical spelling only in case or
    /// padding reads as the permission it names, never as an inert `Unknown`.
    #[rstest]
    #[case::upper("SUBMIT_PLANS")]
    #[case::mixed("Submit_Plans")]
    #[case::padded(" submit_plans ")]
    fn a_non_canonical_stored_name_reads_as_the_permission_it_names(
        #[case] stored: &str,
    ) -> anyhow::Result<()> {
        let set: UserPermissionSet = serde_json::from_value(serde_json::json!([stored]))?;

        assert!(
            set.holds(UserPermission::SubmitPlans),
            "{stored:?} must grant it"
        );
        assert_eq!(
            serde_json::to_value(&set)?,
            serde_json::json!(["submit_plans"])
        );
        Ok(())
    }

    /// Read, write back, read again: the same rights and the same rendering,
    /// so an unrelated mutation never gains or loses a permission.
    #[rstest]
    #[case::canonical("submit_plans")]
    #[case::upper("SUBMIT_PLANS")]
    #[case::misplaced("perform_work")]
    #[case::unknown("rescore_plans")]
    fn a_stored_set_grants_the_same_rights_after_a_rewrite(
        #[case] stored: &str,
    ) -> anyhow::Result<()> {
        let first: UserPermissionSet = serde_json::from_value(serde_json::json!([stored]))?;
        let rewritten: UserPermissionSet = serde_json::from_slice(&serde_json::to_vec(&first)?)?;

        let held = |set: &UserPermissionSet| -> Vec<UserPermission> {
            UserPermission::iter().filter(|p| set.holds(*p)).collect()
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
        assert!(serde_json::from_value::<UserPermissionSet>(body).is_err());
    }

    /// `insert_all` / `remove_all` / `replace` each report whether they changed
    /// anything, which is what lets the CLI skip a write that grants nothing.
    #[test]
    fn a_mutation_reports_whether_it_changed_the_set() {
        let mut set = UserPermissionSet::from_iter([UserPermission::SubmitPlans]);

        assert!(set.insert_all([UserPermission::CancelPlans]));
        assert!(!set.insert_all([UserPermission::CancelPlans]));
        assert!(set.remove_all([UserPermission::CancelPlans]));
        assert!(!set.remove_all([UserPermission::CancelPlans]));
        assert!(!set.replace([UserPermission::SubmitPlans]));
        assert!(set.replace([]));
        assert!(set.is_empty());
    }

    /// An implied name is not stored, so removing the name that implies it
    /// removes both; an object holding the closure would contradict itself.
    #[test]
    fn the_stored_set_is_the_grant_not_its_closure() {
        let mut set = UserPermissionSet::from_iter([UserPermission::SubmitPlans]);
        assert!(set.holds(UserPermission::ReadPlans));
        assert_eq!(set.len(), 1);

        assert!(set.remove_all([UserPermission::SubmitPlans]));
        assert!(set.is_empty());
        assert!(!set.holds(UserPermission::ReadPlans));
    }
}
