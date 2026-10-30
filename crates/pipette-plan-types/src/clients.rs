//! `POST /clients/register` and `/clients/me`, a device's registration and own
//! record; and `/api/v1/clients`, the fleet as a user reads and administers it.

use std::collections::BTreeSet;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::owners::ClientOwner;
use crate::{
    CampaignId, ClientId, ClientPermission, ContactEmail, DeviceFormFactor, NonEmptyTrimmedString,
    OrgId, PublicKeyHex, Tag,
};

/// A device's approval state.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    strum::EnumString,
    strum::Display,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
pub enum ClientStatus {
    Pending,
    Approved,
    #[strum(default)]
    Unknown(String),
}

impl ClientStatus {
    pub fn as_str(&self) -> &str {
        if let Self::Unknown(raw) = self {
            raw
        } else {
            self.into()
        }
    }
}

impl Serialize for ClientStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ClientStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // `Unknown` is the parse's default, so every string reads.
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The statuses this build knows, as the app types them.
#[cfg(feature = "schemars")]
impl schemars::JsonSchema for ClientStatus {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ClientStatus".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "enum": ["pending", "approved"], "type": "string" })
    }
}

/// `POST /clients/register`'s receipt. `private_key` is present only when the
/// server generated the keypair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRegistered {
    pub client_id: ClientId,
    pub status: ClientStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<ClientPrivateKey>,
}

/// A server-generated device signing key, hex-encoded: `Debug` hides it, and
/// only [`Self::expose`] reads it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientPrivateKey(String);

impl ClientPrivateKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The key itself, named so every disclosure is easy to find.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ClientPrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientPrivateKey(redacted)")
    }
}

/// The device's own record, as `GET` and `PATCH /clients/me` answer it. Every
/// `device_*` key is present, `null` when unset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRecord {
    pub client_id: ClientId,
    pub organization: String,
    pub client_details: String,
    pub contact_email: String,
    pub status: ClientStatus,
    /// Assigned by mgmt; sorted.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Granted by an operator; a name this build does not know is kept as sent.
    #[serde(default)]
    pub permissions: Vec<String>,
    /// While `true` the device's leases were voided by a profile change: it
    /// drops any in-flight work and claims afresh.
    #[serde(default)]
    pub reindex_pending: bool,
    /// The flags the device reported itself; sorted.
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub device_name: Option<String>,
    pub device_form_factor: Option<DeviceFormFactor>,
    pub device_os_name: Option<String>,
    pub device_os_version: Option<String>,
    pub device_cpu_arch: Option<String>,
    pub device_os_family: Option<String>,
    pub device_chip_model: Option<String>,
    pub device_ram_bytes: Option<u64>,
    pub device_gpu_model: Option<String>,
    pub device_gpu_vram_bytes: Option<u64>,
    pub device_npu_model: Option<String>,
    pub device_npu_vram_bytes: Option<u64>,
}

/// `POST /clients/register`'s body: exactly one of `public_key` and
/// `generate_key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterClient {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<PublicKeyHex>,
    /// Asks the server to make the keypair and answer the private key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_key: Option<bool>,
    pub organization: NonEmptyTrimmedString,
    pub client_details: NonEmptyTrimmedString,
    pub contact_email: ContactEmail,
    /// Registers the device approved, and possibly owned and tagged. Read under
    /// its former name `preauth_key` too.
    #[serde(
        default,
        alias = "preauth_key",
        skip_serializing_if = "Option::is_none"
    )]
    pub client_preauth_key: Option<ClientPreauthKey>,
    #[serde(flatten)]
    pub device: DeviceProfileFields,
    /// The flags the device reports itself.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub capabilities: BTreeSet<String>,
    /// Refused by the server, which answers `400`: an operator assigns
    /// permissions. Never serialized.
    #[serde(default, skip_serializing, deserialize_with = "present")]
    pub permissions: Option<Refused>,
}

/// `PATCH /clients/me`'s body. An absent field is left as stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateClient {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_details: Option<NonEmptyTrimmedString>,
    #[serde(flatten)]
    pub device: DeviceProfileFields,
    /// When present, replaces the stored set whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<BTreeSet<String>>,
    /// Refused by the server, which answers `400`: an operator assigns
    /// permissions. Never serialized.
    #[serde(default, skip_serializing, deserialize_with = "present")]
    pub permissions: Option<Refused>,
}

