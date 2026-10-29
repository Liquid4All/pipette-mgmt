//! An organization's private benchmark catalog, shared by
//! `pipette-mgmt orgs benchmarks` and the API. A private entry may not take an
//! id the public catalog or another organization holds, nor pin a dataset or
//! name evals-server samples another organization's entry does unless a public
//! entry does too, nor pin a public entry's digest any other way: artifacts are
//! served by digest, and samples by name.

use std::collections::{BTreeSet, HashMap};

use futures::{StreamExt, TryStreamExt};

use pipette_store::{
    CatalogStore, PlanStore, STORAGE_CONCURRENCY, UnreadableObjectError, read_org_benchmark,
};

use pipette_core::benchmark::Benchmark;
use pipette_core::campaign::Campaign;
use pipette_plan_types::{
    BenchmarkDefinition, BenchmarkId, BenchmarkType, Dataset, JobId, OrgId, Sha256,
    VaultPrincipalId, VaultResourceId,
};

/// Why an organization's catalog could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum OrgCatalogError {
    #[error("organization {org_id} keeps no benchmark {benchmark_id}")]
    NotFound {
        org_id: OrgId,
        benchmark_id: BenchmarkId,
    },
    /// The public catalog already answers to this id, so the entry would never
    /// be served.
    #[error("the public catalog already holds a benchmark {0}")]
    ShadowsPublic(BenchmarkId),
    /// Benchmark ids are one server-wide namespace; the first writer keeps it.
    #[error("another organization already keeps a benchmark {0}")]
    HeldElsewhere(BenchmarkId),
    /// The id starts with another type's prefix. Readers without the catalog
    /// type a cell by its prefix, so the entry would read as that type.
    #[error("benchmark {benchmark_id} is {benchmark_type}, but its id reads as {prefix_type}")]
    MisleadingPrefix {
        benchmark_id: BenchmarkId,
        benchmark_type: BenchmarkType,
        prefix_type: BenchmarkType,
    },
    /// Another organization's entry pins this dataset. Artifacts are served by
    /// digest, so a second pin would serve that organization's copy.
    #[error("another organization's benchmark already pins dataset {0}")]
    DatasetHeldElsewhere(Sha256),
    /// Another organization's entry names these samples. The evals server serves
    /// and scores them for any entry that names them, with no tenant of its own.
    #[error("another organization's benchmark already names eval {eval_id} at {dataset}")]
    SamplesHeldElsewhere { eval_id: String, dataset: String },
    /// The evals server's URLs carry the name as a path segment, so a name that
    /// is not one plain segment could reach samples another name spells.
    #[error(
        "{0:?} is not a plain evals-server name: use letters, digits, '.', '_' and '-', starting with a letter or digit"
    )]
    UnsafeSampleName(String),
    /// A public entry pins this digest as another dataset or sample count; build
    /// and probe follow the public pin, so this entry would be dropped as a conflict.
    #[error("a public benchmark pins dataset {0} differently")]
    DatasetPinnedPublicly(Sha256),
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// The entry `benchmark_id` names in `org_id`'s catalog, or
/// [`OrgCatalogError::NotFound`].
pub async fn require(
    catalog: &dyn CatalogStore,
    org_id: &OrgId,
    benchmark_id: &BenchmarkId,
) -> Result<Benchmark, OrgCatalogError> {
    catalog
        .get_org_benchmark(org_id, benchmark_id)
        .await?
        .ok_or_else(|| OrgCatalogError::NotFound {
            org_id: org_id.clone(),
            benchmark_id: benchmark_id.clone(),
        })
}

/// Write `benchmark` into `org_id`'s catalog, replacing what is under its id.
/// Refused when the id's prefix names another type, when another holder has
/// the id, when a public entry pins its dataset's digest differently, or when
/// only another organization's entry pins its dataset or names its evals-server
/// samples, or when an evals-server name is not one plain path segment.
pub async fn put(
    catalog: &dyn CatalogStore,
    public: &HashMap<BenchmarkId, Benchmark>,
    org_id: &OrgId,
    benchmark: Benchmark,
) -> Result<Benchmark, OrgCatalogError> {
    if let Some(prefix_type) = BenchmarkType::from_id(benchmark.benchmark_id().as_ref())
        && prefix_type != benchmark.benchmark_type()
    {
        return Err(OrgCatalogError::MisleadingPrefix {
            benchmark_id: benchmark.benchmark_id().clone(),
            benchmark_type: benchmark.benchmark_type(),
            prefix_type,
        });
    }
    if public.contains_key(benchmark.benchmark_id()) {
        return Err(OrgCatalogError::ShadowsPublic(
            benchmark.benchmark_id().clone(),
        ));
    }
    let elsewhere: Vec<(OrgId, BenchmarkId)> = catalog
        .list_org_benchmark_ids()
        .await?
        .into_iter()
        .filter(|(holder, _)| holder != org_id)
        .collect();
    if elsewhere
        .iter()
        .any(|(_, id)| id == benchmark.benchmark_id())
    {
        return Err(OrgCatalogError::HeldElsewhere(
            benchmark.benchmark_id().clone(),
        ));
    }
    let digest = match benchmark.dataset() {
        Some(dataset) => {
            let samples = crate::dataset_cache::stated_samples(&benchmark);
            let public_pins: Vec<(&Dataset, Option<u32>)> = public
                .values()
                .filter_map(|b| Some((b.dataset()?, crate::dataset_cache::stated_samples(b))))
                .filter(|(pin, _)| pin.digest == dataset.digest)
                .collect();
            if public_pins
                .iter()
                .any(|&(pin, stated)| pin != dataset || stated != samples)
            {
                return Err(OrgCatalogError::DatasetPinnedPublicly(
                    dataset.digest.clone(),
                ));
            }
            // Pinned publicly the same way, the digest is everyone's to pin.
            public_pins.is_empty().then_some(&dataset.digest)
        }
        None => None,
    };
    let samples = eval_samples(&benchmark);
    if let Some(name) = samples
        .into_iter()
        .flat_map(|(eval_id, dataset)| [eval_id, dataset])
        .find(|name| !plain_segment(name))
    {
        return Err(OrgCatalogError::UnsafeSampleName(name.to_owned()));
    }
    let samples = samples.filter(|named| !public.values().any(|b| eval_samples(b) == Some(*named)));
    if digest.is_some() || samples.is_some() {
        // One that does not parse holds nothing, so a bad file cannot block every write.
        let others: Vec<Benchmark> = read_kept(catalog, elsewhere)
            .await?
            .into_iter()
            .filter_map(|kept| kept.entry.ok())
            .collect();
        if let Some(digest) = digest
            && others.iter().any(|b| pins(b, digest))
        {
            return Err(OrgCatalogError::DatasetHeldElsewhere(digest.clone()));
        }
        if let Some((eval_id, dataset)) = samples
            && others.iter().any(|b| eval_samples(b) == samples)
        {
            return Err(OrgCatalogError::SamplesHeldElsewhere {
                eval_id: eval_id.to_owned(),
                dataset: dataset.to_owned(),
            });
        }
    }
    catalog.put_org_benchmark(org_id, &benchmark).await?;
    Ok(benchmark.private_to(org_id.clone()))
}

