//! `vlm-eval-corpus/1`: a `vlm_eval`'s prompt samples with every image moved
//! out of its `data:` URI into a content-addressed media file, so an image that
//! several questions share is stored and downloaded once.
//!
//! `asr-eval-corpus/1` is the same layout for an `asr_eval`: every
//! `input_audio` clip moves into a `media/<sha256>.wav` file, and a line keeps
//! the sample's `chat_template_kwargs`. Each format holds only its own media
//! part: a VL corpus no audio, an ASR corpus no image. The functions taking a
//! [`CorpusFormat`] convert either; the others are the VL ones.
//!
//! mgmt packs a corpus from the evals server's `/samples`; a client rebuilds
//! the evals server's samples from it. Both convert with the functions here,
//! and both check what they hold against the samples digest the evals server
//! reports ([`SamplesDigest`]): the build before it stores a corpus, the client
//! before it sends a prompt. A corpus that does not rebuild the exact samples
//! the scorer grades is never stored and never run.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::{
    AudioFormat, ChatMessage, ChatRole, ContentPart, CorpusFormat, DatasetAdapter, DatasetUpstream,
    EvalSample, ImageUrl, InputAudio, MessageContent, Sha256,
};

/// One `samples.jsonl` line: an [`EvalSample`] whose images or clips name media
/// files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VlmEvalCorpusSample {
    pub id: String,
    pub messages: Vec<VlmEvalCorpusMessage>,
    /// The sample's template arguments, as the evals server sent them (an
    /// `asr_eval` Voxtral sample's language). Absent for every other sample,
    /// so a line without them is written as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VlmEvalCorpusMessage {
    pub role: ChatRole,
    pub content: VlmEvalCorpusContent,
}

/// A string for text, parts for a vision message, as [`MessageContent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum VlmEvalCorpusContent {
    Text(String),
    Parts(Vec<VlmEvalCorpusPart>),
}

/// A [`ContentPart`] whose image or clip is a media file rather than inline
/// base64.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VlmEvalCorpusPart {
    Text {
        text: String,
    },
    Image {
        /// Tar-relative, `media/<sha256 of the bytes>.<ext>`.
        path: String,
        /// The `data:` URI's media type, which the rebuilt URI repeats.
        mime: String,
        bytes: u64,
    },
    /// An `asr-eval-corpus/1` clip: an `input_audio` part's file.
    Audio {
        /// Tar-relative, `media/<sha256 of the bytes>.wav`.
        path: String,
        /// The part's container, which the rebuilt part repeats: `wav` only.
        format: AudioFormat,
        bytes: u64,
    },
}

impl VlmEvalCorpusPart {
    /// The part's name in a refusal.
    fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Image { .. } => "image",
            Self::Audio { .. } => "audio",
        }
    }
}

/// `manifest.json`: what the corpus is, where it came from, and its counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VlmEvalCorpusManifest {
    pub format: CorpusFormat,
    pub adapter: DatasetAdapter,
    pub upstream: DatasetUpstream,
    pub samples: u32,
    /// Distinct media files under `media/`.
    pub media: u32,
}

/// Why a sample could not move into or out of a corpus.
#[derive(Debug, thiserror::Error)]
pub enum VlmEvalCorpusError {
    #[error("sample {id}: image URL is not a base64 `data:image/…` URI")]
    NotADataUri { id: String },
    #[error("sample {id}: image media type {mime:?} has no file extension")]
    UnsupportedMime { id: String, mime: String },
    #[error("sample {id}: image is not canonical base64, so it would not rebuild byte for byte")]
    NonCanonicalBase64 { id: String },
    #[error("sample {id}: an {part} part has no place in the {format} format")]
    PartNotInFormat {
        id: String,
        part: &'static str,
        format: CorpusFormat,
    },
    #[error("{format} is not an eval prompt corpus")]
    NotAnEvalCorpus { format: CorpusFormat },
    #[error("sample {id}: audio clip is {format:?}; an asr-eval corpus holds wav clips only")]
    UnsupportedAudioFormat { id: String, format: AudioFormat },
    #[error("sample {id}: audio clip is not base64")]
    NotBase64Audio { id: String },
    #[error(
        "sample {id}: audio clip is not canonical base64, so it would not rebuild byte for byte"
    )]
    NonCanonicalAudioBase64 { id: String },
    #[error("sample {id}: media path {path:?} is not `media/<sha256>.<ext>`")]
    BadMediaPath { id: String, path: String },
    #[error("sample {id}: media {path} holds {actual} bytes, the sample says {expected}")]
    MediaSize {
        id: String,
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("sample {id}: media {path} does not hash to its name")]
    MediaDigest { id: String, path: String },
    #[error("sample {id}: media {path}: {source}")]
    Media {
        id: String,
        path: String,
        source: std::io::Error,
    },
}

