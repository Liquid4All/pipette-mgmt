//! RFC 9421 §2: the values a request's covered components take, and the
//! signature base over them. The verifier and the signer both build a
//! [`Target`], so the two derive each value one way.

use http::request::Parts;

use crate::CONTENT_DIGEST;
use crate::HttpSigError::{self, Profile};
use crate::fields::combined;
use crate::profile::Component;

/// A request's component values.
pub(crate) struct Target {
    method: String,
    pub(crate) authority: String,
    path: String,
    query: String,
    content_digest: Option<String>,
}

impl Target {
    /// What an arriving request carries.
    pub(crate) fn of_request(parts: &Parts) -> Result<Self, HttpSigError> {
        let authority =
            crate::authority(parts).ok_or(Profile("the request names no authority (Host)"))?;
        let content_digest = match combined(&parts.headers, &CONTENT_DIGEST) {
            Ok(field) => Some(field),
            Err(HttpSigError::MissingHeader(_)) => None,
            Err(e) => return Err(e),
        };
        Ok(Self::new(
            parts.method.as_str(),
            &authority,
            parts.uri.path(),
            parts.uri.query().unwrap_or(""),
            content_digest,
        ))
    }

    /// What a signer is about to send.
    pub(crate) fn outgoing(
        method: &str,
        authority: &str,
        path_and_query: &str,
        content_digest: Option<String>,
    ) -> Self {
        let (path, query) = path_and_query
            .split_once('?')
            .unwrap_or((path_and_query, ""));
        Self::new(method, authority, path, query, content_digest)
    }

    /// The values as RFC 9421 §2.2 derives them: the authority lowercased,
    /// an empty path as `/`, and the query behind its `?`, or `?` alone.
    fn new(
        method: &str,
        authority: &str,
        path: &str,
        query: &str,
        content_digest: Option<String>,
    ) -> Self {
        Self {
            method: method.to_string(),
            authority: authority.to_ascii_lowercase(),
            path: if path.is_empty() { "/" } else { path }.to_string(),
            query: format!("?{query}"),
            content_digest,
        }
    }

    fn value(&self, component: Component) -> Result<&str, HttpSigError> {
        Ok(match component {
            Component::Method => &self.method,
            Component::Authority => &self.authority,
            Component::Path => &self.path,
            Component::Query => &self.query,
            Component::ContentDigest => self
                .content_digest
                .as_deref()
                .ok_or(HttpSigError::MissingHeader(CONTENT_DIGEST))?,
        })
    }

    /// The signature base over `covered`, ending with `params`.
    pub(crate) fn base(&self, covered: &[Component], params: &str) -> Result<String, HttpSigError> {
        let lines = covered
            .iter()
            .map(|component| Ok((component.as_ref(), self.value(*component)?)))
            .collect::<Result<Vec<_>, HttpSigError>>()?;
        Ok(signature_base(&lines, params))
    }
}

