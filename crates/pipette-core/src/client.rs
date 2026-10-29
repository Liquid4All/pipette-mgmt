//! Client records: the fleet-member principal, with their parsing and repair.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use strum::{AsRefStr, Display, EnumString};
use tabled::Tabled;

use pipette_plan_types::{
    ClientId, ContactEmail, ContactEmailError, DeviceFormFactor, DeviceProfile, EmptyStringError,
    NonEmptyTrimmedString, PublicKeyHex, Tag,
};

pub const SELF_HEALED_ORGANIZATION: &str = "<unset>";
pub const SELF_HEALED_CLIENT_DETAILS: &str = "legacy client";
/// Substituted for a stored address that no longer validates; `.invalid` (RFC
/// 2606) can never collide with a routable domain.
pub const SELF_HEALED_CONTACT_EMAIL: &str = "unset@unset.invalid";

/// Whether a registered device may authenticate.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Display,
    EnumString,
    AsRefStr,
    strum::EnumIter,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ClientStatus {
    Pending,
    Approved,
}

/// The body reads a status a newer server may add as `Unknown`; a record holds
/// only the ones this build knows.
impl TryFrom<pipette_plan_types::ClientStatus> for ClientStatus {
    type Error = String;

    fn try_from(status: pipette_plan_types::ClientStatus) -> Result<Self, Self::Error> {
        match status {
            pipette_plan_types::ClientStatus::Pending => Ok(Self::Pending),
            pipette_plan_types::ClientStatus::Approved => Ok(Self::Approved),
            pipette_plan_types::ClientStatus::Unknown(raw) => Err(format!(
                "unknown client status `{raw}`, expected `pending` or `approved`"
            )),
        }
    }
}

impl From<ClientStatus> for pipette_plan_types::ClientStatus {
    fn from(status: ClientStatus) -> Self {
        match status {
            ClientStatus::Pending => Self::Pending,
            ClientStatus::Approved => Self::Approved,
        }
    }
}

/// Capability namespaces the server derives from a client's `device_*`
/// profile. Server-owned: a client reporting one could assert a profile it does
/// not have, so [`is_reserved_capability`] rejects them on the wire.
pub const RESERVED_CAPABILITY_NAMESPACES: &[&str] = &[
    "os",
    "os_version",
    "arch",
    "os_family",
    "device",
    "chip",
    "form_factor",
    "ram_bytes",
    "gpu",
    "gpu_vram_bytes",
    "npu",
    "npu_vram_bytes",
];

/// True when `flag`'s namespace (before its first `:`) is in
/// [`RESERVED_CAPABILITY_NAMESPACES`]. Assumes canonical form ([`slugify`]): a
/// non-canonical `OS:` would slip past, so the wire rejects those first.
pub fn is_reserved_capability(flag: &str) -> bool {
    flag.split_once(':')
        .is_some_and(|(ns, _)| RESERVED_CAPABILITY_NAMESPACES.contains(&ns))
}

/// The namespace a client's tags match in. Operator-assigned (pre-auth key or
/// `clients tag add`), so a client may not report one. Kept out of
/// [`RESERVED_CAPABILITY_NAMESPACES`], which the planner treats as
/// single-valued: a client carries any number of tags.
pub const TAG_NAMESPACE: &str = "tag";

/// The flag `tag` matches as: `tag:<tag>`. Canonical, since a [`Tag`] is.
pub fn tag_flag(tag: &Tag) -> String {
    format!("{TAG_NAMESPACE}:{tag}")
}

/// True when `flag` is in [`TAG_NAMESPACE`]. Assumes `flag` is canonical, as
/// [`is_reserved_capability`] does.
pub fn is_tag_capability(flag: &str) -> bool {
    flag.split_once(':')
        .is_some_and(|(ns, _)| ns == TAG_NAMESPACE)
}

/// The namespace of the flags a client reports, one per app build it can run:
/// `client_version:<version>`. A job names the builds it admits in one `any_of`
/// group of these flags.
pub const CLIENT_VERSION_NAMESPACE: &str = "client_version";