impl VlmEvalCorpusSample {
    /// `sample` with each image moved out to `store(path, bytes)`, where `path`
    /// is content-addressed: the same image twice is the same path, so a
    /// caller that already holds it can skip the write.
    pub fn from_eval_sample(
        sample: &EvalSample,
        store: impl FnMut(&str, &[u8]) -> std::io::Result<()>,
    ) -> Result<Self, VlmEvalCorpusError> {
        Self::from_eval_sample_in(CorpusFormat::VlmEvalCorpusV1, sample, store)
    }

    /// [`Self::from_eval_sample`] for a corpus of `format`: a vlm-eval corpus
    /// moves images out and refuses a clip, an asr-eval corpus moves clips out
    /// and refuses an image.
    pub fn from_eval_sample_in(
        format: CorpusFormat,
        sample: &EvalSample,
        mut store: impl FnMut(&str, &[u8]) -> std::io::Result<()>,
    ) -> Result<Self, VlmEvalCorpusError> {
        let media = MediaKind::of(format)?;
        let id = &sample.id;
        let refuse = |part: &'static str| VlmEvalCorpusError::PartNotInFormat {
            id: id.to_owned(),
            part,
            format,
        };
        let messages = sample
            .messages
            .iter()
            .map(|message| {
                let content = match &message.content {
                    MessageContent::Text(text) => VlmEvalCorpusContent::Text(text.clone()),
                    MessageContent::Parts(parts) => VlmEvalCorpusContent::Parts(
                        parts
                            .iter()
                            .map(|part| match part {
                                ContentPart::Text { text } => {
                                    Ok(VlmEvalCorpusPart::Text { text: text.clone() })
                                }
                                ContentPart::ImageUrl { image_url } => match media {
                                    MediaKind::Image => move_out(id, &image_url.url, &mut store),
                                    MediaKind::Audio => Err(refuse("image")),
                                },
                                ContentPart::InputAudio { input_audio } => match media {
                                    MediaKind::Audio => move_out_audio(id, input_audio, &mut store),
                                    MediaKind::Image => Err(refuse("audio")),
                                },
                            })
                            .collect::<Result<_, _>>()?,
                    ),
                };
                Ok(VlmEvalCorpusMessage {
                    role: message.role,
                    content,
                })
            })
            .collect::<Result<_, VlmEvalCorpusError>>()?;
        Ok(Self {
            id: id.clone(),
            messages,
            chat_template_kwargs: sample.chat_template_kwargs.clone(),
        })
    }

    /// The evals server's sample back, each image read through `read(path)`
    /// and checked against its size and its content-addressed name.
    pub fn to_eval_sample(
        &self,
        read: impl FnMut(&str) -> std::io::Result<Vec<u8>>,
    ) -> Result<EvalSample, VlmEvalCorpusError> {
        self.to_eval_sample_in(CorpusFormat::VlmEvalCorpusV1, read)
    }

    /// [`Self::to_eval_sample`] for a line of a corpus of `format`, whose
    /// media part is the only one it may hold.
    pub fn to_eval_sample_in(
        &self,
        format: CorpusFormat,
        mut read: impl FnMut(&str) -> std::io::Result<Vec<u8>>,
    ) -> Result<EvalSample, VlmEvalCorpusError> {
        let media = MediaKind::of(format)?;
        let id = &self.id;
        let refuse = |part: &VlmEvalCorpusPart| VlmEvalCorpusError::PartNotInFormat {
            id: id.to_owned(),
            part: part.kind(),
            format,
        };
        let messages = self
            .messages
            .iter()
            .map(|message| {
                let content = match &message.content {
                    VlmEvalCorpusContent::Text(text) => MessageContent::Text(text.clone()),
                    VlmEvalCorpusContent::Parts(parts) => MessageContent::Parts(
                        parts
                            .iter()
                            .map(|part| match part {
                                VlmEvalCorpusPart::Text { text } => {
                                    Ok(ContentPart::Text { text: text.clone() })
                                }
                                VlmEvalCorpusPart::Image { path, mime, bytes }
                                    if media == MediaKind::Image =>
                                {
                                    move_in(id, path, mime, *bytes, &mut read)
                                }
                                VlmEvalCorpusPart::Audio {
                                    path,
                                    format: audio,
                                    bytes,
                                } if media == MediaKind::Audio => {
                                    move_in_audio(id, path, *audio, *bytes, &mut read)
                                }
                                other @ (VlmEvalCorpusPart::Image { .. }
                                | VlmEvalCorpusPart::Audio { .. }) => Err(refuse(other)),
                            })
                            .collect::<Result<_, _>>()?,
                    ),
                };
                Ok(ChatMessage {
                    role: message.role,
                    content,
                })
            })
            .collect::<Result<_, VlmEvalCorpusError>>()?;
        Ok(EvalSample {
            id: id.clone(),
            messages,
            chat_template_kwargs: self.chat_template_kwargs.clone(),
        })
    }

    /// Every image or audio part's media path, in order, repeats included.
    pub fn media_paths(&self) -> impl Iterator<Item = &str> {
        self.messages
            .iter()
            .flat_map(|message| match &message.content {
                VlmEvalCorpusContent::Text(_) => [].iter(),
                VlmEvalCorpusContent::Parts(parts) => parts.iter(),
            })
            .filter_map(|part| match part {
                VlmEvalCorpusPart::Image { path, .. } | VlmEvalCorpusPart::Audio { path, .. } => {
                    Some(path.as_str())
                }
                VlmEvalCorpusPart::Text { .. } => None,
            })
    }
}

