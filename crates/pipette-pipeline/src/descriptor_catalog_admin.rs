//! Shared administration of readable runtime and model descriptor names.

use std::collections::HashMap;
use std::num::NonZeroUsize;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use pipette_plan_types::{
    AddDescriptorMemberRequest, CatalogName, CatalogScope, CreateDescriptorEntryRequest,
    DescriptorEntry, DescriptorEntryCreator, DescriptorEntryKind, DescriptorEntryState,
    DescriptorKind, DescriptorMember, EntryError, GitOid, HfRevision, MemberError, Model,
    ModelFormat, ModelMemberRequest, ModelType, NonEmptyVec, Runtime, descriptor_digest, hf_repos,
    hf_repos_mut, names_vault_source,
};
use pipette_store::{
    AppendOutcome, CatalogEntryKey, CreateEntryOutcome, DescriptorCatalogStore,
    DescriptorEntryFilter, DescriptorEntryPage, SetStateOutcome, TransitionOutcome, VaultStore,
};

use crate::hf_import::{self, HfImportError, HubClient, HubError, HubGate, ImportFault};
use crate::vault_alias;
use crate::vault_check::{self, VaultCheckError, VaultScope};

/// Importer-owned prefixes. Add the pattern for each new importer source here.
/// This stops an operator from taking a name that the source creates later.
pub struct ReservedNamePattern {
    pub source: &'static str,
    pub prefix: &'static str,
}

/// The name prefix of a llama.cpp stable entry.
pub const LLAMACPP_STABLE_PREFIX: &str = "llamacpp-v";
/// The name prefix of a llama.cpp daily entry.
pub const LLAMACPP_DAILY_PREFIX: &str = "llamacpp-daily-";

pub const RESERVED_NAME_PATTERNS: &[ReservedNamePattern] = &[
    ReservedNamePattern {
        source: "llamacpp",
        prefix: LLAMACPP_STABLE_PREFIX,
    },
    ReservedNamePattern {
        source: "llamacpp",
        prefix: LLAMACPP_DAILY_PREFIX,
    },
];

fn log_catalog_write(
    scope: &CatalogScope,
    name: &CatalogName,
    flavor: &str,
    state: &str,
    event: &'static str,
) {
    match scope {
        CatalogScope::Global => tracing::info!(catalog_name = %name, flavor, state, "{event}"),
        CatalogScope::Org(org_id) => {
            tracing::info!(catalog_name = %name, org_id = %org_id, flavor, state, "{event}");
        }
    }
}

fn log_model_member(
    scope: &CatalogScope,
    name: &CatalogName,
    member: &DescriptorMember,
    event: &'static str,
) {
    let Some((format, quantization)) = member.model_key() else {
        return;
    };
    let hf_refs = member_refs(member);
    match scope {
        CatalogScope::Global => tracing::info!(
            catalog_name = %name,
            format = %format,
            quantization = %quantization,
            hf_refs,
            "{event}"
        ),
        CatalogScope::Org(org_id) => tracing::info!(
            catalog_name = %name,
            org_id = %org_id,
            format = %format,
            quantization = %quantization,
            hf_refs,
            "{event}"
        ),
    }
}

fn log_model_entry(entry: &DescriptorEntry) {
    let (formats, quantizations): (Vec<_>, Vec<_>) = entry
        .members
        .iter()
        .filter_map(DescriptorMember::model_key)
        .map(|(format, quantization)| (format.to_string(), quantization.to_string()))
        .unzip();
    let name = &entry.name;
    let state = entry.state.as_ref();
    match &entry.scope {
        CatalogScope::Global => tracing::info!(
            catalog_name = %name,
            format = ?formats,
            quantization = ?quantizations,
            state,
            "catalog entry created"
        ),
        CatalogScope::Org(org_id) => tracing::info!(
            catalog_name = %name,
            org_id = %org_id,
            format = ?formats,
            quantization = ?quantizations,
            state,
            "catalog entry created"
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateDocument {
    name: CatalogName,
    #[serde(default, deserialize_with = "release_time")]
    release_time: Option<DateTime<Utc>>,
    #[serde(default)]
    runtimes: Option<NonEmptyVec<Runtime>>,
    #[serde(default)]
    models: Option<NonEmptyVec<ModelMemberRequest>>,
}

/// Accepts a quoted RFC 3339 string or a native TOML datetime.
fn release_time<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<DateTime<Utc>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Str(String),
        Native(toml::value::Datetime),
    }

    let text = match Raw::deserialize(deserializer)? {
        Raw::Str(s) => s,
        Raw::Native(dt) => dt.to_string(),
    };
    DateTime::parse_from_rfc3339(&text)
        .map(|time| Some(time.with_timezone(&Utc)))
        .map_err(serde::de::Error::custom)
}

/// The members from a file or a request before the command pins each model.
enum MemberInput {
    Runtimes(NonEmptyVec<Runtime>),
    Models(NonEmptyVec<ModelMemberRequest>),
}

/// A create file with a checked name and one kind of member.
struct CreateInput {
    name: CatalogName,
    release_time: Option<DateTime<Utc>>,
    members: MemberInput,
}

/// The commit that one Hugging Face revision in a file resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitPin {
    /// The repository as `org/repo_name`.
    pub repo: String,
    /// The revision that the file gives, or `main` if it gives none.
    pub requested: String,
    pub commit: GitOid,
}

pub struct Preview {
    pub entry: DescriptorEntry,
    /// The other live names for each digest that this command adds.
    pub synonyms: Vec<String>,
    /// The commit of each Hugging Face revision that this command pinned.
    pub pins: Vec<CommitPin>,
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogWriteError {
    #[error("{0:#}")]
    Invalid(anyhow::Error),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0:#}")]
    Storage(anyhow::Error),
    /// Hugging Face failed or rate-limited the call that pins a model.
    #[error("{0:#}")]
    Upstream(anyhow::Error),
}

/// The member keys in order, joined with commas: a runtime member's flavor, or
/// a model member's `format::quantization`. `-` marks a runtime member with no
/// flavor.
pub fn member_keys(members: &NonEmptyVec<DescriptorMember>) -> String {
    members.iter().map(member_key).collect::<Vec<_>>().join(",")
}

/// A runtime member's flavor, or a model member's `format::quantization`.
pub fn member_key(member: &DescriptorMember) -> String {
    match (member.model_key(), member.flavor()) {
        (Some((format, quantization)), _) => format!("{format}::{quantization}"),
        (None, flavor) => flavor.unwrap_or("-").to_owned(),
    }
}

/// The Hugging Face repositories of a model member as `org/repo_name@commit`,
/// joined with commas. A runtime member gives an empty string.
pub fn member_refs(member: &DescriptorMember) -> String {
    member
        .model()
        .and_then(hf_repos)
        .unwrap_or_default()
        .iter()
        .map(|repo| repo.reference())
        .collect::<Vec<_>>()
        .join(",")
}

fn build_identity(runtime: &Runtime) -> anyhow::Result<serde_json::Value> {
    let mut value = serde_json::to_value(runtime)?;
    value
        .as_object_mut()
        .context("a runtime descriptor must be an object")?
        .remove("flavor");
    Ok(value)
}

/// Check the invariant shared by operator appends and importer refreshes. The
/// members of a model entry come from the operator's claim that they hold one
/// model, so the check passes them.
pub fn same_build(members: &NonEmptyVec<DescriptorMember>) -> anyhow::Result<()> {
    let Some(first) = members.head().runtime() else {
        return Ok(());
    };
    let first = build_identity(first)?;
    for member in members.iter().skip(1) {
        let runtime = member
            .runtime()
            .context("members of one catalog entry mix runtimes and models")?;
        anyhow::ensure!(
            build_identity(runtime)? == first,
            "members of one catalog entry may differ only in flavor"
        );
    }
    Ok(())
}

fn parse_create(scope: &CatalogScope, document: &str) -> anyhow::Result<CreateInput> {
    let input: CreateDocument = toml::from_str(document).context("invalid catalog create file")?;
    check_reserved_name(scope, &input.name)?;
    Ok(CreateInput {
        name: input.name,
        release_time: input.release_time,
        members: MemberInput::of_entry(input.runtimes, input.models)?,
    })
}

fn check_reserved_name(scope: &CatalogScope, name: &CatalogName) -> anyhow::Result<()> {
    if matches!(scope, CatalogScope::Global)
        && let Some(pattern) = RESERVED_NAME_PATTERNS
            .iter()
            .find(|pattern| name.key().starts_with(pattern.prefix))
    {
        anyhow::bail!("{name} is reserved for the {} importer", pattern.source);
    }
    Ok(())
}

async fn build_create(
    hub: &HubClient,
    vault: Option<&dyn VaultStore>,
    scope: &CatalogScope,
    input: CreateDescriptorEntryRequest,
    now: DateTime<Utc>,
    creator: DescriptorEntryCreator,
) -> Result<DescriptorEntry, CatalogWriteError> {
    check_reserved_name(scope, &input.name).map_err(CatalogWriteError::Invalid)?;
    let (members, _) = MemberInput::of_entry(input.runtimes, input.models)
        .map_err(CatalogWriteError::Invalid)?
        .resolve(hub, vault, scope, now)
        .await
        .map_err(resolve_error)?;
    new_entry(scope, input.name, input.release_time, members, now, creator)
        .map_err(CatalogWriteError::Invalid)
}

fn parse_member(document: &str) -> anyhow::Result<MemberInput> {
    let input: AddDescriptorMemberRequest =
        toml::from_str(document).context("invalid catalog member file")?;
    MemberInput::of_member(input)
}

/// A Hugging Face failure or rate limit is upstream. Every other failure to
/// pin a member is a fault of the request.
fn resolve_error(error: anyhow::Error) -> CatalogWriteError {
    if matches!(
        error.downcast_ref::<VaultCheckError>(),
        Some(VaultCheckError::Store(_))
    ) {
        return CatalogWriteError::Storage(error);
    }
    match error.downcast_ref::<HfImportError>() {
        Some(import_error)
            if matches!(
                import_error.fault(),
                ImportFault::RateLimited | ImportFault::Upstream
            ) =>
        {
            CatalogWriteError::Upstream(error)
        }
        _ => CatalogWriteError::Invalid(error),
    }
}

impl MemberInput {
    fn descriptor_kind(&self) -> DescriptorKind {
        match self {
            Self::Runtimes(_) => DescriptorKind::Runtime,
            Self::Models(_) => DescriptorKind::Model,
        }
    }