/// The device's profile as a registration or an update sets it; an absent
/// field is left as stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceProfileFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<NonEmptyTrimmedString>,
    /// A string rather than a [`DeviceFormFactor`], so a value outside the set
    /// reaches the server's documented refusal instead of a parse error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_form_factor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_os_name: Option<NonEmptyTrimmedString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_os_version: Option<NonEmptyTrimmedString>,
    /// The CPU architecture that the device reports. The server maps known
    /// aliases to a canonical value for the `arch:` flag, for example `amd64`
    /// to `x86_64` and `aarch64` to `arm64`. It keeps other values as a
    /// lowercase slug. These examples are not the full list:
    /// `canonical_cpu_arch` in `pipette-core` holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_cpu_arch: Option<NonEmptyTrimmedString>,
    /// The OS family that the device reports. The server maps known aliases
    /// to a canonical value for the `os_family:` flag, for example `win32` to
    /// `windows` and `ipados` to `ios`. It keeps other values as a lowercase
    /// slug. These examples are not the full list: `canonical_os_family` in
    /// `pipette-core` holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_os_family: Option<NonEmptyTrimmedString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_chip_model: Option<NonEmptyTrimmedString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_ram_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_gpu_model: Option<NonEmptyTrimmedString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_gpu_vram_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_npu_model: Option<NonEmptyTrimmedString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_npu_vram_bytes: Option<u64>,
}

/// A device pre-auth key as the device presents it: `Debug` hides it, and only
/// [`Self::expose`] reads it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientPreauthKey(String);

impl ClientPreauthKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The key itself, named so every disclosure is easy to find.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ClientPreauthKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientPreauthKey(redacted)")
    }
}

/// A request field that exists only to be refused: any value, `null` included,
/// reads as present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused;

impl<'de> Deserialize<'de> for Refused {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer)?;
        Ok(Self)
    }
}

/// `Some` even for an explicit `null`, which serde's `Option` reads as absent.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Refused>, D::Error> {
    Refused::deserialize(deserializer).map(Some)
}

/// A device's rights by name, a name this build does not know included.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct ClientPermissionSet(pub BTreeSet<String>);

/// The device's profile as it reported it; a field it never set is absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DeviceProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_name: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub device_form_factor: Option<DeviceFormFactor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_os_name: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_os_version: Option<NonEmptyTrimmedString>,
    /// The CPU architecture that the device reports. The server maps known
    /// aliases to a canonical value for the `arch:` flag, for example `amd64`
    /// to `x86_64` and `aarch64` to `arm64`. It keeps other values as a
    /// lowercase slug. These examples are not the full list:
    /// `canonical_cpu_arch` in `pipette-core` holds it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_cpu_arch: Option<NonEmptyTrimmedString>,
    /// The OS family that the device reports. The server maps known aliases
    /// to a canonical value for the `os_family:` flag, for example `win32` to
    /// `windows` and `ipados` to `ios`. It keeps other values as a lowercase
    /// slug. These examples are not the full list: `canonical_os_family` in
    /// `pipette-core` holds it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_os_family: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_chip_model: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_ram_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_gpu_model: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_gpu_vram_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_npu_model: Option<NonEmptyTrimmedString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_npu_vram_bytes: Option<u64>,
}

impl DeviceProfile {
    /// True when no `device_*` field is set.
    pub fn is_empty(&self) -> bool {
        *self == DeviceProfile::default()
    }
}

/// A device's record as a user reads it: its `contact_email` only for the
/// device's administrators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Client {
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub client_id: ClientId,
    pub public_key: PublicKeyHex,
    pub organization: NonEmptyTrimmedString,
    pub client_details: NonEmptyTrimmedString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "ContactEmail"))]
    pub contact_email: Option<ContactEmail>,
    pub status: ClientStatus,
    pub registered_at: DateTime<Utc>,
    #[serde(flatten)]
    pub device_profile: DeviceProfile,
    /// The flags the device reports itself, e.g. `runtime:llama_cpp`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub capabilities: BTreeSet<String>,
}

/// `owner=` on `GET /api/v1/clients`. An organization's devices are `org_id=`,
/// since they roll up through its members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumString)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum FleetOwner {
    /// The devices the caller owns as a person.
    Mine,
}

