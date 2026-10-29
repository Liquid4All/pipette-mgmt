//! A benchmark's corpus as only the server handles it: the upstream pin, the
//! artifact mgmt repacks it into, and the probe verdict that gates serving
//! (`docs/design/vl-spec-decode-dataset-delivery.md`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use pipette_plan_types::{CorpusFormat, Dataset, DatasetAdapter, DatasetUpstream, GitOid, Sha256};

// ---------------------------------------------------------------------------
// Catalog checks
// ---------------------------------------------------------------------------

/// A catalog entry the loader refuses.
#[derive(Debug, thiserror::Error)]
pub enum DatasetError {
    #[error("dataset.subset is blank; omit it to keep the whole upstream")]
    BlankSubset,
    #[error(
        "dataset format {format}, adapter {adapter} and a {upstream} upstream do not go together"
    )]
    Mismatched {
        format: CorpusFormat,
        adapter: DatasetAdapter,
        upstream: &'static str,
    },
    #[error("dataset.subset names part of a git upstream; an evals-server corpus has none")]
    SubsetOfEvalsServer,
    #[error("{0:?} is not one plain name (letters, digits, `.`, `_`, `-`)")]
    NotASegment(String),
    #[error("a {benchmark} entry needs a {expected} dataset, not {format}")]
    WrongFormat {
        benchmark: &'static str,
        expected: CorpusFormat,
        format: CorpusFormat,
    },
    #[error(
        "a vlm_eval entry must pin the samples it runs: a `[dataset]` of format \
         vlm-eval-corpus/1 (`pipette-mgmt dataset-cache pins` prints one)"
    )]
    VlmEvalUnpinned,
    #[error(
        "an asr_eval entry must pin the samples it runs: a `[dataset]` of format \
         asr-eval-corpus/1 (`pipette-mgmt dataset-cache pins` prints one)"
    )]
    AsrEvalUnpinned,
    #[error(
        "dataset.upstream pins {pinned}, but the entry runs {runs}: the corpus would not \
         hold the samples the scorer grades"
    )]
    VlmEvalOtherSamples { pinned: String, runs: String },
}

/// The format a corpus is packed in follows from its adapter, and the adapter
/// from its upstream: one combination per kind of corpus.
fn kind(dataset: &Dataset) -> Result<(), DatasetError> {
    let upstream = match &dataset.upstream {
        DatasetUpstream::Git { .. } => "git",
        DatasetUpstream::EvalsServer { .. } => "evals-server",
    };
    let coherent = matches!(
        (dataset.format, dataset.adapter, &dataset.upstream),
        (
            CorpusFormat::VlSpecDecodeCorpusV1,
            DatasetAdapter::MmspecV1,
            DatasetUpstream::Git { .. }
        ) | (
            CorpusFormat::VlmEvalCorpusV1 | CorpusFormat::AsrEvalCorpusV1,
            DatasetAdapter::EvalsServerV1,
            DatasetUpstream::EvalsServer { .. }
        )
    );
    if coherent {
        Ok(())
    } else {
        Err(DatasetError::Mismatched {
            format: dataset.format,
            adapter: dataset.adapter,
            upstream,
        })
    }
}

