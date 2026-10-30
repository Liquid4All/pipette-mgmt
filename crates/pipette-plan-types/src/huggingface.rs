//! A HuggingFace repo coordinate, the token a gated one is fetched with, and
//! the paths inside a repo.

use nutype::nutype;
use serde::{Deserialize, Serialize};

#[nutype(
    validate(regex = r"^[A-Za-z0-9][A-Za-z0-9._-]*$"),
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
pub struct HfOrg(String);

#[nutype(
    validate(regex = r"^[A-Za-z0-9][A-Za-z0-9._-]*$"),
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
pub struct HfRepoName(String);

/// A path relative to a repo root, possibly nested: no leading slash and no
/// `.`/`..` segments, so it can't be absolute or escape the repo.
fn is_relative_repo_subpath(path: &str) -> bool {
    path.split('/').all(|seg| {
        !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    })
}

/// A path relative to an HF repo root — a file (e.g. `Q4_K_M.gguf`) or a
/// subdirectory (e.g. `4bit`). No leading slash and no `.`/`..` segments, so
/// it can't be absolute or escape the repo.
#[nutype(
    validate(predicate = is_relative_repo_subpath),
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
pub struct RepoSubpath(String);

/// A HuggingFace repo revision: a git tag, branch, or commit SHA, which pins a
/// repo to a snapshot. Only ref-shaped is checked; HF resolves the rest.
#[nutype(
    validate(regex = r"^[A-Za-z0-9][A-Za-z0-9._/-]*$"),
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
pub struct HfRevision(String);

/// A HuggingFace access token for a gated/private repo, carried in the plan.
///
/// Secret (`docs/architecture.md`): `Debug` and `Display` render `<redacted>`,
/// so [`AsRef`] is the one door to the raw value.
#[nutype(
    sanitize(trim),
    validate(not_empty),
    derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, AsRef)
)]
pub struct AuthToken(String);

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthToken(<redacted>)")
    }
}

impl std::fmt::Display for AuthToken {
    /// Present rather than omitted so `{token}` compiles to something safe: an
    /// author who needs `{}` is never pushed to the raw-value accessor to get it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The one environment variable the CLI consults for an HF access token, which
/// [`inject_hf_auth_token`](crate::inject_hf_auth_token) puts into a gated,
/// tokenless model. Not the generic `HF_TOKEN`, so a token only ever enters via
/// the model spec.
pub const HF_TOKEN_ENV: &str = "PIPETTE_HF_TOKEN";

/// HuggingFace repo coordinate.  Flattened into every `Model` variant
/// so TOML stays `{ type = "...", org = "...", repo_name = "...", ... }`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct HfRepo {
    pub org: HfOrg,
    pub repo_name: HfRepoName,
    /// Optional pin to a repo snapshot (tag/branch/commit); `None` is the
    /// default branch, which moves. Part of the identity
    /// ([`HfRepo::reference`]), so cells pinning other revisions don't collide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<HfRevision>,
    /// Access token for a gated/private repo; `None` for public repos. Not part
    /// of the repo's identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    // A schema reader never needs a token, so its generated types never offer one.
    #[cfg_attr(feature = "schemars", schemars(skip))]
    pub auth_token: Option<AuthToken>,
}

