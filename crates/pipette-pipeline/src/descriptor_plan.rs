//! Pure resolution of a descriptor catalog entry for one plan variant.
//!
//! The caller looks up the name in the plan's scope and supplies the effective
//! flags of each listed client. This module does not change job eligibility.

use std::collections::{BTreeSet, HashSet};

use pipette_core::config::DescriptorCatalogConfig;
use pipette_plan_types::{
    DescriptorEntry, DescriptorEntryState, DescriptorMember, MemberSelector, QualifiedCatalogName,
    VaultResourceId,
};

use crate::{
    descriptor_offer::{OfferError, coverage, offer_flags},
    vault_check::named_resources,
};

/// The two positive paths by which a variant admits a device. A non-empty
/// `requires` enables the capability path; listed clients use their own flags.
#[derive(Debug, Clone, Copy)]
pub struct Eligibility<'a> {
    pub requires: &'a BTreeSet<String>,
    pub any_of: &'a [BTreeSet<String>],
    pub clients: &'a [BTreeSet<String>],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    HiddenName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<'a> {
    pub member: &'a DescriptorMember,
    pub warnings: Vec<Warning>,
}

/// A refusal includes the entry's member keys where they can help the author
/// choose an explicit member.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("unknown catalog name `{0}`")]
    UnknownName(String),
    #[error("catalog name `{name}` is tombstoned. Available members: {available}.")]
    Tombstoned { name: String, available: String },
    #[error(
        "invalid catalog member selector `{selector}`: {reason}. Available members: {available}."
    )]
    InvalidSelector {
        selector: String,
        reason: String,
        available: String,
    },
    #[error(
        "`{name}` selects `{member}`, but vault resource `{resource_id}` is missing. Available members: {available}."
    )]
    MissingVaultResource {
        name: String,
        member: String,
        resource_id: VaultResourceId,
        available: String,
    },
    #[error("`{name}` has no member `{member}`; available members: {available}")]
    UnknownMember {
        name: String,
        member: String,
        available: String,
    },
    #[error(
        "`{name}` offers no member to a device that this variant admits. Available members: {available}. Select a member with `::`. Alternatively, add an offer rule."
    )]
    NoCandidate { name: String, available: String },
    #[error(
        "`{name}` selects `{candidate}`. The offer rule excludes some devices that this variant admits. Available members: {available}. Add a `requires`. Alternatively, select a member with `::`."
    )]
    NotOfferedToAll {
        name: String,
        candidate: String,
        available: String,
    },
    #[error(
        "`{name}` selects `{candidate}`, but a listed client does not report the build flag `{flag}`. Update that client to a build with this flag. Alternatively, select a member with `::`."
    )]
    ClientMissingBuild {
        name: String,
        candidate: String,
        flag: String,
    },
    #[error(
        "`{name}` selects `{candidate}`. Only a device that reports the build flag `{flag}` has this build. Add the flag to `requires`. Alternatively, select a member with `::`."
    )]
    BuildNotRequired {
        name: String,
        candidate: String,
        flag: String,
    },
}

