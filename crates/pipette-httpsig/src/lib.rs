//! RFC 9421 HTTP Message Signatures in pipette's one profile: Ed25519 over the
//! method, authority, path, query and any body's `Content-Digest`. Anything else
//! is refused, so the signer and the verifier cannot read a signature differently.

mod base;
mod digest;
mod fields;
mod profile;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, TimeDelta, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey};
use http::request::Parts;
use http::{HeaderMap, HeaderName};

use pipette_plan_types::{Sha256, UserKeyId};

use crate::HttpSigError::Profile;
use crate::base::Target;
use crate::profile::{Component, Params};

pub use crate::profile::Nonce;

pub const SIGNATURE_INPUT: HeaderName = HeaderName::from_static("signature-input");
pub const SIGNATURE: HeaderName = HeaderName::from_static("signature");
pub const CONTENT_DIGEST: HeaderName = HeaderName::from_static("content-digest");

/// Why a request's signature fields are refused.
#[derive(Debug, thiserror::Error)]
pub enum HttpSigError {
    #[error("missing {0} header")]
    MissingHeader(HeaderName),
    #[error("timestamp expired")]
    Stale { skew_secs: i64 },
    #[error("signature expired")]
    Expired,
    #[error("signature: {0}")]
    Profile(&'static str),
}

/// A signature that fits the profile and is fresh, not yet verified.
pub struct Verifiable {
    pub key_id: UserKeyId,
    pub created: DateTime<Utc>,
    pub nonce: Nonce,
    pub signature: Signature,
    /// The body digest the signature covers, `None` when it covers none.
    pub content_sha256: Option<Sha256>,
    /// The covered `@authority`, lowercased.
    pub authority: String,
    /// What the signature must verify over.
    pub base: String,
}

/// Whether the request carries an RFC 9421 signature, so it is judged as one.
pub fn present(headers: &HeaderMap) -> bool {
    headers.contains_key(SIGNATURE_INPUT) || headers.contains_key(SIGNATURE)
}

/// The authority a request arrived for, lowercased: its target's, else its
/// `Host`. What `@authority` covers, and what a device's `v3` payload signs.
pub fn authority(parts: &Parts) -> Option<String> {
    parts
        .uri
        .authority()
        .map(|a| a.as_str())
        .or_else(|| parts.headers.get(http::header::HOST)?.to_str().ok())
        .map(str::to_ascii_lowercase)
}

/// Parse and check the request's signature against the profile, with
/// `created` no further than `max_skew` from `now`.
pub fn parse(
    parts: &Parts,
    now: DateTime<Utc>,
    max_skew: TimeDelta,
) -> Result<Verifiable, HttpSigError> {
    let (label, list) =
        fields::signature_input(&fields::single(&parts.headers, &SIGNATURE_INPUT)?)?;
    let (signature_label, signature) =
        fields::signature_field(&fields::single(&parts.headers, &SIGNATURE)?)?;
    if signature_label != label {
        return Err(Profile(
            "Signature and Signature-Input name different labels",
        ));
    }
    let signature: [u8; 64] = signature
        .try_into()
        .map_err(|_| Profile("an Ed25519 signature is 64 bytes"))?;

    let covered = profile::components(&list)?;
    let params = Params::check(&list, now, max_skew)?;
    let covers_digest = covered.contains(&Component::ContentDigest);
    if digest::declares_body(&parts.headers) && !covers_digest {
        return Err(Profile("a request with a body must cover content-digest"));
    }
    let content_sha256 = covers_digest
        .then(|| digest::content_sha256(&fields::combined(&parts.headers, &CONTENT_DIGEST)?))
        .transpose()?;

    let target = Target::of_request(parts)?;
    Ok(Verifiable {
        key_id: params.key_id,
        created: params.created,
        nonce: params.nonce,
        signature: Signature::from_bytes(&signature),
        content_sha256,
        base: target.base(&covered, &fields::serialize_inner_list(&list)?)?,
        authority: target.authority,
    })
}

/// The request a signer is about to send.
pub struct Outgoing<'a> {
    pub method: &'a str,
    /// The `Host` it goes to.
    pub authority: &'a str,
    pub path_and_query: &'a str,
    pub body: Option<&'a [u8]>,
}

/// The headers an RFC 9421 signature adds, in the profile's shape.
pub struct Signed {
    pub signature_input: String,
    pub signature: String,
    pub content_digest: Option<String>,
}

