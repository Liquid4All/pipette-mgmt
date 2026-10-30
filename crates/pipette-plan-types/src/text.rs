//! Free text: strings kept verbatim, trimmed, or refused when blank, and the
//! case fold the crate's names share.

use nutype::nutype;

fn is_not_blank(s: &str) -> bool {
    !s.trim().is_empty()
}

/// Rejects whitespace-only but keeps the value verbatim: pip requirements
/// `contents` hash into runtime keys, so trimming would move existing entries.
#[nutype(
    validate(predicate = is_not_blank),
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
pub struct NonEmptyString(String);

/// A `String` trimmed on every construction and deserialization, so a padded
/// value from a platform detector (e.g. PowerShell's
/// `Win32_VideoController.Name`) does not reach the server as a distinct row.
#[nutype(
    sanitize(trim),
    default = "",
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        Default,
        Serialize,
        Deserialize,
        AsRef,
        Display,
        From,
    )
)]
pub struct TrimmedString(String);

/// A `String` that is both **trimmed** *and* **non-empty**, enforced at
/// `Deserialize`, so a blank required field is a `400` rather than a blank
/// warehouse row.
#[nutype::nutype(
    sanitize(trim),
    validate(with = validate_non_empty, error = EmptyStringError),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        AsRef,
        Deref,
        Display,
        FromStr,
        Into,
        Serialize,
        Deserialize,
    ),
)]
pub struct NonEmptyTrimmedString(String);

/// Construction error for [`NonEmptyTrimmedString`]. Empty or
/// whitespace-only input is rejected.
#[derive(Debug, thiserror::Error)]
#[error("must be a non-empty string (whitespace-only is not accepted)")]
pub struct EmptyStringError;

/// Runs after the `trim` sanitizer, so whitespace-only input arrives here as
/// the empty string and is rejected by the same branch as `""`.
fn validate_non_empty(s: &str) -> Result<(), EmptyStringError> {
    if s.is_empty() {
        return Err(EmptyStringError);
    }
    Ok(())
}

impl NonEmptyTrimmedString {
    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

/// The lowercasing sanitizer behind [`Tag`](crate::Tag), [`OrgSlug`](crate::OrgSlug)
/// and [`PermissionName`](crate::PermissionName).
///
/// ASCII-only, not nutype's Unicode-aware `lowercase`, which maps `K` (U+212A
/// KELVIN SIGN) to a plain `k` and so would turn a refused homoglyph into a
/// valid name.
pub(crate) fn ascii_lowercase(s: String) -> String {
    s.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use serde_json::json;

    use super::*;

    #[test]
    fn non_empty_string_rejects_blank_strings() {
        assert!(NonEmptyString::try_new(String::new()).is_err());
        assert!(NonEmptyString::try_new(" \n\t".to_owned()).is_err());
    }

    /// Requirements `contents` hash into runtime keys, so the value is kept
    /// byte-for-byte, trailing newline included.
    #[test]
    fn non_empty_string_keeps_surrounding_whitespace() -> anyhow::Result<()> {
        let contents = NonEmptyString::try_new("vllm==0.10.0\n".to_owned())?;
        assert_eq!(contents.as_ref(), "vllm==0.10.0\n");
        Ok(())
    }

    #[test]
    fn non_empty_trimmed_string_trims_padded_input() -> anyhow::Result<()> {
        let v: NonEmptyTrimmedString = serde_json::from_value(json!("  llama-3.2-1b\t"))?;
        assert_eq!(v.as_str(), "llama-3.2-1b");
        Ok(())
    }

    #[test]
    fn non_empty_trimmed_string_rejects_empty_string() -> anyhow::Result<()> {
        let err = serde_json::from_value::<NonEmptyTrimmedString>(json!(""))
            .err()
            .context("empty string must be rejected")?;
        assert!(err.to_string().contains("non-empty"));
        Ok(())
    }

    #[test]
    fn non_empty_trimmed_string_rejects_whitespace_only() -> anyhow::Result<()> {
        let err = serde_json::from_value::<NonEmptyTrimmedString>(json!("   \t\n  "))
            .err()
            .context("whitespace-only string must be rejected")?;
        assert!(err.to_string().contains("non-empty"));
        Ok(())
    }

    #[test]
    fn trimmed_string_trims_on_construction_and_deserialize() -> anyhow::Result<()> {
        // Construction trims.
        let padded = TrimmedString::new("AMD RYZEN AI MAX+ 395 w/ Radeon 8060S          ");
        assert_eq!(padded.as_ref(), "AMD RYZEN AI MAX+ 395 w/ Radeon 8060S");

        // Deserialization trims.
        let raw = serde_json::json!("  Apple M3 Max\n");
        let parsed: TrimmedString = serde_json::from_value(raw)?;
        assert_eq!(parsed.as_ref(), "Apple M3 Max");

        // From<&str> and From<String> both trim via the sanitizer.
        let from_str: TrimmedString = "  hello  ".into();
        assert_eq!(from_str.as_ref(), "hello");
        let from_string: TrimmedString = "  world  ".to_string().into();
        assert_eq!(from_string.as_ref(), "world");
        Ok(())
    }
}
