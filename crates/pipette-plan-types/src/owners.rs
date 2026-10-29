//! Who a record belongs to, as every `owner` field spells it, and owners as a
//! listing filters by them and a form offers them.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{ListOrder, OrgId, UserId, VaultPrincipalId};

/// The owner of a record that must have one: `{"type": "org", "org_id": …}`
/// or `{"type": "user", "user_id": …}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PrincipalOwner {
    Org { org_id: OrgId },
    User { user_id: UserId },
}

/// A device's owner, in [`PrincipalOwner`]'s shape. Its own type because a
/// device may belong to nobody, which its fields say with `null`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClientOwner {
    Org { org_id: OrgId },
    User { user_id: UserId },
}

/// An owner's type, as a form offers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumString)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum OwnerType {
    Org,
    User,
}

/// One destination a form offers: the principal id it posts, the name a reader
/// types for it (a handle or an address), and how it is drawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OwnerMatch {
    pub value: VaultPrincipalId,
    pub name: String,
    pub label: String,
    /// The `type` tag of the `owner` this choice becomes once picked.
    #[serde(rename = "type")]
    pub owner_type: OwnerType,
}

/// An owner lookup's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OwnerMatches {
    pub owners: Vec<OwnerMatch>,
}

/// The query string of an owner lookup: what was typed, and the owner type
/// the field takes when it takes only one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OwnerLookupQuery {
    /// Text a choice's name, label or id contains, ignoring case; empty keeps
    /// every choice.
    #[serde(default)]
    pub q: String,
    #[serde(
        rename = "type",
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub owner_type: Option<OwnerType>,
}

/// Which slice of the caller's reach a listing asked for: "mine" and "my
/// team's" are the questions a reader has, so no owner id to look up first.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::EnumIter,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum OwnerFilter {
    Mine,
    /// Organization-owned records, narrowed further by an `org_id` a reader
    /// outside its reach may name to no effect.
    Org,
}

impl OwnerFilter {
    /// The `snake_case` spelling, for a listing that echoes its filter.
    pub fn label(self) -> &'static str {
        self.into()
    }
}

/// Query parameters every cursor-paginated listing of owned records takes:
/// `/api/v1/campaigns` and `/api/v1/vault/resources`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct OwnedListQuery {
    /// Narrow to one owner inside the caller's reach: a filter, not a
    /// permission, so an owner out of reach gives an empty listing.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub owner: Option<OwnerFilter>,
    /// Narrow to one organization's records.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub org_id: Option<OrgId>,
    /// Rows a page holds: 50 by default, at most 200 (the server's
    /// `list_default_limit` and `list_max_limit`). `0` or more than that is a
    /// `400`, not clamped.
    #[cfg_attr(feature = "schemars", schemars(range(min = 1)))]
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub limit: Option<usize>,
    /// The preceding page's `next_cursor`, unchanged; omit it for the first
    /// page.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub cursor: Option<String>,
    /// By id, which is minted in creation order: `oldest` (the default) or
    /// `newest` first. `newest` reads every row for each page.
    #[serde(
        default,
        deserialize_with = "crate::empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub order: Option<ListOrder>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Tag {
    Org,
    User,
}

/// The fields as they arrive, before the tag says which id must be present.
#[derive(Serialize, Deserialize)]
struct Wire {
    #[serde(rename = "type")]
    tag: Tag,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    org_id: Option<OrgId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_id: Option<UserId>,
}

macro_rules! owner_serde {
    ($owner:ident, $schema_name:literal) => {
        impl $owner {
            /// The owning organization, when an organization owns this.
            pub fn org_id(&self) -> Option<&OrgId> {
                match self {
                    Self::Org { org_id } => Some(org_id),
                    Self::User { .. } => None,
                }
            }

            /// The owning account, when a person owns this.
            pub fn user_id(&self) -> Option<&UserId> {
                match self {
                    Self::User { user_id } => Some(user_id),
                    Self::Org { .. } => None,
                }
            }

            /// The owner as one printable string. Each id carries its kind in
            /// its prefix, so the id alone is unambiguous.
            pub fn label(&self) -> String {
                match self {
                    Self::Org { org_id } => org_id.to_string(),
                    Self::User { user_id } => user_id.to_string(),
                }
            }
        }

        impl Serialize for $owner {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let wire = match self {
                    Self::Org { org_id } => Wire {
                        tag: Tag::Org,
                        org_id: Some(org_id.clone()),
                        user_id: None,
                    },
                    Self::User { user_id } => Wire {
                        tag: Tag::User,
                        org_id: None,
                        user_id: Some(user_id.clone()),
                    },
                };
                wire.serialize(s)
            }
        }

        impl<'de> Deserialize<'de> for $owner {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let wire = Wire::deserialize(d)?;
                match (wire.tag, wire.org_id, wire.user_id) {
                    (Tag::Org, Some(org_id), _) => Ok(Self::Org { org_id }),
                    (Tag::Org, None, _) => Err(serde::de::Error::missing_field("org_id")),
                    (Tag::User, _, Some(user_id)) => Ok(Self::User { user_id }),
                    (Tag::User, _, None) => Err(serde::de::Error::missing_field("user_id")),
                }
            }
        }

        #[cfg(feature = "schemars")]
        impl schemars::JsonSchema for $owner {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                $schema_name.into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                owner_schema()
            }
        }
    };
}