/// An evals-server pin's names go into a URL path and into the key the probe
/// groups pins by, so each is one plain segment.
fn plain_segments(upstream: &DatasetUpstream) -> Result<(), DatasetError> {
    let DatasetUpstream::EvalsServer {
        eval_id,
        dataset_version,
        prompt_profile,
        ..
    } = upstream
    else {
        return Ok(());
    };
    let plain = |s: &str| {
        s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    [
        Some(eval_id.as_ref()),
        Some(dataset_version.as_ref()),
        prompt_profile.as_ref().map(AsRef::as_ref),
    ]
    .into_iter()
    .flatten()
    .find(|s: &&str| !plain(s))
    .map_or(Ok(()), |s| Err(DatasetError::NotASegment(s.to_owned())))
}

/// What the shared type's parse cannot say.
pub fn check(dataset: &Dataset) -> Result<(), DatasetError> {
    kind(dataset)?;
    plain_segments(&dataset.upstream)?;
    match (&dataset.subset, &dataset.upstream) {
        (Some(s), _) if s.trim().is_empty() => Err(DatasetError::BlankSubset),
        (Some(_), DatasetUpstream::EvalsServer { .. }) => Err(DatasetError::SubsetOfEvalsServer),
        _ => Ok(()),
    }
}

/// A `vl_spec_decode` entry reads a `vl-spec-decode-corpus/1`.
pub fn check_vl_spec_decode(dataset: &Dataset) -> Result<(), DatasetError> {
    match dataset.format {
        CorpusFormat::VlSpecDecodeCorpusV1 => Ok(()),
        format @ (CorpusFormat::VlmEvalCorpusV1 | CorpusFormat::AsrEvalCorpusV1) => {
            Err(DatasetError::WrongFormat {
                benchmark: "vl_spec_decode",
                expected: CorpusFormat::VlSpecDecodeCorpusV1,
                format,
            })
        }
    }
}

/// A `vlm_eval` entry pins the evals server's samples for exactly the task,
/// dataset version and prompt profile it runs: those are what the scorer
/// grades its answers against.
pub fn check_vlm_eval(
    dataset: Option<&Dataset>,
    task: &str,
    dataset_version: &str,
    prompt_profile: Option<&str>,
) -> Result<(), DatasetError> {
    let dataset = dataset.ok_or(DatasetError::VlmEvalUnpinned)?;
    // An asr-eval corpus goes with the same adapter and upstream, but holds
    // clips, not images.
    if dataset.format != CorpusFormat::VlmEvalCorpusV1 {
        return Err(DatasetError::WrongFormat {
            benchmark: "vlm_eval",
            expected: CorpusFormat::VlmEvalCorpusV1,
            format: dataset.format,
        });
    }
    let DatasetUpstream::EvalsServer {
        eval_id,
        dataset_version: pinned_version,
        prompt_profile: pinned_profile,
        ..
    } = &dataset.upstream
    else {
        return Err(DatasetError::WrongFormat {
            benchmark: "vlm_eval",
            expected: CorpusFormat::VlmEvalCorpusV1,
            format: dataset.format,
        });
    };
    let pinned = (
        eval_id.as_ref(),
        pinned_version.as_ref(),
        pinned_profile.as_ref().map(AsRef::as_ref),
    );
    if pinned == (task, dataset_version, prompt_profile) {
        Ok(())
    } else {
        Err(DatasetError::VlmEvalOtherSamples {
            pinned: dataset.upstream.reference(),
            runs: format!(
                "{task}/{dataset_version}{}",
                prompt_profile.map_or(String::new(), |p| format!("?prompt_profile={p}"))
            ),
        })
    }
}

/// An `asr_eval` entry pins the evals server's samples for exactly the task,
/// dataset version and prompt profile it runs, as a `vlm_eval` does. The
/// profile is required: an ASR dataset holds no prompt, so each profile's
/// samples are a corpus of their own.
pub fn check_asr_eval(
    dataset: Option<&Dataset>,
    task: &str,
    dataset_version: &str,
    prompt_profile: &str,
) -> Result<(), DatasetError> {
    let dataset = dataset.ok_or(DatasetError::AsrEvalUnpinned)?;
    let wrong_format = || DatasetError::WrongFormat {
        benchmark: "asr_eval",
        expected: CorpusFormat::AsrEvalCorpusV1,
        format: dataset.format,
    };
    if dataset.format != CorpusFormat::AsrEvalCorpusV1 {
        return Err(wrong_format());
    }
    let DatasetUpstream::EvalsServer {
        eval_id,
        dataset_version: pinned_version,
        prompt_profile: pinned_profile,
        ..
    } = &dataset.upstream
    else {
        return Err(wrong_format());
    };
    let pinned = (
        eval_id.as_ref(),
        pinned_version.as_ref(),
        pinned_profile.as_ref().map(AsRef::as_ref),
    );
    if pinned == (task, dataset_version, Some(prompt_profile)) {
        Ok(())
    } else {
        Err(DatasetError::VlmEvalOtherSamples {
            pinned: dataset.upstream.reference(),
            runs: format!("{task}/{dataset_version}?prompt_profile={prompt_profile}"),
        })
    }
}

/// The tables every `vlm_eval` catalog entry carries, for a test catalog to
/// append after an entry's top-level keys: a `[dataset]` pinning the evals
/// server's samples for `eval_id`, `dataset_version` and `prompt_profile`
/// (placeholder hashes), and a `[recipe]` covering [`TESTKIT_RECIPE_REPO`]
/// with thinking unset.
#[cfg(any(test, feature = "testkit"))]
pub fn vlm_eval_catalog_tables(
    eval_id: &str,
    dataset_version: &str,
    prompt_profile: Option<&str>,
) -> String {
    let profile = prompt_profile.map_or(String::new(), |p| format!("prompt_profile  = \"{p}\"\n"));
    format!(
        "\n[dataset]\nformat  = \"vlm-eval-corpus/1\"\nadapter = \"evals-server@1\"\n\
         digest  = \"{}\"\nbytes   = 1024\n\n[dataset.upstream]\nsource          = \"evals_server\"\n\
         eval_id         = \"{eval_id}\"\ndataset_version = \"{dataset_version}\"\n{profile}\
         identity        = \"{}\"\n\n[recipe]\nfamily  = \"Test family\"\n\
         models  = [{{ repo = \"{TESTKIT_RECIPE_REPO}\" }}]\n\
         sources = [\"https://huggingface.co/{TESTKIT_RECIPE_REPO}\"]\n",
        "1".repeat(64),
        "2".repeat(64)
    )
}

/// The tables every `asr_eval` catalog entry carries, as
/// [`vlm_eval_catalog_tables`]: a `[dataset]` of format `asr-eval-corpus/1`
/// pinning `eval_id`, `dataset_version` and `prompt_profile` (placeholder
/// hashes), and a `[recipe]` covering [`TESTKIT_RECIPE_REPO`] with thinking
/// off.
#[cfg(any(test, feature = "testkit"))]
pub fn asr_eval_catalog_tables(
    eval_id: &str,
    dataset_version: &str,
    prompt_profile: &str,
) -> String {
    format!(
        "\n[dataset]\nformat  = \"asr-eval-corpus/1\"\nadapter = \"evals-server@1\"\n\
         digest  = \"{}\"\nbytes   = 1024\n\n[dataset.upstream]\nsource          = \"evals_server\"\n\
         eval_id         = \"{eval_id}\"\ndataset_version = \"{dataset_version}\"\n\
         prompt_profile  = \"{prompt_profile}\"\nidentity        = \"{}\"\n\n[recipe]\n\
         family  = \"Test family\"\nmodels  = [{{ repo = \"{TESTKIT_RECIPE_REPO}\" }}]\n\
         enable_thinking = false\n\
         sources = [\"https://huggingface.co/{TESTKIT_RECIPE_REPO}\"]\n",
        "1".repeat(64),
        "2".repeat(64)
    )
}

/// The one repo [`vlm_eval_catalog_tables`]' recipe covers.
#[cfg(any(test, feature = "testkit"))]
pub const TESTKIT_RECIPE_REPO: &str = "org/repo";

// ---------------------------------------------------------------------------
// Cached artifact
// ---------------------------------------------------------------------------

/// What a probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProbeState {
    /// Reachable, and publishing the pinned identity.
    Ok,
    /// Network failure, timeout or upstream fault: may clear on its own.
    Unavailable,
    /// Repository, revision or subpath missing or no longer public.
    Gone,
    /// Reachable, but publishing different content under the pin.
    Moved,
}

