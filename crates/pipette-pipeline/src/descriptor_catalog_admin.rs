//! Shared administration of readable runtime and model descriptor names.

use std::collections::HashMap;
use std::num::NonZeroUsize;

use anyhow::Context;
use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt};
use serde::Deserialize;

use pipette_plan_types::{
    AddDescriptorMemberRequest, AuditAction, CatalogName, CatalogScope,
    CreateDescriptorEntryRequest, DescriptorEntry, DescriptorEntryCreator, DescriptorEntryKind,
    DescriptorEntryState, DescriptorKind, DescriptorMember, EntryError, GitOid, HfRevision,
    MemberError, Model, ModelFormat, ModelMemberRequest, ModelType, NonEmptyVec, QuantizationLabel,
    Runtime, descriptor_digest, hf_repos, hf_repos_mut, names_vault_source,
};
use pipette_store::{
    AppendOutcome, CatalogEntryKey, CreateEntryOutcome, DescriptorCatalogStore,
    DescriptorEntryFilter, DescriptorEntryPage, MergeOutcome, MergedMembers, RelabelOutcome,
    RenameOutcome, SetStateOutcome, TransitionOutcome, VaultStore, merge_outcome,
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

/// The input of `catalog create` or `catalog add-member`: the text of a TOML
/// file, or a request that the caller built from command-line values.
pub enum Document<'a, T> {
    Toml(&'a str),
    Parsed(T),
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
pub fn member_keys(members: &[DescriptorMember]) -> String {
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

/// Check the invariant shared by operator appends and importer refreshes. The
/// members of a model entry come from the operator's claim that they hold one
/// model, so the check passes them.
pub fn same_build(members: &NonEmptyVec<DescriptorMember>) -> anyhow::Result<()> {
    DescriptorEntry::validate_runtime_build(members)?;
    Ok(())
}

fn parse_create(scope: &CatalogScope, document: &str) -> anyhow::Result<CreateInput> {
    let input: CreateDocument = toml::from_str(document).context("invalid catalog create file")?;
    create_input(
        scope,
        CreateDescriptorEntryRequest {
            name: input.name,
            release_time: input.release_time,
            runtimes: input.runtimes,
            models: input.models,
        },
    )
}

fn create_input(
    scope: &CatalogScope,
    request: CreateDescriptorEntryRequest,
) -> anyhow::Result<CreateInput> {
    check_reserved_name(scope, &request.name)?;
    Ok(CreateInput {
        name: request.name,
        release_time: request.release_time,
        members: MemberInput::of_entry(request.runtimes, request.models)?,
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
    let CreateInput {
        name,
        release_time,
        members,
    } = create_input(scope, input).map_err(CatalogWriteError::Invalid)?;
    let (members, _) = members
        .resolve(hub, vault, scope, now)
        .await
        .map_err(resolve_error)?;
    new_entry(scope, name, release_time, members, now, creator).map_err(CatalogWriteError::Invalid)
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
            Self::Runtimes(runtimes) => {
                let members = futures::stream::iter(runtimes.into_inner())
                    .then(|runtime| async move {
                        let descriptor = serde_json::to_value(&runtime)?;
                        if names_vault_source(&descriptor) {
                            let (org_id, vault) = org_vault(scope, vault)?;
                            check_catalog_vault(vault, org_id, &descriptor).await?;
                        }
                        Ok::<_, anyhow::Error>(DescriptorMember::new(runtime, now)?)
                    })
                    .try_collect::<Vec<_>>()
                    .await?;
                Ok((NonEmptyVec::try_new(members)?, Vec::new()))
            }
            Self::Models(models) => {
                for document in models.iter() {
                    validate_model_source(scope, &document.model)?;
                }
                let mut members = Vec::new();
                let mut pins = Vec::new();
                for document in models.into_inner() {
                    let (member, member_pins) =
                        if names_vault_source(&serde_json::to_value(&document.model)?) {
                            let (org_id, vault) = org_vault(scope, vault)?;
                            let descriptor = serde_json::to_value(&document.model)?;
                            let member = DescriptorMember::new_model(
                                document.model,
                                document.quantization,
                                now,
                            )?;
                            check_catalog_vault(vault, org_id, &descriptor).await?;
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

/// The organization and the vault store that check a vault descriptor of an
/// entry in `scope`. A global entry cannot name a vault source.
fn org_vault<'a>(
    scope: &'a CatalogScope,
    vault: Option<&'a dyn VaultStore>,
) -> anyhow::Result<(&'a pipette_plan_types::OrgId, &'a dyn VaultStore)> {
    let CatalogScope::Org(org_id) = scope else {
        return Err(EntryError::GlobalVaultSource.into());
    };
    let vault = vault.context("the vault store is required for a vault source")?;
    Ok((org_id, vault))
}

async fn check_catalog_vault(
    vault: &dyn VaultStore,
    org_id: &pipette_plan_types::OrgId,
    descriptor: &serde_json::Value,
) -> anyhow::Result<()> {
    vault_check::check(
        vault,
        &VaultScope::Org(org_id.clone()),
        descriptor,
        &mut HashMap::new(),
    )
    .await
    .map_err(anyhow::Error::new)
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
    create_with_vault(
        store,
        hub,
        None,
        scope,
        Document::Toml(document),
        now,
        dry_run,
    )
    .await
}

/// Create an entry with access to the organization's vault records.
pub async fn create_with_vault(
    store: &dyn DescriptorCatalogStore,
    hub: &HubClient,
    vault: Option<&dyn VaultStore>,
    scope: &CatalogScope,
    document: Document<'_, CreateDescriptorEntryRequest>,
    now: DateTime<Utc>,
    dry_run: bool,
) -> anyhow::Result<Preview> {
    let CreateInput {
        name,
        release_time,
        members,
    } = match document {
        Document::Toml(text) => parse_create(scope, text)?,
        Document::Parsed(request) => create_input(scope, request)?,
    };
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
        Document::Toml(document),
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
    document: Document<'_, AddDescriptorMemberRequest>,
    now: DateTime<Utc>,
    dry_run: bool,
) -> anyhow::Result<Preview> {
    // Preserve the CLI's lookup and kind refusals before it parses the file.
    let entry = live_entry(store, scope, name).await?;
    anyhow::ensure!(
        entry.kind == DescriptorEntryKind::HandMade,
        "only a hand-made catalog entry accepts add-member"
    );
    let input = match document {
        Document::Toml(text) => parse_member(text)?,
        Document::Parsed(request) => MemberInput::of_member(request)?,
    };
    let (added, pins) = input
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
        | EntryError::GlobalVaultSource
        | EntryError::DescriptorInvalid(_)
        | EntryError::DescriptorKindMismatch
        | EntryError::RuntimeTypeMismatch
        | EntryError::RuntimeBuildMismatch
        | EntryError::RuntimeBuildInvalid(_)
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

fn log_rename(scope: &CatalogScope, old: &CatalogName, new: &CatalogName) {
    match scope {
        CatalogScope::Global => {
            tracing::info!(catalog_name = %new, old_catalog_name = %old, "catalog entry renamed");
        }
        CatalogScope::Org(org_id) => tracing::info!(
            catalog_name = %new,
            old_catalog_name = %old,
            org_id = %org_id,
            "catalog entry renamed"
        ),
    }
}

fn log_merge(scope: &CatalogScope, old: &CatalogName, into: &CatalogName, merge: &MergedMembers) {
    let moved = member_keys(&merge.moved);
    let skipped = member_keys(&merge.skipped);
    match scope {
        CatalogScope::Global => tracing::info!(
            catalog_name = %into,
            old_catalog_name = %old,
            member_keys = %moved,
            skipped_member_keys = %skipped,
            "catalog entries merged"
        ),
        CatalogScope::Org(org_id) => tracing::info!(
            catalog_name = %into,
            old_catalog_name = %old,
            org_id = %org_id,
            member_keys = %moved,
            skipped_member_keys = %skipped,
            "catalog entries merged"
        ),
    }
}

fn log_relabel(
    scope: &CatalogScope,
    name: &CatalogName,
    format: ModelFormat,
    from: &QuantizationLabel,
    to: &QuantizationLabel,
) {
    match scope {
        CatalogScope::Global => tracing::info!(
            catalog_name = %name,
            format = %format,
            quantization = %to,
            old_quantization = %from,
            "catalog member relabeled"
        ),
        CatalogScope::Org(org_id) => tracing::info!(
            catalog_name = %name,
            org_id = %org_id,
            format = %format,
            quantization = %to,
            old_quantization = %from,
            "catalog member relabeled"
        ),
    }
}

/// The outcome that [`DescriptorCatalogStore::rename_entry`] gives for the
/// entry as it stands, without a write.
pub async fn preview_rename(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
    new_name: &CatalogName,
) -> anyhow::Result<RenameOutcome> {
    let Some(entry) = store.get_entry(scope, name).await? else {
        return Ok(RenameOutcome::NoSuchEntry);
    };
    if entry.state == DescriptorEntryState::Tombstoned {
        return Ok(RenameOutcome::Tombstoned);
    }
    if entry.kind != DescriptorEntryKind::HandMade {
        return Ok(RenameOutcome::ImporterKind(entry.kind));
    }
    if entry.name == *new_name {
        return Ok(RenameOutcome::SameName);
    }
    if new_name.key() != entry.name.key()
        && let Some(holder) = store.get_entry(scope, new_name).await?
    {
        return Ok(RenameOutcome::NameTaken {
            name: holder.name,
            state: holder.state,
        });
    }
    Ok(RenameOutcome::Renamed {
        old_name: entry.name.clone(),
        entry: DescriptorEntry {
            name: new_name.clone(),
            ..entry
        },
    })
}

/// The outcome that [`DescriptorCatalogStore::relabel_member`] gives for the
/// entry as it stands, without a write.
pub async fn preview_relabel(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
    format: ModelFormat,
    from: &QuantizationLabel,
    to: &QuantizationLabel,
) -> anyhow::Result<RelabelOutcome> {
    let Some(entry) = store.get_entry(scope, name).await? else {
        return Ok(RelabelOutcome::NoSuchEntry);
    };
    if entry.state == DescriptorEntryState::Tombstoned {
        return Ok(RelabelOutcome::Tombstoned);
    }
    if entry.descriptor_kind() == DescriptorKind::Runtime {
        return Ok(RelabelOutcome::RuntimeEntry);
    }
    let holds = |label: &QuantizationLabel| {
        entry
            .members
            .iter()
            .any(|member| member.model_key() == Some((format, label)))
    };
    if !holds(from) {
        return Ok(RelabelOutcome::NoSuchMember);
    }
    if from == to {
        return Ok(RelabelOutcome::SameLabel);
    }
    if holds(to) {
        return Ok(RelabelOutcome::LabelTaken);
    }
    let members = entry
        .members
        .iter()
        .map(|member| match member.model_key() {
            Some(key) if key == (format, from) => member.relabeled(to.clone()),
            _ => Some(member.clone()),
        })
        .collect::<Option<Vec<_>>>()
        .context("a model entry holds a runtime member")?;
    Ok(RelabelOutcome::Relabeled(DescriptorEntry {
        members: NonEmptyVec::try_new(members)?,
        ..entry
    }))
}

/// The result of a rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameResult {
    /// The entry has the new name.
    Renamed {
        /// The stored spelling of the old name.
        old_name: CatalogName,
        entry: DescriptorEntry,
    },
    /// Another live hand-made entry held the new name, so that entry got the
    /// members of the renamed entry, and the renamed entry is gone.
    Merged {
        /// The stored spelling of the old name.
        old_name: CatalogName,
        /// The entry that holds the new name, with its new members.
        entry: DescriptorEntry,
        merge: MergedMembers,
    },
}

impl RenameResult {
    /// The entry under the new name.
    pub fn entry(&self) -> &DescriptorEntry {
        match self {
            Self::Renamed { entry, .. } | Self::Merged { entry, .. } => entry,
        }
    }

    pub fn into_entry(self) -> DescriptorEntry {
        match self {
            Self::Renamed { entry, .. } | Self::Merged { entry, .. } => entry,
        }
    }

    /// The audit action that records the stored names of this result.
    pub fn audit_action(&self, scope: &CatalogScope) -> AuditAction {
        match self {
            Self::Renamed { old_name, entry } => AuditAction::CatalogRename {
                scope: scope.clone(),
                old_name: old_name.clone(),
                new_name: entry.name.clone(),
            },
            Self::Merged {
                old_name,
                entry,
                merge,
            } => AuditAction::CatalogMerge {
                scope: scope.clone(),
                old_name: old_name.clone(),
                into_name: entry.name.clone(),
                moved: merge.moved.iter().map(member_key).collect(),
                skipped: merge.skipped.iter().map(member_key).collect(),
            },
        }
    }
}

/// Give a hand-made entry the name `new_name`, and return the entry under it.
/// If another live hand-made entry holds `new_name`, merge the entry into it
/// ([`rename_entry_with_old_name`]). With `dry_run`, check the same things and
/// write nothing.
pub async fn rename_entry(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
    new_name: &CatalogName,
    dry_run: bool,
) -> Result<DescriptorEntry, CatalogWriteError> {
    rename_entry_with_old_name(store, scope, name, new_name, dry_run)
        .await
        .map(RenameResult::into_entry)
}

/// Rename an entry and return its exact stored old spelling with the result.
/// A caller can add a new audit event without changing the request audit record.
///
/// If another live hand-made entry in the scope holds `new_name`, the members
/// of the entry `name` move to the end of that entry, as `add-member` would
/// append them, and the entry `name` is deleted. A member with the digest and
/// the key of a member of that entry is already there, so the merge skips it.
pub async fn rename_entry_with_old_name(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
    new_name: &CatalogName,
    dry_run: bool,
) -> Result<RenameResult, CatalogWriteError> {
    check_reserved_name(scope, new_name).map_err(CatalogWriteError::Invalid)?;
    // A concurrent write can free the name between the rename and the merge.
    // The second attempt then renames the entry.
    for _ in 0..2 {
        let outcome = if dry_run {
            preview_rename(store, scope, name, new_name).await
        } else {
            store.rename_entry(scope, name, new_name).await
        }
        .map_err(CatalogWriteError::Storage)?;
        let (old_name, entry) = match outcome {
            RenameOutcome::Renamed { old_name, entry } => (old_name, entry),
            RenameOutcome::NameTaken {
                name: holder,
                state,
            } if state != DescriptorEntryState::Tombstoned => {
                match merge_into(store, scope, name, &holder, dry_run).await? {
                    Some(merged) => return Ok(merged),
                    None => continue,
                }
            }
            RenameOutcome::NoSuchEntry => {
                return Err(CatalogWriteError::NotFound(format!(
                    "no catalog entry {name}"
                )));
            }
            RenameOutcome::Tombstoned => {
                return Err(CatalogWriteError::Conflict(format!(
                    "catalog entry {name} is tombstoned"
                )));
            }
            RenameOutcome::ImporterKind(kind) => {
                return Err(CatalogWriteError::Conflict(format!(
                    "catalog entry {name} is a {kind} entry; only a hand-made entry gets a new name"
                )));
            }
            RenameOutcome::SameName => {
                return Err(CatalogWriteError::Conflict(format!(
                    "catalog entry {name} already has the name {new_name}"
                )));
            }
            RenameOutcome::NameTaken {
                name: holder,
                state,
            } => {
                return Err(CatalogWriteError::Conflict(format!(
                    "the {state} entry already holds the catalog name {holder}"
                )));
            }
        };
        if !dry_run {
            log_rename(scope, name, &entry.name);
        }
        return Ok(RenameResult::Renamed { old_name, entry });
    }
    Err(CatalogWriteError::Conflict(format!(
        "concurrent writes kept changing the catalog name {new_name}; try again"
    )))
}

/// Merge the entry `name` into the entry `into`. `None` means that no entry
/// holds `into` any more.
async fn merge_into(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
    into: &CatalogName,
    dry_run: bool,
) -> Result<Option<RenameResult>, CatalogWriteError> {
    let refused =
        |reason: &dyn std::fmt::Display| format!("cannot merge {name} into {into}: {reason}");
    let read = |name| async move {
        store
            .get_entry(scope, name)
            .await
            .map_err(CatalogWriteError::Storage)
    };
    let preview = merge_outcome(read(name).await?, read(into).await?);
    let outcome = if dry_run {
        preview
    } else {
        store
            .merge_entry(scope, name, into)
            .await
            .map_err(CatalogWriteError::Storage)?
    };
    match outcome {
        MergeOutcome::Merged {
            old_name,
            entry,
            merge,
        } => {
            if !dry_run {
                log_merge(scope, &old_name, &entry.name, &merge);
            }
            Ok(Some(RenameResult::Merged {
                old_name,
                entry,
                merge,
            }))
        }
        MergeOutcome::TargetMissing => Ok(None),
        MergeOutcome::NoSuchEntry => Err(CatalogWriteError::NotFound(format!(
            "no catalog entry {name}"
        ))),
        MergeOutcome::Tombstoned => Err(CatalogWriteError::Conflict(format!(
            "catalog entry {name} is tombstoned"
        ))),
        MergeOutcome::ImporterKind(kind) => Err(CatalogWriteError::Conflict(format!(
            "catalog entry {name} is a {kind} entry; only a hand-made entry gets a new name"
        ))),
        MergeOutcome::TargetTombstoned => Err(CatalogWriteError::Conflict(format!(
            "the tombstoned entry already holds the catalog name {into}"
        ))),
        MergeOutcome::TargetImporterKind(kind) => Err(CatalogWriteError::Conflict(refused(
            &format_args!("{into} is a {kind} entry, and only a hand-made entry accepts members"),
        ))),
        MergeOutcome::Refused(
            error @ (EntryError::DuplicateFlavor(_)
            | EntryError::DuplicateModelKey { .. }
            | EntryError::DuplicateDigest(_)),
        ) => Err(CatalogWriteError::Conflict(refused(&error))),
        MergeOutcome::Refused(error) => {
            Err(CatalogWriteError::Invalid(anyhow::anyhow!(refused(&error))))
        }
    }
}

/// Give the model member `format::from` of an entry the label `to`, and return
/// the entry. With `dry_run`, check the same things and write nothing.
pub async fn relabel_entry(
    store: &dyn DescriptorCatalogStore,
    scope: &CatalogScope,
    name: &CatalogName,
    format: ModelFormat,
    from: &QuantizationLabel,
    to: &QuantizationLabel,
    dry_run: bool,
) -> Result<DescriptorEntry, CatalogWriteError> {
    let outcome = if dry_run {
        preview_relabel(store, scope, name, format, from, to).await
    } else {
        store.relabel_member(scope, name, format, from, to).await
    }
    .map_err(CatalogWriteError::Storage)?;
    let entry = match outcome {
        RelabelOutcome::Relabeled(entry) => entry,
        RelabelOutcome::NoSuchEntry => {
            return Err(CatalogWriteError::NotFound(format!(
                "no catalog entry {name}"
            )));
        }
        RelabelOutcome::NoSuchMember => {
            return Err(CatalogWriteError::NotFound(format!(
                "catalog entry {name} has no member {format}::{from}"
            )));
        }
        RelabelOutcome::Tombstoned => {
            return Err(CatalogWriteError::Conflict(format!(
                "catalog entry {name} is tombstoned"
            )));
        }
        RelabelOutcome::RuntimeEntry => {
            return Err(CatalogWriteError::Invalid(anyhow::anyhow!(
                "catalog entry {name} holds runtimes, and a runtime member has no quantization label"
            )));
        }
        RelabelOutcome::SameLabel => {
            return Err(CatalogWriteError::Conflict(format!(
                "member {name}::{format}::{from} already has the label {to}"
            )));
        }
        RelabelOutcome::LabelTaken => {
            return Err(CatalogWriteError::Conflict(format!(
                "catalog entry {name} already has a member {format}::{to}"
            )));
        }
    };
    if !dry_run {
        log_relabel(scope, &entry.name, format, from, to);
    }
    Ok(entry)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};

    use rstest::rstest;
    use serde_json::json;

    use pipette_core::vault::VaultOwner;
    use pipette_hub_testkit::{MockFile, MockGate, MockHub, MockRepo, PR_REF};
    use pipette_plan_types::{OrgId, UserId, VaultResourceId};

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

    /// A request from command-line values writes what the same TOML writes.
    #[tokio::test]
    async fn a_parsed_request_writes_what_its_toml_writes() -> anyhow::Result<()> {
        let hub = no_hub()?;
        let now = Utc::now();
        let from_toml =
            dry_run_entry(&format!("name = 'Fork'\nruntimes = [{RUNTIME}]"), now).await?;
        let runtime = |flavor: &str| {
            let mut runtime = json_runtime();
            runtime["flavor"] = json!(flavor);
            serde_json::from_value::<Runtime>(runtime)
        };
        let request = |name: &str| -> anyhow::Result<_> {
            Ok(CreateDescriptorEntryRequest {
                name: CatalogName::try_new(name)?,
                release_time: None,
                runtimes: Some(NonEmptyVec::try_new(vec![runtime("macos-arm64")?])?),
                models: None,
            })
        };
        let store = Memory::default();
        let create = |name| -> anyhow::Result<_> {
            Ok(create_with_vault(
                &store,
                &hub,
                None,
                &CatalogScope::Global,
                Document::Parsed(request(name)?),
                now,
                false,
            ))
        };
        assert!(
            create("llamacpp-v9.0.0")?
                .await
                .is_err_and(|error| error.to_string().contains("reserved"))
        );
        assert_eq!(create("Fork")?.await?.entry, from_toml);

        let appended = add_member_with_vault(
            &store,
            CatalogSources {
                hub: &hub,
                vault: None,
            },
            &CatalogScope::Global,
            &from_toml.name,
            Document::Parsed(AddDescriptorMemberRequest {
                runtime: Some(runtime("linux-x64")?),
                model: None,
                quantization: None,
            }),
            now,
            false,
        )
        .await?;
        let flavors = appended
            .entry
            .members
            .iter()
            .map(|member| member.flavor())
            .collect::<Vec<_>>();
        assert_eq!(flavors, [Some("macos-arm64"), Some("linux-x64")]);
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

    /// A vault runtime table that names `resource` for `principal`.
    fn vault_runtime(resource: &VaultResourceId, principal: &str, flavor: &str) -> String {
        format!(
            "{{ type = 'llamacpp_cli_stock_tools', source = 'vault_remote_archive', \
             url = '/vault/resources/{resource}/content', principal_id = '{principal}', \
             flavor = '{flavor}' }}"
        )
    }

    /// A vault record owned by `owner`, with its object. The record is in
    /// `vault` only when `written`.
    async fn vault_record(
        vault: &dyn VaultStore,
        owner: VaultOwner,
        written: bool,
    ) -> anyhow::Result<VaultResourceId> {
        let sha256 = pipette_store_testkit::vault_blob_sha256()?;
        let bytes = pipette_store_testkit::VAULT_BLOB;
        let resource = pipette_store_testkit::vault_resource_for(
            owner.clone(),
            sha256.clone(),
            bytes.len() as u64,
        )?;
        if written {
            let key = pipette_core::vault::blob_key(&owner, &sha256)?;
            vault
                .put_blob(&key, &sha256, pipette_store_testkit::blob_body(bytes))
                .await?;
            vault.put_resource(&resource).await?;
        }
        Ok(resource.resource_id)
    }

    /// `catalog create --org` and `catalog add-member --org` accept the
    /// organization's own vault runtime. They refuse a resource outside the
    /// organization with the refusal for a resource that does not exist.
    #[tokio::test]
    async fn an_org_file_names_only_the_orgs_own_vault_runtime() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Default::default()
        };
        let stores = pipette_store_fs::build_local_fs_stores(&config)?;
        let vault = &*stores.vault;
        let org_id = OrgId::try_new("org_1")?;
        let scope = CatalogScope::Org(org_id.clone());
        let store = Memory::default();
        let hub = no_hub()?;
        let org = VaultOwner::Org { org_id };

        let own = vault_record(vault, org.clone(), true).await?;
        let created = create_with_vault(
            &store,
            &hub,
            Some(vault),
            &scope,
            Document::Toml(&format!(
                "name = 'Fork'\nruntimes = [{}]",
                vault_runtime(&own, "org_1", "linux-x64-rocm")
            )),
            Utc::now(),
            false,
        )
        .await?;
        let name = created.entry.name;

        // Both writes of `resource`, each refusal with the resource id masked.
        let refusals = |resource: VaultResourceId, principal: &'static str| {
            let create = format!(
                "name = 'Other'\nruntimes = [{}]",
                vault_runtime(&resource, principal, "linux-x64-rocm")
            );
            let add = format!(
                "runtime = {}",
                vault_runtime(&resource, principal, "macos-arm64")
            );
            let (store, hub, scope, name) = (&store, &hub, &scope, &name);
            async move {
                let created = create_with_vault(
                    store,
                    hub,
                    Some(vault),
                    scope,
                    Document::Toml(&create),
                    Utc::now(),
                    true,
                )
                .await
                .err()
                .context("the create is refused")?;
                let added = add_member_with_vault(
                    store,
                    CatalogSources {
                        hub,
                        vault: Some(vault),
                    },
                    scope,
                    name,
                    Document::Toml(&add),
                    Utc::now(),
                    true,
                )
                .await
                .err()
                .context("the add-member is refused")?;
                let id = resource.to_string();
                Ok::<_, anyhow::Error>(
                    [created, added].map(|error| format!("{error:#}").replace(&id, "RESOURCE")),
                )
            }
        };
        let unknown = refusals(vault_record(vault, org, false).await?, "org_1").await?;
        assert!(unknown[0].contains("does not exist"), "{unknown:?}");
        for (owner, principal) in [
            (
                VaultOwner::Org {
                    org_id: OrgId::try_new("org_2")?,
                },
                "org_2",
            ),
            (
                VaultOwner::User {
                    user_id: UserId::try_new("user_1")?,
                },
                "user_1",
            ),
        ] {
            let foreign = vault_record(vault, owner, true).await?;
            assert_eq!(refusals(foreign, principal).await?, unknown);
        }
        Ok(())
    }

    const VAULT: &str = "runtime = { type = 'llamacpp_cli_stock_tools', source = 'vault_remote_archive', url = '/vault/resources/vault_1/content', principal_id = 'org_1', flavor = 'linux-x64-rocm' }";

    #[rstest]
    #[case::one_runtime(format!("runtime = {RUNTIME}"), None)]
    #[case::runtimes_array(format!("runtimes = [{RUNTIME}]"), Some("invalid catalog member file"))]
    #[case::vault_source(VAULT.to_owned(), Some("global catalog entry cannot name a vault source"))]
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
    pub(crate) struct Memory(
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

        async fn rename_entry(
            &self,
            _scope: &CatalogScope,
            name: &CatalogName,
            new_name: &CatalogName,
        ) -> anyhow::Result<RenameOutcome> {
            let mut entries = self.entries()?;
            let Some(entry) = entries.get(&name.key()).cloned() else {
                return Ok(RenameOutcome::NoSuchEntry);
            };
            if entry.state == DescriptorEntryState::Tombstoned {
                return Ok(RenameOutcome::Tombstoned);
            }
            if entry.kind != DescriptorEntryKind::HandMade {
                return Ok(RenameOutcome::ImporterKind(entry.kind));
            }
            if entry.name == *new_name {
                return Ok(RenameOutcome::SameName);
            }
            if new_name.key() != entry.name.key()
                && let Some(holder) = entries.get(&new_name.key())
            {
                return Ok(RenameOutcome::NameTaken {
                    name: holder.name.clone(),
                    state: holder.state,
                });
            }
            let old_name = entry.name.clone();
            let renamed = DescriptorEntry {
                name: new_name.clone(),
                ..entry
            };
            entries.remove(&name.key());
            entries.insert(renamed.name.key(), renamed.clone());
            Ok(RenameOutcome::Renamed {
                old_name,
                entry: renamed,
            })
        }

        async fn merge_entry(
            &self,
            _scope: &CatalogScope,
            name: &CatalogName,
            into: &CatalogName,
        ) -> anyhow::Result<MergeOutcome> {
            let mut entries = self.entries()?;
            let outcome = merge_outcome(
                entries.get(&name.key()).cloned(),
                entries.get(&into.key()).cloned(),
            );
            if let MergeOutcome::Merged { entry, .. } = &outcome {
                entries.remove(&name.key());
                entries.insert(entry.name.key(), entry.clone());
            }
            Ok(outcome)
        }

        async fn relabel_member(
            &self,
            _scope: &CatalogScope,
            name: &CatalogName,
            format: ModelFormat,
            from: &QuantizationLabel,
            to: &QuantizationLabel,
        ) -> anyhow::Result<RelabelOutcome> {
            let mut entries = self.entries()?;
            let Some(entry) = entries.get(&name.key()).cloned() else {
                return Ok(RelabelOutcome::NoSuchEntry);
            };
            if entry.state == DescriptorEntryState::Tombstoned {
                return Ok(RelabelOutcome::Tombstoned);
            }
            if entry.descriptor_kind() == DescriptorKind::Runtime {
                return Ok(RelabelOutcome::RuntimeEntry);
            }
            let holds = |label: &QuantizationLabel| {
                entry
                    .members
                    .iter()
                    .any(|member| member.model_key() == Some((format, label)))
            };
            if !holds(from) {
                return Ok(RelabelOutcome::NoSuchMember);
            }
            if from == to {
                return Ok(RelabelOutcome::SameLabel);
            }
            if holds(to) {
                return Ok(RelabelOutcome::LabelTaken);
            }
            let members = entry
                .members
                .iter()
                .map(|member| match member.model_key() {
                    Some(key) if key == (format, from) => member.relabeled(to.clone()),
                    _ => Some(member.clone()),
                })
                .collect::<Option<Vec<_>>>()
                .context("a model entry holds a runtime member")?;
            let relabeled = DescriptorEntry {
                members: NonEmptyVec::try_new(members)?,
                ..entry
            };
            entries.insert(relabeled.name.key(), relabeled.clone());
            Ok(RelabelOutcome::Relabeled(relabeled))
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

        let mut q8_0 = json_gguf();
        q8_0["path"] = json!("AcmeLM-Q8_0.gguf");
        let parsed = add_member_with_vault(
            &store,
            CatalogSources {
                hub: &client,
                vault: None,
            },
            &scope,
            &name,
            Document::Parsed(AddDescriptorMemberRequest {
                runtime: None,
                model: Some(serde_json::from_value(q8_0)?),
                quantization: Some(QuantizationLabel::try_new("q8_0")?),
            }),
            now,
            false,
        )
        .await?;
        assert_eq!(
            member_keys(&parsed.entry.members),
            "gguf::q4_0,mlx::4bit,gguf::q8_0"
        );
        Ok(())
    }

    /// A store with these entries:
    ///
    /// - the hand-made runtime entries `Fork` and `Held`, with the same
    ///   member of build `b1`;
    /// - `Vulkan`, with another flavor of build `b1`;
    /// - `OtherBuild`, with another flavor from build `b2`;
    /// - a tombstoned `Dead` and a stable `llamacpp-v1`;
    /// - the model entry `AcmeLM`, with the members `gguf::q4_0` and
    ///   `gguf::q8_0`;
    /// - `AcmeLM-Q5`, with a new member `gguf::q5_0`;
    /// - `AcmeLM-Clash`, with another model under the label `gguf::q4_0`;
    /// - `AcmeLM-Q4`, with the `gguf::q4_0` model of `AcmeLM` under the label
    ///   `gguf::q4`.
    async fn renamable() -> anyhow::Result<Memory> {
        use pipette_store_testkit::{a_descriptor_entry, a_member, a_model_member};
        let store = Memory::default();
        let runtime = || a_member("b1", "linux-x64-rocm");
        let mut dead = a_descriptor_entry("Dead", DescriptorEntryKind::HandMade, 1, runtime()?)?;
        dead.state = DescriptorEntryState::Tombstoned;
        let mut model = a_descriptor_entry(
            "AcmeLM",
            DescriptorEntryKind::HandMade,
            1,
            a_model_member("gguf_text", "AcmeLM-GGUF", "q4_0")?,
        )?;
        model.members = NonEmptyVec::try_new(vec![
            model.members.head().clone(),
            a_model_member("gguf_text", "AcmeLM-Q8-GGUF", "q8_0")?,
        ])?;
        let entries = [
            a_descriptor_entry("Fork", DescriptorEntryKind::HandMade, 1, runtime()?)?,
            a_descriptor_entry("Held", DescriptorEntryKind::HandMade, 1, runtime()?)?,
            dead,
            a_descriptor_entry("llamacpp-v1", DescriptorEntryKind::Stable, 1, runtime()?)?,
            model,
            a_descriptor_entry(
                "Vulkan",
                DescriptorEntryKind::HandMade,
                1,
                a_member("b1", "linux-x64-vulkan")?,
            )?,
            a_descriptor_entry(
                "OtherBuild",
                DescriptorEntryKind::HandMade,
                1,
                a_member("b2", "linux-x64-cuda")?,
            )?,
            a_descriptor_entry(
                "AcmeLM-Q5",
                DescriptorEntryKind::HandMade,
                1,
                a_model_member("gguf_text", "AcmeLM-Q5-GGUF", "q5_0")?,
            )?,
            a_descriptor_entry(
                "AcmeLM-Clash",
                DescriptorEntryKind::HandMade,
                1,
                a_model_member("gguf_text", "Other-GGUF", "q4_0")?,
            )?,
            a_descriptor_entry(
                "AcmeLM-Q4",
                DescriptorEntryKind::HandMade,
                1,
                a_model_member("gguf_text", "AcmeLM-GGUF", "q4")?,
            )?,
        ];
        for entry in &entries {
            store.create_entry(entry).await?;
        }
        Ok(store)
    }

    #[tokio::test]
    async fn concurrent_memory_renames_keep_both_entries() -> anyhow::Result<()> {
        let store = renamable().await?;
        let scope = CatalogScope::Global;
        let fork = CatalogName::try_new("Fork")?;
        let held = CatalogName::try_new("Held")?;
        let spoon = CatalogName::try_new("Spoon")?;
        let before_len = store.entries()?.len();
        *store
            .2
            .lock()
            .map_err(|_| anyhow::anyhow!("barrier lock poisoned"))? =
            Some(Arc::new(tokio::sync::Barrier::new(2)));

        let (first, second) = tokio::join!(
            store.rename_entry(&scope, &fork, &spoon),
            store.rename_entry(&scope, &held, &spoon),
        );
        let outcomes = [first?, second?];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, RenameOutcome::Renamed { .. }))
                .count(),
            1,
            "{outcomes:?}"
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, RenameOutcome::NameTaken { .. }))
                .count(),
            1,
            "{outcomes:?}"
        );
        let entries = store.entries()?;
        assert_eq!(entries.len(), before_len);
        assert!(entries.contains_key(&spoon.key()));
        assert_ne!(
            entries.contains_key(&fork.key()),
            entries.contains_key(&held.key())
        );
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_memory_relabels_keep_the_winner() -> anyhow::Result<()> {
        let store = renamable().await?;
        let scope = CatalogScope::Global;
        let name = CatalogName::try_new("AcmeLM")?;
        let from = QuantizationLabel::try_new("q4_0")?;
        let q5 = QuantizationLabel::try_new("q5_0")?;
        let q6 = QuantizationLabel::try_new("q6_0")?;
        *store
            .2
            .lock()
            .map_err(|_| anyhow::anyhow!("barrier lock poisoned"))? =
            Some(Arc::new(tokio::sync::Barrier::new(2)));

        let (first, second) = tokio::join!(
            store.relabel_member(&scope, &name, ModelFormat::Gguf, &from, &q5),
            store.relabel_member(&scope, &name, ModelFormat::Gguf, &from, &q6),
        );
        let outcomes = [first?, second?];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, RelabelOutcome::Relabeled(_)))
                .count(),
            1,
            "{outcomes:?}"
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, RelabelOutcome::NoSuchMember))
                .count(),
            1,
            "{outcomes:?}"
        );
        let entries = store.entries()?;
        let entry = entries.get(&name.key()).context("model entry was lost")?;
        let labels = member_keys(&entry.members);
        assert!(
            labels == "gguf::q5_0,gguf::q8_0" || labels == "gguf::q6_0,gguf::q8_0",
            "{labels}"
        );
        Ok(())
    }

    #[rstest]
    #[case::dry_run(true)]
    #[case::write(false)]
    #[tokio::test]
    async fn rename_result_keeps_the_stored_old_spelling(
        #[case] dry_run: bool,
    ) -> anyhow::Result<()> {
        let store = renamable().await?;
        let scope = CatalogScope::Global;
        let requested = CatalogName::try_new("fork")?;
        let new_name = CatalogName::try_new("Spoon")?;
        let result =
            rename_entry_with_old_name(&store, &scope, &requested, &new_name, dry_run).await?;
        let RenameResult::Renamed { old_name, entry } = result else {
            anyhow::bail!("the rename must not merge: {result:?}");
        };
        assert_eq!(old_name, CatalogName::try_new("Fork")?);
        assert_eq!(entry.name, new_name);
        Ok(())
    }

    fn refusal(error: &CatalogWriteError) -> (&'static str, String) {
        let variant = match error {
            CatalogWriteError::Invalid(_) => "invalid",
            CatalogWriteError::Conflict(_) => "conflict",
            CatalogWriteError::NotFound(_) => "not found",
            CatalogWriteError::Storage(_) => "storage",
            CatalogWriteError::Upstream(_) => "upstream",
        };
        (variant, error.to_string())
    }

    #[rstest]
    #[case::reserved("Fork", "llamacpp-v9", "invalid", "reserved for the llamacpp importer")]
    #[case::missing("Missing", "Other", "not found", "no catalog entry Missing")]
    #[case::tombstoned("Dead", "Other", "conflict", "is tombstoned")]
    #[case::stable("llamacpp-v1", "Other", "conflict", "only a hand-made entry")]
    #[case::same_name("Fork", "Fork", "conflict", "already has the name Fork")]
    #[case::other_build(
        "Fork",
        "otherbuild",
        "invalid",
        "cannot merge Fork into OtherBuild: members of one catalog entry may differ only in flavor"
    )]
    #[case::runtime_into_model(
        "Fork",
        "AcmeLM",
        "invalid",
        "cannot merge Fork into AcmeLM: members mix runtime and model descriptors"
    )]
    #[case::label_clash(
        "AcmeLM-Clash",
        "AcmeLM",
        "conflict",
        "cannot merge AcmeLM-Clash into AcmeLM: duplicate member format and quantization: gguf::q4_0"
    )]
    #[case::same_model_new_label(
        "AcmeLM-Q4",
        "AcmeLM",
        "conflict",
        "cannot merge AcmeLM-Q4 into AcmeLM: duplicate member digest"
    )]
    #[case::tombstoned_holder(
        "Fork",
        "dead",
        "conflict",
        "the tombstoned entry already holds the catalog name Dead"
    )]
    #[tokio::test]
    async fn a_rename_refusal_is_the_same_in_a_dry_run(
        #[case] from: &str,
        #[case] to: &str,
        #[case] variant: &str,
        #[case] message: &str,
        #[values(true, false)] dry_run: bool,
    ) -> anyhow::Result<()> {
        let store = renamable().await?;
        let (from, to) = (CatalogName::try_new(from)?, CatalogName::try_new(to)?);
        let result = rename_entry(&store, &CatalogScope::Global, &from, &to, dry_run).await;
        let (got, text) = refusal(&result.err().context("the rename must be refused")?);
        assert_eq!(got, variant, "{text}");
        assert!(text.contains(message), "{text}");
        Ok(())
    }

    #[rstest]
    #[case::missing("Missing", "q4_0", "q5_0", "not found", "no catalog entry Missing")]
    #[case::no_member("AcmeLM", "q2_k", "q5_0", "not found", "has no member gguf::q2_k")]
    #[case::tombstoned("Dead", "q4_0", "q5_0", "conflict", "is tombstoned")]
    #[case::runtime_entry("Fork", "q4_0", "q5_0", "invalid", "holds runtimes")]
    #[case::same_label("AcmeLM", "q4_0", "q4_0", "conflict", "already has the label q4_0")]
    #[case::taken(
        "AcmeLM",
        "q4_0",
        "q8_0",
        "conflict",
        "already has a member gguf::q8_0"
    )]
    #[tokio::test]
    async fn a_relabel_refusal_is_the_same_in_a_dry_run(
        #[case] entry: &str,
        #[case] from: &str,
        #[case] to: &str,
        #[case] variant: &str,
        #[case] message: &str,
        #[values(true, false)] dry_run: bool,
    ) -> anyhow::Result<()> {
        let store = renamable().await?;
        let result = relabel_entry(
            &store,
            &CatalogScope::Global,
            &CatalogName::try_new(entry)?,
            ModelFormat::Gguf,
            &QuantizationLabel::try_new(from)?,
            &QuantizationLabel::try_new(to)?,
            dry_run,
        )
        .await;
        let (got, text) = refusal(&result.err().context("the relabel must be refused")?);
        assert_eq!(got, variant, "{text}");
        assert!(text.contains(message), "{text}");
        Ok(())
    }

    #[tokio::test]
    async fn a_dry_run_shows_the_change_and_writes_nothing() -> anyhow::Result<()> {
        let store = renamable().await?;
        let scope = CatalogScope::Global;
        let (fork, spoon) = (
            CatalogName::try_new("Fork")?,
            CatalogName::try_new("Spoon")?,
        );
        let model = CatalogName::try_new("AcmeLM")?;
        let (q4_0, q5_0) = (
            QuantizationLabel::try_new("q4_0")?,
            QuantizationLabel::try_new("q5_0")?,
        );
        let before = store
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .clone();

        let preview = rename_entry(&store, &scope, &fork, &spoon, true).await?;
        assert_eq!(preview.name, spoon);
        let preview = relabel_entry(
            &store,
            &scope,
            &model,
            ModelFormat::Gguf,
            &q4_0,
            &q5_0,
            true,
        )
        .await?;
        assert_eq!(
            preview.members.head().model_key(),
            Some((ModelFormat::Gguf, &q5_0))
        );
        assert_eq!(*store.entries()?, before, "a dry run wrote");

        assert_eq!(
            rename_entry(&store, &scope, &fork, &spoon, false)
                .await?
                .name,
            spoon
        );
        assert!(store.get_entry(&scope, &fork).await?.is_none());
        let relabeled = relabel_entry(
            &store,
            &scope,
            &model,
            ModelFormat::Gguf,
            &q4_0,
            &q5_0,
            false,
        )
        .await?;
        assert_eq!(store.get_entry(&scope, &model).await?, Some(relabeled));
        Ok(())
    }

    #[rstest]
    #[case::runtime_flavor(
        "Vulkan",
        "held",
        "Held",
        "linux-x64-rocm,linux-x64-vulkan",
        "linux-x64-vulkan",
        ""
    )]
    #[case::model_quantization(
        "AcmeLM-Q5",
        "acmelm",
        "AcmeLM",
        "gguf::q4_0,gguf::q8_0,gguf::q5_0",
        "gguf::q5_0",
        ""
    )]
    #[case::identical_member("Fork", "Held", "Held", "linux-x64-rocm", "", "linux-x64-rocm")]
    #[tokio::test]
    async fn a_rename_to_a_live_name_merges_the_entries(
        #[case] from: &str,
        #[case] to: &str,
        #[case] into: &str,
        #[case] members: &str,
        #[case] moved: &str,
        #[case] skipped: &str,
    ) -> anyhow::Result<()> {
        let store = renamable().await?;
        let scope = CatalogScope::Global;
        let (from, to) = (CatalogName::try_new(from)?, CatalogName::try_new(to)?);
        let before = store.entries()?.clone();

        let preview = rename_entry_with_old_name(&store, &scope, &from, &to, true).await?;
        assert_eq!(*store.entries()?, before, "a dry run wrote");
        let result = rename_entry_with_old_name(&store, &scope, &from, &to, false).await?;
        assert_eq!(preview, result);
        let RenameResult::Merged {
            old_name,
            entry,
            merge,
        } = &result
        else {
            anyhow::bail!("the rename must merge: {result:?}");
        };
        assert_eq!(*old_name, from);
        assert_eq!(entry.name.as_str(), into);
        assert_eq!(member_keys(&entry.members), members);
        assert_eq!(member_keys(&merge.moved), moved);
        assert_eq!(member_keys(&merge.skipped), skipped);
        assert_eq!(store.get_entry(&scope, &to).await?.as_ref(), Some(entry));
        assert_eq!(
            result.audit_action(&scope),
            AuditAction::CatalogMerge {
                scope: scope.clone(),
                old_name: from.clone(),
                into_name: entry.name.clone(),
                moved: merge.moved.iter().map(member_key).collect(),
                skipped: merge.skipped.iter().map(member_key).collect(),
            }
        );

        // The merge deleted the source entry, so its name is free.
        assert!(store.get_entry(&scope, &from).await?.is_none());
        let reused = DescriptorEntry {
            name: from,
            ..entry.clone()
        };
        assert_eq!(
            store.create_entry(&reused).await?,
            CreateEntryOutcome::Created
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_merge_keeps_the_added_time_of_each_moved_member() -> anyhow::Result<()> {
        let store = renamable().await?;
        let scope = CatalogScope::Global;
        let source = CatalogName::try_new("AcmeLM-Q5")?;
        let added = store
            .get_entry(&scope, &source)
            .await?
            .context("source entry")?
            .members
            .head()
            .added_at();
        let merged = rename_entry(
            &store,
            &scope,
            &source,
            &CatalogName::try_new("AcmeLM")?,
            false,
        )
        .await?;
        assert_eq!(merged.members.last().added_at(), added);
        Ok(())
    }
}
