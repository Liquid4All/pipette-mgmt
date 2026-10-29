//! `vl-spec-decode-corpus/1`: user turns of text and images, no ground truth,
//! plus the tree digest and the deterministic tar it packs into. Another shape
//! gets another format, not a wider version of this one.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use pipette_plan_types::{CorpusFormat, DatasetAdapter, DatasetUpstream};

// ---------------------------------------------------------------------------
// Tree digest
// ---------------------------------------------------------------------------

/// Each file's root-relative path and the sha256 of its contents, sorted by
/// path, hashed as `"{path}\0{hex_sha256}\n"`. A `.git` directory directly
/// under `root` is not part of the tree.
pub(crate) fn tree_digest(root: &Path) -> anyhow::Result<pipette_plan_types::Sha256> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort();
    let tree = files.iter().try_fold(Sha256::new(), |mut tree, relative| {
        let hex = file_sha256(&root.join(relative))?;
        tree.update(relative.as_bytes());
        tree.update(b"\0");
        tree.update(hex.as_bytes());
        tree.update(b"\n");
        anyhow::Ok(tree)
    })?;
    Ok(pipette_plan_types::Sha256::try_new(hex::encode(
        tree.finalize(),
    ))?)
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> anyhow::Result<()> {
    std::fs::read_dir(dir)?
        .filter(|entry| {
            !entry
                .as_ref()
                .is_ok_and(|e| dir == root && e.file_name() == ".git")
        })
        .try_for_each(|entry| {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                collect_files(root, &path, out)
            } else if kind.is_file() {
                let relative = path
                    .strip_prefix(root)?
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path in upstream tree: {path:?}"))?
                    .replace(std::path::MAIN_SEPARATOR, "/");
                out.push(relative);
                Ok(())
            } else {
                anyhow::bail!("upstream tree holds a non-regular file: {path:?}")
            }
        })
}

pub(super) fn file_sha256(path: &Path) -> anyhow::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = HashingWriter::new(std::io::sink());
    std::io::copy(&mut file, &mut hasher)?;
    let (digest, _, _) = hasher.finish();
    Ok(digest)
}

// ---------------------------------------------------------------------------
// Corpus model
// ---------------------------------------------------------------------------

/// Maps one-to-one onto a `/v1/chat/completions` content part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Part {
    Text {
        text: String,
    },
    Image {
        /// Tar-relative, `media/<sha256>.<ext>`.
        path: String,
        mime: String,
        bytes: u64,
    },
}

/// One `samples.jsonl` line. Only user turns: each arm generates the replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Sample {
    pub(crate) id: String,
    pub(crate) subset: String,
    pub(crate) system: String,
    pub(crate) turns: Vec<Vec<Part>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SubsetCounts {
    pub(crate) samples: u32,
    pub(crate) turns: u32,
}

/// `manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) format: CorpusFormat,
    pub(crate) adapter: DatasetAdapter,
    pub(crate) upstream: DatasetUpstream,
    pub(crate) samples: u32,
    pub(crate) turns: u32,
    pub(crate) subsets: BTreeMap<String, SubsetCounts>,
}

/// Samples in traversal order, and every media file by its tar path.
pub(crate) struct Corpus {
    pub(crate) samples: Vec<Sample>,
    pub(crate) media: BTreeMap<String, PathBuf>,
}

impl Corpus {
    /// The samples of one subset, in their traversal order, and only the media
    /// they reference. The upstream's other subsets are other benchmarks.
    pub(crate) fn only(mut self, subset: &str) -> anyhow::Result<Corpus> {
        self.samples.retain(|s| s.subset == subset);
        anyhow::ensure!(
            !self.samples.is_empty(),
            "the corpus has no subset {subset:?}"
        );
        let used: BTreeSet<&str> = self
            .samples
            .iter()
            .flat_map(|s| s.turns.iter().flatten())
            .filter_map(|part| match part {
                Part::Image { path, .. } => Some(path.as_str()),
                Part::Text { .. } => None,
            })
            .collect();
        self.media.retain(|path, _| used.contains(path.as_str()));
        Ok(self)
    }

    pub(crate) fn manifest(
        &self,
        adapter: DatasetAdapter,
        upstream: &DatasetUpstream,
    ) -> anyhow::Result<Manifest> {
        let subsets = self.samples.iter().try_fold(
            BTreeMap::<String, SubsetCounts>::new(),
            |mut subsets, sample| {
                let counts = subsets.entry(sample.subset.clone()).or_default();
                counts.samples += 1;
                counts.turns += u32::try_from(sample.turns.len())?;
                anyhow::Ok(subsets)
            },
        )?;
        Ok(Manifest {
            format: CorpusFormat::VlSpecDecodeCorpusV1,
            adapter,
            upstream: upstream.clone(),
            samples: u32::try_from(self.samples.len())?,
            turns: subsets.values().map(|counts| counts.turns).sum(),
            subsets,
        })
    }
}

