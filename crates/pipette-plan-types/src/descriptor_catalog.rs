//! Readable names for runtime descriptors and their ordered members.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use jiff::Timestamp;
use nutype::nutype;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::Digest;

use crate::principal::GLOBAL_CATALOG_QUALIFIER;
use crate::{
    GgufText, GgufTextSource, GgufVision, GgufVisionSource, GgufVisionSpeculative,
    GgufVisionSpeculativeSource, GitOid, HfRepo, IdError, Mlx, Model, ModelSource, ModelType,
    NonEmptyVec, OnnxGenai, Openvino, OrgId, OrgSlug, OrgSlugError, PrincipalIdError, Runtime,
    RuntimeType, Sha256, Torch, UserId, descriptor_digest, drop_vault_custom_labels,
    is_vault_source, names_vault_source,
};

/// The namespace that owns a catalog name. The database stores the global
/// scope as an empty string. Public interfaces use `global`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CatalogScope {
    Global,
    Org(OrgId),
}

#[cfg(feature = "schemars")]
impl schemars::JsonSchema for CatalogScope {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "CatalogScope".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "string" })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogParseError {
    #[error("{0}")]
    Principal(#[from] PrincipalIdError),
    #[error("unknown catalog creator {0:?}")]
    UnknownCreator(String),
}

impl CatalogScope {
    /// The word that names the global scope.
    pub const GLOBAL: &str = GLOBAL_CATALOG_QUALIFIER;

    pub fn sql_key(&self) -> &str {
        match self {
            Self::Global => "",
            Self::Org(id) => id.as_str(),
        }
    }

    pub fn from_sql_key(key: &str) -> Result<Self, CatalogParseError> {
        if key.is_empty() {
            Ok(Self::Global)
        } else {
            Ok(Self::Org(OrgId::try_new(key)?))
        }
    }

    pub fn org_id(&self) -> Option<&OrgId> {
        match self {
            Self::Global => None,
            Self::Org(id) => Some(id),
        }
    }
}

impl fmt::Display for CatalogScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => f.write_str(Self::GLOBAL),
            Self::Org(id) => id.fmt(f),
        }
    }
}

impl FromStr for CatalogScope {
    type Err = CatalogParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == Self::GLOBAL {
            Ok(Self::Global)
        } else {
            Ok(Self::Org(OrgId::try_new(s)?))
        }
    }
}

impl Serialize for CatalogScope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CatalogScope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A case-preserving catalog name. The store compares its lowercase key.
#[nutype(
    validate(with = validate_catalog_name, error = IdError),
    derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, AsRef, Display, Serialize, Deserialize, FromStr),
)]
pub struct CatalogName(String);

impl CatalogName {
    pub const MAX_LEN: usize = 200;
    /// The character and spacing rules for a catalog name in JSON Schema.
    pub const SCHEMA_PATTERN: &str = "^[A-Za-z0-9](?: ?[A-Za-z0-9._-])*$";

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }

    pub fn key(&self) -> String {
        self.as_str().to_ascii_lowercase()
    }
}

fn validate_catalog_name(s: &str) -> Result<(), IdError> {
    const FIELD: &str = "catalog name";
    const ALLOWED: &str = "[A-Za-z0-9._-] and single spaces between them; the first character must be a letter or a digit";
    if s.is_empty() {
        return Err(IdError::Empty { field: FIELD });
    }
    // A space needs a character before it and a non-space character after it.
    let followers = s.chars().skip(1).map(Some).chain([None]);
    if let Some(((_, ch), _)) = s.char_indices().zip(followers).find(|&((i, ch), next)| {
        let inner = i > 0;
        !(ch.is_ascii_alphanumeric()
            || (inner && matches!(ch, '.' | '_' | '-'))
            || (inner && ch == ' ' && next.is_some_and(|next| next != ' ')))
    }) {
        return Err(IdError::InvalidChar {
            field: FIELD,
            ch,
            allowed: ALLOWED,
        });
    }
    // Every character is ASCII here, so the byte length is the character count.
    if s.len() > CatalogName::MAX_LEN {
        return Err(IdError::Width {
            field: FIELD,
            expected: "at most 200 characters",
            got: s.len(),
        });
    }
    Ok(())
}

/// The URI of a model family's logo. It has one of two forms:
///
/// - A URI under [`LogoUri::PREFIX`], the one remote image source that the
///   web console's content security policy admits.
/// - A data URI that holds the image: `data:`, a media type of
///   [`LogoUri::DATA_MEDIA_TYPES`], `;base64,`, and then padded base64.
#[nutype(
    validate(with = validate_logo_uri, error = IdError),
    derive(Debug, Clone, PartialEq, Eq, Hash, AsRef, Display, Serialize, Deserialize, FromStr),
)]
pub struct LogoUri(String);

impl LogoUri {
    pub const PREFIX: &str = "https://pipette.liquid.ai/logos/";
    /// The image types that a logo data URI can have.
    pub const DATA_MEDIA_TYPES: [&str; 3] = ["image/png", "image/svg+xml", "image/webp"];
    pub const MAX_LEN: usize = 65_536;
    /// The rules of [`validate_logo_uri`] in JSON Schema.
    pub const SCHEMA_PATTERN: &str = "^(?:https://pipette\\.liquid\\.ai/logos/[A-Za-z0-9_-][A-Za-z0-9._-]*(?:/[A-Za-z0-9_-][A-Za-z0-9._-]*)*|data:image/(?:png|svg\\+xml|webp);base64,(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=|[A-Za-z0-9+/]{4}))$";

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }

    /// The lowercase hex SHA-256 of the URI's bytes. A catalog read gives
    /// this digest, and a client fetches the logo with it.
    pub fn sha256(&self) -> String {
        hex::encode(sha2::Sha256::digest(self.as_str().as_bytes()))
    }
}

/// A logo URI is [`LogoUri::PREFIX`] and a path, or a data URI. It has at
/// most [`LogoUri::MAX_LEN`] characters.
fn validate_logo_uri(s: &str) -> Result<(), IdError> {
    const FIELD: &str = "logo URI";
    if let Some(path) = s.strip_prefix(LogoUri::PREFIX) {
        validate_logo_path(path)?;
    } else if let Some(data) = s.strip_prefix("data:") {
        validate_logo_data(data)?;
    } else {
        return Err(IdError::Form {
            field: FIELD,
            expected: "a URI under https://pipette.liquid.ai/logos/ or a data URI",
        });
    }
    // Every character is ASCII here, so the byte length is the character count.
    if s.len() > LogoUri::MAX_LEN {
        return Err(IdError::Width {
            field: FIELD,
            expected: "at most 65536 characters",
            got: s.len(),
        });
    }
    Ok(())
}

/// Use `/` between path segments after [`LogoUri::PREFIX`]. A segment uses
/// the characters `[A-Za-z0-9._-]`. A segment does not start with `.`.
/// No segment is `..`, so the path stays in the logo folder.
fn validate_logo_path(path: &str) -> Result<(), IdError> {
    const ALLOWED: &str = "path segments of [A-Za-z0-9._-] separated by '/'; a segment must not be empty or start with '.'";
    if path.is_empty() {
        return Err(IdError::Empty {
            field: "logo URI path",
        });
    }
    let bad = path.split('/').find_map(|segment| {
        let mut chars = segment.chars();
        match chars.next() {
            None => Some('/'),
            Some(first) if first == '.' || !is_logo_char(first) => Some(first),
            Some(_) => chars.find(|&ch| !is_logo_char(ch)),
        }
    });
    match bad {
        Some(ch) => Err(IdError::InvalidChar {
            field: "logo URI",
            ch,
            allowed: ALLOWED,
        }),
        None => Ok(()),
    }
}

/// The part of a data URI after `data:` is a media type of
/// [`LogoUri::DATA_MEDIA_TYPES`], `;base64,`, and then base64 of the standard
/// alphabet. Use a nonempty base64 value with a length that is a multiple of
/// four. End the value with at most two `=` characters.
fn validate_logo_data(data: &str) -> Result<(), IdError> {
    let payload = data
        .split_once(";base64,")
        .filter(|(media_type, _)| LogoUri::DATA_MEDIA_TYPES.contains(media_type))
        .map(|(_, payload)| payload)
        .ok_or(IdError::Form {
            field: "logo data URI",
            expected: "data:image/png, data:image/svg+xml or data:image/webp, then ;base64,",
        })?;
    let digits = payload.trim_end_matches('=');
    let valid = !payload.is_empty()
        && payload.len() % 4 == 0
        && payload.len() - digits.len() <= 2
        && digits
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/'));
    if valid {
        Ok(())
    } else {
        Err(IdError::Form {
            field: "logo data URI",
            expected: "padded base64 of the standard alphabet after ;base64,",
        })
    }
}

fn is_logo_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')
}

/// The logo of a model family in one scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct FamilyLogo {
    pub scope: CatalogScope,
    /// The family in the case that the operator wrote. Lookup ignores case.
    pub family: CatalogName,
    pub logo_uri: LogoUri,
}

/// The digest of the logo of a model family in one scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyLogoDigest {
    pub scope: CatalogScope,
    /// The family in the case that the operator wrote. Lookup ignores case.
    pub family: CatalogName,
    /// [`LogoUri::sha256`] of the logo.
    pub sha256: Sha256,
}

/// The family logo digests of some scopes, for the readers of their entries.
#[derive(Debug, Default)]
pub struct FamilyLogos(HashMap<(CatalogScope, String), Sha256>);

impl FromIterator<FamilyLogoDigest> for FamilyLogos {
    fn from_iter<I: IntoIterator<Item = FamilyLogoDigest>>(logos: I) -> Self {
        Self(
            logos
                .into_iter()
                .map(|logo| ((logo.scope, logo.family.key()), logo.sha256))
                .collect(),
        )
    }
}

