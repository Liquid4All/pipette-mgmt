//! Import the GitHub releases of ggml-org/llama.cpp.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::num::NonZeroUsize;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Days, NaiveDate, Utc};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing::{Instrument, debug, info, info_span, warn};

use pipette_core::config::DescriptorCatalogConfig;
use pipette_plan_types::{
    CatalogName, DescriptorEntry, DescriptorEntryCreator, DescriptorEntryKind,
    DescriptorEntryState, DescriptorMember, LlamaCppFlavor, LlamacppCliStockTools,
    LlamacppCliStockToolsRelease, LlamacppCliStockToolsSource, NonEmptyString, NonEmptyVec,
    RepositoryUrl, Runtime,
};
use pipette_store::{
    AppendOutcome, CatalogImportRunFinish, CatalogImportRunStore, CreateEntryOutcome,
    DescriptorCatalogStore, DescriptorEntryFilter, TransitionOutcome,
};

use super::expiry::expired;

use crate::descriptor_catalog_admin::{LLAMACPP_DAILY_PREFIX, LLAMACPP_STABLE_PREFIX, same_build};

const REPO: &str = "ggml-org/llama.cpp";
const PAGE_SIZE: usize = 100;

/// A GitHub access token. `Debug` prints `(redacted)`, and [`Self::expose`]
/// is the one place that reads the value.
#[derive(Clone)]
pub struct GitHubToken(String);

impl GitHubToken {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    /// The raw token, for the `Authorization` header.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for GitHubToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitHubToken(redacted)")
    }
}

#[derive(Clone)]
pub struct GitHubClient {
    client: Client,
    base: Url,
    token: GitHubToken,
}

impl GitHubClient {
    pub fn new(token: GitHubToken) -> anyhow::Result<Self> {
        Self::with_base(token, Url::parse("https://api.github.com/")?)
    }

    /// A separate endpoint lets tests use a local GitHub server.
    pub(crate) fn with_base(token: GitHubToken, base: Url) -> anyhow::Result<Self> {
        anyhow::ensure!(!token.expose().is_empty(), "GITHUB_TOKEN is empty");
        Ok(Self {
            client: crate::http_client::builder_for(base.as_str())
                .user_agent("pipette-mgmt catalog-import")
                .timeout(Duration::from_secs(30))
                .build()?,
            base,
            token,
        })
    }

