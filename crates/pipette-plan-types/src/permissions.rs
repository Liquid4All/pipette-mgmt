//! What a principal may do: an account's rights and a device's, and the name a
//! stored set carries for a right this build does not know.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::text::ascii_lowercase;

/// What an account may do. The `_any_` form of a right reaches every owner's
/// records, the plain one the account's own and its organization's.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::Display,
    strum::AsRefStr,
    strum::EnumIter,
    strum::VariantNames,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum UserPermission {
    /// File a plan for the caller itself.
    SubmitPlans,
    /// File a plan naming any organization's private benchmarks and vault
    /// resources.
    SubmitAnyPlans,
    /// Read the plans of the campaigns the caller reaches, and its own.
    ReadPlans,
    /// Read every plan.
    ReadAnyPlans,
    /// Cancel the plans of the caller's campaigns and its organization's, and
    /// its own.
    CancelPlans,
    /// Cancel every plan.
    CancelAnyPlans,
    /// Read the fleet the caller's plans can reach.
    ReadClients,
    /// Read the whole fleet.
    ReadAnyClients,
    /// Read the caller's organization: its accounts, what each holds, and
    /// which devices belong where.
    ReadOrgs,
    /// Read every organization and account.
    ReadAnyOrgs,
    /// Administer the caller's organization and its accounts, handing out only
    /// rights the caller holds.
    ManageOrgs,
    /// Administer any organization and account, every campaign and vault
    /// resource, and create organizations.
    ManageAnyOrgs,
    /// Administer the devices the caller's organization owns.
    ManageClients,
    /// Administer every device.
    ManageAnyClients,
    /// Create and change descriptor catalog entries in the caller's organization.
    EditCatalog,
    /// Create and change descriptor catalog entries in every organization.
    EditAnyCatalog,
}

impl UserPermission {
    /// The permissions this one grants directly: the rules of
    /// `docs/authentication.md` live here and nowhere else.
    ///
    /// `cancel_plans` is granted on its own: a cancellation is destructive.
    fn direct(self) -> &'static [UserPermission] {
        match self {
            Self::SubmitAnyPlans => &[Self::SubmitPlans],
            Self::SubmitPlans => &[Self::ReadPlans],
            Self::ReadAnyPlans => &[Self::ReadPlans],
            Self::CancelAnyPlans => &[Self::CancelPlans],
            Self::ReadAnyClients => &[Self::ReadClients],
            // Administering the roster means reading it back.
            Self::ManageOrgs => &[Self::ReadOrgs],
            Self::ReadAnyOrgs => &[Self::ReadOrgs],
            // The wider read too, so a holder sees everything it may change.
            Self::ManageAnyOrgs => &[Self::ManageOrgs, Self::ReadAnyOrgs],
            Self::ManageClients => &[Self::ReadClients],
            Self::ManageAnyClients => &[Self::ManageClients],
            Self::EditAnyCatalog => &[Self::EditCatalog],
            Self::ReadPlans
            | Self::CancelPlans
            | Self::ReadClients
            | Self::ReadOrgs
            | Self::EditCatalog => &[],
        }
    }

    /// Every permission this one grants, transitively, excluding itself. The
    /// rules descend strictly, so the walk needs no visited set of its own.
    pub fn implied(self) -> impl Iterator<Item = UserPermission> {
        let mut closure = BTreeSet::new();
        let mut frontier = vec![self];
        while let Some(permission) = frontier.pop() {
            frontier.extend(
                permission
                    .direct()
                    .iter()
                    .copied()
                    .filter(|&implied| closure.insert(implied)),
            );
        }
        closure.into_iter()
    }

    /// Whether this is an `_any_` form, reaching past the holder's organization.
    pub fn is_server_wide(self) -> bool {
        match self {
            Self::SubmitAnyPlans
            | Self::ReadAnyPlans
            | Self::CancelAnyPlans
            | Self::ReadAnyClients
            | Self::ReadAnyOrgs
            | Self::ManageAnyOrgs
            | Self::ManageAnyClients
            | Self::EditAnyCatalog => true,
            Self::SubmitPlans
            | Self::ReadPlans
            | Self::CancelPlans
            | Self::ReadClients
            | Self::ReadOrgs
            | Self::ManageOrgs
            | Self::ManageClients
            | Self::EditCatalog => false,
        }
    }

    /// Whether this administers accounts or devices, or reaches past the
    /// holder's organization: the names a service account never holds.
    pub fn is_admin(self) -> bool {
        match self {
            Self::ManageOrgs
            | Self::ManageClients
            | Self::SubmitAnyPlans
            | Self::ReadAnyPlans
            | Self::CancelAnyPlans
            | Self::ReadAnyClients
            | Self::ReadAnyOrgs
            | Self::ManageAnyOrgs
            | Self::ManageAnyClients
            | Self::EditAnyCatalog => true,
            Self::SubmitPlans
            | Self::ReadPlans
            | Self::CancelPlans
            | Self::ReadClients
            | Self::ReadOrgs
            | Self::EditCatalog => false,
        }
    }
}