impl FamilyLogos {
    /// The logo digest of the family of `entry`: the digest of the entry's
    /// scope, or else the global digest. A runtime entry has no family, so it
    /// has no logo.
    pub fn of(&self, entry: &DescriptorEntry) -> Option<&Sha256> {
        let key = entry.family()?.to_ascii_lowercase();
        self.0
            .get(&(entry.scope.clone(), key.clone()))
            .or_else(|| self.0.get(&(CatalogScope::Global, key)))
    }
}

/// The scope part of a qualified catalog name: `global`, or an organization
/// slug.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CatalogQualifier {
    Global,
    Org(OrgSlug),
}

/// A catalog name as a reader writes it. A bare name follows the reader's
/// lookup order. A qualified name, `global/<name>` or `<org-slug>/<name>`,
/// selects one scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QualifiedCatalogName {
    pub qualifier: Option<CatalogQualifier>,
    pub name: CatalogName,
}

#[derive(Debug, thiserror::Error)]
pub enum QualifiedNameError {
    #[error(transparent)]
    Name(#[from] IdError),
    #[error(transparent)]
    Slug(#[from] OrgSlugError),
}

impl QualifiedCatalogName {
    /// The qualifier that selects the global scope.
    pub const GLOBAL: &str = CatalogScope::GLOBAL;
}

impl FromStr for QualifiedCatalogName {
    type Err = QualifiedNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((qualifier, name)) = s.split_once('/') else {
            return Ok(Self {
                qualifier: None,
                name: CatalogName::try_new(s)?,
            });
        };
        let qualifier = if qualifier.eq_ignore_ascii_case(Self::GLOBAL) {
            CatalogQualifier::Global
        } else {
            CatalogQualifier::Org(OrgSlug::try_new(qualifier)?)
        };
        Ok(Self {
            qualifier: Some(qualifier),
            name: CatalogName::try_new(name)?,
        })
    }
}

impl fmt::Display for QualifiedCatalogName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.qualifier {
            None => {}
            Some(CatalogQualifier::Global) => write!(f, "{}/", Self::GLOBAL)?,
            Some(CatalogQualifier::Org(slug)) => write!(f, "{slug}/")?,
        }
        self.name.fmt(f)
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum DescriptorEntryKind {
    Stable,
    Daily,
    HandMade,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum DescriptorEntryState {
    Pinned,
    Active,
    Hidden,
    Tombstoned,
}

/// Who created a catalog entry: an operator, an account, or one importer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptorEntryCreator {
    Operator,
    ImporterLlamacpp,
    Account(UserId),
}

#[cfg(feature = "schemars")]
impl schemars::JsonSchema for DescriptorEntryCreator {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "DescriptorEntryCreator".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "string" })
    }
}

impl fmt::Display for DescriptorEntryCreator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Operator => f.write_str("operator"),
            Self::ImporterLlamacpp => f.write_str("importer:llamacpp"),
            Self::Account(id) => write!(f, "account:{id}"),
        }
    }
}

impl FromStr for DescriptorEntryCreator {
    type Err = CatalogParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "operator" => Ok(Self::Operator),
            "importer:llamacpp" => Ok(Self::ImporterLlamacpp),
            _ => match s.strip_prefix("account:") {
                Some(id) => Ok(Self::Account(UserId::try_new(id)?)),
                None => Err(CatalogParseError::UnknownCreator(s.to_owned())),
            },
        }
    }
}

impl Serialize for DescriptorEntryCreator {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for DescriptorEntryCreator {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Which descriptor family an entry contains.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum DescriptorKind {
    Runtime,
    Model,
}

/// The file format of a model member. The model type gives the format.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ModelFormat {
    Gguf,
    Mlx,
    CoreAi,
    Torch,
    Openvino,
    Onnx,
    // A model that ships with the operating system, so it names no files.
    System,
}

impl ModelFormat {
    /// The format of a model type. A speculative type has the format of its
    /// base type.
    pub fn of(model_type: ModelType) -> Self {
        match model_type {
            ModelType::GgufText | ModelType::GgufVision | ModelType::GgufVisionSpeculative => {
                Self::Gguf
            }
            ModelType::Mlx | ModelType::MlxSpeculative => Self::Mlx,
            ModelType::AppleCoreAi => Self::CoreAi,
            ModelType::Torch | ModelType::TorchSpeculative => Self::Torch,
            ModelType::Openvino => Self::Openvino,
            ModelType::OnnxGenai => Self::Onnx,
            ModelType::AppleFoundationText => Self::System,
        }
    }
}

/// The quantization of a model member, as the operator writes it, for example
/// `q4_0` or `4bit`. One `@` can name the source of the quantization, for
/// example `q4_k_m@unsloth`.
#[nutype(
    validate(regex = r"^[a-z0-9_]+(@[a-z0-9_]+)?$"),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        AsRef,
        Display,
        Serialize,
        Deserialize,
        FromStr
    )
)]
pub struct QuantizationLabel(String);

impl QuantizationLabel {
    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

/// The parts that identify one member within an entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DescriptorMemberKey {
    Runtime {
        flavor: Option<String>,
    },
    Model {
        format: ModelFormat,
        #[cfg_attr(feature = "schemars", schemars(with = "String"))]
        quantization: QuantizationLabel,
    },
}

/// The descriptor carried by one catalog member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum DescriptorMemberDescriptor {
    Runtime(Runtime),
    Model(Model),
}

#[derive(Debug, thiserror::Error)]
pub enum MemberError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("The member digest does not match its descriptor.")]
    DigestMismatch,
    #[error("The member key does not match its descriptor.")]
    KeyMismatch,
    #[error("each source of a model member must be a Hugging Face repository")]
    NotHuggingFace,
    #[error("{0} must pin a commit: a revision of 40 lowercase hex characters")]
    UnpinnedRevision(String),
    #[error("a model member must not carry an access token")]
    AuthToken,
}

fn whole_micros(at: Timestamp) -> Timestamp {
    at - jiff::SignedDuration::from_nanos(i64::from(at.subsec_nanosecond() % 1_000))
}

/// One immutable descriptor at its position in the entry. `added_at` keeps
/// whole microseconds, what a SQL timestamp holds, so a member round-trips
/// equal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(try_from = "MemberWire", into = "MemberWire")]
pub struct DescriptorMember {
    body: MemberBody,
    digest: String,
    alias_digest: Option<String>,
    /// The download size in bytes for a model member. The digest excludes
    /// this value. The member's JSON representation also omits it.
    size_bytes: Option<u64>,
    added_at: Timestamp,
}

/// A member's descriptor with the key that the descriptor gives.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MemberBody {
    Runtime {
        runtime: Runtime,
        flavor: Option<String>,
    },
    Model {
        model: Model,
        format: ModelFormat,
        quantization: QuantizationLabel,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(untagged)]
enum MemberWire {
    Runtime {
        runtime: Runtime,
        digest: String,
        flavor: Option<String>,
        #[cfg_attr(feature = "schemars", schemars(with = "String", extend("format" = "date-time")))]
        added_at: Timestamp,
    },
    Model {
        model: Model,
        format: ModelFormat,
        #[cfg_attr(feature = "schemars", schemars(with = "String"))]
        quantization: QuantizationLabel,
        digest: String,
        #[cfg_attr(feature = "schemars", schemars(with = "String", extend("format" = "date-time")))]
        added_at: Timestamp,
    },
}

impl TryFrom<MemberWire> for DescriptorMember {
    type Error = MemberError;
    fn try_from(wire: MemberWire) -> Result<Self, Self::Error> {
        match wire {
            MemberWire::Runtime {
                runtime,
                digest,
                flavor,
                added_at,
            } => Self::verified(
                DescriptorMemberDescriptor::Runtime(runtime),
                &digest,
                &DescriptorMemberKey::Runtime { flavor },
                added_at,
            ),
            MemberWire::Model {
                model,
                format,
                quantization,
                digest,
                added_at,
            } => Self::verified(
                DescriptorMemberDescriptor::Model(model),
                &digest,
                &DescriptorMemberKey::Model {
                    format,
                    quantization,
                },
                added_at,
            ),
        }
    }
}

impl From<DescriptorMember> for MemberWire {
    fn from(member: DescriptorMember) -> Self {
        let DescriptorMember {
            body,
            digest,
            added_at,
            ..
        } = member;
        match body {
            MemberBody::Runtime { runtime, flavor } => Self::Runtime {
                runtime,
                digest,
                flavor,
                added_at,
            },
            MemberBody::Model {
                model,
                format,
                quantization,
            } => Self::Model {
                model,
                format,
                quantization,
                digest,
                added_at,
            },
        }
    }
}

/// At least one model source, and every source is a vault source.
fn only_vault_sources(value: &serde_json::Value) -> bool {
    fn walk(value: &serde_json::Value, found: &mut bool) -> bool {
        match value {
            serde_json::Value::Object(fields) => {
                if fields
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .is_some()
                {
                    *found = true;
                    if !is_vault_source(fields) {
                        return false;
                    }
                }
                fields.values().all(|value| walk(value, found))
            }
            serde_json::Value::Array(items) => items.iter().all(|value| walk(value, found)),
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => true,
        }
    }
    let mut found = false;
    walk(value, &mut found) && found
}

/// Whether the iOS app builds in runtimes of this type.
fn is_app_runtime(runtime_type: RuntimeType) -> bool {
    matches!(
        runtime_type,
        RuntimeType::LlamacppIosPipette
            | RuntimeType::MlxIosPipette
            | RuntimeType::OnnxGenaiIosPipette
    )
}

fn build_flag(digest: &str) -> String {
    format!("runtime:descriptor:{digest}")
}

