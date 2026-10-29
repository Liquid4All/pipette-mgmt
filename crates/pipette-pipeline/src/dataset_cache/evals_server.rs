//! `evals-server@1`: the evals server's `/samples` for one eval, dataset
//! version and prompt profile, repacked as the entry's format with each media
//! part moved into a content-addressed `media/` file (`vlm-eval-corpus/1`: each
//! `data:` image; `asr-eval-corpus/1`: each `input_audio` clip, as a `.wav`);
//! and the probe, which reads only `/samples/digest`.
//!
//! The build proves the corpus lossless before anything is stored: it hashes
//! every sample as served and requires the evals server's digest, then reads
//! the packed tar back, rebuilds every sample from it, and requires the same
//! digest again. A client checks that digest once more before it runs.
//!
//! The body is parsed as it downloads, one sample at a time, and never stored:
//! an ASR set's is gigabytes of base64 (SPGISpeech's about 15 GB). Scratch holds
//! the media and the packed tar.
//!
//! The parser runs on a blocking thread. A build past its deadline cancels it
//! and joins it before returning, so nothing writes to the caller's scratch
//! once the caller drops it, and the next build's disk is not shared with a
//! stale one; a body that stops arriving fails after [`super::BODY_STALL`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use futures::StreamExt;
use serde::de::{
    DeserializeSeed, Deserializer, Error as _, IgnoredAny, MapAccess, SeqAccess, Visitor,
};
use tokio_util::io::{StreamReader, SyncIoBridge};
use tokio_util::sync::CancellationToken;

use pipette_plan_types::{
    CorpusFormat, Dataset, DatasetUpstream, EvalSample, SamplesDigest, Sha256,
    VlmEvalCorpusManifest, VlmEvalCorpusSample, parse_samples_digest,
};

use super::format;

/// The header `/samples` carries its body's digest in.
const DIGEST_HEADER: &str = "x-samples-digest";

/// What a probe read off the evals server.
pub(super) enum Reached {
    Digest(Sha256),
    /// The evals server answers, but no longer serves these samples.
    Gone(String),
    Unavailable(String),
}

/// What a build fetched and packed.
pub(super) struct Built {
    /// The digest every sample hashed to, as served and as rebuilt.
    pub(super) identity: Sha256,
    pub(super) digest: Sha256,
    pub(super) bytes: u64,
    pub(super) samples: u32,
    pub(super) media: u32,
    /// The `/samples` body the corpus replaces, for the record.
    pub(super) served_bytes: u64,
}

/// The corpus formats this adapter packs: the eval prompt corpora.
fn eval_corpus_format(dataset: &Dataset) -> anyhow::Result<CorpusFormat> {
    match dataset.format {
        format @ (CorpusFormat::VlmEvalCorpusV1 | CorpusFormat::AsrEvalCorpusV1) => Ok(format),
        format @ CorpusFormat::VlSpecDecodeCorpusV1 => {
            anyhow::bail!("the evals server's samples do not pack as {format}")
        }
    }
}

/// `{base}/evals/{eval}/datasets/{version}/samples[/digest][?prompt_profile=]`.
fn samples_url(
    base: &str,
    upstream: &DatasetUpstream,
    digest: bool,
) -> anyhow::Result<reqwest::Url> {
    let DatasetUpstream::EvalsServer {
        eval_id,
        dataset_version,
        prompt_profile,
        ..
    } = upstream
    else {
        anyhow::bail!("{} is not an evals-server upstream", upstream.reference());
    };
    if let Some(bad) = [eval_id.as_ref(), dataset_version.as_ref()]
        .into_iter()
        .find(|s| !crate::http_client::is_plain_path_segment(s))
    {
        anyhow::bail!("{bad:?} cannot be an evals server url path segment");
    }
    let mut url = reqwest::Url::parse(base)
        .with_context(|| format!("evals_server_url {base:?} is not a URL"))?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|()| anyhow::anyhow!("evals_server_url {base:?} cannot carry a path"))?;
        path.pop_if_empty().extend([
            "evals",
            eval_id.as_ref(),
            "datasets",
            dataset_version.as_ref(),
            "samples",
        ]);
        if digest {
            path.push("digest");
        }
    }
    if let Some(profile) = prompt_profile {
        url.query_pairs_mut()
            .append_pair("prompt_profile", profile.as_ref());
    }
    Ok(url)
}