    async fn get(&self, path: &str, accept: &str) -> anyhow::Result<reqwest::Response> {
        let response = self
            .client
            .get(self.base.join(path)?)
            .bearer_auth(self.token.expose())
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .with_context(|| format!("GitHub {path}"))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
        };
        if header("X-RateLimit-Remaining") == Some("0") {
            anyhow::bail!(
                "GitHub {path} returned {status}: rate limit reached until {}",
                header("X-RateLimit-Reset")
                    .and_then(|reset| reset.parse::<i64>().ok())
                    .and_then(|reset| DateTime::<Utc>::from_timestamp(reset, 0))
                    .map_or_else(|| "an unknown time".to_owned(), |reset| reset.to_rfc3339())
            );
        }
        anyhow::bail!("GitHub {path} returned {status}")
    }

    /// Read a GitHub list page by page. Stop after a short page, or after a
    /// page for which `more` returns false.
    async fn paged<T: DeserializeOwned>(
        &self,
        path: &str,
        mut more: impl FnMut(&[T]) -> bool,
    ) -> anyhow::Result<Vec<T>> {
        let mut items = Vec::new();
        for page in 1.. {
            let batch: Vec<T> = self
                .get(
                    &format!("{path}?per_page={PAGE_SIZE}&page={page}"),
                    "application/vnd.github+json",
                )
                .await?
                .json()
                .await
                .with_context(|| format!("decoding GitHub {path}"))?;
            let last = batch.len() < PAGE_SIZE || !more(&batch);
            items.extend(batch);
            if last {
                break;
            }
        }
        Ok(items)
    }

    /// List releases, newest first. With `until`, stop after the first page
    /// that holds a release published before that time.
    async fn releases(&self, until: Option<DateTime<Utc>>) -> anyhow::Result<Vec<Release>> {
        self.paged(&format!("repos/{REPO}/releases"), |batch: &[Release]| {
            until.is_none_or(|until| {
                batch
                    .iter()
                    .filter_map(|release| release.published_at)
                    .all(|at| at >= until)
            })
        })
        .await
    }

    async fn release(&self, tag: &str) -> anyhow::Result<Release> {
        let path = format!("repos/{REPO}/releases/tags/{tag}");
        self.get(&path, "application/vnd.github+json")
            .await?
            .json()
            .await
            .context("decoding GitHub release")
    }

    async fn assets(&self, release: &Release) -> anyhow::Result<Vec<Asset>> {
        self.paged(
            &format!("repos/{REPO}/releases/{}/assets", release.id),
            |_: &[Asset]| true,
        )
        .await
    }

    async fn nightly_tag(&self, release: &Release) -> anyhow::Result<(String, Vec<String>)> {
        let assets = self.assets(release).await?;
        let unknown = assets
            .iter()
            .filter(|asset| asset.state == "uploaded" && asset.name != "nightly-tag.txt")
            .map(|asset| asset.name.clone())
            .collect();
        let asset = assets
            .into_iter()
            .find(|asset| asset.name == "nightly-tag.txt" && asset.state == "uploaded")
            .with_context(|| format!("{} has no uploaded nightly-tag.txt", release.tag_name))?;
        let path = format!("repos/{REPO}/releases/assets/{}", asset.id);
        let body = self
            .get(&path, "application/octet-stream")
            .await?
            .text()
            .await?;
        let tag = body.trim();
        anyhow::ensure!(
            build_number(tag).is_some(),
            "{} has an invalid nightly-tag.txt",
            release.tag_name
        );
        Ok((tag.to_owned(), unknown))
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Release {
    id: i64,
    tag_name: String,
    published_at: Option<DateTime<Utc>>,
    prerelease: bool,
    draft: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct Asset {
    id: i64,
    name: String,
    state: String,
}

fn decimal(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn build_number(tag: &str) -> Option<u32> {
    decimal(tag.strip_prefix('b')?)
}

fn stable_version(tag: &str) -> bool {
    let Some(rest) = tag.strip_prefix('v') else {
        return false;
    };
    let parts = rest.split('.').collect::<Vec<_>>();
    parts.len() == 3 && parts.iter().all(|part| decimal(part).is_some())
}

fn ignored_asset(name: &str, tag: &str) -> bool {
    name == "nightly-tag.txt"
        || name.starts_with("cudart-")
        || name == format!("llama-{tag}-ui.tar.gz")
        || name == format!("llama-{tag}-xcframework.zip")
}

struct Classified {
    flavors: Vec<LlamaCppFlavor>,
    unknown: Vec<String>,
    /// Asset names that a known flavor has at the tag and that are not uploaded.
    missing: Vec<String>,
    complete: bool,
}

fn classify(tag: &str, assets: &[Asset], config: &DescriptorCatalogConfig) -> Classified {
    let uploaded: HashSet<&str> = assets
        .iter()
        .filter(|asset| asset.state == "uploaded")
        .map(|asset| asset.name.as_str())
        .collect();
    let known: Vec<_> = LlamaCppFlavor::known()
        .filter_map(|flavor| flavor.release_asset_name(tag).map(|name| (flavor, name)))
        .collect();
    let present: HashSet<&str> = known
        .iter()
        .filter(|(_, name)| uploaded.contains(name.as_str()))
        .map(|(flavor, _)| flavor.as_str())
        .collect();
    let preferred: Vec<&LlamaCppFlavor> = config
        .precedence
        .llamacpp
        .iter()
        .filter(|name| present.contains(name.as_str()))
        .filter_map(|name| {
            known
                .iter()
                .find(|(flavor, _)| flavor.as_str() == name)
                .map(|(flavor, _)| flavor)
        })
        .collect();
    let flavors = preferred
        .iter()
        .copied()
        .chain(
            known
                .iter()
                .map(|(flavor, _)| flavor)
                .filter(|flavor| present.contains(flavor.as_str()) && !preferred.contains(flavor)),
        )
        .cloned()
        .collect();
    let expected: HashSet<&str> = known.iter().map(|(_, name)| name.as_str()).collect();
    let unknown = assets
        .iter()
        .filter(|asset| {
            asset.state == "uploaded"
                && !expected.contains(asset.name.as_str())
                && !ignored_asset(&asset.name, tag)
        })
        .map(|asset| asset.name.clone())
        .collect();
    let missing: Vec<String> = known
        .iter()
        .filter(|(_, name)| !uploaded.contains(name.as_str()))
        .map(|(_, name)| name.clone())
        .collect();
    Classified {
        flavors,
        unknown,
        complete: !known.is_empty() && missing.is_empty(),
        missing,
    }
}

fn descriptor(
    tag: &str,
    flavor: &LlamaCppFlavor,
    at: DateTime<Utc>,
) -> anyhow::Result<DescriptorMember> {
    let runtime = Runtime::LlamacppCliStockTools(LlamacppCliStockTools {
        source: LlamacppCliStockToolsSource::GithubRelease(LlamacppCliStockToolsRelease {
            repository_url: RepositoryUrl::new(format!("github.com/{REPO}")),
            repository_version: NonEmptyString::try_new(tag.to_owned())?,
            asset: None,
        }),
        flavor: flavor.clone(),
    });
    Ok(DescriptorMember::new(runtime, at)?)
}

fn entry_tag(entry: &DescriptorEntry) -> anyhow::Result<String> {
    let Some(Runtime::LlamacppCliStockTools(runtime)) = entry.members.head().runtime() else {
        anyhow::bail!("catalog entry {} is not a llama.cpp entry", entry.name);
    };
    let version = runtime.source.reference();
    anyhow::ensure!(
        build_number(version).is_some(),
        "catalog entry {} has no b<N> build",
        entry.name
    );
    Ok(version.to_owned())
}

fn record_unknown(tag: &str, classified: &Classified, finish: &mut CatalogImportRunFinish) {
    for asset in &classified.unknown {
        let tagged = format!("{tag}/{asset}");
        if !finish.unclassified_assets.contains(&tagged) {
            warn!(release_tag = tag, asset, "catalog asset is not classified");
            finish.unclassified_assets.push(tagged);
        }
    }
}

struct EntryCandidate<'a> {
    name: CatalogName,
    kind: DescriptorEntryKind,
    published_at: DateTime<Utc>,
    tag: &'a str,
    classified: &'a Classified,
}

async fn create_entry(
    store: &dyn DescriptorCatalogStore,
    config: &DescriptorCatalogConfig,
    stable_entries: &mut Vec<DescriptorEntry>,
    EntryCandidate {
        name,
        kind,
        published_at,
        tag,
        classified,
    }: EntryCandidate<'_>,
    now: DateTime<Utc>,
    finish: &mut CatalogImportRunFinish,
) -> anyhow::Result<()> {
    if let Some(existing) = store
        .get_entry(&pipette_plan_types::CatalogScope::Global, &name)
        .await?
    {
        if existing.state == DescriptorEntryState::Tombstoned {
            return Ok(());
        }
        anyhow::ensure!(
            entry_tag(&existing)? == tag && existing.kind == kind,
            "catalog entry {name} names a different release"
        );
        return Ok(());
    }
    let members = NonEmptyVec::try_new(
        classified
            .flavors
            .iter()
            .map(|flavor| descriptor(tag, flavor, now))
            .collect::<anyhow::Result<Vec<_>>>()?,
    )?;
    same_build(&members)?;
    let mut entry = DescriptorEntry::new(
        pipette_plan_types::CatalogScope::Global,
        name.clone(),
        kind,
        DescriptorEntryState::Active,
        published_at,
        now,
        DescriptorEntryCreator::ImporterLlamacpp,
        members,
    )?;
    if starts_hidden(stable_entries, &entry, config, now) {
        entry.state = DescriptorEntryState::Hidden;
    }
    match store.create_entry(&entry).await? {
        CreateEntryOutcome::Created => {
            if entry.kind == DescriptorEntryKind::Stable
                && entry.state == DescriptorEntryState::Active
            {
                stable_entries.push(entry.clone());
            }
            finish.entries_created += 1;
            finish.members_added += i32::try_from(entry.members.len())?;
            if entry.state == DescriptorEntryState::Hidden {
                finish.entries_hidden += 1;
            }
            info!(
                catalog_name = %name,
                flavor = "all",
                kind = %entry.kind,
                release_time = %entry.release_time,
                state = %entry.state,
                "catalog entry created"
            );
            for member in entry.members.iter() {
                info!(catalog_name = %name, flavor = member.flavor().unwrap_or("-"), "catalog member added");
            }
        }
        CreateEntryOutcome::NameTaken { .. } => {
            anyhow::bail!("catalog entry {name} was created during import")
        }
    }
    Ok(())
}

async fn append_from_release(
    api: &GitHubClient,
    store: &dyn DescriptorCatalogStore,
    entry: &DescriptorEntry,
    config: &DescriptorCatalogConfig,
    now: DateTime<Utc>,
    finish: &mut CatalogImportRunFinish,
) -> anyhow::Result<()> {
    let tag = entry_tag(entry)?;
    let have: HashSet<&str> = entry
        .members
        .iter()
        .filter_map(DescriptorMember::flavor)
        .collect();
    if LlamaCppFlavor::known()
        .filter(|flavor| flavor.release_asset_name(&tag).is_some())
        .all(|flavor| have.contains(flavor.as_str()))
    {
        return Ok(());
    }
    let release = api.release(&tag).await?;
    let classified = classify(&tag, &api.assets(&release).await?, config);
    record_unknown(&tag, &classified, finish);
    let additions = classified
        .flavors
        .iter()
        .filter(|flavor| !have.contains(flavor.as_str()))
        .map(|flavor| descriptor(&tag, flavor, now))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if additions.is_empty() {
        return Ok(());
    }
    let mut all = entry.members.iter().cloned().collect::<Vec<_>>();
    all.extend(additions.iter().cloned());
    let all = NonEmptyVec::try_new(all)?;
    same_build(&all)?;
    match store
        .append_members(
            &pipette_plan_types::CatalogScope::Global,
            &entry.name,
            NonEmptyVec::try_new(additions.clone())?,
        )
        .await?
    {
        AppendOutcome::Appended(_) => {
            finish.members_added += i32::try_from(additions.len())?;
            for member in additions {
                info!(catalog_name = %entry.name, flavor = member.flavor().unwrap_or("-"), "catalog member added");
            }
        }
        other @ (AppendOutcome::NoSuchEntry
        | AppendOutcome::Tombstoned
        | AppendOutcome::DescriptorKindMismatch
        | AppendOutcome::RuntimeTypeMismatch
        | AppendOutcome::DuplicateFlavor(_)
        | AppendOutcome::DuplicateModelKey { .. }
        | AppendOutcome::FormatTypeClash { .. }
        | AppendOutcome::SpeculativeMix
        | AppendOutcome::DuplicateDigest(_)) => {
            anyhow::bail!("cannot append to {}: {other:?}", entry.name)
        }
    }
    Ok(())
}

/// The global entries from this importer with one of `kinds` and one of
/// `states`. An empty set admits every value.
async fn importer_entries(
    store: &dyn DescriptorCatalogStore,
    kinds: BTreeSet<DescriptorEntryKind>,
    states: BTreeSet<DescriptorEntryState>,
) -> anyhow::Result<Vec<DescriptorEntry>> {
    let filter = DescriptorEntryFilter {
        scopes: [pipette_plan_types::CatalogScope::Global].into(),
        kinds,
        states,
        ..Default::default()
    };
    let mut entries = Vec::new();
    let mut cursor = None;
    loop {
        let page = store
            .list_entries(
                &filter,
                cursor.as_ref(),
                NonZeroUsize::new(100).context("page size is zero")?,
            )
            .await?;
        anyhow::ensure!(
            page.unreadable.is_empty(),
            "the catalog contains unreadable entries"
        );
        entries.extend(
            page.items
                .into_iter()
                .filter(|entry| entry.created_by == DescriptorEntryCreator::ImporterLlamacpp),
        );
        match page.resume_after {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(entries)
}

/// Whether a new entry is already past expiry. The function ranks a stable
/// entry against the active and pinned stable entries loaded for this run.
fn starts_hidden(
    stable_entries: &mut Vec<DescriptorEntry>,
    candidate: &DescriptorEntry,
    config: &DescriptorCatalogConfig,
    now: DateTime<Utc>,
) -> bool {
    if candidate.kind != DescriptorEntryKind::Stable {
        return !expired(std::slice::from_ref(candidate), &config.expiry, now).is_empty();
    }
    stable_entries.push(candidate.clone());
    let hidden = expired(stable_entries, &config.expiry, now)
        .iter()
        .any(|entry| entry.name == candidate.name);
    stable_entries.pop();
    hidden
}

/// Move each active entry from this importer that is past expiry to hidden.
/// Preserve a state change that an operator makes during the run.
async fn expire(
    store: &dyn DescriptorCatalogStore,
    config: &DescriptorCatalogConfig,
    now: DateTime<Utc>,
    finish: &mut CatalogImportRunFinish,
) -> anyhow::Result<()> {
    let importer_lock = store.lock_importer_entries().await?;
    let entries = importer_entries(
        store,
        [DescriptorEntryKind::Stable, DescriptorEntryKind::Daily].into(),
        [DescriptorEntryState::Active, DescriptorEntryState::Pinned].into(),
    )
    .await?;
    for entry in expired(&entries, &config.expiry, now) {
        let outcome = store
            .transition_state_under_importer_lock(
                &pipette_plan_types::CatalogScope::Global,
                &entry.name,
                DescriptorEntryState::Active,
                DescriptorEntryState::Hidden,
            )
            .await;
        match outcome {
            Ok(TransitionOutcome::Changed) => {
                finish.entries_hidden += 1;
                info!(
                    catalog_name = %entry.name,
                    kind = %entry.kind,
                    release_time = %entry.release_time,
                    "catalog entry expired: active to hidden"
                );
            }
            Ok(
                other @ (TransitionOutcome::NoSuchEntry
                | TransitionOutcome::Tombstoned
                | TransitionOutcome::UnexpectedState(_)),
            ) => {
                debug!(
                    catalog_name = %entry.name,
                    outcome = ?other,
                    "catalog entry expiry skipped: state changed concurrently"
                );
            }
            Err(error) => record_failure(entry.name.as_str(), &error, finish),
        }
    }
    importer_lock.release().await?;
    Ok(())
}

/// The release time of the newest stable entry from this importer. Older
/// stable releases already have entries.
async fn newest_stable_release(
    store: &dyn DescriptorCatalogStore,
) -> anyhow::Result<Option<DateTime<Utc>>> {
    let filter = DescriptorEntryFilter {
        scopes: [pipette_plan_types::CatalogScope::Global].into(),
        kinds: [DescriptorEntryKind::Stable].into(),
        states: [
            DescriptorEntryState::Active,
            DescriptorEntryState::Pinned,
            DescriptorEntryState::Hidden,
            DescriptorEntryState::Tombstoned,
        ]
        .into(),
        name_prefix: Some(LLAMACPP_STABLE_PREFIX.into()),
        ..Default::default()
    };
    let page = store.list_entries(&filter, None, NonZeroUsize::MIN).await?;
    anyhow::ensure!(
        page.unreadable.is_empty(),
        "the catalog contains an unreadable stable entry"
    );
    Ok(page.items.first().map(|entry| entry.release_time))
}

async fn default_since(
    store: &dyn DescriptorCatalogStore,
    today: NaiveDate,
) -> anyhow::Result<NaiveDate> {
    let filter = DescriptorEntryFilter {
        scopes: [pipette_plan_types::CatalogScope::Global].into(),
        kinds: [DescriptorEntryKind::Daily].into(),
        states: [
            DescriptorEntryState::Active,
            DescriptorEntryState::Pinned,
            DescriptorEntryState::Hidden,
        ]
        .into(),
        name_prefix: Some(LLAMACPP_DAILY_PREFIX.into()),
        ..Default::default()
    };
    let page = store.list_entries(&filter, None, NonZeroUsize::MIN).await?;
    anyhow::ensure!(
        page.unreadable.is_empty(),
        "the catalog contains an unreadable daily entry"
    );
    let recent = page
        .items
        .first()
        .and_then(|entry| entry.name.as_str().strip_prefix(LLAMACPP_DAILY_PREFIX))
        .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok());
    let since = match recent {
        Some(day) => day.checked_sub_days(Days::new(2)),
        None => today.checked_sub_days(Days::new(14)),
    };
    Ok(since.unwrap_or(today))
}

/// Create the entry for one stable release from the build that its
/// `nightly-tag.txt` names.
async fn import_stable(
    api: &GitHubClient,
    store: &dyn DescriptorCatalogStore,
    config: &DescriptorCatalogConfig,
    stable_entries: &mut Vec<DescriptorEntry>,
    release: &Release,
    now: DateTime<Utc>,
    finish: &mut CatalogImportRunFinish,
) -> anyhow::Result<()> {
    let published_at = release
        .published_at
        .with_context(|| format!("{} has no publication time", release.tag_name))?;
    let version = release
        .tag_name
        .strip_prefix('v')
        .with_context(|| format!("{} is not a vX.Y.Z tag", release.tag_name))?;
    let name = CatalogName::try_new(format!("{LLAMACPP_STABLE_PREFIX}{version}"))?;
    if store
        .get_entry(&pipette_plan_types::CatalogScope::Global, &name)
        .await?
        .is_some()
    {
        return Ok(());
    }
    let (tag, unexpected) = api.nightly_tag(release).await?;
    finish.unclassified_assets.extend(
        unexpected
            .into_iter()
            .inspect(|asset| {
                warn!(release_tag = %release.tag_name, asset, "catalog asset is not classified");
            })
            .map(|asset| format!("{}/{asset}", release.tag_name)),
    );
    let build = api.release(&tag).await?;
    let classified = classify(&tag, &api.assets(&build).await?, config);
    record_unknown(&tag, &classified, finish);
    anyhow::ensure!(
        !classified.flavors.is_empty(),
        "{} has no known build assets",
        release.tag_name
    );
    create_entry(
        store,
        config,
        stable_entries,
        EntryCandidate {
            name,
            kind: DescriptorEntryKind::Stable,
            published_at,
            tag: &tag,
            classified: &classified,
        },
        now,
        finish,
    )
    .await
}

/// Record a failed release or entry in the run, so the run ends in an error
/// state and the next release or entry still runs.
fn record_failure(subject: &str, error: &anyhow::Error, finish: &mut CatalogImportRunFinish) {
    let message = format!("{subject}: {error:#}");
    warn!(subject, error = %message, "catalog import recorded a failure and continued");
    add_error(message, finish);
}

/// Add one error to the run's summary, after any earlier errors.
fn add_error(message: String, finish: &mut CatalogImportRunFinish) {
    finish.error_summary = Some(match finish.error_summary.take() {
        Some(summary) => format!("{summary}; {message}"),
        None => message,
    });
}

async fn import_inner(
    api: &GitHubClient,
    store: &dyn DescriptorCatalogStore,
    config: &DescriptorCatalogConfig,
    since: Option<NaiveDate>,
    now: DateTime<Utc>,
    finish: &mut CatalogImportRunFinish,
) -> anyhow::Result<()> {
    let since = match since {
        Some(date) => date,
        None => default_since(store, now.date_naive()).await?,
    };
    let since_start = since.and_time(chrono::NaiveTime::MIN).and_utc();
    let until = newest_stable_release(store)
        .await?
        .map(|newest| newest.min(since_start));
    let releases = api.releases(until).await?;
    let mut stable = releases
        .iter()
        .filter(|release| {
            !release.draft && !release.prerelease && stable_version(&release.tag_name)
        })
        .collect::<Vec<_>>();
    stable.sort_by_key(|release| release.published_at);
    let importer_lock = store.lock_importer_entries().await?;
    let mut stable_entries = importer_entries(
        store,
        [DescriptorEntryKind::Stable].into(),
        [DescriptorEntryState::Active, DescriptorEntryState::Pinned].into(),
    )
    .await?;
    for release in stable {
        if let Err(error) = import_stable(
            api,
            store,
            config,
            &mut stable_entries,
            release,
            now,
            finish,
        )
        .await
        {
            record_failure(&release.tag_name, &error, finish);
        }
    }
    importer_lock.release().await?;
    let mut days = releases
        .iter()
        .filter(|release| !release.draft && build_number(&release.tag_name).is_some())
        .try_fold(
            HashMap::<NaiveDate, Vec<&Release>>::new(),
            |mut days, release| {
                let day = release
                    .published_at
                    .with_context(|| format!("{} has no publication time", release.tag_name))?
                    .date_naive();
                if day >= since && day < now.date_naive() {
                    days.entry(day).or_default().push(release);
                }
                anyhow::Ok(days)
            },
        )?;
    let mut dates = days.keys().copied().collect::<Vec<_>>();
    dates.sort();
    let min_age = i64::try_from(config.llamacpp_min_build_age_secs)
        .ok()
        .and_then(chrono::Duration::try_seconds)
        .context("llamacpp_min_build_age_secs is out of range")?;
    for date in dates {
        let name = CatalogName::try_new(format!("{LLAMACPP_DAILY_PREFIX}{date}"))?;
        if store
            .get_entry(&pipette_plan_types::CatalogScope::Global, &name)
            .await?
            .is_some()
        {
            continue;
        }
        let mut candidates = days.remove(&date).context("daily candidates are missing")?;
        candidates.sort_by_key(|release| {
            (
                std::cmp::Reverse(release.published_at),
                std::cmp::Reverse(build_number(&release.tag_name)),
            )
        });
        let mut newest_incomplete = None;
        let mut created = false;
        for release in candidates {
            let at = release
                .published_at
                .context("daily build has no publication time")?;
            if now.signed_duration_since(at) < min_age {
                continue;
            }
            let classified = classify(&release.tag_name, &api.assets(release).await?, config);
            record_unknown(&release.tag_name, &classified, finish);
            if !classified.complete {
                if newest_incomplete.is_none() {
                    newest_incomplete = Some((&release.tag_name, classified.missing));
                }
                continue;
            }
            create_entry(
                store,
                config,
                &mut stable_entries,
                EntryCandidate {
                    name: name.clone(),
                    kind: DescriptorEntryKind::Daily,
                    published_at: at,
                    tag: &release.tag_name,
                    classified: &classified,
                },
                now,
                finish,
            )
            .await?;
            created = true;
            break;
        }
        if !created && let Some((tag, missing)) = newest_incomplete {
            warn!(
                catalog_name = %name,
                release_tag = %tag,
                ?missing,
                "daily entry skipped: no build of the day is complete"
            );
        }
    }
    for entry in importer_entries(
        store,
        BTreeSet::new(),
        [DescriptorEntryState::Active].into(),
    )
    .await?
    {
        if let Err(error) = append_from_release(api, store, &entry, config, now, finish).await {
            record_failure(entry.name.as_str(), &error, finish);
        }
    }
    Ok(())
}

async fn run_recorded<F>(
    runs: &dyn CatalogImportRunStore,
    command: &str,
    target: Option<&CatalogName>,
    work: F,
) -> anyhow::Result<CatalogImportRunFinish>
where
    F: std::future::Future<Output = CatalogImportRunFinish>,
{
    let id = uuid::Uuid::now_v7().to_string();
    runs.start_run(&id, command, target, Utc::now()).await?;
    let mut finish = work.await;
    // Unclassified assets put the run in an error state, even when every
    // entry and member was written.
    if !finish.unclassified_assets.is_empty() && finish.error_summary.is_none() {
        finish.error_summary = Some(pipette_plan_types::unclassified_summary(
            finish.unclassified_assets.len(),
        ));
    }
    if let Some(error) = &finish.error_summary {
        tracing::error!(
            run_id = %id,
            command,
            error,
            unclassified_assets = finish.unclassified_assets.len(),
            entries_created = finish.entries_created,
            members_added = finish.members_added,
            entries_hidden = finish.entries_hidden,
            "catalog import run failed"
        );
    }
    runs.finish_run(&id, Utc::now(), &finish)
        .await
        .with_context(|| format!("recording the end of catalog import run {id}"))?;
    if let Some(error) = &finish.error_summary {
        anyhow::bail!("{error}");
    }
    tracing::info!(
        run_id = %id,
        command,
        entries_created = finish.entries_created,
        members_added = finish.members_added,
        entries_hidden = finish.entries_hidden,
        "catalog import run finished"
    );
    Ok(finish)
}

/// Import stable releases and complete daily builds, then apply expiry.
pub async fn import(
    api: &GitHubClient,
    store: &dyn DescriptorCatalogStore,
    runs: &dyn CatalogImportRunStore,
    config: &DescriptorCatalogConfig,
    since: Option<NaiveDate>,
    now: DateTime<Utc>,
) -> anyhow::Result<CatalogImportRunFinish> {
    let work = async {
        let mut finish = CatalogImportRunFinish::default();
        let result = import_inner(api, store, config, since, now, &mut finish).await;
        if let Err(error) = result {
            add_error(format!("{error:#}"), &mut finish);
        }
        // Expiry reads only the store, so it runs after a failed import too.
        if let Err(error) = expire(store, config, now, &mut finish).await {
            add_error(format!("expiry: {error:#}"), &mut finish);
        }
        finish
    };
    run_recorded(runs, "import", None, work)
        .instrument(info_span!(
            "catalog_import",
            source = "llamacpp",
            command = "import"
        ))
        .await
}

/// Read the build of one existing importer entry and append known flavors.
pub async fn refresh(
    api: &GitHubClient,
    store: &dyn DescriptorCatalogStore,
    runs: &dyn CatalogImportRunStore,
    config: &DescriptorCatalogConfig,
    name: &CatalogName,
    now: DateTime<Utc>,
) -> anyhow::Result<CatalogImportRunFinish> {
    let work = async {
        let mut finish = CatalogImportRunFinish::default();
        let result = async {
            let entry = store
                .get_entry(&pipette_plan_types::CatalogScope::Global, name)
                .await?
                .with_context(|| format!("no catalog entry {name}"))?;
            anyhow::ensure!(
                entry.state != DescriptorEntryState::Tombstoned,
                "catalog entry {name} is tombstoned"
            );
            anyhow::ensure!(
                entry.created_by == DescriptorEntryCreator::ImporterLlamacpp,
                "catalog entry {name} was not imported from llama.cpp"
            );
            append_from_release(api, store, &entry, config, now, &mut finish).await
        }
        .await;
        if let Err(error) = result {
            add_error(format!("{error:#}"), &mut finish);
        }
        finish
    };
    run_recorded(runs, "refresh", Some(name), work)
        .instrument(info_span!("catalog_import", source = "llamacpp", command = "refresh", catalog_name = %name))
        .await
}

#[cfg(test)]
mod tests;
