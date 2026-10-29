//! The deterministic tar an import packs a model directory into: one commit
//! always gives one digest. A header needs only the listed size, so the archive
//! streams; a file of 8 GiB or more takes the GNU base-256 size field.

use std::future::{self, Future};
use std::sync::Arc;

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream};

use pipette_core::vault::BlobDigest;
use pipette_plan_types::Sha256;
use pipette_store::BlobBody;

/// The size of a tar header, and the unit that each entry is padded to.
const BLOCK: u64 = 512;

/// The two zero blocks that end an archive.
const END: [u8; 1024] = [0; 1024];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TarEntry {
    /// The path in the archive, from the model directory.
    pub(super) name: String,
    /// The path in the repository, which the download names.
    pub(super) repo_path: String,
    pub(super) size: u64,
    /// The digest that the Hub gives for the file, which the bytes must match.
    pub(super) sha256: Option<Sha256>,
}

/// A path that a ustar header cannot hold.
#[derive(Debug, thiserror::Error)]
#[error("a tar archive cannot hold the path `{path}`: {reason}")]
pub struct UnsupportedPathError {
    pub path: String,
    #[source]
    pub reason: std::io::Error,
}

/// A downloaded file whose bytes are not what the Hub listing says.
#[derive(Debug, thiserror::Error)]
pub enum BodyMismatchError {
    #[error("HuggingFace sent {actual} bytes for `{path}`, not the listed {expected}")]
    Length {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("the bytes of `{path}` hash to {actual}, not the listed {expected}")]
    Digest {
        path: String,
        expected: Sha256,
        actual: Sha256,
    },
}

/// The ustar header of a regular file, with every variable field fixed.
fn header(name: &str, size: u64) -> Result<[u8; 512], UnsupportedPathError> {
    let unsupported = |reason| UnsupportedPathError {
        path: name.to_owned(),
        reason,
    };
    let mut header = tar::Header::new_ustar();
    header.set_path(name).map_err(unsupported)?;
    header.set_size(size);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_username("").map_err(unsupported)?;
    header.set_groupname("").map_err(unsupported)?;
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    Ok(*header.as_bytes())
}

/// Refuse the first entry whose path a ustar header cannot hold.
pub(super) fn check(entries: &[TarEntry]) -> Result<(), UnsupportedPathError> {
    entries
        .iter()
        .try_for_each(|entry| header(&entry.name, entry.size).map(drop))
}

/// The zeros that pad `size` bytes to a whole block.
fn padding(size: u64) -> u64 {
    (BLOCK - size % BLOCK) % BLOCK
}

pub(super) fn archive_size(entries: &[TarEntry]) -> u64 {
    entries
        .iter()
        .map(|entry| {
            BLOCK
                .saturating_add(entry.size)
                .saturating_add(padding(entry.size))
        })
        .fold(2 * BLOCK, u64::saturating_add)
}

/// The archive of `entries`, sorted by name; `open` runs only when the archive
/// reaches that entry. Every header is built first, so a path ustar cannot hold
/// is refused before any download.
pub(super) fn archive<F, Fut>(
    mut entries: Vec<TarEntry>,
    open: F,
) -> Result<BlobBody, UnsupportedPathError>
where
    F: Fn(TarEntry) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = anyhow::Result<BlobBody>> + Send + 'static,
{
    entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let headers = entries
        .iter()
        .map(|entry| header(&entry.name, entry.size))
        .collect::<Result<Vec<_>, _>>()?;
    let open = Arc::new(open);
    let files = stream::iter(entries.into_iter().zip(headers)).flat_map(move |(entry, header)| {
        let open = Arc::clone(&open);
        let pad = padding(entry.size);
        let body = stream::once(async move {
            let opened = open(entry.clone()).await?;
            Ok::<_, anyhow::Error>(checked(entry, opened))
        })
        .try_flatten();
        stream::once(async move { Ok(Bytes::copy_from_slice(&header)) })
            .chain(body)
            .chain(stream::once(async move {
                Ok(Bytes::from(vec![0; usize::try_from(pad)?]))
            }))
    });
    let end = stream::once(async { Ok(Bytes::from_static(&END)) });
    let fused = files
        .chain(end)
        .scan(false, |failed, item: anyhow::Result<Bytes>| {
            let next = (!*failed).then_some(item);
            *failed = next.as_ref().is_some_and(Result::is_err);
            future::ready(next)
        });
    Ok(fused.boxed())
}