    /// The members of a new entry, which has exactly one kind of member.
    fn of_entry(
        runtimes: Option<NonEmptyVec<Runtime>>,
        models: Option<NonEmptyVec<ModelMemberRequest>>,
    ) -> anyhow::Result<Self> {
        match (runtimes, models) {
            (Some(runtimes), None) => Ok(Self::Runtimes(runtimes)),
            (None, Some(models)) => Ok(Self::Models(models)),
            (None, None) => anyhow::bail!("a catalog entry needs `runtimes` or `models`"),
            (Some(_), Some(_)) => {
                anyhow::bail!("a catalog entry has `runtimes` or `models`, not both")
            }
        }
    }

    fn of_member(input: AddDescriptorMemberRequest) -> anyhow::Result<Self> {
        match (input.runtime, input.model, input.quantization) {
            (Some(runtime), None, None) => Ok(Self::Runtimes(NonEmptyVec::try_new(vec![runtime])?)),
            (None, Some(model), Some(quantization)) => {
                Ok(Self::Models(NonEmptyVec::try_new(vec![
                    ModelMemberRequest {
                        quantization,
                        model,
                    },
                ])?))
            }
            (None, Some(_), None) => anyhow::bail!("a model member needs `quantization`"),
            (Some(_), None, Some(_)) => {
                anyhow::bail!("a runtime member cannot have `quantization`")
            }
            (None, None, _) => anyhow::bail!("a catalog member needs `runtime` or `model`"),
            (Some(_), Some(_), _) => {
                anyhow::bail!("a catalog member has `runtime` or `model`, not both")
            }
        }
    }