/// Resolve one catalog name or explicit member selector against a variant.
/// `entry` is the result of scoped lookup. Pass a tombstoned entry when the
/// caller needs a distinct tombstone error. An explicit selector is the
/// author's choice and skips the offer check, like a written descriptor.
/// `available_resources` contains the vault resources the caller found.
pub fn resolve<'a>(
    selector: &str,
    entry: Option<&'a DescriptorEntry>,
    eligibility: Eligibility<'_>,
    config: &DescriptorCatalogConfig,
    available_resources: &HashSet<VaultResourceId>,
) -> Result<Resolved<'a>, ResolveError> {
    let entry = entry.ok_or_else(|| ResolveError::UnknownName(selector.to_owned()))?;
    let available = member_keys(entry);
    let written_name = selector.split_once("::").map_or(selector, |(name, _)| name);
    let parsed: QualifiedCatalogName =
        written_name
            .parse()
            .map_err(|error: pipette_plan_types::QualifiedNameError| {
                ResolveError::InvalidSelector {
                    selector: selector.to_owned(),
                    reason: error.to_string(),
                    available: available.clone(),
                }
            })?;
    if parsed.name.key() != entry.name.key() {
        return Err(ResolveError::UnknownName(selector.to_owned()));
    }
    if entry.state == DescriptorEntryState::Tombstoned {
        return Err(ResolveError::Tombstoned {
            name: selector.to_owned(),
            available,
        });
    }
    let name = entry.name.to_string();
    let warnings = if entry.state == DescriptorEntryState::Hidden {
        vec![Warning::HiddenName]
    } else {
        Vec::new()
    };

    if let Some((_, explicit_member)) = selector.split_once("::") {
        let selected = MemberSelector::parse_for_entry(selector, entry).map_err(|error| {
            ResolveError::InvalidSelector {
                selector: selector.to_owned(),
                reason: error.to_string(),
                available: available.clone(),
            }
        })?;
        let member = entry
            .members
            .iter()
            .find(|member| member.key() == selected.member_key)
            .ok_or_else(|| ResolveError::UnknownMember {
                name: name.clone(),
                member: explicit_member.to_owned(),
                available: available.clone(),
            })?;
        check_resources(member, &name, &available, available_resources)?;
        return Ok(Resolved { member, warnings });
    }

    // Positive capability clauses have a possible witness for every ruled
    // member. The implication check below must prove universal coverage; it
    // deliberately refuses when these clauses cannot prove that coverage.
    let candidate = entry.members.iter().find_map(|member| {
        let rule = coverage(config, member).ok()?;
        let build = member.app_build_flag();
        let could_offer = (!eligibility.requires.is_empty()
            && eligibility.any_of.iter().all(|g| !g.is_empty()))
            || eligibility
                .clients
                .iter()
                .any(|flags| offer_flags(config, member, flags).is_ok());
        could_offer.then_some((member, rule, build))
    });
    let (member, rule, build) = candidate.ok_or_else(|| {
        entry
            .members
            .iter()
            .find_map(|member| {
                member.app_build_flag()?;
                eligibility.clients.iter().find_map(|flags| {
                    match offer_flags(config, member, flags) {
                        Err(OfferError::NoBuild { flag }) => {
                            Some(ResolveError::ClientMissingBuild {
                                name: name.clone(),
                                candidate: member_key(member),
                                flag,
                            })
                        }
                        _ => None,
                    }
                })
            })
            .unwrap_or_else(|| ResolveError::NoCandidate {
                name: name.clone(),
                available: available.clone(),
            })
    })?;
    if let Some(flag) =
        eligibility
            .clients
            .iter()
            .find_map(|flags| match offer_flags(config, member, flags) {
                Err(OfferError::NoBuild { flag }) => Some(flag),
                _ => None,
            })
    {
        return Err(ResolveError::ClientMissingBuild {
            name,
            candidate: member_key(member),
            flag,
        });
    }
    let clients_pass = eligibility
        .clients
        .iter()
        .all(|flags| offer_flags(config, member, flags).is_ok());
    let capability_pass = eligibility.requires.is_empty()
        || (rule.requires.is_subset(eligibility.requires)
            && rule.any_of.iter().all(|group| {
                !group.is_disjoint(eligibility.requires)
                    || eligibility
                        .any_of
                        .iter()
                        .any(|variant| variant.is_subset(group))
            }));
    if !clients_pass || !capability_pass {
        return Err(ResolveError::NotOfferedToAll {
            name,
            candidate: member_key(member),
            available,
        });
    }
    if let Some(flag) = build
        && !eligibility.requires.is_empty()
        && !eligibility.requires.contains(&flag)
    {
        return Err(ResolveError::BuildNotRequired {
            name,
            candidate: member_key(member),
            flag,
        });
    }
    check_resources(member, &name, &available, available_resources)?;
    Ok(Resolved { member, warnings })
}

fn check_resources(
    member: &DescriptorMember,
    name: &str,
    available: &str,
    available_resources: &HashSet<VaultResourceId>,
) -> Result<(), ResolveError> {
    let mut missing: Vec<_> = named_resources(&member.descriptor())
        .into_iter()
        .filter(|id| !available_resources.contains(id))
        .collect();
    missing.sort();
    if let Some(resource_id) = missing.into_iter().next() {
        return Err(ResolveError::MissingVaultResource {
            name: name.to_owned(),
            member: member_key(member),
            resource_id,
            available: available.to_owned(),
        });
    }
    Ok(())
}

fn member_key(member: &DescriptorMember) -> String {
    match member.model_key() {
        Some((format, quantization)) => format!("{format}::{quantization}"),
        None => member.flavor().unwrap_or("<none>").to_owned(),
    }
}