/// Enough of an error body to recognise it, never a whole one.
fn excerpt(body: &str) -> String {
    body.chars().take(300).collect()
}

/// `GET …/samples/digest`'s body, of which the probe reads the digest.
#[derive(serde::Deserialize)]
struct DigestResponse {
    digest: String,
}

/// The digest the evals server reports for the pinned samples, in one small
/// request.
pub(super) async fn probe(
    http: &reqwest::Client,
    base: &str,
    upstream: &DatasetUpstream,
) -> Reached {
    let url = match samples_url(base, upstream, true) {
        Ok(url) => url,
        Err(e) => return Reached::Unavailable(format!("{e:#}")),
    };
    let response = match http.get(url).send().await {
        Ok(response) => response,
        Err(e) => return Reached::Unavailable(format!("evals server unreachable: {e}")),
    };
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    // 404: no such eval or dataset version; 400: no such prompt profile for it.
    // Either way the evals server no longer publishes what the pin names.
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::BAD_REQUEST {
        return Reached::Gone(format!(
            "evals server answers {status} for {}: {}",
            upstream.reference(),
            excerpt(&body)
        ));
    }
    if !status.is_success() {
        return Reached::Unavailable(format!("evals server answers {status}: {}", excerpt(&body)));
    }
    match serde_json::from_str::<DigestResponse>(&body)
        .ok()
        .and_then(|r| parse_samples_digest(&r.digest))
    {
        Some(digest) => Reached::Digest(digest),
        None => Reached::Unavailable(format!(
            "evals server's digest response is unreadable: {}",
            excerpt(&body)
        )),
    }
}

/// Fetch the pinned samples, packing each into `scratch` as it arrives, then
/// pack them into `tar`. The manifest records the upstream as observed, so a
/// correctly pinned entry packs to its pinned digest.
///
/// `None` when `deadline` passed first. The blocking parser is then cancelled
/// and joined before this returns: nothing writes to `scratch` afterwards. The
/// body's reads fail as soon as it is cancelled, and the parser checks between
/// samples and before each later phase. A body that sends no byte for `stall`
/// fails the build.
pub(super) async fn materialize(
    http: &reqwest::Client,
    base: &str,
    scratch: &Path,
    dataset: &Dataset,
    tar: &Path,
    deadline: Duration,
    stall: Duration,
) -> anyhow::Result<Option<Built>> {
    let format = eval_corpus_format(dataset)?;
    let expires = tokio::time::Instant::now()
        .checked_add(deadline)
        .ok_or_else(|| anyhow::anyhow!("the build deadline overflows the clock"))?;
    let cancel = CancellationToken::new();
    // A build dropped by its caller stops its parser too, though unjoined.
    let _cancel_on_drop = cancel.clone().drop_guard();
    let url = samples_url(base, &dataset.upstream, false)?;
    let Ok(downloaded) =
        tokio::time::timeout_at(expires, download(http, url, cancel.clone(), stall)).await
    else {
        return Ok(None);
    };
    let (reported, body) = downloaded?;
    let (dataset, scratch, tar) = (dataset.clone(), scratch.to_path_buf(), tar.to_path_buf());
    let parser = cancel.clone();
    let mut packing = tokio::task::spawn_blocking(move || {
        let mut body = Counted::new(BufReader::new(body));
        pack(
            &mut body, reported, format, &dataset, &scratch, &tar, &parser,
        )
    });
    if let Ok(packed) = tokio::time::timeout_at(expires, &mut packing).await {
        return packed?.map(Some);
    }
    cancel.cancel();
    // Its error is the cancellation's; the deadline is the cause.
    let _ = packing.await;
    Ok(None)
}

/// A blocking reader over the response body, for the parser to pull from on a
/// blocking thread while the body is still arriving.
type BodyReader = SyncIoBridge<Box<dyn tokio::io::AsyncRead + Send + Unpin>>;