impl DescriptorMember {
    pub fn runtime(&self) -> Option<&Runtime> {
        match &self.body {
            MemberBody::Runtime { runtime, .. } => Some(runtime),
            MemberBody::Model { .. } => None,
        }
    }
    pub fn model(&self) -> Option<&Model> {
        match &self.body {
            MemberBody::Runtime { .. } => None,
            MemberBody::Model { model, .. } => Some(model),
        }
    }
    pub fn descriptor(&self) -> DescriptorMemberDescriptor {
        match &self.body {
            MemberBody::Runtime { runtime, .. } => {
                DescriptorMemberDescriptor::Runtime(runtime.clone())
            }
            MemberBody::Model { model, .. } => DescriptorMemberDescriptor::Model(model.clone()),
        }
    }
    pub fn key(&self) -> DescriptorMemberKey {
        match &self.body {
            MemberBody::Runtime { flavor, .. } => DescriptorMemberKey::Runtime {
                flavor: flavor.clone(),
            },
            MemberBody::Model {
                format,
                quantization,
                ..
            } => DescriptorMemberKey::Model {
                format: *format,
                quantization: quantization.clone(),
            },
        }
    }
    pub fn descriptor_kind(&self) -> DescriptorKind {
        match &self.body {
            MemberBody::Runtime { .. } => DescriptorKind::Runtime,
            MemberBody::Model { .. } => DescriptorKind::Model,
        }
    }
    /// The runtime type or the model type, as the descriptor's `type` field specifies it.
    pub fn descriptor_type(&self) -> String {
        match &self.body {
            MemberBody::Runtime { runtime, .. } => RuntimeType::of(runtime).to_string(),
            MemberBody::Model { model, .. } => ModelType::of(model).to_string(),
        }
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    /// The build flag that a device must report to run this member, if the
    /// iOS app builds in its runtime. It equals
    /// [`crate::runtime_build_flag`] of the member's runtime.
    pub fn app_build_flag(&self) -> Option<String> {
        self.runtime()
            .filter(|runtime| is_app_runtime(RuntimeType::of(runtime)))
            .map(|_| build_flag(&self.digest))
    }
    /// The verified Hugging Face alias of a vault model, when every source has one.
    pub fn alias_digest(&self) -> Option<&str> {
        self.alias_digest.as_deref()
    }
    /// Attach an alias whose origin the caller verified against the vault.
    pub fn with_alias_digest(mut self, alias_digest: Option<String>) -> Self {
        self.alias_digest = alias_digest;
        self
    }
    /// The bytes that a client downloads for this model member, if the
    /// server recorded them.
    pub fn size_bytes(&self) -> Option<u64> {
        self.size_bytes
    }
    /// Attach the download size that the caller read at the pinned commit.
    pub fn with_size_bytes(mut self, size_bytes: Option<u64>) -> Self {
        self.size_bytes = size_bytes;
        self
    }
    /// The same model member with the label `quantization`. The label is not
    /// part of the digest, so the digest stays the same. A runtime member has
    /// no label, so the result is `None` for it.
    pub fn relabeled(&self, quantization: QuantizationLabel) -> Option<Self> {
        match &self.body {
            MemberBody::Runtime { .. } => None,
            MemberBody::Model { .. } => {
                let mut member = self.clone();
                if let MemberBody::Model {
                    quantization: label,
                    ..
                } = &mut member.body
                {
                    *label = quantization;
                }
                Some(member)
            }
        }
    }
    /// The flavor of a runtime member. A model member has none.
    pub fn flavor(&self) -> Option<&str> {
        match &self.body {
            MemberBody::Runtime { flavor, .. } => flavor.as_deref(),
            MemberBody::Model { .. } => None,
        }
    }
    /// The format and the quantization of a model member.
    pub fn model_key(&self) -> Option<(ModelFormat, &QuantizationLabel)> {
        match &self.body {
            MemberBody::Runtime { .. } => None,
            MemberBody::Model {
                format,
                quantization,
                ..
            } => Some((*format, quantization)),
        }
    }
    pub fn added_at(&self) -> Timestamp {
        self.added_at
    }

    pub fn new(runtime: Runtime, added_at: Timestamp) -> Result<Self, MemberError> {
        let mut value = serde_json::to_value(&runtime)?;
        let flavor = value
            .get("flavor")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        drop_vault_custom_labels(&mut value);
        let digest = descriptor_digest(&value)?;
        Ok(Self {
            body: MemberBody::Runtime { runtime, flavor },
            digest,
            alias_digest: None,
            size_bytes: None,
            added_at: whole_micros(added_at),
        })
    }

    /// A model member. Hugging Face sources pin a commit and carry no access
    /// token. A vault model is checked against its org owner by the writer.
    pub fn new_model(
        model: Model,
        quantization: QuantizationLabel,
        added_at: Timestamp,
    ) -> Result<Self, MemberError> {
        let model_type = ModelType::of(&model);
        let format = ModelFormat::of(model_type);
        let value = serde_json::to_value(&model)?;
        if only_vault_sources(&value) {
            // The writer checks each resource's org owner and its record.
        } else {
            hf_repos(&model)
                .ok_or(MemberError::NotHuggingFace)?
                .into_iter()
                .try_for_each(|repo| {
                    if repo.auth_token.is_some() {
                        return Err(MemberError::AuthToken);
                    }
                    match &repo.revision {
                        Some(revision) if GitOid::try_new(revision.as_ref()).is_ok() => Ok(()),
                        _ => Err(MemberError::UnpinnedRevision(repo.reference())),
                    }
                })?;
        }
        let mut identity = value;
        drop_vault_custom_labels(&mut identity);
        let digest = descriptor_digest(&identity)?;
        Ok(Self {
            body: MemberBody::Model {
                model,
                format,
                quantization,
            },
            digest,
            alias_digest: None,
            size_bytes: None,
            added_at: whole_micros(added_at),
        })
    }

    /// A member rebuilt from its stored parts. The digest and the key must be
    /// the ones that `descriptor` gives.
    pub fn verified(
        descriptor: DescriptorMemberDescriptor,
        digest: &str,
        key: &DescriptorMemberKey,
        added_at: Timestamp,
    ) -> Result<Self, MemberError> {
        let member = match (descriptor, key) {
            (DescriptorMemberDescriptor::Runtime(runtime), _) => Self::new(runtime, added_at)?,
            (
                DescriptorMemberDescriptor::Model(model),
                DescriptorMemberKey::Model { quantization, .. },
            ) => Self::new_model(model, quantization.clone(), added_at)?,
            (DescriptorMemberDescriptor::Model(_), DescriptorMemberKey::Runtime { .. }) => {
                return Err(MemberError::KeyMismatch);
            }
        };
        if member.digest != digest {
            return Err(MemberError::DigestMismatch);
        }
        if member.key() != *key {
            return Err(MemberError::KeyMismatch);
        }
        Ok(member)
    }
}

/// Every Hugging Face repository that `model` names, or `None` if a source is
/// not a Hugging Face repository.
pub fn hf_repos(model: &Model) -> Option<Vec<&HfRepo>> {
    fn directory(source: &ModelSource) -> Option<&HfRepo> {
        match source {
            ModelSource::HuggingFace { repo, .. } => Some(repo),
            ModelSource::VaultArchive { .. }
            | ModelSource::RelativeDir { .. }
            | ModelSource::AbsoluteDir { .. } => None,
        }
    }
    match model {
        Model::GgufText(GgufText { source }) => match source {
            GgufTextSource::HuggingFace { repo, .. } => Some(vec![repo]),
            GgufTextSource::RelativeFile { .. }
            | GgufTextSource::AbsoluteFile { .. }
            | GgufTextSource::Url { .. }
            | GgufTextSource::VaultUrl { .. } => None,
        },
        Model::GgufVision(GgufVision { source }) => match source {
            GgufVisionSource::HuggingFace(files) => Some(vec![&files.repo]),
            GgufVisionSource::RelativeFiles(_)
            | GgufVisionSource::AbsoluteFiles(_)
            | GgufVisionSource::Url(_)
            | GgufVisionSource::VaultUrl(_) => None,
        },
        Model::GgufVisionSpeculative(GgufVisionSpeculative { source }) => match source {
            GgufVisionSpeculativeSource::HuggingFace { files, .. } => Some(vec![&files.repo]),
            GgufVisionSpeculativeSource::RelativeFiles { .. }
            | GgufVisionSpeculativeSource::AbsoluteFiles { .. }
            | GgufVisionSpeculativeSource::Url { .. }
            | GgufVisionSpeculativeSource::VaultUrl { .. } => None,
        },
        Model::Mlx(Mlx { source })
        | Model::AppleCoreAi(crate::AppleCoreAi { source })
        | Model::Torch(Torch { source })
        | Model::Openvino(Openvino { source })
        | Model::OnnxGenai(OnnxGenai { source }) => directory(source).map(|repo| vec![repo]),
        Model::MlxSpeculative(pair) | Model::TorchSpeculative(pair) => {
            pair.sources().into_iter().map(directory).collect()
        }
        Model::AppleFoundationText => Some(Vec::new()),
    }
}

/// [`hf_repos`] with each repository open to change, so a caller can pin its
/// revision.
pub fn hf_repos_mut(model: &mut Model) -> Option<Vec<&mut HfRepo>> {
    fn directory(source: &mut ModelSource) -> Option<&mut HfRepo> {
        match source {
            ModelSource::HuggingFace { repo, .. } => Some(repo),
            ModelSource::VaultArchive { .. }
            | ModelSource::RelativeDir { .. }
            | ModelSource::AbsoluteDir { .. } => None,
        }
    }
    match model {
        Model::GgufText(GgufText { source }) => match source {
            GgufTextSource::HuggingFace { repo, .. } => Some(vec![repo]),
            GgufTextSource::RelativeFile { .. }
            | GgufTextSource::AbsoluteFile { .. }
            | GgufTextSource::Url { .. }
            | GgufTextSource::VaultUrl { .. } => None,
        },
        Model::GgufVision(GgufVision { source }) => match source {
            GgufVisionSource::HuggingFace(files) => Some(vec![&mut files.repo]),
            GgufVisionSource::RelativeFiles(_)
            | GgufVisionSource::AbsoluteFiles(_)
            | GgufVisionSource::Url(_)
            | GgufVisionSource::VaultUrl(_) => None,
        },
        Model::GgufVisionSpeculative(GgufVisionSpeculative { source }) => match source {
            GgufVisionSpeculativeSource::HuggingFace { files, .. } => Some(vec![&mut files.repo]),
            GgufVisionSpeculativeSource::RelativeFiles { .. }
            | GgufVisionSpeculativeSource::AbsoluteFiles { .. }
            | GgufVisionSpeculativeSource::Url { .. }
            | GgufVisionSpeculativeSource::VaultUrl { .. } => None,
        },
        Model::Mlx(Mlx { source })
        | Model::AppleCoreAi(crate::AppleCoreAi { source })
        | Model::Torch(Torch { source })
        | Model::Openvino(Openvino { source })
        | Model::OnnxGenai(OnnxGenai { source }) => directory(source).map(|repo| vec![repo]),
        Model::MlxSpeculative(pair) | Model::TorchSpeculative(pair) => {
            pair.sources_mut().into_iter().map(directory).collect()
        }
        Model::AppleFoundationText => Some(Vec::new()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EntryError {
    #[error("a new entry must not start in the tombstoned state")]
    Tombstoned,
    #[error("an organization catalog entry must be hand-made")]
    OrgKind,
    #[error("a model catalog entry must be hand-made")]
    ModelKind,
    #[error(
        "an importer cannot create an organization entry, and an account cannot create a global entry"
    )]
    CreatorScope,
    #[error("a global catalog entry cannot name a vault source")]
    GlobalVaultSource,
    #[error("cannot inspect a member descriptor: {0}")]
    DescriptorInvalid(String),
    #[error("members mix runtime and model descriptors")]
    DescriptorKindMismatch,
    #[error("members have different runtime types")]
    RuntimeTypeMismatch,
    #[error("members of one catalog entry may differ only in flavor")]
    RuntimeBuildMismatch,
    #[error("cannot inspect a runtime build: {0}")]
    RuntimeBuildInvalid(String),
    #[error("duplicate member flavor: {0}")]
    DuplicateFlavor(String),
    #[error("duplicate member format and quantization: {format}::{quantization}")]
    DuplicateModelKey {
        format: ModelFormat,
        quantization: QuantizationLabel,
    },
    #[error("model types {first} and {second} share the format {format}")]
    FormatTypeClash {
        format: ModelFormat,
        first: ModelType,
        second: ModelType,
    },
    #[error("members mix speculative and plain model types")]
    SpeculativeMix,
    #[error("duplicate member digest: {0}")]
    DuplicateDigest(String),
    #[error("only a model catalog entry has a family")]
    RuntimeFamily,
}

/// The family of a model entry with no override: the text of `name` before
/// its first hyphen, or the whole name if it has no hyphen.
fn default_family(name: &CatalogName) -> &str {
    let name = name.as_str();
    name.split_once('-').map_or(name, |(head, _)| head)
}

/// A named, ordered sequence of descriptors of one kind: runtimes of one
/// runtime type, or models. The times keep whole microseconds, what a SQL
/// timestamp holds, so an entry round-trips equal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(try_from = "EntryWire")]
pub struct DescriptorEntry {
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    pub scope: CatalogScope,
    pub name: CatalogName,
    pub kind: DescriptorEntryKind,
    pub state: DescriptorEntryState,
    #[cfg_attr(feature = "schemars", schemars(with = "String", extend("format" = "date-time")))]
    pub release_time: Timestamp,
    #[cfg_attr(feature = "schemars", schemars(with = "String", extend("format" = "date-time")))]
    pub created_at: Timestamp,
    pub created_by: DescriptorEntryCreator,
    pub members: NonEmptyVec<DescriptorMember>,
    /// The family that an operator set for a model entry. A runtime entry has
    /// none. [`DescriptorEntry::family`] gives the family that readers see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    pub family: Option<CatalogName>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
struct EntryWire {
    #[cfg_attr(feature = "schemars", schemars(with = "String"))]
    scope: CatalogScope,
    name: CatalogName,
    kind: DescriptorEntryKind,
    state: DescriptorEntryState,
    #[cfg_attr(feature = "schemars", schemars(with = "String", extend("format" = "date-time")))]
    release_time: Timestamp,
    #[cfg_attr(feature = "schemars", schemars(with = "String", extend("format" = "date-time")))]
    created_at: Timestamp,
    created_by: DescriptorEntryCreator,
    members: NonEmptyVec<DescriptorMember>,
    #[serde(default)]
    #[cfg_attr(feature = "schemars", schemars(with = "Option<String>"))]
    family: Option<CatalogName>,
}

impl TryFrom<EntryWire> for DescriptorEntry {
    type Error = EntryError;
    fn try_from(wire: EntryWire) -> Result<Self, Self::Error> {
        Self::validate_identity(
            &wire.scope,
            wire.kind,
            &wire.created_by,
            wire.members.head().descriptor_kind(),
        )?;
        Self::validate_members(&wire.members)?;
        Self::validate_scope_members(&wire.scope, &wire.members)?;
        Self {
            scope: wire.scope,
            name: wire.name,
            kind: wire.kind,
            state: wire.state,
            release_time: whole_micros(wire.release_time),
            created_at: whole_micros(wire.created_at),
            created_by: wire.created_by,
            members: wire.members,
            family: None,
        }
        .with_family(wire.family)
    }
}

impl DescriptorEntry {
    #[allow(
        clippy::too_many_arguments,
        reason = "scope, identity, timestamps, and members are all required for validation"
    )]
    pub fn new(
        scope: CatalogScope,
        name: CatalogName,
        kind: DescriptorEntryKind,
        state: DescriptorEntryState,
        release_time: Timestamp,
        created_at: Timestamp,
        created_by: DescriptorEntryCreator,
        members: NonEmptyVec<DescriptorMember>,
    ) -> Result<Self, EntryError> {
        if state == DescriptorEntryState::Tombstoned {
            return Err(EntryError::Tombstoned);
        }
        Self::validate_identity(&scope, kind, &created_by, members.head().descriptor_kind())?;
        Self::validate_members(&members)?;
        Self::validate_scope_members(&scope, &members)?;
        Ok(Self {
            scope,
            name,
            kind,
            state,
            release_time: whole_micros(release_time),
            created_at: whole_micros(created_at),
            created_by,
            members,
            family: None,
        })
    }

    /// Set or clear the family override. A runtime entry refuses a family.
    pub fn with_family(mut self, family: Option<CatalogName>) -> Result<Self, EntryError> {
        if family.is_some() && self.descriptor_kind() != DescriptorKind::Model {
            return Err(EntryError::RuntimeFamily);
        }
        self.family = family;
        Ok(self)
    }

    /// The family that readers see for a model entry: the operator's
    /// override, or else the text of the name before its first hyphen. A
    /// runtime entry has no family.
    pub fn family(&self) -> Option<&str> {
        if self.descriptor_kind() != DescriptorKind::Model {
            return None;
        }
        Some(match &self.family {
            Some(family) => family.as_str(),
            None => default_family(&self.name),
        })
    }

    pub fn descriptor_kind(&self) -> DescriptorKind {
        self.members.head().descriptor_kind()
    }

    /// Refuse a vault source in a global entry. The writer checks the owner
    /// of each vault resource, and a read of a stored entry does not.
    pub fn validate_scope_members(
        scope: &CatalogScope,
        members: &NonEmptyVec<DescriptorMember>,
    ) -> Result<(), EntryError> {
        if !matches!(scope, CatalogScope::Global) {
            return Ok(());
        }
        members.iter().try_for_each(|member| {
            let value = match &member.body {
                MemberBody::Runtime { runtime, .. } => serde_json::to_value(runtime),
                MemberBody::Model { model, .. } => serde_json::to_value(model),
            }
            .map_err(|error| EntryError::DescriptorInvalid(error.to_string()))?;
            if names_vault_source(&value) {
                Err(EntryError::GlobalVaultSource)
            } else {
                Ok(())
            }
        })
    }

    /// The descriptor types of the members, each once, in member order. A
    /// runtime entry has one type. A model entry has one type for each format.
    pub fn descriptor_types(&self) -> Vec<String> {
        self.members
            .iter()
            .map(DescriptorMember::descriptor_type)
            .fold(Vec::new(), |mut types, found| {
                if !types.contains(&found) {
                    types.push(found);
                }
                types
            })
    }

    pub fn validate_identity(
        scope: &CatalogScope,
        kind: DescriptorEntryKind,
        creator: &DescriptorEntryCreator,
        descriptor_kind: DescriptorKind,
    ) -> Result<(), EntryError> {
        if matches!(scope, CatalogScope::Org(_)) && kind != DescriptorEntryKind::HandMade {
            return Err(EntryError::OrgKind);
        }
        if descriptor_kind == DescriptorKind::Model && kind != DescriptorEntryKind::HandMade {
            return Err(EntryError::ModelKind);
        }
        if matches!(
            (scope, creator),
            (
                CatalogScope::Org(_),
                DescriptorEntryCreator::ImporterLlamacpp
            ) | (CatalogScope::Global, DescriptorEntryCreator::Account(_))
        ) {
            return Err(EntryError::CreatorScope);
        }
        Ok(())
    }

    /// Check that runtime members describe one build, allowing different flavors.
    /// Model members have no runtime build to compare.
    pub fn validate_runtime_build(
        members: &NonEmptyVec<DescriptorMember>,
    ) -> Result<(), EntryError> {
        let Some(first) = members.head().runtime() else {
            return Ok(());
        };
        let identity = |runtime: &Runtime| {
            let mut value = serde_json::to_value(runtime)
                .map_err(|error| EntryError::RuntimeBuildInvalid(error.to_string()))?;
            value
                .as_object_mut()
                .ok_or_else(|| {
                    EntryError::RuntimeBuildInvalid("descriptor is not an object".into())
                })?
                .remove("flavor");
            drop_vault_custom_labels(&mut value);
            Ok::<_, EntryError>(value)
        };
        let first = identity(first)?;
        members.iter().skip(1).try_for_each(|member| {
            let runtime = member.runtime().ok_or(EntryError::DescriptorKindMismatch)?;
            if identity(runtime)? == first {
                Ok(())
            } else {
                Err(EntryError::RuntimeBuildMismatch)
            }
        })
    }

    /// Check that the members can share one entry:
    ///
    /// - every member is a runtime, or every member is a model;
    /// - runtime members share one runtime type and differ in flavor;
    /// - model members are all speculative or all plain, use one model type for
    ///   each format, and differ in format or quantization;
    /// - no two members have the same digest.
    pub fn validate_members(members: &NonEmptyVec<DescriptorMember>) -> Result<(), EntryError> {
        let head = members.head();
        let mut flavors = HashSet::new();
        let mut model_keys = HashSet::new();
        let mut format_types = HashMap::new();
        let mut digests = HashSet::new();
        members.iter().try_for_each(|member| {
            match (&head.body, &member.body) {
                (
                    MemberBody::Runtime { runtime: first, .. },
                    MemberBody::Runtime { runtime, flavor },
                ) => {
                    if RuntimeType::of(runtime) != RuntimeType::of(first) {
                        return Err(EntryError::RuntimeTypeMismatch);
                    }
                    if let Some(flavor) = flavor
                        && !flavors.insert(flavor)
                    {
                        return Err(EntryError::DuplicateFlavor(flavor.clone()));
                    }
                }
                (
                    MemberBody::Model { model: first, .. },
                    MemberBody::Model {
                        model,
                        format,
                        quantization,
                    },
                ) => {
                    let model_type = ModelType::of(model);
                    if model_type.is_speculative() != ModelType::of(first).is_speculative() {
                        return Err(EntryError::SpeculativeMix);
                    }
                    let known = *format_types.entry(*format).or_insert(model_type);
                    if known != model_type {
                        return Err(EntryError::FormatTypeClash {
                            format: *format,
                            first: known,
                            second: model_type,
                        });
                    }
                    if !model_keys.insert((*format, quantization)) {
                        return Err(EntryError::DuplicateModelKey {
                            format: *format,
                            quantization: quantization.clone(),
                        });
                    }
                }
                (MemberBody::Runtime { .. }, MemberBody::Model { .. })
                | (MemberBody::Model { .. }, MemberBody::Runtime { .. }) => {
                    return Err(EntryError::DescriptorKindMismatch);
                }
            }
            if !digests.insert(&member.digest) {
                return Err(EntryError::DuplicateDigest(member.digest.clone()));
            }
            Ok(())
        })
    }
}

/// The catalog names of one descriptor digest, as one reader sees them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct DescriptorDisplay {
    /// The member selector that a row shows, for example
    /// `llamacpp-v0.7.0::linux-cuda-12-8`. It comes from the name with the
    /// highest rank.
    pub display: String,
    /// The entry's name in `display`, as the reader sees it: bare,
    /// `global/<name>`, or `<org-slug>/<name>`. The member key follows it in
    /// `display`.
    pub name: String,
    /// The member selectors of the other names, in rank order.
    pub synonyms: Vec<String>,
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use anyhow::Context;
    use rstest::rstest;
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    use super::*;