/// The version `flag` names when it is in [`CLIENT_VERSION_NAMESPACE`].
pub fn client_version_of(flag: &str) -> Option<&str> {
    flag.split_once(':')
        .filter(|(ns, _)| *ns == CLIENT_VERSION_NAMESPACE)
        .map(|(_, version)| version)
}

/// A flag's **canonical form**: lowercase with all whitespace removed
/// (`"iPhone 17 Pro"` → `"iphone17pro"`). Matching is exact set-containment,
/// so both a client's flags and a plan's `requires` must be canonical.
pub fn slugify(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Canonical CPU architecture for the server-owned `arch:` flag.
fn canonical_cpu_arch(value: &str) -> String {
    let slug = slugify(value);
    match slug.as_str() {
        "amd64" | "x64" | "x86-64" | "x86_64" => "x86_64".into(),
        "aarch64" | "arm64-v8a" | "arm64e" | "arm64" => "arm64".into(),
        _ => slug,
    }
}

/// Canonical OS family for the server-owned `os_family:` flag. `darwin` is the
/// kernel name on every Apple platform, so a phone or tablet that reports it
/// is `ios` and any other device is `macos`.
fn canonical_os_family(value: &str, form_factor: Option<DeviceFormFactor>) -> String {
    let slug = slugify(value);
    match slug.as_str() {
        "darwin" => match form_factor {
            Some(DeviceFormFactor::Phone | DeviceFormFactor::Tablet) => "ios".into(),
            _ => "macos".into(),
        },
        "macos" | "macosx" | "osx" => "macos".into(),
        "win32" | "win64" | "windows" => "windows".into(),
        "ipados" | "ios" => "ios".into(),
        _ => slug,
    }
}

/// The reserved-namespace flags a device profile normalizes to, one per
/// populated field; byte counts match exactly as their decimal value.
pub fn normalized_flags(profile: &DeviceProfile) -> BTreeSet<String> {
    let mut flags = BTreeSet::new();
    let mut string_flag = |ns: &str, value: Option<&NonEmptyTrimmedString>| {
        if let Some(v) = value {
            flags.insert(format!("{ns}:{}", slugify(v.as_str())));
        }
    };
    string_flag("device", profile.device_name.as_ref());
    string_flag("os", profile.device_os_name.as_ref());
    string_flag("os_version", profile.device_os_version.as_ref());
    string_flag("chip", profile.device_chip_model.as_ref());
    string_flag("gpu", profile.device_gpu_model.as_ref());
    string_flag("npu", profile.device_npu_model.as_ref());
    if let Some(arch) = &profile.device_cpu_arch {
        flags.insert(format!("arch:{}", canonical_cpu_arch(arch.as_str())));
    }
    if let Some(family) = &profile.device_os_family {
        flags.insert(format!(
            "os_family:{}",
            canonical_os_family(family.as_str(), profile.device_form_factor)
        ));
    }
    if let Some(ff) = profile.device_form_factor {
        flags.insert(format!("form_factor:{}", slugify(&ff.to_string())));
    }
    if let Some(n) = profile.device_ram_bytes {
        flags.insert(format!("ram_bytes:{n}"));
    }
    if let Some(n) = profile.device_gpu_vram_bytes {
        flags.insert(format!("gpu_vram_bytes:{n}"));
    }
    if let Some(n) = profile.device_npu_vram_bytes {
        flags.insert(format!("npu_vram_bytes:{n}"));
    }
    flags
}

/// The flags a submission record proves its run held: [`normalized_flags`] of
/// its `device_*` fields, and its `client_version` as a flag. A field that is
/// absent, empty or mistyped adds nothing.
pub fn recorded_run_flags(record: &serde_json::Value) -> BTreeSet<String> {
    let text = |field: &str| {
        record
            .get(field)
            .and_then(serde_json::Value::as_str)
            .and_then(|value| NonEmptyTrimmedString::try_new(value).ok())
    };
    let bytes = |field: &str| record.get(field).and_then(serde_json::Value::as_u64);
    let mut flags = normalized_flags(&DeviceProfile {
        device_name: text("device_name"),
        device_form_factor: record
            .get("device_form_factor")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| value.parse().ok()),
        device_os_name: text("device_os_name"),
        device_os_version: text("device_os_version"),
        device_cpu_arch: text("device_cpu_arch"),
        device_os_family: text("device_os_family"),
        device_chip_model: text("device_chip_model"),
        device_ram_bytes: bytes("device_ram_bytes"),
        device_gpu_model: text("device_gpu_model"),
        device_gpu_vram_bytes: bytes("device_gpu_vram_bytes"),
        device_npu_model: text("device_npu_model"),
        device_npu_vram_bytes: bytes("device_npu_vram_bytes"),
    });
    if let Some(build) = text("client_version") {
        flags.insert(format!(
            "{CLIENT_VERSION_NAMESPACE}:{}",
            slugify(build.as_str())
        ));
    }
    flags
}

