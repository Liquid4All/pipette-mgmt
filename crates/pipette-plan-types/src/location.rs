//! Where an artifact's bytes live (a URL, a vault route, a path) and the
//! digest that checks them. Model and runtime sources are built from these.

use nutype::nutype;
use serde::{Deserialize, Serialize};

use crate::{PrincipalIdError, VaultResourceId};

/// A URL locating a fetchable file or directory: `http(s)://` for a download,
/// `file://` for a local path. For llama.cpp remote archives, use
/// [`RemoteArchiveUrl`] (http(s) only).
#[nutype(
    sanitize(trim),
    validate(regex = r"^(https?|file)://.+"),
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
pub struct ResourceUrl(String);

/// Where the management server serves a vault artifact, **relative to the
/// server**: `/vault/resources/{resource_id}/content`. Never an absolute URL:
/// a plan that spelled a host could point a "vault" fetch anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(try_from = "String", into = "String")]
pub struct VaultPath {
    resource_id: VaultResourceId,
}

impl VaultPath {
    const PREFIX: &'static str = "/vault/resources/";
    const SUFFIX: &'static str = "/content";

    pub fn of(resource_id: VaultResourceId) -> Self {
        Self { resource_id }
    }

    pub fn resource_id(&self) -> &VaultResourceId {
        &self.resource_id
    }
}

/// Why a value is not a vault path. The route is checked before the id, so a
/// refusal names whichever was wrong.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VaultPathError {
    #[error("a vault path is `/vault/resources/{{resource_id}}/content`, relative to the server")]
    Shape,
    #[error(transparent)]
    ResourceId(#[from] PrincipalIdError),
}

impl std::str::FromStr for VaultPath {
    type Err = VaultPathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let id = s
            .trim()
            .strip_prefix(Self::PREFIX)
            .and_then(|rest| rest.strip_suffix(Self::SUFFIX))
            .ok_or(VaultPathError::Shape)?;
        Ok(Self {
            resource_id: VaultResourceId::try_new(id)?,
        })
    }
}

impl TryFrom<String> for VaultPath {
    type Error = VaultPathError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl std::fmt::Display for VaultPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}{}", Self::PREFIX, self.resource_id, Self::SUFFIX)
    }
}

impl From<VaultPath> for String {
    fn from(path: VaultPath) -> Self {
        path.to_string()
    }
}

/// Prefix a scheme-less `host/path` with `https://`, with a warning, so older
/// plans keep parsing. An existing `scheme://` (even `file://`) passes through,
/// so validation rejects the real scheme, not a doubly-prefixed string. Only a
/// `://` before the first `/` counts; later it is path or query
/// (`ex.com/a.tgz?to=https://cdn/x`).
fn default_https_scheme(raw: String) -> String {
    let s = raw.trim();
    let has_scheme = s
        .split_once("://")
        .is_some_and(|(scheme, _)| !scheme.is_empty() && !scheme.contains('/'));
    if s.is_empty() || has_scheme {
        return s.to_owned();
    }
    log::warn!(
        "remote archive URL `{s}` has no scheme; assuming `https://`. \
         Write the scheme in the plan to say which one you want."
    );
    format!("https://{s}")
}

/// Absolute `http(s)://host/path/…` form of a remote prebuilt archive. Host
/// and path are required, which also rejects a local path: the sanitizer
/// turns `/tmp/a.tgz` into `https:///tmp/a.tgz`, leaving no host.
fn is_remote_archive_url(s: &str) -> bool {
    let Some(rest) = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
    else {
        return false;
    };
    !rest.starts_with('/') && rest.contains('/') && !rest.chars().any(char::is_whitespace)
}

/// Remote archive download URL, e.g. `https://example.com/path/a.tar.gz`; a
/// scheme-less spelling is stored with `https://` prepended.
#[nutype(
    sanitize(with = default_https_scheme),
    validate(predicate = is_remote_archive_url),
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
pub struct RemoteArchiveUrl(String);

/// Sanitizer: drop a redundant leading `./` so the stored path is normalized.
fn strip_leading_current_dir(raw: String) -> String {
    raw.strip_prefix("./").map(str::to_owned).unwrap_or(raw)
}

/// Portable store/entry-relative path only. No absolute Unix/Windows forms,
/// no `~`, no `.`/`..` segments. Leading `./` is stripped by the sanitizer first.
fn is_relative_fs_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('~') || path.starts_with('/') {
        return false;
    }
    // Windows drive absolute: `C:\…` or `C:/…`.
    if matches!(path.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic()) {
        return false;
    }
    // UNC `\\server\share` or `//server/share`.
    if path.starts_with("\\\\") || path.starts_with("//") {
        return false;
    }
    path.split(['/', '\\'])
        .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// Absolute host path only (Unix `/…`, Windows `C:\…` / `C:/…`, optional UNC).