    fn apple() -> Value {
        json!({"type": "apple_foundation"})
    }

    fn llama(asset: Option<&str>) -> Value {
        let mut value = json!({
            "type": "llamacpp_cli_stock_tools", "source": "github_release",
            "repository_url": "github.com/ggml-org/llama.cpp", "repository_version": "b1",
            "flavor": "linux-x64-rocm"
        });
        if let (Some(asset), Some(fields)) = (asset, value.as_object_mut()) {
            fields.insert("asset".into(), asset.into());
        }
        value
    }

    fn a_member(runtime: Value, at: Timestamp) -> anyhow::Result<DescriptorMember> {
        Ok(DescriptorMember::new(serde_json::from_value(runtime)?, at)?)
    }

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn hf(repo: &str, revision: Option<&str>) -> Value {
        let mut value = json!({"source": "huggingface", "org": "acme", "repo_name": repo});
        if let (Some(revision), Some(fields)) = (revision, value.as_object_mut()) {
            fields.insert("revision".into(), revision.into());
        }
        value
    }

    /// A model of `model_type` from one pinned repository. A speculative pair
    /// takes the same repository as its target and its draft.
    fn pinned(model_type: &str, repo: &str) -> Value {
        let source = hf(repo, Some(COMMIT));
        let files = match model_type {
            "mlx_speculative" | "torch_speculative" => {
                return json!({"type": model_type, "target": source, "draft": source});
            }
            "gguf_text" => json!({"path": "model.gguf"}),
            "gguf_vision" | "gguf_vision_speculative" => {
                json!({"model": "model.gguf", "mmproj": "mmproj.gguf"})
            }
            _ => json!({}),
        };
        let mut value = json!({"type": model_type});
        if let Some(fields) = value.as_object_mut() {
            [source, files]
                .iter()
                .filter_map(Value::as_object)
                .for_each(|parts| fields.extend(parts.clone()));
        }
        value
    }