impl Client {
    /// The flags normalized from `device_*` plus the reported capabilities. The
    /// matcher adds tags, so read [`crate::matching::FleetTags::capabilities_of`].
    pub(crate) fn effective_capabilities(&self) -> BTreeSet<String> {
        let mut caps = normalized_flags(&self.device_profile);
        caps.extend(self.capabilities.iter().cloned());
        caps
    }
}

/// A device's identity record, `clients/{client_id}.json`. Who owns it is a
/// separate record in `[storage]` ([`crate::organization::Ownership`]);
/// `organization` is only a label the device sent about itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Tabled)]
pub struct Client {
    #[tabled(rename = "Client ID")]
    pub client_id: ClientId,
    #[tabled(skip)]
    pub public_key: PublicKeyHex,
    /// Strict; legacy records are repaired through `RepairableClient` at
    /// auth-store read boundaries.
    #[tabled(rename = "Organization")]
    pub organization: NonEmptyTrimmedString,
    #[tabled(rename = "Details", display = "truncate_details")]
    pub client_details: NonEmptyTrimmedString,
    #[tabled(rename = "Contact")]
    pub contact_email: ContactEmail,
    #[tabled(rename = "Status")]
    pub status: ClientStatus,
    #[tabled(rename = "Registered")]
    pub registered_at: DateTime<Utc>,
    /// Flattened to top-level `device_*` keys; absent on legacy records. Tags
    /// are deliberately not a field: they live in the `tags-index/` marker trees.
    #[tabled(skip)]
    #[serde(flatten)]
    pub device_profile: DeviceProfile,
    /// Free-form capability flags the client reports directly (e.g.
    /// `runtime:llama_cpp`), unioned with [`normalized_flags`]. Never a reserved
    /// namespace: the wire rejects those.
    #[tabled(skip)]
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub capabilities: BTreeSet<String>,
}

