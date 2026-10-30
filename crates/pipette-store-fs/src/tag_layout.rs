//! Filesystem layout for [`super::LocalFsTagStore`]: an empty marker file per
//! `(client, tag)` at `{by_client_dir}/{client_id}/{tag}` and at
//! `{by_tag_dir}/{tag}/{client_id}`. Tags and ids hold no `/`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use pipette_core::types::id_from_key;
use pipette_plan_types::{ClientId, Tag};

fn forward_tag_path(by_client_dir: &Path, client_id: &ClientId, tag: &Tag) -> PathBuf {
    by_client_dir.join(client_id.as_str()).join(tag.as_str())
}

fn reverse_tag_path(by_tag_dir: &Path, tag: &Tag, client_id: &ClientId) -> PathBuf {
    by_tag_dir.join(tag.as_str()).join(client_id.as_str())
}

fn touch_marker(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, [])?;
    Ok(())
}

fn remove_marker(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    // Best-effort: drop the now-empty parent dir so an emptied tag/client leaves
    // no stray directory.
    if let Some(parent) = path.parent() {
        crate::drop_empty_dir(parent);
    }
    Ok(())
}

/// Add a `(client, tag)` membership — writes both markers; idempotent.
pub(super) fn add_tag(
    by_client_dir: &Path,
    by_tag_dir: &Path,
    client_id: &ClientId,
    tag: &Tag,
) -> anyhow::Result<()> {
    touch_marker(&forward_tag_path(by_client_dir, client_id, tag))?;
    touch_marker(&reverse_tag_path(by_tag_dir, tag, client_id))?;
    Ok(())
}

/// Remove a `(client, tag)` membership from both trees. Idempotent.
pub(super) fn remove_tag(
    by_client_dir: &Path,
    by_tag_dir: &Path,
    client_id: &ClientId,
    tag: &Tag,
) -> anyhow::Result<()> {
    remove_marker(&forward_tag_path(by_client_dir, client_id, tag))?;
    remove_marker(&reverse_tag_path(by_tag_dir, tag, client_id))?;
    Ok(())
}

/// Read one entry level of a marker directory, returning the leaf names parsed
/// through `parse` (corrupt entries are logged and skipped). Missing dir → empty.
fn list_marker_names<T, E, F>(dir: &Path, parse: F) -> anyhow::Result<Vec<T>>
where
    E: std::fmt::Display,
    F: Fn(&str) -> Result<T, E>,
{
    if !dir.exists() {
        return Ok(Vec::new());
    }
    std::fs::read_dir(dir)?
        .map(|entry| Ok(entry?.path()))
        .filter_map(|path: anyhow::Result<PathBuf>| match path {
            Ok(p) => {
                let name = p.file_name().and_then(|n| n.to_str())?;
                parse(name)
                    .inspect_err(|e| {
                        tracing::warn!(
                            path = %p.display(),
                            error = %e,
                            "skipping unparseable tag-index entry"
                        );
                    })
                    .ok()
                    .map(Ok)
            }
            Err(e) => Some(Err(e)),
        })
        .collect()
}

pub(super) fn list_client_tags(
    by_client_dir: &Path,
    client_id: &ClientId,
) -> anyhow::Result<BTreeSet<Tag>> {
    let dir = by_client_dir.join(client_id.as_str());
    Ok(list_marker_names(&dir, |name| Tag::try_new(name))?
        .into_iter()
        .collect())
}

pub(super) fn list_client_ids_by_tag(
    by_tag_dir: &Path,
    tag: &Tag,
) -> anyhow::Result<Vec<ClientId>> {
    let dir = by_tag_dir.join(tag.as_str());
    let mut ids = list_marker_names(&dir, |name| id_from_key(name, ClientId::try_new))?;
    ids.sort();
    Ok(ids)
}

/// Every `(client_id, tag)` in the forward tree (`{by_client_dir}/{id}/{tag}`),
/// for the reverse-index reconcile.
pub(super) fn list_all_forward_markers(
    by_client_dir: &Path,
) -> anyhow::Result<Vec<(ClientId, Tag)>> {
    list_marker_names(by_client_dir, |name| id_from_key(name, ClientId::try_new))?
        .into_iter()
        .map(|id| -> anyhow::Result<Vec<(ClientId, Tag)>> {
            let tags =
                list_marker_names(&by_client_dir.join(id.as_str()), |name| Tag::try_new(name))?;
            Ok(tags.into_iter().map(|tag| (id.clone(), tag)).collect())
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(|nested| nested.into_iter().flatten().collect())
}

/// Every `(client_id, tag)` in the reverse tree (`{by_tag_dir}/{tag}/{id}`), for
/// the reconcile.
pub(super) fn list_all_reverse_markers(by_tag_dir: &Path) -> anyhow::Result<Vec<(ClientId, Tag)>> {
    list_marker_names(by_tag_dir, |name| Tag::try_new(name))?
        .into_iter()
        .map(|tag| -> anyhow::Result<Vec<(ClientId, Tag)>> {
            let ids = list_marker_names(&by_tag_dir.join(tag.as_str()), |name| {
                id_from_key(name, ClientId::try_new)
            })?;
            Ok(ids.into_iter().map(|id| (id, tag.clone())).collect())
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .map(|nested| nested.into_iter().flatten().collect())
}

/// Remove every tag membership for a client (used on delete): drop each reverse
/// marker, then the client's whole forward directory.
pub(super) fn delete_all_tags(
    by_client_dir: &Path,
    by_tag_dir: &Path,
    client_id: &ClientId,
) -> anyhow::Result<()> {
    list_client_tags(by_client_dir, client_id)?
        .iter()
        .try_for_each(|tag| remove_marker(&reverse_tag_path(by_tag_dir, tag, client_id)))?;
    match std::fs::remove_dir_all(by_client_dir.join(client_id.as_str())) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tag_markers_both_directions() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let by_client = dir.path().join("tags-index/by-client");
        let by_tag = dir.path().join("tags-index/by-tag");
        let a = ClientId::try_new("ev1_a")?;
        let b = ClientId::try_new("ev1_b")?;
        let team = Tag::try_new("team-mobile")?;
        let east = Tag::try_new("us-east")?;

        add_tag(&by_client, &by_tag, &a, &team)?;
        add_tag(&by_client, &by_tag, &a, &east)?;
        add_tag(&by_client, &by_tag, &b, &team)?;
        // Idempotent re-add.
        add_tag(&by_client, &by_tag, &a, &team)?;

        assert_eq!(
            list_client_tags(&by_client, &a)?,
            BTreeSet::from([team.clone(), east.clone()])
        );
        assert_eq!(
            list_client_ids_by_tag(&by_tag, &team)?,
            vec![a.clone(), b.clone()]
        );
        assert_eq!(list_client_ids_by_tag(&by_tag, &east)?, vec![a.clone()]);

        remove_tag(&by_client, &by_tag, &a, &east)?;
        assert!(list_client_ids_by_tag(&by_tag, &east)?.is_empty());
        assert_eq!(
            list_client_tags(&by_client, &a)?,
            BTreeSet::from([team.clone()])
        );

        delete_all_tags(&by_client, &by_tag, &b)?;
        assert_eq!(list_client_ids_by_tag(&by_tag, &team)?, vec![a.clone()]);
        assert!(list_client_tags(&by_client, &b)?.is_empty());

        assert!(list_client_ids_by_tag(&by_tag, &Tag::try_new("nope")?)?.is_empty());
        Ok(())
    }
}