// ---------------------------------------------------------------------------
// Deterministic tar
// ---------------------------------------------------------------------------

const BLOCK: usize = 512;

/// The archive's identity, plus the manifest it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Packed {
    pub(crate) digest: pipette_plan_types::Sha256,
    pub(crate) bytes: u64,
    pub(crate) manifest: Manifest,
}

/// Uncompressed ustar, entries sorted, mode 0644, uid/gid 0, mtime 0, no
/// directories: the same corpus yields the same bytes, so a catalog can pin it.
pub(crate) fn pack(
    corpus: &Corpus,
    adapter: DatasetAdapter,
    upstream: &DatasetUpstream,
    dest: &Path,
) -> anyhow::Result<Packed> {
    let recorded = corpus.manifest(adapter, upstream)?;
    let mut manifest = serde_json::to_vec(&recorded)?;
    manifest.push(b'\n');
    let samples = corpus
        .samples
        .iter()
        .try_fold(Vec::new(), |mut lines, sample| {
            serde_json::to_writer(&mut lines, sample)?;
            lines.push(b'\n');
            anyhow::Ok(lines)
        })?;
    let (digest, bytes) = write_corpus(&manifest, &samples, &corpus.media, dest)?;
    Ok(Packed {
        digest,
        bytes,
        manifest: recorded,
    })
}