/// What a device may do.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::Display,
    strum::AsRefStr,
    strum::EnumIter,
    strum::VariantNames,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ClientPermission {
    /// Submit an ad-hoc result for the caller's own device.
    SubmitResults,
    /// Claim, heartbeat, and reclaim queued jobs, and report a claimed job.
    PerformWork,
}

/// The name of a permission as it appears in storage and on the wire, such as
/// `perform_work`, including one this build does not know: a later release may
/// have added it, and the reader carries it through rather than fail.
///
/// No `-`, unlike [`Tag`](crate::Tag): a second word separator would let one
/// permission arrive under two names.
#[nutype::nutype(
    sanitize(trim, with = ascii_lowercase),
    validate(with = validate_permission_name, error = PermissionNameError),
    derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        AsRef,
        Deref,
        Display,
        Into,
        Serialize,
        Deserialize,
    ),
)]
pub struct PermissionName(String);

/// Construction error for [`PermissionName`], carrying the offending value.
#[derive(Debug, thiserror::Error)]
pub enum PermissionNameError {
    #[error("permission name must not be empty")]
    Empty,
    #[error("permission name {name:?} is too long ({len} chars, max {max})")]
    TooLong {
        name: String,
        len: usize,
        max: usize,
    },
    #[error("permission name {name:?} contains invalid character {ch:?}; allowed: [a-z0-9_]")]
    InvalidChar { name: String, ch: char },
}

/// Runs on the trimmed, lowercased value.
fn validate_permission_name(s: &str) -> Result<(), PermissionNameError> {
    if s.is_empty() {
        return Err(PermissionNameError::Empty);
    }
    if s.len() > PermissionName::MAX_LEN {
        return Err(PermissionNameError::TooLong {
            name: s.to_string(),
            len: s.len(),
            max: PermissionName::MAX_LEN,
        });
    }
    if let Some(ch) = s
        .chars()
        .find(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit() && *c != '_')
    {
        return Err(PermissionNameError::InvalidChar {
            ch,
            name: s.to_string(),
        });
    }
    Ok(())
}

impl PermissionName {
    pub const MAX_LEN: usize = 64;

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    /// A hyphen is rejected here and accepted by [`Tag`](crate::Tag).
    #[rstest]
    #[case("perform_work", true)]
    #[case("submit_results", true)]
    #[case("rescore_plans_v2", true)]
    #[case("a", true)]
    #[case("", false)]
    #[case("   ", false)]
    #[case("perform-work", false)]
    #[case("perform work", false)]
    #[case("perform/work", false)]
    #[case("perform.work", false)]
    #[case("café", false)]
    fn permission_name_validation(#[case] input: &str, #[case] valid: bool) {
        assert_eq!(PermissionName::try_new(input).is_ok(), valid, "{input:?}");
    }

    #[test]
    fn permission_name_enforces_length_bound() {
        assert!(matches!(
            PermissionName::try_new("a".repeat(PermissionName::MAX_LEN + 1)),
            Err(PermissionNameError::TooLong { .. })
        ));
    }

    #[rstest]
    #[case("  Perform_Work\n", "perform_work")]
    #[case("PERFORM_WORK", "perform_work")]
    fn permission_name_trims_and_lowercases(
        #[case] input: &str,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(PermissionName::try_new(input)?.as_str(), expected);
        let deserialized: PermissionName = serde_json::from_value(json!(input))?;
        assert_eq!(deserialized.as_str(), expected);
        assert_eq!(serde_json::to_value(&deserialized)?, json!(expected));
        Ok(())
    }
}