/// `body`, failing unless it is exactly `entry.size` bytes hashing to
/// `entry.sha256`: the header has already stated the size.
fn checked(entry: TarEntry, body: BlobBody) -> BlobBody {
    struct State {
        entry: TarEntry,
        body: BlobBody,
        seen: u64,
        digest: Option<BlobDigest>,
    }
    let digest = entry.sha256.as_ref().map(|_| BlobDigest::default());
    let state = State {
        entry,
        body,
        seen: 0,
        digest,
    };
    stream::unfold(Some(state), |state| async move {
        let mut state = state?;
        match state.body.next().await {
            Some(Ok(chunk)) => {
                state.seen += chunk.len() as u64;
                if state.seen > state.entry.size {
                    let error = length_mismatch(&state.entry, state.seen);
                    return Some((Err(error), None));
                }
                if let Some(digest) = &mut state.digest {
                    digest.update(&chunk);
                }
                Some((Ok(chunk), Some(state)))
            }
            Some(Err(e)) => Some((Err(e), None)),
            None => finish(state.entry, state.seen, state.digest).map(|e| (Err(e), None)),
        }
    })
    .boxed()
}

fn length_mismatch(entry: &TarEntry, actual: u64) -> anyhow::Error {
    anyhow::Error::new(BodyMismatchError::Length {
        path: entry.repo_path.clone(),
        expected: entry.size,
        actual,
    })
}

