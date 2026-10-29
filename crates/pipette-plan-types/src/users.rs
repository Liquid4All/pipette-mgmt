//! `/api/v1/users`: accounts, their keys and pre-auth keys, what they may do,
//! and their organization membership.

use std::collections::BTreeSet;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    CampaignId, ContactEmail, NonEmptyTrimmedString, OrgId, OrgSlug, PublicKeyHex, UserId,
    UserKeyId, UserPermission, UserPreauthKeyId,
};

/// A set of rights by name. A name this build does not know is kept as sent,
/// so a set a newer server granted reads whole.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct UserPermissionSet(pub BTreeSet<String>);

/// Whether an account may authenticate.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum UserStatus {
    Active,
    /// Requested, unadmitted: it authenticates nothing until an operator admits
    /// it.
    Pending,
    Disabled,
}

/// A person's account, or a job's.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum UserAccountType {
    #[default]
    Human,
    Service,
}

impl UserAccountType {
    /// Whether an account of this type may hold `permission`. A service account
    /// works inside its organization and administers nothing.
    pub fn may_hold(self, permission: UserPermission) -> bool {
        match self {
            Self::Human => true,
            Self::Service => !permission.is_admin(),
        }
    }
}

/// An account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct User {
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub user_id: UserId,
    /// The account's handle, case-folded and unique across accounts.
    pub contact_email: ContactEmail,
    #[serde(default)]
    pub account_type: UserAccountType,
    pub status: UserStatus,
    pub created_at: DateTime<Utc>,
    /// A campaign reader's campaigns, the only ones it reads, while its
    /// organization owns them; absent for every other account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign_scope: Option<BTreeSet<CampaignId>>,
}

/// A key an account signs with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct UserKey {
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub key_id: UserKeyId,
    pub public_key: PublicKeyHex,
    /// The machine the key lives on, e.g. `"work laptop"`.
    pub key_details: NonEmptyTrimmedString,
    pub added_at: DateTime<Utc>,
    /// When the key stops signing, for a key made to sign in one browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

/// The `POST /api/v1/users/register` and `POST /api/v1/users/me/keys`
/// receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct UserKeyRegistered {
    /// The account the key signs as.
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub user_id: UserId,
    /// Whether the account may sign: `pending` until an operator approves an
    /// account asked for by address; until then its key reads
    /// `GET /api/v1/users/me` alone.
    pub status: UserStatus,
    /// The key's id: `userkey_` and the lowercase hex SHA-256 of its public
    /// key's 32 bytes. Each signature names it as its `keyid`.
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub key_id: UserKeyId,
}

/// The `GET /api/v1/users/me` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OwnUserView {
    #[serde(flatten)]
    pub user: User,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub org_id: Option<OrgId>,
    /// The organization's handle, which a member may not read off
    /// `/api/v1/orgs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_slug: Option<OrgSlug>,
    /// The resolved set: what the granted rights imply, expanded.
    pub permissions: UserPermissionSet,
    pub keys: Vec<UserKey>,
    /// The account's first Clerk sign-in, once there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_clerk_sign_in: Option<FirstClerkSignIn>,
    /// What the account may do, as the routes decide it.
    pub may: UserAbilities,
}

/// When an account first signed in through Clerk, and the keys it held then.
/// Each was registered before its owner signed in, so maybe by an
/// administrator rather than the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct FirstClerkSignIn {
    pub at: DateTime<Utc>,
    #[cfg_attr(feature = "schemars", schemars(with = "Vec<String>"))]
    pub prior_keys: Vec<UserKeyId>,
}

/// How far one of an account's authorities reaches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Reach {
    #[default]
    None,
    /// The account's own organization, and the account itself.
    Own,
    /// The campaigns a campaign reader's `campaign_scope` names, and nothing
    /// of its own.
    Listed,
    Every,
}