fn member_keys(entry: &DescriptorEntry) -> String {
    entry
        .members
        .iter()
        .map(member_key)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use rstest::rstest;
    use serde_json::json;

    use pipette_core::types::Mint;
    use pipette_plan_types::{
        CatalogName, CatalogScope, DescriptorEntryCreator, DescriptorEntryKind, NonEmptyVec, OrgId,
        QuantizationLabel, VaultPath,
    };

    use super::*;

    fn flags(flags: &[&str]) -> BTreeSet<String> {
        flags.iter().map(|flag| (*flag).to_owned()).collect()
    }

    fn config() -> anyhow::Result<DescriptorCatalogConfig> {
        Ok(toml::from_str(
            r#"
[[offer_rules]]
runtime_type = "llamacpp_cli_stock_tools"
flavor = "macos-arm64"
requires = ["os:macos"]
any_of = [["chip:m1", "chip:m2"]]

[[offer_rules]]
runtime_type = "llamacpp_cli_stock_tools"
flavor = "macos-x64"
requires = ["os:macos", "chip:intel"]

[[offer_rules]]
runtime_type = "llamacpp_cli_stock_tools"
flavor = "linux-cuda-12-8"
requires = ["os_family:linux", "gpu:nvidia"]

[[offer_rules]]
runtime_type = "llamacpp_cli_stock_tools"
flavor = "linux-x64"
requires = ["os_family:linux"]

[[offer_rules]]
runtime_type = "llamacpp_ios_pipette"
flavor = "ios-arm64"
requires = ["os:ios"]
"#,
        )?)
    }

    fn entry(flavors: &[&str], state: DescriptorEntryState) -> anyhow::Result<DescriptorEntry> {
        let members = flavors
            .iter()
            .map(|flavor| {
                Ok(DescriptorMember::new(
                    serde_json::from_value(json!({
                        "type": "llamacpp_cli_stock_tools",
                        "source": "github_release",
                        "repository_url": "github.com/ggml-org/llama.cpp",
                        "repository_version": "b1",
                        "flavor": flavor
                    }))?,
                    Utc::now(),
                )?)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut entry = DescriptorEntry::new(
            CatalogScope::Global,
            CatalogName::try_new("llama")?,
            DescriptorEntryKind::Stable,
            DescriptorEntryState::Active,
            Utc::now(),
            Utc::now(),
            DescriptorEntryCreator::ImporterLlamacpp,
            NonEmptyVec::try_new(members)?,
        )?;
        entry.state = state;
        Ok(entry)
    }

    #[rstest]
    #[case::cuda_client(&["linux-cuda-12-8", "linux-x64"], &[], &[], &[&["os_family:linux", "gpu:nvidia"] as &[&str]], Some("linux-cuda-12-8"))]
    #[case::mac_only(&["macos-arm64", "macos-x64"], &["os:macos"], &[], &[], None)]
    #[case::apple_group(&["macos-arm64", "macos-x64"], &["os:macos"], &[&["chip:m1", "chip:m2"] as &[&str]], &[], Some("macos-arm64"))]
    #[case::union_passes(&["macos-arm64", "macos-x64"], &["os:macos", "chip:m1"], &[], &[&["os:macos", "chip:m2"] as &[&str]], Some("macos-arm64"))]
    #[case::union_fails(&["macos-arm64", "macos-x64"], &["os:macos", "chip:m1"], &[], &[&["os:macos", "chip:intel"] as &[&str]], None)]
    #[case::append_does_not_change(&["macos-arm64", "macos-x64", "linux-x64"], &["os:macos", "chip:m1"], &[], &[], Some("macos-arm64"))]
    fn bare_names(
        #[case] flavors: &[&str],
        #[case] requires: &[&str],
        #[case] any_of: &[&[&str]],
        #[case] clients: &[&[&str]],
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let entry = entry(flavors, DescriptorEntryState::Active)?;
        let requires = flags(requires);
        let any_of: Vec<_> = any_of.iter().map(|group| flags(group)).collect();
        let clients: Vec<_> = clients.iter().map(|client| flags(client)).collect();
        let result = resolve(
            "llama",
            Some(&entry),
            Eligibility {
                requires: &requires,
                any_of: &any_of,
                clients: &clients,
            },
            &config()?,
            &HashSet::new(),
        );
        match expected {
            Some(flavor) => assert_eq!(result?.member.flavor(), Some(flavor)),
            None => assert!(matches!(result, Err(ResolveError::NotOfferedToAll { .. }))),
        }
        Ok(())
    }

    /// How an iOS variant admits devices: a listed client, or `requires`.
    #[derive(Debug, Clone, Copy)]
    enum Admits {
        ClientWithBuild,
        ClientWithoutBuild,
        OneClientWithoutBuild,
        RequiresOs,
        RequiresOsAndBuild,
    }

    /// An entry with one `llamacpp_ios_pipette` build gives a bare name only to
    /// a variant whose every device reports that build, as the short list does.
    #[rstest]
    #[case::client_with_build("llama", Admits::ClientWithBuild, Ok(()))]
    #[case::client_without_build("llama", Admits::ClientWithoutBuild, Err("client_missing_build"))]
    #[case::one_client_without_build(
        "llama",
        Admits::OneClientWithoutBuild,
        Err("client_missing_build")
    )]
    #[case::requires_os_only("llama", Admits::RequiresOs, Err("build_not_required"))]
    #[case::requires_the_build("llama", Admits::RequiresOsAndBuild, Ok(()))]
    #[case::explicit_member("llama::ios-arm64", Admits::RequiresOs, Ok(()))]
    fn app_build_names(
        #[case] selector: &str,
        #[case] admits: Admits,
        #[case] expected: Result<(), &str>,
    ) -> anyhow::Result<()> {
        let member = DescriptorMember::new(
            serde_json::from_value(json!({
                "type": "llamacpp_ios_pipette",
                "repository_url": "github.com/ggml-org/llama.cpp",
                "repository_version": "b1",
                "flavor": "ios-arm64"
            }))?,
            Utc::now(),
        )?;
        let flag = member
            .app_build_flag()
            .ok_or_else(|| anyhow::anyhow!("an iOS app runtime has a build flag"))?;
        let entry = DescriptorEntry::new(
            CatalogScope::Global,
            CatalogName::try_new("llama")?,
            DescriptorEntryKind::HandMade,
            DescriptorEntryState::Active,
            Utc::now(),
            Utc::now(),
            DescriptorEntryCreator::Operator,
            NonEmptyVec::try_new(vec![member])?,
        )?;
        let (requires, clients) = match admits {
            Admits::ClientWithBuild => (flags(&[]), vec![flags(&["os:ios", &flag])]),
            Admits::ClientWithoutBuild => (flags(&[]), vec![flags(&["os:ios"])]),
            Admits::OneClientWithoutBuild => (
                flags(&[]),
                vec![flags(&["os:ios", &flag]), flags(&["os:ios"])],
            ),
            Admits::RequiresOs => (flags(&["os:ios"]), Vec::new()),
            Admits::RequiresOsAndBuild => (flags(&["os:ios", &flag]), Vec::new()),
        };
        let result = resolve(
            selector,
            Some(&entry),
            Eligibility {
                requires: &requires,
                any_of: &[],
                clients: &clients,
            },
            &config()?,
            &HashSet::new(),
        );
        if let Err(
            ResolveError::BuildNotRequired { flag: named, .. }
            | ResolveError::ClientMissingBuild { flag: named, .. },
        ) = &result
        {
            assert_eq!(named, &flag);
            assert!(
                result
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.to_string().contains(&flag))
            );
        }
        let result = result.map(|_| ()).map_err(|error| match error {
            ResolveError::NoCandidate { .. } => "no_candidate",
            ResolveError::NotOfferedToAll { .. } => "not_offered_to_all",
            ResolveError::ClientMissingBuild { .. } => "client_missing_build",
            ResolveError::BuildNotRequired { .. } => "build_not_required",
            ResolveError::UnknownName(_)
            | ResolveError::Tombstoned { .. }
            | ResolveError::InvalidSelector { .. }
            | ResolveError::MissingVaultResource { .. }
            | ResolveError::UnknownMember { .. } => "other",
        });
        assert_eq!(result, expected);
        Ok(())
    }

    #[test]
    fn explicit_member_skips_offer_check_even_without_a_rule() -> anyhow::Result<()> {
        let entry = entry(&["macos-arm64", "custom"], DescriptorEntryState::Hidden)?;
        let requires = flags(&["os:linux"]);
        let result = resolve(
            "global/llama::custom",
            Some(&entry),
            Eligibility {
                requires: &requires,
                any_of: &[],
                clients: &[],
            },
            &config()?,
            &HashSet::new(),
        )?;
        assert_eq!(result.member.flavor(), Some("custom"));
        assert_eq!(result.warnings, vec![Warning::HiddenName]);
        Ok(())
    }

    #[test]
    fn name_and_member_refusals() -> anyhow::Result<()> {
        let active_entry = entry(&["macos-arm64"], DescriptorEntryState::Active)?;
        let empty = BTreeSet::new();
        let eligibility = Eligibility {
            requires: &empty,
            any_of: &[],
            clients: &[],
        };
        let config = config()?;
        assert!(matches!(
            resolve("llama", None, eligibility, &config, &HashSet::new()),
            Err(ResolveError::UnknownName(_))
        ));
        assert!(matches!(
            resolve(
                "other",
                Some(&active_entry),
                eligibility,
                &config,
                &HashSet::new()
            ),
            Err(ResolveError::UnknownName(_))
        ));
        assert!(matches!(
            resolve(
                "llama::missing",
                Some(&active_entry),
                eligibility,
                &config,
                &HashSet::new()
            ),
            Err(ResolveError::UnknownMember { .. })
        ));
        assert!(matches!(
            resolve(
                "llama",
                Some(&active_entry),
                eligibility,
                &config,
                &HashSet::new()
            ),
            Err(ResolveError::NoCandidate { .. })
        ));
        let tombstone = entry(&["macos-arm64"], DescriptorEntryState::Tombstoned)?;
        assert!(matches!(
            resolve(
                "global/llama",
                Some(&tombstone),
                eligibility,
                &config,
                &HashSet::new()
            ),
            Err(ResolveError::Tombstoned { .. })
        ));
        let tombstone_error = resolve(
            "global/llama",
            Some(&tombstone),
            eligibility,
            &config,
            &HashSet::new(),
        )
        .err()
        .ok_or_else(|| anyhow::anyhow!("expected tombstone refusal"))?;
        assert!(tombstone_error.to_string().contains("macos-arm64"));
        let invalid_error = resolve(
            "llama::",
            Some(&active_entry),
            eligibility,
            &config,
            &HashSet::new(),
        )
        .err()
        .ok_or_else(|| anyhow::anyhow!("expected invalid selector refusal"))?;
        assert!(invalid_error.to_string().contains("macos-arm64"));
        let unruled = entry(&["custom"], DescriptorEntryState::Active)?;
        let mac = flags(&["os:macos"]);
        assert!(matches!(
            resolve(
                "llama",
                Some(&unruled),
                Eligibility {
                    requires: &mac,
                    ..eligibility
                },
                &config,
                &HashSet::new(),
            ),
            Err(ResolveError::NoCandidate { .. })
        ));
        Ok(())
    }

    #[test]
    fn refusal_names_candidate_and_available_members() -> anyhow::Result<()> {
        let entry = entry(&["macos-arm64", "macos-x64"], DescriptorEntryState::Active)?;
        let requires = flags(&["os:macos"]);
        let result = resolve(
            "llama",
            Some(&entry),
            Eligibility {
                requires: &requires,
                any_of: &[],
                clients: &[],
            },
            &config()?,
            &HashSet::new(),
        );
        let Err(error) = result else {
            anyhow::bail!("expected a coverage refusal");
        };
        let error = error.to_string();
        assert!(error.contains("selects `macos-arm64`"));
        assert!(error.contains("Available members: macos-arm64, macos-x64"));
        assert!(error.contains("Add a `requires`. Alternatively, select a member with `::`."));
        Ok(())
    }

    #[test]
    fn explicit_vault_member_requires_existing_resource() -> anyhow::Result<()> {
        let resource_id = VaultResourceId::mint();
        let member = DescriptorMember::new_model(
            serde_json::from_value(json!({
                "type": "gguf_text",
                "source": "vault_url",
                "url": VaultPath::of(resource_id.clone()).to_string(),
                "principal_id": "org_1"
            }))?,
            QuantizationLabel::try_new("q4_0")?,
            Utc::now(),
        )?;
        let entry = DescriptorEntry::new(
            CatalogScope::Org(OrgId::try_new("org_1")?),
            CatalogName::try_new("model")?,
            DescriptorEntryKind::HandMade,
            DescriptorEntryState::Active,
            Utc::now(),
            Utc::now(),
            DescriptorEntryCreator::Operator,
            NonEmptyVec::try_new(vec![member])?,
        )?;
        let empty = BTreeSet::new();
        let eligibility = Eligibility {
            requires: &empty,
            any_of: &[],
            clients: &[],
        };
        let selector = "model::gguf::q4_0";
        assert_eq!(member_keys(&entry), "gguf::q4_0");
        let missing = resolve(
            selector,
            Some(&entry),
            eligibility,
            &config()?,
            &HashSet::new(),
        );
        let message = missing
            .as_ref()
            .err()
            .ok_or_else(|| anyhow::anyhow!("expected missing-resource refusal"))?
            .to_string();
        assert!(message.contains("Available members: gguf::q4_0"));
        assert!(matches!(
            missing,
            Err(ResolveError::MissingVaultResource { resource_id: id, .. }) if id == resource_id
        ));
        let available = HashSet::from([resource_id]);
        assert!(resolve(selector, Some(&entry), eligibility, &config()?, &available).is_ok());
        Ok(())
    }
}