/// What a probe reads off an upstream to compare with its pin: a git tree id,
/// or the digest the evals server reports for its samples. Untagged, so an
/// artifact recorded before the evals server was an upstream reads unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UpstreamIdentity {
    GitTree(GitOid),
    Samples(Sha256),
}

impl UpstreamIdentity {
    /// The identity `upstream` pins.
    pub fn pinned(upstream: &DatasetUpstream) -> Self {
        match upstream {
            DatasetUpstream::Git { identity, .. } => Self::GitTree(identity.clone()),
            DatasetUpstream::EvalsServer { identity, .. } => Self::Samples(identity.clone()),
        }
    }
}

impl std::fmt::Display for UpstreamIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GitTree(oid) => oid.fmt(f),
            Self::Samples(digest) => write!(f, "sha256:{digest}"),
        }
    }
}

/// The latest probe, as recorded on an artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeVerdict {
    pub state: ProbeState,
    pub checked_at: DateTime<Utc>,
    /// When `state` last changed: what an alert keys on.
    pub since: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_identity: Option<UpstreamIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ProbeVerdict {
    /// `since` carries over unless the state changed, so an alert fires once
    /// per transition.
    pub fn next(
        previous: &ProbeVerdict,
        state: ProbeState,
        now: DateTime<Utc>,
        observed_identity: Option<UpstreamIdentity>,
        detail: Option<String>,
    ) -> ProbeVerdict {
        ProbeVerdict {
            state,
            checked_at: now,
            since: if previous.state == state {
                previous.since
            } else {
                now
            },
            observed_identity,
            detail,
        }
    }
}