/// A corpus tar: `manifest.json`, `samples.jsonl` and each media file, in the
/// fixed layout of [`pack`]. Every format is these three kinds of entry.
pub(crate) fn write_corpus(
    manifest: &[u8],
    samples: &[u8],
    media: &BTreeMap<String, PathBuf>,
    dest: &Path,
) -> anyhow::Result<(pipette_plan_types::Sha256, u64)> {
    enum Body<'a> {
        Memory(&'a [u8]),
        File(&'a Path),
    }

    let mut entries: Vec<(&str, Body<'_>)> = vec![
        ("manifest.json", Body::Memory(manifest)),
        ("samples.jsonl", Body::Memory(samples)),
    ];
    entries.extend(
        media
            .iter()
            .map(|(path, source)| (path.as_str(), Body::File(source))),
    );
    entries.sort_by(|a, b| a.0.cmp(b.0));

    let mut out = HashingWriter::new(std::io::BufWriter::new(std::fs::File::create(dest)?));
    entries
        .into_iter()
        .try_for_each(|(path, body)| match body {
            Body::Memory(data) => {
                out.write_all(&header(path, data.len() as u64)?)?;
                out.write_all(data)?;
                pad(&mut out, data.len() as u64)?;
                anyhow::Ok(())
            }
            Body::File(source) => {
                let size = std::fs::metadata(source)?.len();
                out.write_all(&header(path, size)?)?;
                let copied = std::io::copy(&mut std::fs::File::open(source)?.take(size), &mut out)?;
                anyhow::ensure!(copied == size, "{source:?} changed size while packing");
                pad(&mut out, size)?;
                Ok(())
            }
        })?;
    out.write_all(&[0_u8; BLOCK * 2])?;
    let (digest, bytes, inner) = out.finish();
    inner
        .into_inner()
        .map_err(|e| anyhow::anyhow!("flushing {dest:?}: {e}"))?
        .sync_all()?;
    Ok((pipette_plan_types::Sha256::try_new(digest)?, bytes))
}

fn pad(out: &mut impl Write, size: u64) -> std::io::Result<()> {
    let rem = size % BLOCK as u64;
    if rem != 0 {
        std::io::copy(&mut std::io::repeat(0).take(BLOCK as u64 - rem), out)?;
    }
    Ok(())
}

/// Every variable field fixed; see [`pack`].
fn header(path: &str, size: u64) -> anyhow::Result<[u8; BLOCK]> {
    anyhow::ensure!(
        path.is_ascii() && !path.is_empty() && path.len() <= 100,
        "tar path {path:?} must be 1..=100 ASCII bytes"
    );
    anyhow::ensure!(
        size < 8_u64.pow(11),
        "{path:?} is too large for a ustar entry"
    );
    let mut h = [0_u8; BLOCK];
    h.iter_mut()
        .zip(path.bytes())
        .for_each(|(dst, src)| *dst = src);
    h[100..108].copy_from_slice(b"0000644\0");
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    h[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    h[136..148].copy_from_slice(b"00000000000\0");
    h[148..156].copy_from_slice(b"        ");
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    Ok(h)
}

/// Hashes and counts what passes through.
struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(self) -> (String, u64, W) {
        (hex::encode(self.hasher.finalize()), self.bytes, self.inner)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        let written = buf
            .get(..n)
            .ok_or_else(|| std::io::Error::other("writer reported more bytes than it was given"))?;
        self.hasher.update(written);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPSTREAM: &str = r#"
source   = "git"
url      = "github.com/acme/corpus"
revision = "0123456789abcdef0123456789abcdef01234567"
subpath  = "dataset/test"
identity = "2923603292c7dd463ad01a119f57965817778fc3"
digest   = "2222222222222222222222222222222222222222222222222222222222222222"
license  = "Apache-2.0"
"#;

    /// A corpus with one text turn and one image, built by hand: the packer is
    /// not an adapter's business.
    fn fixture(dir: &Path) -> anyhow::Result<Corpus> {
        let image = dir.join("a.jpg");
        std::fs::write(&image, vec![7_u8; 1300])?;
        Ok(Corpus {
            samples: vec![Sample {
                id: "s1".to_string(),
                subset: "GQA".to_string(),
                system: "be helpful".to_string(),
                turns: vec![vec![
                    Part::Text {
                        text: "what shape?".to_string(),
                    },
                    Part::Image {
                        path: "media/aa.jpg".to_string(),
                        mime: "image/jpeg".to_string(),
                        bytes: 1300,
                    },
                ]],
            }],
            media: BTreeMap::from([("media/aa.jpg".to_string(), image)]),
        })
    }

    /// A subset the corpus lacks is an error, not an empty artifact.
    #[test]
    fn only_keeps_one_subset_and_its_media() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut corpus = fixture(dir.path())?;
        let mut coco = corpus.samples[0].clone();
        coco.id = "s2".to_string();
        coco.subset = "COCO".to_string();
        coco.turns = vec![vec![Part::Image {
            path: "media/bb.jpg".to_string(),
            mime: "image/jpeg".to_string(),
            bytes: 1,
        }]];
        corpus.samples.push(coco);
        corpus
            .media
            .insert("media/bb.jpg".to_string(), dir.path().join("b.jpg"));

        let gqa = corpus.only("GQA")?;
        assert_eq!(
            gqa.samples
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            ["s1"]
        );
        assert_eq!(gqa.media.keys().collect::<Vec<_>>(), ["media/aa.jpg"]);
        assert_eq!(
            gqa.manifest(DatasetAdapter::MmspecV1, &toml::from_str(UPSTREAM)?)?
                .subsets
                .keys()
                .collect::<Vec<_>>(),
            ["GQA"]
        );
        assert!(gqa.only("TextVQA").is_err());
        Ok(())
    }

    #[test]
    fn packing_is_deterministic_and_readable() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let upstream: DatasetUpstream = toml::from_str(UPSTREAM)?;
        let corpus = fixture(dir.path())?;
        let first = pack(
            &corpus,
            DatasetAdapter::MmspecV1,
            &upstream,
            &dir.path().join("a.tar"),
        )?;
        let second = pack(
            &corpus,
            DatasetAdapter::MmspecV1,
            &upstream,
            &dir.path().join("b.tar"),
        )?;
        assert_eq!(first, second);
        assert_eq!(first.bytes % BLOCK as u64, 0);
        assert_eq!(
            std::fs::read(dir.path().join("a.tar"))?,
            std::fs::read(dir.path().join("b.tar"))?
        );

        let names: Vec<String> = read_tar(&std::fs::read(dir.path().join("a.tar"))?)?
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["manifest.json", "media/aa.jpg", "samples.jsonl"]);
        Ok(())
    }

    /// The image is 1300 bytes, so its entry is padded.
    #[test]
    fn the_archive_holds_the_manifest_samples_and_media() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let upstream: DatasetUpstream = toml::from_str(UPSTREAM)?;
        let mut corpus = fixture(dir.path())?;
        let mut second = corpus.samples[0].clone();
        second.id = "s2".to_string();
        second.subset = "COCO".to_string();
        second.turns.push(vec![Part::Text {
            text: "and now?".to_string(),
        }]);
        corpus.samples.push(second);
        pack(
            &corpus,
            DatasetAdapter::MmspecV1,
            &upstream,
            &dir.path().join("c.tar"),
        )?;
        let entries: BTreeMap<String, Vec<u8>> =
            read_tar(&std::fs::read(dir.path().join("c.tar"))?)?
                .into_iter()
                .collect();

        let manifest: Manifest = serde_json::from_slice(&entries["manifest.json"])?;
        assert_eq!(manifest.format, CorpusFormat::VlSpecDecodeCorpusV1);
        assert_eq!(manifest.adapter, DatasetAdapter::MmspecV1);
        assert_eq!(manifest.upstream, upstream);
        assert_eq!((manifest.samples, manifest.turns), (2, 3));
        assert_eq!(
            manifest.subsets,
            BTreeMap::from([
                (
                    "COCO".to_string(),
                    SubsetCounts {
                        samples: 1,
                        turns: 2
                    }
                ),
                (
                    "GQA".to_string(),
                    SubsetCounts {
                        samples: 1,
                        turns: 1
                    }
                ),
            ])
        );

        let lines: Vec<&[u8]> = entries["samples.jsonl"]
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 2, "one line per sample");
        let parsed: Vec<Sample> = lines
            .iter()
            .map(|l| serde_json::from_slice(l))
            .collect::<Result<_, _>>()?;
        assert_eq!(parsed, corpus.samples, "order and content survive the pack");
        let wire: serde_json::Value = serde_json::from_slice(lines[0])?;
        assert_eq!(wire["turns"][0][0]["type"], "text");
        assert_eq!(wire["turns"][0][1]["type"], "image");
        assert_eq!(wire["turns"][0][1]["path"], "media/aa.jpg");

        assert_eq!(entries["media/aa.jpg"], vec![7_u8; 1300]);
        Ok(())
    }

    /// ustar stores a name in 100 bytes, so a longer one must fail the pack
    /// rather than be written truncated.
    #[test]
    fn rejects_a_path_ustar_cannot_hold() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let upstream: DatasetUpstream = toml::from_str(UPSTREAM)?;
        let mut corpus = fixture(dir.path())?;
        let long = format!("media/{}.jpg", "a".repeat(100));
        let source = corpus
            .media
            .remove("media/aa.jpg")
            .ok_or_else(|| anyhow::anyhow!("the fixture has one media file"))?;
        corpus.media.insert(long, source);
        assert!(
            pack(
                &corpus,
                DatasetAdapter::MmspecV1,
                &upstream,
                &dir.path().join("d.tar")
            )
            .is_err()
        );
        Ok(())
    }

    /// Walk it the way a reader does: header, checksum, size, padding.
    fn read_tar(tar: &[u8]) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
        let mut names = Vec::new();
        let mut at = 0;
        while at + BLOCK <= tar.len() {
            let h = &tar[at..at + BLOCK];
            if h.iter().all(|&b| b == 0) {
                break;
            }
            let stored = u32::from_str_radix(std::str::from_utf8(&h[148..154])?.trim(), 8)?;
            let mut blank = [0_u8; BLOCK];
            blank.copy_from_slice(h);
            blank[148..156].copy_from_slice(b"        ");
            anyhow::ensure!(
                stored == blank.iter().map(|&b| u32::from(b)).sum::<u32>(),
                "checksum"
            );
            anyhow::ensure!(&h[257..263] == b"ustar\0", "magic");
            let name_len = h[..100].iter().position(|&b| b == 0).unwrap_or(100);
            let name = std::str::from_utf8(&h[..name_len])?.to_string();
            let size =
                usize::try_from(u64::from_str_radix(std::str::from_utf8(&h[124..135])?, 8)?)?;
            names.push((name, tar[at + BLOCK..at + BLOCK + size].to_vec()));
            // Every entry is padded to a whole block, so the next header is
            // block-aligned however long the body was.
            at += BLOCK + size.div_ceil(BLOCK) * BLOCK;
        }
        Ok(names)
    }

    #[test]
    fn tree_digest_skips_the_git_directory_and_orders_by_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join(".git"))?;
        std::fs::write(dir.path().join(".git/HEAD"), b"ref")?;
        std::fs::create_dir_all(dir.path().join("b"))?;
        std::fs::write(dir.path().join("b/x"), b"x")?;
        std::fs::write(dir.path().join("a"), b"a")?;
        let expected = hex::encode(
            [("a", b"a"), ("b/x", b"x")]
                .iter()
                .fold(Sha256::new(), |mut h, (path, body)| {
                    h.update(path.as_bytes());
                    h.update(b"\0");
                    h.update(hex::encode(Sha256::digest(body)).as_bytes());
                    h.update(b"\n");
                    h
                })
                .finalize(),
        );
        assert_eq!(tree_digest(dir.path())?.as_ref(), expected);
        Ok(())
    }
}
