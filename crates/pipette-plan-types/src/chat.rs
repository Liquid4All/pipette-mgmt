//! An eval's prompts as the evals server writes them: OpenAI-style chat
//! messages, whose content is a string or, for a vision or audio message,
//! parts.

use serde::{Deserialize, Serialize};

/// One prompt of an eval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSample {
    pub id: String,
    pub messages: Vec<ChatMessage>,
    /// Template arguments this prompt needs on the request, as the evals server
    /// writes them: an `asr_eval` Voxtral sample's `{"transcription": true,
    /// "language": "nl"}`. Absent for every other sample, so their wire is
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Map<String, serde_json::Value>>,
}

/// One chat turn. The evals server sends no key beside these two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

/// A string for text, an array of parts for a vision or audio message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl MessageContent {
    /// Drop the image parts and keep the text: what a scored sample stores.
    pub fn drop_images(&mut self) {
        if let Self::Parts(parts) = self {
            parts.retain(|part| !matches!(part, ContentPart::ImageUrl { .. }));
        }
    }

    /// Empty every media part's payload, images and audio, and keep the part:
    /// what a scored sample stores. A clip is megabytes and the dataset holds
    /// it, but the part stays (its type, and a clip's format) so a stored
    /// sample still shows the turn's order, the prompt before or after its clip.
    pub fn drop_media(&mut self) {
        if let Self::Parts(parts) = self {
            for part in parts {
                match part {
                    ContentPart::Text { .. } => {}
                    ContentPart::ImageUrl { image_url } => image_url.url.clear(),
                    ContentPart::InputAudio { input_audio } => input_audio.data.clear(),
                }
            }
        }
    }
}

/// One part of a vision or audio message. An unknown `type` is refused rather
/// than dropped: a device could not send the prompt the scorer grades.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
    InputAudio { input_audio: InputAudio },
}

/// An image part's image: a base64 `data:` URI, as the evals server sends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
}

/// An audio part's clip, as the OpenAI `input_audio` part carries it: the file's
/// bytes, base64, and its container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputAudio {
    pub data: String,
    pub format: AudioFormat,
}

/// The containers an evals server's clip may come in; llama-server's media
/// decoder reads each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    Wav,
    Flac,
    Mp3,
    Ogg,
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    /// Both content shapes are written as the evals server writes them, byte
    /// for byte, so a text eval's prompt is served as before.
    #[rstest]
    #[case::text(r#"{"role":"system","content":"Answer with a letter."}"#)]
    #[case::parts(
        r#"{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBO"}},{"type":"text","text":"Which shape is red?"}]}"#
    )]
    #[case::assistant(r#"{"role":"assistant","content":""}"#)]
    #[case::audio(
        r#"{"role":"user","content":[{"type":"text","text":"Transcribe."},{"type":"input_audio","input_audio":{"data":"UklGR","format":"wav"}}]}"#
    )]
    #[case::audio_flac(
        r#"{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"ZkxhQw","format":"flac"}}]}"#
    )]
    fn a_message_round_trips_byte_for_byte(#[case] wire: &str) -> anyhow::Result<()> {
        let message: ChatMessage = serde_json::from_str(wire)?;
        assert_eq!(serde_json::to_string(&message)?, wire);
        Ok(())
    }

    /// A part or role no device could send fails the parse, not the run.
    #[rstest]
    #[case::part_type(json!({"role": "user", "content": [{"type": "input_video", "input_video": {"data": ""}}]}))]
    #[case::audio_without_format(json!({"role": "user", "content": [{"type": "input_audio", "input_audio": {"data": ""}}]}))]
    #[case::audio_format(json!({"role": "user", "content": [{"type": "input_audio", "input_audio": {"data": "", "format": "aiff"}}]}))]
    #[case::audio_extra_key(json!({"role": "user", "content": [{"type": "input_audio", "input_audio": {"data": "", "format": "wav", "rate": 16000}}]}))]
    #[case::image_without_url(json!({"role": "user", "content": [{"type": "image_url", "image_url": {}}]}))]
    #[case::role(json!({"role": "tool", "content": "42"}))]
    #[case::content_object(json!({"role": "user", "content": {"text": "2+2?"}}))]
    fn an_unknown_part_or_role_is_refused(#[case] wire: serde_json::Value) {
        assert!(
            serde_json::from_value::<ChatMessage>(wire.clone()).is_err(),
            "{wire}"
        );
    }

    #[rstest]
    #[case::text(json!("2+2?"), json!("2+2?"))]
    #[case::parts(
        json!([
            {"type": "text", "text": "Look:"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBO"}},
            {"type": "text", "text": "Which shape is red?"},
        ]),
        json!([
            {"type": "text", "text": "Look:"},
            {"type": "text", "text": "Which shape is red?"},
        ])
    )]
    #[case::image_only(
        json!([{"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBO"}}]),
        json!([])
    )]
    fn dropping_images_keeps_the_text(
        #[case] content: serde_json::Value,
        #[case] expected: serde_json::Value,
    ) -> anyhow::Result<()> {
        let mut content: MessageContent = serde_json::from_value(content)?;
        content.drop_images();
        assert_eq!(serde_json::to_value(&content)?, expected);
        Ok(())
    }

    /// Dropping media empties each image and clip and keeps every part in its
    /// place: the order of prompt and clip stays auditable.
    #[rstest]
    #[case::prompt_first(json!([
        {"type": "text", "text": "Transcribe."},
        {"type": "input_audio", "input_audio": {"data": "UklGR", "format": "wav"}},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBO"}},
    ]), json!([
        {"type": "text", "text": "Transcribe."},
        {"type": "input_audio", "input_audio": {"data": "", "format": "wav"}},
        {"type": "image_url", "image_url": {"url": ""}},
    ]))]
    #[case::clip_first(json!([
        {"type": "input_audio", "input_audio": {"data": "ZkxhQw", "format": "flac"}},
        {"type": "text", "text": "Transcribe."},
    ]), json!([
        {"type": "input_audio", "input_audio": {"data": "", "format": "flac"}},
        {"type": "text", "text": "Transcribe."},
    ]))]
    fn dropping_media_empties_each_part_in_place(
        #[case] content: serde_json::Value,
        #[case] expected: serde_json::Value,
    ) -> anyhow::Result<()> {
        let mut content: MessageContent = serde_json::from_value(content)?;
        content.drop_media();
        assert_eq!(serde_json::to_value(&content)?, expected);
        Ok(())
    }

    /// A sample's template arguments round-trip, and a sample without them is
    /// written as before.
    #[rstest]
    #[case::without(r#"{"id":"a","messages":[{"role":"user","content":"hi"}]}"#)]
    #[case::with(
        r#"{"id":"a","messages":[{"role":"user","content":"hi"}],"chat_template_kwargs":{"language":"nl","transcription":true}}"#
    )]
    fn a_sample_round_trips_with_or_without_template_kwargs(
        #[case] wire: &str,
    ) -> anyhow::Result<()> {
        let sample: EvalSample = serde_json::from_str(wire)?;
        assert_eq!(serde_json::to_string(&sample)?, wire);
        Ok(())
    }
}