impl Client {
    /// The record as a reader sees it: the address only for one who administers
    /// the device.
    pub fn view(self, administered: bool) -> pipette_plan_types::Client {
        pipette_plan_types::Client {
            client_id: self.client_id,
            public_key: self.public_key,
            organization: self.organization,
            client_details: self.client_details,
            contact_email: administered.then_some(self.contact_email),
            status: self.status.into(),
            registered_at: self.registered_at,
            device_profile: self.device_profile,
            capabilities: self.capabilities,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RepairableClient {
    client_id: ClientId,
    public_key: PublicKeyHex,
    #[serde(default)]
    organization: Option<String>,
    #[serde(default)]
    client_details: Option<String>,
    #[serde(default)]
    contact_email: Option<String>,
    status: ClientStatus,
    registered_at: DateTime<Utc>,
    /// Needs no repair: every field is optional, and the write path validates
    /// `device_form_factor` before persisting.
    #[serde(flatten, default)]
    device_profile: DeviceProfile,
    #[serde(default)]
    capabilities: BTreeSet<String>,
}

impl RepairableClient {
    fn into_client(self) -> Result<Client, ClientRecordError> {
        let organization =
            optional_non_empty_string(self.organization, SELF_HEALED_ORGANIZATION.to_string())?;
        let client_details =
            optional_non_empty_string(self.client_details, SELF_HEALED_CLIENT_DETAILS.to_string())?;
        let contact_email = contact_email_or_self_healed(self.contact_email)?;

        Ok(Client {
            client_id: self.client_id,
            public_key: self.public_key,
            organization,
            client_details,
            contact_email,
            status: self.status,
            registered_at: self.registered_at,
            device_profile: self.device_profile,
            capabilities: self.capabilities,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientRecordError {
    #[error("invalid client record: {strict}; self-heal failed: {repair}")]
    RepairFailed {
        strict: serde_json::Error,
        repair: serde_json::Error,
    },
    #[error("self-heal produced an invalid fallback value: {0}")]
    InvalidFallback(#[from] EmptyStringError),
    #[error("self-heal produced an invalid fallback address: {0}")]
    InvalidFallbackEmail(#[from] ContactEmailError),
}

pub fn truncate_details(s: &NonEmptyTrimmedString) -> String {
    let truncated: String = s.as_str().chars().take(30).collect();
    if truncated.len() < s.as_str().len() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

pub fn parse_client_or_self_heal(data: &[u8]) -> Result<(Client, bool), ClientRecordError> {
    match serde_json::from_slice(data) {
        Ok(client) => Ok((client, false)),
        Err(original_error) => {
            let repairable: RepairableClient =
                serde_json::from_slice(data).map_err(|repair| ClientRecordError::RepairFailed {
                    strict: original_error,
                    repair,
                })?;
            Ok((repairable.into_client()?, true))
        }
    }
}

/// A stored address that no longer validates is **replaced**, not rejected:
/// callers drop a record they cannot parse, which would silently delete the
/// client from the fleet listing.
fn contact_email_or_self_healed(value: Option<String>) -> Result<ContactEmail, ContactEmailError> {
    if let Some(raw) = value
        && let Ok(email) = ContactEmail::try_new(raw)
    {
        return Ok(email);
    }
    ContactEmail::try_new(SELF_HEALED_CONTACT_EMAIL)
}

fn optional_non_empty_string(
    value: Option<String>,
    default: String,
) -> Result<NonEmptyTrimmedString, EmptyStringError> {
    if let Some(raw) = value
        && !raw.trim().is_empty()
    {
        return NonEmptyTrimmedString::try_new(raw);
    }
    NonEmptyTrimmedString::try_new(default)
}

/// A record that a client has presented a `v1` signature, written once. A
/// client with one is refused the timestamp-only fallback, so a signature
/// captured before it migrated authenticates nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureMigration {
    pub first_seen: DateTime<Utc>,
}

/// Whether recording a migration was the client's first, so that the transition
/// is logged once in the system's life rather than once per process that
/// observes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationRecord {
    /// This call wrote the marker: the client's first `v1` signature.
    First,
    /// A marker was already in place from an earlier request.
    Existing,
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use rstest::rstest;

    use crate::client::*;
    use crate::types::DeriveFromKey;

    #[test]
    fn test_derive_client_id() -> anyhow::Result<()> {
        let pk = PublicKeyHex::try_new(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        )?;
        let id = ClientId::derive(&pk);
        assert!(id.as_str().starts_with(ClientId::PREFIX));
        assert_eq!(id.as_str().len(), 4 + 64); // "ev1_" + 64 hex chars

        // Same input should produce same output
        let id2 = ClientId::derive(&pk);
        assert_eq!(id, id2);
        Ok(())
    }

    use pipette_plan_types::DeviceFormFactor;

    fn sample_client() -> anyhow::Result<Client> {
        Ok(Client {
            client_id: ClientId::try_new("ev1_dev")?,
            public_key: PublicKeyHex::try_new(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            )?,
            organization: NonEmptyTrimmedString::try_new("org")?,
            client_details: NonEmptyTrimmedString::try_new("details")?,
            contact_email: ContactEmail::try_new("a@b.com")?,
            status: ClientStatus::Approved,
            registered_at: "2026-01-01T00:00:00Z".parse()?,
            device_profile: DeviceProfile::default(),
            capabilities: Default::default(),
        })
    }

    #[test]
    fn test_device_profile_is_empty() -> anyhow::Result<()> {
        assert!(DeviceProfile::default().is_empty());

        let with_form_factor = DeviceProfile {
            device_form_factor: Some(DeviceFormFactor::Laptop),
            ..Default::default()
        };
        assert!(!with_form_factor.is_empty());

        // A numeric field alone is also enough to be non-empty.
        let with_ram = DeviceProfile {
            device_ram_bytes: Some(36_000_000_000),
            ..Default::default()
        };
        assert!(!with_ram.is_empty());
        Ok(())
    }

    #[test]
    fn test_empty_profile_omits_device_keys() -> anyhow::Result<()> {
        // `skip_serializing_if` keeps an empty profile from writing any
        // `device_*` keys, so stored records match the legacy shape.
        let raw = serde_json::to_value(sample_client()?)?;
        let obj = raw.as_object().context("expected object")?;
        assert!(
            obj.keys().all(|k| !k.starts_with("device_")),
            "empty profile should emit no device_* keys, got: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn test_client_device_profile_roundtrip_flat_keys() -> anyhow::Result<()> {
        let mut client = sample_client()?;
        client.device_profile = DeviceProfile {
            device_name: Some(NonEmptyTrimmedString::try_new("MacBook Pro")?),
            device_form_factor: Some(DeviceFormFactor::Laptop),
            device_ram_bytes: Some(36_000_000_000),
            device_gpu_model: Some(NonEmptyTrimmedString::try_new("M3 Pro GPU")?),
            device_gpu_vram_bytes: Some(18_000_000_000),
            ..Default::default()
        };

        let raw = serde_json::to_value(&client)?;
        // Flattened: the device fields are top-level, not nested under a
        // `device_profile` key.
        assert!(raw.get("device_profile").is_none());
        assert_eq!(raw["device_form_factor"], "laptop");
        assert_eq!(raw["device_ram_bytes"], 36_000_000_000_u64);

        let back: Client = serde_json::from_value(raw)?;
        assert_eq!(back.device_profile, client.device_profile);
        Ok(())
    }

    #[test]
    fn test_legacy_record_without_device_keys_deserializes_empty() -> anyhow::Result<()> {
        // A legacy record has no `device_*` keys; it must load with an empty
        // profile.
        let legacy = r#"{
  "client_id": "ev1_legacy",
  "public_key": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
  "organization": "org",
  "client_details": "details",
  "contact_email": "a@b.com",
  "status": "approved",
  "registered_at": "2026-01-01T00:00:00Z"
}"#;
        let client: Client = serde_json::from_str(legacy)?;
        assert!(client.device_profile.is_empty());
        Ok(())
    }

    /// The placeholder has to clear the same bar as a real address,
    /// or every repair turns into a `RepairFailed`.
    #[test]
    fn test_self_healed_contact_email_is_itself_valid() -> anyhow::Result<()> {
        ContactEmail::try_new(SELF_HEALED_CONTACT_EMAIL)?;
        Ok(())
    }

    /// `parse_client_or_self_heal`'s repair contract, pinned on the bytes. The
    /// `bool` matters: a caller persists the repair only when it is `true`.
    #[rstest]
    #[case::clean(
        serde_json::json!({}),
        false,
        "org",
        "details",
        "a@b.com"
    )]
    #[case::missing_organization(
        serde_json::json!({ "organization": serde_json::Value::Null }),
        true,
        SELF_HEALED_ORGANIZATION,
        "details",
        "a@b.com"
    )]
    #[case::empty_client_details(
        serde_json::json!({ "client_details": "" }),
        true,
        "org",
        SELF_HEALED_CLIENT_DETAILS,
        "a@b.com"
    )]
    #[case::whitespace_client_details(
        serde_json::json!({ "client_details": "   " }),
        true,
        "org",
        SELF_HEALED_CLIENT_DETAILS,
        "a@b.com"
    )]
    #[case::refused_contact_email(
        serde_json::json!({ "contact_email": "user@localhost" }),
        true,
        "org",
        "details",
        SELF_HEALED_CONTACT_EMAIL
    )]
    // The owner an earlier build kept on the record, before it moved to
    // `client-owners/`: ignored, not a repair.
    #[case::owner_from_an_earlier_build(
        serde_json::json!({
            "owner": { "type": "org", "org_id": "org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e" }
        }),
        false,
        "org",
        "details",
        "a@b.com"
    )]
    fn test_parse_client_or_self_heal(
        #[case] overrides: serde_json::Value,
        #[case] expect_repaired: bool,
        #[case] expect_organization: &str,
        #[case] expect_client_details: &str,
        #[case] expect_contact_email: &str,
    ) -> anyhow::Result<()> {
        let mut record = serde_json::json!({
            "client_id": "ev1_parse",
            "public_key": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "organization": "org",
            "client_details": "details",
            "contact_email": "a@b.com",
            "status": "approved",
            "registered_at": "2026-01-01T00:00:00Z",
        });
        let fields = overrides
            .as_object()
            .context("overrides must be a JSON object")?
            .clone();
        record
            .as_object_mut()
            .context("skeleton is a JSON object")?
            .extend(fields);

        let (client, repaired) = parse_client_or_self_heal(&serde_json::to_vec(&record)?)?;
        assert_eq!(repaired, expect_repaired);
        assert_eq!(client.organization.as_str(), expect_organization);
        assert_eq!(client.client_details.as_str(), expect_client_details);
        assert_eq!(client.contact_email.as_str(), expect_contact_email);
        Ok(())
    }

    /// A record the repair pass cannot parse either is an error, not a
    /// silently-defaulted client.
    #[test]
    fn test_parse_client_or_self_heal_refuses_unrepairable() -> anyhow::Result<()> {
        let record = serde_json::json!({
            "client_id": "ev1_parse",
            "public_key": "not-hex",
            "organization": "org",
            "client_details": "details",
            "contact_email": "a@b.com",
            "status": "approved",
            "registered_at": "2026-01-01T00:00:00Z",
        });
        assert!(parse_client_or_self_heal(&serde_json::to_vec(&record)?).is_err());
        Ok(())
    }

    #[test]
    fn test_normalized_flags_cover_every_populated_field() -> anyhow::Result<()> {
        let profile = DeviceProfile {
            device_name: Some(NonEmptyTrimmedString::try_new("iPhone 17 Pro")?),
            device_form_factor: Some(DeviceFormFactor::Phone),
            device_os_name: Some(NonEmptyTrimmedString::try_new("iOS")?),
            device_os_version: Some(NonEmptyTrimmedString::try_new("26.1")?),
            device_cpu_arch: Some(NonEmptyTrimmedString::try_new("aarch64")?),
            device_os_family: Some(NonEmptyTrimmedString::try_new("iPadOS")?),
            device_chip_model: Some(NonEmptyTrimmedString::try_new("Apple A19 Pro")?),
            device_ram_bytes: Some(8_000_000_000),
            device_gpu_model: Some(NonEmptyTrimmedString::try_new("Apple GPU")?),
            device_gpu_vram_bytes: Some(4_000_000_000),
            device_npu_model: Some(NonEmptyTrimmedString::try_new("Apple Neural Engine")?),
            device_npu_vram_bytes: Some(2_000_000_000),
        };
        let expected: BTreeSet<String> = [
            "device:iphone17pro",
            "form_factor:phone",
            "os:ios",
            "os_version:26.1",
            "arch:arm64",
            "os_family:ios",
            "chip:applea19pro",
            "ram_bytes:8000000000",
            "gpu:applegpu",
            "gpu_vram_bytes:4000000000",
            "npu:appleneuralengine",
            "npu_vram_bytes:2000000000",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(normalized_flags(&profile), expected);
        // An empty profile normalizes to nothing.
        assert!(normalized_flags(&DeviceProfile::default()).is_empty());

        // Drift guard: a full profile emits exactly the reserved namespaces, so a
        // `device_*` flag missing from the list (spoofable by a client) fails here.
        let flags = normalized_flags(&profile);
        let emitted: BTreeSet<&str> = flags
            .iter()
            .filter_map(|f| f.split_once(':').map(|(ns, _)| ns))
            .collect();
        let reserved: BTreeSet<&str> = RESERVED_CAPABILITY_NAMESPACES.iter().copied().collect();
        assert_eq!(emitted, reserved);
        Ok(())
    }

    #[rstest]
    #[case::amd64("amd64", "arch:x86_64")]
    #[case::x64("x64", "arch:x86_64")]
    #[case::x86_dash_64("x86-64", "arch:x86_64")]
    #[case::upper_x86_64("X86_64", "arch:x86_64")]
    #[case::x86_64("x86_64", "arch:x86_64")]
    #[case::aarch64("aarch64", "arch:arm64")]
    #[case::arm64("arm64", "arch:arm64")]
    #[case::arm64_v8a("arm64-v8a", "arch:arm64")]
    #[case::arm64e("arm64e", "arch:arm64")]
    #[case::riscv64("riscv64", "arch:riscv64")]
    fn canonical_arch_flag(#[case] value: &str, #[case] expected: &str) -> anyhow::Result<()> {
        let profile = DeviceProfile {
            device_cpu_arch: Some(NonEmptyTrimmedString::try_new(value)?),
            ..Default::default()
        };
        assert_eq!(
            normalized_flags(&profile),
            BTreeSet::from([expected.to_string()])
        );
        Ok(())
    }

    #[rstest]
    #[case::linux("Linux", "os_family:linux")]
    #[case::macos("macOS", "os_family:macos")]
    #[case::macosx("macOSX", "os_family:macos")]
    #[case::osx("OSX", "os_family:macos")]
    #[case::windows("Windows", "os_family:windows")]
    #[case::win32("win32", "os_family:windows")]
    #[case::win64("win64", "os_family:windows")]
    #[case::ios("iOS", "os_family:ios")]
    #[case::ipados("iPadOS", "os_family:ios")]
    #[case::android("Android", "os_family:android")]
    #[case::freebsd("freebsd", "os_family:freebsd")]
    fn canonical_os_family_flag(#[case] value: &str, #[case] expected: &str) -> anyhow::Result<()> {
        let profile = DeviceProfile {
            device_os_family: Some(NonEmptyTrimmedString::try_new(value)?),
            ..Default::default()
        };
        assert_eq!(
            normalized_flags(&profile),
            BTreeSet::from([expected.to_string()])
        );
        Ok(())
    }

    #[rstest]
    #[case::unknown(None, "os_family:macos")]
    #[case::laptop(Some(DeviceFormFactor::Laptop), "os_family:macos")]
    #[case::desktop(Some(DeviceFormFactor::Desktop), "os_family:macos")]
    #[case::phone(Some(DeviceFormFactor::Phone), "os_family:ios")]
    #[case::tablet(Some(DeviceFormFactor::Tablet), "os_family:ios")]
    fn darwin_os_family_follows_form_factor(
        #[case] form_factor: Option<DeviceFormFactor>,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let profile = DeviceProfile {
            device_os_family: Some(NonEmptyTrimmedString::try_new("Darwin")?),
            device_form_factor: form_factor,
            ..Default::default()
        };
        let family: BTreeSet<String> = normalized_flags(&profile)
            .into_iter()
            .filter(|flag| flag.starts_with("os_family:"))
            .collect();
        assert_eq!(family, BTreeSet::from([expected.to_string()]));
        Ok(())
    }

    /// A record's `device_*` fields normalize as a registered profile does, its
    /// build joins as a flag, and a null, empty or mistyped field adds none.
    #[test]
    fn recorded_run_flags_read_a_submission_record() {
        let record = serde_json::json!({
            "message_type": "success",
            "device_name": "SM-S948U1",
            "device_form_factor": "phone",
            "device_os_name": "Android",
            "device_os_version": "16",
            "device_cpu_arch": "aarch64",
            "device_os_family": "Android",
            "device_chip_model": "SM8850",
            "device_ram_bytes": 11_662_970_880u64,
            "device_gpu_model": null,
            "device_npu_model": "",
            "device_gpu_vram_bytes": "8",
            "client_version": "2026.10.05-E925778",
        });
        let expected: BTreeSet<String> = [
            "device:sm-s948u1",
            "form_factor:phone",
            "os:android",
            "os_version:16",
            "arch:arm64",
            "os_family:android",
            "chip:sm8850",
            "ram_bytes:11662970880",
            "client_version:2026.10.05-e925778",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(recorded_run_flags(&record), expected);
        assert!(recorded_run_flags(&serde_json::json!({"message_type": "failure"})).is_empty());
    }

    #[test]
    fn test_effective_capabilities_union() -> anyhow::Result<()> {
        let mut client = sample_client()?;
        client.device_profile = DeviceProfile {
            device_os_name: Some(NonEmptyTrimmedString::try_new("iOS")?),
            ..Default::default()
        };
        client.capabilities = BTreeSet::from(["runtime:llama_cpp".to_string()]);
        assert_eq!(
            client.effective_capabilities(),
            BTreeSet::from(["os:ios".to_string(), "runtime:llama_cpp".to_string()])
        );
        Ok(())
    }

    #[rstest]
    // Server-owned namespaces.
    #[case::os("os:ios", true)]
    #[case::arch("arch:arm64", true)]
    #[case::os_family("os_family:linux", true)]
    #[case::chip("chip:a19", true)]
    #[case::ram_bytes("ram_bytes:8", true)]
    #[case::gpu_vram_bytes("gpu_vram_bytes:4", true)]
    // Client-owned / free-form (including a versioned runtime with extra colons).
    #[case::runtime("runtime:llama_cpp", false)]
    #[case::runtime_versioned("runtime:llama_cpp:b9999", false)]
    #[case::no_namespace("job_retry", false)]
    // Boundaries: a bare token with no `:` has no namespace, and `os_version:`
    // is its own namespace distinct from `os:`.
    #[case::bare_token("os", false)]
    #[case::os_version_not_os("os_version:26.1", true)]
    // Operator-assigned, and still not reserved: reserved namespaces are
    // single-valued, and a client carries many tags.
    #[case::tag("tag:lab", false)]
    fn test_is_reserved_capability(#[case] flag: &str, #[case] expected: bool) {
        assert_eq!(is_reserved_capability(flag), expected, "flag {flag:?}");
    }

    #[rstest]
    #[case::tag("tag:lab", true)]
    // The namespace is the whole token before the first `:`.
    #[case::plural("tags:lab", false)]
    #[case::bare_token("tag", false)]
    #[case::value_named_tag("runtime:tag", false)]
    fn test_is_tag_capability(#[case] flag: &str, #[case] expected: bool) {
        assert_eq!(is_tag_capability(flag), expected, "flag {flag:?}");
    }

    /// The flag a tag matches as is one a client may not report, and is
    /// canonical, as every flag the matcher compares must be.
    #[test]
    fn test_tag_flag_is_a_canonical_tag_capability() -> anyhow::Result<()> {
        let flag = tag_flag(&Tag::try_new("lab-h100")?);
        assert_eq!(flag, "tag:lab-h100");
        assert!(is_tag_capability(&flag));
        assert_eq!(flag, slugify(&flag));
        Ok(())
    }

    #[test]
    fn test_capabilities_roundtrip_and_legacy_default() -> anyhow::Result<()> {
        // A populated set round-trips as a top-level `capabilities` array.
        let mut client = sample_client()?;
        client.capabilities = BTreeSet::from(["runtime:llama_cpp".to_string()]);
        let raw = serde_json::to_value(&client)?;
        assert_eq!(
            raw["capabilities"],
            serde_json::json!(["runtime:llama_cpp"])
        );
        let back: Client = serde_json::from_value(raw)?;
        assert_eq!(back.capabilities, client.capabilities);

        // An empty set emits no key (legacy shape), and a legacy record with no
        // `capabilities` key loads as an empty set.
        let empty = serde_json::to_value(sample_client()?)?;
        assert!(empty.get("capabilities").is_none());
        let legacy: Client = serde_json::from_value(empty)?;
        assert!(legacy.capabilities.is_empty());
        Ok(())
    }

    #[test]
    fn test_tags_never_serialized_into_record() -> anyhow::Result<()> {
        // Tags live in the marker trees, not the record — the client JSON must
        // never carry a `tags` key.
        let raw = serde_json::to_value(sample_client()?)?;
        assert!(raw.get("tags").is_none(), "record must not serialize tags");
        Ok(())
    }
}