/// No `~`, no `.`/`..` segments. Leading `./` is stripped (then rejected if not abs).
fn is_absolute_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('~') {
        return false;
    }
    let is_abs = path.starts_with('/')
        || path.starts_with("\\\\")
        || path.starts_with("//")
        || matches!(
            path.as_bytes(),
            [drive, b':', b'/' | b'\\', ..] if drive.is_ascii_alphabetic()
        );
    if !is_abs {
        return false;
    }
    // Normalize check on body after the absolute prefix (UNC before bare `/`).
    let body = if let Some(rest) = path
        .strip_prefix("\\\\")
        .or_else(|| path.strip_prefix("//"))
    {
        if !rest.split(['/', '\\']).take(2).all(|s| !s.is_empty()) {
            return false;
        }
        rest
    } else if let Some(rest) = path.strip_prefix('/') {
        rest
    } else {
        // C:\rest or C:/rest
        let Some(rest) = path.get(3..) else {
            return false;
        };
        rest
    };
    body.is_empty()
        || body
            .split(['/', '\\'])
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// Portable relative filesystem path (store / entry layout). Never absolute.
/// Host-absolute paths use [`AbsolutePath`].
#[nutype(
    sanitize(with = strip_leading_current_dir),
    validate(predicate = is_relative_fs_path),
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
pub struct RelativePath(String);

/// Absolute host filesystem path (Unix `/…`, Windows `C:\…` / `C:/…`, UNC).
/// Runnable paths after ensure/bind — not portable store coordinates.
#[nutype(
    sanitize(with = strip_leading_current_dir),
    validate(predicate = is_absolute_path),
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
pub struct AbsolutePath(String);

/// A file's SHA-256 content hash (64 lowercase hex chars): the same bytes
/// wherever they were fetched from.
#[nutype(
    validate(regex = r"^[0-9a-f]{64}$"),
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
pub struct Sha256(String);

/// A file's own name, one path segment: what a listing shows and a download
/// is saved as.
#[nutype(
    validate(with = check_file_name, error = FileNameError),
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
pub struct FileName(String);

/// `[A-Za-z0-9._-]`, starting with a letter or digit so it is never `.`, `..`
/// or hidden, and at most 255 characters.
fn check_file_name(name: &str) -> Result<(), FileNameError> {
    let mut chars = name.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        // ASCII by now, so bytes are characters.
        && name.len() <= 255;
    if valid {
        Ok(())
    } else {
        Err(FileNameError(name.to_string()))
    }
}

/// A name [`FileName`] refuses. The message quotes it: a variant's files
/// sit in a flattened body, so the refusal reaches the caller naming no field.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "file name `{}` must be one path segment of [A-Za-z0-9._-], starting with a letter or \
     digit, at most 255 characters",
    shown(.0)
)]
pub struct FileNameError(String);