/// The error at the end of an entry's body, or `None` if the body is what the
/// listing says.
fn finish(entry: TarEntry, seen: u64, digest: Option<BlobDigest>) -> Option<anyhow::Error> {
    if seen != entry.size {
        return Some(length_mismatch(&entry, seen));
    }
    let (expected, digest) = entry.sha256.zip(digest)?;
    match digest.finish() {
        Ok((actual, _)) if actual == expected => None,
        Ok((actual, _)) => Some(anyhow::Error::new(BodyMismatchError::Digest {
            path: entry.repo_path,
            expected,
            actual,
        })),
        Err(e) => Some(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Read;

    use rstest::rstest;

    use super::*;

    fn sha256_of(bytes: &[u8]) -> anyhow::Result<Sha256> {
        let mut digest = BlobDigest::default();
        digest.update(bytes);
        Ok(digest.finish()?.0)
    }

    const FILES: &[(&str, &[u8])] = &[
        ("config.json", b"{}"),
        ("tokenizer/vocab.txt", b"a b c"),
        ("model.safetensors", &[7; 1500]),
        ("empty", b""),
    ];

    fn entries(order: &[usize]) -> anyhow::Result<Vec<TarEntry>> {
        order
            .iter()
            .filter_map(|&i| FILES.get(i))
            .map(|(name, bytes)| {
                Ok(TarEntry {
                    name: (*name).to_owned(),
                    repo_path: format!("4bit/{name}"),
                    size: bytes.len() as u64,
                    sha256: Some(sha256_of(bytes)?),
                })
            })
            .collect()
    }

    /// Serves each entry from [`FILES`], in chunks of 100 bytes.
    async fn open(entry: TarEntry) -> anyhow::Result<BlobBody> {
        let (_, bytes) = FILES
            .iter()
            .find(|(name, _)| *name == entry.name)
            .ok_or_else(|| anyhow::anyhow!("no file {}", entry.name))?;
        let chunks: Vec<anyhow::Result<Bytes>> = bytes
            .chunks(100)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        Ok(stream::iter(chunks).boxed())
    }

    async fn packed(entries: Vec<TarEntry>) -> anyhow::Result<Vec<u8>> {
        let chunks: Vec<Bytes> = archive(entries, open)?.try_collect().await?;
        Ok(chunks.concat())
    }

    /// The archive does not depend on the Hub's listing order, has the size
    /// [`archive_size`] gives, and unpacks with fixed metadata.
    #[tokio::test]
    async fn one_file_list_gives_one_archive() -> anyhow::Result<()> {
        let first = packed(entries(&[0, 1, 2, 3])?).await?;
        let second = packed(entries(&[3, 2, 1, 0])?).await?;
        assert_eq!(first, second);
        assert_eq!(first.len() as u64, archive_size(&entries(&[0, 1, 2, 3])?));

        let mut tar = tar::Archive::new(first.as_slice());
        let unpacked = tar
            .entries()?
            .map(|entry| {
                let mut entry = entry?;
                let header = entry.header();
                assert_eq!(header.mode()?, 0o644);
                assert_eq!(header.mtime()?, 0);
                assert_eq!((header.uid()?, header.gid()?), (0, 0));
                assert_eq!(header.username()?, Some(""));
                let path = entry.path()?.to_string_lossy().into_owned();
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                anyhow::Ok((path, bytes))
            })
            .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
        let expected: BTreeMap<String, Vec<u8>> = FILES
            .iter()
            .map(|(name, bytes)| ((*name).to_owned(), bytes.to_vec()))
            .collect();
        assert_eq!(unpacked, expected);

        let dir = tempfile::tempdir()?;
        tar::Archive::new(first.as_slice()).unpack(dir.path())?;
        assert_eq!(
            std::fs::read(dir.path().join("tokenizer/vocab.txt"))?,
            b"a b c"
        );
        Ok(())
    }

    /// The refusal comes before any entry opens.
    #[test]
    fn a_path_that_ustar_cannot_hold_is_refused() {
        let entry = TarEntry {
            name: "a".repeat(300),
            repo_path: "a".repeat(300),
            size: 0,
            sha256: None,
        };
        let refused = archive(vec![entry], open);
        assert!(
            matches!(&refused, Err(UnsupportedPathError { path, .. }) if path.len() == 300),
            "{:?}",
            refused.err()
        );
    }

    /// A listed size near `u64::MAX` gives the largest size, not a small one.
    #[test]
    fn a_huge_listed_size_saturates() {
        let entry = TarEntry {
            name: "a".to_owned(),
            repo_path: "a".to_owned(),
            size: u64::MAX - 1,
            sha256: None,
        };
        assert_eq!(archive_size(&[entry]), u64::MAX);
    }

    /// After an entry fails, the archive gives no more items and opens no
    /// later entry, even if its consumer reads on.
    #[tokio::test]
    async fn the_archive_ends_at_its_first_error() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let opened = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&opened);
        let failing = move |entry: TarEntry| {
            counter.fetch_add(1, Ordering::SeqCst);
            async move { Err::<BlobBody, _>(anyhow::anyhow!("no file {}", entry.name)) }
        };
        let items: Vec<anyhow::Result<Bytes>> =
            archive(entries(&[0, 1, 2])?, failing)?.collect().await;
        assert_eq!(items.iter().filter(|item| item.is_err()).count(), 1);
        assert!(items.last().is_some_and(Result::is_err));
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[rstest]
    #[case::short(1, None)]
    #[case::long(3, None)]
    #[case::other_bytes(2, Some('0'))]
    #[tokio::test]
    async fn a_body_unlike_its_listing_fails(
        #[case] size: u64,
        #[case] digest: Option<char>,
    ) -> anyhow::Result<()> {
        let sha256 = match digest {
            Some(c) => Sha256::try_new(c.to_string().repeat(64))?,
            None => sha256_of(b"{}")?,
        };
        let entry = TarEntry {
            name: "config.json".to_owned(),
            repo_path: "config.json".to_owned(),
            size,
            sha256: Some(sha256),
        };
        let failed = packed(vec![entry]).await;
        let mismatch = failed
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<BodyMismatchError>());
        assert!(mismatch.is_some(), "{failed:?}");
        Ok(())
    }
}