/// The one media part a corpus format holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Image,
    Audio,
}

impl MediaKind {
    fn of(format: CorpusFormat) -> Result<Self, VlmEvalCorpusError> {
        match format {
            CorpusFormat::VlmEvalCorpusV1 => Ok(Self::Image),
            CorpusFormat::AsrEvalCorpusV1 => Ok(Self::Audio),
            CorpusFormat::VlSpecDecodeCorpusV1 => {
                Err(VlmEvalCorpusError::NotAnEvalCorpus { format })
            }
        }
    }
}

/// The one clip container an asr-eval corpus holds, and its file extension.
const AUDIO_FORMAT: AudioFormat = AudioFormat::Wav;
const AUDIO_EXTENSION: &str = "wav";

const DATA_PREFIX: &str = "data:";
const BASE64_MARKER: &str = ";base64,";

/// The part a `data:` image becomes, after handing its bytes to `store`.
fn move_out(
    id: &str,
    url: &str,
    store: &mut impl FnMut(&str, &[u8]) -> std::io::Result<()>,
) -> Result<VlmEvalCorpusPart, VlmEvalCorpusError> {
    let not_a_data_uri = || VlmEvalCorpusError::NotADataUri { id: id.to_owned() };
    let (mime, payload) = url
        .strip_prefix(DATA_PREFIX)
        .and_then(|rest| rest.split_once(BASE64_MARKER))
        .ok_or_else(not_a_data_uri)?;
    let ext = extension(mime).ok_or_else(|| VlmEvalCorpusError::UnsupportedMime {
        id: id.to_owned(),
        mime: mime.to_owned(),
    })?;
    let bytes = STANDARD.decode(payload).map_err(|_| not_a_data_uri())?;
    // Decoding tolerates nothing here, but a URI is only reproducible if
    // re-encoding its bytes gives back the very string the scorer sent.
    if STANDARD.encode(&bytes) != payload {
        return Err(VlmEvalCorpusError::NonCanonicalBase64 { id: id.to_owned() });
    }
    let path = format!("media/{}.{ext}", hex::encode(sha2::Sha256::digest(&bytes)));
    store(&path, &bytes).map_err(|source| VlmEvalCorpusError::Media {
        id: id.to_owned(),
        path: path.clone(),
        source,
    })?;
    Ok(VlmEvalCorpusPart::Image {
        path,
        mime: mime.to_owned(),
        bytes: bytes.len() as u64,
    })
}

