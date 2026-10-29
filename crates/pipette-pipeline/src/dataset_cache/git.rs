//! Upstream access for `source = "git"`. Every command ignores user and system
//! git config, disables prompts and follows no redirect, so a checkout depends
//! on the pin alone and reaches only the host it names.
//! No deadline here: the caller's timeout drops the future and kills the child.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use pipette_plan_types::{DatasetPath, GitOid};

/// What the upstream said about a pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reached {
    /// The pinned revision is fetchable; this is the tree id of its subpath.
    Tree(GitOid),
    /// The repository, revision or subpath is missing or not public.
    Gone(String),
    /// Network failure, timeout, or an upstream fault.
    Unavailable(String),
}

#[derive(Debug, thiserror::Error)]
enum GitError {
    #[error("upstream gone: {0}")]
    Gone(String),
    #[error("upstream unavailable: {0}")]
    Unavailable(String),
}

impl From<GitError> for Reached {
    fn from(f: GitError) -> Self {
        match f {
            GitError::Gone(d) => Reached::Gone(d),
            GitError::Unavailable(d) => Reached::Unavailable(d),
        }
    }
}

/// stderr of a thing that does not exist or is not readable anonymously, as
/// opposed to a transport failure.
const GONE_MARKERS: &[&str] = &[
    "not our ref",
    "couldn't find remote ref",
    "could not read username",
    "authentication failed",
    "repository not found",
    "does not appear to be a git repository",
    "returned error: 403",
    "returned error: 404",
    "returned error: 410",
];