/// Sign `request` as `key_id`, at `created` (Unix seconds).
pub fn sign(
    key: &SigningKey,
    key_id: &UserKeyId,
    nonce: &Nonce,
    created: i64,
    request: &Outgoing<'_>,
) -> Result<Signed, HttpSigError> {
    let content_digest = request.body.map(digest::content_digest);
    let mut covered = Component::REQUIRED.to_vec();
    if content_digest.is_some() {
        covered.push(Component::ContentDigest);
    }
    let params =
        fields::serialize_inner_list(&profile::inner_list(&covered, created, nonce, key_id)?)?;
    let target = Target::outgoing(
        request.method,
        request.authority,
        request.path_and_query,
        content_digest.clone(),
    );
    let signature = key.sign(target.base(&covered, &params)?.as_bytes());
    Ok(Signed {
        signature_input: format!("sig={params}"),
        signature: format!("sig=:{}:", BASE64.encode(signature.to_bytes())),
        content_digest,
    })
}

/// RFC 9421 Appendix B.1.4's `test-key-ed25519`, which the modules' tests sign
/// and verify with.
#[cfg(test)]
pub(crate) mod testing {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ed25519_dalek::SigningKey;

    /// Its JWK `d`.
    const RFC_KEY_D: &str = "n4Ni-HpISpVObnQMW0wOhCKROaIKqKtW_2ZYb2p9KcU";