fn pins(benchmark: &Benchmark, digest: &Sha256) -> bool {
    benchmark.dataset().is_some_and(|d| d.digest == *digest)
}

fn plain_segment(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The eval and dataset the evals server serves and scores this entry's
/// samples under.
fn eval_samples(benchmark: &Benchmark) -> Option<(&str, &str)> {
    match &benchmark.def {
        BenchmarkDefinition::Eval(b) => Some((
            b.parameter_eval_id.as_str(),
            b.parameter_dataset_name.as_str(),
        )),
        BenchmarkDefinition::VlmEval(b) => Some((
            b.parameter_lmms_task.as_str(),
            b.parameter_dataset_version.as_str(),
        )),
        BenchmarkDefinition::AsrEval(b) => Some((
            b.parameter_asr_task.as_str(),
            b.parameter_dataset_version.as_str(),
        )),
        BenchmarkDefinition::PrefillThroughput(_)
        | BenchmarkDefinition::DecodeThroughput(_)
        | BenchmarkDefinition::EndToEndLatency(_)
        | BenchmarkDefinition::MaxMemoryUsage(_)
        | BenchmarkDefinition::VlThroughput(_)
        | BenchmarkDefinition::VlMaxMemory(_)
        | BenchmarkDefinition::VlSpecDecode(_)
        | BenchmarkDefinition::AsrThroughput(_)
        | BenchmarkDefinition::AsrMaxMemory(_) => None,
    }
}

/// Delete the entry `benchmark_id` names in `org_id`'s catalog; absent is
/// [`OrgCatalogError::NotFound`], so a mistyped id is reported. An unreadable
/// entry is still deletable, since it still refuses its organization's delete.
pub async fn delete(
    catalog: &dyn CatalogStore,
    org_id: &OrgId,
    benchmark_id: &BenchmarkId,
) -> Result<(), OrgCatalogError> {
    if !catalog
        .list_benchmark_ids_of(org_id)
        .await?
        .contains(benchmark_id)
    {
        return Err(OrgCatalogError::NotFound {
            org_id: org_id.clone(),
            benchmark_id: benchmark_id.clone(),
        });
    }
    catalog.delete_org_benchmark(org_id, benchmark_id).await?;
    Ok(())
}

/// An entry as a listing reads it by its key.
#[derive(Debug)]
pub struct Kept {
    pub org_id: OrgId,
    pub benchmark_id: BenchmarkId,
    /// The entry, or why it does not parse. The reason can name a storage key,
    /// so only the host CLI prints it.
    pub entry: Result<Benchmark, UnreadableObjectError>,
}

/// Every entry of `org_id`'s catalog, found by key and sorted by id so a
/// listing does not reorder between reads.
pub async fn list(catalog: &dyn CatalogStore, org_id: &OrgId) -> anyhow::Result<Vec<Kept>> {
    Ok(catalog
        .list_org_benchmarks(org_id)
        .await?
        .into_iter()
        .map(|(benchmark_id, entry)| kept(org_id.clone(), benchmark_id, entry))
        .collect())
}

/// Read the entry under each of `keys`, concurrently and in order. One deleted
/// since the keys were listed is dropped.
pub async fn read_kept(
    catalog: &dyn CatalogStore,
    keys: Vec<(OrgId, BenchmarkId)>,
) -> anyhow::Result<Vec<Kept>> {
    let reads: Vec<Option<Kept>> = futures::stream::iter(keys)
        .map(|(org_id, benchmark_id)| read_one(catalog, org_id, benchmark_id))
        .buffered(STORAGE_CONCURRENCY)
        .try_collect()
        .await?;
    Ok(reads.into_iter().flatten().collect())
}

/// The entry under one key, or `None` when nothing is kept there. A read that
/// fails, rather than an entry that does not parse, fails the call.
pub async fn read_one(
    catalog: &dyn CatalogStore,
    org_id: OrgId,
    benchmark_id: BenchmarkId,
) -> anyhow::Result<Option<Kept>> {
    Ok(read_org_benchmark(catalog, &org_id, &benchmark_id)
        .await?
        .map(|entry| kept(org_id, benchmark_id, entry)))
}

fn kept(
    org_id: OrgId,
    benchmark_id: BenchmarkId,
    entry: Result<Benchmark, UnreadableObjectError>,
) -> Kept {
    if let Err(unreadable) = &entry {
        tracing::error!(
            %org_id, %benchmark_id, error = %unreadable,
            "unreadable private benchmark catalog entry"
        );
    }
    Kept {
        org_id,
        benchmark_id,
        entry,
    }
}

/// Whose private entries a caller reaches, beside the public catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogScope {
    /// One organization's, or none for a principal that belongs to none.
    Org(Option<OrgId>),
    /// Every organization's: the server's own passes, the host CLI, and a user
    /// holding `submit_any_plans`.
    Every,
}