/// The `/samples` body as a reader, and the digest its header reports. The
/// reader fails once `cancel` fires, or when the body sends no byte for
/// `stall`; the headers are bounded by the build's deadline instead, as the
/// evals server may first make a pass over the set to learn its digest.
async fn download(
    http: &reqwest::Client,
    url: reqwest::Url,
    cancel: CancellationToken,
    stall: Duration,
) -> anyhow::Result<(Sha256, BodyReader)> {
    let response = http
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("evals server unreachable at {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "evals server answers {status} for {url}: {}",
            excerpt(&body)
        );
    }
    // Required: it is the evals server's own digest of the full body. Hashed
    // here alone, a field the typed samples do not carry (and so drop) would
    // go unnoticed, and the corpus would be pinned without it.
    let value = response.headers().get(DIGEST_HEADER).with_context(|| {
        format!(
            "the evals server sent no {DIGEST_HEADER}; it predates the digest endpoint, or \
             {url} is not its samples route"
        )
    })?;
    let reported = value
        .to_str()
        .ok()
        .and_then(parse_samples_digest)
        .with_context(|| format!("unreadable {DIGEST_HEADER} header: {value:?}"))?;
    // Ends at the first error, and stays ended (fused): the reader may poll
    // again after either.
    let stream = futures::stream::unfold(
        Some((response.bytes_stream(), cancel)),
        move |state| async move {
            let (mut chunks, cancel) = state?;
            let next = tokio::select! {
                () = cancel.cancelled() => Err(std::io::Error::other("the build was cancelled")),
                next = tokio::time::timeout(stall, chunks.next()) => match next {
                    Err(_) => Err(std::io::Error::other(format!(
                        "the samples download stalled: no byte for {}s",
                        stall.as_secs()
                    ))),
                    Ok(None) => return None,
                    Ok(Some(chunk)) => chunk.map_err(|e| {
                        std::io::Error::other(format!("the samples download broke off: {e}"))
                    }),
                },
            };
            let state = next.is_ok().then_some((chunks, cancel));
            Some((next, state))
        },
    );
    let reader: Box<dyn tokio::io::AsyncRead + Send + Unpin> =
        Box::new(StreamReader::new(Box::pin(stream.fuse())));
    Ok((reported, SyncIoBridge::new(reader)))
}

/// A reader that counts what passes through it: the served body's size, for
/// the record, now that the body is never stored.
struct Counted<R> {
    inner: R,
    bytes: u64,
}

impl<R> Counted<R> {
    fn new(inner: R) -> Self {
        Self { inner, bytes: 0 }
    }
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }
}

/// Repack the body as it is read. Memory holds one sample and the corpus
/// index, never the body: POPE's is over a gigabyte, SPGISpeech's fifteen.
fn pack<R: Read>(
    body: &mut Counted<R>,
    reported: Sha256,
    format: CorpusFormat,
    dataset: &Dataset,
    scratch: &Path,
    tar: &Path,
    cancel: &CancellationToken,
) -> anyhow::Result<Built> {
    let go_on = || {
        anyhow::ensure!(!cancel.is_cancelled(), "the build was cancelled");
        Ok(())
    };
    std::fs::create_dir_all(scratch.join("media"))?;
    let mut digest = SamplesDigest::new();
    let mut lines = Vec::new();
    let mut media = BTreeMap::<String, PathBuf>::new();
    let mut ids = HashSet::new();
    for_each_sample(&mut *body, |sample| {
        go_on()?;
        anyhow::ensure!(
            ids.insert(sample.id.clone()),
            "the evals server served sample id {} twice",
            sample.id
        );
        digest.update(&sample)?;
        let corpus = VlmEvalCorpusSample::from_eval_sample_in(format, &sample, |path, bytes| {
            if !media.contains_key(path) {
                let file = scratch.join(path);
                std::fs::write(&file, bytes)?;
                media.insert(path.to_owned(), file);
            }
            Ok(())
        })?;
        serde_json::to_writer(&mut lines, &corpus)?;
        lines.push(b'\n');
        Ok(())
    })?;
    let samples = u32::try_from(digest.samples())?;
    anyhow::ensure!(samples > 0, "the evals server served no samples");
    let identity = digest.finish()?;
    anyhow::ensure!(
        reported == identity,
        "the evals server reports digest {reported} for samples that hash to {identity}: \
         the samples carry something this build does not read"
    );

    let mut upstream = dataset.upstream.clone();
    if let DatasetUpstream::EvalsServer {
        identity: observed, ..
    } = &mut upstream
    {
        *observed = identity.clone();
    }
    let manifest = VlmEvalCorpusManifest {
        format,
        adapter: dataset.adapter,
        upstream,
        samples,
        media: u32::try_from(media.len())?,
    };
    let mut manifest_bytes = serde_json::to_vec(&manifest)?;
    manifest_bytes.push(b'\n');
    go_on()?;
    let (tar_digest, bytes) = format::write_corpus(&manifest_bytes, &lines, &media, tar)?;

    go_on()?;
    let rebuilt = rebuilt_digest(tar, format)?;
    anyhow::ensure!(
        rebuilt == identity,
        "the packed corpus rebuilds samples hashing to {rebuilt}, not the {identity} the \
         evals server served: it would not give a client the prompts the scorer grades"
    );
    Ok(Built {
        identity,
        digest: tar_digest,
        bytes,
        samples,
        media: manifest.media,
        served_bytes: body.bytes,
    })
}