/// `dataset-cache/artifacts/{digest}.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetArtifact {
    pub digest: Sha256,
    pub bytes: u64,
    pub format: CorpusFormat,
    pub adapter: DatasetAdapter,
    pub upstream: DatasetUpstream,
    /// The subset the artifact keeps, as its pin named it. Absent on an
    /// artifact recorded before this was, which is then not checked on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subset: Option<String>,
    /// The samples an evals-server corpus holds, as its build counted them;
    /// absent on a git corpus, and on a vlm-eval corpus recorded before this
    /// was. An asr_eval entry is built and served only at the count it states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples: Option<u32>,
    pub built_at: DateTime<Utc>,
    pub probe: ProbeVerdict,
}

/// Why an artifact must not be served right now. One variant per non-`ok`
/// verdict, so a handler has no `Ok` arm to give a status to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServeRefusal {
    /// The probe has not run within the allowed age: its verdict is not one.
    Stale { checked_at: DateTime<Utc> },
    /// The upstream could not be reached; may clear on its own.
    Unavailable { since: DateTime<Utc> },
    /// The upstream no longer publishes the pin (`gone` or `moved`).
    Broken {
        state: ProbeState,
        since: DateTime<Utc>,
    },
}

impl DatasetArtifact {
    /// Whether this artifact is the one `dataset` pins: its digest names the
    /// bytes, and this checks they were built from the same upstream, in the
    /// same format, by the same adapter.
    pub fn is_built_from(&self, dataset: &Dataset) -> bool {
        self.digest == dataset.digest
            && self.bytes == dataset.bytes
            && self.format == dataset.format
            && self.adapter == dataset.adapter
            && self.upstream == dataset.upstream
            && self
                .subset
                .as_ref()
                .is_none_or(|subset| dataset.subset.as_ref() == Some(subset))
    }