/// The server's whole catalog: the public entries and every organization's,
/// read now. Public wins on a shared id, and an id two organizations hold
/// resolves to neither.
pub async fn server_catalog(
    catalog: &dyn CatalogStore,
    public: &HashMap<BenchmarkId, Benchmark>,
) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
    scoped_catalog(catalog, public, &CatalogScope::Every).await
}

/// The part of [`server_catalog`] `scope` reaches, with ownership still judged
/// across every organization. An unparseable entry is skipped; any other failed
/// read fails the call, so a store fault never reads as an unknown id.
pub async fn scoped_catalog(
    catalog: &dyn CatalogStore,
    public: &HashMap<BenchmarkId, Benchmark>,
    scope: &CatalogScope,
) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
    let mut merged = public.clone();
    let kept = match scope {
        CatalogScope::Org(None) => return Ok(merged),
        // The whole organization's catalog (one query on SQL), cut to what it solely owns.
        CatalogScope::Org(Some(org_id)) => {
            let (listed, own) =
                futures::try_join!(catalog.list_org_benchmark_ids(), list(catalog, org_id))?;
            let owners = sole_owners(listed, public);
            own.into_iter()
                .filter(|kept| owners.get(&kept.benchmark_id) == Some(org_id))
                .collect()
        }
        CatalogScope::Every => {
            let keys = sole_owners(catalog.list_org_benchmark_ids().await?, public)
                .into_iter()
                .map(|(benchmark_id, org_id)| (org_id, benchmark_id))
                .collect();
            read_kept(catalog, keys).await?
        }
    };
    for kept in kept {
        if let Ok(benchmark) = kept.entry {
            merged.insert(kept.benchmark_id, benchmark);
        }
    }
    Ok(merged)
}

/// [`server_catalog`] over a public catalog read now: what a pass that runs
/// away from a request starts from.
pub async fn load_server_catalog(
    catalog: &dyn CatalogStore,
) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
    let public = catalog.load_catalog().await?;
    server_catalog(catalog, &public).await
}

/// The one entry `benchmark_id` names server-wide: [`server_catalog`]'s answer
/// for a single id, at the cost of one listing and at most one read.
pub async fn server_benchmark(
    catalog: &dyn CatalogStore,
    public: &HashMap<BenchmarkId, Benchmark>,
    benchmark_id: &BenchmarkId,
) -> anyhow::Result<Option<Benchmark>> {
    if let Some(benchmark) = public.get(benchmark_id) {
        return Ok(Some(benchmark.clone()));
    }
    let listed = catalog
        .list_org_benchmark_ids()
        .await?
        .into_iter()
        .filter(|(_, id)| id == benchmark_id)
        .collect();
    match sole_owners(listed, public).remove(benchmark_id) {
        Some(org_id) => catalog.get_org_benchmark(&org_id, benchmark_id).await,
        None => Ok(None),
    }
}

/// Why what a campaign ran could not be accounted for.
#[derive(Debug, thiserror::Error)]
pub enum PrivateRunError {
    /// A source that cannot be read may name a private benchmark.
    #[error("{0} of the campaign's job definitions cannot be read")]
    UnreadableSources(usize),
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// What a campaign ran that belongs to one organization: the benchmarks the
/// public catalog does not hold, and the vault resources an organization owns.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PrivateRun {
    /// Each private benchmark, with the organization holding it.
    pub held: HashMap<BenchmarkId, OrgId>,
    /// Each organization's vault resource, with that organization. A person's
    /// own resource is not here: its results are theirs to file anywhere.
    pub vault: HashMap<VaultResourceId, OrgId>,
    /// Ids no catalog resolves now: a private entry since deleted, so whose
    /// results they are is no longer known.
    pub unresolved: BTreeSet<BenchmarkId>,
    /// Jobs whose definition says nothing either way: minted before job
    /// sources existed, or naming a vault resource in a shape no longer read.
    pub unknown_jobs: usize,
}

impl PrivateRun {
    /// Whether some of what the campaign ran cannot be placed in any
    /// organization, so only a caller over every organization hands it over.
    pub fn has_unknowns(&self) -> bool {
        !self.unresolved.is_empty() || self.unknown_jobs > 0
    }

    /// What `destination`'s organization does not hold, by id.
    pub fn outside(&self, destination: Option<&OrgId>) -> BTreeSet<&str> {
        let benchmarks = self.held.iter().map(|(id, org_id)| (id.as_str(), org_id));
        let resources = self.vault.iter().map(|(id, org_id)| (id.as_str(), org_id));
        benchmarks
            .chain(resources)
            .filter(|(_, org_id)| Some(*org_id) != destination)
            .map(|(id, _)| id)
            .collect()
    }
}

