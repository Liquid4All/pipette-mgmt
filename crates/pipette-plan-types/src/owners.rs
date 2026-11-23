//! Who a record belongs to, as every `owner` field spells it, and owners as a
//! listing filters by them and a form offers them.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{ListOrder, OrgId, VaultPrincipalId};

/// The owner of a record that must have one: `{"type": "org", "org_id": …}`.
/// An organization owns everything; an account works through its membership.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PrincipalOwner {
    Org { org_id: OrgId },
}

/// A device's owner, in [`PrincipalOwner`]'s shape. Its own type because a
/// device may belong to nobody, which its fields say with `null`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClientOwner {
    Org { org_id: OrgId },
}

/// An owner's type, as a form offers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::EnumString)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum OwnerType {
    Org,
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

/// Which slice of the caller's reach a listing asked for, with no owner id to
/// look up first.
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
/// `/api/v1/campaigns`, `/api/v1/campaign-groups` and `/api/v1/vault/resources`.
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
}

/// The fields as they arrive.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    #[serde(rename = "type")]
    tag: Tag,
    org_id: OrgId,
}

macro_rules! owner_serde {
    ($owner:ident, $schema_name:literal) => {
        impl $owner {
            /// The owning organization.
            pub fn org_id(&self) -> &OrgId {
                match self {
                    Self::Org { org_id } => org_id,
                }
            }

            /// The owner as one printable string: the organization's id.
            pub fn label(&self) -> String {
                self.org_id().to_string()
            }
        }

        impl Serialize for $owner {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                Wire {
                    tag: Tag::Org,
                    org_id: self.org_id().clone(),
                }
                .serialize(s)
            }
        }

        impl<'de> Deserialize<'de> for $owner {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let Wire {
                    tag: Tag::Org,
                    org_id,
                } = Wire::deserialize(d)?;
                Ok(Self::Org { org_id })
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

/// The plan dialect spells an owner as one bare `org_…` id.
impl From<VaultPrincipalId> for PrincipalOwner {
    fn from(principal: VaultPrincipalId) -> Self {
        match principal {
            VaultPrincipalId::Org(org_id) => Self::Org { org_id },
        }
    }
}

impl From<PrincipalOwner> for VaultPrincipalId {
    fn from(owner: PrincipalOwner) -> Self {
        match owner {
            PrincipalOwner::Org { org_id } => Self::Org(org_id),
        }
    }
}

/// The schema both owners share, as they share [`Wire`].
#[cfg(feature = "schemars")]
fn owner_schema() -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": { "type": { "const": "org" }, "org_id": { "type": "string" } },
        "required": ["type", "org_id"],
        "additionalProperties": false,
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
            serde_json::from_value::<ClientOwner>(
                serde_json::json!({ "type": "org", "org_id": org_id.as_str() })
            )?,
            owner,
        );
        Ok(())
    }

    /// A malformed owner is refused by name, and an account is no owner.
    #[rstest]
    #[case::org_without_org_id(serde_json::json!({ "type": "org" }), "org_id")]
    #[case::a_person(
        serde_json::json!({ "type": "user", "user_id": "user_x" }),
        "unknown variant `user`"
    )]
    #[case::unknown_tag(serde_json::json!({ "type": "team", "org_id": "org_x" }), "team")]
    #[case::the_retired_kind_tag(serde_json::json!({ "kind": "org", "org_id": "org_x" }), "kind")]
    fn a_malformed_owner_names_what_is_missing(
        #[case] value: serde_json::Value,
        #[case] fragment: &str,
    ) {
        let refused = serde_json::from_value::<PrincipalOwner>(value).err();
        let message = refused.map(|e| e.to_string()).unwrap_or_default();
        assert!(message.contains(fragment), "{message}");
    }
}
