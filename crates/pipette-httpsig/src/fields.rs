//! The RFC 9651 structured fields a signature is written in, through `sfv`:
//! `Signature-Input` and `Signature`, each holding one signature.

use http::{HeaderMap, HeaderName};
use sfv::{Dictionary, InnerList, ListEntry, ListSerializer};

use crate::HttpSigError::{self, Profile};

/// The header's lines joined as RFC 9651 combines them.
pub(crate) fn combined(headers: &HeaderMap, name: &HeaderName) -> Result<String, HttpSigError> {
    let values = headers
        .get_all(name)
        .iter()
        .map(|v| v.to_str().map(str::trim))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| Profile("a header that is not visible ASCII"))?;
    if values.is_empty() {
        return Err(HttpSigError::MissingHeader(name.clone()));
    }
    Ok(values.join(", "))
}

/// A signature header's one line: a repeated key in lines combined by RFC 9651
/// keeps its last value, which a reader taking the first would not see.
pub(crate) fn single(headers: &HeaderMap, name: &HeaderName) -> Result<String, HttpSigError> {
    if headers.get_all(name).iter().count() > 1 {
        return Err(Profile("a request carries one signature"));
    }
    combined(headers, name)
}

/// A structured field as a dictionary.
pub(crate) fn dictionary(field: &str) -> Result<Dictionary, HttpSigError> {
    sfv::Parser::new(field)
        .parse::<Dictionary>()
        .map_err(|_| Profile("a malformed structured field"))
}

/// The dictionary's one member: the profile admits a single signature.
pub(crate) fn only_member(dict: Dictionary) -> Result<(String, ListEntry), HttpSigError> {
    if dict.len() != 1 {
        return Err(Profile("a request carries one signature"));
    }
    dict.into_iter()
        .next()
        .map(|(key, entry)| (key.as_str().to_string(), entry))
        .ok_or(Profile("a request carries one signature"))
}

/// The inner list as RFC 9651 serializes it: the `@signature-params` value.
pub(crate) fn serialize_inner_list(list: &InnerList) -> Result<String, HttpSigError> {
    let mut serializer = ListSerializer::new();
    serializer.members([&ListEntry::InnerList(list.clone())]);
    serializer.finish().ok_or(Profile("an empty inner list"))
}

/// `label=("c1" "c2" ...);k=v...`: the label and the inner list.
pub(crate) fn signature_input(field: &str) -> Result<(String, InnerList), HttpSigError> {
    match only_member(dictionary(field)?)? {
        (label, ListEntry::InnerList(list)) => Ok((label, list)),
        (_, ListEntry::Item(_)) => Err(Profile("Signature-Input must be an inner list")),
    }
}

/// `label=:base64:`: the label and the signature bytes.
pub(crate) fn signature_field(field: &str) -> Result<(String, Vec<u8>), HttpSigError> {
    match only_member(dictionary(field)?)? {
        (label, ListEntry::Item(item)) if item.params.is_empty() => {
            let bytes = item
                .bare_item
                .as_byte_sequence()
                .ok_or(Profile("Signature must be a byte sequence"))?;
            Ok((label, bytes.to_vec()))
        }
        (_, ListEntry::Item(_)) => Err(Profile("signature parameters belong on Signature-Input")),
        (_, ListEntry::InnerList(_)) => Err(Profile("Signature must be a byte sequence")),
    }
}

#[cfg(test)]
mod tests {
    use crate::fields::*;

    /// RFC 9421 Appendix B.2.6's `Signature-Input` parses, and `sfv` writes its
    /// inner list back byte for byte.
    #[test]
    fn the_rfc_signature_input_round_trips() -> anyhow::Result<()> {
        let params = r#"("date" "@method" "@path" "@authority" "content-type" "content-length");created=1618884473;keyid="test-key-ed25519""#;
        let (label, list) = signature_input(&format!("sig-b26={params}"))?;
        assert_eq!(label, "sig-b26");
        assert_eq!(serialize_inner_list(&list)?, params);
        Ok(())
    }

    /// A header value outside visible ASCII is refused rather than read lossily.
    #[test]
    fn a_header_outside_visible_ascii_is_refused() -> anyhow::Result<()> {
        let mut headers = HeaderMap::new();
        headers.insert(
            crate::SIGNATURE_INPUT,
            http::HeaderValue::from_bytes(b"sig=(\"caf\xe9\")")?,
        );
        let error = combined(&headers, &crate::SIGNATURE_INPUT)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains("not visible ASCII"), "{error}");
        Ok(())
    }

    /// One signature per request, in the field shapes the profile reads.
    #[test]
    fn a_field_outside_its_shape_is_refused() {
        for (result, expected) in [
            (
                signature_input(r#"a=("@method"), b=("@path")"#).map(|_| ()),
                "one signature",
            ),
            (
                signature_input("a=:AAAA:").map(|_| ()),
                "must be an inner list",
            ),
            (
                signature_input("not a field").map(|_| ()),
                "malformed structured field",
            ),
            (
                signature_field("a=:AAAA:;keyid=\"x\"").map(|_| ()),
                "belong on Signature-Input",
            ),
            (
                signature_field(r#"a=("@method")"#).map(|_| ()),
                "byte sequence",
            ),
            (
                signature_field("a=:AAAA:, b=:AAAA:").map(|_| ()),
                "one signature",
            ),
        ] {
            let error = result.err().map(|e| e.to_string()).unwrap_or_default();
            assert!(error.contains(expected), "{error}");
        }
    }
}