/// What `campaign`'s plans ran that is private to an organization: a new owner
/// outside it would read those results. A vault resource's owner is read from
/// the job, so a deleted record is still placed.
pub async fn private_run(
    catalog: &dyn CatalogStore,
    plans: &dyn PlanStore,
    public: &HashMap<BenchmarkId, Benchmark>,
    campaign: &Campaign,
) -> Result<PrivateRun, PrivateRunError> {
    let owners = sole_owners(catalog.list_org_benchmark_ids().await?, public);
    let job_ids: Vec<JobId> = crate::campaign_chain::every_plan(plans, campaign)
        .await?
        .into_iter()
        .flat_map(|manifest| manifest.job_ids)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let sources = plans.get_job_sources(&job_ids).await?;
    if !sources.unreadable.is_empty() {
        return Err(PrivateRunError::UnreadableSources(sources.unreadable.len()));
    }
    let mut run = PrivateRun {
        // An absent source has no entry.
        unknown_jobs: job_ids.len().saturating_sub(sources.items.len()),
        ..PrivateRun::default()
    };
    for (_, body) in &sources.items {
        if let Some(id) = body
            .pointer("/spec/benchmark")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| BenchmarkId::try_new(id).ok())
            .filter(|id| !public.contains_key(id))
        {
            match owners.get(&id) {
                Some(org_id) => run.held.insert(id, org_id.clone()).is_some(),
                None => run.unresolved.insert(id),
            };
        }
        let owners_named = crate::vault_check::vault_refs(body).ok().and_then(|refs| {
            refs.into_iter()
                .map(|vault_ref| {
                    let owner = vault_ref.principal.parse::<VaultPrincipalId>().ok()?;
                    Some((vault_ref.resource_id, owner))
                })
                .collect::<Option<Vec<_>>>()
        });
        match owners_named {
            Some(named) => run.vault.extend(named.into_iter().filter_map(
                |(resource_id, owner)| match owner {
                    VaultPrincipalId::Org(org_id) => Some((resource_id, org_id)),
                    VaultPrincipalId::User(_) => None,
                },
            )),
            None => run.unknown_jobs += 1,
        }
    }
    Ok(run)
}

