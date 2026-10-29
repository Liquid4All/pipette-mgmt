//! `pipette-mgmt dataset-cache`: probe records an upstream verdict on each
//! artifact (all the serving path reads), build creates missing artifacts, and
//! pins prints a catalog entry's hashes. Nothing here deletes an artifact.

mod adapters;
mod evals_server;
mod format;
mod git;

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;

use pipette_core::config::Config;
use pipette_core::dataset::{DatasetArtifact, ProbeState, ProbeVerdict, UpstreamIdentity};
use pipette_plan_types::{
    BenchmarkId, CorpusFormat, Dataset, DatasetRepositoryUrl, DatasetUpstream, Sha256,
};
use pipette_store::Stores;

/// Upstreams reached at once, so a run takes at most `ceil(n / CONCURRENCY)`
/// deadlines.
const CONCURRENCY: usize = 4;

/// How long an evals server's `/samples` body may send nothing before its
/// build fails, so a stalled connection cannot hold a build (and its blocking
/// parser) to the deadline, or past it.
const BODY_STALL: Duration = Duration::from_secs(300);

/// How the upstreams a catalog pins are reached. Production spells a git `url`
/// `https://` and asks the configured evals server; the tests point the same
/// catalog at a `file://` repository and a local evals server.
trait Upstreams: Sync {
    fn remote(&self, url: &DatasetRepositoryUrl) -> String;
    /// The evals server's base URL (`evals_server_url`).
    fn evals_server(&self) -> &str;
    fn http(&self) -> &reqwest::Client;
    /// [`BODY_STALL`]; the tests shorten it.
    fn body_stall(&self) -> Duration {
        BODY_STALL
    }
}

struct Live {
    evals_server: String,
    http: reqwest::Client,
}

impl Live {
    fn new(config: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            evals_server: config.evals_server_url.clone(),
            // No client-wide read timeout: the evals server may take a while
            // to send `/samples`' headers (a first pass over a set to learn
            // its digest), and the build's deadline bounds that. The body has
            // its own stall bound ([`BODY_STALL`]).
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .build()?,
        })
    }
}

impl Upstreams for Live {
    fn remote(&self, url: &DatasetRepositoryUrl) -> String {
        format!("https://{url}")
    }

    fn evals_server(&self) -> &str {
        &self.evals_server
    }

    fn http(&self) -> &reqwest::Client {
        &self.http
    }
}

/// One catalog pin: the artifact digest, what it pins, and who pins it.
struct Pin {
    digest: Sha256,
    dataset: Dataset,
    benchmarks: Vec<BenchmarkId>,
    /// The `parameter_sample_count` an asr_eval entry pinning it states, which
    /// its corpus must hold; `None` for every other type.
    samples: Option<u32>,
}

/// One digest's pin: its dataset, who pins it, and the sample count they
/// state ([`Pin::samples`]).
type PinnedBy = (Dataset, Vec<BenchmarkId>, Option<u32>);

/// The datasets the catalog pins, one per artifact digest. A benchmark that
/// contradicts an earlier pin of a digest is left out and reported; public
/// entries come first, so a private one never displaces a public pin.
struct Pinned {
    datasets: BTreeMap<String, PinnedBy>,
    conflicts: Vec<String>,
}

impl Pinned {
    fn pins(self) -> impl Iterator<Item = Pin> {
        self.datasets
            .into_iter()
            .map(|(_, (dataset, benchmarks, samples))| Pin {
                digest: dataset.digest.clone(),
                dataset,
                benchmarks,
                samples,
            })
    }
}

/// The samples an asr_eval entry says its corpus holds. Checked at build, so
/// an entry that disagrees with its own corpus fails there, not on every
/// device after the download.
pub fn stated_samples(benchmark: &pipette_core::benchmark::Benchmark) -> Option<u32> {
    use pipette_plan_types::BenchmarkDefinition as Def;
    match &benchmark.def {
        Def::AsrEval(asr) => u32::try_from(asr.parameter_sample_count.into_inner()).ok(),
        Def::PrefillThroughput(_)
        | Def::DecodeThroughput(_)
        | Def::EndToEndLatency(_)
        | Def::MaxMemoryUsage(_)
        | Def::Eval(_)
        | Def::VlmEval(_)
        | Def::VlThroughput(_)
        | Def::VlMaxMemory(_)
        | Def::AsrThroughput(_)
        | Def::AsrMaxMemory(_)
        | Def::VlSpecDecode(_) => None,
    }
}

async fn pinned(stores: &Stores) -> anyhow::Result<Pinned> {
    // Every organization's pins too: a private benchmark's dataset is only
    // servable once this builds its artifact.
    let public = stores.catalog.load_catalog().await?;
    let catalog = crate::org_catalog::server_catalog(&*stores.catalog, &public).await?;
    let mut ids: Vec<_> = catalog
        .keys()
        .map(|id| (!public.contains_key(id), id.clone()))
        .collect();
    ids.sort();
    let mut conflicts = Vec::new();
    let datasets = ids
        .into_iter()
        .filter_map(|(_, id)| {
            let benchmark = catalog.get(&id)?;
            Some((benchmark.dataset()?, stated_samples(benchmark), id))
        })
        .fold(BTreeMap::new(), |mut out, (dataset, samples, id)| {
            match out.entry(dataset.digest.to_string()) {
                Entry::Occupied(mut entry) => {
                    let (existing, benchmarks, stated): &mut PinnedBy = entry.get_mut();
                    if existing == dataset && *stated == samples {
                        benchmarks.push(id);
                    } else if existing == dataset {
                        conflicts.push(format!(
                            "{id} states {} samples for artifact {}, {} another count",
                            samples.unwrap_or_default(),
                            dataset.digest,
                            benchmark_list(benchmarks)
                        ));
                    } else {
                        conflicts.push(format!(
                            "{id} pins artifact {} differently from {}",
                            dataset.digest,
                            benchmark_list(benchmarks)
                        ));
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert((dataset.clone(), vec![id], samples));
                }
            }
            out
        });
    Ok(Pinned {
        datasets,
        conflicts,
    })
}

/// How many artifacts a probe or build run touches, for sizing its lease.
pub async fn pinned_count(stores: &Stores) -> anyhow::Result<usize> {
    Ok(pinned(stores).await?.datasets.len())
}

/// The longest a build run can take, what the build lock's lease is sized
/// from: every pinned artifact at its format's deadline, except that an
/// asr-eval corpus counts only while it is unbuilt. Each has hours, so
/// counting every built one would hold a crashed run's lease for days; a run
/// skips a built one in a moment. With no asr_eval entry this is the lease
/// it always was: every artifact at its deadline, one at least.
pub async fn build_run_deadline(config: &Config, stores: &Stores) -> anyhow::Result<Duration> {
    let deadlines = BuildDeadlines::of(config);
    let mut total = Duration::ZERO;
    for pin in pinned(stores).await?.pins() {
        let format = pin.dataset.format;
        if format == CorpusFormat::AsrEvalCorpusV1 && is_built(stores, &pin.digest).await? {
            continue;
        }
        total = total.saturating_add(deadlines.of_format(format));
    }
    Ok(total.max(deadlines.corpus))
}

/// Whether both an artifact's tar and its record exist, as a build that
/// would skip it reads them.
async fn is_built(stores: &Stores, digest: &Sha256) -> anyhow::Result<bool> {
    Ok(stores.dataset_cache.has_tar(digest).await?
        && stores.dataset_cache.get_artifact(digest).await?.is_some())
}

/// One build's deadline by corpus format. An asr-eval corpus is gigabytes of
/// audio streamed through the evals server (SPGISpeech's about 15 GB of
/// base64), so it gets its own, longer one.
#[derive(Debug, Clone, Copy)]
struct BuildDeadlines {
    corpus: Duration,
    asr_eval: Duration,
}

impl BuildDeadlines {
    fn of(config: &Config) -> Self {
        Self {
            corpus: secs(config.dataset_build_timeout_secs),
            asr_eval: secs(config.dataset_asr_build_timeout_secs),
        }
    }

    fn of_format(self, format: CorpusFormat) -> Duration {
        match format {
            CorpusFormat::AsrEvalCorpusV1 => self.asr_eval,
            CorpusFormat::VlSpecDecodeCorpusV1 | CorpusFormat::VlmEvalCorpusV1 => self.corpus,
        }
    }
}

/// One deadline for every format, as the tests use.
impl From<Duration> for BuildDeadlines {
    fn from(deadline: Duration) -> Self {
        Self {
            corpus: deadline,
            asr_eval: deadline,
        }
    }
}

fn scratch_dir() -> anyhow::Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new()
        .prefix("pipette-dataset-")
        .tempdir()?)
}

fn secs(n: std::num::NonZeroU64) -> Duration {
    Duration::from_secs(n.get())
}

fn benchmark_list(benchmarks: &[BenchmarkId]) -> String {
    benchmarks
        .iter()
        .map(BenchmarkId::as_str)
        .collect::<Vec<_>>()
        .join(",")
}

fn report_conflicts(conflicts: &[String]) {
    conflicts.iter().for_each(|conflict| {
        tracing::error!(
            event = "dataset_pin_conflict",
            conflict,
            "benchmark skipped: it pins an artifact another benchmark pins differently"
        );
    });
}

// ---------------------------------------------------------------------------
// probe
// ---------------------------------------------------------------------------

/// Errors when any artifact is not `ok`, so a failed run pages until the state
/// clears.
pub async fn run_probe(config: &Config, stores: &Stores) -> anyhow::Result<()> {
    probe_all(
        &Live::new(config)?,
        secs(config.dataset_probe_timeout_secs),
        stores,
    )
    .await
}

async fn probe_all(
    upstreams: &impl Upstreams,
    deadline: Duration,
    stores: &Stores,
) -> anyhow::Result<()> {
    let pinned = pinned(stores).await?;
    report_conflicts(&pinned.conflicts);
    let conflicts = pinned.conflicts.clone();
    // One fetch per upstream, however many artifacts keep different subsets
    // of it: the verdict is about the upstream, and every artifact gets it.
    let by_upstream = pinned
        .pins()
        .fold(BTreeMap::<String, Vec<Pin>>::new(), |mut groups, pin| {
            groups
                .entry(pin.dataset.upstream.reference())
                .or_default()
                .push(pin);
            groups
        });
    let outcomes: Vec<Vec<String>> = futures::stream::iter(by_upstream.into_values())
        .map(|pins| probe_upstream_pins(upstreams, deadline, stores, pins))
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    let not_ok: Vec<String> = conflicts
        .into_iter()
        .chain(outcomes.into_iter().flatten())
        .collect();
    anyhow::ensure!(
        not_ok.is_empty(),
        "dataset upstreams not ok: {}",
        not_ok.join(", ")
    );
    Ok(())
}

