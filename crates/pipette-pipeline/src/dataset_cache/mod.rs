//! `pipette-mgmt dataset-cache`: probe records an upstream verdict on each
//! artifact (all the serving path reads), build creates missing git artifacts
//! and registers the scores service's packages, and pins prints a git entry's
//! hashes. Nothing here deletes an artifact.

mod adapters;
mod evals_server;
mod format;
mod git;

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use futures::{StreamExt, TryStreamExt as _};

use pipette_core::config::Config;
use pipette_core::dataset::{DatasetArtifact, ProbeState, ProbeVerdict, UpstreamIdentity};
use pipette_plan_types::{
    BenchmarkId, Dataset, DatasetRepositoryUrl, DatasetUpstream, GitOid, Sha256,
    VlmEvalCorpusManifest,
};
use pipette_store::Stores;

/// Upstreams reached at once, so a run takes at most `ceil(n / CONCURRENCY)`
/// deadlines.
const CONCURRENCY: usize = 4;

/// Reading a package for its manifest (a few hundred bytes) stops once this
/// much of it is in hand, so a registration never downloads the corpus.
const MANIFEST_HEAD_BYTES: usize = 64 * 1024;

/// How the upstreams a catalog pins are reached. Production spells a git `url`
/// `https://` and asks the configured evals server; the tests point the same
/// catalog at a `file://` repository and a local evals server.
trait Upstreams: Sync {
    fn remote(&self, url: &DatasetRepositoryUrl) -> String;
    /// The evals server's base URL (`evals_server_url`).
    fn evals_server(&self) -> &str;
    fn http(&self) -> &reqwest::Client;
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
            // to answer `/samples/digest` (a first pass over a set to learn
            // it), and the probe's or the build's deadline bounds that.
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
/// from: every pinned artifact at the build deadline, one at least, except
/// that an evals-server pin counts only while it is unregistered. A run
/// skips a registered one in a moment, and counting every one would hold a
/// crashed run's lease for hours.
pub async fn build_run_deadline(config: &Config, stores: &Stores) -> anyhow::Result<Duration> {
    let deadline = secs(config.dataset_build_timeout_secs);
    let mut total = Duration::ZERO;
    for pin in pinned(stores).await?.pins() {
        if matches!(pin.dataset.upstream, DatasetUpstream::EvalsServer { .. })
            && is_built(stores, &pin.dataset).await?
        {
            continue;
        }
        total = total.saturating_add(deadline);
    }
    Ok(total.max(deadline))
}

/// Whether both a pin's tar and its record exist, as a build that would skip
/// it reads them.
async fn is_built(stores: &Stores, dataset: &Dataset) -> anyhow::Result<bool> {
    Ok(has_tar(stores, dataset).await?
        && stores
            .dataset_cache
            .get_artifact(&dataset.digest)
            .await?
            .is_some())
}

/// Whether the tar a pin is served from exists: an evals-server pin's is the
/// scores service's package, a git pin's the one its build packed.
async fn has_tar(stores: &Stores, dataset: &Dataset) -> anyhow::Result<bool> {
    Ok(match dataset.upstream {
        DatasetUpstream::Git { .. } => stores.dataset_cache.has_tar(&dataset.digest).await?,
        DatasetUpstream::EvalsServer { .. } => stores
            .eval_packages
            .package_bytes(&dataset.digest)
            .await?
            .is_some(),
    })
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
    let was_ok = built
        .iter()
        .any(|b| b.artifact.probe.state == ProbeState::Ok);
    let digests = built.iter().map(|b| &b.artifact.digest).collect::<Vec<_>>();
    let reached = reach(upstreams, &upstream, &digests, deadline, was_ok).await;
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

/// Probe `upstream` within `deadline`. If an artifact of the upstream `was_ok`
/// and the first attempt finds the upstream unavailable, a second attempt
/// runs. The first attempt then gets half of the deadline, and the second
/// attempt gets the rest. Thus one stalled fetch does not refuse the
/// benchmarks until the next run.
async fn reach(
    upstreams: &impl Upstreams,
    upstream: &DatasetUpstream,
    digests: &[&Sha256],
    deadline: Duration,
    was_ok: bool,
) -> Reached {
    let end = tokio::time::Instant::now() + deadline;
    let first_end = if was_ok { end - deadline / 2 } else { end };
    let first = attempt(upstreams, upstream, digests, 1, first_end).await;
    match &first {
        Reached::Failed(ProbeState::Unavailable, first_detail) if was_ok => {
            let second = attempt(upstreams, upstream, digests, 2, end).await;
            tracing::info!(
                event = "dataset_probe_retried",
                upstream = upstream.reference(),
                first_detail,
                saved = matches!(second, Reached::Observed(_)),
                "dataset upstream probe retried after an unavailable attempt"
            );
            second
        }
        Reached::Failed(..) | Reached::Observed(_) => first,
    }
}

/// One probe of `upstream` in a new scratch directory, stopped at `until`.
/// A git probe initializes its scratch repository, so each attempt needs its
/// own directory.
async fn attempt(
    upstreams: &impl Upstreams,
    upstream: &DatasetUpstream,
    digests: &[&Sha256],
    attempt: u8,
    until: tokio::time::Instant,
) -> Reached {
    let started = tokio::time::Instant::now();
    let reached = match scratch_dir() {
        Ok(scratch) => {
            tokio::time::timeout_at(until, probe_upstream(upstreams, scratch.path(), upstream))
                .await
                .unwrap_or_else(|_| {
                    Reached::Failed(
                        ProbeState::Unavailable,
                        format!(
                            "probe timed out after {}s",
                            until.saturating_duration_since(started).as_secs()
                        ),
                    )
                })
        }
        Err(e) => Reached::Failed(
            ProbeState::Unavailable,
            format!("no scratch directory: {e:#}"),
        ),
    };
    let outcome = match &reached {
        Reached::Observed(_) => "observed".to_owned(),
        Reached::Failed(state, _) => state.to_string(),
    };
    let elapsed_ms = started.elapsed().as_millis();
    for digest in digests {
        tracing::debug!(
            event = "dataset_probe_attempt",
            upstream = upstream.reference(),
            digest = digest.as_ref(),
            attempt,
            elapsed_ms,
            outcome,
            "dataset upstream probe attempt finished"
        );
    }
    reached
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
    build_all(
        &Live::new(config)?,
        secs(config.dataset_build_timeout_secs),
        stores,
    )
    .await
}

/// Builds the missing artifacts, four at a time.
async fn build_all(
    upstreams: &impl Upstreams,
    deadline: Duration,
    stores: &Stores,
) -> anyhow::Result<()> {
    let pinned = pinned(stores).await?;
    report_conflicts(&pinned.conflicts);
    let conflicts = pinned.conflicts.clone();
    let outcomes: Vec<Option<String>> = futures::stream::iter(pinned.pins())
        .map(|pin| build_reported(upstreams, deadline, stores, pin))
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
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
/// new one was written. A git pin is fetched and packed, an evals-server pin
/// registered ([`register`]). `samples` is the count its entries state
/// ([`Pin::samples`]), which the corpus must hold.
async fn build_one(
    upstreams: &impl Upstreams,
    deadline: Duration,
    stores: &Stores,
    digest: &Sha256,
    dataset: &Dataset,
    samples: Option<u32>,
) -> anyhow::Result<bool> {
    if has_tar(stores, dataset).await?
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
    let artifact = match &dataset.upstream {
        DatasetUpstream::Git { .. } => build_git(upstreams, deadline, stores, dataset).await?,
        DatasetUpstream::EvalsServer { .. } => {
            tokio::time::timeout(deadline, register(upstreams, stores, dataset))
                .await
                .map_err(|_| {
                    anyhow::anyhow!("registration timed out after {}s", deadline.as_secs())
                })??
        }
    };
    stores.dataset_cache.put_artifact(&artifact).await?;
    // Checked once the artifact is stored, so a later run fails from the
    // record; `GET /datasets` refuses the entry until its count agrees.
    if let Some(stated) = samples {
        ensure_holds(artifact.samples, stated)?;
    }
    Ok(true)
}

/// Fetch and pack a git pin and store its tar; its record is a passing probe,
/// as the upstream was just verified in full.
async fn build_git(
    upstreams: &impl Upstreams,
    deadline: Duration,
    stores: &Stores,
    dataset: &Dataset,
) -> anyhow::Result<DatasetArtifact> {
    let DatasetUpstream::Git {
        identity: pinned,
        digest: pinned_tree,
        ..
    } = &dataset.upstream
    else {
        anyhow::bail!("{} is not a git upstream", dataset.upstream.reference());
    };
    tracing::info!(digest = %dataset.digest, "building dataset artifact");
    let scratch = scratch_dir()?;
    let built = materialize(upstreams, scratch.path(), dataset, deadline)
        .await?
        .ok_or_else(|| anyhow::anyhow!("build timed out after {}s", deadline.as_secs()))?;
    anyhow::ensure!(
        built.identity == *pinned,
        "upstream moved: {} observed, {pinned} pinned",
        built.identity
    );
    anyhow::ensure!(
        built.upstream_digest == *pinned_tree,
        "upstream tree digest {} differs from the pinned {pinned_tree}",
        built.upstream_digest
    );
    anyhow::ensure!(
        built.digest == dataset.digest && built.bytes == dataset.bytes,
        "packed artifact is {} ({} bytes), pinned {} ({} bytes): the adapter or packer changed",
        built.digest,
        built.bytes,
        dataset.digest,
        dataset.bytes
    );
    stores
        .dataset_cache
        .put_tar(&dataset.digest, &built.tar)
        .await?;
    let now = Utc::now();
    Ok(DatasetArtifact {
        digest: dataset.digest.clone(),
        bytes: dataset.bytes,
        format: dataset.format,
        adapter: dataset.adapter,
        upstream: dataset.upstream.clone(),
        subset: dataset.subset.clone(),
        samples: None,
        built_at: now,
        probe: ProbeVerdict {
            state: ProbeState::Ok,
            checked_at: now,
            since: now,
            observed_identity: Some(UpstreamIdentity::GitTree(built.identity)),
            detail: None,
        },
    })
}

/// An evals-server pin's corpus is the package the scores service wrote,
/// never fetched or packed here: its record is written once the package is
/// the pinned size, its manifest names the pin, and the evals server still
/// serves the pinned samples.
async fn register(
    upstreams: &impl Upstreams,
    stores: &Stores,
    dataset: &Dataset,
) -> anyhow::Result<DatasetArtifact> {
    let digest = &dataset.digest;
    let bytes = stores
        .eval_packages
        .package_bytes(digest)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("no package {digest}: the scores service has not packed it")
        })?;
    anyhow::ensure!(
        bytes == dataset.bytes,
        "package {digest} is {bytes} bytes, pinned {}",
        dataset.bytes
    );
    let manifest = package_manifest(stores, digest).await?;
    anyhow::ensure!(
        manifest.format == dataset.format
            && manifest.adapter == dataset.adapter
            && manifest.upstream == dataset.upstream,
        "package {digest} is {} {} of {:?}, the entry pins {} {} of {:?}",
        manifest.format,
        manifest.adapter,
        manifest.upstream,
        dataset.format,
        dataset.adapter,
        dataset.upstream
    );
    let scratch = scratch_dir()?;
    let (state, observed, detail) = probe_upstream(upstreams, scratch.path(), &dataset.upstream)
        .await
        .verdict(&dataset.upstream);
    anyhow::ensure!(
        state == ProbeState::Ok,
        "upstream {state}: {}",
        detail.unwrap_or_default()
    );
    let now = Utc::now();
    Ok(DatasetArtifact {
        digest: digest.clone(),
        bytes,
        format: dataset.format,
        adapter: dataset.adapter,
        upstream: dataset.upstream.clone(),
        subset: dataset.subset.clone(),
        samples: Some(manifest.samples),
        built_at: now,
        probe: ProbeVerdict {
            state,
            checked_at: now,
            since: now,
            observed_identity: observed,
            detail: None,
        },
    })
}

/// A package's `manifest.json`, its first entry, read from the head of the
/// stream.
async fn package_manifest(
    stores: &Stores,
    digest: &Sha256,
) -> anyhow::Result<VlmEvalCorpusManifest> {
    let mut chunks = stores
        .eval_packages
        .open_package(digest)
        .await?
        .ok_or_else(|| anyhow::anyhow!("package {digest} is gone"))?
        .stream;
    let mut head = Vec::new();
    while head.len() < MANIFEST_HEAD_BYTES
        && let Some(chunk) = chunks.try_next().await?
    {
        head.extend_from_slice(&chunk);
    }
    let mut archive = tar::Archive::new(head.as_slice());
    let mut entry = archive
        .entries()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("package {digest} is an empty tar"))??;
    anyhow::ensure!(
        entry.path()?.to_string_lossy() == "manifest.json",
        "package {digest} does not start with manifest.json"
    );
    let mut manifest = Vec::new();
    entry.read_to_end(&mut manifest)?;
    anyhow::ensure!(
        manifest.len() as u64 == entry.size(),
        "package {digest}'s manifest.json runs past its first {MANIFEST_HEAD_BYTES} bytes"
    );
    serde_json::from_slice(&manifest)
        .with_context(|| format!("package {digest}'s manifest.json is not a corpus manifest"))
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

/// What a git corpus holds, for the operator who pins it: one subset when the
/// entry names a `subset`, otherwise every subset the upstream has, for an
/// operator to split into per-dataset entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contents {
    pub subsets: Vec<String>,
    pub samples: u32,
    pub turns: u32,
}