/// The scalar query parameters of `GET /api/v1/clients`. The repeatable
/// filters (`tag`, `requires`, `any_of`, `excludes`, `not_all_of`) are read
/// from the raw query, one value per occurrence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ClientListQuery {
    /// Absent or empty: every status.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub status: Option<ClientStatus>,
    /// Rows a page holds: 50 by default, at most 200 (the server's
    /// `list_default_limit` and `list_max_limit`). `0` or more than that is a
    /// `400`, not clamped.
    #[cfg_attr(feature = "schemars", schemars(range(min = 1)))]
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub limit: Option<usize>,
    /// The preceding page's `next_cursor`, unchanged; omit it for the first
    /// page.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub cursor: Option<String>,
    /// `false` narrows to the devices nobody owns, `true` to the owned ones.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub owned: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub owner: Option<FleetOwner>,
    /// The devices an organization owns, directly or through a member.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub org_id: Option<OrgId>,
}

/// Query parameters for `GET /api/v1/clients/capabilities`: the status filter
/// alone, since an aggregate has no page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CapabilityVocabularyQuery {
    /// Absent or empty: every status.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub status: Option<ClientStatus>,
}

/// `PUT /api/v1/clients/{client_id}/owner` request body. A `null` `owner`
/// clears ownership, and `{}` is a `400`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetClientOwnerRequest {
    #[serde(deserialize_with = "Option::deserialize")]
    #[cfg_attr(
        feature = "schemars",
        schemars(required, schema_with = "crate::body_values::nullable::<ClientOwner>")
    )]
    pub owner: Option<ClientOwner>,
}

/// `PATCH /api/v1/clients/{client_id}/permissions` request body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AmendClientPermissionsRequest {
    #[serde(default)]
    pub add: BTreeSet<ClientPermission>,
    #[serde(default)]
    pub remove: BTreeSet<ClientPermission>,
}

/// `PUT /api/v1/clients/{client_id}/permissions` request body: the whole set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetClientPermissionsRequest {
    pub permissions: BTreeSet<ClientPermission>,
}

/// `PATCH /api/v1/clients/{client_id}` request body: the three mutable detail
/// fields. The owner has its own route, and status its own verbs, so either
/// sent here is a `400` rather than ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct UpdateClientRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<NonEmptyTrimmedString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_details: Option<NonEmptyTrimmedString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_email: Option<ContactEmail>,
}

/// A device as its writes and its organization's roster answer it: the record
/// and its owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ClientView {
    #[serde(flatten)]
    pub client: Client,
    /// Absent for a device nobody owns; an organization's roster always
    /// carries it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<ClientOwner>,
}

/// One row of the fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ClientListRow {
    #[serde(flatten)]
    pub view: ClientView,
    /// The flags the server normalizes from the `device_*` profile; with
    /// `capabilities` and `tags` (as `tag:` flags), what a job's eligibility
    /// tests.
    pub derived_capabilities: BTreeSet<String>,
    /// Assigned by an operator.
    pub tags: BTreeSet<Tag>,
    /// The stored set, a name a device cannot hold included.
    pub permissions: ClientPermissionSet,
    /// The device takes no work until maintenance re-evaluates its eligible
    /// jobs.
    pub reindex_pending: bool,
    /// The claim path passes the device over until its suspension is cleared.
    pub suspended: bool,
    /// The time of the device's last verified, signed request, accurate to
    /// the server's stamp interval. `null` when the server holds no stamp for
    /// the device.
    pub last_seen_at: Option<DateTime<Utc>>,
    /// The queued jobs the device could claim, as the last `queue-maintenance`
    /// run counted them. `null` before the first run counts them.
    pub backlog: Option<Backlog>,
}

/// The queued jobs a device could claim, as one `queue-maintenance` run
/// counted them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Backlog {
    /// The jobs the device is indexed for and could claim: queued, not leased,
    /// not past their deadline, and not denied by the device.
    pub job_count: u64,
    /// The start of the run that counted the jobs. The count is at most one
    /// `queue-maintenance` interval old.
    pub counted_at: DateTime<Utc>,
}

/// The `GET /api/v1/clients/{client_id}` body: the row, and what the reader may
/// do to the device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ClientDetail {
    #[serde(flatten)]
    pub view: ClientListRow,
    /// `manage_clients` whose device scope reaches the device.
    pub may_administer: bool,
    /// The organization the device rolls up to, only for a reader of that
    /// roster or the organization's own device administrator.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub owning_org_id: Option<OrgId>,
    /// The device's backlog by campaign, as the last `queue-maintenance` run
    /// counted it. The value is `null` before the first run counts the
    /// backlog, or if the server cannot read the count.
    pub backlog_by_campaign: Option<BacklogByCampaign>,
}