/// An account's authorities over each kind of record, so a client draws what
/// the routes would admit without re-deriving it from the permission names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct UserAbilities {
    /// Organizations and the accounts on them.
    pub read_roster: Reach,
    /// Accounts, the organization, its private benchmarks and its vault.
    pub administer_roster: Reach,
    /// The devices in reach, read.
    pub read_fleet: bool,
    pub administer_fleet: Reach,
    /// How far the account may create and change org entries of the descriptor catalog.
    pub edit_catalog: Reach,
    pub read_plans: Reach,
    pub file_plans: bool,
    pub create_orgs: bool,
    /// The names this account may grant another, by the ceiling.
    pub grant: Vec<UserPermission>,
    /// The ones among them a service account may hold.
    pub grant_service: Vec<UserPermission>,
}

/// An account as an administrative read returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct UserView {
    #[serde(flatten)]
    pub user: User,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub org_id: Option<OrgId>,
    /// The resolved set.
    pub permissions: UserPermissionSet,
    /// The set as granted, unknown names included: what a revoke reaches. Only
    /// for a caller whose `manage_orgs` reaches the account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stored_permissions: Option<UserPermissionSet>,
}

/// The `GET /api/v1/users/{user_id}` body: the account, and what the reader
/// may do to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct UserDetail {
    #[serde(flatten)]
    pub account: UserView,
    /// The reader's scope and rank both reach it.
    pub may_administer: bool,
    /// Within the reader's ceiling and what its account type may hold, never
    /// the reader's own administration.
    pub may_grant: Vec<UserPermission>,
    /// The stored names the reader may revoke from it: all but the reader's
    /// own `manage_orgs` and `manage_any_orgs`.
    pub may_revoke: Vec<UserPermission>,
    /// Nobody has signed in as the account and it holds no key, so a preauth
    /// mint may admit its first machine. Only for a reader whose `manage_orgs`
    /// covers the account's organization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awaits_sign_in: Option<bool>,
}

/// `POST /api/v1/users` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateUserRequest {
    /// The account's handle: unique, folded to lower case.
    pub contact_email: ContactEmail,
    /// Bounded by what the caller holds.
    #[serde(default)]
    pub permissions: Vec<UserPermission>,
    /// Which organization it joins; omitted by a scoped caller, the caller's
    /// own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub org_id: Option<OrgId>,
    /// A person's account unless asked for a job's.
    #[serde(default)]
    pub account_type: UserAccountType,
    /// Makes a service account a campaign reader: it reads these campaigns
    /// alone, each one its organization's and one the caller reads, and holds
    /// `read_plans` alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign_scope: Option<BTreeSet<CampaignId>>,
}

/// `PATCH /api/v1/users/{user_id}` request body. The address is not here: it
/// is the account's index key, so a `contact_email` sent here is a `400`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct UpdateUserRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<UserStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_type: Option<UserAccountType>,
    /// Replaces a service account's campaign scope, or makes it a campaign
    /// reader, over its organization's campaigns. No value removes one: a
    /// wider account is a new account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign_scope: Option<BTreeSet<CampaignId>>,
}

/// `PUT /api/v1/users/{user_id}/permissions` request body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetUserPermissionsRequest {
    pub permissions: Vec<UserPermission>,
}

/// `PATCH /api/v1/users/{user_id}/permissions` request body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AmendUserPermissionsRequest {
    #[serde(default)]
    pub add: Vec<UserPermission>,
    #[serde(default)]
    pub remove: Vec<UserPermission>,
}

/// The `PATCH /api/v1/users/{user_id}/permissions` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct UserPermissionsAmended {
    #[serde(flatten)]
    pub view: UserView,
    /// Whether the stored set changed.
    pub changed: bool,
    /// Removed names still held through a wider one, so a no-op revocation
    /// shows.
    pub still_granted: Vec<UserPermission>,
}

/// Whether a user pre-auth key admits one registration or many.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UserPreauthUsage {
    /// The first successful registration spends it.
    SingleUse,
    /// Any number, until it expires or is revoked.
    MultiUse,
}

/// `POST /api/v1/users/{user_id}/preauth-keys` request body. Single-use by
/// construction: unknown fields are refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MintUserPreauthRequest {
    /// Days the key stands, 1 to 90; 90 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_days: Option<u16>,
    /// Who the invite is for, as the listing shows it; at most 4096 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<NonEmptyTrimmedString>,
}