    fn a_model(
        model: Value,
        quantization: &str,
        at: Timestamp,
    ) -> anyhow::Result<DescriptorMember> {
        Ok(DescriptorMember::new_model(
            serde_json::from_value(model)?,
            QuantizationLabel::try_new(quantization)?,
            at,
        )?)
    }

    #[rstest]
    #[case("my-fork", None, "my-fork")]
    #[case("global/my-fork", Some("global"), "global/my-fork")]
    #[case("GLOBAL/my-fork", Some("global"), "global/my-fork")]
    #[case("acme/my-fork", Some("acme"), "acme/my-fork")]
    #[case("Acme/my-fork", Some("acme"), "acme/my-fork")]
    fn qualified_name_parses_and_displays(
        #[case] text: &str,
        #[case] qualifier: Option<&str>,
        #[case] shown: &str,
    ) -> anyhow::Result<()> {
        let parsed: QualifiedCatalogName = text.parse()?;
        let expected = qualifier
            .map(|word| -> anyhow::Result<CatalogQualifier> {
                Ok(match word {
                    QualifiedCatalogName::GLOBAL => CatalogQualifier::Global,
                    slug => CatalogQualifier::Org(OrgSlug::try_new(slug)?),
                })
            })
            .transpose()?;
        assert_eq!(parsed.qualifier, expected);
        assert_eq!(parsed.name.as_str(), "my-fork");
        assert_eq!(parsed.to_string(), shown);
        Ok(())
    }

    #[rstest]
    #[case("")]
    #[case("/my-fork")]
    #[case("acme/")]
    #[case("acme/team/my-fork")]
    #[case("org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e/my-fork")]
    #[case("ac me/my-fork")]
    fn qualified_name_rejects(#[case] text: &str) {
        assert!(text.parse::<QualifiedCatalogName>().is_err(), "{text}");
    }

    #[rstest]
    #[case("Foo-1", true)]
    #[case("A_b.c", true)]
    #[case("0", true)]
    #[case("", false)]
    #[case(".foo", false)]
    #[case("-foo", false)]
    #[case("foo:bar", false)]
    #[case("foo/bar", false)]
    #[case("foo bar", true)]
    #[case("LFM2.5 Audio 1.5B", true)]
    #[case("a - b", true)]
    #[case(" foo", false)]
    #[case("foo ", false)]
    #[case("foo  bar", false)]
    #[case("foo\tbar", false)]
    #[case("foo\u{a0}bar", false)]
    #[case("é", false)]
    #[case::at_max("a".repeat(CatalogName::MAX_LEN), true)]
    #[case::over_max("a".repeat(CatalogName::MAX_LEN + 1), false)]
    fn catalog_name_grammar(#[case] name: String, #[case] valid: bool) {
        assert_eq!(CatalogName::try_new(name).is_ok(), valid);
    }

    #[test]
    fn catalog_name_keeps_case() -> anyhow::Result<()> {
        let name = CatalogName::try_new("Foo-1")?;
        assert_eq!(name.as_str(), "Foo-1");
        assert_eq!(name.key(), "foo-1");
        Ok(())
    }

    #[test]
    fn catalog_name_reports_a_wide_character_before_the_width() {
        assert!(matches!(
            CatalogName::try_new("é".repeat(CatalogName::MAX_LEN / 2 + 1)),
            Err(IdError::InvalidChar { ch: 'é', .. })
        ));
    }

    fn assert_wire_round_trip<T>(word: &str) -> anyhow::Result<()>
    where
        T: FromStr + fmt::Display + Serialize + DeserializeOwned + PartialEq + Debug,
        T::Err: std::error::Error + Send + Sync + 'static,
    {
        let value: T = word.parse()?;
        assert_eq!(value.to_string(), word);
        assert_eq!(
            serde_json::from_str::<T>(&serde_json::to_string(&value)?)?,
            value
        );
        Ok(())
    }