/// A built artifact awaiting a verdict.
struct Built {
    artifact: DatasetArtifact,
    benchmarks: Vec<BenchmarkId>,
}

/// Probe the upstream these pins share and record the verdict on each built
/// artifact; one artifact's store failure does not skip the others.
async fn probe_upstream_pins(
    upstreams: &impl Upstreams,
    deadline: Duration,
    stores: &Stores,
    pins: Vec<Pin>,
) -> Vec<String> {
    let Some(upstream) = pins.first().map(|p| p.dataset.upstream.clone()) else {
        return Vec::new();
    };
    let failed = |digest: &Sha256, benchmarks: &[BenchmarkId], e: &anyhow::Error| {
        tracing::error!(
            event = "dataset_probe_failed",
            digest = digest.as_ref(),
            benchmarks = benchmark_list(benchmarks),
            error = format!("{e:#}"),
            "dataset probe could not record a verdict"
        );
        format!("{digest} (probe failed)")
    };
    // Only a built artifact carries a verdict; the build creates the rest.
    let (built, mut not_ok) = futures::stream::iter(pins)
        .then(|pin| async move {
            let artifact = stores.dataset_cache.get_artifact(&pin.digest).await;
            (pin, artifact)
        })
        .fold(
            (Vec::new(), Vec::new()),
            |(mut built, mut not_ok), (pin, artifact)| async move {
                match artifact {
                    Ok(Some(artifact)) => built.push(Built {
                        artifact,
                        benchmarks: pin.benchmarks,
                    }),
                    Ok(None) => {
                        tracing::debug!(digest = %pin.digest, "no artifact to probe yet; the build creates it");
                    }
                    Err(e) => not_ok.push(failed(&pin.digest, &pin.benchmarks, &e)),
                }
                (built, not_ok)
            },
        )
        .await;
    if built.is_empty() {
        return not_ok;
    }
    let reached = match scratch_dir() {
        Ok(scratch) => {
            match tokio::time::timeout(
                deadline,
                probe_upstream(upstreams, scratch.path(), &upstream),
            )
            .await
            {
                Ok(reached) => reached,
                Err(_) => Reached::Failed(
                    ProbeState::Unavailable,
                    format!("probe timed out after {}s", deadline.as_secs()),
                ),
            }
        }
        Err(e) => Reached::Failed(
            ProbeState::Unavailable,
            format!("no scratch directory: {e:#}"),
        ),
    };
    let now = Utc::now();
    let recorded = futures::stream::iter(built)
        .then(|mut built| {
            // Each artifact against its own pin: two entries may pin one upstream
            // at different identities, and only the one it still publishes is ok.
            let (state, observed, detail) = reached.verdict(&built.artifact.upstream);
            async move {
                let previous = built.artifact.probe.state;
                built.artifact.probe =
                    ProbeVerdict::next(&built.artifact.probe, state, now, observed, detail);
                let written = stores.dataset_cache.put_artifact(&built.artifact).await;
                if written.is_ok() && state != previous {
                    report_transition(&built.artifact, &built.benchmarks);
                }
                (built, written)
            }
        })
        .collect::<Vec<_>>()
        .await;
    not_ok.extend(recorded.into_iter().filter_map(|(built, written)| {
        let digest = &built.artifact.digest;
        let state = built.artifact.probe.state;
        match written {
            Err(e) => Some(failed(digest, &built.benchmarks, &e)),
            Ok(()) if state != ProbeState::Ok => Some(format!("{digest} ({state})")),
            Ok(()) => None,
        }
    }));
    not_ok
}

/// What one probe of an upstream found.
enum Reached {
    /// The identity the upstream publishes now.
    Observed(UpstreamIdentity),
    /// No identity: the state it is in and why.
    Failed(ProbeState, String),
}

impl Reached {
    /// The verdict for an artifact built from `upstream`, judged against the
    /// identity that artifact pins.
    fn verdict(
        &self,
        upstream: &DatasetUpstream,
    ) -> (ProbeState, Option<UpstreamIdentity>, Option<String>) {
        match self {
            Self::Observed(observed) => {
                let pinned = UpstreamIdentity::pinned(upstream);
                if *observed == pinned {
                    (ProbeState::Ok, Some(observed.clone()), None)
                } else {
                    let detail = format!(
                        "{} now publishes {observed}, pinned {pinned}",
                        upstream.reference()
                    );
                    (ProbeState::Moved, Some(observed.clone()), Some(detail))
                }
            }
            Self::Failed(state, detail) => (*state, None, Some(detail.clone())),
        }
    }
}

async fn probe_upstream(
    upstreams: &impl Upstreams,
    scratch: &Path,
    upstream: &DatasetUpstream,
) -> Reached {
    match upstream {
        DatasetUpstream::Git {
            url,
            revision,
            subpath,
            ..
        } => match git::probe(scratch, &upstreams.remote(url), revision, subpath.as_ref()).await {
            git::Reached::Tree(observed) => Reached::Observed(UpstreamIdentity::GitTree(observed)),
            git::Reached::Gone(detail) => Reached::Failed(ProbeState::Gone, detail),
            git::Reached::Unavailable(detail) => Reached::Failed(ProbeState::Unavailable, detail),
        },
        DatasetUpstream::EvalsServer { .. } => {
            match evals_server::probe(upstreams.http(), upstreams.evals_server(), upstream).await {
                evals_server::Reached::Digest(observed) => {
                    Reached::Observed(UpstreamIdentity::Samples(observed))
                }
                evals_server::Reached::Gone(detail) => Reached::Failed(ProbeState::Gone, detail),
                evals_server::Reached::Unavailable(detail) => {
                    Reached::Failed(ProbeState::Unavailable, detail)
                }
            }
        }
    }
}

/// One event per state change, under a stable `event` name an alarm matches.
fn report_transition(artifact: &DatasetArtifact, benchmarks: &[BenchmarkId]) {
    let benchmarks = benchmark_list(benchmarks);
    let digest = artifact.digest.as_ref();
    let upstream = artifact.upstream.reference();
    let detail = artifact.probe.detail.as_deref().unwrap_or_default();
    let expected = UpstreamIdentity::pinned(&artifact.upstream).to_string();
    let observed = artifact
        .probe
        .observed_identity
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    match artifact.probe.state {
        ProbeState::Ok => tracing::warn!(
            event = "dataset_upstream_recovered",
            digest,
            benchmarks,
            upstream,
            "dataset upstream backs its pin again"
        ),
        ProbeState::Unavailable => tracing::error!(
            event = "dataset_upstream_unavailable",
            digest,
            benchmarks,
            upstream,
            detail,
            "dataset upstream unreachable; refusing to serve"
        ),
        ProbeState::Gone => tracing::error!(
            event = "dataset_upstream_gone",
            digest,
            benchmarks,
            upstream,
            detail,
            "dataset upstream no longer publishes the pin; refusing to serve"
        ),
        ProbeState::Moved => tracing::error!(
            event = "dataset_upstream_moved",
            digest,
            benchmarks,
            upstream,
            expected,
            observed,
            detail,
            "dataset upstream content differs from the pin; refusing to serve"
        ),
    }
}

// ---------------------------------------------------------------------------
// build
// ---------------------------------------------------------------------------

/// Build what is missing; an existing artifact is never rebuilt or deleted.
/// Errors when any build failed.
pub async fn run_build(config: &Config, stores: &Stores) -> anyhow::Result<()> {
    build_all(&Live::new(config)?, BuildDeadlines::of(config), stores).await
}

/// Builds the missing artifacts, four at a time; asr-eval corpora after the
/// others, one at a time, as each needs scratch for about twice its corpus.
async fn build_all(
    upstreams: &impl Upstreams,
    deadlines: impl Into<BuildDeadlines>,
    stores: &Stores,
) -> anyhow::Result<()> {
    let deadlines = deadlines.into();
    let pinned = pinned(stores).await?;
    report_conflicts(&pinned.conflicts);
    let conflicts = pinned.conflicts.clone();
    let (asr, others): (Vec<Pin>, Vec<Pin>) = pinned
        .pins()
        .partition(|pin| pin.dataset.format == CorpusFormat::AsrEvalCorpusV1);
    let build = |pin: Pin| async move {
        let deadline = deadlines.of_format(pin.dataset.format);
        build_reported(upstreams, deadline, stores, pin).await
    };
    let mut outcomes: Vec<Option<String>> = futures::stream::iter(others)
        .map(build)
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    outcomes.extend(
        futures::stream::iter(asr)
            .then(build)
            .collect::<Vec<_>>()
            .await,
    );
    let failed: Vec<String> = conflicts
        .into_iter()
        .chain(outcomes.into_iter().flatten())
        .collect();
    anyhow::ensure!(
        failed.is_empty(),
        "dataset builds failed: {}",
        failed.join(", ")
    );
    Ok(())
}

/// One pin's build, logged; the digest of a failed one.
async fn build_reported(
    upstreams: &impl Upstreams,
    deadline: Duration,
    stores: &Stores,
    pin: Pin,
) -> Option<String> {
    let benchmarks = benchmark_list(&pin.benchmarks);
    match build_one(
        upstreams,
        deadline,
        stores,
        &pin.digest,
        &pin.dataset,
        pin.samples,
    )
    .await
    {
        Ok(false) => None,
        Ok(true) => {
            tracing::info!(digest = %pin.digest, benchmarks, bytes = pin.dataset.bytes, "dataset artifact built");
            None
        }
        Err(e) => {
            tracing::error!(
                event = "dataset_build_failed",
                digest = pin.digest.as_ref(),
                benchmarks,
                upstream = %pin.dataset.upstream.reference(),
                error = format!("{e:#}"),
                "dataset artifact build failed"
            );
            Some(pin.digest.to_string())
        }
    }
}