/// Why a string is not an `org/repo_name` HuggingFace repo.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HfRepoError {
    #[error("HF repo must be in the form `org/repo_name`")]
    MissingSeparator,
    #[error("invalid HF org: {0}")]
    Org(#[from] HfOrgError),
    #[error("invalid HF repo name: {0}")]
    RepoName(#[from] HfRepoNameError),
}

impl HfRepo {
    /// Parse an `org/repo_name` string; the one parser across the workspace.
    pub fn parse_org_repo(org_repo: &str) -> Result<Self, HfRepoError> {
        let (org, name) = org_repo
            .split_once('/')
            .ok_or(HfRepoError::MissingSeparator)?;
        Ok(Self {
            org: HfOrg::try_new(org)?,
            repo_name: HfRepoName::try_new(name)?,
            revision: None,
            auth_token: None,
        })
    }

    /// Identity form, `org/repo_name[@revision]`, for model references,
    /// warehouse keys and logs. [`Display`](std::fmt::Display) stays the bare
    /// id the HF client wants, which takes the revision separately.
    pub fn reference(&self) -> String {
        match &self.revision {
            Some(rev) => format!("{self}@{rev}"),
            None => self.to_string(),
        }
    }
}

impl std::fmt::Display for HfRepo {
    /// Canonical `org/repo_name`, as HF clients accept it. Excludes the
    /// revision, an identity concern: see [`HfRepo::reference`].
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.org, self.repo_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both rendering traits hide the value, so a token escapes only via
    /// `as_ref`.
    #[test]
    fn auth_token_renders_redacted_in_both_traits() -> anyhow::Result<()> {
        const SECRET: &str = "hf_tokenthatmustnotescape";
        let token = AuthToken::try_new(SECRET.to_owned())?;
        assert_eq!(format!("{token:?}"), "AuthToken(<redacted>)");
        assert_eq!(format!("{token}"), "<redacted>");
        assert_eq!(token.as_ref(), SECRET, "as_ref stays the one way in");
        Ok(())
    }

    /// A token pasted into a plan with a trailing newline must not reach the
    /// `Authorization` header with it.
    #[test]
    fn auth_token_trims_surrounding_whitespace() -> anyhow::Result<()> {
        let token: AuthToken = serde_json::from_str(r#"" hf_pasted\n""#)?;
        assert_eq!(token.as_ref(), "hf_pasted");
        assert!(AuthToken::try_new(" \n".to_owned()).is_err());
        Ok(())
    }

    #[rstest::rstest]
    #[case("meta-llama", true)]
    #[case("LiquidAI", true)]
    #[case("a", true)]
    #[case("x_y.z-0", true)]
    #[case("0digits-can-start", true)]
    #[case("with..double-dot", true)]
    #[case("", false)]
    #[case("bad org!", false)]
    #[case("-leading-dash", false)]
    #[case(".leading-dot", false)]
    #[case("_leading-underscore", false)]
    #[case("trailing space ", false)]
    fn hf_org_validation(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(HfOrg::try_new(s.to_owned()).is_ok(), ok, "{s:?}");
    }

    #[test]
    fn hf_repo_parse_accepts_canonical() -> anyhow::Result<()> {
        let parsed = HfRepo::parse_org_repo("mlx-community/LFM2-350M-4bit")?;
        assert_eq!(parsed.org.to_string(), "mlx-community");
        assert_eq!(parsed.repo_name.to_string(), "LFM2-350M-4bit");
        assert_eq!(parsed.to_string(), "mlx-community/LFM2-350M-4bit");
        Ok(())
    }

    #[rstest::rstest]
    #[case::missing_separator("just-a-name", HfRepoError::MissingSeparator)]
    #[case::leading_dot_org(".bad/repo", HfRepoError::Org(HfOrgError::RegexViolated))]
    #[case::empty_repo_name("good/", HfRepoError::RepoName(HfRepoNameError::RegexViolated))]
    fn hf_repo_parse_refusals(#[case] org_repo: &str, #[case] expected: HfRepoError) {
        assert_eq!(HfRepo::parse_org_repo(org_repo).err(), Some(expected));
    }

    #[rstest::rstest]
    #[case::missing_separator(
        HfRepoError::MissingSeparator,
        "HF repo must be in the form `org/repo_name`".to_owned()
    )]
    #[case::org(
        HfRepoError::Org(HfOrgError::RegexViolated),
        format!("invalid HF org: {}", HfOrgError::RegexViolated)
    )]
    #[case::repo_name(
        HfRepoError::RepoName(HfRepoNameError::RegexViolated),
        format!("invalid HF repo name: {}", HfRepoNameError::RegexViolated)
    )]
    fn hf_repo_error_messages(#[case] error: HfRepoError, #[case] rendered: String) {
        assert_eq!(error.to_string(), rendered);
    }

    #[rstest::rstest]
    #[case("main", true)]
    #[case("v1.0", true)]
    #[case("refs/pr/3", true)]
    #[case("a1b2c3d4e5f6", true)]
    #[case("", false)]
    #[case("-leading-dash", false)]
    #[case("has space", false)]
    fn hf_revision_validation(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(HfRevision::try_new(s.to_owned()).is_ok(), ok, "{s:?}");
    }

    #[test]
    fn hf_repo_reference_appends_revision_but_display_stays_bare() -> anyhow::Result<()> {
        let mut repo = HfRepo::parse_org_repo("mlx-community/LFM2-350M-4bit")?;
        // No revision: reference() and Display are the bare repo id.
        assert_eq!(repo.reference(), "mlx-community/LFM2-350M-4bit");
        assert_eq!(repo.to_string(), repo.reference());

        // Pinned revision: identity carries `@rev`, Display does not (that's
        // the id the HF client consumes; the revision goes to it separately).
        repo.revision = Some(HfRevision::try_new("v2".to_owned())?);
        assert_eq!(repo.reference(), "mlx-community/LFM2-350M-4bit@v2");
        assert_eq!(repo.to_string(), "mlx-community/LFM2-350M-4bit");
        Ok(())
    }

    #[rstest::rstest]
    #[case("4bit", true)] // a subdirectory
    #[case("variants/mlx-4bit", true)] // nested subdirectory
    #[case("Q4_K_M.gguf", true)] // a file — extension-agnostic
    #[case("dir/weights.gguf", true)] // nested file
    #[case("weights.safetensors", true)] // any extension, not just .gguf
    #[case("a.b_c-d/e", true)]
    #[case("", false)]
    #[case(r"win\weights.gguf", false)] // backslash not a valid segment char
    #[case("/abs", false)] // absolute — must be relative to the repo
    #[case("../escape", false)] // parent traversal
    #[case("dir/../seg", false)] // traversal mid-path
    #[case("dir//seg", false)] // empty segment
    #[case("dir/./seg", false)] // dot segment
    fn repo_subpath_validation(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(RepoSubpath::try_new(s.to_owned()).is_ok(), ok, "{s:?}");
    }
}