    /// The members, each model pinned to the commits that its revisions name
    /// now. A runtime makes no Hugging Face call.
    async fn resolve(
        self,
        hub: &HubClient,
        vault: Option<&dyn VaultStore>,
        scope: &CatalogScope,
        now: DateTime<Utc>,
    ) -> anyhow::Result<(NonEmptyVec<DescriptorMember>, Vec<CommitPin>)> {
        match self {
            Self::Runtimes(runtimes) => Ok((runtime_members(runtimes, now)?, Vec::new())),
            Self::Models(models) => {
                for document in models.iter() {
                    validate_model_source(scope, &document.model)?;
                }
                let mut members = Vec::new();
                let mut pins = Vec::new();
                for document in models.into_inner() {
                    let (member, member_pins) =
                        if names_vault_source(&serde_json::to_value(&document.model)?) {
                            let CatalogScope::Org(org_id) = scope else {
                                anyhow::bail!("a global catalog entry cannot name a vault model");
                            };
                            let vault =
                                vault.context("the vault store is required for a vault model")?;
                            let descriptor = serde_json::to_value(&document.model)?;
                            let member = DescriptorMember::new_model(
                                document.model,
                                document.quantization,
                                now,
                            )?;
                            vault_check::check(
                                vault,
                                &VaultScope::Org(org_id.clone()),
                                &descriptor,
                                &mut HashMap::new(),
                            )
                            .await
                            .map_err(anyhow::Error::new)?;
                            let alias = vault_alias::model_alias_within(
                                vault,
                                Some(&pipette_plan_types::canonical_json(&descriptor)),
                                &VaultScope::Org(org_id.clone()),
                            )
                            .await
                            .map_err(|error| anyhow::Error::new(VaultCheckError::Store(error)))?;
                            let alias_digest = alias
                                .map(|alias| {
                                    descriptor_digest(&serde_json::from_str::<serde_json::Value>(
                                        &alias,
                                    )?)
                                })
                                .transpose()?;
                            (member.with_alias_digest(alias_digest), Vec::new())
                        } else {
                            pin_model(hub, document, now).await?
                        };
                    members.push(member);
                    pins.extend(member_pins);
                }
                Ok((NonEmptyVec::try_new(members)?, pins))
            }
        }
    }
}

fn validate_model_source(scope: &CatalogScope, model: &Model) -> anyhow::Result<()> {
    let model_type = ModelType::of(model);
    if ModelFormat::of(model_type).is_none() {
        return Err(MemberError::NoFormat(model_type).into());
    }
    if names_vault_source(&serde_json::to_value(model)?) {
        anyhow::ensure!(
            matches!(scope, CatalogScope::Org(_)),
            "a global catalog entry cannot name a vault model"
        );
        return Ok(());
    }
    let repos = hf_repos(model).ok_or(MemberError::NotHuggingFace)?;
    if repos.iter().any(|repo| repo.auth_token.is_some()) {
        return Err(MemberError::AuthToken.into());
    }
    Ok(())
}

fn runtime_members(
    runtimes: NonEmptyVec<Runtime>,
    now: DateTime<Utc>,
) -> anyhow::Result<NonEmptyVec<DescriptorMember>> {
    let members = runtimes
        .into_inner()
        .into_iter()
        .map(|runtime| DescriptorMember::new(runtime, now))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(NonEmptyVec::try_new(members)?)
}

/// Pin each Hugging Face repository to the commit that its revision names now.
/// Require public, ungated repositories. The calls carry no token.
/// Each file that the model names must be at that commit.
async fn pin_model(
    hub: &HubClient,
    document: ModelMemberRequest,
    now: DateTime<Utc>,
) -> anyhow::Result<(DescriptorMember, Vec<CommitPin>)> {
    let ModelMemberRequest {
        quantization,
        mut model,
    } = document;
    let listings = hf_import::listings_for(hub, &model)
        .await
        .map_err(|error| {
            if let HfImportError::Hub(HubError::Refused { repo, status }) = &error {
                return not_public(
                    repo,
                    "refused",
                    &format!(
                        "Hugging Face refused {repo} without a token ({status}): the repository is private, or the repository or its revision does not exist"
                    ),
                );
            }
            anyhow::Error::new(error)
        })?;
    let mut pins = Vec::new();
    for repo in hf_repos_mut(&mut model).unwrap_or_default() {
        let listing = listings
            .get(repo)
            .with_context(|| format!("{} has no listing", repo.reference()))?;
        match (listing.private, &listing.gated) {
            (Some(false), Some(HubGate::Open)) => {}
            (Some(true), _) => {
                return Err(not_public(
                    &repo.to_string(),
                    "private",
                    &format!("{repo} is a private repository"),
                ));
            }
            (_, Some(HubGate::Gated(mode))) => {
                return Err(not_public(
                    &repo.to_string(),
                    "gated",
                    &format!("{repo} is a gated repository ({mode})"),
                ));
            }
            (None, _) | (_, None) => {
                return Err(not_public(
                    &repo.to_string(),
                    "unreported",
                    &format!("Hugging Face did not report whether {repo} is public and ungated"),
                ));
            }
        }
        let pin = CommitPin {
            repo: repo.to_string(),
            requested: repo
                .revision
                .as_ref()
                .map_or("main", AsRef::as_ref)
                .to_owned(),
            commit: listing.commit.clone(),
        };
        repo.revision = Some(HfRevision::try_new(listing.commit.to_string())?);
        if !pins.contains(&pin) {
            pins.push(pin);
        }
    }
    Ok((DescriptorMember::new_model(model, quantization, now)?, pins))
}

/// Report why a repository cannot back a public catalog member.
fn not_public(hf_repo: &str, reason: &'static str, message: &str) -> anyhow::Error {
    tracing::warn!(
        hf_repo,
        reason,
        "catalog model member refused: repository not public"
    );
    anyhow::anyhow!("{message}. A catalog name needs a public, ungated Hugging Face repository")
}

/// A hand-made, active entry. `release_time` defaults to `now`.
fn new_entry(
    scope: &CatalogScope,
    name: CatalogName,
    release_time: Option<DateTime<Utc>>,
    members: NonEmptyVec<DescriptorMember>,
    now: DateTime<Utc>,
    creator: DescriptorEntryCreator,
) -> anyhow::Result<DescriptorEntry> {
    same_build(&members)?;
    let release_time = release_time.unwrap_or(now);
    anyhow::ensure!(release_time <= now, "release_time is in the future");
    Ok(DescriptorEntry::new(
        scope.clone(),
        name,
        DescriptorEntryKind::HandMade,
        DescriptorEntryState::Active,
        release_time,
        now,
        creator,
        members,
    )?)
}

async fn synonyms(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    members: &[DescriptorMember],
    own: &CatalogName,
) -> anyhow::Result<Vec<String>> {
    let digests = members
        .iter()
        .map(|member| member.digest().to_owned())
        .collect::<Vec<_>>();
    let hits = store.find_by_digests(&digests).await?;
    let mut names = hits
        .values()
        .flatten()
        .filter(|hit| {
            &hit.scope == scope
                && hit.state != DescriptorEntryState::Tombstoned
                && hit.name.key() != own.key()
        })
        .map(|hit| hit.name.to_string())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    Ok(names)
}

pub async fn create(
    store: &dyn DescriptorCatalogStore,
    hub: &HubClient,
    scope: &CatalogScope,
    document: &str,
    now: DateTime<Utc>,
    dry_run: bool,
) -> anyhow::Result<Preview> {
    create_with_vault(store, hub, None, scope, document, now, dry_run).await
}

/// Create an entry with access to the organization's vault records.
pub async fn create_with_vault(
    store: &dyn DescriptorCatalogStore,
    hub: &HubClient,
    vault: Option<&dyn VaultStore>,
    scope: &CatalogScope,
    document: &str,
    now: DateTime<Utc>,
    dry_run: bool,
) -> anyhow::Result<Preview> {
    let CreateInput {
        name,
        release_time,
        members,
    } = parse_create(scope, document)?;
    if let Some(taken) = store.get_entry(scope, &name).await? {
        anyhow::bail!(
            "catalog name {} is already held by a {} entry",
            taken.name,
            taken.state
        );
    }
    let (members, pins) = members.resolve(hub, vault, scope, now).await?;
    let entry = new_entry(
        scope,
        name,
        release_time,
        members,
        now,
        DescriptorEntryCreator::Operator,
    )?;
    let synonyms = synonyms(store, scope, &entry.members, &entry.name).await?;
    let entry = create_checked(store, scope, entry, dry_run, true).await?;
    Ok(Preview {
        entry,
        synonyms,
        pins,
    })
}

/// The HTTP path supplies JSON fields and the authenticated creator. Both
/// paths pin models in the same way, and enter the same checks and atomic
/// store create.
pub async fn create_entry(
    store: &dyn DescriptorCatalogStore,
    hub: &HubClient,
    scope: &CatalogScope,
    input: CreateDescriptorEntryRequest,
    creator: DescriptorEntryCreator,
    now: DateTime<Utc>,
) -> Result<DescriptorEntry, CatalogWriteError> {
    create_entry_preflighted(store, hub, None, scope, input, creator, now).await
}

/// The HTTP path with access to org-owned vault records.
pub async fn create_entry_with_vault(
    store: &dyn DescriptorCatalogStore,
    hub: &HubClient,
    vault: &dyn VaultStore,
    scope: &CatalogScope,
    input: CreateDescriptorEntryRequest,
    creator: DescriptorEntryCreator,
    now: DateTime<Utc>,
) -> Result<DescriptorEntry, CatalogWriteError> {
    create_entry_preflighted(store, hub, Some(vault), scope, input, creator, now).await
}

async fn create_entry_preflighted(
    store: &dyn DescriptorCatalogStore,
    hub: &HubClient,
    vault: Option<&dyn VaultStore>,
    scope: &CatalogScope,
    input: CreateDescriptorEntryRequest,
    creator: DescriptorEntryCreator,
    now: DateTime<Utc>,
) -> Result<DescriptorEntry, CatalogWriteError> {
    preflight_create(store, scope, &input.name).await?;
    let entry = build_create(hub, vault, scope, input, now, creator).await?;
    create_checked(store, scope, entry, false, false).await
}

async fn preflight_create(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
) -> Result<(), CatalogWriteError> {
    if let Some(taken) = store
        .get_entry(scope, name)
        .await
        .map_err(CatalogWriteError::Storage)?
    {
        return Err(CatalogWriteError::Conflict(format!(
            "catalog name {} is already held by a {} entry",
            taken.name, taken.state
        )));
    }
    Ok(())
}

async fn create_checked(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    entry: DescriptorEntry,
    dry_run: bool,
    log: bool,
) -> Result<DescriptorEntry, CatalogWriteError> {
    preflight_create(store, scope, &entry.name).await?;
    if !dry_run {
        match store
            .create_entry(&entry)
            .await
            .map_err(CatalogWriteError::Storage)?
        {
            CreateEntryOutcome::Created if log => {
                match entry.members.head().descriptor_kind() {
                    DescriptorKind::Runtime => log_catalog_write(
                        scope,
                        &entry.name,
                        &member_keys(&entry.members),
                        entry.state.as_ref(),
                        "catalog entry created",
                    ),
                    DescriptorKind::Model => log_model_entry(&entry),
                }
                for member in entry.members.iter() {
                    log_model_member(scope, &entry.name, member, "catalog model member added");
                }
            }
            CreateEntryOutcome::Created => {}
            CreateEntryOutcome::NameTaken { name, state } => {
                return Err(CatalogWriteError::Conflict(format!(
                    "catalog name {name} is already held by a {state} entry"
                )));
            }
        }
    }
    Ok(entry)
}

pub async fn add_member(
    store: &dyn DescriptorCatalogStore,
    hub: &HubClient,
    scope: &CatalogScope,
    name: &CatalogName,
    document: &str,
    now: DateTime<Utc>,
    dry_run: bool,
) -> anyhow::Result<Preview> {
    add_member_with_vault(
        store,
        CatalogSources { hub, vault: None },
        scope,
        name,
        document,
        now,
        dry_run,
    )
    .await
}

/// The sources a catalog writer can resolve and check.
pub struct CatalogSources<'a> {
    pub hub: &'a HubClient,
    pub vault: Option<&'a dyn VaultStore>,
}

/// Append a member with access to the organization's vault records.
pub async fn add_member_with_vault(
    store: &dyn DescriptorCatalogStore,
    sources: CatalogSources<'_>,
    scope: &CatalogScope,
    name: &CatalogName,
    document: &str,
    now: DateTime<Utc>,
    dry_run: bool,
) -> anyhow::Result<Preview> {
    // Preserve the CLI's lookup and kind refusals before it parses the file.
    let entry = live_entry(store, scope, name).await?;
    anyhow::ensure!(
        entry.kind == DescriptorEntryKind::HandMade,
        "only a hand-made catalog entry accepts add-member"
    );
    let (added, pins) = parse_member(document)?
        .resolve(sources.hub, sources.vault, scope, now)
        .await?;
    let member = added.head().clone();
    let synonyms = synonyms(store, scope, std::slice::from_ref(&member), name).await?;
    let entry = add_checked(store, scope, name, member, dry_run, true).await?;
    Ok(Preview {
        entry,
        synonyms,
        pins,
    })
}

pub async fn add_entry_member(
    store: &dyn DescriptorCatalogStore,
    hub: &HubClient,
    scope: &CatalogScope,
    name: &CatalogName,
    input: AddDescriptorMemberRequest,
    now: DateTime<Utc>,
) -> Result<DescriptorEntry, CatalogWriteError> {
    add_entry_member_preflighted(store, hub, None, scope, name, input, now).await
}

/// The HTTP path with access to org-owned vault records.
pub async fn add_entry_member_with_vault(
    store: &dyn DescriptorCatalogStore,
    hub: &HubClient,
    vault: &dyn VaultStore,
    scope: &CatalogScope,
    name: &CatalogName,
    input: AddDescriptorMemberRequest,
    now: DateTime<Utc>,
) -> Result<DescriptorEntry, CatalogWriteError> {
    add_entry_member_preflighted(store, hub, Some(vault), scope, name, input, now).await
}

async fn add_entry_member_preflighted(
    store: &dyn DescriptorCatalogStore,
    hub: &HubClient,
    vault: Option<&dyn VaultStore>,
    scope: &CatalogScope,
    name: &CatalogName,
    input: AddDescriptorMemberRequest,
    now: DateTime<Utc>,
) -> Result<DescriptorEntry, CatalogWriteError> {
    let input = MemberInput::of_member(input).map_err(CatalogWriteError::Invalid)?;
    preflight_add(store, scope, name, input.descriptor_kind()).await?;
    let (added, _) = input
        .resolve(hub, vault, scope, now)
        .await
        .map_err(resolve_error)?;
    let member = added.head().clone();
    add_checked(store, scope, name, member, false, false).await
}