/// A user pre-auth key without its secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct UserPreauthKeyView {
    /// The invite's id, which its token carries before the secret.
    pub key_id: UserPreauthKeyId,
    /// The account it registers keys to.
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub user_id: UserId,
    /// How many registrations it admits.
    pub usage: UserPreauthUsage,
    /// When it stops registering keys; `null` on one the host minted without
    /// an expiry.
    pub expires_at: Option<DateTime<Utc>>,
    /// It registers nothing, and `users preauth prune` deletes it.
    pub expired: bool,
    /// Who it is for, as its minter wrote.
    pub note: Option<NonEmptyTrimmedString>,
    /// When it was minted.
    pub created_at: DateTime<Utc>,
    /// Absent for a key the host minted.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub minted_by: Option<UserId>,
}

/// A user pre-auth key's token (`user_preauth_{key_id}.{secret}`): `Debug`
/// hides it, and only [`Self::expose`] reads it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserPreauthKey(String);

impl UserPreauthKey {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The token itself, named so every disclosure is easy to find.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for UserPreauthKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UserPreauthKey(redacted)")
    }
}

#[cfg(feature = "schemars")]
impl schemars::JsonSchema for UserPreauthKey {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "UserPreauthKey".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "string" })
    }
}

/// The only answer that carries the token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct UserPreauthKeyMinted {
    #[serde(flatten)]
    pub key: UserPreauthKeyView,
    /// The invite itself, shown this once: whoever holds it registers a key to
    /// the account by sending it as `user_preauth_key`.
    pub token: UserPreauthKey,
}

/// `PUT /api/v1/users/{user_id}/org` request body. A `null` `org_id` clears
/// the membership; the key is required and unknown keys are refused, so `{}`
/// is a `400` rather than a clear.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetUserOrgRequest {
    // Required though nullable: derive alone reads a missing `Option` as `None`.
    #[serde(deserialize_with = "Option::deserialize")]
    #[cfg_attr(
        feature = "schemars",
        schemars(required, schema_with = "crate::body_values::nullable::<String>")
    )]
    pub org_id: Option<OrgId>,
}

/// An account as the membership routes answer it: the record and its
/// membership, without the rights.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OrgUserRow {
    #[serde(flatten)]
    pub user: User,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub org_id: Option<OrgId>,
}

/// `POST /api/v1/users/register` request body: exactly one of
/// `user_preauth_key` (an operator's invite naming the account) and
/// `contact_email` (a request for a new, pending account).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RegisterUserKeyRequest {
    /// The key's Ed25519 public key, hex-encoded: 32 bytes, 64 characters.
    pub public_key: PublicKeyHex,
    /// The invite an administrator minted for the account: the `token` the
    /// mint answered, `user_preauth_{key_id}.{secret}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_preauth_key: Option<UserPreauthKey>,
    /// Where to reach whoever asks for a new account, which starts `pending`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_email: Option<ContactEmail>,
    /// The machine the key lives on, e.g. `"work laptop"`; at most 4096 characters.
    pub key_details: NonEmptyTrimmedString,
}

/// `POST /api/v1/users/me/keys` request body: a registration's key and its
/// details. The request's signature stands in for the pre-auth key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AddUserKeyRequest {
    /// The key's Ed25519 public key, hex-encoded: 32 bytes, 64 characters.
    pub public_key: PublicKeyHex,
    /// The machine the key lives on, e.g. `"work laptop"`; at most 4096 characters.
    pub key_details: NonEmptyTrimmedString,
    /// When the key stops signing, at most 30 days out: for a key made to sign
    /// in one browser. A machine's key has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

/// `POST /api/v1/users/register/clerk` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RegisterClerkUserKeyRequest {
    /// The key's Ed25519 public key, hex-encoded: 32 bytes, 64 characters.
    pub public_key: PublicKeyHex,
    /// The browser the key lives in, as the account's key listing shows it; at
    /// most 4096 characters.
    pub key_details: NonEmptyTrimmedString,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preauth_key_is_redacted_in_debug() {
        let key = UserPreauthKey::new("user_preauth_k1.s3cret");
        assert_eq!(format!("{key:?}"), "UserPreauthKey(redacted)");
        assert_eq!(key.expose(), "user_preauth_k1.s3cret");
    }
}
