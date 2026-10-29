//! A benchmark's corpus as a client reads it: the upstream pin, the artifact
//! mgmt repacks it into, and where to download it. The whole pin travels, as it
//! is what says which corpus a result measured.

use serde::{Deserialize, Serialize};

use crate::{NonEmptyString, Sha256};

/// A git object id: 40 lowercase hex characters, for both the commit a pin
/// names and the tree id a probe compares. A tag moves and an abbreviation is
/// ambiguous, so either would make a corpus nobody here owns unreproducible.
#[nutype::nutype(
    validate(regex = r"^[0-9a-f]{40}$"),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Serialize,
        Deserialize,
        AsRef,
        Display
    )
)]
pub struct GitOid(String);

/// A scheme-less `host/path` repository coordinate on an allowed public host.
/// An `http(s)://` paste is normalized away so one upstream has one spelling.
#[nutype::nutype(
    sanitize(with = strip_http_scheme),
    validate(predicate = is_repository_coordinate),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Serialize,
        Deserialize,
        AsRef,
        Display
    )
)]
pub struct DatasetRepositoryUrl(String);

/// Other schemes are left intact so validation can reject them.
fn strip_http_scheme(raw: String) -> String {
    let s = raw.trim();
    ["https://", "http://"]
        .into_iter()
        .find_map(|scheme| s.strip_prefix(scheme))
        .unwrap_or(s)
        .to_owned()
}

/// The server fetches every pin, so an open host list is an SSRF. A host joins
/// only if it serves a pin with no redirect (gitlab.com 301s a URL without `.git`).
const REPOSITORY_HOSTS: &[&str] = &["github.com"];

fn is_repository_coordinate(s: &str) -> bool {
    s.split_once('/')
        .is_some_and(|(host, path)| REPOSITORY_HOSTS.contains(&host) && !path.is_empty())
        && !s.chars().any(char::is_whitespace)
}

/// A path relative to a repository root, [`crate::RepoSubpath`]'s predicate
/// character for character.
#[nutype::nutype(
    sanitize(trim),
    validate(predicate = is_dataset_path),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Serialize,
        Deserialize,
        AsRef,
        Display
    )
)]
pub struct DatasetPath(String);

fn is_dataset_path(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && s.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
}

/// The layout a corpus is repacked into, named for the benchmark it serves.
/// Closed, not a string: an unreadable layout fails at the catalog, not
/// half-way through a three-hour traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::Display)]
pub enum CorpusFormat {
    /// An uncompressed tar of `manifest.json`, `samples.jsonl` and
    /// content-addressed `media/`.
    #[serde(rename = "vl-spec-decode-corpus/1")]
    #[strum(serialize = "vl-spec-decode-corpus/1")]
    VlSpecDecodeCorpusV1,
    /// An uncompressed tar of `manifest.json`, `samples.jsonl` (one eval
    /// prompt per line, each image part naming a media file) and
    /// content-addressed `media/`: a `vlm_eval`'s prompts with every image
    /// stored once ([`crate::VlmEvalCorpusSample`]).
    #[serde(rename = "vlm-eval-corpus/1")]
    #[strum(serialize = "vlm-eval-corpus/1")]
    VlmEvalCorpusV1,
    /// The same layout for an `asr_eval`'s prompts: each audio part names a
    /// `media/<sha256>.wav` file, and a line keeps the sample's
    /// `chat_template_kwargs`. A corpus of one format holds none of the other's
    /// media parts.
    #[serde(rename = "asr-eval-corpus/1")]
    #[strum(serialize = "asr-eval-corpus/1")]
    AsrEvalCorpusV1,
}

/// The server-side reader of one upstream's layout, versioned. Opaque to a
/// client — it names how the artifact was produced, not how to read it — but
/// carried so a result's provenance is whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::Display)]
pub enum DatasetAdapter {
    /// MMSpec's `mmspec.jsonl` + `images/`.
    #[serde(rename = "mmspec@1")]
    #[strum(serialize = "mmspec@1")]
    MmspecV1,
    /// The evals server's `/samples` for one eval, dataset version and prompt
    /// profile, each `data:` image or `input_audio` clip moved into `media/`.
    #[serde(rename = "evals-server@1")]
    #[strum(serialize = "evals-server@1")]
    EvalsServerV1,
}