/// What fetching and packing an upstream produced.
struct Materialized {
    identity: GitOid,
    /// A git upstream's full tree digest.
    upstream_digest: Sha256,
    digest: Sha256,
    bytes: u64,
    contents: Contents,
    tar: PathBuf,
}

/// `None` when `deadline` passed first.
async fn materialize(
    upstreams: &impl Upstreams,
    scratch: &Path,
    dataset: &Dataset,
    deadline: Duration,
) -> anyhow::Result<Option<Materialized>> {
    let tar = scratch.join("corpus.tar");
    match tokio::time::timeout(deadline, materialize_git(upstreams, scratch, dataset, tar)).await {
        Ok(built) => built.map(Some),
        Err(_) => Ok(None),
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
        identity,
        upstream_digest: tree_digest,
        digest: packed.digest,
        bytes: packed.bytes,
        contents: Contents {
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
    pub identity: GitOid,
    /// A git upstream's tree digest.
    pub upstream_digest: Sha256,
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
        writeln!(f, "identity = \"{}\"", self.identity)?;
        writeln!(f, "digest   = \"{}\"", self.upstream_digest)?;
        writeln!(f)?;
        let Contents {
            subsets,
            samples,
            turns,
        } = &self.contents;
        write!(
            f,
            "# {samples} samples, {turns} turns; subsets: {}",
            subsets.join(", ")
        )
    }
}

/// Only the fields that name the upstream (`url`, `revision`, `subpath`,
/// `adapter`, `subset` and `license`) are read from a git entry: the hashes it
/// holds may still be placeholders. An evals-server entry is refused: the
/// scores service's index lists its hashes.
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
    if let DatasetUpstream::EvalsServer {
        eval_id,
        dataset_version,
        ..
    } = &dataset.upstream
    {
        anyhow::bail!(
            "benchmark {benchmark_id} pins a corpus the scores service packs: its hashes are in \
             scores-packages/index/<scores version>/{eval_id}/{dataset_version}.json"
        );
    }
    let scratch = scratch_dir()?;
    let deadline = secs(config.dataset_build_timeout_secs);
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

    /// A git corpus's subsets and counts.
    fn counts(pins: &Pins) -> (&[String], u32, u32) {
        let Contents {
            subsets,
            samples,
            turns,
        } = &pins.contents;
        (subsets, *samples, *turns)
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
            pins.digest, pins.bytes, pins.identity, pins.upstream_digest
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
            counts(&all),
            (&["GQA".to_string(), "TextVQA".to_string()][..], 2, 3)
        );
        assert!(all.to_string().contains("subsets: GQA, TextVQA"), "{all}");
        let gqa = local_pins(&remotes, dir.path(), &revision, Some("GQA")).await?;
        let textvqa = local_pins(&remotes, dir.path(), &revision, Some("TextVQA")).await?;
        assert_eq!(counts(&gqa), (&["GQA".to_string()][..], 1, 1));
        assert_eq!((counts(&textvqa).1, counts(&textvqa).2), (1, 2));
        assert_ne!(gqa.digest, textvqa.digest);
        // The upstream pins are the upstream's, whatever subset is kept.
        assert_eq!(gqa.identity, all.identity);
        assert_eq!(gqa.upstream_digest, textvqa.upstream_digest);

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
        assert_eq!(
            built.probe.observed_identity,
            Some(UpstreamIdentity::GitTree(gqa.identity.clone()))
        );
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

    /// The first fetch goes to a listener that never answers, so it stalls;
    /// every later fetch reaches the local repository.
    struct StallsOnce {
        local: Local,
        stall: String,
        fetches: std::sync::atomic::AtomicUsize,
    }

    impl Upstreams for StallsOnce {
        fn remote(&self, url: &DatasetRepositoryUrl) -> String {
            if self
                .fetches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                self.stall.clone()
            } else {
                self.local.remote(url)
            }
        }

        fn evals_server(&self) -> &str {
            self.local.evals_server()
        }

        fn http(&self) -> &reqwest::Client {
            self.local.http()
        }
    }

    /// A stalled fetch of an `ok` upstream times out, and the second attempt,
    /// in a new scratch directory, keeps the artifact `ok`.
    #[tokio::test]
    async fn a_stalled_fetch_of_an_ok_upstream_is_retried() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (origin, revision) = origin(dir.path())?;
        let remotes = Local::new(origin.clone());
        let pins = local_pins(&remotes, dir.path(), &revision, Some("GQA")).await?;
        let stores = stores(dir.path(), &revision, &[("GQA", &pins)])?;
        build_all(&remotes, Duration::from_secs(60), &stores).await?;
        probe_all(&remotes, Duration::from_secs(60), &stores).await?;

        // The kernel accepts the connection and nothing answers it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let stalls = StallsOnce {
            local: Local::new(origin),
            stall: format!("http://{}/corpus.git", listener.local_addr()?),
            fetches: std::sync::atomic::AtomicUsize::new(0),
        };
        probe_all(&stalls, Duration::from_secs(4), &stores).await?;
        assert_eq!(stalls.fetches.load(std::sync::atomic::Ordering::SeqCst), 2);
        let artifact = stores
            .dataset_cache
            .get_artifact(&pins.digest)
            .await?
            .ok_or_else(|| anyhow::anyhow!("a probe never deletes"))?;
        assert_eq!(artifact.probe.state, ProbeState::Ok);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // evals-server upstream
    // -----------------------------------------------------------------------

    mod evals {
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        use axum::Router;
        use axum::extract::{Path as UrlPath, Query, State};
        use axum::http::StatusCode;
        use axum::response::{IntoResponse, Response};
        use axum::routing::get;
        use base64::Engine as _;

        use pipette_plan_types::{
            CorpusFormat, DatasetAdapter, EvalSample, SamplesDigest, VlmEvalCorpusSample,
        };

        use super::*;

        /// What the mock serves, and what it was asked.
        #[derive(Default)]
        struct Served {
            samples: Vec<EvalSample>,
            /// Answer every request with this status instead.
            status: Option<StatusCode>,
            digest_requests: AtomicUsize,
            /// Answer this many more requests with `503`.
            fail_first: usize,
        }

        /// An evals server reporting the digest of MMStar 2026.10.1's samples
        /// for the dataset prompt, and of the ASR task's for `voxtral`, the way
        /// the real one answers.
        struct MockEvals {
            url: String,
            served: Arc<Mutex<Served>>,
            task: tokio::task::JoinHandle<()>,
            http: reqwest::Client,
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

        /// The probes the mock has answered.
        fn digest_requests(evals: &MockEvals) -> usize {
            lock(&evals.served).digest_requests.load(Ordering::SeqCst)
        }

        /// The samples a request names, or the status that refuses it.
        fn answer(
            served: &Shared,
            eval: &str,
            version: &str,
            query: &HashMap<String, String>,
        ) -> Result<Vec<EvalSample>, StatusCode> {
            let mut served = lock(served);
            if let Some(status) = served.status {
                return Err(status);
            }
            if served.fail_first > 0 {
                served.fail_first -= 1;
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
            let profile = query.get("prompt_profile").map(String::as_str);
            match (eval, version, profile) {
                ("mmstar", "2026.10.1", None) | (ASR_TASK, "2026.10.1", Some("voxtral")) => {
                    Ok(served.samples.clone())
                }
                _ => Err(StatusCode::NOT_FOUND),
            }
        }

        async fn digest_route(
            State(served): State<Shared>,
            UrlPath((eval, version)): UrlPath<(String, String)>,
            Query(query): Query<HashMap<String, String>>,
        ) -> Response {
            lock(&served).digest_requests.fetch_add(1, Ordering::SeqCst);
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

        /// `served` with one prompt reworded: other samples under the same pin.
        fn reworded(served: &[EvalSample]) -> anyhow::Result<Vec<EvalSample>> {
            let mut reworded = served.to_vec();
            reworded[2] = serde_json::from_value(serde_json::json!(
                {"id": "c", "messages": [{"role": "user", "content": "No picture at all?"}]}
            ))?;
            Ok(reworded)
        }

        /// The asr_eval task the mock serves, under the `voxtral` profile.
        const ASR_TASK: &str = "asr_fleurs_nl_nl";

        /// A WAV-shaped clip of `fill` bytes: what a package moves out is the
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

        /// mgmt's golden samples (the `vlm_eval_corpus.rs` tests of
        /// `pipette-plan-types`), whose digest the scorer pins too.
        fn golden() -> anyhow::Result<Vec<EvalSample>> {
            Ok(serde_json::from_value(serde_json::json!([
                {"id": "a1", "messages": [
                    {"role": "system", "content": "Think first."},
                    {"role": "user", "content": [
                        {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
                        {"type": "text", "text": "Which mood? 心情\nA. Sad\nB. Happy"}
                    ]}
                ]},
                {"id": "b2", "messages": [
                    {"role": "user", "content": "Is there a dog? \"Yes\" or no."}
                ]}
            ]))?)
        }

        /// mgmt's ASR golden samples, from the same tests as [`golden`].
        fn asr_golden() -> anyhow::Result<Vec<EvalSample>> {
            Ok(serde_json::from_value(serde_json::json!([
                {"id": "d4",
                 "messages": [{"role": "user", "content": [
                     {"type": "input_audio", "input_audio": {"data": "UklGRiQAAABXQVZF", "format": "wav"}}
                 ]}],
                 "chat_template_kwargs": {"transcription": true, "language": "nl"}},
                {"id": "e5", "messages": [
                    {"role": "system", "content": "Perform ASR."},
                    {"role": "user", "content": [
                        {"type": "text", "text": "Transcribe."},
                        {"type": "input_audio", "input_audio": {"data": "UklGRiQAAABXQVZF", "format": "wav"}}
                    ]}
                ]}
            ]))?)
        }

        /// The upstream of the MMStar entries [`vlm_stores_of`] writes.
        fn mmstar() -> serde_json::Value {
            serde_json::json!({
                "source": "evals_server",
                "eval_id": "mmstar",
                "dataset_version": "2026.10.1",
            })
        }

        /// The upstream of the asr_eval entry [`put_asr_entry`] writes.
        fn fleurs_voxtral() -> serde_json::Value {
            serde_json::json!({
                "source": "evals_server",
                "eval_id": ASR_TASK,
                "dataset_version": "2026.10.1",
                "prompt_profile": "voxtral",
            })
        }

        /// `samples` packed into `dir` as the scores service packs a package:
        /// each media part moved into `media/`, one line per sample, and a
        /// manifest naming `upstream` at the samples' digest. The pin naming
        /// the package, and the package.
        fn pack(
            dir: &Path,
            format: CorpusFormat,
            mut upstream: serde_json::Value,
            samples: &[EvalSample],
        ) -> anyhow::Result<(Dataset, PathBuf)> {
            std::fs::create_dir_all(dir.join("media"))?;
            let mut media = BTreeMap::new();
            let mut lines = Vec::new();
            samples.iter().try_for_each(|sample| {
                let mut line =
                    VlmEvalCorpusSample::from_eval_sample_in(format, sample, |path, bytes| {
                        let file = dir.join(path);
                        std::fs::write(&file, bytes)?;
                        media.insert(path.to_owned(), file);
                        Ok(())
                    })?;
                // Sorted as the scores service writes them: the test build's serde_json
                // keeps insertion order (aws-sdk-s3's test-util enables preserve_order).
                if let Some(kwargs) = &mut line.chat_template_kwargs {
                    kwargs.sort_keys();
                    kwargs
                        .values_mut()
                        .for_each(serde_json::Value::sort_all_objects);
                }
                serde_json::to_writer(&mut lines, &line)?;
                lines.push(b'\n');
                anyhow::Ok(())
            })?;
            upstream["identity"] = serde_json::to_value(digest_of(samples)?)?;
            let upstream: DatasetUpstream = serde_json::from_value(upstream)?;
            let mut manifest = serde_json::to_vec(&VlmEvalCorpusManifest {
                format,
                adapter: DatasetAdapter::EvalsServerV1,
                upstream: upstream.clone(),
                samples: u32::try_from(samples.len())?,
                media: u32::try_from(media.len())?,
            })?;
            manifest.push(b'\n');
            let tar = dir.join("package.tar");
            let (digest, bytes) = format::write_corpus(&manifest, &lines, &media, &tar)?;
            let pin = Dataset {
                format,
                adapter: DatasetAdapter::EvalsServerV1,
                digest,
                bytes,
                subset: None,
                upstream,
                url: None,
            };
            Ok((pin, tar))
        }

        /// [`pack`] `samples` and write the package where the scores service
        /// writes it, under the packages store at `data_dir`.
        fn put_package(
            data_dir: &Path,
            format: CorpusFormat,
            upstream: serde_json::Value,
            samples: &[EvalSample],
        ) -> anyhow::Result<Dataset> {
            let scratch = tempfile::tempdir()?;
            let (pin, tar) = pack(scratch.path(), format, upstream, samples)?;
            let packages = data_dir.join("scores-packages/tars");
            std::fs::create_dir_all(&packages)?;
            std::fs::copy(tar, packages.join(format!("{}.tar", pin.digest)))?;
            Ok(pin)
        }

        /// The golden samples packed as packages: the scores service's tests
        /// pin the same digests and sizes, so the two packers agree.
        #[rstest::rstest]
        #[case::vlm(
            CorpusFormat::VlmEvalCorpusV1,
            "5654ef883c378c4325cfe70594545d4a19c094defdf21b32487f9de94a6b4849",
            "cecf36b2e29ae61be4fe20df91f8f0e849a3b718e340e831b469b3fd7db90f73",
            4096
        )]
        #[case::asr(
            CorpusFormat::AsrEvalCorpusV1,
            "b49fae68dfd0d735e59dc3df621d8b6189ec7977fc5830b54b368bd10f396ef7",
            "b36109aa457771c0273adefb6c467dab0a2c104b376818539da5eb9a3199f550",
            4096
        )]
        fn the_golden_samples_pack_as_the_scores_service_packs_them(
            #[case] format: CorpusFormat,
            #[case] samples_digest: &str,
            #[case] digest: &str,
            #[case] bytes: u64,
        ) -> anyhow::Result<()> {
            let (samples, upstream) = match format {
                CorpusFormat::VlmEvalCorpusV1 => (golden()?, mmstar()),
                CorpusFormat::AsrEvalCorpusV1 => (asr_golden()?, fleurs_voxtral()),
                CorpusFormat::VlSpecDecodeCorpusV1 => anyhow::bail!("not an eval corpus"),
            };
            let dir = tempfile::tempdir()?;
            let (pin, _) = pack(dir.path(), format, upstream, &samples)?;
            assert_eq!(
                (identity(&pin)?.as_ref(), pin.digest.as_ref(), pin.bytes),
                (samples_digest, digest, bytes)
            );
            Ok(())
        }

        /// The samples digest an evals-server pin names.
        fn identity(pin: &Dataset) -> anyhow::Result<&Sha256> {
            match &pin.upstream {
                DatasetUpstream::EvalsServer { identity, .. } => Ok(identity),
                DatasetUpstream::Git { .. } => anyhow::bail!("not an evals-server pin"),
            }
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

        /// Put the asr_eval entry pinning `pin` and stating `samples` samples
        /// into `org`'s catalog, replacing any earlier one. It is private to
        /// an organization, as every asr_eval entry is.
        async fn put_asr_entry(
            stores: &Stores,
            org: &pipette_plan_types::OrgId,
            pin: &Dataset,
            samples: u32,
        ) -> anyhow::Result<()> {
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
identity        = "{}"

[recipe]
family  = "Test family"
models  = [{{ repo = "org/repo" }}]
enable_thinking = false
sources = ["https://huggingface.co/org/repo"]
"#,
                pin.digest,
                pin.bytes,
                identity(pin)?
            );
            let benchmark = pipette_core::benchmark::Benchmark::from_toml(
                "asr_eval_fleurs_nl_nl_voxtral",
                &toml,
            )?;
            stores.catalog.put_org_benchmark(org, &benchmark).await?;
            Ok(())
        }

        /// A local-fs catalog holding one MMStar entry that pins `pin`.
        fn vlm_stores(dir: &Path, pin: &Dataset) -> anyhow::Result<Stores> {
            vlm_stores_of(dir, &[("vlm_eval_mmstar", pin)])
        }

        /// A local-fs catalog of MMStar entries, one per `(benchmark id, pin)`.
        fn vlm_stores_of(dir: &Path, entries: &[(&str, &Dataset)]) -> anyhow::Result<Stores> {
            let data_dir = dir.join("data");
            std::fs::create_dir_all(data_dir.join("benchmarks"))?;
            for (benchmark_id, pin) in entries {
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
identity        = "{}"

[recipe]
family  = "Test family"
models  = [{{ repo = "org/repo" }}]
sources = ["https://huggingface.co/org/repo"]
"#,
                        pin.digest,
                        pin.bytes,
                        identity(pin)?
                    ),
                )?;
            }
            build_local_fs_stores(&Config {
                storage: StorageConfig::LocalFs { data_dir },
                ..Config::default()
            })
        }

        async fn artifact(stores: &Stores, digest: &Sha256) -> anyhow::Result<DatasetArtifact> {
            stores
                .dataset_cache
                .get_artifact(digest)
                .await?
                .ok_or_else(|| anyhow::anyhow!("nothing ever deletes the artifact"))
        }

        /// [`build_one`] on the catalog's one pin, as a build run reads it.
        async fn build_only(evals: &MockEvals, stores: &Stores) -> anyhow::Result<bool> {
            let pin = pinned(stores)
                .await?
                .pins()
                .next()
                .ok_or_else(|| anyhow::anyhow!("the entry's pin"))?;
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

        /// The whole loop against a local evals server: the scores service's
        /// package registered from its manifest and one probe, never copied
        /// or repacked, then probed by digest alone; a reworded prompt, a
        /// withdrawn dataset and a dead server each stop the serving.
        #[tokio::test]
        async fn a_package_is_registered_and_tracks_its_samples() -> anyhow::Result<()> {
            let served = samples()?;
            let evals = mock_evals(served.clone()).await?;
            let dir = tempfile::tempdir()?;
            let pin = put_package(
                &dir.path().join("data"),
                CorpusFormat::VlmEvalCorpusV1,
                mmstar(),
                &served,
            )?;
            let stores = vlm_stores(dir.path(), &pin)?;
            let deadline = Duration::from_secs(60);
            build_all(&evals, deadline, &stores).await?;
            let registered = artifact(&stores, &pin.digest).await?;
            assert!(registered.is_built_from(&pin));
            assert_eq!(registered.probe.state, ProbeState::Ok);
            assert_eq!(
                registered.probe.observed_identity,
                Some(UpstreamIdentity::Samples(digest_of(&served)?))
            );
            assert_eq!(registered.samples, Some(4), "the manifest's count");
            assert_eq!(digest_requests(&evals), 1, "one probe registers it");
            assert!(!stores.dataset_cache.has_tar(&pin.digest).await?);
            assert!(
                !build_only(&evals, &stores).await?,
                "a registered package is never registered again"
            );
            assert_eq!(digest_requests(&evals), 1);

            probe_all(&evals, deadline, &stores).await?;
            assert_eq!(
                artifact(&stores, &pin.digest).await?.probe.state,
                ProbeState::Ok
            );

            // A reworded prompt is different samples under the same pin.
            let reworded = reworded(&served)?;
            lock(&evals.served).samples = reworded.clone();
            assert!(probe_all(&evals, deadline, &stores).await.is_err());
            let moved = artifact(&stores, &pin.digest).await?;
            assert_eq!(moved.probe.state, ProbeState::Moved);
            assert_eq!(
                moved.probe.observed_identity,
                Some(UpstreamIdentity::Samples(digest_of(&reworded)?))
            );

            lock(&evals.served).status = Some(StatusCode::NOT_FOUND);
            assert!(probe_all(&evals, deadline, &stores).await.is_err());
            assert_eq!(
                artifact(&stores, &pin.digest).await?.probe.state,
                ProbeState::Gone
            );

            evals.task.abort();
            while !evals.task.is_finished() {
                tokio::task::yield_now().await;
            }
            assert!(probe_all(&evals, deadline, &stores).await.is_err());
            assert_eq!(
                artifact(&stores, &pin.digest).await?.probe.state,
                ProbeState::Unavailable
            );
            Ok(())
        }

        /// Retry an unavailable probe once in the same run only if the artifact
        /// was `ok`. Do not retry a `gone` result.
        #[rstest::rstest]
        #[case::one_fault_is_retried(false, 1, None, ProbeState::Ok, 2)]
        #[case::two_faults_are_unavailable(false, 2, None, ProbeState::Unavailable, 2)]
        #[case::gone_is_not_retried(false, 0, Some(StatusCode::NOT_FOUND), ProbeState::Gone, 1)]
        #[case::down_before_is_not_retried(true, 1, None, ProbeState::Unavailable, 1)]
        #[tokio::test]
        async fn an_unavailable_probe_of_an_ok_upstream_is_retried_once(
            #[case] down_before: bool,
            #[case] faults: usize,
            #[case] status: Option<StatusCode>,
            #[case] state: ProbeState,
            #[case] requests: usize,
        ) -> anyhow::Result<()> {
            let served = samples()?;
            let evals = mock_evals(served.clone()).await?;
            let dir = tempfile::tempdir()?;
            let pin = put_package(
                &dir.path().join("data"),
                CorpusFormat::VlmEvalCorpusV1,
                mmstar(),
                &served,
            )?;
            let stores = vlm_stores(dir.path(), &pin)?;
            let deadline = Duration::from_secs(60);
            build_all(&evals, deadline, &stores).await?;
            if down_before {
                lock(&evals.served).status = Some(StatusCode::SERVICE_UNAVAILABLE);
                assert!(probe_all(&evals, deadline, &stores).await.is_err());
            }
            {
                let mut served = lock(&evals.served);
                served.fail_first = faults;
                served.status = status;
            }
            let before = digest_requests(&evals);
            let probed = probe_all(&evals, deadline, &stores).await;
            assert_eq!(probed.is_ok(), state == ProbeState::Ok, "{probed:?}");
            assert_eq!(artifact(&stores, &pin.digest).await?.probe.state, state);
            assert_eq!(digest_requests(&evals) - before, requests);
            Ok(())
        }

        /// Nothing is registered that is not the pinned package of samples
        /// the evals server still serves.
        #[rstest::rstest]
        #[case::not_packed("unpacked", "the scores service has not packed it")]
        #[case::another_size("size", "bytes, pinned")]
        #[case::another_profiles_package("profile", "the entry pins")]
        #[case::moved("moved", "now publishes")]
        #[case::gone("gone", "answers 404")]
        #[case::none("", "")]
        #[tokio::test]
        async fn a_registration_refuses_what_it_cannot_prove(
            #[case] fault: &str,
            #[case] names: &str,
        ) -> anyhow::Result<()> {
            let served = samples()?;
            let evals = mock_evals(served.clone()).await?;
            let dir = tempfile::tempdir()?;
            let format = CorpusFormat::VlmEvalCorpusV1;
            let mut upstream = mmstar();
            if fault == "profile" {
                upstream["prompt_profile"] = "qwen3_5".into();
            }
            let mut pin = if fault == "unpacked" {
                pack(dir.path(), format, upstream, &served)?.0
            } else {
                put_package(&dir.path().join("data"), format, upstream, &served)?
            };
            match fault {
                "size" => pin.bytes += 512,
                "moved" => lock(&evals.served).samples = reworded(&served)?,
                "gone" => lock(&evals.served).status = Some(StatusCode::NOT_FOUND),
                _ => {}
            }
            let stores = vlm_stores(dir.path(), &pin)?;
            let registered = build_only(&evals, &stores).await;
            if fault.is_empty() {
                assert!(registered?);
                return Ok(());
            }
            let error = registered
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_default();
            assert!(error.contains(names), "{error}");
            assert!(
                stores
                    .dataset_cache
                    .get_artifact(&pin.digest)
                    .await?
                    .is_none()
            );
            Ok(())
        }

        /// Two entries pin the same samples at different identities, as a stale
        /// entry beside a current one would: one probe of the evals server, and
        /// each artifact judged against its own pin.
        #[tokio::test]
        async fn each_artifact_is_judged_against_its_own_pin() -> anyhow::Result<()> {
            let before = samples()?;
            let after = reworded(&before)?;
            let evals = mock_evals(before.clone()).await?;
            let dir = tempfile::tempdir()?;
            let data_dir = dir.path().join("data");
            let format = CorpusFormat::VlmEvalCorpusV1;
            let old = put_package(&data_dir, format, mmstar(), &before)?;
            let new = put_package(&data_dir, format, mmstar(), &after)?;
            let stores = vlm_stores_of(
                dir.path(),
                &[("vlm_eval_mmstar_old", &old), ("vlm_eval_mmstar_new", &new)],
            )?;
            let deadline = Duration::from_secs(60);
            assert!(
                build_all(&evals, deadline, &stores).await.is_err(),
                "the old pin registers; the new one is not served yet"
            );
            lock(&evals.served).samples = after;
            build_all(&evals, deadline, &stores).await?;

            let probes = digest_requests(&evals);
            let error = probe_all(&evals, deadline, &stores)
                .await
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert_eq!(digest_requests(&evals), probes + 1);
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

        /// An asr_eval entry whose `parameter_sample_count` is not its
        /// package's fails its registration; a matching one registers. Either
        /// way the record keeps the count the package holds, so a second run
        /// of a disagreeing entry fails from the record without a probe.
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
            use pipette_core::types::Mint;

            let served = asr_samples()?;
            let evals = mock_evals(served.clone()).await?;
            let dir = tempfile::tempdir()?;
            let stores = public_only_stores(dir.path())?;
            let pin = put_package(
                &dir.path().join("data"),
                CorpusFormat::AsrEvalCorpusV1,
                fleurs_voxtral(),
                &served,
            )?;
            put_asr_entry(&stores, &pipette_plan_types::OrgId::mint(), &pin, stated).await?;
            match (build_only(&evals, &stores).await, refused) {
                (Ok(true), None) => {}
                (Err(e), Some(names)) => {
                    assert!(format!("{e:#}").contains(names), "{e:#}");
                    let probes = digest_requests(&evals);
                    let again = build_only(&evals, &stores)
                        .await
                        .err()
                        .map(|e| format!("{e:#}"));
                    assert!(
                        again.as_deref().is_some_and(|e| e.contains(names)),
                        "{again:?}"
                    );
                    assert_eq!(
                        digest_requests(&evals),
                        probes,
                        "the second run reads the record, not the upstream"
                    );
                }
                (got, want) => anyhow::bail!("got {got:?}, want refusal {want:?}"),
            }
            let registered = artifact(&stores, &pin.digest).await?;
            assert_eq!(
                (registered.format, registered.samples),
                (CorpusFormat::AsrEvalCorpusV1, Some(3))
            );
            Ok(())
        }

        /// A package already registered for an entry stating 3 samples is
        /// refused, not skipped, once that entry is replaced by one stating 4,
        /// and is served from the record again under an entry stating 3. A
        /// record without a count, which no registration writes, is refused.
        #[tokio::test]
        async fn a_registered_asr_corpus_must_hold_its_entry_count() -> anyhow::Result<()> {
            use pipette_core::types::Mint;

            let served = asr_samples()?;
            let evals = mock_evals(served.clone()).await?;
            let dir = tempfile::tempdir()?;
            let stores = public_only_stores(dir.path())?;
            let pin = put_package(
                &dir.path().join("data"),
                CorpusFormat::AsrEvalCorpusV1,
                fleurs_voxtral(),
                &served,
            )?;
            let org = pipette_plan_types::OrgId::mint();
            let build = |stated: u32| {
                let (evals, stores, pin, org) = (&evals, &stores, &pin, &org);
                async move {
                    put_asr_entry(stores, org, pin, stated).await?;
                    build_only(evals, stores).await
                }
            };
            let refused = "the corpus holds 3 samples, its entry states parameter_sample_count = 4";

            assert!(build(3).await?, "the first run registers the package");
            assert_eq!(artifact(&stores, &pin.digest).await?.samples, Some(3));
            let error = build(4).await.err().map(|e| format!("{e:#}"));
            assert!(
                error.as_deref().is_some_and(|e| e.contains(refused)),
                "{error:?}"
            );
            // Negative control: the same record under a matching entry.
            assert!(
                !build(3).await?,
                "read from the record, not registered again"
            );

            let countless = DatasetArtifact {
                samples: None,
                ..artifact(&stores, &pin.digest).await?
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

        /// The build lock's lease counts an evals-server pin only while it is
        /// unregistered.
        #[tokio::test]
        async fn the_build_lease_counts_only_unregistered_packages() -> anyhow::Result<()> {
            let served = samples()?;
            let evals = mock_evals(served.clone()).await?;
            let dir = tempfile::tempdir()?;
            let data_dir = dir.path().join("data");
            let format = CorpusFormat::VlmEvalCorpusV1;
            let current = put_package(&data_dir, format, mmstar(), &served)?;
            let stale = put_package(&data_dir, format, mmstar(), &reworded(&served)?)?;
            let stores = vlm_stores_of(
                dir.path(),
                &[
                    ("vlm_eval_mmstar", &current),
                    ("vlm_eval_mmstar_stale", &stale),
                ],
            )?;
            let config = Config::default();
            assert_eq!(
                build_run_deadline(&config, &stores).await?,
                Duration::from_secs(1800)
            );
            assert!(
                build_all(&evals, Duration::from_secs(60), &stores)
                    .await
                    .is_err(),
                "the stale pin's samples are no longer served"
            );
            assert_eq!(
                build_run_deadline(&config, &stores).await?,
                Duration::from_secs(900)
            );
            Ok(())
        }

        /// An evals-server entry's hashes are the scores service's: `pins`
        /// names the index file that holds them, and fetches nothing.
        #[tokio::test]
        async fn pins_points_an_evals_entry_at_the_scores_index() -> anyhow::Result<()> {
            let dir = tempfile::tempdir()?;
            let (pin, _) = pack(
                dir.path(),
                CorpusFormat::VlmEvalCorpusV1,
                mmstar(),
                &samples()?,
            )?;
            let stores = vlm_stores(dir.path(), &pin)?;
            let error = pins(
                &Config::default(),
                &stores,
                &BenchmarkId::try_new("vlm_eval_mmstar")?,
            )
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
            assert!(
                error.contains("scores-packages/index/<scores version>/mmstar/2026.10.1.json"),
                "{error}"
            );
            Ok(())
        }
    }
}