async fn preflight_add(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
    kind: DescriptorKind,
) -> Result<(), CatalogWriteError> {
    let entry = store
        .get_entry(scope, name)
        .await
        .map_err(CatalogWriteError::Storage)?
        .ok_or_else(|| CatalogWriteError::NotFound(format!("no catalog entry {name}")))?;
    if entry.state == DescriptorEntryState::Tombstoned {
        return Err(CatalogWriteError::Conflict(format!(
            "catalog entry {name} is tombstoned"
        )));
    }
    if entry.kind != DescriptorEntryKind::HandMade {
        return Err(CatalogWriteError::Invalid(anyhow::anyhow!(
            "only a hand-made catalog entry accepts add-member"
        )));
    }
    if entry.descriptor_kind() != kind {
        return Err(CatalogWriteError::Invalid(anyhow::anyhow!(
            "member and entry mix runtime and model descriptors"
        )));
    }
    Ok(())
}

async fn add_checked(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
    member: DescriptorMember,
    dry_run: bool,
    log: bool,
) -> Result<DescriptorEntry, CatalogWriteError> {
    let mut entry = store
        .get_entry(scope, name)
        .await
        .map_err(CatalogWriteError::Storage)?
        .ok_or_else(|| CatalogWriteError::NotFound(format!("no catalog entry {name}")))?;
    if entry.state == DescriptorEntryState::Tombstoned {
        return Err(CatalogWriteError::Conflict(format!(
            "catalog entry {name} is tombstoned"
        )));
    }
    if entry.kind != DescriptorEntryKind::HandMade {
        return Err(CatalogWriteError::Invalid(anyhow::anyhow!(
            "only a hand-made catalog entry accepts add-member"
        )));
    }
    let mut all = entry.members.iter().cloned().collect::<Vec<_>>();
    all.push(member.clone());
    let members = NonEmptyVec::try_new(all).map_err(|e| CatalogWriteError::Invalid(e.into()))?;
    DescriptorEntry::validate_members(&members).map_err(|error| match error {
        EntryError::DuplicateFlavor(_)
        | EntryError::DuplicateModelKey { .. }
        | EntryError::DuplicateDigest(_) => CatalogWriteError::Conflict(error.to_string()),
        other @ (EntryError::Tombstoned
        | EntryError::OrgKind
        | EntryError::ModelKind
        | EntryError::CreatorScope
        | EntryError::DescriptorKindMismatch
        | EntryError::RuntimeTypeMismatch
        | EntryError::FormatTypeClash { .. }
        | EntryError::SpeculativeMix) => CatalogWriteError::Invalid(other.into()),
    })?;
    same_build(&members).map_err(CatalogWriteError::Invalid)?;
    let mut stored = None;
    if !dry_run {
        match store
            .append_members(
                scope,
                name,
                NonEmptyVec::try_new(vec![member.clone()])
                    .map_err(|e| CatalogWriteError::Invalid(e.into()))?,
            )
            .await
            .map_err(CatalogWriteError::Storage)?
        {
            AppendOutcome::Appended(committed) if log => {
                match member.descriptor_kind() {
                    DescriptorKind::Runtime => log_catalog_write(
                        scope,
                        &entry.name,
                        member.flavor().unwrap_or("-"),
                        entry.state.as_ref(),
                        "catalog member appended",
                    ),
                    DescriptorKind::Model => {
                        log_model_member(scope, &entry.name, &member, "catalog member appended");
                    }
                }
                stored = Some(committed);
            }
            AppendOutcome::Appended(committed) => stored = Some(committed),
            AppendOutcome::NoSuchEntry => {
                return Err(CatalogWriteError::NotFound(format!(
                    "no catalog entry {name}"
                )));
            }
            AppendOutcome::Tombstoned => {
                return Err(CatalogWriteError::Conflict(format!(
                    "catalog entry {name} is tombstoned"
                )));
            }
            AppendOutcome::DescriptorKindMismatch => {
                return Err(CatalogWriteError::Invalid(anyhow::anyhow!(
                    "member and entry mix runtime and model descriptors"
                )));
            }
            AppendOutcome::RuntimeTypeMismatch => {
                return Err(CatalogWriteError::Invalid(anyhow::anyhow!(
                    "member has a different runtime type"
                )));
            }
            AppendOutcome::DuplicateFlavor(flavor) => {
                return Err(CatalogWriteError::Conflict(format!(
                    "duplicate member flavor {flavor}"
                )));
            }
            AppendOutcome::DuplicateModelKey {
                format,
                quantization,
            } => {
                return Err(CatalogWriteError::Conflict(format!(
                    "duplicate member format and quantization {format}::{quantization}"
                )));
            }
            AppendOutcome::FormatTypeClash {
                format,
                first,
                second,
            } => {
                return Err(CatalogWriteError::Invalid(anyhow::anyhow!(
                    "model types {first} and {second} share the format {format}"
                )));
            }
            AppendOutcome::SpeculativeMix => {
                return Err(CatalogWriteError::Invalid(anyhow::anyhow!(
                    "member mixes speculative and plain model types"
                )));
            }
            AppendOutcome::DuplicateDigest(digest) => {
                return Err(CatalogWriteError::Conflict(format!(
                    "duplicate member digest {digest}"
                )));
            }
        }
    }
    entry.members = members;
    Ok(stored.unwrap_or(entry))
}

pub async fn show(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
) -> anyhow::Result<Option<DescriptorEntry>> {
    Ok(store
        .get_entry(scope, name)
        .await?
        .filter(|entry| entry.state != DescriptorEntryState::Tombstoned))
}

/// Read an entry that a write command changes. A tombstoned name fails with
/// its own message, so the operator can tell it from a name that never existed.
pub async fn live_entry(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
) -> anyhow::Result<DescriptorEntry> {
    let entry = store
        .get_entry(scope, name)
        .await?
        .with_context(|| format!("no catalog entry {name}"))?;
    anyhow::ensure!(
        entry.state != DescriptorEntryState::Tombstoned,
        "catalog entry {name} is tombstoned"
    );
    Ok(entry)
}

pub async fn list(
    store: &dyn DescriptorCatalogStore,
    mut filter: DescriptorEntryFilter,
    after: Option<&CatalogEntryKey>,
    limit: NonZeroUsize,
) -> anyhow::Result<DescriptorEntryPage> {
    anyhow::ensure!(
        !filter.states.contains(&DescriptorEntryState::Tombstoned),
        "tombstoned entries are not listed"
    );
    if filter.states.is_empty() {
        filter.states = [
            DescriptorEntryState::Pinned,
            DescriptorEntryState::Active,
            DescriptorEntryState::Hidden,
        ]
        .into();
    }
    store.list_entries(&filter, after, limit).await
}

/// A state change that an operator command asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogTransition {
    Pin,
    Unpin,
    Hide,
    Unhide,
}

impl CatalogTransition {
    /// The state that the entry must have, and the state that it moves to.
    pub fn states(self) -> (DescriptorEntryState, DescriptorEntryState) {
        match self {
            Self::Pin => (DescriptorEntryState::Active, DescriptorEntryState::Pinned),
            Self::Unpin => (DescriptorEntryState::Pinned, DescriptorEntryState::Active),
            Self::Hide => (DescriptorEntryState::Active, DescriptorEntryState::Hidden),
            Self::Unhide => (DescriptorEntryState::Hidden, DescriptorEntryState::Active),
        }
    }
}

pub async fn transition(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
    change: CatalogTransition,
) -> anyhow::Result<()> {
    transition_entry(store, scope, name, change).await?;
    let (_, target) = change.states();
    let event = match change {
        CatalogTransition::Pin => "catalog entry pinned",
        CatalogTransition::Unpin => "catalog entry unpinned",
        CatalogTransition::Hide => "catalog entry hidden",
        CatalogTransition::Unhide => "catalog entry unhidden",
    };
    log_catalog_write(scope, name, "all", target.as_ref(), event);
    Ok(())
}

pub async fn transition_entry(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
    change: CatalogTransition,
) -> Result<(), CatalogWriteError> {
    let (expected, target) = change.states();
    match store
        .transition_state(scope, name, expected, target)
        .await
        .map_err(CatalogWriteError::Storage)?
    {
        TransitionOutcome::Changed => Ok(()),
        TransitionOutcome::NoSuchEntry => Err(CatalogWriteError::NotFound(format!(
            "no catalog entry {name}"
        ))),
        TransitionOutcome::Tombstoned => Err(CatalogWriteError::Conflict(format!(
            "catalog entry {name} is tombstoned"
        ))),
        TransitionOutcome::UnexpectedState(actual) => Err(CatalogWriteError::Conflict(format!(
            "catalog entry {name} is {actual}; expected {expected}"
        ))),
    }
}