    #[rstest]
    #[case("stable")]
    #[case("daily")]
    #[case("hand-made")]
    fn kind_round_trip(#[case] word: &str) -> anyhow::Result<()> {
        assert_wire_round_trip::<DescriptorEntryKind>(word)
    }

    #[rstest]
    #[case("pinned")]
    #[case("active")]
    #[case("hidden")]
    #[case("tombstoned")]
    fn state_round_trip(#[case] word: &str) -> anyhow::Result<()> {
        assert_wire_round_trip::<DescriptorEntryState>(word)
    }

    #[rstest]
    #[case("operator")]
    #[case("importer:llamacpp")]
    #[case("account:user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")]
    fn creator_round_trip(#[case] word: &str) -> anyhow::Result<()> {
        assert_wire_round_trip::<DescriptorEntryCreator>(word)
    }

    #[rstest]
    #[case("global")]
    #[case("org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")]
    fn scope_round_trip(#[case] word: &str) -> anyhow::Result<()> {
        assert_wire_round_trip::<CatalogScope>(word)
    }

    #[rstest]
    #[case("Operator")]
    #[case("importer:other")]
    #[case("importer:")]
    #[case("llamacpp")]
    fn creator_refuses_unknown_words(#[case] word: &str) {
        assert!(word.parse::<DescriptorEntryCreator>().is_err());
        assert!(serde_json::from_value::<DescriptorEntryCreator>(json!(word)).is_err());
    }

    #[test]
    fn parse_errors_retain_identifier_sources() -> anyhow::Result<()> {
        let errors = [
            CatalogScope::from_sql_key("bad")
                .err()
                .context("invalid SQL scope")?,
            "bad"
                .parse::<CatalogScope>()
                .err()
                .context("invalid scope")?,
            "account:bad"
                .parse::<DescriptorEntryCreator>()
                .err()
                .context("invalid account creator")?,
        ];
        errors.into_iter().for_each(|error| {
            assert!(matches!(&error, CatalogParseError::Principal(_)));
            assert!(
                std::error::Error::source(&error)
                    .and_then(|source| source.downcast_ref::<PrincipalIdError>())
                    .is_some()
            );
        });
        let unknown = "importer:other"
            .parse::<DescriptorEntryCreator>()
            .err()
            .context("unknown creator")?;
        assert!(matches!(
            unknown,
            CatalogParseError::UnknownCreator(word) if word == "importer:other"
        ));
        Ok(())
    }

    #[test]
    fn tombstoned_entry_round_trips_but_cannot_be_created() -> anyhow::Result<()> {
        let at = Timestamp::now();
        let name = CatalogName::try_new("retired-1")?;
        let members = NonEmptyVec::try_new(vec![a_member(apple(), at)?])?;
        let entry = |state| {
            DescriptorEntry::new(
                CatalogScope::Global,
                name.clone(),
                DescriptorEntryKind::Stable,
                state,
                at,
                at,
                DescriptorEntryCreator::Operator,
                members.clone(),
            )
        };
        assert!(matches!(
            entry(DescriptorEntryState::Tombstoned),
            Err(EntryError::Tombstoned)
        ));
        let mut entry = entry(DescriptorEntryState::Active)?;
        entry.state = DescriptorEntryState::Tombstoned;
        assert_eq!(
            serde_json::from_str::<DescriptorEntry>(&serde_json::to_string(&entry)?)?,
            entry
        );
        Ok(())
    }