/// The `data:` image a media part stands for.
fn move_in(
    id: &str,
    path: &str,
    mime: &str,
    expected: u64,
    read: &mut impl FnMut(&str) -> std::io::Result<Vec<u8>>,
) -> Result<ContentPart, VlmEvalCorpusError> {
    let named = media_digest(path).ok_or_else(|| VlmEvalCorpusError::BadMediaPath {
        id: id.to_owned(),
        path: path.to_owned(),
    })?;
    let bytes = read(path).map_err(|source| VlmEvalCorpusError::Media {
        id: id.to_owned(),
        path: path.to_owned(),
        source,
    })?;
    if bytes.len() as u64 != expected {
        return Err(VlmEvalCorpusError::MediaSize {
            id: id.to_owned(),
            path: path.to_owned(),
            expected,
            actual: bytes.len() as u64,
        });
    }
    if hex::encode(sha2::Sha256::digest(&bytes)) != named {
        return Err(VlmEvalCorpusError::MediaDigest {
            id: id.to_owned(),
            path: path.to_owned(),
        });
    }
    Ok(ContentPart::ImageUrl {
        image_url: ImageUrl {
            url: format!(
                "{DATA_PREFIX}{mime}{BASE64_MARKER}{}",
                STANDARD.encode(&bytes)
            ),
        },
    })
}

/// The part a clip becomes, after handing its bytes to `store`.
fn move_out_audio(
    id: &str,
    input_audio: &InputAudio,
    store: &mut impl FnMut(&str, &[u8]) -> std::io::Result<()>,
) -> Result<VlmEvalCorpusPart, VlmEvalCorpusError> {
    if input_audio.format != AUDIO_FORMAT {
        return Err(VlmEvalCorpusError::UnsupportedAudioFormat {
            id: id.to_owned(),
            format: input_audio.format,
        });
    }
    let bytes = STANDARD
        .decode(&input_audio.data)
        .map_err(|_| VlmEvalCorpusError::NotBase64Audio { id: id.to_owned() })?;
    // As for an image: only a canonical encoding rebuilds the very string the
    // scorer hashed.
    if STANDARD.encode(&bytes) != input_audio.data {
        return Err(VlmEvalCorpusError::NonCanonicalAudioBase64 { id: id.to_owned() });
    }
    let path = format!(
        "media/{}.{AUDIO_EXTENSION}",
        hex::encode(sha2::Sha256::digest(&bytes))
    );
    store(&path, &bytes).map_err(|source| VlmEvalCorpusError::Media {
        id: id.to_owned(),
        path: path.clone(),
        source,
    })?;
    Ok(VlmEvalCorpusPart::Audio {
        path,
        format: input_audio.format,
        bytes: bytes.len() as u64,
    })
}

/// The `input_audio` part a media part stands for.
fn move_in_audio(
    id: &str,
    path: &str,
    format: AudioFormat,
    expected: u64,
    read: &mut impl FnMut(&str) -> std::io::Result<Vec<u8>>,
) -> Result<ContentPart, VlmEvalCorpusError> {
    if format != AUDIO_FORMAT {
        return Err(VlmEvalCorpusError::UnsupportedAudioFormat {
            id: id.to_owned(),
            format,
        });
    }
    let bad_path = || VlmEvalCorpusError::BadMediaPath {
        id: id.to_owned(),
        path: path.to_owned(),
    };
    if path.rsplit_once('.').map(|(_, ext)| ext) != Some(AUDIO_EXTENSION) {
        return Err(bad_path());
    }
    let named = media_digest(path).ok_or_else(bad_path)?;
    let bytes = read(path).map_err(|source| VlmEvalCorpusError::Media {
        id: id.to_owned(),
        path: path.to_owned(),
        source,
    })?;
    if bytes.len() as u64 != expected {
        return Err(VlmEvalCorpusError::MediaSize {
            id: id.to_owned(),
            path: path.to_owned(),
            expected,
            actual: bytes.len() as u64,
        });
    }
    if hex::encode(sha2::Sha256::digest(&bytes)) != named {
        return Err(VlmEvalCorpusError::MediaDigest {
            id: id.to_owned(),
            path: path.to_owned(),
        });
    }
    Ok(ContentPart::InputAudio {
        input_audio: InputAudio {
            data: STANDARD.encode(&bytes),
            format,
        },
    })
}