pub async fn tombstone(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
) -> anyhow::Result<()> {
    tombstone_entry(store, scope, name).await?;
    log_catalog_write(scope, name, "all", "tombstoned", "catalog entry tombstoned");
    Ok(())
}

pub async fn tombstone_entry(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
) -> Result<(), CatalogWriteError> {
    match store
        .set_state(scope, name, DescriptorEntryState::Tombstoned)
        .await
        .map_err(CatalogWriteError::Storage)?
    {
        SetStateOutcome::Changed { .. } => Ok(()),
        SetStateOutcome::NoSuchEntry => Err(CatalogWriteError::NotFound(format!(
            "no catalog entry {name}"
        ))),
        SetStateOutcome::Tombstoned | SetStateOutcome::Unchanged => Err(
            CatalogWriteError::Conflict(format!("catalog entry {name} is already tombstoned")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};

    use rstest::rstest;
    use serde_json::json;

    use pipette_hub_testkit::{MockFile, MockGate, MockHub, MockRepo, PR_REF};

    use super::*;

    const RUNTIME: &str = r#"{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b1", flavor = "macos-arm64" }"#;
    const GGUF: &str = "{ type = 'gguf_text', source = 'huggingface', org = 'acme', repo_name = 'AcmeLM-VL-3B-GGUF', path = 'AcmeLM-Q4_0.gguf' }";

    /// A Hub client at an address where nothing listens. A runtime file
    /// makes no Hub call.
    fn no_hub() -> anyhow::Result<HubClient> {
        Ok(HubClient::new(
            "http://127.0.0.1:9/".parse()?,
            NonZeroUsize::MIN,
        )?)
    }

    /// The entry that `document` creates in an empty store, written nowhere.
    async fn dry_run_entry(document: &str, now: DateTime<Utc>) -> anyhow::Result<DescriptorEntry> {
        let store = Memory::default();
        let preview = create(
            &store,
            &no_hub()?,
            &CatalogScope::Global,
            document,
            now,
            true,
        )
        .await?;
        Ok(preview.entry)
    }

    #[rstest]
    #[case::leading_dot(".bad", false)]
    #[case::colon("bad:name", false)]
    #[case::reserved_prefix("LLAMACPP-v0.1", false)]
    #[case::plain("Fork", true)]
    fn create_checks_the_name(#[case] name: &str, #[case] ok: bool) {
        let document = format!("name = '{name}'\nruntimes = [{RUNTIME}]");
        assert_eq!(parse_create(&CatalogScope::Global, &document).is_ok(), ok);
    }

    #[test]
    fn an_org_can_use_a_global_importer_pattern() -> anyhow::Result<()> {
        let scope = CatalogScope::Org(pipette_plan_types::OrgId::try_new("org_acme")?);
        let document = format!("name = 'llamacpp-v0.7.0'\nruntimes = [{RUNTIME}]");
        assert!(parse_create(&CatalogScope::Global, &document).is_err());
        assert!(parse_create(&scope, &document).is_ok());
        Ok(())
    }

    #[rstest]
    #[case::neither("name = 'Fork'", "needs `runtimes` or `models`")]
    #[case::both(
        &format!("name = 'Fork'\nruntimes = [{RUNTIME}]\nmodels = [{{ quantization = 'q4_0', model = {GGUF} }}]"),
        "not both"
    )]
    fn create_file_has_one_kind_of_member(#[case] document: &str, #[case] refusal: &str) {
        let parsed = parse_create(&CatalogScope::Global, document);
        assert!(parsed.is_err_and(|error| error.to_string().contains(refusal)));
    }

    /// A JSON request takes the same one-kind checks as a file.
    #[rstest]
    #[case::create_neither(json!({ "name": "Fork" }), "needs `runtimes` or `models`")]
    #[case::create_both(
        json!({ "name": "Fork", "runtimes": [json_runtime()], "models": [{ "quantization": "q4_0", "model": json_gguf() }] }),
        "not both"
    )]
    #[case::member_without_quantization(json!({ "model": json_gguf() }), "needs `quantization`")]
    #[case::member_runtime_with_quantization(
        json!({ "runtime": json_runtime(), "quantization": "q4_0" }),
        "cannot have `quantization`"
    )]
    #[case::member_neither(json!({}), "needs `runtime` or `model`")]
    #[tokio::test]
    async fn requests_have_one_kind_of_member(
        #[case] body: serde_json::Value,
        #[case] refusal: &str,
    ) -> anyhow::Result<()> {
        let store = Memory::default();
        let scope = CatalogScope::Global;
        let hub = no_hub()?;
        let now = Utc::now();
        let refused = if body.get("name").is_some() {
            let input = serde_json::from_value(body)?;
            create_entry(
                &store,
                &hub,
                &scope,
                input,
                DescriptorEntryCreator::Operator,
                now,
            )
            .await
            .err()
        } else {
            let name = CatalogName::try_new("Fork")?;
            let input = serde_json::from_value(body)?;
            add_entry_member(&store, &hub, &scope, &name, input, now)
                .await
                .err()
        };
        assert!(
            matches!(&refused, Some(CatalogWriteError::Invalid(error)) if error.to_string().contains(refusal)),
            "{refused:?}"
        );
        Ok(())
    }

    fn json_runtime() -> serde_json::Value {
        json!({
            "type": "llamacpp_cli_stock_tools", "source": "github_release",
            "repository_url": "github.com/ggml-org/llama.cpp", "repository_version": "b1",
            "flavor": "macos-arm64"
        })
    }

    fn json_gguf() -> serde_json::Value {
        json!({
            "type": "gguf_text", "source": "huggingface", "org": "acme",
            "repo_name": "AcmeLM-VL-3B-GGUF", "path": "AcmeLM-Q4_0.gguf"
        })
    }

    #[rstest]
    #[case::quoted("'2026-10-20T00:00:00Z'")]
    #[case::native_toml_datetime("2026-10-20T00:00:00Z")]
    fn create_reads_the_release_time(#[case] release_time: &str) -> anyhow::Result<()> {
        let document =
            format!("name = 'Fork'\nrelease_time = {release_time}\nruntimes = [{RUNTIME}]");
        let dated = parse_create(&CatalogScope::Global, &document)?;
        assert_eq!(
            dated.release_time.map(|time| time.to_rfc3339()),
            Some("2026-10-20T00:00:00+00:00".to_owned())
        );
        Ok(())
    }

    #[tokio::test]
    async fn create_checks_the_release_time_and_builds() -> anyhow::Result<()> {
        let now = Utc::now();
        let entry = dry_run_entry(&format!("name = 'Fork'\nruntimes = [{RUNTIME}]"), now).await?;
        assert_eq!(entry.release_time, entry.created_at);
        let future =
            format!("name = 'Fork'\nrelease_time = 2062-10-20T00:00:00Z\nruntimes = [{RUNTIME}]");
        assert!(
            dry_run_entry(&future, now)
                .await
                .is_err_and(|error| error.to_string().contains("future"))
        );
        let other = RUNTIME
            .replace("b1", "b2")
            .replace("macos-arm64", "linux-x64");
        assert!(
            dry_run_entry(
                &format!("name = 'Fork'\nruntimes = [{RUNTIME}, {other}]"),
                now
            )
            .await
            .is_err()
        );
        Ok(())
    }

    #[rstest]
    #[case::pin(
        CatalogTransition::Pin,
        DescriptorEntryState::Active,
        DescriptorEntryState::Pinned
    )]
    #[case::unpin(
        CatalogTransition::Unpin,
        DescriptorEntryState::Pinned,
        DescriptorEntryState::Active
    )]
    #[case::hide(
        CatalogTransition::Hide,
        DescriptorEntryState::Active,
        DescriptorEntryState::Hidden
    )]
    #[case::unhide(
        CatalogTransition::Unhide,
        DescriptorEntryState::Hidden,
        DescriptorEntryState::Active
    )]
    fn each_transition_moves_from_one_state_to_another(
        #[case] change: CatalogTransition,
        #[case] expected: DescriptorEntryState,
        #[case] target: DescriptorEntryState,
    ) {
        assert_eq!(change.states(), (expected, target));
    }

    const VAULT: &str = "runtime = { type = 'llamacpp_cli_stock_tools', source = 'vault_remote_archive', url = '/vault/resources/vault_1/content', principal_id = 'org_1', flavor = 'linux-x64-rocm' }";

    #[rstest]
    #[case::one_runtime(format!("runtime = {RUNTIME}"), None)]
    #[case::runtimes_array(format!("runtimes = [{RUNTIME}]"), Some("invalid catalog member file"))]
    #[case::vault_source(VAULT.to_owned(), Some("vault source"))]
    #[case::position_field(
        format!("runtime = {RUNTIME}\nposition = 0"),
        Some("invalid catalog member file")
    )]
    #[case::runtime_quantization(
        format!("runtime = {RUNTIME}\nquantization = 'q4_0'"),
        Some("cannot have `quantization`")
    )]
    #[case::model_without_quantization(format!("model = {GGUF}"), Some("needs `quantization`"))]
    #[case::runtime_and_model(
        format!("runtime = {RUNTIME}\nmodel = {GGUF}\nquantization = 'q4_0'"),
        Some("not both")
    )]
    #[case::bad_quantization(
        format!("model = {GGUF}\nquantization = 'Q4_0'"),
        Some("invalid catalog member file")
    )]
    #[tokio::test]
    async fn member_file_has_exactly_one_member(
        #[case] document: String,
        #[case] refusal: Option<&str>,
    ) -> anyhow::Result<()> {
        let parsed = match parse_member(&document) {
            Ok(input) => input
                .resolve(&no_hub()?, None, &CatalogScope::Global, Utc::now())
                .await
                .map(|_| ()),
            Err(error) => Err(error),
        };
        match refusal {
            None => assert!(parsed.is_ok(), "{parsed:?}"),
            Some(text) => {
                assert!(
                    parsed
                        .as_ref()
                        .is_err_and(|error| format!("{error:#}").contains(text)),
                    "{parsed:?}"
                );
            }
        }
        Ok(())
    }

    #[derive(Default)]
    struct Memory(
        Mutex<BTreeMap<String, DescriptorEntry>>,
        AtomicBool,
        Mutex<Option<Arc<tokio::sync::Barrier>>>,
    );

    impl Memory {
        fn entries(&self) -> anyhow::Result<MutexGuard<'_, BTreeMap<String, DescriptorEntry>>> {
            self.0
                .lock()
                .map_err(|_| anyhow::anyhow!("memory store poisoned"))
        }
    }

    #[async_trait::async_trait]
    impl DescriptorCatalogStore for Memory {
        async fn lock_importer_entries(
            &self,
        ) -> anyhow::Result<Box<dyn pipette_store::CatalogEntryLock>> {
            Ok(Box::new(pipette_store::NoopCatalogEntryLock))
        }

        async fn create_entry(
            &self,
            entry: &DescriptorEntry,
        ) -> anyhow::Result<CreateEntryOutcome> {
            let mut entries = self.entries()?;
            if let Some(old) = entries.get(&entry.name.key()) {
                return Ok(CreateEntryOutcome::NameTaken {
                    name: old.name.clone(),
                    state: old.state,
                });
            }
            entries.insert(entry.name.key(), entry.clone());
            Ok(CreateEntryOutcome::Created)
        }

        async fn append_members(
            &self,
            _scope: &CatalogScope,
            name: &CatalogName,
            members: NonEmptyVec<DescriptorMember>,
        ) -> anyhow::Result<AppendOutcome> {
            let mut entries = self.entries()?;
            let Some(entry) = entries.get_mut(&name.key()) else {
                return Ok(AppendOutcome::NoSuchEntry);
            };
            if entry.state == DescriptorEntryState::Tombstoned {
                return Ok(AppendOutcome::Tombstoned);
            }
            let mut all = entry.members.iter().cloned().collect::<Vec<_>>();
            all.extend(members.into_inner());
            entry.members = NonEmptyVec::try_new(all)?;
            Ok(AppendOutcome::Appended(entry.clone()))
        }

        async fn set_state(
            &self,
            _scope: &CatalogScope,
            name: &CatalogName,
            state: DescriptorEntryState,
        ) -> anyhow::Result<SetStateOutcome> {
            let mut entries = self.entries()?;
            let Some(entry) = entries.get_mut(&name.key()) else {
                return Ok(SetStateOutcome::NoSuchEntry);
            };
            let old = entry.state;
            if old == DescriptorEntryState::Tombstoned {
                return Ok(SetStateOutcome::Tombstoned);
            }
            if old == state {
                return Ok(SetStateOutcome::Unchanged);
            }
            entry.state = state;
            Ok(SetStateOutcome::Changed { from: old })
        }

        async fn transition_state(
            &self,
            _scope: &CatalogScope,
            name: &CatalogName,
            expected: DescriptorEntryState,
            state: DescriptorEntryState,
        ) -> anyhow::Result<TransitionOutcome> {
            let mut entries = self.entries()?;
            let Some(entry) = entries.get_mut(&name.key()) else {
                return Ok(TransitionOutcome::NoSuchEntry);
            };
            if entry.state == DescriptorEntryState::Tombstoned {
                return Ok(TransitionOutcome::Tombstoned);
            }
            if entry.state != expected {
                return Ok(TransitionOutcome::UnexpectedState(entry.state));
            }
            entry.state = state;
            Ok(TransitionOutcome::Changed)
        }

        async fn transition_state_under_importer_lock(
            &self,
            scope: &CatalogScope,
            name: &CatalogName,
            expected: DescriptorEntryState,
            state: DescriptorEntryState,
        ) -> anyhow::Result<TransitionOutcome> {
            self.transition_state(scope, name, expected, state).await
        }

        async fn get_entry(
            &self,
            _scope: &CatalogScope,
            name: &CatalogName,
        ) -> anyhow::Result<Option<DescriptorEntry>> {
            let entry = self.entries()?.get(&name.key()).cloned();
            let barrier = self
                .2
                .lock()
                .map_err(|_| anyhow::anyhow!("barrier lock poisoned"))?
                .clone();
            if let Some(barrier) = barrier {
                barrier.wait().await;
            }
            Ok(entry)
        }

        async fn list_entries(
            &self,
            filter: &DescriptorEntryFilter,
            _after: Option<&CatalogEntryKey>,
            _limit: NonZeroUsize,
        ) -> anyhow::Result<DescriptorEntryPage> {
            let items = self
                .entries()?
                .values()
                .filter(|entry| filter.states.is_empty() || filter.states.contains(&entry.state))
                .cloned()
                .collect();
            Ok(DescriptorEntryPage {
                items,
                ..Default::default()
            })
        }

        async fn find_by_digests(
            &self,
            digests: &[String],
        ) -> anyhow::Result<HashMap<String, Vec<pipette_store::DigestHit>>> {
            if self.1.load(Ordering::Relaxed) {
                anyhow::bail!("digest lookup unavailable");
            }
            let entries = self.entries()?;
            let hits = entries
                .values()
                .flat_map(|entry| entry.members.iter().map(move |member| (entry, member)))
                .filter(|(_, member)| digests.iter().any(|digest| digest == member.digest()))
                .fold(
                    HashMap::<String, Vec<pipette_store::DigestHit>>::new(),
                    |mut hits, (entry, member)| {
                        hits.entry(member.digest().to_owned()).or_default().push(
                            pipette_store::DigestHit {
                                scope: entry.scope.clone(),
                                name: entry.name.clone(),
                                kind: entry.kind,
                                state: entry.state,
                                member_key: member.key(),
                            },
                        );
                        hits
                    },
                );
            Ok(hits)
        }

        async fn live_entries_in_org(
            &self,
            _org_id: &pipette_plan_types::OrgId,
        ) -> anyhow::Result<Vec<CatalogName>> {
            Ok(Vec::new())
        }
        async fn delete_tombstones_in_org(
            &self,
            _org_id: &pipette_plan_types::OrgId,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn http_writes_do_not_require_cli_synonyms() -> anyhow::Result<()> {
        let store = Memory::default();
        store.1.store(true, Ordering::Relaxed);
        let scope = CatalogScope::Global;
        let now = Utc::now();
        let input = serde_json::from_value(serde_json::json!({
            "name": "Fork",
            "runtimes": [{
                "type": "llamacpp_cli_stock_tools",
                "source": "github_release",
                "repository_url": "github.com/ggml-org/llama.cpp",
                "repository_version": "b1",
                "flavor": "macos-arm64"
            }]
        }))?;
        let created = create_entry(
            &store,
            &no_hub()?,
            &scope,
            input,
            DescriptorEntryCreator::Operator,
            now,
        )
        .await?;
        assert_eq!(created.members.len(), 1);

        let input = serde_json::from_value(serde_json::json!({
            "runtime": {
                "type": "llamacpp_cli_stock_tools",
                "source": "github_release",
                "repository_url": "github.com/ggml-org/llama.cpp",
                "repository_version": "b1",
                "flavor": "linux-x64"
            }
        }))?;
        let updated =
            add_entry_member(&store, &no_hub()?, &scope, &created.name, input, now).await?;
        assert_eq!(updated.members.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn overlapping_http_appends_return_committed_members() -> anyhow::Result<()> {
        let store = Memory::default();
        let scope = CatalogScope::Global;
        let now = Utc::now();
        let input = serde_json::from_value(serde_json::json!({
            "name": "Fork",
            "runtimes": [{
                "type": "llamacpp_cli_stock_tools",
                "source": "github_release",
                "repository_url": "github.com/ggml-org/llama.cpp",
                "repository_version": "b1",
                "flavor": "macos-arm64"
            }]
        }))?;
        let entry = create_entry(
            &store,
            &no_hub()?,
            &scope,
            input,
            DescriptorEntryCreator::Operator,
            now,
        )
        .await?;
        *store
            .2
            .lock()
            .map_err(|_| anyhow::anyhow!("barrier lock poisoned"))? =
            Some(Arc::new(tokio::sync::Barrier::new(2)));
        let input = |flavor| {
            serde_json::from_value(serde_json::json!({
                "runtime": {
                    "type": "llamacpp_cli_stock_tools",
                    "source": "github_release",
                    "repository_url": "github.com/ggml-org/llama.cpp",
                    "repository_version": "b1",
                    "flavor": flavor
                }
            }))
        };
        let hub = no_hub()?;
        let (first, second) = tokio::join!(
            add_entry_member(&store, &hub, &scope, &entry.name, input("linux-x64")?, now),
            add_entry_member(&store, &hub, &scope, &entry.name, input("custom")?, now),
        );
        let mut sizes = [first?.members.len(), second?.members.len()];
        sizes.sort_unstable();
        assert_eq!(sizes, [2, 3]);
        Ok(())
    }

    #[tokio::test]
    async fn commands_keep_live_members_and_hide_tombstones() -> anyhow::Result<()> {
        let store = Memory::default();
        let hub = no_hub()?;
        let now = Utc::now();
        let name = CatalogName::try_new("Fork")?;
        let document = format!("name = 'Fork'\nruntimes = [{RUNTIME}]");
        let preview = create(&store, &hub, &CatalogScope::Global, &document, now, true).await?;
        assert_eq!(preview.entry.name, name);
        assert!(
            store
                .get_entry(&CatalogScope::Global, &name)
                .await?
                .is_none()
        );
        create(&store, &hub, &CatalogScope::Global, &document, now, false).await?;
        assert!(
            create(
                &store,
                &hub,
                &CatalogScope::Global,
                &document.replace("Fork", "fOrK"),
                now,
                false
            )
            .await
            .is_err()
        );
        let other = create(
            &store,
            &hub,
            &CatalogScope::Global,
            &document.replace("Fork", "Other"),
            now,
            false,
        )
        .await?;
        assert_eq!(other.synonyms, ["Fork"]);
        let mut imported = dry_run_entry(&document.replace("Fork", "Imported"), now).await?;
        imported.kind = DescriptorEntryKind::Stable;
        imported.created_by = DescriptorEntryCreator::ImporterLlamacpp;
        assert_eq!(
            store.create_entry(&imported).await?,
            CreateEntryOutcome::Created
        );

        let second = RUNTIME.replace("macos-arm64", "linux-x64");
        let append = format!("runtime = {second}");
        let preview = add_member(
            &store,
            &hub,
            &CatalogScope::Global,
            &name,
            &append,
            now,
            true,
        )
        .await?;
        assert_eq!(preview.entry.members.len(), 2);
        assert_eq!(
            store
                .get_entry(&CatalogScope::Global, &name)
                .await?
                .context("entry")?
                .members
                .len(),
            1
        );
        add_member(
            &store,
            &hub,
            &CatalogScope::Global,
            &name,
            &append,
            now,
            false,
        )
        .await?;
        assert_eq!(
            store
                .get_entry(&CatalogScope::Global, &name)
                .await?
                .context("entry")?
                .members
                .len(),
            2
        );
        let wrong_build = append
            .replace("b1", "b2")
            .replace("linux-x64", "windows-x64");
        let refusals = [
            (&imported.name, &append, "only a hand-made catalog entry"),
            (&name, &append, "duplicate member"),
            (&name, &wrong_build, "differ only in flavor"),
        ];
        for (target, document, message) in refusals {
            let refusal = add_member(
                &store,
                &hub,
                &CatalogScope::Global,
                target,
                document,
                now,
                false,
            )
            .await;
            assert!(
                refusal.is_err_and(|error| format!("{error:#}").contains(message)),
                "{target}: expected {message:?}"
            );
        }

        assert!(
            transition(
                &store,
                &CatalogScope::Global,
                &name,
                CatalogTransition::Unhide
            )
            .await
            .is_err()
        );
        transition(&store, &CatalogScope::Global, &name, CatalogTransition::Pin).await?;
        assert!(
            transition(
                &store,
                &CatalogScope::Global,
                &name,
                CatalogTransition::Hide
            )
            .await
            .is_err()
        );
        transition(
            &store,
            &CatalogScope::Global,
            &name,
            CatalogTransition::Unpin,
        )
        .await?;
        transition(
            &store,
            &CatalogScope::Global,
            &name,
            CatalogTransition::Hide,
        )
        .await?;
        assert!(show(&store, &CatalogScope::Global, &name).await?.is_some());
        transition(
            &store,
            &CatalogScope::Global,
            &name,
            CatalogTransition::Unhide,
        )
        .await?;
        transition(
            &store,
            &CatalogScope::Global,
            &name,
            CatalogTransition::Hide,
        )
        .await?;
        assert!(
            list(
                &store,
                DescriptorEntryFilter {
                    scopes: [CatalogScope::Global].into(),
                    ..Default::default()
                },
                None,
                NonZeroUsize::MIN
            )
            .await?
            .items
            .iter()
            .any(|entry| entry.name == name)
        );
        tombstone(&store, &CatalogScope::Global, &name).await?;
        assert!(show(&store, &CatalogScope::Global, &name).await?.is_none());
        let refusal = add_member(
            &store,
            &hub,
            &CatalogScope::Global,
            &name,
            &append,
            now,
            false,
        )
        .await;
        assert!(refusal.is_err_and(|error| error.to_string().contains("is tombstoned")));
        let refusal =
            transition(&store, &CatalogScope::Global, &name, CatalogTransition::Pin).await;
        assert!(refusal.is_err_and(|error| error.to_string().contains("is tombstoned")));
        let refusal = live_entry(&store, &CatalogScope::Global, &name).await;
        assert!(refusal.is_err_and(|error| error.to_string().contains("is tombstoned")));
        let tombstoned = DescriptorEntryFilter {
            scopes: [CatalogScope::Global].into(),
            states: [DescriptorEntryState::Tombstoned].into(),
            ..Default::default()
        };
        let refusal = list(&store, tombstoned, None, NonZeroUsize::MIN).await;
        assert!(refusal.is_err_and(|error| error.to_string().contains("not listed")));
        assert!(
            !list(
                &store,
                DescriptorEntryFilter {
                    scopes: [CatalogScope::Global].into(),
                    ..Default::default()
                },
                None,
                NonZeroUsize::MIN
            )
            .await?
            .items
            .iter()
            .any(|entry| entry.name == name)
        );
        assert!(
            create(&store, &hub, &CatalogScope::Global, &document, now, false)
                .await
                .is_err()
        );
        Ok(())
    }

    const GGUF_REPO: &str = "acme/AcmeLM-VL-3B-GGUF";
    const MLX_REPO: &str = "acme/AcmeLM-VL-3B-MLX-4bit";

    fn commit(of: char) -> String {
        of.to_string().repeat(40)
    }

    fn gguf_files() -> Vec<MockFile> {
        vec![
            MockFile::lfs("AcmeLM-Q4_0.gguf", b"GGUF q4_0"),
            MockFile::lfs("AcmeLM-Q8_0.gguf", b"GGUF q8_0"),
        ]
    }

    /// Public, ungated GGUF and MLX repositories, and one GGUF repository for
    /// each refusal.
    async fn model_hub() -> anyhow::Result<MockHub> {
        let repo = |of, public, gate, files| MockRepo {
            commit: commit(of),
            files,
            public,
            rate_limited: false,
            gate,
        };
        let mlx_files = vec![
            MockFile::git("config.json", b"{}"),
            MockFile::lfs("model.safetensors", &[4; 16]),
        ];
        MockHub::start(HashMap::from([
            (
                GGUF_REPO.to_owned(),
                repo('a', true, MockGate::Open, gguf_files()),
            ),
            (
                MLX_REPO.to_owned(),
                repo('b', true, MockGate::Open, mlx_files),
            ),
            (
                "acme/Private-GGUF".to_owned(),
                repo('c', false, MockGate::Open, gguf_files()),
            ),
            (
                "acme/Gated-Auto".to_owned(),
                repo('d', true, MockGate::Gated("auto"), gguf_files()),
            ),
            (
                "acme/Gated-Manual".to_owned(),
                repo('e', true, MockGate::Gated("manual"), gguf_files()),
            ),
            (
                "acme/Unreported".to_owned(),
                repo('f', true, MockGate::Unreported, gguf_files()),
            ),
        ]))
        .await
    }

    fn client(hub: &MockHub) -> anyhow::Result<HubClient> {
        Ok(HubClient::new(hub.endpoint.clone(), NonZeroUsize::MIN)?)
    }

    fn gguf(repo_name: &str, path: &str, extra: &str) -> String {
        format!(
            "{{ type = 'gguf_text', source = 'huggingface', org = 'acme', repo_name = '{repo_name}', path = '{path}'{extra} }}"
        )
    }

    fn mlx(extra: &str) -> String {
        format!(
            "{{ type = 'mlx', source = 'huggingface', org = 'acme', repo_name = 'AcmeLM-VL-3B-MLX-4bit'{extra} }}"
        )
    }

    fn model_entry(name: &str, members: &[(&str, &str)]) -> String {
        let models = members
            .iter()
            .map(|(quantization, model)| {
                format!("{{ quantization = '{quantization}', model = {model} }}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("name = '{name}'\nmodels = [{models}]")
    }

    #[rstest]
    #[case::private(
        gguf("Private-GGUF", "AcmeLM-Q4_0.gguf", ""),
        "401 Unauthorized): the repository is private",
        1
    )]
    #[case::gated_auto(
        gguf("Gated-Auto", "AcmeLM-Q4_0.gguf", ""),
        "gated repository (auto)",
        1
    )]
    #[case::gated_manual(
        gguf("Gated-Manual", "AcmeLM-Q4_0.gguf", ""),
        "gated repository (manual)",
        1
    )]
    #[case::unreported(
        gguf("Unreported", "AcmeLM-Q4_0.gguf", ""),
        "did not report whether",
        1
    )]
    #[case::unknown_revision(
        gguf("AcmeLM-VL-3B-GGUF", "AcmeLM-Q4_0.gguf", ", revision = 'v9'"),
        "404 Not Found",
        1
    )]
    #[case::missing_file(
        gguf("AcmeLM-VL-3B-GGUF", "Missing.gguf", ""),
        "is not a file at commit",
        1
    )]
    #[case::empty_prefix(mlx(", prefix = 'nothing'"), "has no files at commit", 1)]
    #[case::url(
        "{ type = 'gguf_text', source = 'url', url = 'https://example.com/model.gguf' }".to_owned(),
        "must be a Hugging Face repository",
        0
    )]
    #[case::relative_file(
        "{ type = 'gguf_text', source = 'relative_file', path = 'models/m.gguf' }".to_owned(),
        "must be a Hugging Face repository",
        0
    )]
    #[case::vault(
        "{ type = 'mlx', source = 'vault_archive', url = '/vault/resources/vault_1/content', principal_id = 'org_1' }".to_owned(),
        "a global catalog entry cannot name a vault model",
        0
    )]
    #[case::token(
        gguf(
            "AcmeLM-VL-3B-GGUF",
            "AcmeLM-Q4_0.gguf",
            ", auth_token = 'hf_good_SECRET'"
        ),
        "must not carry an access token",
        0
    )]
    #[case::no_format(
        "{ type = 'apple_foundation_text' }".to_owned(),
        "has no catalog format",
        0
    )]
    #[tokio::test]
    async fn create_refuses_a_model_that_a_global_name_cannot_show(
        #[case] model: String,
        #[case] refusal: &str,
        #[case] requests: usize,
    ) -> anyhow::Result<()> {
        let hub = model_hub().await?;
        let store = Memory::default();
        let document = model_entry("AcmeLM-VL-3B", &[("q4_0", &model)]);
        let created = create(
            &store,
            &client(&hub)?,
            &CatalogScope::Global,
            &document,
            Utc::now(),
            false,
        )
        .await;
        assert!(
            created
                .as_ref()
                .is_err_and(|error| format!("{error:#}").contains(refusal)),
            "{:?}",
            created.err()
        );
        assert_eq!(hub.revision_requests(), requests);
        assert!(store.entries()?.is_empty());
        Ok(())
    }

    #[rstest]
    #[case::token(
        gguf(
            "AcmeLM-VL-3B-GGUF",
            "AcmeLM-Q8_0.gguf",
            ", auth_token = 'hf_good_SECRET'"
        ),
        "must not carry an access token"
    )]
    #[case::url(
        "{ type = 'gguf_text', source = 'url', url = 'https://example.com/model.gguf' }".to_owned(),
        "must be a Hugging Face repository"
    )]
    #[tokio::test]
    async fn create_refuses_a_later_invalid_model_before_calling_the_hub(
        #[case] invalid_model: String,
        #[case] refusal: &str,
    ) -> anyhow::Result<()> {
        let hub = model_hub().await?;
        let store = Memory::default();
        let first = gguf("AcmeLM-VL-3B-GGUF", "AcmeLM-Q4_0.gguf", "");
        let document = model_entry(
            "AcmeLM-VL-3B",
            &[("q4_0", &first), ("q8_0", &invalid_model)],
        );
        let error = create(
            &store,
            &client(&hub)?,
            &CatalogScope::Global,
            &document,
            Utc::now(),
            false,
        )
        .await
        .err()
        .context("the later model must be refused")?;
        assert!(format!("{error:#}").contains(refusal), "{error:#}");
        assert_eq!(hub.revision_requests(), 0);
        assert!(store.entries()?.is_empty());
        Ok(())
    }

    #[rstest]
    #[case::no_revision("", "main")]
    #[case::branch(", revision = 'main'", "main")]
    #[case::pull_request(", revision = 'refs/pr/1'", PR_REF)]
    #[case::commit(&format!(", revision = '{}'", commit('a')), &commit('a'))]
    #[tokio::test]
    async fn create_pins_each_revision_to_its_commit(
        #[case] revision: &str,
        #[case] requested: &str,
    ) -> anyhow::Result<()> {
        let hub = model_hub().await?;
        let store = Memory::default();
        let model = gguf("AcmeLM-VL-3B-GGUF", "AcmeLM-Q4_0.gguf", revision);
        let document = model_entry("AcmeLM-VL-3B", &[("q4_0", &model)]);
        let preview = create(
            &store,
            &client(&hub)?,
            &CatalogScope::Global,
            &document,
            Utc::now(),
            false,
        )
        .await?;
        let pin = CommitPin {
            repo: GGUF_REPO.to_owned(),
            requested: requested.to_owned(),
            commit: GitOid::try_new(commit('a'))?,
        };
        assert_eq!(preview.pins, [pin]);
        let stored = store
            .get_entry(&CatalogScope::Global, &preview.entry.name)
            .await?
            .context("the entry must be stored")?;
        let reference = hub.reference(GGUF_REPO).context("mock repo")?;
        assert_eq!(member_refs(stored.members.head()), reference);
        assert_eq!(member_key(stored.members.head()), "gguf::q4_0");
        Ok(())
    }

    #[tokio::test]
    async fn model_entries_pin_append_and_note_synonyms() -> anyhow::Result<()> {
        let hub = model_hub().await?;
        let client = client(&hub)?;
        let store = Memory::default();
        let scope = CatalogScope::Global;
        let now = Utc::now();
        let name = CatalogName::try_new("AcmeLM-VL-3B")?;
        let q4_0 = gguf("AcmeLM-VL-3B-GGUF", "AcmeLM-Q4_0.gguf", "");
        let q8_0 = gguf("AcmeLM-VL-3B-GGUF", "AcmeLM-Q8_0.gguf", "");
        let document = model_entry("AcmeLM-VL-3B", &[("q4_0", &q4_0)]);

        let dry = create(&store, &client, &scope, &document, now, true).await?;
        assert_eq!(dry.pins.len(), 1);
        assert!(store.entries()?.is_empty());
        create(&store, &client, &scope, &document, now, false).await?;

        let appended = add_member(
            &store,
            &client,
            &scope,
            &name,
            &format!("model = {}\nquantization = '4bit'", mlx("")),
            now,
            false,
        )
        .await?;
        let refs = appended
            .entry
            .members
            .iter()
            .map(member_refs)
            .collect::<Vec<_>>();
        let expected = [GGUF_REPO, MLX_REPO].map(|repo| hub.reference(repo).unwrap_or_default());
        assert_eq!(refs, expected);
        assert_eq!(member_keys(&appended.entry.members), "gguf::q4_0,mlx::4bit");
        assert_eq!(appended.pins.len(), 1);

        let refusals = [
            (
                format!("model = {q8_0}\nquantization = 'q4_0'"),
                "duplicate member format and quantization",
            ),
            (format!("runtime = {RUNTIME}"), "mix runtime and model"),
        ];
        for (member, message) in refusals {
            let refusal = add_member(&store, &client, &scope, &name, &member, now, false).await;
            assert!(
                refusal.is_err_and(|error| format!("{error:#}").contains(message)),
                "expected {message:?}"
            );
        }
        let twice = model_entry("AcmeLM-Twice", &[("q4_0", &q4_0), ("q4_0", &q8_0)]);
        let refusal = create(&store, &client, &scope, &twice, now, false).await;
        assert!(refusal.is_err_and(|error| format!("{error:#}").contains("duplicate member")));

        let synonym = create(
            &store,
            &client,
            &scope,
            &model_entry("AcmeLM-VL-3B-copy", &[("q4_0", &q4_0)]),
            now,
            false,
        )
        .await?;
        assert_eq!(synonym.synonyms, ["AcmeLM-VL-3B"]);
        Ok(())
    }
}