/// Where a corpus lives upstream, pinned. Tagged by `source`, like the model
/// and runtime sources a plan carries.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum DatasetUpstream {
    /// A git repository at one commit, narrowed to `subpath`.
    Git {
        url: DatasetRepositoryUrl,
        revision: GitOid,
        /// Subdirectory the corpus lives in; absent = repository root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subpath: Option<DatasetPath>,
        /// Git tree id of `revision:subpath`, which the server's probe reads
        /// without fetching a blob.
        identity: GitOid,
        /// Digest over the fetched files, recomputed by the server's build.
        digest: Sha256,
        /// Recorded, not interpreted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        license: Option<String>,
    },
    /// The prompt samples the deployment's evals server serves for one eval,
    /// dataset version and prompt profile. The pin names content, not a host:
    /// mgmt reaches the evals server it is configured with.
    EvalsServer {
        /// The eval the server keys the samples by (`mmstar`, `pope`, …).
        eval_id: NonEmptyString,
        dataset_version: NonEmptyString,
        /// The model family's wording; absent = the dataset's own prompts. An
        /// asr_eval's dataset holds no prompt, so its pin always names one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_profile: Option<NonEmptyString>,
        /// The digest the evals server reports for those samples
        /// (`GET …/samples/digest`, the hex after `sha256:`): what the probe
        /// compares and the build recomputes from every sample it packs.
        identity: Sha256,
        /// Recorded, not interpreted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        license: Option<String>,
    },
}

impl DatasetUpstream {
    /// Identity string for the pinned snapshot: `url@revision[:subpath]` for
    /// git, `evals-server:eval/version[?prompt_profile=p]` for the evals server.
    pub fn reference(&self) -> String {
        match self {
            Self::Git {
                url,
                revision,
                subpath: Some(subpath),
                ..
            } => format!("{url}@{revision}:{subpath}"),
            Self::Git {
                url,
                revision,
                subpath: None,
                ..
            } => format!("{url}@{revision}"),
            Self::EvalsServer {
                eval_id,
                dataset_version,
                prompt_profile: Some(profile),
                ..
            } => format!("evals-server:{eval_id}/{dataset_version}?prompt_profile={profile}"),
            Self::EvalsServer {
                eval_id,
                dataset_version,
                prompt_profile: None,
                ..
            } => format!("evals-server:{eval_id}/{dataset_version}"),
        }
    }

    pub fn license(&self) -> Option<&str> {
        match self {
            Self::Git { license, .. } | Self::EvalsServer { license, .. } => license.as_deref(),
        }
    }
}

/// A benchmark's corpus, as the catalog pins it and the server serves it.
/// No `deny_unknown_fields`: a field a newer server adds must not make the
/// definition unreadable to a client.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Dataset {
    pub format: CorpusFormat,
    pub adapter: DatasetAdapter,
    /// sha256 of the packed tar: what a client verifies and submits back as
    /// `observation_dataset_digest`.
    pub digest: Sha256,
    pub bytes: u64,
    /// The one subset of the upstream this corpus keeps, as the adapter names
    /// it (`GQA`, `CharXiv`, …); absent = the whole upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subset: Option<String>,
    pub upstream: DatasetUpstream,
    /// Where to download the packed tar: a server-relative path
    /// (`/datasets/{benchmark_id}`) the server adds on the way out. Absent in a
    /// catalog entry itself; a run without one fails then, not at parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<NonEmptyString>,
}