    pub(crate) fn rfc_key() -> anyhow::Result<SigningKey> {
        let d: [u8; 32] = URL_SAFE_NO_PAD
            .decode(RFC_KEY_D)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("32 bytes"))?;
        Ok(SigningKey::from_bytes(&d))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta, Utc};
    use http::Request;
    use http::request::Parts;
    use sha2::Digest;

    use crate::testing::rfc_key;
    use crate::*;

    const SKEW: TimeDelta = TimeDelta::seconds(300);

    fn key_id() -> anyhow::Result<UserKeyId> {
        Ok(UserKeyId::try_new(
            "userkey_0000000000000000000000000000000000000000000000000000000000000000",
        )?)
    }

    /// `uri` as a request signed by the RFC key, carrying what a client sends.
    fn signed_parts(
        method: &str,
        uri: &str,
        body: Option<&[u8]>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Parts> {
        let request = Outgoing {
            method,
            authority: "Mgmt.Example",
            path_and_query: uri,
            body,
        };
        let signed = sign(
            &rfc_key()?,
            &key_id()?,
            &Nonce::try_new("n1")?,
            now.timestamp(),
            &request,
        )?;
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "mgmt.example")
            .header("signature-input", signed.signature_input)
            .header("signature", signed.signature);
        if let (Some(digest), Some(body)) = (signed.content_digest, body) {
            builder = builder
                .header("content-digest", digest)
                .header("content-length", body.len());
        }
        Ok(builder.body(())?.into_parts().0)
    }

    /// The profile's bytes for fixed inputs; `ui/test/httpsig.test.ts` pins the
    /// same strings, so the app's signer and this one cannot drift.
    #[test]
    fn the_shared_vector_reproduces() -> anyhow::Result<()> {
        let signed = sign(
            &rfc_key()?,
            &UserKeyId::try_new(
                "userkey_b16c2d1bead1262639764fdb0ee4d3774599336bd493404cda4b1136c59f2062",
            )?,
            &Nonce::try_new("4f1c0de5a9b7")?,
            1_760_000_000,
            &Outgoing {
                method: "POST",
                authority: "mgmt.example",
                path_and_query: "/api/v1/orgs?dry_run=true",
                body: Some(b"{\"slug\":\"acme\"}"),
            },
        )?;
        assert_eq!(
            signed.signature_input,
            r#"sig=("@method" "@authority" "@path" "@query" "content-digest");created=1760000000;nonce="4f1c0de5a9b7";keyid="userkey_b16c2d1bead1262639764fdb0ee4d3774599336bd493404cda4b1136c59f2062";alg="ed25519";tag="pipette""#
        );
        assert_eq!(
            signed.signature,
            "sig=:xErs0vVcglQO4jeofP53oNks9pyROP4E0OOttcbMlHt5CAanP85tQV21Sy4jOZEzJg3Bm9gRrGXvqDssOFy+Bg==:"
        );
        assert_eq!(
            signed.content_digest.as_deref(),
            Some("sha-256=:Dmcct3wNRuKmSGlN9J7lDIw9DvE0Xp6DUXXIZHWJ65k=:")
        );
        Ok(())
    }

    /// A request signed by Python's `http-message-signatures` 2.0.1, which writes
    /// the parameters in its own order, verifies here.
    #[test]
    fn an_independent_signer_verifies() -> anyhow::Result<()> {
        let parts = Request::builder()
            .method("POST")
            .uri("/api/v1/orgs?dry_run=true")
            .header("host", "mgmt.example")
            .header("content-type", "application/json")
            .header("content-length", "15")
            .header(
                "content-digest",
                "sha-256=:Dmcct3wNRuKmSGlN9J7lDIw9DvE0Xp6DUXXIZHWJ65k=:",
            )
            .header(
                "signature-input",
                r#"sig=("@method" "@authority" "@path" "@query" "content-digest");created=1760000000;keyid="userkey_b16c2d1bead1262639764fdb0ee4d3774599336bd493404cda4b1136c59f2062";alg="ed25519";nonce="b7e1d9a04c2f";tag="pipette""#,
            )
            .header(
                "signature",
                "sig=:wK1mHwXMrEMNV1zwGwNeJJkLSsTOZTRTvWS1XdIIU+HUHHXqt/x3yZrTiKnuwvFdG2aeM2lAu6D40DipQOjtBg==:",
            )
            .body(())?
            .into_parts()
            .0;
        let now = DateTime::from_timestamp(1_760_000_010, 0)
            .ok_or_else(|| anyhow::anyhow!("a valid instant"))?;
        let parsed = parse(&parts, now, SKEW)?;
        assert_eq!(parsed.nonce.as_ref(), "b7e1d9a04c2f");
        rfc_key()?
            .verifying_key()
            .verify_strict(parsed.base.as_bytes(), &parsed.signature)?;
        Ok(())
    }

    #[test]
    fn a_signed_request_parses_and_verifies() -> anyhow::Result<()> {
        let now = Utc::now();
        for (method, uri, body) in [
            ("GET", "/api/v1/users/me", None),
            ("GET", "/api/v1/plans?limit=5&status=running", None),
            (
                "POST",
                "/api/v1/campaigns?name=Nightly",
                Some(b"{\"a\":1}".as_slice()),
            ),
        ] {
            let parsed = parse(&signed_parts(method, uri, body, now)?, now, SKEW)?;
            assert_eq!(parsed.key_id, key_id()?);
            assert_eq!(
                parsed.content_sha256.map(|d| d.as_ref().to_string()),
                body.map(|b| hex::encode(sha2::Sha256::digest(b)))
            );
            rfc_key()?
                .verifying_key()
                .verify_strict(parsed.base.as_bytes(), &parsed.signature)?;
        }
        Ok(())
    }

    /// A body must be covered by a `Content-Digest` the request carries, and
    /// the two fields must name one signature.
    #[test]
    fn a_request_is_held_to_what_its_signature_covers() -> anyhow::Result<()> {
        let now = Utc::now();
        let error = |parts: &Parts| {
            parse(parts, now, SKEW)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        };

        let mut parts = signed_parts("POST", "/api/v1/plans", Some(b"{}"), now)?;
        parts.headers.remove(CONTENT_DIGEST);
        assert_eq!(error(&parts), "missing content-digest header");

        let mut parts = signed_parts("GET", "/api/v1/plans", None, now)?;
        parts.headers.insert("content-length", "2".parse()?);
        assert!(error(&parts).contains("must cover content-digest"));

        let mut parts = signed_parts("GET", "/api/v1/plans", None, now)?;
        let relabeled = parts
            .headers
            .get(SIGNATURE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.replacen("sig=", "other=", 1))
            .unwrap_or_default();
        parts.headers.insert(SIGNATURE, relabeled.parse()?);
        assert!(error(&parts).contains("different labels"));

        let mut parts = signed_parts("GET", "/api/v1/plans", None, now)?;
        parts.headers.remove(SIGNATURE);
        assert_eq!(error(&parts), "missing signature header");

        let mut parts = signed_parts("GET", "/api/v1/plans", None, now)?;
        parts.headers.insert(SIGNATURE, "sig=:AAAA:".parse()?);
        assert!(error(&parts).contains("64 bytes"));

        // A second line, even a copy, is refused rather than collapsed.
        let mut parts = signed_parts("GET", "/api/v1/plans", None, now)?;
        let input = parts.headers.get(SIGNATURE_INPUT).cloned();
        if let Some(input) = input {
            parts.headers.append(SIGNATURE_INPUT, input);
        }
        assert!(error(&parts).contains("one signature"));
        Ok(())
    }

    /// Either signature field marks a request as signed this way, so a request
    /// carrying one alone is refused rather than read as `v2`.
    #[test]
    fn either_field_marks_a_request_as_signed_this_way() -> anyhow::Result<()> {
        let mut headers = HeaderMap::new();
        assert!(!present(&headers));
        headers.insert(SIGNATURE, "sig=:AAAA:".parse()?);
        assert!(present(&headers));
        let mut headers = HeaderMap::new();
        headers.insert(SIGNATURE_INPUT, "sig=()".parse()?);
        assert!(present(&headers));
        Ok(())
    }
}