fn classify(stderr: &str) -> GitError {
    let lower = stderr.to_ascii_lowercase();
    let detail = stderr.trim().lines().last().unwrap_or_default().to_string();
    if GONE_MARKERS.iter().any(|m| lower.contains(m)) {
        GitError::Gone(detail)
    } else {
        GitError::Unavailable(detail)
    }
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.autocrlf=false",
            "-c",
            "advice.detachedHead=false",
            "-c",
            "http.followRedirects=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .kill_on_drop(true);
    let output = cmd
        .output()
        .await
        .map_err(|e| GitError::Unavailable(format!("running git: {e}")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(classify(&String::from_utf8_lossy(&output.stderr)))
    }
}

/// Fetch the pinned commit's trees, no blobs.
async fn fetch_trees(dir: &Path, remote: &str, revision: &GitOid) -> Result<(), GitError> {
    std::fs::create_dir_all(dir).map_err(|e| GitError::Unavailable(format!("scratch dir: {e}")))?;
    git(dir, &["init", "-q"]).await?;
    git(dir, &["remote", "add", "origin", remote]).await?;
    git(
        dir,
        &[
            "fetch",
            "-q",
            "--depth",
            "1",
            "--filter=blob:none",
            "origin",
            revision.as_ref(),
        ],
    )
    .await?;
    Ok(())
}

/// The tree id of `revision:subpath` in an initialized scratch repo.
async fn tree_id(
    dir: &Path,
    revision: &GitOid,
    subpath: Option<&DatasetPath>,
) -> Result<GitOid, GitError> {
    let spec = match subpath {
        Some(subpath) => format!("{revision}:{subpath}"),
        None => format!("{revision}^{{tree}}"),
    };
    // `rev-parse -q` is silent on a missing path, so a non-zero exit with no
    // stderr is an absent path.
    let oid = git(dir, &["rev-parse", "--verify", "-q", &spec])
        .await
        .map_err(|f| match f {
            GitError::Gone(_) => GitError::Gone(format!("{spec} does not resolve")),
            GitError::Unavailable(d) if d.is_empty() => {
                GitError::Gone(format!("{spec} does not resolve"))
            }
            GitError::Unavailable(d) => GitError::Unavailable(d),
        })?;
    let kind = git(dir, &["cat-file", "-t", &oid]).await?;
    if kind != "tree" {
        return Err(GitError::Gone(format!(
            "{spec} is a {kind}, not a directory"
        )));
    }
    GitOid::try_new(oid).map_err(|e| GitError::Unavailable(format!("git printed a bad oid: {e}")))
}

/// Is the revision fetchable at `remote`, and what is its subpath's tree id?
/// No blob is fetched. `scratch` is created and left for the caller to remove.
pub(crate) async fn probe(
    scratch: &Path,
    remote: &str,
    revision: &GitOid,
    subpath: Option<&DatasetPath>,
) -> Reached {
    if let Err(f) = fetch_trees(scratch, remote, revision).await {
        return f.into();
    }
    match tree_id(scratch, revision, subpath).await {
        Ok(oid) => Reached::Tree(oid),
        Err(f) => f.into(),
    }
}

/// Check out the pinned subpath from `remote` and return its root and tree
/// id. Only that subpath's blobs are fetched.
pub(crate) async fn checkout(
    scratch: &Path,
    remote: &str,
    revision: &GitOid,
    subpath: Option<&DatasetPath>,
) -> anyhow::Result<(PathBuf, GitOid)> {
    fetch_trees(scratch, remote, revision).await?;
    let oid = tree_id(scratch, revision, subpath).await?;
    if let Some(subpath) = subpath {
        let pattern = format!("/{subpath}/");
        git(scratch, &["sparse-checkout", "set", "--no-cone", &pattern]).await?;
    }
    git(scratch, &["checkout", "-q", revision.as_ref()]).await?;
    let root = match subpath {
        Some(subpath) => scratch.join(subpath.as_ref()),
        None => scratch.to_path_buf(),
    };
    Ok((root, oid))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::missing_sha("fatal: remote error: upload-pack: not our ref 1111", true)]
    #[case::private_or_deleted(
        "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
        true
    )]
    #[case::http_404(
        "fatal: unable to access 'x': The requested URL returned error: 404",
        true
    )]
    #[case::dns(
        "fatal: unable to access 'https://nosuchhost.invalid/a/b/': Could not resolve host",
        false
    )]
    #[case::server_error(
        "fatal: unable to access 'x': The requested URL returned error: 503",
        false
    )]
    fn classifies_gone_apart_from_unavailable(#[case] stderr: &str, #[case] gone: bool) {
        assert_eq!(
            matches!(classify(stderr), GitError::Gone(_)),
            gone,
            "{stderr}"
        );
    }

    /// A local `file://` repository stands in for the upstream: the tree id is
    /// git's own, so both calls must agree with `git rev-parse` run by hand.
    #[tokio::test]
    async fn probe_and_checkout_agree_with_git() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let origin = dir.path().join("origin");
        std::fs::create_dir_all(origin.join("data/test"))?;
        std::fs::write(origin.join("data/test/a.txt"), b"a")?;
        std::fs::write(origin.join("other.txt"), b"not in the subpath")?;
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
        let expected = GitOid::try_new(run(&["rev-parse", "HEAD:data/test"])?)?;
        let remote = format!("file://{}", origin.display());
        let subpath = DatasetPath::try_new("data/test")?;

        let probed = probe(
            &dir.path().join("probe"),
            &remote,
            &revision,
            Some(&subpath),
        )
        .await;
        assert_eq!(probed, Reached::Tree(expected.clone()));

        let missing = DatasetPath::try_new("data/nope")?;
        let gone = probe(
            &dir.path().join("probe-missing"),
            &remote,
            &revision,
            Some(&missing),
        )
        .await;
        assert!(matches!(gone, Reached::Gone(_)), "{gone:?}");

        let (root, oid) = checkout(
            &dir.path().join("checkout"),
            &remote,
            &revision,
            Some(&subpath),
        )
        .await?;
        assert_eq!(oid, expected);
        assert_eq!(std::fs::read(root.join("a.txt"))?, b"a");
        assert!(!dir.path().join("checkout/other.txt").exists());
        Ok(())
    }
}