/// Build the artifact unless both its tar and metadata exist; `true` when a
/// new one was written. `samples` is the count its entries state
/// ([`Pin::samples`]), which the corpus must hold.
async fn build_one(
    upstreams: &impl Upstreams,
    deadline: Duration,
    stores: &Stores,
    digest: &Sha256,
    dataset: &Dataset,
    samples: Option<u32>,
) -> anyhow::Result<bool> {
    if stores.dataset_cache.has_tar(digest).await?
        && let Some(artifact) = stores.dataset_cache.get_artifact(digest).await?
    {
        anyhow::ensure!(
            artifact.is_built_from(dataset),
            "artifact {digest} was built from {}, this entry pins {}: two pins share a digest",
            artifact.upstream.reference(),
            dataset.upstream.reference()
        );
        // A count stated after the build is checked here, as `GET /datasets`
        // checks it before serving.
        if let Some(stated) = samples {
            ensure_holds(artifact.samples, stated)?;
        }
        return Ok(false);
    }
    tracing::info!(%digest, "building dataset artifact");
    let scratch = scratch_dir()?;
    let built = materialize(upstreams, scratch.path(), dataset, deadline)
        .await?
        .ok_or_else(|| anyhow::anyhow!("build timed out after {}s", deadline.as_secs()))?;
    let pinned = UpstreamIdentity::pinned(&dataset.upstream);
    anyhow::ensure!(
        built.identity == pinned,
        "upstream moved: {} observed, {pinned} pinned",
        built.identity
    );
    if let (
        DatasetUpstream::Git {
            digest: pinned_tree,
            ..
        },
        Some(tree),
    ) = (&dataset.upstream, &built.upstream_digest)
    {
        anyhow::ensure!(
            tree == pinned_tree,
            "upstream tree digest {tree} differs from the pinned {pinned_tree}"
        );
    }
    anyhow::ensure!(
        built.digest == dataset.digest && built.bytes == dataset.bytes,
        "packed artifact is {} ({} bytes), pinned {} ({} bytes): the adapter or packer changed",
        built.digest,
        built.bytes,
        dataset.digest,
        dataset.bytes
    );
    let held = match &built.contents {
        Contents::VlmEval { samples, .. } => Some(*samples),
        Contents::VlSpecDecode { .. } => None,
    };
    stores
        .dataset_cache
        .put_tar(&dataset.digest, &built.tar)
        .await?;
    // A completed build is a passing probe: the upstream was just verified in
    // full.
    let now = Utc::now();
    stores
        .dataset_cache
        .put_artifact(&DatasetArtifact {
            digest: dataset.digest.clone(),
            bytes: dataset.bytes,
            format: dataset.format,
            adapter: dataset.adapter,
            upstream: dataset.upstream.clone(),
            subset: dataset.subset.clone(),
            samples: held,
            built_at: now,
            probe: ProbeVerdict {
                state: ProbeState::Ok,
                checked_at: now,
                since: now,
                observed_identity: Some(built.identity),
                detail: None,
            },
        })
        .await?;
    // Checked once the artifact is stored, so a later run reads the count
    // rather than streaming the whole set again; `GET /datasets` refuses
    // the entry until its count agrees.
    if let Some(stated) = samples {
        ensure_holds(held, stated)?;
    }
    Ok(true)
}

/// A corpus holding `held` samples serves an entry stating `stated` only when
/// they agree. Every corpus an asr_eval entry pins (`asr-eval-corpus/1`) is
/// recorded with its count.
pub fn ensure_holds(held: Option<u32>, stated: u32) -> anyhow::Result<()> {
    let held =
        held.ok_or_else(|| anyhow::anyhow!("the corpus was recorded without a sample count"))?;
    anyhow::ensure!(
        held == stated,
        "the corpus holds {held} samples, its entry states parameter_sample_count = \
         {stated}: every device would download it and then refuse it"
    );
    Ok(())
}

/// What a corpus holds, for the operator who pins it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Contents {
    /// One subset when the entry names a `subset`, otherwise every subset the
    /// upstream has, for an operator to split into per-dataset entries.
    VlSpecDecode {
        subsets: Vec<String>,
        samples: u32,
        turns: u32,
    },
    VlmEval {
        samples: u32,
        media: u32,
        /// The `/samples` body this corpus replaces.
        served_bytes: u64,
    },
}

/// What fetching and packing an upstream produced.
struct Materialized {
    identity: UpstreamIdentity,
    /// A git upstream's full tree digest; the evals server's identity is
    /// already one.
    upstream_digest: Option<Sha256>,
    digest: Sha256,
    bytes: u64,
    contents: Contents,
    tar: PathBuf,
}

/// `None` when `deadline` passed first. An evals-server build's blocking
/// parser has then been cancelled and joined, so nothing writes to `scratch`
/// once the caller drops it.
async fn materialize(
    upstreams: &impl Upstreams,
    scratch: &Path,
    dataset: &Dataset,
    deadline: Duration,
) -> anyhow::Result<Option<Materialized>> {
    let tar = scratch.join("corpus.tar");
    match &dataset.upstream {
        DatasetUpstream::Git { .. } => {
            match tokio::time::timeout(deadline, materialize_git(upstreams, scratch, dataset, tar))
                .await
            {
                Ok(built) => built.map(Some),
                Err(_) => Ok(None),
            }
        }
        DatasetUpstream::EvalsServer { .. } => {
            let Some(built) = evals_server::materialize(
                upstreams.http(),
                upstreams.evals_server(),
                scratch,
                dataset,
                &tar,
                deadline,
                upstreams.body_stall(),
            )
            .await?
            else {
                return Ok(None);
            };
            Ok(Some(Materialized {
                identity: UpstreamIdentity::Samples(built.identity),
                upstream_digest: None,
                digest: built.digest,
                bytes: built.bytes,
                contents: Contents::VlmEval {
                    samples: built.samples,
                    media: built.media,
                    served_bytes: built.served_bytes,
                },
                tar,
            }))
        }
    }
}

/// The manifest records the upstream as observed, so a correctly pinned entry
/// packs to its pinned digest.
async fn materialize_git(
    upstreams: &impl Upstreams,
    scratch: &Path,
    dataset: &Dataset,
    tar: PathBuf,
) -> anyhow::Result<Materialized> {
    let DatasetUpstream::Git {
        url,
        revision,
        subpath,
        license,
        ..
    } = &dataset.upstream
    else {
        anyhow::bail!("{} is not a git upstream", dataset.upstream.reference());
    };
    let checkout = scratch.join("checkout");
    let (root, identity) = git::checkout(
        &checkout,
        &upstreams.remote(url),
        revision,
        subpath.as_ref(),
    )
    .await?;
    let adapter = dataset.adapter;
    let keep = dataset.subset.clone();
    let (url, revision, subpath, license) = (
        url.clone(),
        revision.clone(),
        subpath.clone(),
        license.clone(),
    );
    let identity_for_manifest = identity.clone();
    let tar_path = tar.clone();
    let (tree_digest, packed) = tokio::task::spawn_blocking(move || {
        let tree_digest = format::tree_digest(&root)?;
        let observed = DatasetUpstream::Git {
            url,
            revision,
            subpath,
            identity: identity_for_manifest,
            digest: tree_digest.clone(),
            license,
        };
        let corpus = adapters::adapt(adapter, &root)?;
        let corpus = match &keep {
            Some(subset) => corpus.only(subset)?,
            None => corpus,
        };
        let packed = format::pack(&corpus, adapter, &observed, &tar_path)?;
        anyhow::Ok((tree_digest, packed))
    })
    .await??;
    let manifest = packed.manifest;
    Ok(Materialized {
        identity: UpstreamIdentity::GitTree(identity),
        upstream_digest: Some(tree_digest),
        digest: packed.digest,
        bytes: packed.bytes,
        contents: Contents::VlSpecDecode {
            subsets: manifest.subsets.into_keys().collect(),
            samples: manifest.samples,
            turns: manifest.turns,
        },
        tar,
    })
}

// ---------------------------------------------------------------------------
// pins
// ---------------------------------------------------------------------------

/// The hashes a catalog entry pins, and what the corpus they describe holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pins {
    pub identity: UpstreamIdentity,
    /// A git upstream's tree digest.
    pub upstream_digest: Option<Sha256>,
    pub digest: Sha256,
    pub bytes: u64,
    pub contents: Contents,
}

impl Pins {
    fn from_built(built: Materialized) -> Self {
        Self {
            identity: built.identity,
            upstream_digest: built.upstream_digest,
            digest: built.digest,
            bytes: built.bytes,
            contents: built.contents,
        }
    }
}

impl std::fmt::Display for Pins {
    /// The TOML lines to paste into the entry.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "[dataset]")?;
        writeln!(f, "digest = \"{}\"", self.digest)?;
        writeln!(f, "bytes  = {}", self.bytes)?;
        writeln!(f)?;
        writeln!(f, "[dataset.upstream]")?;
        match &self.identity {
            UpstreamIdentity::GitTree(oid) => writeln!(f, "identity = \"{oid}\"")?,
            UpstreamIdentity::Samples(digest) => writeln!(f, "identity = \"{digest}\"")?,
        }
        if let Some(tree) = &self.upstream_digest {
            writeln!(f, "digest   = \"{tree}\"")?;
        }
        writeln!(f)?;
        match &self.contents {
            Contents::VlSpecDecode {
                subsets,
                samples,
                turns,
            } => write!(
                f,
                "# {samples} samples, {turns} turns; subsets: {}",
                subsets.join(", ")
            ),
            Contents::VlmEval {
                samples,
                media,
                served_bytes,
            } => write!(
                f,
                "# {samples} samples, {media} media files; replaces a {served_bytes}-byte /samples body"
            ),
        }
    }
}

/// Only the fields that name the upstream (`url`, `revision`, `subpath`,
/// `adapter`, `subset` and `license` for git; `eval_id`, `dataset_version` and
/// `prompt_profile` for the evals server) are read from the entry: the hashes
/// it holds may still be placeholders.
pub async fn pins(
    config: &Config,
    stores: &Stores,
    benchmark_id: &BenchmarkId,
) -> anyhow::Result<Pins> {
    let catalog = crate::org_catalog::load_server_catalog(&*stores.catalog).await?;
    let dataset = catalog
        .get(benchmark_id)
        .ok_or_else(|| anyhow::anyhow!("benchmark {benchmark_id} not in the catalog"))?
        .dataset()
        .ok_or_else(|| anyhow::anyhow!("benchmark {benchmark_id} has no dataset"))?;
    let scratch = scratch_dir()?;
    let deadline = BuildDeadlines::of(config).of_format(dataset.format);
    let built = materialize(&Live::new(config)?, scratch.path(), dataset, deadline)
        .await?
        .ok_or_else(|| anyhow::anyhow!("fetch timed out after {}s", deadline.as_secs()))?;
    Ok(Pins::from_built(built))
}

#[cfg(test)]
mod tests {
    use pipette_core::config::{Config, StorageConfig};
    use pipette_plan_types::GitOid;
    use pipette_store_fs::build_local_fs_stores;

    use super::*;

    /// The catalog's `github.com/acme/corpus` is this local repository;
    /// its evals server is never reached.
    struct Local(PathBuf, reqwest::Client);

    impl Local {
        fn new(origin: PathBuf) -> Self {
            Self(origin, reqwest::Client::new())
        }
    }

    impl Upstreams for Local {
        fn remote(&self, _: &DatasetRepositoryUrl) -> String {
            format!("file://{}", self.0.display())
        }

        fn evals_server(&self) -> &str {
            "http://127.0.0.1:9"
        }

        fn http(&self) -> &reqwest::Client {
            &self.1
        }
    }