/// `media/<64 hex>.<ext>` → the hex; anything else, including a path that
/// could leave the corpus, is `None`.
fn media_digest(path: &str) -> Option<&str> {
    let name = path.strip_prefix("media/")?;
    let (digest, ext) = name.split_once('.')?;
    let hex_ok = digest.len() == 64
        && digest
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let ext_ok = !ext.is_empty()
        && ext
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    (hex_ok && ext_ok).then_some(digest)
}

/// `image/<subtype>` → a file extension: `jpg` for JPEG, the subtype itself
/// when it is a plain lowercase word.
fn extension(mime: &str) -> Option<&str> {
    match mime.strip_prefix("image/")? {
        "jpeg" => Some("jpg"),
        subtype
            if !subtype.is_empty()
                && subtype.len() <= 16
                && subtype
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) =>
        {
            Some(subtype)
        }
        _ => None,
    }
}

/// The evals server's digest of samples as it serves them (its
/// `X-Samples-Digest`): SHA-256 over each sample's canonical JSON — keys
/// sorted, compact, UTF-8 — and a newline, in served order. The evals server
/// computes the same in Python; a shared test vector holds the two together.
pub struct SamplesDigest {
    hasher: sha2::Sha256,
    samples: u64,
}

impl Default for SamplesDigest {
    fn default() -> Self {
        Self::new()
    }
}

impl SamplesDigest {
    pub fn new() -> Self {
        Self {
            hasher: sha2::Sha256::new(),
            samples: 0,
        }
    }

    pub fn update(&mut self, sample: &EvalSample) -> Result<(), serde_json::Error> {
        let canonical = crate::canonical_json(&serde_json::to_value(sample)?);
        self.hasher.update(canonical.as_bytes());
        self.hasher.update(b"\n");
        self.samples += 1;
        Ok(())
    }

    /// Samples hashed so far.
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// The digest. A SHA-256 always validates as one; the `Result` is the
    /// newtype's.
    pub fn finish(self) -> Result<Sha256, crate::Sha256Error> {
        Sha256::try_new(hex::encode(self.hasher.finalize()))
    }
}