/// `str::escape_debug` minus its `\\`, `\'` and `\"` escapes, so the refusal
/// quotes a name as it is on disk and escapes only what cannot be printed.
fn shown(name: &str) -> String {
    let mut shown = String::with_capacity(name.len());
    let mut escaped = name.escape_debug();
    while let Some(c) = escaped.next() {
        if c != '\\' {
            shown.push(c);
            continue;
        }
        match escaped.next() {
            Some(bare @ ('\\' | '\'' | '"')) => shown.push(bare),
            next => {
                shown.push('\\');
                shown.extend(next);
            }
        }
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case("https://example.com/a.gguf", true)]
    #[case("http://example.com/a.gguf", true)]
    #[case("file:///models/a.gguf", true)]
    #[case("ftp://example.com/a.gguf", false)] // unsupported scheme
    #[case("example.com/a.gguf", false)] // scheme-less
    #[case("", false)]
    #[case("   ", false)]
    fn resource_url_validation(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(ResourceUrl::try_new(s.to_owned()).is_ok(), ok, "{s:?}");
    }

    #[test]
    fn resource_url_trims_surrounding_whitespace() -> anyhow::Result<()> {
        let url = ResourceUrl::try_new(" https://example.com/a.gguf\n".to_owned())?;
        assert_eq!(url.as_ref(), "https://example.com/a.gguf");
        Ok(())
    }

    #[rstest::rstest]
    #[case("https://example.com/a.tar.gz", true)]
    #[case("http://example.com/a.tar.gz", true)]
    #[case("example.com/a.tar.gz", true)] // scheme-less: defaulted to https://
    #[case("https://example.com", false)] // host without a path
    #[case("example.com", false)] // host without a path, scheme-less
    #[case("https:///a.tar.gz", false)] // path without a host
    #[case("file:///tmp/a.tar.gz", false)]
    #[case("ftp://example.com/a.tar.gz", false)]
    #[case("/absolute/path.tgz", false)] // a local path is not a remote archive
    #[case("", false)]
    #[case("https://has space/a.tgz", false)]
    #[case("has space/a.tgz", false)]
    #[case("ex.com/a.tgz?to=https://cdn.example.com/x", true)] // query, not a scheme
    fn remote_archive_url_validation(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(RemoteArchiveUrl::try_new(s.to_owned()).is_ok(), ok, "{s:?}");
    }

    /// Every accepted spelling stores the absolute URL a fetcher will download.
    #[rstest::rstest]
    #[case(
        "  https://cdn.example.com/llama.tar.gz ",
        "https://cdn.example.com/llama.tar.gz"
    )]
    #[case("cdn.example.com/llama.tar.gz", "https://cdn.example.com/llama.tar.gz")]
    #[case(
        "http://cdn.example.com/llama.tar.gz",
        "http://cdn.example.com/llama.tar.gz"
    )]
    fn remote_archive_url_normalizes_to_an_absolute_url(
        #[case] input: &str,
        #[case] stored: &str,
    ) -> anyhow::Result<()> {
        let url = RemoteArchiveUrl::try_new(input.to_owned())?;
        assert_eq!(url.as_ref(), stored);
        Ok(())
    }

    /// Captures `log` records for assertions. One logger serves the whole test
    /// binary, so assertions filter the shared buffer by a per-case host.
    struct CaptureLogger;

    static LOGGER: CaptureLogger = CaptureLogger;
    static LOGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    impl log::Log for CaptureLogger {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            if let Ok(mut logs) = LOGS.lock() {
                logs.push(format!("{} {}", record.level(), record.args()));
            }
        }

        fn flush(&self) {}
    }

    fn install_capture_logger() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // An Err means another logger won the race; assertions then find
            // an empty buffer and fail rather than pass vacuously.
            let _ = log::set_logger(&LOGGER);
            log::set_max_level(log::LevelFilter::Warn);
        });
    }

    fn captured(containing: &str) -> Vec<String> {
        LOGS.lock()
            .map(|logs| {
                logs.iter()
                    .filter(|line| line.contains(containing))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn defaulting_a_scheme_less_remote_archive_url_warns() -> anyhow::Result<()> {
        install_capture_logger();
        RemoteArchiveUrl::try_new("defaulted.example/a.tgz".to_owned())?;
        RemoteArchiveUrl::try_new("https://explicit.example/a.tgz".to_owned())?;

        let warned = captured("defaulted.example");
        assert_eq!(warned.len(), 1, "expected one warning, got {warned:?}");
        assert!(warned[0].starts_with("WARN "), "got {warned:?}");
        assert!(warned[0].contains("https://"), "got {warned:?}");
        assert!(
            captured("explicit.example").is_empty(),
            "a URL that states its scheme must not warn"
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case("models/m", true)] // relative
    #[case("./local/m", true)] // leading ./ stripped
    #[case("entry/blobs/Q4.gguf", true)]
    #[case("/models/m", false)] // absolute unix
    #[case("C:/models/m", false)] // absolute windows
    #[case(r"C:\models\m", false)]
    #[case(r"\\server\share\m", false)] // UNC
    #[case("~/models/m", false)]
    #[case("../sibling/m", false)]
    #[case("a/../b", false)]
    #[case("a/./b", false)]
    #[case("", false)]
    fn relative_path_validation(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(RelativePath::try_new(s.to_owned()).is_ok(), ok, "{s:?}");
    }

    #[test]
    fn relative_path_strips_leading_dot_slash() -> anyhow::Result<()> {
        assert_eq!(
            RelativePath::try_new("./models/m".to_owned())?.as_ref(),
            "models/m"
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case("/models/m", true)]
    #[case("C:/models/m", true)]
    #[case(r"C:\models\m", true)]
    #[case(r"\\server\share\m", true)]
    #[case("//server/share/m", true)]
    #[case("///share/m", false)] // UNC with no server
    #[case("models/m", false)] // relative rejected
    #[case("./models/m", false)] // becomes relative after sanitize
    #[case("~/models/m", false)]
    #[case("/models/../x", false)]
    #[case("", false)]
    fn local_path_abs_validation(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(AbsolutePath::try_new(s.to_owned()).is_ok(), ok, "{s:?}");
    }

    #[test]
    fn local_path_keeps_unix_absolute() -> anyhow::Result<()> {
        assert_eq!(
            AbsolutePath::try_new("/models/m".to_owned())?.as_ref(),
            "/models/m"
        );
        Ok(())
    }

    #[rstest::rstest]
    #[case(
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        true
    )]
    #[case(
        "0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
        false
    )] // uppercase
    #[case("abc123", false)] // too short
    #[case(
        "g123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0",
        false
    )] // non-hex
    fn sha256_validation(#[case] s: &str, #[case] ok: bool) {
        assert_eq!(Sha256::try_new(s.to_owned()).is_ok(), ok, "{s:?}");
    }

    /// A vault path is the route and nothing else: an absolute URL, another
    /// route or a local file is refused, and it round-trips as the one string
    /// a plan author writes.
    #[rstest::rstest]
    #[case::content("/vault/resources/vault_0195c3e2/content", true)]
    #[case::padded("  /vault/resources/vault_0195c3e2/content  ", true)]
    #[case::absolute("https://mgmt.example/vault/resources/vault_0195c3e2/content", false)]
    #[case::no_suffix("/vault/resources/vault_0195c3e2", false)]
    #[case::another_route("/datasets/vl_spec_decode_k8", false)]
    #[case::not_an_id("/vault/resources/nonsense/content", false)]
    #[case::empty_id("/vault/resources//content", false)]
    #[case::local("file:///tmp/model.gguf", false)]
    fn a_vault_path_is_the_route(
        #[case] raw: &str,
        #[case] ok: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let parsed = raw.parse::<VaultPath>();
        assert_eq!(parsed.is_ok(), ok, "{raw:?}");
        if let Ok(path) = parsed {
            assert_eq!(path.resource_id().as_ref(), "vault_0195c3e2");
            assert_eq!(VaultPath::of(path.resource_id().clone()), path);
            assert_eq!(path.to_string(), "/vault/resources/vault_0195c3e2/content");
            let json = serde_json::to_string(&path)?;
            assert_eq!(json, "\"/vault/resources/vault_0195c3e2/content\"");
            assert_eq!(serde_json::from_str::<VaultPath>(&json)?, path);
        }
        Ok(())
    }

    /// The route is judged before the id, so each refusal names what was
    /// actually wrong.
    #[rstest::rstest]
    #[case::route(
        "/datasets/x",
        "a vault path is `/vault/resources/{resource_id}/content`, relative to the server"
    )]
    #[case::id(
        "/vault/resources/org_1/content",
        "vault resource id must start with `vault_`"
    )]
    fn a_vault_path_refusal_names_the_route_or_the_id(#[case] raw: &str, #[case] rendered: &str) {
        assert_eq!(
            raw.parse::<VaultPath>()
                .err()
                .map(|e| e.to_string())
                .as_deref(),
            Some(rendered)
        );
    }

    /// A file name is one path segment: nothing a download could save outside
    /// its directory, or hide.
    #[rstest::rstest]
    #[case::gguf("LFM2-VL-3B-Q4_K_M.gguf", true)]
    #[case::archive("llama-b1234-bin-macos-arm64.tar.gz", true)]
    #[case::nested("LFM2-VL-3B/model.gguf", false)]
    #[case::parent("..", false)]
    #[case::hidden(".model.gguf", false)]
    #[case::space("LFM2 VL.gguf", false)]
    #[case::non_ascii("modèle.gguf", false)]
    #[case::empty("", false)]
    #[case::longest(&"a".repeat(255), true)]
    #[case::too_long(&"a".repeat(256), false)]
    fn a_file_name_is_one_path_segment(#[case] raw: &str, #[case] accepted: bool) {
        assert_eq!(FileName::try_new(raw).is_ok(), accepted, "{raw:?}");
    }

    /// The refusal quotes the name as it is on disk, escaping only what
    /// cannot be printed, and states the whole rule.
    #[rstest::rstest]
    #[case::space("my model (1).gguf", "my model (1).gguf")]
    #[case::quotes("Bob's \"model\".gguf", "Bob's \"model\".gguf")]
    #[case::backslash(r"C:\models\a.gguf", r"C:\models\a.gguf")]
    #[case::unprintable("a\nb\u{200b}.gguf", r"a\nb\u{200b}.gguf")]
    #[case::combining_mark("cafe\u{301}.gguf", "cafe\u{301}.gguf")]
    fn a_refused_file_name_is_quoted_with_the_rule(
        #[case] raw: &str,
        #[case] quoted: &str,
    ) -> anyhow::Result<()> {
        let Err(error) = FileName::try_new(raw) else {
            anyhow::bail!("{raw:?} is refused");
        };
        assert_eq!(
            error.to_string(),
            format!(
                "file name `{quoted}` must be one path segment of [A-Za-z0-9._-], starting \
                 with a letter or digit, at most 255 characters"
            )
        );
        Ok(())
    }
}
