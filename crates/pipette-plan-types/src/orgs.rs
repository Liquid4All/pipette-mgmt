//! `/api/v1/orgs`: organizations, their rosters and devices, their device
//! pre-auth keys, and their overview.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::campaigns::CampaignView;
use crate::clients::{Client, ClientPermissionSet, ClientStatus};
use crate::users::{User, UserStatus};
use crate::{
    ClientId, ClientPreauthKey, ClientPreauthKeyId, NonEmptyTrimmedString, OrgId, OrgSlug, Tag,
    UserId,
};

/// Whether an organization is in use.
#[derive(
    Debug,
    Clone,
    Copy,
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
pub enum OrgStatus {
    Active,
    Disabled,
}

/// An organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Organization {
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub org_id: OrgId,
    /// The handle, unique across organizations.
    pub slug: OrgSlug,
    /// What to print for a human; nothing keys on it.
    pub display_name: NonEmptyTrimmedString,
    pub status: OrgStatus,
    pub created_at: DateTime<Utc>,
}

/// An organization as its listing, its read and its writes answer it: the
/// record and its counts, not its rosters (`/users`, `/clients`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OrgView {
    #[serde(flatten)]
    pub org: Organization,
    /// Absent only on `GET /api/v1/orgs?counts=false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_count: Option<usize>,
    /// Absent only on `GET /api/v1/orgs?counts=false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_count: Option<usize>,
}

/// `GET /api/v1/orgs`'s query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OrgListQuery {
    /// `false` lists the records alone, for a caller after names.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub counts: Option<bool>,
}

/// `POST /api/v1/orgs` request body. The display name defaults to the handle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateOrgRequest {
    /// The handle: `[a-z0-9_-]`, at most 64 characters, trimmed and folded to
    /// lower case; never `org_`-prefixed, which spells an id.
    pub slug: OrgSlug,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<NonEmptyTrimmedString>,
}

/// `PATCH /api/v1/orgs/{org_id}` request body. The slug is not here: it is the
/// index key, meant to be stable, so a `slug` sent here is a `400`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct UpdateOrgRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<NonEmptyTrimmedString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<OrgStatus>,
}

/// Whether a device pre-auth key admits one registration or many.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ClientPreauthUsage {
    /// The first successful registration spends it.
    SingleUse,
    /// Any number, until it expires or is revoked.
    MultiUse,
}

/// `POST /api/v1/orgs/{org_id}/client-preauth-keys` request body. Unknown fields
/// are refused, so a caller asking for tags or a permission template is a
/// `400`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MintClientPreauthRequest {
    /// Days the key stands, 1 to 90; 90 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_days: Option<u16>,
    /// `multi_use` for one key a rack shares rather than one per device;
    /// `single_use` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ClientPreauthUsage>,
    /// What the key is for, as the listing shows it; at most 4096 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<NonEmptyTrimmedString>,
}

/// A device pre-auth key without its secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ClientPreauthKeyView {
    pub key_id: ClientPreauthKeyId,
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub org_id: OrgId,
    pub usage: ClientPreauthUsage,
    pub expires_at: Option<DateTime<Utc>>,
    /// It registers nothing, and `clients preauth prune` deletes it.
    pub expired: bool,
    /// A host-minted key's; empty, and absent, for every key minted here.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub default_tags: BTreeSet<Tag>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_organization: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_permissions: Option<ClientPermissionSet>,
    pub note: Option<NonEmptyTrimmedString>,
    pub created_at: DateTime<Utc>,
    /// Absent for a key the host minted.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub minted_by: Option<UserId>,
}

/// The only answer that carries the token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ClientPreauthKeyMinted {
    #[serde(flatten)]
    pub key: ClientPreauthKeyView,
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub token: ClientPreauthKey,
}

/// The first five records of a kind an overview draws, and how many there are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schemars", schemars(rename = "{T}Highlights"))]
pub struct Highlights<T> {
    pub first: Vec<T>,
    pub total: usize,
}

impl<T> Highlights<T> {
    /// The first `shown` of `all`, and their count.
    pub fn of(all: impl IntoIterator<Item = T>, shown: usize) -> Self {
        let mut first: Vec<T> = all.into_iter().collect();
        let total = first.len();
        first.truncate(shown);
        Self { first, total }
    }

    pub fn map<U>(self, row: impl FnMut(T) -> U) -> Highlights<U> {
        Highlights {
            first: self.first.into_iter().map(row).collect(),
            total: self.total,
        }
    }
}

/// The `GET /api/v1/orgs/{org_id}/overview` body: the record, what it holds by
/// count, and what waits on a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OrgOverview {
    #[serde(flatten)]
    pub org: Organization,
    pub member_count: usize,
    pub client_count: usize,
    /// The statuses present only.
    pub member_status_counts: BTreeMap<UserStatus, usize>,
    /// Accounts waiting to be admitted.
    pub pending_members: Highlights<User>,
    /// The statuses present only; `null` without `read_clients`.
    pub client_status_counts: Option<BTreeMap<ClientStatus, usize>>,
    /// Registrations waiting to be approved or rejected; `null` without
    /// `read_clients`.
    pub pending_clients: Option<Highlights<Client>>,
    /// Devices the claim path passes over until their suspension is cleared;
    /// `null` without `read_clients`.
    pub suspended_clients: Option<Highlights<Client>>,
    /// Its campaigns the reader reads, newest first, as the listing's
    /// `plan=true` rows; `null` where the listing refuses the reader.
    pub campaigns: Option<Highlights<CampaignView>>,
    /// Its vault resources the reader reads.
    pub vault_resource_count: Option<usize>,
    /// Its private benchmarks the reader reads.
    pub benchmark_count: Option<usize>,
    /// Its device pre-auth keys not yet expired; `null` for a reader who may
    /// not mint one.
    pub client_preauth_key_count: Option<usize>,
    /// Whether the reader may rename it.
    pub may_administer: bool,
    /// Whether the reader may set its status.
    pub may_delete: bool,
}

/// `PUT /api/v1/orgs/{org_id}/clients` request body: the devices to hand to
/// the organization in one call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AssignClientsRequest {
    pub client_ids: Vec<ClientId>,
}

/// One device's outcome in a bulk assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct AssignedClient {
    pub client_id: ClientId,
    /// `false` when the device already belonged to this organization.
    pub changed: bool,
}

/// The `PUT /api/v1/orgs/{org_id}/clients` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ClientsAssigned {
    pub org_id: OrgId,
    pub clients: Vec<AssignedClient>,
}