    #[rstest]
    #[case::model_dialect(json!({
        "type": "llamacpp_cli_stock_tools", "source": "vault_remote_archive",
        "url": "/vault/resources/vault_1/content", "principal_id": "org_1",
        "custom_label": "nightly", "flavor": "linux-x64-rocm"
    }))]
    #[case::uv_wheelhouse(json!({
        "type": "uv_vllm", "server_version": "0.31.0", "build": "cu128",
        "python_version": "3.12",
        "source": {
            "type": "vault_wheelhouse", "url": "/vault/resources/vault_1/content",
            "contents": "x", "principal_id": "org_1"
        }
    }))]
    fn vault_member_requires_org_scope(#[case] runtime: Value) -> anyhow::Result<()> {
        let member =
            DescriptorMember::new(serde_json::from_value(runtime.clone())?, Timestamp::now())?;
        let members = NonEmptyVec::try_new(vec![member.clone()])?;
        assert_eq!(
            DescriptorEntry::validate_scope_members(&CatalogScope::Global, &members),
            Err(EntryError::GlobalVaultSource)
        );
        DescriptorEntry::validate_scope_members(
            &CatalogScope::Org(OrgId::try_new("org_1")?),
            &members,
        )?;
        let mut unlabelled = runtime;
        drop_vault_custom_labels(&mut unlabelled);
        assert_eq!(member.digest(), descriptor_digest(&unlabelled)?);
        let read: DescriptorMember = serde_json::from_value(serde_json::to_value(member.clone())?)?;
        assert_eq!(read, member);
        Ok(())
    }

    #[test]
    fn a_vault_label_is_not_part_of_the_runtime_build() -> anyhow::Result<()> {
        let member = |label: &str, flavor: &str| -> anyhow::Result<DescriptorMember> {
            let runtime = json!({
                "type": "llamacpp_cli_stock_tools", "source": "vault_remote_archive",
                "url": "/vault/resources/vault_1/content", "principal_id": "org_1",
                "custom_label": label, "flavor": flavor
            });
            Ok(DescriptorMember::new(
                serde_json::from_value(runtime)?,
                Timestamp::now(),
            )?)
        };
        let members = NonEmptyVec::try_new(vec![
            member("nightly", "linux-x64-rocm")?,
            member("release", "macos-arm64")?,
        ])?;
        DescriptorEntry::validate_runtime_build(&members)?;
        Ok(())
    }

    #[rstest]
    #[case::runtime_wrong_digest(DescriptorKind::Runtime, json!({"digest": "00"}), "digest")]
    #[case::runtime_wrong_flavor(DescriptorKind::Runtime, json!({"flavor": "macos-arm64"}), "key")]
    #[case::model_wrong_digest(DescriptorKind::Model, json!({"digest": "00"}), "digest")]
    #[case::model_wrong_format(DescriptorKind::Model, json!({"format": "mlx"}), "key")]
    fn member_refuses_parts_that_disagree_with_its_descriptor(
        #[case] kind: DescriptorKind,
        #[case] change: Value,
        #[case] field: &str,
    ) -> anyhow::Result<()> {
        let member = match kind {
            DescriptorKind::Runtime => a_member(llama(None), Timestamp::now())?,
            DescriptorKind::Model => {
                a_model(pinned("gguf_text", "AcmeLM-3B"), "q4_0", Timestamp::now())?
            }
        };
        let mut wire = serde_json::to_value(member)?;
        if let (Some(fields), Some(change)) = (wire.as_object_mut(), change.as_object()) {
            fields.extend(change.clone());
        }
        let error = serde_json::from_value::<DescriptorMember>(wire)
            .err()
            .context("the member must be refused")?;
        assert!(error.to_string().contains(field), "{error}");
        Ok(())
    }

    #[rstest]
    #[case::gguf_text(ModelType::GgufText, ModelFormat::Gguf)]
    #[case::gguf_vision(ModelType::GgufVision, ModelFormat::Gguf)]
    #[case::gguf_vision_speculative(ModelType::GgufVisionSpeculative, ModelFormat::Gguf)]
    #[case::mlx(ModelType::Mlx, ModelFormat::Mlx)]
    #[case::mlx_speculative(ModelType::MlxSpeculative, ModelFormat::Mlx)]
    #[case::torch(ModelType::Torch, ModelFormat::Torch)]
    #[case::torch_speculative(ModelType::TorchSpeculative, ModelFormat::Torch)]
    #[case::openvino(ModelType::Openvino, ModelFormat::Openvino)]
    #[case::onnx_genai(ModelType::OnnxGenai, ModelFormat::Onnx)]
    #[case::apple_foundation_text(ModelType::AppleFoundationText, ModelFormat::System)]
    fn model_format_follows_the_type_table(
        #[case] model_type: ModelType,
        #[case] format: ModelFormat,
    ) {
        assert_eq!(ModelFormat::of(model_type), format);
    }

    #[rstest]
    #[case::gguf_text("gguf_text", "gguf")]
    #[case::gguf_vision_speculative("gguf_vision_speculative", "gguf")]
    #[case::mlx("mlx", "mlx")]
    #[case::torch_speculative("torch_speculative", "torch")]
    #[case::openvino("openvino", "openvino")]
    #[case::onnx_genai("onnx_genai", "onnx")]
    fn model_member_round_trips(
        #[case] model_type: &str,
        #[case] format: &str,
    ) -> anyhow::Result<()> {
        let member = a_model(pinned(model_type, "AcmeLM-3B"), "q4_0", Timestamp::now())?;
        assert_eq!(member.descriptor_kind(), DescriptorKind::Model);
        assert_eq!(member.descriptor_type(), model_type);
        assert_eq!(member.flavor(), None);
        assert!(member.runtime().is_none());
        let wire = serde_json::to_value(&member)?;
        assert_eq!(wire["format"], format);
        assert_eq!(wire["quantization"], "q4_0");
        assert_eq!(wire["model"]["type"], model_type);
        assert_eq!(serde_json::from_value::<DescriptorMember>(wire)?, member);
        Ok(())
    }

    #[rstest]
    #[case::gguf_text(pinned("gguf_text", "AcmeLM-3B"), Some(1))]
    #[case::mlx_speculative(pinned("mlx_speculative", "AcmeLM-3B"), Some(2))]
    #[case::apple_foundation_text(json!({"type": "apple_foundation_text"}), Some(0))]
    #[case::url(
        json!({"type": "gguf_text", "source": "url", "url": "https://example.com/model.gguf"}),
        None
    )]
    fn hf_repos_mut_opens_each_repository(
        #[case] model: Value,
        #[case] count: Option<usize>,
    ) -> anyhow::Result<()> {
        let mut model: Model = serde_json::from_value(model)?;
        let revision = crate::HfRevision::try_new("main")?;
        let opened = hf_repos_mut(&mut model).map(|repos| {
            repos
                .into_iter()
                .map(|repo| repo.revision = Some(revision.clone()))
                .count()
        });
        assert_eq!(opened, count);
        let pinned = hf_repos(&model).map(|repos| {
            repos
                .iter()
                .filter(|repo| repo.revision.as_ref() == Some(&revision))
                .count()
        });
        assert_eq!(pinned, count);
        Ok(())
    }

    #[rstest]
    #[case::url(
        json!({"type": "gguf_text", "source": "url", "url": "https://example.com/model.gguf"}),
        |e: &MemberError| matches!(e, MemberError::NotHuggingFace),
    )]
    #[case::relative_dir(
        json!({"type": "torch", "source": "relative_dir", "dir": "models/acme"}),
        |e: &MemberError| matches!(e, MemberError::NotHuggingFace),
    )]
    #[case::vault_draft(
        json!({
            "type": "mlx_speculative", "target": hf("AcmeLM-3B", Some(COMMIT)),
            "draft": {
                "source": "vault_archive", "url": "/vault/resources/vault_1/content",
                "principal_id": "org_1"
            }
        }),
        |e: &MemberError| matches!(e, MemberError::NotHuggingFace),
    )]
    #[case::branch(
        json!({"type": "mlx", "source": "huggingface", "org": "acme", "repo_name": "AcmeLM-3B", "revision": "main"}),
        |e: &MemberError| matches!(e, MemberError::UnpinnedRevision(r) if r == "acme/AcmeLM-3B@main"),
    )]
    #[case::no_revision(
        json!({"type": "mlx", "source": "huggingface", "org": "acme", "repo_name": "AcmeLM-3B"}),
        |e: &MemberError| matches!(e, MemberError::UnpinnedRevision(_)),
    )]
    #[case::short_commit(
        json!({"type": "mlx", "source": "huggingface", "org": "acme", "repo_name": "AcmeLM-3B", "revision": "0123456"}),
        |e: &MemberError| matches!(e, MemberError::UnpinnedRevision(_)),
    )]
    #[case::uppercase_commit(
        json!({
            "type": "mlx", "source": "huggingface", "org": "acme", "repo_name": "AcmeLM-3B",
            "revision": "0123456789ABCDEF0123456789ABCDEF01234567"
        }),
        |e: &MemberError| matches!(e, MemberError::UnpinnedRevision(_)),
    )]
    #[case::unpinned_draft(
        json!({
            "type": "torch_speculative", "target": hf("AcmeLM-3B", Some(COMMIT)),
            "draft": hf("AcmeLM-3B-Draft", None)
        }),
        |e: &MemberError| matches!(e, MemberError::UnpinnedRevision(r) if r == "acme/AcmeLM-3B-Draft"),
    )]
    #[case::auth_token(
        json!({
            "type": "mlx", "source": "huggingface", "org": "acme", "repo_name": "AcmeLM-3B",
            "revision": COMMIT, "auth_token": "hf_secret"
        }),
        |e: &MemberError| matches!(e, MemberError::AuthToken),
    )]
    fn model_member_is_refused(
        #[case] model: Value,
        #[case] expected: fn(&MemberError) -> bool,
    ) -> anyhow::Result<()> {
        let error = DescriptorMember::new_model(
            serde_json::from_value(model)?,
            QuantizationLabel::try_new("q4_0")?,
            Timestamp::now(),
        )
        .err()
        .context("the model must be refused")?;
        assert!(expected(&error), "{error:?}");
        Ok(())
    }

    #[test]
    fn model_entry_lists_one_type_per_format() -> anyhow::Result<()> {
        let at = Timestamp::now();
        let members = NonEmptyVec::try_new(vec![
            a_model(pinned("gguf_text", "AcmeLM-3B-GGUF"), "q4_0", at)?,
            a_model(pinned("gguf_text", "AcmeLM-3B-GGUF-Q8"), "q8_0", at)?,
            a_model(pinned("mlx", "AcmeLM-3B-MLX-4bit"), "4bit", at)?,
        ])?;
        let entry = DescriptorEntry::new(
            CatalogScope::Global,
            CatalogName::try_new("AcmeLM-3B")?,
            DescriptorEntryKind::HandMade,
            DescriptorEntryState::Active,
            at,
            at,
            DescriptorEntryCreator::Operator,
            members,
        )?;
        assert_eq!(entry.descriptor_kind(), DescriptorKind::Model);
        assert_eq!(entry.descriptor_types(), ["gguf_text", "mlx"]);
        assert_eq!(
            serde_json::from_str::<DescriptorEntry>(&serde_json::to_string(&entry)?)?,
            entry
        );
        Ok(())
    }

    #[test]
    fn model_entry_must_be_hand_made() -> anyhow::Result<()> {
        let at = Timestamp::now();
        let members = NonEmptyVec::try_new(vec![a_model(pinned("mlx", "AcmeLM-3B"), "4bit", at)?])?;
        let refused = DescriptorEntry::new(
            CatalogScope::Global,
            CatalogName::try_new("AcmeLM-3B")?,
            DescriptorEntryKind::Stable,
            DescriptorEntryState::Active,
            at,
            at,
            DescriptorEntryCreator::Operator,
            members,
        );
        assert!(matches!(refused, Err(EntryError::ModelKind)), "{refused:?}");
        Ok(())
    }

    #[rstest]
    #[case("LFM2.5-1.2B-Instruct", None, "LFM2.5")]
    #[case("Gemma", None, "Gemma")]
    #[case("Qwen3 Coder -7B", None, "Qwen3 Coder ")]
    #[case("LFM2.5-1.2B", Some("LFM"), "LFM")]
    fn a_model_entry_has_a_family(
        #[case] name: &str,
        #[case] family: Option<&str>,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let at = Timestamp::now();
        let members = NonEmptyVec::try_new(vec![a_model(pinned("mlx", "AcmeLM-3B"), "4bit", at)?])?;
        let entry = DescriptorEntry::new(
            CatalogScope::Global,
            CatalogName::try_new(name)?,
            DescriptorEntryKind::HandMade,
            DescriptorEntryState::Active,
            at,
            at,
            DescriptorEntryCreator::Operator,
            members,
        )?
        .with_family(family.map(CatalogName::try_new).transpose()?)?;
        assert_eq!(entry.family(), Some(expected));
        assert_eq!(
            serde_json::from_str::<DescriptorEntry>(&serde_json::to_string(&entry)?)?,
            entry
        );
        Ok(())
    }

    #[test]
    fn a_runtime_entry_has_no_family() -> anyhow::Result<()> {
        let at = Timestamp::now();
        let entry = DescriptorEntry::new(
            CatalogScope::Global,
            CatalogName::try_new("apple-1")?,
            DescriptorEntryKind::HandMade,
            DescriptorEntryState::Active,
            at,
            at,
            DescriptorEntryCreator::Operator,
            NonEmptyVec::try_new(vec![a_member(apple(), at)?])?,
        )?;
        assert_eq!(entry.family(), None);
        let refused = entry.with_family(Some(CatalogName::try_new("Apple")?));
        assert!(
            matches!(refused, Err(EntryError::RuntimeFamily)),
            "{refused:?}"
        );
        Ok(())
    }

    #[rstest]
    #[case("https://pipette.liquid.ai/logos/lfm.svg", true)]
    #[case("https://pipette.liquid.ai/logos/vendors/Qwen_3-x.png", true)]
    #[case("https://pipette.liquid.ai/logos/", false)]
    #[case("https://pipette.liquid.ai/logos/a//b.png", false)]
    #[case("https://pipette.liquid.ai/logos/a/", false)]
    #[case("https://pipette.liquid.ai/logos/../x.png", false)]
    #[case("https://pipette.liquid.ai/logos/a/../../x.png", false)]
    #[case("https://pipette.liquid.ai/logos/.hidden", false)]
    #[case("https://pipette.liquid.ai/logos/a.png?v=1", false)]
    #[case("https://pipette.liquid.ai/logos/a.png#top", false)]
    #[case("https://pipette.liquid.ai/logos/a b.png", false)]
    #[case("https://pipette.liquid.ai/logos/a%2F.png", false)]
    #[case("http://pipette.liquid.ai/logos/a.png", false)]
    #[case("https://pipette.liquid.ai/a.png", false)]
    #[case("https://pipette.liquid.ai.example.com/logos/a.png", false)]
    #[case("https://user@pipette.liquid.ai/logos/a.png", false)]
    #[case("https://pipette.liquid.ai:8443/logos/a.png", false)]
    #[case("HTTPS://pipette.liquid.ai/logos/a.png", false)]
    #[case("data:image/png;base64,iVBORw0KGgo=", true)]
    #[case("data:image/svg+xml;base64,PHN2Zy8+", true)]
    #[case("data:image/webp;base64,UklGRg==", true)]
    #[case("data:image/png;base64,a+/9", true)]
    #[case("data:image/png;base64,", false)]
    #[case("data:image/png;base64,abc", false)]
    #[case("data:image/png;base64,a===", false)]
    #[case("data:image/png;base64,ab=c", false)]
    #[case("data:image/png;base64,ab-_", false)]
    #[case("data:image/png;base64,ab c", false)]
    #[case("data:image/png,abcd", false)]
    #[case("data:image/jpeg;base64,abcd", false)]
    #[case("data:text/html;base64,abcd", false)]
    #[case("data:image/svg+xml;charset=utf-8;base64,abcd", false)]
    #[case("data:;base64,abcd", false)]
    #[case("DATA:image/png;base64,abcd", false)]
    #[case("data:IMAGE/PNG;base64,abcd", false)]
    #[case("data://image/png;base64,abcd", false)]
    fn a_logo_uri_is_in_the_logo_folder_or_a_data_uri(
        #[case] uri: &str,
        #[case] ok: bool,
    ) -> anyhow::Result<()> {
        assert_eq!(LogoUri::try_new(uri).is_ok(), ok, "{uri}");
        let pattern = regex::Regex::new(LogoUri::SCHEMA_PATTERN)?;
        assert_eq!(pattern.is_match(uri), ok, "schema pattern disagrees: {uri}");
        Ok(())
    }

    #[rstest]
    #[case::https_at_limit(LogoUri::PREFIX, 0, true)]
    #[case::https_over_limit(LogoUri::PREFIX, 1, false)]
    // Base64 grows in groups of four characters.
    #[case::data_at_limit("data:image/png;base64,", 0, true)]
    #[case::data_over_limit("data:image/png;base64,", 4, false)]
    fn a_logo_uri_has_a_length_limit(
        #[case] prefix: &str,
        #[case] extra: usize,
        #[case] ok: bool,
    ) -> anyhow::Result<()> {
        let room = LogoUri::MAX_LEN - prefix.len();
        let fill = if prefix.starts_with("data:") {
            room - room % 4
        } else {
            room
        };
        let uri = format!("{prefix}{}", "a".repeat(fill + extra));
        if ok {
            LogoUri::try_new(uri)?;
        } else {
            assert!(LogoUri::try_new(uri).is_err());
        }
        Ok(())
    }

    #[test]
    fn a_logo_digest_is_the_sha256_of_the_uri() -> anyhow::Result<()> {
        let uri = LogoUri::try_new("data:image/png;base64,abcd")?;
        assert_eq!(
            uri.sha256(),
            "c2e7d21098f9131392a4dc3789f3b8de9561b38e3ad627389872d24a5ca4fa02"
        );
        Ok(())
    }

    #[rstest]
    #[case::own_scope(&[("org", "LFM2.5", "org.svg"), ("global", "LFM2.5", "global.svg")], "org", false, Some("org.svg"))]
    #[case::global_fallback(&[("global", "LFM2.5", "global.svg")], "org", false, Some("global.svg"))]
    #[case::case_ignored(&[("global", "lfm2.5", "global.svg")], "global", false, Some("global.svg"))]
    #[case::other_org_ignored(&[("other", "LFM2.5", "other.svg")], "org", false, None)]
    #[case::org_logo_not_global(&[("org", "LFM2.5", "org.svg")], "global", false, None)]
    #[case::other_family(&[("global", "LFM2", "global.svg")], "global", false, None)]
    // A runtime entry has no family, so the logo of the family its name would give does not apply.
    #[case::runtime_entry(&[("global", "LFM2.5", "global.svg")], "global", true, None)]
    fn a_family_logo_comes_from_the_entry_scope_then_global(
        #[case] logos: &[(&str, &str, &str)],
        #[case] entry_scope: &str,
        #[case] runtime: bool,
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let scope = |name: &str| -> anyhow::Result<CatalogScope> {
            Ok(match name {
                "global" => CatalogScope::Global,
                org => CatalogScope::Org(OrgId::try_new(format!("org_{org}"))?),
            })
        };
        // A stand-in digest that differs for each file.
        let digest = |file: &str| format!("{:0>64}", hex::encode(file));
        let logos = logos
            .iter()
            .map(|&(at, family, file)| {
                Ok(FamilyLogoDigest {
                    scope: scope(at)?,
                    family: CatalogName::try_new(family)?,
                    sha256: Sha256::try_new(digest(file))?,
                })
            })
            .collect::<anyhow::Result<FamilyLogos>>()?;
        let at = Timestamp::now();
        let member = if runtime {
            a_member(apple(), at)?
        } else {
            a_model(pinned("mlx", "AcmeLM-3B"), "4bit", at)?
        };
        let mut entry = DescriptorEntry::new(
            CatalogScope::Global,
            CatalogName::try_new("LFM2.5-1.2B")?,
            DescriptorEntryKind::HandMade,
            DescriptorEntryState::Active,
            at,
            at,
            DescriptorEntryCreator::Operator,
            NonEmptyVec::try_new(vec![member])?,
        )?;
        entry.scope = scope(entry_scope)?;
        assert_eq!(
            logos.of(&entry).map(AsRef::as_ref),
            expected.map(digest).as_deref()
        );
        Ok(())
    }

    /// One member of a validation case: a runtime, or a model with its
    /// quantization.
    fn a_case_member(
        (descriptor, quantization): (Value, Option<&str>),
        at: Timestamp,
    ) -> anyhow::Result<DescriptorMember> {
        match quantization {
            None => a_member(descriptor, at),
            Some(quantization) => a_model(descriptor, quantization, at),
        }
    }

    #[rstest]
    #[case::duplicate_runtime_digest(
        vec![(apple(), None), (apple(), None)],
        |e: &EntryError| matches!(e, EntryError::DuplicateDigest(_)),
    )]
    #[case::mixed_runtime_types(
        vec![(apple(), None), (llama(None), None)],
        |e: &EntryError| matches!(e, EntryError::RuntimeTypeMismatch),
    )]
    #[case::duplicate_runtime_flavor(
        vec![(llama(None), None), (llama(Some("alternate.tar.gz")), None)],
        |e: &EntryError| matches!(e, EntryError::DuplicateFlavor(_)),
    )]
    #[case::runtime_then_model(
        vec![(llama(None), None), (pinned("gguf_text", "AcmeLM-3B"), Some("q4_0"))],
        |e: &EntryError| matches!(e, EntryError::DescriptorKindMismatch),
    )]
    #[case::model_then_runtime(
        vec![(pinned("gguf_text", "AcmeLM-3B"), Some("q4_0")), (llama(None), None)],
        |e: &EntryError| matches!(e, EntryError::DescriptorKindMismatch),
    )]
    #[case::duplicate_model_key(
        vec![
            (pinned("gguf_text", "AcmeLM-3B-GGUF"), Some("q4_0")),
            (pinned("gguf_text", "AcmeLM-3B-Other-GGUF"), Some("q4_0")),
        ],
        |e: &EntryError| matches!(e, EntryError::DuplicateModelKey { format: ModelFormat::Gguf, .. }),
    )]
    #[case::format_type_clash(
        vec![
            (pinned("gguf_text", "AcmeLM-3B-GGUF"), Some("q4_0")),
            (pinned("gguf_vision", "AcmeLM-3B-GGUF"), Some("q8_0")),
        ],
        |e: &EntryError| matches!(e, EntryError::FormatTypeClash { format: ModelFormat::Gguf, .. }),
    )]
    #[case::speculative_mix(
        vec![
            (pinned("mlx", "AcmeLM-3B-MLX"), Some("4bit")),
            (pinned("torch_speculative", "AcmeLM-3B"), Some("bf16")),
        ],
        |e: &EntryError| matches!(e, EntryError::SpeculativeMix),
    )]
    #[case::duplicate_model_digest(
        vec![
            (pinned("mlx", "AcmeLM-3B-MLX"), Some("4bit")),
            (pinned("mlx", "AcmeLM-3B-MLX"), Some("8bit")),
        ],
        |e: &EntryError| matches!(e, EntryError::DuplicateDigest(_)),
    )]
    fn entry_refuses_invalid_members(
        #[case] members: Vec<(Value, Option<&str>)>,
        #[case] expected: fn(&EntryError) -> bool,
    ) -> anyhow::Result<()> {
        let at = Timestamp::now();
        let members = members
            .into_iter()
            .map(|member| a_case_member(member, at))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let error = DescriptorEntry::validate_members(&NonEmptyVec::try_new(members)?)
            .err()
            .context("the members must be refused")?;
        assert!(expected(&error), "{error:?}");
        Ok(())
    }

    /// A catalog member stores the digest that the build flag carries, so the
    /// server's check and a client's report agree.
    #[rstest]
    #[case::llamacpp_ios(json!({
        "type": "llamacpp_ios_pipette", "repository_url": "github.com/ggml-org/llama.cpp",
        "repository_version": "b11988", "flavor": "ios-arm64"
    }), true)]
    #[case::mlx_ios(json!({
        "type": "mlx_ios_pipette", "flavor": "ios-arm64", "private_thermal": true,
        "packages": {
            "mlx_swift": {"repository_url": "github.com/ml-explore/mlx-swift",
                          "repository_version": "0.25.6"},
            "mlx_swift_lm": {"repository_url": "github.com/ml-explore/mlx-swift-examples",
                             "repository_version": "2.25.6"},
            "swift_transformers": {"repository_url": "github.com/huggingface/swift-transformers",
                                   "repository_version": "0.1.22"}
        }
    }), true)]
    #[case::stock(json!({
        "type": "llamacpp_cli_stock_tools", "source": "github_release",
        "repository_url": "github.com/ggml-org/llama.cpp", "repository_version": "b1",
        "flavor": "macos-arm64"
    }), false)]
    fn runtime_build_flag_matches_the_member_digest(
        #[case] runtime: Value,
        #[case] is_app: bool,
    ) -> anyhow::Result<()> {
        let runtime: Runtime = serde_json::from_value(runtime)?;
        let member = DescriptorMember::new(runtime.clone(), jiff::Timestamp::now())?;
        let flag = crate::runtime_build_flag(&runtime)?.map(|flag| flag.into_inner());
        let expected = is_app.then(|| format!("runtime:descriptor:{}", member.digest()));
        assert_eq!(flag, expected);
        assert_eq!(member.app_build_flag(), flag);
        Ok(())
    }
}
