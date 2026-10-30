//! The profile that narrows RFC 9421: its vocabulary, and the checks a
//! signature's covered components and parameters must pass.

use std::collections::BTreeSet;

use chrono::{DateTime, TimeDelta, Utc};
use sfv::{BareItem, InnerList, Item};
use strum::{AsRefStr, Display, EnumString};

use pipette_plan_types::UserKeyId;

use crate::HttpSigError::{self, Profile};

/// The signature algorithm; the profile admits Ed25519 alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, AsRefStr, Display, EnumString)]
pub(crate) enum Alg {
    #[strum(serialize = "ed25519")]
    Ed25519,
}

/// The tag that marks a signature as this profile's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, AsRefStr, Display, EnumString)]
pub(crate) enum Tag {
    #[strum(serialize = "pipette")]
    Pipette,
}

fn is_nonce(s: &str) -> bool {
    (1..=256).contains(&s.len()) && s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// A signature's nonce, unique per request: 1 to 256 visible ASCII characters.
#[nutype::nutype(
    validate(predicate = is_nonce),
    derive(Debug, Clone, PartialEq, Eq, AsRef, Display)
)]
pub struct Nonce(String);

/// A component the profile covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, AsRefStr, Display, EnumString)]
pub(crate) enum Component {
    #[strum(serialize = "@method")]
    Method,
    #[strum(serialize = "@authority")]
    Authority,
    #[strum(serialize = "@path")]
    Path,
    #[strum(serialize = "@query")]
    Query,
    #[strum(serialize = "content-digest")]
    ContentDigest,
}

impl Component {
    /// What every signature covers; [`Self::ContentDigest`] joins them when the
    /// request has a body.
    pub(crate) const REQUIRED: [Self; 4] = [Self::Method, Self::Authority, Self::Path, Self::Query];
}

/// A signature parameter the profile reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, AsRefStr, Display, EnumString)]
#[strum(serialize_all = "lowercase")]
pub(crate) enum ParamName {
    Created,
    Expires,
    Nonce,
    #[strum(serialize = "keyid")]
    KeyId,
    Alg,
    Tag,
}