/// The MMSpec GQA corpus object as the server serves it, `url` included.
#[cfg(test)]
pub(crate) fn mmspec_corpus() -> serde_json::Value {
    serde_json::json!({
        "format": "vl-spec-decode-corpus/1",
        "adapter": "mmspec@1",
        "digest": "1862eedc832710f6c4776472d00d5ee4aa0a16d85d6a9c433398109f90239810",
        "bytes": 15955456,
        "subset": "GQA",
        "upstream": {
            "source": "git",
            "url": "github.com/killthefullmoon/MMSpec",
            "revision": "290486861eabd746075ca776adb66175636c1fc0",
            "subpath": "dataset/MMSpec/test",
            "identity": "2923603292c7dd463ad01a119f57965817778fc3",
            "digest": "2bd3c7b41707c10d91726e90545b9ec5d741ac947637c90e2f9d5fe6321fa8f7",
            "license": "Apache-2.0"
        },
        "url": "/datasets/vl_spec_decode_mmspec_test_gqa_k8"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVISION: &str = "290486861eabd746075ca776adb66175636c1fc0";
    const IDENTITY: &str = "2923603292c7dd463ad01a119f57965817778fc3";
    const TREE_DIGEST: &str = "2bd3c7b41707c10d91726e90545b9ec5d741ac947637c90e2f9d5fe6321fa8f7";

    /// The fixture's upstream pin.
    fn mmspec_upstream(
        subpath: Option<&str>,
        license: Option<&str>,
    ) -> anyhow::Result<DatasetUpstream> {
        Ok(DatasetUpstream::Git {
            url: DatasetRepositoryUrl::try_new("github.com/killthefullmoon/MMSpec".to_owned())?,
            revision: GitOid::try_new(REVISION.to_owned())?,
            subpath: subpath
                .map(|s| DatasetPath::try_new(s.to_owned()))
                .transpose()?,
            identity: GitOid::try_new(IDENTITY.to_owned())?,
            digest: Sha256::try_new(TREE_DIGEST.to_owned())?,
            license: license.map(str::to_owned),
        })
    }

    /// The subpath is part of the pin, so two corpora from one commit stay apart.
    #[rstest::rstest]
    #[case::subpath(
        Some("dataset/MMSpec/test"),
        "github.com/killthefullmoon/MMSpec@290486861eabd746075ca776adb66175636c1fc0:dataset/MMSpec/test"
    )]
    #[case::repository_root(
        None,
        "github.com/killthefullmoon/MMSpec@290486861eabd746075ca776adb66175636c1fc0"
    )]
    fn an_upstream_reference_names_the_pinned_snapshot(
        #[case] subpath: Option<&str>,
        #[case] reference: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(mmspec_upstream(subpath, None)?.reference(), reference);
        Ok(())
    }

    /// Tagged by `source`; an absent subpath or license is omitted, not null.
    #[test]
    fn a_bare_git_upstream_omits_its_optional_keys() -> anyhow::Result<()> {
        let upstream = mmspec_upstream(None, None)?;
        let json = serde_json::to_value(&upstream)?;
        assert_eq!(
            json,
            serde_json::json!({
                "source": "git",
                "url": "github.com/killthefullmoon/MMSpec",
                "revision": REVISION,
                "identity": IDENTITY,
                "digest": TREE_DIGEST,
            })
        );
        assert_eq!(serde_json::from_value::<DatasetUpstream>(json)?, upstream);
        assert_eq!(upstream.license(), None);
        Ok(())
    }

    /// The object the server documents, verbatim, including the `url` it
    /// stamps on after reading the catalog.
    #[test]
    fn the_documented_dataset_object_round_trips() -> anyhow::Result<()> {
        let dataset: Dataset = serde_json::from_value(mmspec_corpus())?;
        assert_eq!(dataset.format, CorpusFormat::VlSpecDecodeCorpusV1);
        assert_eq!(dataset.adapter, DatasetAdapter::MmspecV1);
        assert_eq!(dataset.bytes, 15_955_456);
        assert_eq!(dataset.subset.as_deref(), Some("GQA"));
        assert_eq!(
            dataset.url.as_ref().map(AsRef::as_ref),
            Some("/datasets/vl_spec_decode_mmspec_test_gqa_k8")
        );
        assert_eq!(
            dataset.upstream,
            mmspec_upstream(Some("dataset/MMSpec/test"), Some("Apache-2.0"))?
        );
        assert_eq!(dataset.upstream.license(), Some("Apache-2.0"));
        assert_eq!(
            serde_json::from_str::<Dataset>(&serde_json::to_string(&dataset)?)?,
            dataset
        );
        Ok(())
    }

    /// A catalog entry has no `url` — the server adds one. Both shapes read.
    #[test]
    fn a_catalog_entry_parses_without_a_url() -> anyhow::Result<()> {
        let mut value = mmspec_corpus();
        value
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("the fixture is an object"))?
            .remove("url");
        assert!(serde_json::from_value::<Dataset>(value)?.url.is_none());
        Ok(())
    }

    /// A field a newer server adds must not cost a client the definition.
    #[test]
    fn an_unknown_field_is_tolerated() -> anyhow::Result<()> {
        let mut value = mmspec_corpus();
        value["something_newer"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Dataset>(value).is_ok());
        Ok(())
    }

    /// A format or adapter this build cannot read fails at the catalog, not
    /// three hours into a traversal.
    #[rstest::rstest]
    #[case("format", "vl-spec-decode-corpus/2")]
    #[case("adapter", "mmspec@2")]
    fn an_unreadable_corpus_is_rejected(#[case] field: &str, #[case] value: &str) {
        let mut json = mmspec_corpus();
        json[field] = serde_json::json!(value);
        assert!(serde_json::from_value::<Dataset>(json).is_err());
    }

    /// Each format reads and prints as its name; a near miss is not one.
    #[rstest::rstest]
    #[case::vl_spec_decode("vl-spec-decode-corpus/1", Some(CorpusFormat::VlSpecDecodeCorpusV1))]
    #[case::vlm_eval("vlm-eval-corpus/1", Some(CorpusFormat::VlmEvalCorpusV1))]
    #[case::asr_eval("asr-eval-corpus/1", Some(CorpusFormat::AsrEvalCorpusV1))]
    #[case::asr_eval_v2("asr-eval-corpus/2", None)]
    #[case::generic("eval-corpus/1", None)]
    fn a_corpus_format_is_its_name(#[case] name: &str, #[case] format: Option<CorpusFormat>) {
        let parsed = serde_json::from_value::<CorpusFormat>(serde_json::json!(name)).ok();
        assert_eq!(parsed, format);
        if let Some(format) = format {
            assert_eq!(format.to_string(), name);
        }
    }

    /// A moving pin is not a pin: only a full commit sha is accepted.
    #[rstest::rstest]
    #[case("main")]
    #[case("v1.0")]
    #[case("2904868")]
    fn a_moving_revision_is_not_a_pin(#[case] revision: &str) {
        assert!(GitOid::try_new(revision.to_owned()).is_err());
    }

    /// A pasted browser URL names the same upstream as the coordinate.
    #[rstest::rstest]
    #[case("https://github.com/acme/corpus")]
    #[case("http://github.com/acme/corpus")]
    #[case("github.com/acme/corpus")]
    fn a_repository_url_normalizes_its_scheme_away(#[case] raw: &str) -> anyhow::Result<()> {
        assert_eq!(
            DatasetRepositoryUrl::try_new(raw.to_owned())?.as_ref(),
            "github.com/acme/corpus"
        );
        Ok(())
    }

    /// The server fetches whatever host a pin names, so only the listed public
    /// hosts pass: no internal name, IP literal, port or userinfo.
    #[rstest::rstest]
    #[case("ssh://github.com/acme/corpus")]
    #[case("/acme/corpus")]
    #[case("acme")]
    #[case("github.com/acme corpus")]
    #[case("github.com/")]
    #[case("github.example/acme/corpus")]
    #[case("gitlab.internal/acme/corpus")]
    #[case("gitlab.com/acme/corpus")]
    #[case("169.254.169.254/latest/meta-data")]
    #[case("127.0.0.1/acme/corpus")]
    #[case("[::1]/acme/corpus")]
    #[case("github.com:8443/acme/corpus")]
    #[case("user:pass@github.com/acme/corpus")]
    #[case("github.com@10.0.0.1/acme/corpus")]
    #[case("https://localhost/acme/corpus")]
    fn an_unusable_repository_url_is_rejected(#[case] raw: &str) {
        assert!(DatasetRepositoryUrl::try_new(raw.to_owned()).is_err());
    }

    /// The subpath reaches a `git` argv and a local join on the server side, so
    /// an escape is refused here too rather than trusted across the wire.
    #[rstest::rstest]
    #[case("/absolute")]
    #[case("../escape")]
    #[case("dataset/../escape")]
    #[case("")]
    fn an_escaping_subpath_is_rejected(#[case] raw: &str) {
        assert!(DatasetPath::try_new(raw.to_owned()).is_err());
    }
}