/// The organization each listed id resolves to server-wide: dropped when the
/// public catalog holds it or more than one organization does.
fn sole_owners(
    listed: Vec<(OrgId, BenchmarkId)>,
    public: &HashMap<BenchmarkId, Benchmark>,
) -> HashMap<BenchmarkId, OrgId> {
    let holders = listed.into_iter().fold(
        HashMap::<BenchmarkId, Vec<OrgId>>::new(),
        |mut holders, (org_id, benchmark_id)| {
            holders.entry(benchmark_id).or_default().push(org_id);
            holders
        },
    );
    holders
        .into_iter()
        .filter_map(|(benchmark_id, mut orgs)| {
            if public.contains_key(&benchmark_id) {
                tracing::warn!(
                    %benchmark_id, orgs = ?orgs,
                    "a private benchmark takes a public id and is not served"
                );
                return None;
            }
            if orgs.len() > 1 {
                tracing::error!(
                    %benchmark_id, orgs = ?orgs,
                    "more than one organization keeps this benchmark id; the server resolves it \
                     to neither"
                );
                return None;
            }
            orgs.pop().map(|org_id| (benchmark_id, org_id))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pipette_core::config::Config;
    use pipette_core::types::Mint;
    use pipette_plan_types::{BenchmarkDefinition, PrefillThroughputBenchmark};
    use pipette_store_fs::build_local_fs_stores;

    use super::*;

    fn stores(dir: &tempfile::TempDir) -> anyhow::Result<Arc<dyn CatalogStore>> {
        let config = Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Config::default()
        };
        Ok(build_local_fs_stores(&config)?.catalog)
    }

    fn entry(id: &str) -> anyhow::Result<Benchmark> {
        Benchmark::from_def(BenchmarkDefinition::PrefillThroughput(
            PrefillThroughputBenchmark {
                benchmark_id: BenchmarkId::try_new(id)?,
                parameter_prefill_tokens: pipette_plan_types::NonNegativeCount::try_new(64)?,
                parameter_batch_size: pipette_plan_types::BatchSize::one(),
            },
        ))
    }

    #[tokio::test]
    async fn a_private_entry_cannot_take_a_public_id() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = stores(&dir)?;
        let acme = OrgId::mint();
        let public: HashMap<BenchmarkId, Benchmark> = [entry("prefill_throughput_256")?]
            .into_iter()
            .map(|b| (b.benchmark_id().clone(), b))
            .collect();

        let refused = put(&*catalog, &public, &acme, entry("prefill_throughput_256")?).await;
        assert!(matches!(refused, Err(OrgCatalogError::ShadowsPublic(_))));
        let refused = put(&*catalog, &public, &acme, entry("decode_throughput_acme")?).await;
        assert!(matches!(
            refused,
            Err(OrgCatalogError::MisleadingPrefix {
                benchmark_type: BenchmarkType::PrefillThroughput,
                prefix_type: BenchmarkType::DecodeThroughput,
                ..
            })
        ));
        assert!(
            list(&*catalog, &acme).await?.is_empty(),
            "nothing was written"
        );

        let written = put(&*catalog, &public, &acme, entry("prefill_throughput_64")?).await?;
        assert_eq!(written.org_id, Some(acme.clone()));
        let listed = list(&*catalog, &acme).await?;
        assert_eq!(
            listed
                .into_iter()
                .map(|kept| kept.entry.ok())
                .collect::<Vec<_>>(),
            vec![Some(written)]
        );
        Ok(())
    }

    /// The first organization may still replace its own.
    #[tokio::test]
    async fn a_private_entry_cannot_take_another_organizations_id() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = stores(&dir)?;
        let (acme, labs) = (OrgId::mint(), OrgId::mint());
        let public = HashMap::new();

        put(&*catalog, &public, &acme, entry("prefill_throughput_64")?).await?;
        let refused = put(&*catalog, &public, &labs, entry("prefill_throughput_64")?).await;
        assert!(matches!(refused, Err(OrgCatalogError::HeldElsewhere(_))));
        assert!(list(&*catalog, &labs).await?.is_empty());
        put(&*catalog, &public, &acme, entry("prefill_throughput_64")?).await?;
        Ok(())
    }

    /// A `vl_spec_decode` entry pinning a dataset whose digest repeats `digit`.
    fn pinning(id: &str, digit: char) -> anyhow::Result<Benchmark> {
        pinning_from(id, digit, "github.com/acme/corpus")
    }

    /// [`pinning`], with the dataset's upstream at `url`.
    fn pinning_from(id: &str, digit: char, url: &str) -> anyhow::Result<Benchmark> {
        let digest = digit.to_string().repeat(64);
        Benchmark::from_toml(
            id,
            &format!(
                r#"benchmark_type = "vl_spec_decode"
parameter_draft_length = 8
parameter_max_tokens = 2048

[dataset]
format  = "vl-spec-decode-corpus/1"
adapter = "mmspec@1"
digest  = "{digest}"
bytes   = 1024

[dataset.upstream]
source   = "git"
url      = "{url}"
revision = "0123456789abcdef0123456789abcdef01234567"
subpath  = "dataset/test"
identity = "2923603292c7dd463ad01a119f57965817778fc3"
digest   = "{upstream}"
"#,
                upstream = "0".repeat(64)
            ),
        )
    }

    /// The organization's own second pin, a dataset a public entry pins too, and a
    /// fresh one are allowed.
    #[tokio::test]
    async fn a_private_entry_cannot_pin_another_organizations_dataset() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = stores(&dir)?;
        let (acme, labs) = (OrgId::mint(), OrgId::mint());
        let public: HashMap<BenchmarkId, Benchmark> = [pinning("vl_spec_decode_public_k8", 'a')?]
            .into_iter()
            .map(|b| (b.benchmark_id().clone(), b))
            .collect();
        put(
            &*catalog,
            &public,
            &acme,
            pinning("vl_spec_decode_acme_k4", 'b')?,
        )
        .await?;

        let refused = put(
            &*catalog,
            &public,
            &labs,
            pinning("vl_spec_decode_labs_k4", 'b')?,
        )
        .await;
        assert!(matches!(
            refused,
            Err(OrgCatalogError::DatasetHeldElsewhere(_))
        ));
        assert!(list(&*catalog, &labs).await?.is_empty());

        put(
            &*catalog,
            &public,
            &acme,
            pinning("vl_spec_decode_acme_k8", 'b')?,
        )
        .await?;
        put(
            &*catalog,
            &public,
            &labs,
            pinning("vl_spec_decode_labs_k8", 'a')?,
        )
        .await?;
        put(
            &*catalog,
            &public,
            &labs,
            pinning("vl_spec_decode_labs_k2", 'c')?,
        )
        .await?;
        Ok(())
    }

    /// An `asr_eval` entry pinning one corpus and stating `samples` samples.
    fn asr_pinning(id: &str, samples: u32) -> anyhow::Result<Benchmark> {
        Benchmark::from_toml(
            id,
            &format!(
                r#"benchmark_type = "asr_eval"
parameter_asr_task = "fleurs_nl_nl"
parameter_dataset_version = "2026.10.1"
parameter_sample_count = {samples}
parameter_prompt_profile = "voxtral"
parameter_max_tokens = 500

[dataset]
format  = "asr-eval-corpus/1"
adapter = "evals-server@1"
digest  = "{digest}"
bytes   = 1024

[dataset.upstream]
source          = "evals_server"
eval_id         = "fleurs_nl_nl"
dataset_version = "2026.10.1"
prompt_profile  = "voxtral"
identity        = "{digest}"

[recipe]
family  = "Test family"
models  = [{{ repo = "org/repo" }}]
enable_thinking = false
sources = ["https://huggingface.co/org/repo"]
"#,
                digest = "d".repeat(64)
            ),
        )
    }

    /// Even an organization alone in pinning the digest: the probe follows one
    /// pin per digest, and the public one is served to every organization.
    #[tokio::test]
    async fn a_private_entry_cannot_pin_a_public_digest_differently() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = stores(&dir)?;
        let acme = OrgId::mint();
        let public: HashMap<BenchmarkId, Benchmark> = [
            pinning("vl_spec_decode_public_k8", 'a')?,
            asr_pinning("asr_eval_public", 100)?,
        ]
        .into_iter()
        .map(|b| (b.benchmark_id().clone(), b))
        .collect();

        let rivals = [
            pinning_from("vl_spec_decode_a_k8", 'a', "github.com/rival/corpus")?,
            asr_pinning("asr_eval_acme", 99)?,
        ];
        let refused = futures::stream::iter(rivals)
            .then(|rival| put(&*catalog, &public, &acme, rival))
            .collect::<Vec<_>>()
            .await;
        assert!(
            refused
                .iter()
                .all(|r| matches!(r, Err(OrgCatalogError::DatasetPinnedPublicly(_)))),
            "{refused:?}"
        );
        assert!(list(&*catalog, &acme).await?.is_empty());

        put(
            &*catalog,
            &public,
            &acme,
            asr_pinning("asr_eval_acme", 100)?,
        )
        .await?;
        Ok(())
    }

    fn text_eval(id: &str, dataset: &str) -> anyhow::Result<Benchmark> {
        Benchmark::from_toml(
            id,
            &format!(
                "benchmark_type = \"eval\"\nparameter_eval_id = \"math_500\"\n\
                 parameter_dataset_name = \"{dataset}\"\nparameter_max_tokens = 16\n"
            ),
        )
    }

    /// `tables` with the placeholder corpus digest made `id`'s own, so only the
    /// samples an entry names can tie it to another.
    fn own_digest(id: &str, tables: &str) -> String {
        use sha2::Digest as _;
        tables.replace(&"1".repeat(64), &hex::encode(sha2::Sha256::digest(id)))
    }

    fn vlm_eval(id: &str, dataset: &str) -> anyhow::Result<Benchmark> {
        Benchmark::from_toml(
            id,
            &format!(
                "benchmark_type = \"vlm_eval\"\nparameter_lmms_task = \"mmstar\"\n\
                 parameter_dataset_version = \"{dataset}\"\nparameter_sample_count = 2\n\
                 parameter_max_tokens = 16\n{}",
                own_digest(
                    id,
                    &pipette_core::dataset::vlm_eval_catalog_tables("mmstar", dataset, None)
                )
            ),
        )
    }

    fn asr_eval(id: &str, dataset: &str) -> anyhow::Result<Benchmark> {
        Benchmark::from_toml(
            id,
            &format!(
                "benchmark_type = \"asr_eval\"\nparameter_asr_task = \"asr_fleurs_nl_nl\"\n\
                 parameter_dataset_version = \"{dataset}\"\nparameter_sample_count = 2\n\
                 parameter_prompt_profile = \"voxtral\"\nparameter_max_tokens = 16\n{}",
                own_digest(
                    id,
                    &pipette_core::dataset::asr_eval_catalog_tables(
                        "asr_fleurs_nl_nl",
                        dataset,
                        "voxtral"
                    )
                )
            ),
        )
    }

    /// Another organization's samples are refused; the organization's own
    /// second entry, samples a public entry names, and fresh ones are allowed.
    #[rstest::rstest]
    #[case::text_eval(text_eval, "eval")]
    #[case::vlm_eval(vlm_eval, "vlm_eval")]
    #[case::asr_eval(asr_eval, "asr_eval")]
    #[tokio::test]
    async fn a_private_entry_cannot_name_another_organizations_samples(
        #[case] named: fn(&str, &str) -> anyhow::Result<Benchmark>,
        #[case] prefix: &str,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = stores(&dir)?;
        let (acme, labs) = (OrgId::mint(), OrgId::mint());
        let public: HashMap<BenchmarkId, Benchmark> = [named(&format!("{prefix}_public"), "1")?]
            .into_iter()
            .map(|b| (b.benchmark_id().clone(), b))
            .collect();
        put(
            &*catalog,
            &public,
            &acme,
            named(&format!("{prefix}_acme"), "2")?,
        )
        .await?;

        let refused = put(
            &*catalog,
            &public,
            &labs,
            named(&format!("{prefix}_labs"), "2")?,
        )
        .await;
        assert!(matches!(
            refused,
            Err(OrgCatalogError::SamplesHeldElsewhere { dataset, .. }) if dataset == "2"
        ));
        assert!(list(&*catalog, &labs).await?.is_empty());

        let allowed = [
            (&acme, "acme_2", "2"),
            (&labs, "labs_1", "1"),
            (&labs, "labs_3", "3"),
        ];
        futures::stream::iter(allowed)
            .map(Ok)
            .try_for_each(|(org, suffix, dataset)| {
                let (catalog, public) = (&catalog, &public);
                async move {
                    let benchmark = named(&format!("{prefix}_{suffix}"), dataset)?;
                    put(&**catalog, public, org, benchmark).await?;
                    anyhow::Ok(())
                }
            })
            .await
    }

    /// Spellings the evals server's URL would resolve to acme's `math_500` at
    /// `2` are refused before any comparison.
    #[rstest::rstest]
    #[case::dot("./2")]
    #[case::dot_dot("..")]
    #[case::encoded_dot("%2e/2")]
    #[case::traversal("a/../2")]
    #[case::query("2/samples?")]
    #[case::fragment("2/samples#")]
    #[tokio::test]
    async fn a_name_that_is_not_one_plain_segment_is_refused(
        #[case] dataset: &str,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = stores(&dir)?;
        let (acme, labs) = (OrgId::mint(), OrgId::mint());
        let public = HashMap::new();
        put(&*catalog, &public, &acme, text_eval("eval_acme", "2")?).await?;

        let refused = put(&*catalog, &public, &labs, text_eval("eval_labs", dataset)?).await;
        assert!(matches!(
            refused,
            Err(OrgCatalogError::UnsafeSampleName(name)) if name == dataset
        ));
        assert!(list(&*catalog, &labs).await?.is_empty());
        Ok(())
    }

    /// The public `prefill_throughput_256`, also planted in acme's catalog;
    /// `prefill_throughput_shared` held by both organizations behind the
    /// routes' back; and one entry of each organization's own.
    async fn planted(
        catalog: &dyn CatalogStore,
    ) -> anyhow::Result<(HashMap<BenchmarkId, Benchmark>, OrgId, OrgId)> {
        let (acme, labs) = (OrgId::mint(), OrgId::mint());
        let public: HashMap<BenchmarkId, Benchmark> = [entry("prefill_throughput_256")?]
            .into_iter()
            .map(|b| (b.benchmark_id().clone(), b))
            .collect();
        futures::future::try_join_all(
            [
                (&acme, "prefill_throughput_acme"),
                (&labs, "prefill_throughput_labs"),
                (&acme, "prefill_throughput_shared"),
                (&labs, "prefill_throughput_shared"),
                (&acme, "prefill_throughput_256"),
            ]
            .map(|(org, id)| async move { catalog.put_org_benchmark(org, &entry(id)?).await }),
        )
        .await?;
        Ok((public, acme, labs))
    }

    fn ids(catalog: &HashMap<BenchmarkId, Benchmark>) -> Vec<(&str, Option<&OrgId>)> {
        let mut ids: Vec<_> = catalog
            .values()
            .map(|b| (b.benchmark_id().as_str(), b.org_id.as_ref()))
            .collect();
        ids.sort();
        ids
    }

    /// A scope cuts the server's view, never widens it.
    #[tokio::test]
    async fn the_views_resolve_each_id_to_one_entry_or_none() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = stores(&dir)?;
        let (public, acme, labs) = planted(&*catalog).await?;

        let every = server_catalog(&*catalog, &public).await?;
        assert_eq!(
            ids(&every),
            vec![
                ("prefill_throughput_256", None),
                ("prefill_throughput_acme", Some(&acme)),
                ("prefill_throughput_labs", Some(&labs)),
            ]
        );
        let scope = CatalogScope::Org(Some(acme.clone()));
        assert_eq!(
            ids(&scoped_catalog(&*catalog, &public, &scope).await?),
            vec![
                ("prefill_throughput_256", None),
                ("prefill_throughput_acme", Some(&acme)),
            ]
        );
        let nobody = scoped_catalog(&*catalog, &public, &CatalogScope::Org(None)).await?;
        assert_eq!(ids(&nobody), vec![("prefill_throughput_256", None)]);
        Ok(())
    }

    /// One id at a time, the same answers the whole view gives.
    #[rstest::rstest]
    #[case::public("prefill_throughput_256", true)]
    #[case::sole_owner("prefill_throughput_labs", true)]
    #[case::two_owners("prefill_throughput_shared", false)]
    #[case::unknown("prefill_throughput_1", false)]
    #[tokio::test]
    async fn server_benchmark_agrees_with_the_view(
        #[case] id: &str,
        #[case] resolves: bool,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = stores(&dir)?;
        let (public, ..) = planted(&*catalog).await?;
        let id = BenchmarkId::try_new(id)?;

        let one = server_benchmark(&*catalog, &public, &id).await?;
        assert_eq!(one.is_some(), resolves);
        assert_eq!(
            one.as_ref(),
            server_catalog(&*catalog, &public).await?.get(&id)
        );
        Ok(())
    }

    /// A second delete is a refusal, not a silent no-op.
    #[tokio::test]
    async fn a_missing_entry_is_named() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = stores(&dir)?;
        let acme = OrgId::mint();
        let id = BenchmarkId::try_new("prefill_throughput_64")?;

        assert!(matches!(
            require(&*catalog, &acme, &id).await,
            Err(OrgCatalogError::NotFound { .. })
        ));
        put(&*catalog, &HashMap::new(), &acme, entry(id.as_str())?).await?;
        delete(&*catalog, &acme, &id).await?;
        assert!(matches!(
            delete(&*catalog, &acme, &id).await,
            Err(OrgCatalogError::NotFound { .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn an_unreadable_entry_is_listed_and_still_deleted() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = stores(&dir)?;
        let acme = OrgId::mint();
        let id = BenchmarkId::try_new("prefill_throughput_64")?;
        let org_dir = dir.path().join("org-benchmarks").join(acme.as_str());
        std::fs::create_dir_all(&org_dir)?;
        std::fs::write(
            org_dir.join(format!("{id}.toml")),
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 64\n\
             parameter_retired = 1\n",
        )?;
        let readable = put(
            &*catalog,
            &HashMap::new(),
            &acme,
            entry("prefill_throughput_128")?,
        )
        .await?;
        assert!(require(&*catalog, &acme, &id).await.is_err());
        let listed = list(&*catalog, &acme).await?;
        let [
            Kept {
                entry: Ok(first), ..
            },
            Kept {
                benchmark_id: broken,
                entry: Err(error),
                ..
            },
        ] = listed.as_slice()
        else {
            anyhow::bail!("the readable entry, then the unreadable one, by id: {listed:?}");
        };
        assert_eq!(first, &readable);
        assert_eq!(broken, &id);
        assert!(error.reason.contains("parameter_retired"), "{error}");

        delete(&*catalog, &acme, &id).await?;
        assert_eq!(
            catalog.list_org_benchmark_ids().await?,
            vec![(acme, readable.benchmark_id().clone())]
        );
        Ok(())
    }

    /// A failed read is the server's fault, not the entry's.
    #[tokio::test]
    async fn a_failed_read_fails_the_listing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = stores(&dir)?;
        let acme = OrgId::mint();
        // Listed by its key, and not readable as a file.
        std::fs::create_dir_all(
            dir.path()
                .join("org-benchmarks")
                .join(acme.as_str())
                .join("prefill_throughput_64.toml"),
        )?;
        assert_eq!(catalog.list_org_benchmark_ids().await?.len(), 1);
        assert!(list(&*catalog, &acme).await.is_err());
        Ok(())
    }

    /// A local catalog whose read of `failing` fails with `fault`'s error.
    /// The views only list and read, so any other call is recorded.
    struct FailingRead {
        inner: Arc<dyn CatalogStore>,
        failing: BenchmarkId,
        fault: fn() -> anyhow::Error,
        unexpected: pipette_store_testkit::UnexpectedCalls,
    }

    impl FailingRead {
        fn new(
            inner: Arc<dyn CatalogStore>,
            failing: &str,
            fault: fn() -> anyhow::Error,
        ) -> anyhow::Result<Self> {
            Ok(Self {
                inner,
                failing: BenchmarkId::try_new(failing)?,
                fault,
                unexpected: pipette_store_testkit::UnexpectedCalls::default(),
            })
        }
    }

    #[async_trait::async_trait]
    impl CatalogStore for FailingRead {
        async fn load_catalog(&self) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
            Err(self.unexpected.record("load_catalog"))
        }

        async fn load_org_catalog(
            &self,
            _: &OrgId,
        ) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
            Err(self.unexpected.record("load_org_catalog"))
        }

        async fn get_org_benchmark(
            &self,
            org_id: &OrgId,
            benchmark_id: &BenchmarkId,
        ) -> anyhow::Result<Option<Benchmark>> {
            if *benchmark_id == self.failing {
                return Err((self.fault)());
            }
            self.inner.get_org_benchmark(org_id, benchmark_id).await
        }

        async fn put_org_benchmark(&self, _: &OrgId, _: &Benchmark) -> anyhow::Result<()> {
            Err(self.unexpected.record("put_org_benchmark"))
        }

        async fn delete_org_benchmark(&self, _: &OrgId, _: &BenchmarkId) -> anyhow::Result<()> {
            Err(self.unexpected.record("delete_org_benchmark"))
        }

        async fn list_org_benchmark_ids(&self) -> anyhow::Result<Vec<(OrgId, BenchmarkId)>> {
            self.inner.list_org_benchmark_ids().await
        }
    }

    #[tokio::test]
    async fn an_unparsable_entry_is_skipped_by_the_views() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let inner = stores(&dir)?;
        let (public, acme, labs) = planted(&*inner).await?;
        let catalog = FailingRead::new(inner, "prefill_throughput_acme", || {
            UnreadableObjectError {
                key: "org-benchmarks/acme/prefill_throughput_acme.toml".into(),
                reason: "unknown field `parameter_retired`".into(),
            }
            .into()
        })?;

        assert_eq!(
            ids(&server_catalog(&catalog, &public).await?),
            vec![
                ("prefill_throughput_256", None),
                ("prefill_throughput_labs", Some(&labs)),
            ]
        );
        let scope = CatalogScope::Org(Some(acme));
        assert_eq!(
            ids(&scoped_catalog(&catalog, &public, &scope).await?),
            vec![("prefill_throughput_256", None)]
        );
        catalog.unexpected.none()
    }

    /// The view fails rather than answer without the entry, which a plan check
    /// would call unknown. A scope that does not admit the entry never reads it.
    #[tokio::test]
    async fn a_failed_read_fails_the_views() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let inner = stores(&dir)?;
        let (public, acme, labs) = planted(&*inner).await?;
        let catalog = FailingRead::new(inner, "prefill_throughput_acme", || {
            anyhow::anyhow!("pool timed out while waiting for an open connection")
        })?;

        for scope in [CatalogScope::Every, CatalogScope::Org(Some(acme))] {
            let Err(e) = scoped_catalog(&catalog, &public, &scope).await else {
                anyhow::bail!("{scope:?}: the view answered without the entry");
            };
            assert!(format!("{e:#}").contains("pool timed out"), "{e:#}");
        }
        let scope = CatalogScope::Org(Some(labs.clone()));
        assert_eq!(
            ids(&scoped_catalog(&catalog, &public, &scope).await?),
            vec![
                ("prefill_throughput_256", None),
                ("prefill_throughput_labs", Some(&labs)),
            ]
        );
        catalog.unexpected.none()
    }

    /// A person's own vault resource is placed nowhere; a job with no definition or
    /// an unreadable vault source counts as unknown.
    #[tokio::test]
    async fn a_run_places_vault_resources_and_counts_what_it_cannot() -> anyhow::Result<()> {
        use chrono::Utc;
        use serde_json::json;

        use pipette_core::campaign::{CampaignOwner, NewCampaign};
        use pipette_core::plan::PlanManifest;
        use pipette_plan_types::{NonEmptyTrimmedString, PlanId, PlanStatus, UserId, VaultPath};

        let dir = tempfile::tempdir()?;
        let catalog = stores(&dir)?;
        let plans = pipette_store_fs::build_local_fs_plan_store(
            &pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
        )?;
        let (acme, labs) = (OrgId::mint(), OrgId::mint());
        let (shared, personal) = (VaultResourceId::mint(), VaultResourceId::mint());
        let model = |resource_id: &VaultResourceId, principal: Option<&str>| {
            let mut model = json!({
                "source": "vault_url",
                "url": VaultPath::of(resource_id.clone()).to_string(),
            });
            if let (Some(principal), Some(fields)) = (principal, model.as_object_mut()) {
                fields.insert("principal_id".into(), json!(principal));
            }
            json!({ "spec": { "benchmark": "prefill_throughput_256", "model": model } })
        };
        let owner = UserId::mint();
        let bodies = [
            Some(model(&shared, Some(acme.as_str()))),
            Some(model(&personal, Some(owner.as_str()))),
            // No principal: a shape filing refuses, so nothing says whose it is.
            Some(model(&shared, None)),
            // Minted before job sources existed.
            None,
        ];
        let job_ids: Vec<JobId> = (1..=bodies.len() as u128)
            .map(|n| JobId::from_uuid(uuid::Uuid::from_u128(n)))
            .collect();
        for (job_id, body) in job_ids.iter().zip(&bodies) {
            if let Some(body) = body {
                plans.put_job_source(job_id, body).await?;
            }
        }
        let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
        plans
            .put_plan(&PlanManifest {
                plan_id: plan_id.clone(),
                status: PlanStatus::Active,
                created_at: Utc::now(),
                submitted_by: None,
                campaign_id: None,
                previous_plan_id: None,
                source_sha256: None,
                submitted_sha256: None,
                plan_warnings: Vec::new(),
                benchmark_types: Default::default(),
                job_ids,
                content_keys: Default::default(),
                rerun_of: Default::default(),
                warnings: Vec::new(),
                job_expiry: None,
                expired_jobs: Default::default(),
                settled_jobs: Default::default(),
                progress_snapshot: None,
                terminal_at: None,
            })
            .await?;
        let campaign = NewCampaign::new(
            NonEmptyTrimmedString::try_new("Nightly")?,
            CampaignOwner::User { user_id: owner },
        )
        .around(plan_id, Utc::now());
        let public: HashMap<BenchmarkId, Benchmark> = [entry("prefill_throughput_256")?]
            .into_iter()
            .map(|benchmark| (benchmark.benchmark_id().clone(), benchmark))
            .collect();

        let run = private_run(&*catalog, &*plans, &public, &campaign).await?;
        assert_eq!(
            run,
            PrivateRun {
                vault: HashMap::from([(shared.clone(), acme.clone())]),
                unknown_jobs: 2,
                ..PrivateRun::default()
            }
        );
        assert!(run.has_unknowns());
        assert!(run.outside(Some(&acme)).is_empty());
        assert_eq!(
            run.outside(Some(&labs)),
            BTreeSet::from([shared.as_str()]),
            "another organization does not hold it"
        );
        assert_eq!(run.outside(None), BTreeSet::from([shared.as_str()]));
        Ok(())
    }
}
