//! `mmspec@1`: [MMSpec](https://github.com/killthefullmoon/MMSpec)'s
//! `mmspec.jsonl` + `images/`, under MMSpec's own evaluation protocol.

use std::collections::{BTreeMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::super::format::{Corpus, Part, Sample, file_sha256};

/// Verbatim from MMSpec's `evaluation/utils.py`.
const SYSTEM_PROMPT: &str = "A chat between a curious human and an artificial \
    intelligence assistant. The assistant gives helpful, detailed, and polite answers to \
    the human's questions.";

/// `topic` → subset name, as the DSpark reference results name them, so
/// per-subset rows compare directly. Renaming one changes the artifact digest.
const TOPICS: &[(&str, &str)] = &[
    ("chart understanding", "CharXiv"),
    ("complex reasoning pro", "MMMU-Pro"),
    ("general vqa", "GQA"),
    ("image captioning", "COCO"),
    ("multi-turn conversation", "multi-turn"),
    ("text vqa", "TextVQA"),
];

#[derive(Deserialize)]
struct Line {
    id: String,
    /// One image per sample: MMSpec's format has no multi-image field.
    image: String,
    turns: Vec<String>,
    topic: String,
}

pub(super) fn adapt(root: &Path) -> anyhow::Result<Corpus> {
    let index = std::fs::File::open(root.join("mmspec.jsonl"))?;
    let lines = std::io::BufReader::new(index)
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.as_ref().is_ok_and(|l| l.trim().is_empty()))
        .map(|(n, line)| {
            serde_json::from_str::<Line>(&line?)
                .map_err(|e| anyhow::anyhow!("mmspec.jsonl line {}: {e}", n + 1))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut ids = HashSet::new();
    let (samples, media) = lines.into_iter().try_fold(
        (Vec::new(), BTreeMap::new()),
        |(mut samples, mut media), line| {
            anyhow::ensure!(
                ids.insert(line.id.clone()),
                "duplicate sample id {}",
                line.id
            );
            let (path, source, image) = media_part(root, &line)?;
            media.insert(path, source);
            samples.push(sample(line, image)?);
            Ok((samples, media))
        },
    )?;
    anyhow::ensure!(!samples.is_empty(), "mmspec.jsonl holds no samples");
    Ok(Corpus { samples, media })
}

/// The line's image as a content-addressed part: its tar path, its source
/// file, and the part itself.
fn media_part(root: &Path, line: &Line) -> anyhow::Result<(String, PathBuf, Part)> {
    let name = &line.image;
    anyhow::ensure!(
        !name.is_empty() && !name.contains('/') && !name.starts_with('.'),
        "{}: image name {name:?} is not a plain file name",
        line.id
    );
    let source = root.join("images").join(name);
    let (ext, mime) = media_type(name)
        .ok_or_else(|| anyhow::anyhow!("{}: unsupported image {name:?}", line.id))?;
    let bytes = std::fs::metadata(&source)
        .map_err(|e| anyhow::anyhow!("{}: image {name:?}: {e}", line.id))?
        .len();
    let path = format!("media/{}.{ext}", file_sha256(&source)?);
    let image = Part::Image {
        path: path.clone(),
        mime: mime.to_string(),
        bytes,
    };
    Ok((path, source, image))
}

fn sample(line: Line, image: Part) -> anyhow::Result<Sample> {
    let subset = TOPICS
        .iter()
        .find(|(topic, _)| *topic == line.topic)
        .map(|(_, subset)| (*subset).to_string())
        .ok_or_else(|| anyhow::anyhow!("{}: unknown topic {:?}", line.id, line.topic))?;
    anyhow::ensure!(!line.turns.is_empty(), "{}: no turns", line.id);
    let mut image = Some(image);
    let turns = line
        .turns
        .into_iter()
        .enumerate()
        .map(|(i, text)| {
            // MMSpec attaches the image to the first user turn only.
            let mut parts = vec![Part::Text { text }];
            parts.extend(image.take().filter(|_| i == 0));
            parts
        })
        .collect();
    Ok(Sample {
        id: line.id,
        subset,
        system: SYSTEM_PROMPT.to_string(),
        turns,
    })
}

fn media_type(name: &str) -> Option<(&'static str, &'static str)> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Some(("jpg", "image/jpeg")),
        "png" => Some(("png", "image/png")),
        "webp" => Some(("webp", "image/webp")),
        "gif" => Some(("gif", "image/gif")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use sha2::{Digest, Sha256};

    use super::*;

    /// One single-turn GQA sample, one three-turn conversation.
    fn fixture(dir: &Path) -> anyhow::Result<std::path::PathBuf> {
        let root = dir.join("test");
        std::fs::create_dir_all(root.join("images"))?;
        std::fs::write(root.join("images/mmspec_001.jpg"), b"jpeg-one")?;
        std::fs::write(root.join("images/mmspec_553.jpg"), vec![7_u8; 1300])?;
        std::fs::write(
            root.join("mmspec.jsonl"),
            concat!(
                r#"{"id": "mmspec_001", "image": "mmspec_001.jpg", "turns": ["Which shape?"], "category": "default", "topic": "general vqa"}"#,
                "\n",
                r#"{"id": "mmspec_553", "image": "mmspec_553.jpg", "turns": ["Trends?", "Highest?", "Why?"], "category": "charts", "topic": "multi-turn conversation"}"#,
                "\n",
            ),
        )?;
        Ok(root)
    }

    /// A tree holding `lines` as the index, over the fixture's images.
    fn with_index(dir: &Path, lines: &str) -> anyhow::Result<std::path::PathBuf> {
        let root = fixture(dir)?;
        std::fs::write(root.join("mmspec.jsonl"), lines)?;
        Ok(root)
    }

    /// One index line, for the cases that vary only in topic or image.
    fn line(id: &str, image: &str, topic: &str) -> String {
        format!(r#"{{"id": "{id}", "image": "{image}", "turns": ["q"], "topic": "{topic}"}}"#)
    }

    #[test]
    fn maps_topics_and_puts_the_image_on_the_first_turn() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let corpus = adapt(&fixture(dir.path())?)?;
        assert_eq!(corpus.samples.len(), 2);
        assert_eq!(
            corpus.samples[0].id, "mmspec_001",
            "file order is traversal order"
        );
        let multi = &corpus.samples[1];
        assert_eq!(multi.subset, "multi-turn");
        assert_eq!(multi.system, SYSTEM_PROMPT);
        assert_eq!(multi.turns.len(), 3);
        assert_eq!(multi.turns[0].len(), 2, "text then image on turn one");
        assert!(
            matches!(&multi.turns[0][1], Part::Image { mime, bytes: 1300, .. } if mime == "image/jpeg")
        );
        assert_eq!(
            multi.turns[1],
            vec![Part::Text {
                text: "Highest?".into()
            }],
            "later turns carry no image"
        );
        assert_eq!(corpus.media.len(), 2);
        Ok(())
    }

    /// An unknown topic fails the build rather than inventing a subset.
    #[rstest]
    #[case("chart understanding", Some("CharXiv"))]
    #[case("complex reasoning pro", Some("MMMU-Pro"))]
    #[case("general vqa", Some("GQA"))]
    #[case("image captioning", Some("COCO"))]
    #[case("multi-turn conversation", Some("multi-turn"))]
    #[case("text vqa", Some("TextVQA"))]
    #[case("astrology", None)]
    #[case("General VQA", None)]
    fn maps_every_topic(#[case] topic: &str, #[case] subset: Option<&str>) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = with_index(dir.path(), &line("a", "mmspec_001.jpg", topic))?;
        match (adapt(&root), subset) {
            (Ok(corpus), Some(subset)) => assert_eq!(corpus.samples[0].subset, subset),
            (Err(_), None) => {}
            (result, _) => anyhow::bail!("{topic:?}: unexpected {:?}", result.map(|_| ())),
        }
        Ok(())
    }

    /// Media is content-addressed, so one image used twice is stored once, and
    /// the extension is normalized with the mime type.
    #[test]
    fn content_addresses_and_deduplicates_media() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = with_index(
            dir.path(),
            &format!(
                "{}\n{}\n{}\n",
                line("a", "mmspec_001.jpg", "general vqa"),
                line("b", "copy.jpeg", "text vqa"),
                line("c", "shot.png", "image captioning"),
            ),
        )?;
        // `copy.jpeg` holds the same bytes as `mmspec_001.jpg` under another name.
        std::fs::write(root.join("images/copy.jpeg"), b"jpeg-one")?;
        std::fs::write(root.join("images/shot.png"), b"png-bytes")?;
        let corpus = adapt(&root)?;

        // A text part here would fail the assertions below just as loudly.
        let path_of = |sample: &Sample| match &sample.turns[0][1] {
            Part::Image { path, mime, .. } => (path.clone(), mime.clone()),
            Part::Text { text } => (text.clone(), String::new()),
        };
        let (jpg, jpg_mime) = path_of(&corpus.samples[0]);
        assert_eq!(
            jpg,
            format!("media/{}.jpg", hex::encode(Sha256::digest(b"jpeg-one")))
        );
        assert_eq!(jpg_mime, "image/jpeg");
        assert_eq!(path_of(&corpus.samples[1]).0, jpg, "same bytes, same entry");
        assert_eq!(
            path_of(&corpus.samples[2]),
            (
                format!("media/{}.png", hex::encode(Sha256::digest(b"png-bytes"))),
                "image/png".to_string()
            ),
            "a .jpeg normalizes to .jpg, a .png stays .png"
        );
        assert_eq!(corpus.media.len(), 2, "three samples, two distinct images");
        Ok(())
    }

    /// Each of these would otherwise reach a client as a corpus that is wrong
    /// rather than absent.
    #[rstest]
    #[case::duplicate_id(concat!(
        r#"{"id": "a", "image": "mmspec_001.jpg", "turns": ["q"], "topic": "general vqa"}"#,
        "\n",
        r#"{"id": "a", "image": "mmspec_553.jpg", "turns": ["q"], "topic": "text vqa"}"#,
    ))]
    #[case::missing_image_file(
        r#"{"id": "b", "image": "absent.jpg", "turns": ["q"], "topic": "general vqa"}"#
    )]
    #[case::unsupported_extension(
        r#"{"id": "b", "image": "notes.txt", "turns": ["q"], "topic": "general vqa"}"#
    )]
    #[case::path_traversal(
        r#"{"id": "b", "image": "../mmspec.jsonl", "turns": ["q"], "topic": "general vqa"}"#
    )]
    #[case::absolute_name(
        r#"{"id": "b", "image": "/etc/passwd", "turns": ["q"], "topic": "general vqa"}"#
    )]
    #[case::no_turns(
        r#"{"id": "b", "image": "mmspec_001.jpg", "turns": [], "topic": "general vqa"}"#
    )]
    #[case::no_image_field(r#"{"id": "b", "turns": ["q"], "topic": "general vqa"}"#)]
    #[case::not_json("{not json")]
    #[case::empty("\n\n")]
    fn rejects_a_corpus_it_cannot_represent(#[case] index: &str) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = with_index(dir.path(), index)?;
        std::fs::write(root.join("images/notes.txt"), b"not an image")?;
        assert!(adapt(&root).is_err(), "should have been refused: {index}");
        Ok(())
    }
}