/// The RFC 9421 §2.5 signature base over `components` (name, value), ending
/// with `@signature-params`.
pub(crate) fn signature_base(components: &[(&str, &str)], params: &str) -> String {
    let mut base = String::new();
    for (name, value) in components {
        base.push_str(&format!("\"{name}\": {value}\n"));
    }
    base.push_str(&format!("\"@signature-params\": {params}"));
    base
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signature, Signer};
    use http::Request;

    use crate::base::*;
    use crate::fields::{serialize_inner_list, signature_field, signature_input};
    use crate::testing::rfc_key;

    /// RFC 9421 Appendix B.2.6 byte for byte: the base this module builds, and
    /// the RFC's own signature verifying over it.
    #[test]
    fn the_rfc_ed25519_example_reproduces() -> anyhow::Result<()> {
        let (_, list) = signature_input(
            r#"sig-b26=("date" "@method" "@path" "@authority" "content-type" "content-length");created=1618884473;keyid="test-key-ed25519""#,
        )?;
        let base = signature_base(
            &[
                ("date", "Tue, 20 Apr 2021 02:07:55 GMT"),
                ("@method", "POST"),
                ("@path", "/foo"),
                ("@authority", "example.com"),
                ("content-type", "application/json"),
                ("content-length", "18"),
            ],
            &serialize_inner_list(&list)?,
        );
        assert_eq!(
            base,
            "\"date\": Tue, 20 Apr 2021 02:07:55 GMT\n\
             \"@method\": POST\n\
             \"@path\": /foo\n\
             \"@authority\": example.com\n\
             \"content-type\": application/json\n\
             \"content-length\": 18\n\
             \"@signature-params\": (\"date\" \"@method\" \"@path\" \"@authority\" \"content-type\" \"content-length\");created=1618884473;keyid=\"test-key-ed25519\""
        );

        let (_, bytes) = signature_field(
            "sig-b26=:wqcAqbmYJ2ji2glfAMaRy4gruYYnx2nEFN2HN6jrnDnQCK1u02Gb04v9EDgwUPiu4A0w6vuQv5lIp5WPpBKRCw==:",
        )?;
        let bytes: [u8; 64] = bytes.try_into().map_err(|_| anyhow::anyhow!("64 bytes"))?;
        let key = rfc_key()?;
        assert_eq!(
            key.sign(base.as_bytes()).to_bytes(),
            bytes,
            "Ed25519 is deterministic"
        );
        key.verifying_key()
            .verify_strict(base.as_bytes(), &Signature::from_bytes(&bytes))?;
        Ok(())
    }

    /// An arriving request and a signer's values derive the same base.
    #[test]
    fn a_request_and_its_signer_derive_one_base() -> anyhow::Result<()> {
        let covered = [
            Component::Method,
            Component::Authority,
            Component::Path,
            Component::Query,
        ];
        for (uri, query) in [
            ("/api/v1/plans?limit=5", "?limit=5"),
            ("/api/v1/users/me", "?"),
        ] {
            let parts = Request::builder()
                .method("GET")
                .uri(uri)
                .header("host", "Mgmt.Example")
                .body(())?
                .into_parts()
                .0;
            let arriving = Target::of_request(&parts)?.base(&covered, "()")?;
            let outgoing =
                Target::outgoing("GET", "mgmt.example", uri, None).base(&covered, "()")?;
            assert_eq!(arriving, outgoing);
            assert!(
                arriving.contains(&format!("\"@query\": {query}\n")),
                "{arriving}"
            );
            assert!(
                arriving.contains("\"@authority\": mgmt.example\n"),
                "{arriving}"
            );
        }
        Ok(())
    }

    /// The authority comes from the request target when it names one, as an
    /// HTTP/2 request's does, and from `Host` otherwise.
    #[test]
    fn the_authority_comes_from_the_target_before_host() -> anyhow::Result<()> {
        let covered = [Component::Authority];
        let parts = Request::builder()
            .uri("https://Mgmt.Example:8443/api/v1/users/me")
            .header("host", "ignored.example")
            .body(())?
            .into_parts()
            .0;
        assert!(
            Target::of_request(&parts)?
                .base(&covered, "()")?
                .contains("\"@authority\": mgmt.example:8443\n")
        );
        Ok(())
    }

    /// A covered digest the request does not carry, or no authority at all,
    /// is refused.
    #[test]
    fn a_value_the_request_lacks_is_refused() -> anyhow::Result<()> {
        let parts = Request::builder().uri("/x").body(())?.into_parts().0;
        let error = Target::of_request(&parts)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains("no authority"), "{error}");

        let parts = Request::builder()
            .uri("/x")
            .header("host", "h")
            .header(
                "content-digest",
                http::HeaderValue::from_bytes(b"sha-256=:\xe9:")?,
            )
            .body(())?
            .into_parts()
            .0;
        let error = Target::of_request(&parts)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains("not visible ASCII"), "{error}");

        let target = Target::outgoing("POST", "h", "/x", None);
        let error = target
            .base(&[Component::ContentDigest], "()")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(error, "missing content-digest header");
        Ok(())
    }
}