/// The covered components, each once, the required four among them.
pub(crate) fn components(list: &InnerList) -> Result<Vec<Component>, HttpSigError> {
    let mut seen = BTreeSet::new();
    let covered = list
        .items
        .iter()
        .map(|item| {
            if !item.params.is_empty() {
                return Err(Profile("component parameters are outside the profile"));
            }
            let component: Component = item
                .bare_item
                .as_string()
                .ok_or(Profile("a component is a string"))?
                .as_str()
                .parse()
                .map_err(|_| Profile("a component outside the profile"))?;
            if !seen.insert(component) {
                return Err(Profile("a component covered twice"));
            }
            Ok(component)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if Component::REQUIRED.iter().any(|c| !seen.contains(c)) {
        return Err(Profile(
            "the signature must cover @method, @authority, @path and @query",
        ));
    }
    Ok(covered)
}

pub(crate) struct Params {
    pub(crate) key_id: UserKeyId,
    pub(crate) created: DateTime<Utc>,
    pub(crate) nonce: Nonce,
}

impl Params {
    /// RFC 9651 keeps a repeated key's last value; the signature over the base
    /// then decides.
    pub(crate) fn check(
        list: &InnerList,
        now: DateTime<Utc>,
        max_skew: TimeDelta,
    ) -> Result<Self, HttpSigError> {
        let (mut created, mut expires, mut nonce, mut key_id, mut alg, mut tag) =
            (None, None, None, None, None, None);
        for (key, value) in &list.params {
            let name: ParamName = key
                .as_str()
                .parse()
                .map_err(|_| Profile("a parameter outside the profile"))?;
            let integer = || {
                value
                    .as_integer()
                    .map(i64::from)
                    .ok_or(Profile("a parameter of the wrong type"))
            };
            let string = || {
                value
                    .as_string()
                    .map(|s| s.as_str())
                    .ok_or(Profile("a parameter of the wrong type"))
            };
            match name {
                ParamName::Created => created = Some(integer()?),
                ParamName::Expires => expires = Some(integer()?),
                ParamName::Nonce => nonce = Some(string()?),
                ParamName::KeyId => key_id = Some(string()?),
                ParamName::Alg => alg = Some(string()?),
                ParamName::Tag => tag = Some(string()?),
            }
        }
        if alg.and_then(|alg| alg.parse::<Alg>().ok()).is_none() {
            return Err(Profile("alg must be \"ed25519\""));
        }
        if tag.and_then(|tag| tag.parse::<Tag>().ok()).is_none() {
            return Err(Profile("tag must be \"pipette\""));
        }
        let nonce = Nonce::try_new(nonce.ok_or(Profile("nonce is required"))?)
            .map_err(|_| Profile("nonce must be 1 to 256 visible characters"))?;
        let key_id = UserKeyId::try_new(key_id.ok_or(Profile("keyid is required"))?)
            .map_err(|_| Profile("keyid is not a user key id"))?;
        let created = created
            .and_then(|s| DateTime::from_timestamp(s, 0))
            .ok_or(Profile("created is required"))?;
        if (now - created).abs() > max_skew {
            return Err(HttpSigError::Stale {
                skew_secs: (now - created).num_seconds(),
            });
        }
        if expires.is_some_and(|expires| expires <= now.timestamp()) {
            return Err(HttpSigError::Expired);
        }
        Ok(Self {
            key_id,
            created,
            nonce,
        })
    }
}

/// The inner list a signer writes: `covered`, then the parameters in the
/// profile's order.
pub(crate) fn inner_list(
    covered: &[Component],
    created: i64,
    nonce: &Nonce,
    key_id: &UserKeyId,
) -> Result<InnerList, HttpSigError> {
    let text = |s: &str| {
        sfv::String::from_string(s.to_string())
            .map(BareItem::String)
            .map_err(|_| Profile("a value that is not visible ASCII"))
    };
    let created = sfv::Integer::try_from(created).map_err(|_| Profile("created out of range"))?;
    Ok(InnerList {
        items: covered
            .iter()
            .map(|component| Ok(Item::new(text(component.as_ref())?)))
            .collect::<Result<_, HttpSigError>>()?,
        params: [
            (ParamName::Created, BareItem::Integer(created)),
            (ParamName::Nonce, text(nonce.as_ref())?),
            (ParamName::KeyId, text(key_id.as_str())?),
            (ParamName::Alg, text(Alg::Ed25519.as_ref())?),
            (ParamName::Tag, text(Tag::Pipette.as_ref())?),
        ]
        .into_iter()
        .map(|(name, value)| {
            sfv::Key::from_string(name.to_string())
                .map(|key| (key, value))
                .map_err(|_| Profile("a malformed parameter name"))
        })
        .collect::<Result<_, HttpSigError>>()?,
    })
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Utc};

    use crate::fields::signature_input;
    use crate::profile::*;

    const KEY_ID: &str = "userkey_0000000000000000000000000000000000000000000000000000000000000000";
    const SKEW: TimeDelta = TimeDelta::seconds(300);

    fn checked(input: &str) -> Result<(), HttpSigError> {
        let (_, list) = signature_input(input)?;
        components(&list)?;
        Params::check(&list, Utc::now(), SKEW).map(|_| ())
    }

    #[test]
    fn a_signature_outside_the_profile_is_refused() {
        let created = Utc::now().timestamp();
        let all = r#""@method" "@authority" "@path" "@query""#;
        let good =
            format!(r#";created={created};nonce="n";keyid="{KEY_ID}";alg="ed25519";tag="pipette""#);
        let input = |params: &str, components: &str| format!("sig=({components}){params}");
        assert!(checked(&input(&good, all)).is_ok());
        for (input, expected) in [
            (
                input(&good, r#""@method" "@authority" "@path""#),
                "must cover",
            ),
            (
                input(&good, &format!(r#"{all} "date""#)),
                "outside the profile",
            ),
            (
                input(&good, &format!(r#"{all} "@method""#)),
                "covered twice",
            ),
            (
                input(&good, r#""@method";sf "@authority" "@path" "@query""#),
                "component parameters",
            ),
            (
                input(&good.replace("ed25519", "rsa-pss-sha512"), all),
                "alg must be",
            ),
            (input(&good.replace("pipette", "other"), all), "tag must be"),
            (
                input(&good.replace(r#";nonce="n""#, ""), all),
                "nonce is required",
            ),
            (input(&format!("{good};foo=1"), all), "outside the profile"),
            (
                input(
                    &good.replace(&format!("created={created}"), "created=\"x\""),
                    all,
                ),
                "wrong type",
            ),
            (
                input(&good.replace(KEY_ID, "test-key-ed25519"), all),
                "not a user key id",
            ),
            (
                input(
                    &good.replace(&created.to_string(), &(created - 600).to_string()),
                    all,
                ),
                "timestamp expired",
            ),
            (
                input(&format!("{good};expires={}", created - 1), all),
                "signature expired",
            ),
            (
                input(
                    &good.replace(&created.to_string(), &(created + 600).to_string()),
                    all,
                ),
                "timestamp expired",
            ),
            (
                input(&good.replace(&format!(r#";keyid="{KEY_ID}""#), ""), all),
                "keyid is required",
            ),
            (
                input(&good.replace(&format!(";created={created}"), ""), all),
                "created is required",
            ),
            (
                input(&good.replace(r#"nonce="n""#, "nonce=1"), all),
                "wrong type",
            ),
            (
                input(&good.replace(r#"nonce="n""#, r#"nonce="""#), all),
                "nonce must be",
            ),
            (
                input(
                    &good.replace(r#"nonce="n""#, &format!(r#"nonce="{}""#, "n".repeat(257))),
                    all,
                ),
                "nonce must be",
            ),
        ] {
            let error = checked(&input)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(error.contains(expected), "{input}: {error}");
        }
    }

    /// An `expires` still ahead is accepted; only one that has passed is
    /// refused.
    #[test]
    fn an_expires_ahead_is_accepted() {
        let created = Utc::now().timestamp();
        let input = format!(
            r#"sig=("@method" "@authority" "@path" "@query");created={created};expires={};nonce="n";keyid="{KEY_ID}";alg="ed25519";tag="pipette""#,
            created + 60
        );
        assert!(checked(&input).is_ok());
    }

    /// The enums spell the RFC's names, and a nonce is held to its bounds.
    #[test]
    fn the_vocabulary_spells_the_rfc_names() {
        assert_eq!(
            Component::REQUIRED.map(|c| c.to_string()),
            ["@method", "@authority", "@path", "@query"]
        );
        assert_eq!(Component::ContentDigest.as_ref(), "content-digest");
        assert_eq!(ParamName::KeyId.as_ref(), "keyid");
        assert_eq!(
            "created".parse::<ParamName>().ok(),
            Some(ParamName::Created)
        );
        assert_eq!(Alg::Ed25519.as_ref(), "ed25519");
        assert_eq!(Tag::Pipette.as_ref(), "pipette");
        assert!(Nonce::try_new("").is_err());
        assert!(Nonce::try_new("a".repeat(257)).is_err());
        assert!(Nonce::try_new("tab\there").is_err());
        assert!(Nonce::try_new("4f1c0de5a9b7").is_ok());
    }
}