/// Read a packed corpus back as a client does and hash the samples it
/// rebuilds. The media stay in the tar: each is read at its offset when a
/// sample names it, so memory holds the index and one image or clip, not the
/// corpus.
fn rebuilt_digest(tar: &Path, format: CorpusFormat) -> anyhow::Result<Sha256> {
    let mut archive = tar::Archive::new(BufReader::new(std::fs::File::open(tar)?));
    let mut index = None;
    let mut media = HashMap::<String, (u64, u64)>::new();
    archive.entries()?.try_for_each(|entry| {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        match name.as_str() {
            "samples.jsonl" => {
                let mut body = Vec::new();
                entry.read_to_end(&mut body)?;
                index = Some(body);
            }
            "manifest.json" => {}
            path if path.starts_with("media/") => {
                media.insert(name, (entry.raw_file_position(), entry.size()));
            }
            other => anyhow::bail!("the packed corpus holds an unexpected entry {other:?}"),
        }
        anyhow::Ok(())
    })?;
    let index = index.context("the packed corpus has no samples.jsonl")?;
    let mut file = std::fs::File::open(tar)?;
    let mut read_media = |path: &str| -> std::io::Result<Vec<u8>> {
        let (offset, size) = *media
            .get(path)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, path))?;
        let size = usize::try_from(size).map_err(std::io::Error::other)?;
        let mut body = vec![0_u8; size];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut body)?;
        Ok(body)
    };
    let mut digest = SamplesDigest::new();
    index
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .try_for_each(|line| {
            let corpus: VlmEvalCorpusSample = serde_json::from_slice(line)?;
            digest.update(&corpus.to_eval_sample_in(format, &mut read_media)?)?;
            anyhow::Ok(())
        })?;
    Ok(digest.finish()?)
}

/// Call `each` on every sample of a `{"samples": [...]}` body as it is read.
fn for_each_sample(
    reader: impl Read,
    mut each: impl FnMut(EvalSample) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut failure = None;
    let mut saw_samples = false;
    let mut de = serde_json::Deserializer::from_reader(reader);
    let parsed = Body {
        each: &mut each,
        failure: &mut failure,
        saw_samples: &mut saw_samples,
    }
    .deserialize(&mut de);
    if let Some(e) = failure {
        return Err(e);
    }
    parsed.context("the samples response is not the evals server's JSON")?;
    de.end().context("the samples response has trailing data")?;
    anyhow::ensure!(saw_samples, "the samples response has no `samples`");
    Ok(())
}

/// The response object: `samples` is read element by element, any other key
/// skipped.
struct Body<'a, F> {
    each: &'a mut F,
    failure: &'a mut Option<anyhow::Error>,
    saw_samples: &'a mut bool,
}

impl<'de, F: FnMut(EvalSample) -> anyhow::Result<()>> DeserializeSeed<'de> for Body<'_, F> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de, F: FnMut(EvalSample) -> anyhow::Result<()>> Visitor<'de> for Body<'_, F> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an object with a `samples` array")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<String>()? {
            if key == "samples" {
                if *self.saw_samples {
                    return Err(A::Error::custom("duplicate `samples` key"));
                }
                map.next_value_seed(Samples {
                    each: &mut *self.each,
                    failure: &mut *self.failure,
                })?;
                *self.saw_samples = true;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

struct Samples<'a, F> {
    each: &'a mut F,
    failure: &'a mut Option<anyhow::Error>,
}

