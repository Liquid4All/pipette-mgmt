//! RFC 9530 `Content-Digest`, as the profile admits it: a single sha-256.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use http::HeaderMap;
use sfv::ListEntry;
use sha2::Digest;

use pipette_plan_types::Sha256;

use crate::HttpSigError::{self, Profile};
use crate::fields::{dictionary, only_member};

/// `Content-Digest` for `body`.
pub(crate) fn content_digest(body: &[u8]) -> String {
    format!("sha-256=:{}:", BASE64.encode(sha2::Sha256::digest(body)))
}

/// The SHA-256 a `Content-Digest` field names.
pub(crate) fn content_sha256(field: &str) -> Result<Sha256, HttpSigError> {
    let refuse = || Profile("Content-Digest must be a single sha-256");
    let (key, entry) = only_member(dictionary(field)?).map_err(|_| refuse())?;
    let digest = match entry {
        ListEntry::Item(item) if key == "sha-256" && item.params.is_empty() => {
            item.bare_item.as_byte_sequence().map(<[u8]>::to_vec)
        }
        ListEntry::Item(_) | ListEntry::InnerList(_) => None,
    }
    .filter(|digest| digest.len() == 32)
    .ok_or_else(refuse)?;
    Sha256::try_new(hex::encode(digest)).map_err(|_| refuse())
}

/// Whether the request's framing declares a body, which the signature must
/// then cover a digest of.
pub(crate) fn declares_body(headers: &HeaderMap) -> bool {
    headers.contains_key(http::header::TRANSFER_ENCODING)
        || headers
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .is_some_and(|len| len > 0)
}

#[cfg(test)]
mod tests {
    use sha2::Digest;

    use crate::digest::*;

    /// RFC 9530's example: `{"hello": "world"}` and its sha-256.
    #[test]
    fn the_rfc_example_reproduces() -> anyhow::Result<()> {
        let body = b"{\"hello\": \"world\"}";
        let field = content_digest(body);
        assert_eq!(
            field,
            "sha-256=:X48E9qOokqqrvdts8nOJRJN3OWDUoyWxBf7kbu9DBPE=:"
        );
        assert_eq!(
            content_sha256(&field)?.as_ref(),
            hex::encode(sha2::Sha256::digest(body))
        );
        Ok(())
    }

    /// Nothing but one 32-byte sha-256 is read.
    #[test]
    fn only_a_single_sha256_is_read() {
        for field in [
            "sha-512=:WZDPaVn/7XgHaAy8pmojAkGWoRx2UFChF41A2svX+TaPm+AbwAgBWnrIiYllu7BNNyealdVLvRwEmTHWXvJwew==:",
            "sha-256=:X48E9qOokqqrvdts8nOJRJN3OWDUoyWxBf7kbu9DBPE=:, sha-512=:AA==:",
            "sha-256=:AA==:",
        ] {
            let error = content_sha256(field)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(error.contains("sha-256"), "{field}: {error}");
        }
    }

    #[test]
    fn a_body_is_declared_by_its_length_or_its_chunking() -> anyhow::Result<()> {
        let headers = |pairs: &[(&'static str, &str)]| -> anyhow::Result<HeaderMap> {
            let mut map = HeaderMap::new();
            for (name, value) in pairs {
                map.insert(*name, value.parse()?);
            }
            Ok(map)
        };
        assert!(!declares_body(&headers(&[])?));
        assert!(!declares_body(&headers(&[("content-length", "0")])?));
        assert!(declares_body(&headers(&[("content-length", "2")])?));
        assert!(declares_body(&headers(&[(
            "transfer-encoding",
            "chunked"
        )])?));
        Ok(())
    }
}