    /// [`materialize`] well inside its deadline, as `pins` fetches.
    async fn fetch(
        upstreams: &impl Upstreams,
        scratch: &Path,
        dataset: &Dataset,
    ) -> anyhow::Result<Materialized> {
        materialize(upstreams, scratch, dataset, Duration::from_secs(60))
            .await?
            .ok_or_else(|| anyhow::anyhow!("fetch timed out"))
    }

    /// A git pin's tree digest, which every git pin has.
    fn tree(pins: &Pins) -> anyhow::Result<&Sha256> {
        pins.upstream_digest
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("a git pin has a tree digest"))
    }

    /// A git corpus's subsets and counts.
    fn counts(pins: &Pins) -> anyhow::Result<(&[String], u32, u32)> {
        match &pins.contents {
            Contents::VlSpecDecode {
                subsets,
                samples,
                turns,
            } => Ok((subsets, *samples, *turns)),
            Contents::VlmEval { .. } => anyhow::bail!("not a git corpus"),
        }
    }

    /// A two-subset MMSpec tree (one GQA sample, one TextVQA sample sharing
    /// nothing) committed at `dataset/test`, served over `file://`.
    fn origin(dir: &Path) -> anyhow::Result<(PathBuf, GitOid)> {
        let origin = dir.join("origin");
        let tree = origin.join("dataset/test");
        std::fs::create_dir_all(tree.join("images"))?;
        std::fs::write(tree.join("images/a.jpg"), b"jpeg-bytes")?;
        std::fs::write(tree.join("images/b.jpg"), b"other-jpeg-bytes")?;
        std::fs::write(
            tree.join("mmspec.jsonl"),
            concat!(
                r#"{"id": "a", "image": "a.jpg", "turns": ["q"], "topic": "general vqa"}"#,
                "\n",
                r#"{"id": "b", "image": "b.jpg", "turns": ["q", "r"], "topic": "text vqa"}"#,
                "\n",
            ),
        )?;
        let run = |args: &[&str]| -> anyhow::Result<String> {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&origin)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.test"])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()?;
            anyhow::ensure!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        run(&["init", "-q"])?;
        run(&["config", "uploadpack.allowFilter", "true"])?;
        run(&["config", "uploadpack.allowAnySHA1InWant", "true"])?;
        run(&["add", "."])?;
        run(&["commit", "-q", "-m", "c"])?;
        let revision = GitOid::try_new(run(&["rev-parse", "HEAD"])?)?;
        Ok((origin, revision))
    }

    /// One catalog entry per `(subset, pins)`, over local-fs stores in `dir`.
    fn stores(dir: &Path, revision: &GitOid, entries: &[(&str, &Pins)]) -> anyhow::Result<Stores> {
        let data_dir = dir.join("data");
        std::fs::create_dir_all(data_dir.join("benchmarks"))?;
        entries.iter().try_for_each(|(subset, pins)| {
            std::fs::write(
                data_dir.join(format!(
                    "benchmarks/vl_spec_decode_local_{}_k8.toml",
                    subset.to_lowercase()
                )),
                entry(subset, pins, revision),
            )
        })?;
        let config = Config {
            storage: StorageConfig::LocalFs { data_dir },
            ..Config::default()
        };
        build_local_fs_stores(&config)
    }

    fn entry(subset: &str, pins: &Pins, revision: &GitOid) -> String {
        format!(
            r#"benchmark_type = "vl_spec_decode"
parameter_draft_length = 8
parameter_max_tokens = 2048

[dataset]
format  = "vl-spec-decode-corpus/1"
adapter = "mmspec@1"
digest  = "{}"
bytes   = {}
subset  = "{subset}"

[dataset.upstream]
source   = "git"
url      = "github.com/acme/corpus"
revision = "{revision}"
subpath  = "dataset/test"
identity = "{}"
digest   = "{}"
"#,
            pins.digest,
            pins.bytes,
            pins.identity,
            pins.upstream_digest
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default()
        )
    }

    /// Pins computed from the origin for `subset` (or the whole tree), exactly
    /// as the operator would paste them.
    async fn local_pins(
        remotes: &Local,
        dir: &Path,
        revision: &GitOid,
        subset: Option<&str>,
    ) -> anyhow::Result<Pins> {
        let subset_line = subset.map_or(String::new(), |s| format!("subset  = \"{s}\""));
        let placeholder: Dataset = toml::from_str(&format!(
            r#"
format  = "vl-spec-decode-corpus/1"
adapter = "mmspec@1"
digest  = "{0}"
bytes   = 0
{subset_line}

[upstream]
source   = "git"
url      = "github.com/acme/corpus"
revision = "{revision}"
subpath  = "dataset/test"
identity = "0000000000000000000000000000000000000000"
digest   = "{0}"
"#,
            "0".repeat(64)
        ))?;
        let scratch = dir.join(format!("pins-{}", subset.unwrap_or("all")));
        Ok(Pins::from_built(
            fetch(remotes, &scratch, &placeholder).await?,
        ))
    }

    /// A private pin must be built too: nothing else would give it a tar.
    #[tokio::test]
    async fn a_private_pin_is_built() -> anyhow::Result<()> {
        use pipette_core::types::Mint;

        let dir = tempfile::tempdir()?;
        let (origin, revision) = origin(dir.path())?;
        let remotes = Local::new(origin);
        let gqa = local_pins(&remotes, dir.path(), &revision, Some("GQA")).await?;
        let stores = stores(dir.path(), &revision, &[])?;
        std::fs::write(
            dir.path()
                .join("data/benchmarks/prefill_throughput_256.toml"),
            "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256",
        )?;
        let benchmark = pipette_core::benchmark::Benchmark::from_toml(
            "vl_spec_decode_private_gqa_k8",
            &entry("GQA", &gqa, &revision),
        )?;
        stores
            .catalog
            .put_org_benchmark(&pipette_plan_types::OrgId::mint(), &benchmark)
            .await?;

        assert_eq!(pinned_count(&stores).await?, 1);
        build_all(&remotes, Duration::from_secs(60), &stores).await?;
        assert!(stores.dataset_cache.has_tar(&gqa.digest).await?);
        Ok(())
    }

    /// A private entry that sorts first and pins a public digest differently is
    /// the one left out, so it never steers that artifact's build or probe.
    #[tokio::test]
    async fn a_public_pin_keeps_its_digest() -> anyhow::Result<()> {
        use pipette_core::types::Mint;

        let dir = tempfile::tempdir()?;
        let revision = GitOid::try_new("0".repeat(40))?;
        let stores = stores(dir.path(), &revision, &[])?;
        let pin = |url: &str| {
            format!(
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
revision = "{revision}"
subpath  = "dataset/test"
identity = "{revision}"
digest   = "{digest}"
"#,
                digest = "a".repeat(64)
            )
        };
        std::fs::write(
            dir.path()
                .join("data/benchmarks/vl_spec_decode_public_k8.toml"),
            pin("github.com/acme/corpus"),
        )?;
        let rival = pipette_core::benchmark::Benchmark::from_toml(
            "vl_spec_decode_a_k8",
            &pin("github.com/rival/corpus"),
        )?;
        stores
            .catalog
            .put_org_benchmark(&pipette_plan_types::OrgId::mint(), &rival)
            .await?;

        let pinned = pinned(&stores).await?;
        assert!(
            pinned
                .conflicts
                .iter()
                .any(|c| c.starts_with("vl_spec_decode_a_k8 ")),
            "{:?}",
            pinned.conflicts
        );
        let kept: Vec<_> = pinned
            .pins()
            .map(|pin| (pin.dataset.upstream.reference(), pin.benchmarks))
            .collect();
        assert_eq!(
            kept,
            vec![(
                format!("github.com/acme/corpus@{revision}:dataset/test"),
                vec![BenchmarkId::try_new("vl_spec_decode_public_k8")?]
            )]
        );
        Ok(())
    }

    /// The whole loop against a local origin with two entries on one upstream:
    /// build, a no-op rebuild, a probe refresh, then a gone origin fails the probe.
    #[tokio::test]
    async fn build_then_probe_tracks_the_upstream() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (origin, revision) = origin(dir.path())?;
        let remotes = Local::new(origin.clone());

        // The unfiltered pins are what an operator splits from.
        let all = local_pins(&remotes, dir.path(), &revision, None).await?;
        assert_eq!(
            counts(&all)?,
            (&["GQA".to_string(), "TextVQA".to_string()][..], 2, 3)
        );
        assert!(all.to_string().contains("subsets: GQA, TextVQA"), "{all}");
        let gqa = local_pins(&remotes, dir.path(), &revision, Some("GQA")).await?;
        let textvqa = local_pins(&remotes, dir.path(), &revision, Some("TextVQA")).await?;
        assert_eq!(counts(&gqa)?, (&["GQA".to_string()][..], 1, 1));
        assert_eq!((counts(&textvqa)?.1, counts(&textvqa)?.2), (1, 2));
        assert_ne!(gqa.digest, textvqa.digest);
        // The upstream pins are the upstream's, whatever subset is kept.
        assert_eq!(gqa.identity, all.identity);
        assert_eq!(tree(&gqa)?, tree(&textvqa)?);

        let stores = stores(
            dir.path(),
            &revision,
            &[("GQA", &gqa), ("TextVQA", &textvqa)],
        )?;
        let deadline = Duration::from_secs(60);
        assert_eq!(pinned_count(&stores).await?, 2);
        probe_all(&remotes, deadline, &stores).await?;
        assert!(
            stores
                .dataset_cache
                .get_artifact(&gqa.digest)
                .await?
                .is_none()
        );

        build_all(&remotes, deadline, &stores).await?;
        let built = stores
            .dataset_cache
            .get_artifact(&gqa.digest)
            .await?
            .ok_or_else(|| anyhow::anyhow!("the build writes the artifact"))?;
        assert_eq!(built.probe.state, ProbeState::Ok);
        assert_eq!(built.probe.observed_identity.as_ref(), Some(&gqa.identity));
        assert!(stores.dataset_cache.has_tar(&gqa.digest).await?);
        assert!(stores.dataset_cache.has_tar(&textvqa.digest).await?);
        let catalog = stores.catalog.load_catalog().await?;
        let gqa_dataset = catalog
            .values()
            .find_map(|b| b.dataset().filter(|d| d.subset.as_deref() == Some("GQA")))
            .ok_or_else(|| anyhow::anyhow!("the catalog pins GQA"))?;
        assert!(!build_one(&remotes, deadline, &stores, &gqa.digest, gqa_dataset, None).await?);

        probe_all(&remotes, deadline, &stores).await?;
        let still_ok = stores
            .dataset_cache
            .get_artifact(&gqa.digest)
            .await?
            .ok_or_else(|| anyhow::anyhow!("the probe keeps the artifact"))?;
        assert_eq!(still_ok.probe.state, ProbeState::Ok);
        assert_eq!(still_ok.probe.since, built.probe.since);
        assert!(still_ok.probe.checked_at >= built.probe.checked_at);

        // The origin is rewritten so the pinned commit is unreachable; one
        // probe now marks both artifacts that pin it.
        std::fs::remove_dir_all(origin.join(".git"))?;
        let err = probe_all(&remotes, deadline, &stores)
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("not ok"), "{err}");
        assert!(
            err.contains(gqa.digest.as_ref()) && err.contains(textvqa.digest.as_ref()),
            "{err}"
        );
        let after = stores
            .dataset_cache
            .get_artifact(&textvqa.digest)
            .await?
            .ok_or_else(|| anyhow::anyhow!("a failing probe never deletes"))?;
        assert_ne!(after.probe.state, ProbeState::Ok);
        assert!(after.probe.since > built.probe.since);
        Ok(())
    }

    /// A timed-out probe is `unavailable`, recorded, and fails the run; the
    /// artifact is left in place.
    #[tokio::test]
    async fn a_probe_past_its_deadline_is_unavailable() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (origin, revision) = origin(dir.path())?;
        let remotes = Local::new(origin);
        let pins = local_pins(&remotes, dir.path(), &revision, Some("GQA")).await?;
        let stores = stores(dir.path(), &revision, &[("GQA", &pins)])?;
        build_all(&remotes, Duration::from_secs(60), &stores).await?;

        assert!(
            probe_all(&remotes, Duration::from_millis(1), &stores)
                .await
                .is_err()
        );
        let artifact = stores
            .dataset_cache
            .get_artifact(&pins.digest)
            .await?
            .ok_or_else(|| anyhow::anyhow!("a timeout never deletes"))?;
        assert_eq!(artifact.probe.state, ProbeState::Unavailable);
        assert!(
            artifact
                .probe
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("timed out")),
            "{:?}",
            artifact.probe.detail
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // evals-server upstream
    // -----------------------------------------------------------------------

    mod evals {
        use std::collections::HashMap;
        use std::io::Read as _;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        use axum::Router;
        use axum::extract::{Path as UrlPath, Query, State};
        use axum::http::{HeaderMap, HeaderValue, StatusCode};
        use axum::response::{IntoResponse, Response};
        use axum::routing::get;
        use base64::Engine as _;
        use futures::TryStreamExt as _;

        use pipette_plan_types::{
            EvalSample, SamplesDigest, VlmEvalCorpusManifest, VlmEvalCorpusSample,
        };

        use super::*;

        /// What the mock serves, and what it was asked.
        #[derive(Default)]
        struct Served {
            samples: Vec<EvalSample>,
            /// Answer every request with this status instead.
            status: Option<StatusCode>,
            /// Send a `X-Samples-Digest` that is not the body's.
            lie_about_the_digest: bool,
            /// Send no `X-Samples-Digest`, as an evals server older than it.
            omit_the_digest: bool,
            /// Cut the body off before its closing `]}`, as the evals server
            /// does when what it streams would not hash to its header.
            cut_off: bool,
            /// Stream new ASR samples, each with its own clip, without end.
            endless: bool,
            /// Send the body's opening and then nothing, holding it open.
            stall: bool,
            samples_requests: AtomicUsize,
        }

        /// An evals server serving MMStar 2026.10.1 for the dataset prompt and
        /// for `qwen3_5`, the way the real one answers.
        struct MockEvals {
            url: String,
            served: Arc<Mutex<Served>>,
            task: tokio::task::JoinHandle<()>,
            http: reqwest::Client,
            body_stall: Duration,
        }

        impl Upstreams for MockEvals {
            fn remote(&self, _: &DatasetRepositoryUrl) -> String {
                // An evals-server corpus has no git remote; nothing asks.
                "file:///nonexistent".to_owned()
            }

            fn evals_server(&self) -> &str {
                &self.url
            }

            fn http(&self) -> &reqwest::Client {
                &self.http
            }

            fn body_stall(&self) -> Duration {
                self.body_stall
            }
        }

        /// A `/samples` body that never ends (`endless`), or stops after its
        /// opening (`stall`).
        fn unending_body(endless: bool) -> axum::body::Body {
            let open = futures::stream::once(async {
                Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"{\"samples\":["))
            });
            let samples = futures::stream::unfold(0_u64, move |i| async move {
                if !endless {
                    return std::future::pending().await;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
                let mut wav = b"RIFF\x24\x00\x00\x00WAVEfmt ".to_vec();
                wav.extend_from_slice(&i.to_le_bytes());
                wav.resize(256, 3);
                let sample = serde_json::json!({"id": format!("e{i}"), "messages": [{"role": "user", "content": [
                    {"type": "input_audio", "input_audio": {
                        "data": base64::engine::general_purpose::STANDARD.encode(wav),
                        "format": "wav",
                    }}
                ]}]});
                let text = format!("{}{sample}", if i == 0 { "" } else { "," });
                Some((Ok(axum::body::Bytes::from(text)), i + 1))
            });
            axum::body::Body::from_stream(open.chain(samples))
        }

        fn digest_of(samples: &[EvalSample]) -> anyhow::Result<Sha256> {
            let mut digest = SamplesDigest::new();
            samples.iter().try_for_each(|s| digest.update(s))?;
            Ok(digest.finish()?)
        }

        type Shared = Arc<Mutex<Served>>;

        fn lock(served: &Shared) -> std::sync::MutexGuard<'_, Served> {
            served
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        /// The samples a request names, or the status that refuses it.
        fn answer(
            served: &Shared,
            eval: &str,
            version: &str,
            query: &HashMap<String, String>,
        ) -> Result<Vec<EvalSample>, StatusCode> {
            let served = lock(served);
            if let Some(status) = served.status {
                return Err(status);
            }
            let profile = query.get("prompt_profile").map(String::as_str);
            match (eval, version, profile) {
                ("mmstar", "2026.10.1", None | Some("qwen3_5"))
                | (ASR_TASK, "2026.10.1", Some("voxtral" | "gemma4")) => Ok(served.samples.clone()),
                ("mmstar" | ASR_TASK, "2026.10.1", _) => Err(StatusCode::BAD_REQUEST),
                _ => Err(StatusCode::NOT_FOUND),
            }
        }

        async fn samples_route(
            State(served): State<Shared>,
            UrlPath((eval, version)): UrlPath<(String, String)>,
            Query(query): Query<HashMap<String, String>>,
        ) -> Response {
            lock(&served)
                .samples_requests
                .fetch_add(1, Ordering::SeqCst);
            match answer(&served, &eval, &version, &query) {
                Err(status) => status.into_response(),
                Ok(samples) => {
                    let digest = match digest_of(&samples) {
                        Ok(_) if lock(&served).lie_about_the_digest => {
                            format!("sha256:{}", "f".repeat(64))
                        }
                        Ok(d) => format!("sha256:{d}"),
                        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                    };
                    let mut headers = HeaderMap::new();
                    if let Ok(value) = HeaderValue::from_str(&digest)
                        && !lock(&served).omit_the_digest
                    {
                        headers.insert("x-samples-digest", value);
                    }
                    let (endless, stall) = {
                        let served = lock(&served);
                        (served.endless, served.stall)
                    };
                    if endless || stall {
                        return (headers, unending_body(endless)).into_response();
                    }
                    let Ok(mut body) =
                        serde_json::to_vec(&serde_json::json!({ "samples": samples }))
                    else {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    };
                    if lock(&served).cut_off {
                        body.truncate(body.len().saturating_sub(2));
                    }
                    (headers, body).into_response()
                }
            }
        }

        async fn digest_route(
            State(served): State<Shared>,
            UrlPath((eval, version)): UrlPath<(String, String)>,
            Query(query): Query<HashMap<String, String>>,
        ) -> Response {
            match answer(&served, &eval, &version, &query).map(|s| (digest_of(&s), s.len())) {
                Err(status) => status.into_response(),
                Ok((Ok(digest), count)) => axum::Json(serde_json::json!({
                    "digest": format!("sha256:{digest}"),
                    "sample_count": count,
                }))
                .into_response(),
                Ok((Err(_), _)) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }

        async fn mock_evals(samples: Vec<EvalSample>) -> anyhow::Result<MockEvals> {
            let served = Arc::new(Mutex::new(Served {
                samples,
                ..Served::default()
            }));
            let app = Router::new()
                .route(
                    "/evals/{eval}/datasets/{version}/samples",
                    get(samples_route),
                )
                .route(
                    "/evals/{eval}/datasets/{version}/samples/digest",
                    get(digest_route),
                )
                .with_state(served.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let url = format!("http://{}", listener.local_addr()?);
            let task = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Ok(MockEvals {
                url,
                served,
                task,
                http: reqwest::Client::builder().no_proxy().build()?,
                body_stall: BODY_STALL,
            })
        }

        fn data_uri(mime: &str, bytes: &[u8]) -> String {
            format!(
                "data:{mime};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            )
        }

        /// Four questions: two share one picture, one is text only, one has a
        /// system prompt and a JPEG.
        fn samples() -> anyhow::Result<Vec<EvalSample>> {
            let shared = data_uri("image/png", &[7_u8; 3000]);
            let other = data_uri("image/jpeg", &[9_u8; 1200]);
            Ok(serde_json::from_value(serde_json::json!([
                {"id": "a", "messages": [{"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": shared}},
                    {"type": "text", "text": "Which mood?\nA. Sad\nB. Happy"}
                ]}]},
                {"id": "b", "messages": [{"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": shared}},
                    {"type": "text", "text": "How many? 心情"}
                ]}]},
                {"id": "c", "messages": [{"role": "user", "content": "No picture?"}]},
                {"id": "d", "messages": [
                    {"role": "system", "content": "Think first."},
                    {"role": "user", "content": [
                        {"type": "text", "text": "Before:"},
                        {"type": "image_url", "image_url": {"url": other}},
                        {"type": "text", "text": "Answer."}
                    ]}
                ]}
            ]))?)
        }

        /// The asr_eval task the mock serves, under the `voxtral` and `gemma4`
        /// profiles.
        const ASR_TASK: &str = "asr_fleurs_nl_nl";

        /// A WAV-shaped clip of `fill` bytes: what the build moves out is the
        /// bytes, whatever they hold.
        fn clip(fill: u8, len: usize) -> String {
            let mut wav = b"RIFF\x24\x00\x00\x00WAVEfmt ".to_vec();
            wav.resize(len, fill);
            base64::engine::general_purpose::STANDARD.encode(wav)
        }

        /// Three clips the way the evals server serves `voxtral`: two samples
        /// share a clip, each carries its template arguments, one has a
        /// system turn and the prompt before its clip.
        fn asr_samples() -> anyhow::Result<Vec<EvalSample>> {
            let shared = clip(1, 4000);
            let other = clip(2, 2500);
            Ok(serde_json::from_value(serde_json::json!([
                {"id": "a1", "messages": [{"role": "user", "content": [
                    {"type": "input_audio", "input_audio": {"data": shared, "format": "wav"}}
                ]}], "chat_template_kwargs": {"language": "nl", "transcription": true}},
                {"id": "b2", "messages": [{"role": "user", "content": [
                    {"type": "input_audio", "input_audio": {"data": shared, "format": "wav"}}
                ]}], "chat_template_kwargs": {"language": "nl", "transcription": true}},
                {"id": "c3", "messages": [
                    {"role": "system", "content": "Perform ASR."},
                    {"role": "user", "content": [
                        {"type": "text", "text": "Transcribe."},
                        {"type": "input_audio", "input_audio": {"data": other, "format": "wav"}}
                    ]}
                ]}
            ]))?)
        }

        fn asr_placeholder(profile: &str) -> anyhow::Result<Dataset> {
            Ok(serde_json::from_value(serde_json::json!({
                "format": "asr-eval-corpus/1",
                "adapter": "evals-server@1",
                "digest": "0".repeat(64),
                "bytes": 0,
                "upstream": {
                    "source": "evals_server",
                    "eval_id": ASR_TASK,
                    "dataset_version": "2026.10.1",
                    "prompt_profile": profile,
                    "identity": "0".repeat(64),
                },
            }))?)
        }

        async fn asr_pins(evals: &MockEvals, profile: &str) -> anyhow::Result<Pins> {
            let scratch = tempfile::tempdir()?;
            Ok(Pins::from_built(
                fetch(evals, scratch.path(), &asr_placeholder(profile)?).await?,
            ))
        }

        /// A local-fs catalog holding one asr_eval entry, private to an
        /// organization as every asr_eval entry is, that pins `pins`.
        async fn asr_stores(dir: &Path, pins: &Pins) -> anyhow::Result<Stores> {
            asr_stores_stating(dir, pins, 3).await
        }

        /// [`asr_stores`], its entry stating `samples` samples.
        async fn asr_stores_stating(
            dir: &Path,
            pins: &Pins,
            samples: u32,
        ) -> anyhow::Result<Stores> {
            use pipette_core::types::Mint;

            let stores = public_only_stores(dir)?;
            put_asr_entry(&stores, &pipette_plan_types::OrgId::mint(), pins, samples).await?;
            Ok(stores)
        }

        /// Local-fs stores whose catalog holds one public entry, as a
        /// deployment's is never empty, and no organization's.
        fn public_only_stores(dir: &Path) -> anyhow::Result<Stores> {
            let data_dir = dir.join("data");
            std::fs::create_dir_all(data_dir.join("benchmarks"))?;
            std::fs::write(
                data_dir.join("benchmarks/prefill_throughput_256.toml"),
                "benchmark_type = \"prefill_throughput\"\nparameter_prefill_tokens = 256",
            )?;
            build_local_fs_stores(&Config {
                storage: StorageConfig::LocalFs { data_dir },
                ..Config::default()
            })
        }

        /// Put the asr_eval entry pinning `pins` and stating `samples`
        /// samples into `org`'s catalog, replacing any earlier one.
        async fn put_asr_entry(
            stores: &Stores,
            org: &pipette_plan_types::OrgId,
            pins: &Pins,
            samples: u32,
        ) -> anyhow::Result<()> {
            let UpstreamIdentity::Samples(identity) = &pins.identity else {
                anyhow::bail!("an evals-server pin is a samples digest");
            };
            let toml = format!(
                r#"benchmark_type = "asr_eval"
parameter_asr_task = "{ASR_TASK}"
parameter_dataset_version = "2026.10.1"
parameter_sample_count = {samples}
parameter_prompt_profile = "voxtral"
parameter_max_tokens = 500

[dataset]
format  = "asr-eval-corpus/1"
adapter = "evals-server@1"
digest  = "{}"
bytes   = {}

[dataset.upstream]
source          = "evals_server"
eval_id         = "{ASR_TASK}"
dataset_version = "2026.10.1"
prompt_profile  = "voxtral"
identity        = "{identity}"

[recipe]
family  = "Test family"
models  = [{{ repo = "org/repo" }}]
enable_thinking = false
sources = ["https://huggingface.co/org/repo"]
"#,
                pins.digest, pins.bytes
            );
            let benchmark = pipette_core::benchmark::Benchmark::from_toml(
                "asr_eval_fleurs_nl_nl_voxtral",
                &toml,
            )?;
            stores.catalog.put_org_benchmark(org, &benchmark).await?;
            Ok(())
        }

        /// An asr-eval corpus end to end: pinned, built from the streamed body
        /// without storing it, every clip once as a `.wav`, the template
        /// arguments kept, and the stored tar rebuilding exactly the served
        /// samples; then probed by digest alone. The entry is private to an
        /// organization, as every asr_eval entry is.
        #[tokio::test]
        async fn an_asr_eval_corpus_is_lossless_and_tracks_its_samples() -> anyhow::Result<()> {
            let served = asr_samples()?;
            let evals = mock_evals(served.clone()).await?;
            let pins = asr_pins(&evals, "voxtral").await?;
            assert_eq!(
                pins.identity,
                UpstreamIdentity::Samples(digest_of(&served)?)
            );
            let Contents::VlmEval {
                samples: count,
                media,
                served_bytes,
            } = pins.contents.clone()
            else {
                anyhow::bail!("an evals-server corpus");
            };
            assert_eq!((count, media), (3, 2), "the shared clip is one file");
            // Counted as it streamed past, though never stored.
            let body = serde_json::to_vec(&serde_json::json!({ "samples": served }))?;
            assert_eq!(served_bytes, body.len() as u64);
            assert_eq!(pins, asr_pins(&evals, "voxtral").await?, "deterministic");
            // Another profile's samples are another corpus, even when they hash
            // alike: the manifest names the profile.
            assert_ne!(pins.digest, asr_pins(&evals, "gemma4").await?.digest);

            let dir = tempfile::tempdir()?;
            let stores = asr_stores(dir.path(), &pins).await?;
            let deadline = Duration::from_secs(60);
            build_all(&evals, deadline, &stores).await?;
            let built = artifact(&stores, &pins.digest).await?;
            assert_eq!(built.probe.state, ProbeState::Ok);
            assert_eq!(built.format, CorpusFormat::AsrEvalCorpusV1);

            let entries = stored_tar(&stores, &pins.digest).await?;
            let manifest: VlmEvalCorpusManifest =
                serde_json::from_slice(&entries["manifest.json"])?;
            assert_eq!(manifest.format, CorpusFormat::AsrEvalCorpusV1);
            assert_eq!((manifest.samples, manifest.media), (3, 2));
            assert!(
                entries
                    .keys()
                    .filter(|name| name.starts_with("media/"))
                    .all(|name| name.rsplit_once('.').is_some_and(|(_, ext)| ext == "wav")),
                "{:?}",
                entries.keys()
            );
            let lines: Vec<VlmEvalCorpusSample> = entries["samples.jsonl"]
                .split(|&b| b == b'\n')
                .filter(|line| !line.is_empty())
                .map(serde_json::from_slice)
                .collect::<Result<_, _>>()?;
            let rebuilt = lines
                .iter()
                .map(|corpus| {
                    corpus.to_eval_sample_in(CorpusFormat::AsrEvalCorpusV1, |path| {
                        entries
                            .get(path)
                            .cloned()
                            .ok_or_else(|| std::io::Error::other("missing media"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(rebuilt, served);
            // Read as a vlm-eval corpus, the same lines are refused.
            assert!(
                lines[0]
                    .to_eval_sample(|path| Ok(entries.get(path).cloned().unwrap_or_default()))
                    .is_err()
            );

            let downloads = lock(&evals.served).samples_requests.load(Ordering::SeqCst);
            probe_all(&evals, deadline, &stores).await?;
            assert_eq!(
                artifact(&stores, &pins.digest).await?.probe.state,
                ProbeState::Ok
            );
            assert_eq!(
                lock(&evals.served).samples_requests.load(Ordering::SeqCst),
                downloads,
                "a probe never downloads the samples"
            );
            Ok(())
        }

        /// Nothing is stored that would not rebuild the scorer's samples in
        /// the entry's own format: an image in an ASR set, or a body the
        /// evals server cut off.
        #[rstest::rstest]
        #[case::image_in_an_asr_corpus(
            "image",
            "an image part has no place in the asr-eval-corpus/1 format"
        )]
        #[case::cut_off("cut", "not the evals server's JSON")]
        #[case::none("", "")]
        #[tokio::test]
        async fn an_asr_build_refuses_what_it_cannot_prove(
            #[case] fault: &str,
            #[case] names: &str,
        ) -> anyhow::Result<()> {
            let mut served = asr_samples()?;
            let evals = mock_evals(served.clone()).await?;
            let pins = asr_pins(&evals, "voxtral").await?;
            match fault {
                "image" => {
                    served[2] = serde_json::from_value(serde_json::json!(
                        {"id": "c3", "messages": [{"role": "user", "content": [
                            {"type": "image_url", "image_url": {"url": data_uri("image/png", &[1_u8; 10])}}
                        ]}]}
                    ))?;
                    lock(&evals.served).samples = served;
                }
                "cut" => lock(&evals.served).cut_off = true,
                _ => {}
            }
            let dir = tempfile::tempdir()?;
            let stores = asr_stores(dir.path(), &pins).await?;
            let built = build_all(&evals, Duration::from_secs(60), &stores).await;
            if fault.is_empty() {
                built?;
                assert!(stores.dataset_cache.has_tar(&pins.digest).await?);
                return Ok(());
            }
            assert!(built.is_err());
            let scratch = tempfile::tempdir()?;
            let error = fetch(&evals, scratch.path(), &asr_placeholder("voxtral")?)
                .await
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_default();
            assert!(error.contains(names), "{error}");
            assert!(!stores.dataset_cache.has_tar(&pins.digest).await?);
            Ok(())
        }

        /// An asr-eval corpus builds under its own, longer deadline; the
        /// other formats keep theirs.
        #[test]
        fn an_asr_build_has_its_own_deadline() {
            let config = Config::default();
            let deadlines = BuildDeadlines::of(&config);
            assert_eq!(
                deadlines.of_format(CorpusFormat::AsrEvalCorpusV1),
                Duration::from_secs(7200)
            );
            for format in [
                CorpusFormat::VlmEvalCorpusV1,
                CorpusFormat::VlSpecDecodeCorpusV1,
            ] {
                assert_eq!(deadlines.of_format(format), Duration::from_secs(900));
            }
        }

        /// A build past its deadline stops before it returns: its blocking
        /// parser is cancelled and joined, so nothing writes to the scratch its
        /// caller then drops, and the next build does not share the disk.
        #[tokio::test]
        async fn a_timed_out_build_stops_writing_before_it_returns() -> anyhow::Result<()> {
            let evals = mock_evals(asr_samples()?).await?;
            lock(&evals.served).endless = true;
            let scratch = tempfile::tempdir()?;
            let built = materialize(
                &evals,
                scratch.path(),
                &asr_placeholder("voxtral")?,
                Duration::from_millis(500),
            )
            .await?;
            assert!(built.is_none(), "an endless body times out");
            let media = || {
                std::fs::read_dir(scratch.path().join("media"))
                    .map_or(0, std::iter::Iterator::count)
            };
            let written = media();
            assert!(written > 0, "the build was writing clips when it timed out");
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                media(),
                written,
                "a clip was written after the build returned"
            );
            Ok(())
        }

        /// A body that stops arriving fails its build well before the deadline.
        #[tokio::test]
        async fn a_stalled_body_fails_its_build() -> anyhow::Result<()> {
            let mut evals = mock_evals(asr_samples()?).await?;
            evals.body_stall = Duration::from_millis(200);
            lock(&evals.served).stall = true;
            let scratch = tempfile::tempdir()?;
            let error = tokio::time::timeout(
                Duration::from_secs(20),
                fetch(&evals, scratch.path(), &asr_placeholder("voxtral")?),
            )
            .await?
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
            assert!(error.contains("stalled: no byte for"), "{error}");
            Ok(())
        }

        /// An asr_eval entry whose `parameter_sample_count` is not its
        /// corpus's fails its build; a matching one builds. Either way the
        /// corpus is stored with the count it holds, so a second run of a
        /// disagreeing entry fails from the record without streaming the set
        /// again.
        #[rstest::rstest]
        #[case::matching(3, None)]
        #[case::more(
            4,
            Some("the corpus holds 3 samples, its entry states parameter_sample_count = 4")
        )]
        #[case::fewer(2, Some("holds 3 samples"))]
        #[tokio::test]
        async fn an_asr_corpus_must_hold_its_entry_count(
            #[case] stated: u32,
            #[case] refused: Option<&str>,
        ) -> anyhow::Result<()> {
            let evals = mock_evals(asr_samples()?).await?;
            let pins = asr_pins(&evals, "voxtral").await?;
            let dir = tempfile::tempdir()?;
            let stores = asr_stores_stating(dir.path(), &pins, stated).await?;
            let pin = pinned(&stores)
                .await?
                .pins()
                .next()
                .ok_or_else(|| anyhow::anyhow!("the entry's pin"))?;
            assert_eq!(pin.samples, Some(stated));
            let built = build_one(
                &evals,
                Duration::from_secs(60),
                &stores,
                &pin.digest,
                &pin.dataset,
                pin.samples,
            )
            .await;
            match (built, refused) {
                (Ok(true), None) => {}
                (Err(e), Some(names)) => {
                    assert!(format!("{e:#}").contains(names), "{e:#}");
                    let downloads = lock(&evals.served).samples_requests.load(Ordering::SeqCst);
                    let again = build_one(
                        &evals,
                        Duration::from_secs(60),
                        &stores,
                        &pin.digest,
                        &pin.dataset,
                        pin.samples,
                    )
                    .await
                    .err()
                    .map(|e| format!("{e:#}"));
                    assert!(
                        again.as_deref().is_some_and(|e| e.contains(names)),
                        "{again:?}"
                    );
                    assert_eq!(
                        lock(&evals.served).samples_requests.load(Ordering::SeqCst),
                        downloads,
                        "the second run reads the record, not the upstream"
                    );
                }
                (got, want) => anyhow::bail!("got {got:?}, want refusal {want:?}"),
            }
            assert!(stores.dataset_cache.has_tar(&pins.digest).await?);
            assert_eq!(artifact(&stores, &pins.digest).await?.samples, Some(3));
            Ok(())
        }

        /// A corpus already built for an entry stating 3 samples is refused,
        /// not skipped, once that entry is replaced by one stating 4, and is
        /// served from the cache again under an entry stating 3. A record
        /// without a count, which no asr-eval corpus build writes, is refused.
        #[tokio::test]
        async fn a_cached_asr_corpus_must_hold_its_entry_count() -> anyhow::Result<()> {
            use pipette_core::types::Mint;

            let evals = mock_evals(asr_samples()?).await?;
            let pins = asr_pins(&evals, "voxtral").await?;
            let dir = tempfile::tempdir()?;
            let stores = public_only_stores(dir.path())?;
            let org = pipette_plan_types::OrgId::mint();
            let build = |stated: u32| {
                let (evals, stores, pins, org) = (&evals, &stores, &pins, &org);
                async move {
                    put_asr_entry(stores, org, pins, stated).await?;
                    let pin = pinned(stores)
                        .await?
                        .pins()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("the entry's pin"))?;
                    anyhow::ensure!(pin.samples == Some(stated));
                    build_one(
                        evals,
                        Duration::from_secs(60),
                        stores,
                        &pin.digest,
                        &pin.dataset,
                        pin.samples,
                    )
                    .await
                }
            };
            let refused = "the corpus holds 3 samples, its entry states parameter_sample_count = 4";

            assert!(build(3).await?, "the first build writes the corpus");
            assert_eq!(artifact(&stores, &pins.digest).await?.samples, Some(3));
            let error = build(4).await.err().map(|e| format!("{e:#}"));
            assert!(
                error.as_deref().is_some_and(|e| e.contains(refused)),
                "{error:?}"
            );
            // Negative control: the same cached corpus under a matching entry.
            assert!(!build(3).await?, "served from the cache, not rebuilt");

            let countless = DatasetArtifact {
                samples: None,
                ..artifact(&stores, &pins.digest).await?
            };
            stores.dataset_cache.put_artifact(&countless).await?;
            let error = build(3).await.err().map(|e| format!("{e:#}"));
            assert!(
                error
                    .as_deref()
                    .is_some_and(|e| e.contains("recorded without a sample count")),
                "{error:?}"
            );
            Ok(())
        }

        /// The build lock's lease counts an asr-eval corpus only while it is
        /// unbuilt; with nothing else pinned it falls back to one artifact's
        /// deadline, as the lease always has.
        #[tokio::test]
        async fn the_build_lease_counts_only_unbuilt_asr_corpora() -> anyhow::Result<()> {
            let evals = mock_evals(asr_samples()?).await?;
            let pins = asr_pins(&evals, "voxtral").await?;
            let dir = tempfile::tempdir()?;
            let stores = asr_stores(dir.path(), &pins).await?;
            let config = Config::default();
            assert_eq!(
                build_run_deadline(&config, &stores).await?,
                Duration::from_secs(7200)
            );
            build_all(&evals, Duration::from_secs(60), &stores).await?;
            assert_eq!(
                build_run_deadline(&config, &stores).await?,
                Duration::from_secs(900)
            );
            Ok(())
        }

        /// The upstream an operator writes before `pins` fills its hashes in.
        fn placeholder(profile: Option<&str>) -> anyhow::Result<Dataset> {
            Ok(serde_json::from_value(serde_json::json!({
                "format": "vlm-eval-corpus/1",
                "adapter": "evals-server@1",
                "digest": "0".repeat(64),
                "bytes": 0,
                "upstream": {
                    "source": "evals_server",
                    "eval_id": "mmstar",
                    "dataset_version": "2026.10.1",
                    "prompt_profile": profile,
                    "identity": "0".repeat(64),
                },
            }))?)
        }

        async fn evals_pins(evals: &MockEvals, profile: Option<&str>) -> anyhow::Result<Pins> {
            let scratch = tempfile::tempdir()?;
            Ok(Pins::from_built(
                fetch(evals, scratch.path(), &placeholder(profile)?).await?,
            ))
        }

        /// A local-fs catalog holding one MMStar entry that pins `pins`.
        fn vlm_stores(dir: &Path, pins: &Pins) -> anyhow::Result<Stores> {
            vlm_stores_of(dir, &[("vlm_eval_mmstar", pins)])
        }

        /// A local-fs catalog of MMStar entries, one per `(benchmark id, pins)`.
        fn vlm_stores_of(dir: &Path, entries: &[(&str, &Pins)]) -> anyhow::Result<Stores> {
            let data_dir = dir.join("data");
            std::fs::create_dir_all(data_dir.join("benchmarks"))?;
            for (benchmark_id, pins) in entries {
                let UpstreamIdentity::Samples(identity) = &pins.identity else {
                    anyhow::bail!("an evals-server pin is a samples digest");
                };
                std::fs::write(
                    data_dir.join(format!("benchmarks/{benchmark_id}.toml")),
                    format!(
                        r#"benchmark_type = "vlm_eval"
parameter_lmms_task = "mmstar"
parameter_dataset_version = "2026.10.1"
parameter_sample_count = 4
parameter_max_tokens = 64

[dataset]
format  = "vlm-eval-corpus/1"
adapter = "evals-server@1"
digest  = "{}"
bytes   = {}

[dataset.upstream]
source          = "evals_server"
eval_id         = "mmstar"
dataset_version = "2026.10.1"
identity        = "{identity}"

[recipe]
family  = "Test family"
models  = [{{ repo = "org/repo" }}]
sources = ["https://huggingface.co/org/repo"]
"#,
                        pins.digest, pins.bytes
                    ),
                )?;
            }
            build_local_fs_stores(&Config {
                storage: StorageConfig::LocalFs { data_dir },
                ..Config::default()
            })
        }

        /// The stored tar's entries by name.
        async fn stored_tar(
            stores: &Stores,
            digest: &Sha256,
        ) -> anyhow::Result<HashMap<String, Vec<u8>>> {
            let body = stores
                .dataset_cache
                .open_tar(digest)
                .await?
                .ok_or_else(|| anyhow::anyhow!("the build stores the tar"))?;
            let bytes: Vec<u8> = body
                .stream
                .try_fold(Vec::new(), |mut all, chunk| async move {
                    all.extend_from_slice(&chunk);
                    Ok(all)
                })
                .await?;
            let mut entries = HashMap::new();
            let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
            for entry in archive.entries()? {
                let mut entry = entry?;
                let name = entry.path()?.to_string_lossy().into_owned();
                let mut body = Vec::new();
                entry.read_to_end(&mut body)?;
                entries.insert(name, body);
            }
            Ok(entries)
        }

        async fn artifact(stores: &Stores, digest: &Sha256) -> anyhow::Result<DatasetArtifact> {
            stores
                .dataset_cache
                .get_artifact(digest)
                .await?
                .ok_or_else(|| anyhow::anyhow!("nothing ever deletes the artifact"))
        }

        /// The whole loop against a local evals server: pin, build, read the
        /// stored corpus back into exactly the served samples, probe without
        /// downloading, then a reworded prompt, a withdrawn dataset and a dead
        /// server each stop the serving.
        #[tokio::test]
        async fn an_evals_server_corpus_is_lossless_and_tracks_its_samples() -> anyhow::Result<()> {
            let served = samples()?;
            let evals = mock_evals(served.clone()).await?;
            let pins = evals_pins(&evals, None).await?;
            assert_eq!(
                pins.identity,
                UpstreamIdentity::Samples(digest_of(&served)?)
            );
            let Contents::VlmEval {
                samples: count,
                media,
                served_bytes,
            } = pins.contents.clone()
            else {
                anyhow::bail!("an evals-server corpus");
            };
            assert_eq!((count, media), (4, 2), "the shared PNG is one file");
            assert!(pins.bytes < served_bytes + 10_240, "{pins}");
            assert!(
                pins.to_string().contains("4 samples, 2 media files"),
                "{pins}"
            );
            assert_eq!(
                pins,
                evals_pins(&evals, None).await?,
                "packing is deterministic"
            );

            let dir = tempfile::tempdir()?;
            let stores = vlm_stores(dir.path(), &pins)?;
            let deadline = Duration::from_secs(60);
            build_all(&evals, deadline, &stores).await?;
            let built = artifact(&stores, &pins.digest).await?;
            assert_eq!(built.probe.state, ProbeState::Ok);
            assert_eq!(built.probe.observed_identity.as_ref(), Some(&pins.identity));

            // What a client gets back is the served samples, byte for byte.
            let entries = stored_tar(&stores, &pins.digest).await?;
            let manifest: VlmEvalCorpusManifest =
                serde_json::from_slice(&entries["manifest.json"])?;
            assert_eq!((manifest.samples, manifest.media), (4, 2));
            assert_eq!(UpstreamIdentity::pinned(&manifest.upstream), pins.identity);
            let rebuilt = entries["samples.jsonl"]
                .split(|&b| b == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| {
                    let corpus: VlmEvalCorpusSample = serde_json::from_slice(line)?;
                    Ok(corpus.to_eval_sample(|path| {
                        entries
                            .get(path)
                            .cloned()
                            .ok_or_else(|| std::io::Error::other("missing media"))
                    })?)
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            assert_eq!(rebuilt, served);
            let catalog = stores.catalog.load_catalog().await?;
            let pinned = catalog
                .values()
                .find_map(|b| b.dataset())
                .ok_or_else(|| anyhow::anyhow!("the entry pins a dataset"))?;
            assert!(
                !build_one(&evals, deadline, &stores, &pins.digest, pinned, None).await?,
                "a built artifact is never rebuilt"
            );

            // The probe reads the digest alone.
            let downloads = lock(&evals.served).samples_requests.load(Ordering::SeqCst);
            probe_all(&evals, deadline, &stores).await?;
            assert_eq!(
                artifact(&stores, &pins.digest).await?.probe.state,
                ProbeState::Ok
            );
            assert_eq!(
                lock(&evals.served).samples_requests.load(Ordering::SeqCst),
                downloads,
                "a probe never downloads the samples"
            );

            // A reworded prompt is different samples under the same pin.
            let mut reworded = served.clone();
            reworded[2] = serde_json::from_value(serde_json::json!(
                {"id": "c", "messages": [{"role": "user", "content": "No picture at all?"}]}
            ))?;
            lock(&evals.served).samples = reworded.clone();
            assert!(probe_all(&evals, deadline, &stores).await.is_err());
            let moved = artifact(&stores, &pins.digest).await?;
            assert_eq!(moved.probe.state, ProbeState::Moved);
            assert_eq!(
                moved.probe.observed_identity,
                Some(UpstreamIdentity::Samples(digest_of(&reworded)?))
            );

            lock(&evals.served).status = Some(StatusCode::NOT_FOUND);
            assert!(probe_all(&evals, deadline, &stores).await.is_err());
            assert_eq!(
                artifact(&stores, &pins.digest).await?.probe.state,
                ProbeState::Gone
            );

            evals.task.abort();
            while !evals.task.is_finished() {
                tokio::task::yield_now().await;
            }
            assert!(probe_all(&evals, deadline, &stores).await.is_err());
            assert_eq!(
                artifact(&stores, &pins.digest).await?.probe.state,
                ProbeState::Unavailable
            );
            assert!(
                stores.dataset_cache.has_tar(&pins.digest).await?,
                "a probe never deletes"
            );
            Ok(())
        }

        /// Two entries pin the same samples at different identities, as a stale
        /// entry beside a current one would: one probe of the evals server, and
        /// each artifact judged against its own pin.
        #[tokio::test]
        async fn each_artifact_is_judged_against_its_own_pin() -> anyhow::Result<()> {
            let before = samples()?;
            let evals = mock_evals(before.clone()).await?;
            let old = evals_pins(&evals, None).await?;
            let mut after = before;
            after[2] = serde_json::from_value(serde_json::json!(
                {"id": "c", "messages": [{"role": "user", "content": "No picture at all?"}]}
            ))?;
            lock(&evals.served).samples = after;
            let new = evals_pins(&evals, None).await?;
            assert_ne!(old.identity, new.identity);

            let dir = tempfile::tempdir()?;
            let stores = vlm_stores_of(
                dir.path(),
                &[("vlm_eval_mmstar_old", &old), ("vlm_eval_mmstar_new", &new)],
            )?;
            let deadline = Duration::from_secs(60);
            // The old pin cannot be built from what is served now; plant the
            // artifact it was built into, as an earlier build would have.
            let scratch = tempfile::tempdir()?;
            let tar = scratch.path().join("old.tar");
            std::fs::write(&tar, b"an earlier build")?;
            stores.dataset_cache.put_tar(&old.digest, &tar).await?;
            let catalog = stores.catalog.load_catalog().await?;
            let old_dataset = catalog
                .values()
                .filter_map(|b| b.dataset())
                .find(|d| d.digest == old.digest)
                .ok_or_else(|| anyhow::anyhow!("the old entry pins a dataset"))?;
            let now = Utc::now();
            stores
                .dataset_cache
                .put_artifact(&DatasetArtifact {
                    digest: old.digest.clone(),
                    bytes: old.bytes,
                    format: old_dataset.format,
                    adapter: old_dataset.adapter,
                    upstream: old_dataset.upstream.clone(),
                    subset: None,
                    samples: None,
                    built_at: now,
                    probe: ProbeVerdict {
                        state: ProbeState::Ok,
                        checked_at: now,
                        since: now,
                        observed_identity: Some(old.identity.clone()),
                        detail: None,
                    },
                })
                .await?;
            assert!(
                build_all(&evals, deadline, &stores).await.is_ok(),
                "the new pin builds; the old one is already built"
            );

            let error = probe_all(&evals, deadline, &stores)
                .await
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(error.contains(old.digest.as_ref()), "{error}");
            assert!(!error.contains(new.digest.as_ref()), "{error}");
            assert_eq!(
                artifact(&stores, &old.digest).await?.probe.state,
                ProbeState::Moved
            );
            assert_eq!(
                artifact(&stores, &new.digest).await?.probe.state,
                ProbeState::Ok
            );
            Ok(())
        }

        /// A profile is part of the pin: its samples are their own corpus.
        #[tokio::test]
        async fn a_prompt_profile_is_its_own_pin() -> anyhow::Result<()> {
            let evals = mock_evals(samples()?).await?;
            let qwen = evals_pins(&evals, Some("qwen3_5")).await?;
            let unknown = evals_pins(&evals, Some("nope"))
                .await
                .err()
                .map(|e| format!("{e:#}"));
            assert!(
                unknown.as_deref().is_some_and(|e| e.contains("400")),
                "{unknown:?}"
            );
            // The same samples under another profile hash alike but pack apart:
            // the manifest names the profile.
            assert_eq!(qwen.identity, evals_pins(&evals, None).await?.identity);
            assert_ne!(qwen.digest, evals_pins(&evals, None).await?.digest);
            Ok(())
        }

        /// Nothing is stored that the build could not prove the scorer's.
        #[rstest::rstest]
        #[case::header_disagrees("lie", "reports digest")]
        #[case::no_header("omit", "sent no x-samples-digest")]
        #[case::pin_disagrees("pin", "upstream moved")]
        #[case::image_would_not_rebuild("image", "not a base64 `data:image")]
        #[tokio::test]
        async fn a_build_refuses_what_it_cannot_prove(
            #[case] fault: &str,
            #[case] names: &str,
        ) -> anyhow::Result<()> {
            let mut served = samples()?;
            let evals = mock_evals(served.clone()).await?;
            let mut pins = evals_pins(&evals, None).await?;
            match fault {
                "lie" => lock(&evals.served).lie_about_the_digest = true,
                "omit" => lock(&evals.served).omit_the_digest = true,
                "pin" => {
                    pins.identity = UpstreamIdentity::Samples(Sha256::try_new("1".repeat(64))?)
                }
                _ => {
                    served[0] = serde_json::from_value(serde_json::json!(
                        {"id": "a", "messages": [{"role": "user", "content": [
                            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo"}}
                        ]}]}
                    ))?;
                    lock(&evals.served).samples = served;
                }
            }
            let dir = tempfile::tempdir()?;
            let stores = vlm_stores(dir.path(), &pins)?;
            let error = build_one(
                &evals,
                Duration::from_secs(60),
                &stores,
                &pins.digest,
                stores
                    .catalog
                    .load_catalog()
                    .await?
                    .values()
                    .find_map(|b| b.dataset().cloned())
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("the entry pins a dataset"))?,
                None,
            )
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
            assert!(error.contains(names), "{error}");
            assert!(!stores.dataset_cache.has_tar(&pins.digest).await?);
            assert!(
                stores
                    .dataset_cache
                    .get_artifact(&pins.digest)
                    .await?
                    .is_none()
            );
            Ok(())
        }

        /// What the corpus is for: a picture forty questions share is stored
        /// once, where `/samples` repeats it forty times.
        #[tokio::test]
        async fn a_shared_picture_is_stored_once() -> anyhow::Result<()> {
            let picture = data_uri("image/png", &vec![3_u8; 50_000]);
            let shared: Vec<EvalSample> = (0..40)
                .map(|i| {
                    serde_json::from_value(serde_json::json!(
                        {"id": format!("q{i}"), "messages": [{"role": "user", "content": [
                            {"type": "image_url", "image_url": {"url": picture}},
                            {"type": "text", "text": format!("Question {i}?")}
                        ]}]}
                    ))
                })
                .collect::<Result<_, _>>()?;
            let evals = mock_evals(shared).await?;
            let pins = evals_pins(&evals, None).await?;
            let Contents::VlmEval {
                media,
                served_bytes,
                ..
            } = pins.contents
            else {
                anyhow::bail!("an evals-server corpus");
            };
            assert_eq!(media, 1);
            assert!(
                pins.bytes * 20 < served_bytes,
                "{} byte corpus for a {served_bytes} byte body",
                pins.bytes
            );
            Ok(())
        }
    }
}