impl<'de, F: FnMut(EvalSample) -> anyhow::Result<()>> DeserializeSeed<'de> for Samples<'_, F> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de, F: FnMut(EvalSample) -> anyhow::Result<()>> Visitor<'de> for Samples<'_, F> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an array of samples")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(sample) = seq.next_element::<EvalSample>()? {
            if let Err(e) = (self.each)(sample) {
                *self.failure = Some(e);
                return Err(A::Error::custom("stopped at a sample the build refused"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples_of(json: &str) -> anyhow::Result<Vec<String>> {
        let mut ids = Vec::new();
        for_each_sample(json.as_bytes(), |s| {
            ids.push(s.id);
            Ok(())
        })?;
        Ok(ids)
    }

    #[test]
    fn samples_are_read_in_order_and_other_keys_skipped() -> anyhow::Result<()> {
        let ids = samples_of(
            r#"{"note": {"x": [1, 2]}, "samples": [
                {"id": "a", "messages": [{"role": "user", "content": "1?"}]},
                {"id": "b", "messages": [{"role": "user", "content": "2?"}]}
            ], "tail": null}"#,
        )?;
        assert_eq!(ids, ["a", "b"]);
        Ok(())
    }

    #[rstest::rstest]
    #[case::no_samples(r#"{"other": []}"#, "has no `samples`")]
    #[case::not_json("<html>", "not the evals server's JSON")]
    #[case::trailing(r#"{"samples": []} x"#, "trailing data")]
    #[case::bad_sample(r#"{"samples": [{"id": "a"}]}"#, "not the evals server's JSON")]
    fn a_body_that_is_not_samples_is_refused(#[case] body: &str, #[case] names: &str) {
        let error = samples_of(body)
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(error.contains(names), "{error}");
    }

    /// A refusal from the callback is the error reported, not serde's.
    #[test]
    fn the_callbacks_refusal_is_what_surfaces() {
        let error = for_each_sample(
            r#"{"samples": [{"id": "a", "messages": []}]}"#.as_bytes(),
            |_| anyhow::bail!("nope"),
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert_eq!(error, "nope");
    }

    #[rstest::rstest]
    #[case::bare(
        "http://scores:8000",
        None,
        false,
        "http://scores:8000/evals/mmstar/datasets/2026.10.1/samples"
    )]
    #[case::trailing_slash(
        "http://scores:8000/",
        None,
        true,
        "http://scores:8000/evals/mmstar/datasets/2026.10.1/samples/digest"
    )]
    #[case::profile(
        "http://scores:8000",
        Some("qwen3_5"),
        true,
        "http://scores:8000/evals/mmstar/datasets/2026.10.1/samples/digest?prompt_profile=qwen3_5"
    )]
    fn the_url_names_the_pinned_samples(
        #[case] base: &str,
        #[case] profile: Option<&str>,
        #[case] digest: bool,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let upstream: DatasetUpstream = serde_json::from_value(serde_json::json!({
            "source": "evals_server",
            "eval_id": "mmstar",
            "dataset_version": "2026.10.1",
            "prompt_profile": profile,
            "identity": "1".repeat(64),
        }))?;
        assert_eq!(samples_url(base, &upstream, digest)?.as_str(), expected);
        Ok(())
    }

    #[rstest::rstest]
    #[case::dot_dot_eval("..", "2026.10.1")]
    #[case::tab_dot_dot_version("mmstar", ".\t.")]
    fn a_dot_dot_url_segment_is_refused(
        #[case] eval_id: &str,
        #[case] dataset_version: &str,
    ) -> anyhow::Result<()> {
        let upstream: DatasetUpstream = serde_json::from_value(serde_json::json!({
            "source": "evals_server",
            "eval_id": eval_id,
            "dataset_version": dataset_version,
            "identity": "1".repeat(64),
        }))?;
        assert!(samples_url("http://scores:8000", &upstream, true).is_err());
        Ok(())
    }
}