/// The hex an evals server's `sha256:<hex>` digest carries.
pub fn parse_samples_digest(reported: &str) -> Option<Sha256> {
    Sha256::try_new(reported.strip_prefix("sha256:")?.to_owned()).ok()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;

    /// The scorer's golden samples (`tests/test_hashing.py`): the two digests
    /// must agree on them, or every pin a build checks would read as moved.
    fn golden() -> anyhow::Result<Vec<EvalSample>> {
        Ok(serde_json::from_value(json!([
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

    fn digest(samples: &[EvalSample]) -> anyhow::Result<Sha256> {
        let mut digest = SamplesDigest::new();
        samples.iter().try_for_each(|s| digest.update(s))?;
        Ok(digest.finish()?)
    }

    #[test]
    fn the_digest_matches_the_evals_servers() -> anyhow::Result<()> {
        assert_eq!(
            digest(&golden()?)?.as_ref(),
            "5654ef883c378c4325cfe70594545d4a19c094defdf21b32487f9de94a6b4849"
        );
        Ok(())
    }

    /// Escapes the two languages could write differently: control characters,
    /// quotes, backslashes, astral characters. Pinned from the scorer's
    /// `samples_digest` over the same sample.
    #[test]
    fn awkward_text_digests_as_the_evals_server_digests_it() -> anyhow::Result<()> {
        let sample: EvalSample = serde_json::from_value(json!({
            "id": "c3",
            "messages": [{"role": "user", "content":
                "tab\there\u{1}\u{1f}\u{7f} \\ \"q\" / é 😀 \u{2028}\r\n\u{8}\u{c}"}]
        }))?;
        assert_eq!(
            digest(&[sample])?.as_ref(),
            "398cc6ebdc6f48b76c7bc670f047e4f82df20b1443fa0a30b2926808c29238f6"
        );
        Ok(())
    }

    /// Into the corpus and back gives the same samples, and so the same digest;
    /// an image two questions share is stored once.
    #[test]
    fn a_sample_round_trips_through_the_corpus() -> anyhow::Result<()> {
        let mut samples = golden()?;
        samples.push(serde_json::from_value(json!({
            "id": "d4", "messages": [{"role": "user", "content": [
                {"type": "text", "text": "Same picture:"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,/9j/4AAQ"}}
            ]}]
        }))?);
        let mut media = BTreeMap::<String, Vec<u8>>::new();
        let corpus = samples
            .iter()
            .map(|s| {
                VlmEvalCorpusSample::from_eval_sample(s, |path, bytes| {
                    media.insert(path.to_owned(), bytes.to_vec());
                    Ok(())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(media.len(), 2, "the shared PNG is one file: {media:?}");
        assert!(
            media
                .keys()
                .any(|p| p.rsplit_once('.').is_some_and(|(_, ext)| ext == "jpg")),
            "{media:?}"
        );

        let rebuilt = corpus
            .iter()
            .map(|c| {
                c.to_eval_sample(|path| {
                    media
                        .get(path)
                        .cloned()
                        .ok_or_else(|| std::io::Error::other("missing"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(rebuilt, samples);
        assert_eq!(digest(&rebuilt)?, digest(&samples)?);
        Ok(())
    }

    #[test]
    fn a_corpus_line_reads_as_written() -> anyhow::Result<()> {
        let line = r#"{"id":"a","messages":[{"role":"system","content":"Be brief."},{"role":"user","content":[{"type":"image","path":"media/aa.png","mime":"image/png","bytes":3},{"type":"text","text":"What?"}]}]}"#;
        let sample: VlmEvalCorpusSample = serde_json::from_str(line)?;
        assert_eq!(serde_json::to_string(&sample)?, line);
        assert_eq!(sample.media_paths().collect::<Vec<_>>(), ["media/aa.png"]);
        Ok(())
    }

    /// What would not rebuild the scorer's URI byte for byte is refused when
    /// the corpus is packed, not discovered when a device sends the prompt.
    #[rstest::rstest]
    #[case::https("https://example.test/a.png")]
    #[case::no_base64_marker("data:image/png,iVBORw0KGgo=")]
    #[case::not_an_image("data:text/plain;base64,aGk=")]
    #[case::whitespace("data:image/png;base64,iVBO Rw0KGgo=")]
    #[case::missing_padding("data:image/png;base64,iVBORw0KGgo")]
    #[case::url_safe_alphabet("data:image/png;base64,_-8=")]
    fn an_image_that_would_not_rebuild_is_refused(#[case] url: &str) -> anyhow::Result<()> {
        let sample: EvalSample = serde_json::from_value(json!({
            "id": "x", "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": url}}
            ]}]
        }))?;
        assert!(VlmEvalCorpusSample::from_eval_sample(&sample, |_, _| Ok(())).is_err());
        Ok(())
    }

    fn one_image(path: &str, bytes: u64) -> VlmEvalCorpusSample {
        VlmEvalCorpusSample {
            id: "x".into(),
            messages: vec![VlmEvalCorpusMessage {
                role: ChatRole::User,
                content: VlmEvalCorpusContent::Parts(vec![VlmEvalCorpusPart::Image {
                    path: path.into(),
                    mime: "image/png".into(),
                    bytes,
                }]),
            }],
            chat_template_kwargs: None,
        }
    }

    /// A corpus line cannot point outside `media/`, and a media file must be
    /// the one its name and size say: the tar's digest covers the transfer,
    /// these cover what the index says about it.
    #[rstest::rstest]
    #[case::escape(format!("media/../{}.png", "a".repeat(64)), 3, "is not `media/")]
    #[case::not_content_addressed("media/aa.png".to_owned(), 3, "is not `media/")]
    #[case::wrong_size(format!("media/{}.png", hex::encode(sha2::Sha256::digest(b"png"))), 4, "holds 3 bytes")]
    #[case::wrong_content(format!("media/{}.png", "a".repeat(64)), 3, "does not hash")]
    fn a_media_file_must_be_what_the_line_says(
        #[case] path: String,
        #[case] bytes: u64,
        #[case] expected: &str,
    ) {
        let error = one_image(&path, bytes)
            .to_eval_sample(|_| Ok(b"png".to_vec()))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains(expected), "{error}");
    }

    /// The scorer's ASR golden samples (`tests/test_hashing.py`,
    /// `test_an_asr_sample_is_pinned`): a clip, template arguments, a system
    /// turn and a text part beside the clip.
    fn asr_golden() -> anyhow::Result<Vec<EvalSample>> {
        Ok(serde_json::from_value(json!([
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

    #[test]
    fn an_asr_sample_digests_as_the_evals_server_digests_it() -> anyhow::Result<()> {
        assert_eq!(
            digest(&asr_golden()?)?.as_ref(),
            "b49fae68dfd0d735e59dc3df621d8b6189ec7977fc5830b54b368bd10f396ef7"
        );
        // The template arguments are part of what is pinned.
        let mut without = asr_golden()?;
        without[0].chat_template_kwargs = None;
        assert_ne!(
            digest(&without)?.as_ref(),
            "b49fae68dfd0d735e59dc3df621d8b6189ec7977fc5830b54b368bd10f396ef7"
        );
        Ok(())
    }

    /// Into an asr-eval corpus and back gives the same samples, template
    /// arguments included; a clip two samples share is stored once, as a wav.
    #[test]
    fn an_asr_sample_round_trips_through_its_corpus() -> anyhow::Result<()> {
        let samples = asr_golden()?;
        let mut media = BTreeMap::<String, Vec<u8>>::new();
        let corpus = samples
            .iter()
            .map(|s| {
                VlmEvalCorpusSample::from_eval_sample_in(
                    CorpusFormat::AsrEvalCorpusV1,
                    s,
                    |path, bytes| {
                        media.insert(path.to_owned(), bytes.to_vec());
                        Ok(())
                    },
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(media.len(), 1, "the shared clip is one file: {media:?}");
        assert!(
            media
                .keys()
                .all(|p| p.rsplit_once('.').is_some_and(|(_, ext)| ext == "wav")),
            "{media:?}"
        );
        let rebuilt = corpus
            .iter()
            .map(|c| {
                c.to_eval_sample_in(CorpusFormat::AsrEvalCorpusV1, |path| {
                    media
                        .get(path)
                        .cloned()
                        .ok_or_else(|| std::io::Error::other("missing"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(rebuilt, samples);
        assert_eq!(digest(&rebuilt)?, digest(&samples)?);
        Ok(())
    }

    /// The line the design writes: an audio part names its file, format and
    /// size, and the template arguments ride on the line.
    #[test]
    fn an_asr_corpus_line_reads_as_written() -> anyhow::Result<()> {
        let path = format!("media/{}.wav", "a".repeat(64));
        let line = format!(
            r#"{{"id":"d4","messages":[{{"role":"user","content":[{{"type":"text","text":"Transcribe."}},{{"type":"audio","path":"{path}","format":"wav","bytes":12}}]}}],"chat_template_kwargs":{{"language":"nl","transcription":true}}}}"#
        );
        let sample: VlmEvalCorpusSample = serde_json::from_str(&line)?;
        assert_eq!(serde_json::to_string(&sample)?, line);
        assert_eq!(sample.media_paths().collect::<Vec<_>>(), [path.as_str()]);
        Ok(())
    }

    /// Each format holds its own media part only, in both directions.
    #[rstest::rstest]
    #[case::audio_in_a_vlm_corpus(
        CorpusFormat::VlmEvalCorpusV1,
        json!({"type": "input_audio", "input_audio": {"data": "UklGRiQAAABXQVZF", "format": "wav"}}),
        "an audio part has no place in the vlm-eval-corpus/1 format"
    )]
    #[case::image_in_an_asr_corpus(
        CorpusFormat::AsrEvalCorpusV1,
        json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}}),
        "an image part has no place in the asr-eval-corpus/1 format"
    )]
    #[case::flac_clip(
        CorpusFormat::AsrEvalCorpusV1,
        json!({"type": "input_audio", "input_audio": {"data": "ZkxhQw==", "format": "flac"}}),
        "wav clips only"
    )]
    #[case::non_canonical_clip(
        CorpusFormat::AsrEvalCorpusV1,
        json!({"type": "input_audio", "input_audio": {"data": "UklGRiQAAABXQVZ", "format": "wav"}}),
        "audio clip is not"
    )]
    #[case::not_an_eval_corpus(
        CorpusFormat::VlSpecDecodeCorpusV1,
        json!({"type": "text", "text": "hi"}),
        "is not an eval prompt corpus"
    )]
    fn a_part_off_its_format_is_refused(
        #[case] format: CorpusFormat,
        #[case] part: serde_json::Value,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let sample: EvalSample = serde_json::from_value(json!({
            "id": "x", "messages": [{"role": "user", "content": [part]}]
        }))?;
        let error = VlmEvalCorpusSample::from_eval_sample_in(format, &sample, |_, _| Ok(()))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains(expected), "{error}");
        Ok(())
    }

    /// A stored line is held to its format too: a VL corpus line naming a clip,
    /// an ASR one naming an image, or a clip not stored as a wav, never
    /// rebuilds.
    #[rstest::rstest]
    #[case::audio_read_as_vlm(CorpusFormat::VlmEvalCorpusV1, "wav", "wav", "has no place")]
    #[case::image_read_as_asr(CorpusFormat::AsrEvalCorpusV1, "png", "", "has no place")]
    #[case::clip_not_named_wav(CorpusFormat::AsrEvalCorpusV1, "mp3", "wav", "is not `media/")]
    #[case::clip_not_wav(CorpusFormat::AsrEvalCorpusV1, "wav", "flac", "wav clips only")]
    fn a_stored_part_off_its_format_is_refused(
        #[case] format: CorpusFormat,
        #[case] ext: &str,
        #[case] audio_format: &str,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let bytes = b"RIFF".to_vec();
        let path = format!("media/{}.{ext}", hex::encode(sha2::Sha256::digest(&bytes)));
        let part = if audio_format.is_empty() {
            json!({"type": "image", "path": path, "mime": "image/png", "bytes": 4})
        } else {
            json!({"type": "audio", "path": path, "format": audio_format, "bytes": 4})
        };
        let line: VlmEvalCorpusSample = serde_json::from_value(json!({
            "id": "x", "messages": [{"role": "user", "content": [part]}]
        }))?;
        let error = line
            .to_eval_sample_in(format, |_| Ok(bytes.clone()))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains(expected), "{error}");
        Ok(())
    }

    /// The negative control: the same stored clip rebuilds in its own format.
    #[test]
    fn a_stored_clip_rebuilds_in_its_format() -> anyhow::Result<()> {
        let bytes = b"RIFF".to_vec();
        let path = format!("media/{}.wav", hex::encode(sha2::Sha256::digest(&bytes)));
        let line: VlmEvalCorpusSample = serde_json::from_value(json!({
            "id": "x", "messages": [{"role": "user", "content": [
                {"type": "audio", "path": path, "format": "wav", "bytes": 4}
            ]}]
        }))?;
        let sample =
            line.to_eval_sample_in(CorpusFormat::AsrEvalCorpusV1, |_| Ok(bytes.clone()))?;
        assert_eq!(
            serde_json::to_value(&sample.messages)?,
            json!([{"role": "user", "content": [
                {"type": "input_audio", "input_audio": {"data": "UklGRg==", "format": "wav"}}
            ]}])
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case(
        "sha256:5654ef883c378c4325cfe70594545d4a19c094defdf21b32487f9de94a6b4849",
        true
    )]
    #[case(
        "5654ef883c378c4325cfe70594545d4a19c094defdf21b32487f9de94a6b4849",
        false
    )]
    #[case("sha256:XYZ", false)]
    fn a_reported_digest_parses_only_with_its_prefix(#[case] reported: &str, #[case] ok: bool) {
        assert_eq!(parse_samples_digest(reported).is_some(), ok);
    }
}