    /// A stale `ok` refuses like a failing one: a verdict nobody refreshed says
    /// nothing about the upstream now.
    pub fn serveable(
        &self,
        now: DateTime<Utc>,
        max_age: chrono::Duration,
    ) -> Result<(), ServeRefusal> {
        if now - self.probe.checked_at > max_age {
            return Err(ServeRefusal::Stale {
                checked_at: self.probe.checked_at,
            });
        }
        let since = self.probe.since;
        match self.probe.state {
            ProbeState::Ok => Ok(()),
            ProbeState::Unavailable => Err(ServeRefusal::Unavailable { since }),
            state @ (ProbeState::Gone | ProbeState::Moved) => {
                Err(ServeRefusal::Broken { state, since })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use pipette_plan_types::{DatasetPath, DatasetRepositoryUrl};

    use super::*;

    const ENTRY: &str = r#"
format  = "vl-spec-decode-corpus/1"
adapter = "mmspec@1"
digest  = "1111111111111111111111111111111111111111111111111111111111111111"
bytes   = 79163392
subset  = "GQA"

[upstream]
source   = "git"
url      = "https://github.com/acme/corpus"
revision = "0123456789abcdef0123456789abcdef01234567"
subpath  = "dataset/test"
identity = "2923603292c7dd463ad01a119f57965817778fc3"
digest   = "2222222222222222222222222222222222222222222222222222222222222222"
license  = "Apache-2.0"
"#;

    #[test]
    fn parses_a_git_dataset() -> anyhow::Result<()> {
        let dataset: Dataset = toml::from_str(ENTRY)?;
        assert_eq!(dataset.format, CorpusFormat::VlSpecDecodeCorpusV1);
        assert_eq!(dataset.adapter, DatasetAdapter::MmspecV1);
        assert_eq!(dataset.bytes, 79_163_392);
        assert_eq!(
            dataset.upstream.reference(),
            "github.com/acme/corpus@0123456789abcdef0123456789abcdef01234567:dataset/test"
        );
        assert_eq!(dataset.upstream.license(), Some("Apache-2.0"));
        assert_eq!(dataset.format.to_string(), "vl-spec-decode-corpus/1");
        assert_eq!(dataset.subset.as_deref(), Some("GQA"));
        check(&dataset)?;
        Ok(())
    }

    /// An evals-server pin's names are plain path segments: they reach a URL
    /// and the key the probe groups pins by, where `a/b` + `c` and `a` + `b/c`
    /// would otherwise read alike.
    #[rstest]
    #[case::plain("mmbench_en_dev", "2026.10.1", Some("lfm2_5_vl_3b"), true)]
    #[case::slash_in_the_eval("a/b", "c", None, false)]
    #[case::slash_in_the_version("a", "b/c", None, false)]
    #[case::query_in_the_profile("mmstar", "2026.10.1", Some("x?y"), false)]
    #[case::dot_segment("..", "2026.10.1", None, false)]
    #[case::current_segment("mmstar", ".", None, false)]
    fn an_evals_server_pin_names_plain_segments(
        #[case] eval_id: &str,
        #[case] dataset_version: &str,
        #[case] prompt_profile: Option<&str>,
        #[case] ok: bool,
    ) -> anyhow::Result<()> {
        let dataset: Dataset = serde_json::from_value(serde_json::json!({
            "format": "vlm-eval-corpus/1",
            "adapter": "evals-server@1",
            "digest": "1".repeat(64),
            "bytes": 1,
            "upstream": {
                "source": "evals_server",
                "eval_id": eval_id,
                "dataset_version": dataset_version,
                "prompt_profile": prompt_profile,
                "identity": "2".repeat(64),
            },
        }))?;
        assert_eq!(check(&dataset).is_ok(), ok);
        Ok(())
    }

    /// A pin's format, adapter and upstream go together, one combination per
    /// kind of corpus.
    #[test]
    fn a_pin_mixing_two_kinds_is_refused() -> anyhow::Result<()> {
        let dataset: Dataset =
            toml::from_str(&ENTRY.replace("vl-spec-decode-corpus/1", "vlm-eval-corpus/1"))?;
        assert!(matches!(
            check(&dataset),
            Err(DatasetError::Mismatched { .. })
        ));
        Ok(())
    }

    fn evals_server_pin(format: &str, profile: Option<&str>) -> anyhow::Result<Dataset> {
        Ok(serde_json::from_value(serde_json::json!({
            "format": format,
            "adapter": "evals-server@1",
            "digest": "1".repeat(64),
            "bytes": 1,
            "upstream": {
                "source": "evals_server",
                "eval_id": "asr_fleurs_nl_nl",
                "dataset_version": "2026.10.1",
                "prompt_profile": profile,
                "identity": "2".repeat(64),
            },
        }))?)
    }

    /// An asr-eval corpus goes with the evals server's adapter and upstream,
    /// as a vlm-eval one does, and with no other.
    #[test]
    fn an_asr_eval_corpus_comes_from_the_evals_server() -> anyhow::Result<()> {
        check(&evals_server_pin("asr-eval-corpus/1", Some("voxtral"))?)?;
        let from_git: Dataset =
            toml::from_str(&ENTRY.replace("vl-spec-decode-corpus/1", "asr-eval-corpus/1"))?;
        assert!(matches!(
            check(&from_git),
            Err(DatasetError::Mismatched { .. })
        ));
        Ok(())
    }

    /// An asr_eval pins the samples of exactly its task, version and profile,
    /// in its own format; a vlm_eval and a vl_spec_decode refuse its format.
    #[rstest]
    #[case::its_own(Some("voxtral"), "asr-eval-corpus/1", "asr_fleurs_nl_nl", None)]
    #[case::no_profile(
        None,
        "asr-eval-corpus/1",
        "asr_fleurs_nl_nl",
        Some("but the entry runs asr_fleurs_nl_nl/2026.10.1?prompt_profile=voxtral")
    )]
    #[case::other_profile(
        Some("gemma4"),
        "asr-eval-corpus/1",
        "asr_fleurs_nl_nl",
        Some("prompt_profile=gemma4, but")
    )]
    #[case::other_task(
        Some("voxtral"),
        "asr-eval-corpus/1",
        "asr_fleurs_de_de",
        Some("but the entry runs asr_fleurs_de_de")
    )]
    #[case::vlm_format(
        Some("voxtral"),
        "vlm-eval-corpus/1",
        "asr_fleurs_nl_nl",
        Some("needs a asr-eval-corpus/1 dataset")
    )]
    fn an_asr_eval_pins_its_own_samples(
        #[case] profile: Option<&str>,
        #[case] format: &str,
        #[case] task: &str,
        #[case] refused: Option<&str>,
    ) -> anyhow::Result<()> {
        let pin = evals_server_pin(format, profile)?;
        let checked = check_asr_eval(Some(&pin), task, "2026.10.1", "voxtral");
        match refused {
            None => checked?,
            Some(names) => {
                let err = checked.err().map(|e| e.to_string()).unwrap_or_default();
                assert!(err.contains(names), "{err}");
            }
        }
        Ok(())
    }

    #[test]
    fn an_unpinned_asr_eval_is_refused() {
        assert!(matches!(
            check_asr_eval(None, "asr_fleurs_nl_nl", "2026.10.1", "voxtral"),
            Err(DatasetError::AsrEvalUnpinned)
        ));
    }

    #[test]
    fn other_entries_refuse_an_asr_eval_corpus() -> anyhow::Result<()> {
        let pin = evals_server_pin("asr-eval-corpus/1", None)?;
        assert!(matches!(
            check_vlm_eval(Some(&pin), "asr_fleurs_nl_nl", "2026.10.1", None),
            Err(DatasetError::WrongFormat { .. })
        ));
        assert!(matches!(
            check_vl_spec_decode(&pin),
            Err(DatasetError::WrongFormat { .. })
        ));
        // The negative control: the same pin in the vlm format passes.
        check_vlm_eval(
            Some(&evals_server_pin("vlm-eval-corpus/1", None)?),
            "asr_fleurs_nl_nl",
            "2026.10.1",
            None,
        )?;
        Ok(())
    }

    /// Absent means the whole upstream; a blank name is a mistake, not a
    /// spelling of that.
    #[rstest]
    #[case::absent("", true)]
    #[case::blank("subset  = \" \"", false)]
    fn a_subset_is_absent_or_named(#[case] line: &str, #[case] ok: bool) -> anyhow::Result<()> {
        let dataset: Dataset = toml::from_str(&ENTRY.replace("subset  = \"GQA\"", line))?;
        assert_eq!(check(&dataset).is_ok(), ok);
        Ok(())
    }

    // The shared newtypes below reach a `git` argv and a local join here, so
    // their refusals are pinned on this side too: a change there must not loosen them.
    #[rstest]
    #[case::traversal("../etc/passwd")]
    #[case::absolute("/etc/passwd")]
    #[case::empty_segment("a//b")]
    #[case::current_dir("a/./b")]
    #[case::whitespace("a b/c")]
    #[case::shell_metachar("a/$(whoami)")]
    #[case::url_escape("a/%2e%2e")]
    fn rejects_a_path_that_is_not_a_contained_segment_list(#[case] path: &str) {
        assert!(DatasetPath::try_new(path).is_err(), "{path:?}");
    }

    /// An origin is stored scheme-less, so the same repo pasted two ways is one
    /// identity. Anything that is not a `host/path` coordinate on an allowed
    /// host is refused, including a leading `-` that `git` would read as an
    /// option.
    #[rstest]
    #[case::bare("github.com/acme/corpus", Some("github.com/acme/corpus"))]
    #[case::https("https://github.com/acme/corpus", Some("github.com/acme/corpus"))]
    #[case::other_scheme("file:///etc/passwd", None)]
    #[case::absolute("/etc/passwd", None)]
    #[case::host_only("github.com", None)]
    #[case::internal_host("gitlab.internal/acme/corpus", None)]
    #[case::userinfo("user@github.com/acme/corpus", None)]
    #[case::option("--upload-pack=x/y", None)]
    fn repository_url_normalizes_the_scheme_away(
        #[case] input: &str,
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        match expected {
            Some(stored) => {
                let url = DatasetRepositoryUrl::try_new(input)?;
                assert_eq!(url.as_ref(), stored);
            }
            None => assert!(DatasetRepositoryUrl::try_new(input).is_err(), "{input:?}"),
        }
        Ok(())
    }

    #[rstest]
    #[case::full("0123456789abcdef0123456789abcdef01234567", true)]
    #[case::branch("main", false)]
    #[case::tag("v1.0", false)]
    #[case::abbreviated("0123456", false)]
    fn a_git_oid_is_a_full_sha(#[case] value: &str, #[case] ok: bool) {
        assert_eq!(GitOid::try_new(value).is_ok(), ok, "{value:?}");
    }

    fn artifact(state: ProbeState, checked_at: DateTime<Utc>) -> anyhow::Result<DatasetArtifact> {
        let dataset: Dataset = toml::from_str(ENTRY)?;
        Ok(DatasetArtifact {
            digest: dataset.digest,
            bytes: dataset.bytes,
            format: dataset.format,
            adapter: dataset.adapter,
            upstream: dataset.upstream,
            subset: dataset.subset,
            samples: None,
            built_at: checked_at,
            probe: ProbeVerdict {
                state,
                checked_at,
                since: checked_at,
                observed_identity: None,
                detail: None,
            },
        })
    }

    /// Served only on a fresh `ok`; a stale `ok` refuses like a failing verdict.
    #[rstest]
    #[case::fresh_ok(ProbeState::Ok, 60, true)]
    #[case::stale_ok(ProbeState::Ok, 181, false)]
    #[case::fresh_unavailable(ProbeState::Unavailable, 60, false)]
    #[case::fresh_gone(ProbeState::Gone, 60, false)]
    #[case::fresh_moved(ProbeState::Moved, 60, false)]
    fn serves_only_a_fresh_ok(
        #[case] state: ProbeState,
        #[case] age_secs: i64,
        #[case] served: bool,
    ) -> anyhow::Result<()> {
        let now = Utc::now();
        let a = artifact(state, now - chrono::Duration::seconds(age_secs))?;
        assert_eq!(
            a.serveable(now, chrono::Duration::seconds(180)).is_ok(),
            served
        );
        Ok(())
    }

    /// `since` moves only on a transition, which is what keeps alerts to one
    /// per change rather than one per probe.
    #[test]
    fn since_moves_only_on_a_transition() -> anyhow::Result<()> {
        let t0 = Utc::now() - chrono::Duration::minutes(10);
        let t1 = t0 + chrono::Duration::minutes(1);
        let t2 = t1 + chrono::Duration::minutes(1);
        let first = artifact(ProbeState::Ok, t0)?.probe;
        let still_ok = ProbeVerdict::next(&first, ProbeState::Ok, t1, None, None);
        assert_eq!(still_ok.since, t0);
        assert_eq!(still_ok.checked_at, t1);
        let moved = ProbeVerdict::next(&still_ok, ProbeState::Moved, t2, None, None);
        assert_eq!(moved.since, t2);
        Ok(())
    }
}