/// The queued jobs a device could claim, by campaign, as one
/// `queue-maintenance` run counted them. A job counts once in each campaign
/// that holds it, so the counts can add up to more than the device's backlog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct BacklogByCampaign {
    /// The campaigns that the reader may read. The campaign with the most
    /// jobs is first.
    pub campaigns: Vec<BacklogCampaign>,
    /// The jobs that no plan of a campaign holds.
    pub outside_campaign_job_count: u64,
    /// The jobs in the campaigns that the reader may not read, summed over
    /// those campaigns.
    pub unreadable_job_count: u64,
    /// The start of the run that counted the jobs.
    pub counted_at: DateTime<Utc>,
}

/// One campaign's share of a device's backlog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct BacklogCampaign {
    pub campaign_id: CampaignId,
    pub name: NonEmptyTrimmedString,
    /// The distinct jobs that the campaign's plans hold.
    pub job_count: u64,
}

/// What a permission write left the device holding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ClientPermissions {
    pub client_id: ClientId,
    pub permissions: ClientPermissionSet,
}

/// What an approval did: the device as its writes answer it, and the status
/// it moved from. `changed: false` is a device approved already.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ClientApproved {
    #[serde(flatten)]
    pub view: ClientView,
    pub changed: bool,
    /// The status before the write, absent when nothing moved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_status: Option<ClientStatus>,
}

/// Where a capability flag comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySource {
    /// Normalized by the server from the device's profile, in a reserved
    /// namespace a device cannot report into.
    Derived,
    /// Reported by the device, e.g. `runtime:llama_cpp`.
    Declared,
    /// An operator's tag, `tag:<tag>`.
    Assigned,
}

/// One flag, and the devices that carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CapabilityFlagCount {
    pub flag: String,
    pub client_count: usize,
}

/// The flags of one namespace and source, sorted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CapabilityNamespace {
    pub namespace: String,
    pub source: CapabilitySource,
    pub flags: Vec<CapabilityFlagCount>,
}

/// The `GET /api/v1/clients/capabilities` body: the flags the fleet reports,
/// grouped by namespace, which a plan author writes `requires` from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct CapabilityVocabulary {
    /// Ascending by namespace.
    pub namespaces: Vec<CapabilityNamespace>,
    /// Flags with no namespace, ascending.
    pub free_form: Vec<CapabilityFlagCount>,
    /// Devices that pass the status filter.
    pub client_count: usize,
    /// Devices whose record did not read: a flag only they carry is missing.
    pub unreadable_client_count: usize,
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    /// A status the build knows round-trips; one it does not is kept as sent.
    #[rstest]
    #[case("pending", ClientStatus::Pending)]
    #[case("approved", ClientStatus::Approved)]
    #[case("retired", ClientStatus::Unknown("retired".into()))]
    fn a_client_status_keeps_its_spelling(#[case] raw: &str, #[case] status: ClientStatus) {
        assert_eq!(
            serde_json::from_value::<ClientStatus>(json!(raw)).ok(),
            Some(status.clone())
        );
        assert_eq!(serde_json::to_value(&status).ok(), Some(json!(raw)));
    }

    /// The key never reaches a log through a surrounding struct's `Debug`.
    #[test]
    fn a_preauth_key_is_redacted_in_debug() {
        let key = ClientPreauthKey::new("client_preauth_k1.s3cret");
        assert_eq!(format!("{key:?}"), "ClientPreauthKey(redacted)");
        assert_eq!(key.expose(), "client_preauth_k1.s3cret");
    }

    /// A registration answer can be logged whole without its private key.
    #[test]
    fn a_private_key_is_redacted_in_debug() -> anyhow::Result<()> {
        let registered = ClientRegistered {
            client_id: ClientId::try_new("c1")?,
            status: ClientStatus::Approved,
            private_key: Some(ClientPrivateKey::new("deadbeef")),
        };
        assert!(!format!("{registered:?}").contains("deadbeef"));
        assert_eq!(
            serde_json::to_value(&registered)?["private_key"],
            json!("deadbeef")
        );
        Ok(())
    }
}