owner_serde!(PrincipalOwner, "PrincipalOwner");
owner_serde!(ClientOwner, "ClientOwner");

/// The plan dialect spells an owner as one bare id (`org_…` or `user_…`); the
/// two agree because the prefixes are disjoint.
impl From<VaultPrincipalId> for PrincipalOwner {
    fn from(principal: VaultPrincipalId) -> Self {
        match principal {
            VaultPrincipalId::Org(org_id) => Self::Org { org_id },
            VaultPrincipalId::User(user_id) => Self::User { user_id },
        }
    }
}

impl From<PrincipalOwner> for VaultPrincipalId {
    fn from(owner: PrincipalOwner) -> Self {
        match owner {
            PrincipalOwner::Org { org_id } => Self::Org(org_id),
            PrincipalOwner::User { user_id } => Self::User(user_id),
        }
    }
}

/// The schema both owners share, as they share [`Wire`].
#[cfg(feature = "schemars")]
fn owner_schema() -> schemars::Schema {
    schemars::json_schema!({
        "oneOf": [
            {
                "type": "object",
                "properties": { "type": { "const": "org" }, "org_id": { "type": "string" } },
                "required": ["type", "org_id"],
                "additionalProperties": false,
            },
            {
                "type": "object",
                "properties": { "type": { "const": "user" }, "user_id": { "type": "string" } },
                "required": ["type", "user_id"],
                "additionalProperties": false,
            },
        ]
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// The stored shape of an owner, pinned in both directions: written and
    /// read under `type`. A rename would strand every record already written.
    #[test]
    fn the_owner_shape_is_pinned() -> anyhow::Result<()> {
        let org_id = OrgId::try_new("org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?;
        let owner = ClientOwner::Org {
            org_id: org_id.clone(),
        };
        assert_eq!(
            serde_json::to_value(&owner)?,
            serde_json::json!({ "type": "org", "org_id": org_id.as_str() }),
        );
        assert_eq!(
            serde_json::from_value::<ClientOwner>(serde_json::json!({
                "type": "user",
                "user_id": "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
            }))?,
            ClientOwner::User {
                user_id: UserId::try_new("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?,
            },
        );
        Ok(())
    }

    /// A tag without the id it needs is refused by name, rather than read as
    /// an owner of nobody.
    #[rstest]
    #[case::org_without_org_id(serde_json::json!({ "type": "org" }), "org_id")]
    #[case::user_with_the_wrong_id(serde_json::json!({ "type": "user", "org_id": "org_x" }), "user_id")]
    #[case::unknown_tag(serde_json::json!({ "type": "team", "org_id": "org_x" }), "team")]
    #[case::the_retired_kind_tag(serde_json::json!({ "kind": "org", "org_id": "org_x" }), "`type`")]
    fn a_malformed_owner_names_what_is_missing(
        #[case] value: serde_json::Value,
        #[case] fragment: &str,
    ) {
        let refused = serde_json::from_value::<PrincipalOwner>(value).err();
        let message = refused.map(|e| e.to_string()).unwrap_or_default();
        assert!(message.contains(fragment), "{message}");
    }

    #[test]
    fn an_owner_answers_for_one_kind() -> anyhow::Result<()> {
        let org_owned = ClientOwner::Org {
            org_id: OrgId::try_new("org_x")?,
        };
        let user_owned = ClientOwner::User {
            user_id: UserId::try_new("user_x")?,
        };
        assert!(org_owned.org_id().is_some() && org_owned.user_id().is_none());
        assert!(user_owned.user_id().is_some() && user_owned.org_id().is_none());
        Ok(())
    }
}
