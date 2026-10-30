//! Pure device offer checks for ordered descriptor catalog members.

use std::collections::BTreeSet;

use pipette_core::{
    client::Client,
    config::{DescriptorCatalogConfig, OfferRule},
    matching::{FleetTags, capabilities_satisfy},
};
use pipette_plan_types::{
    DescriptorEntry, DescriptorMember, Model, ModelType, Runtime, RuntimeType, is_compatible,
};
/// The reason that the rules do not offer a member to a device.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OfferError {
    #[error("no offer rule for `{runtime_type}` `{flavor}`")]
    NoRule {
        runtime_type: RuntimeType,
        flavor: String,
    },
    #[error("no offer rules exist for runtime type `{runtime_type}`")]
    Unsupported { runtime_type: RuntimeType },
    #[error("a model member has no offer rules")]
    ModelMember,
    #[error("device does not meet the offer rule")]
    NotMet,
    #[error("no offered runtime loads `{model_type}`")]
    NoLoadingRuntime { model_type: ModelType },
}

/// A member, and the reason that the rules do not offer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withheld<'a> {
    pub member: &'a DescriptorMember,
    pub reason: OfferError,
}

/// The first member that the rules offer or, when the rules offer no member,
/// the reason for each member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution<'a> {
    Resolved(&'a DescriptorMember),
    Withheld(Vec<Withheld<'a>>),
}

/// Check one member against the flags that job matching sees for `client`:
/// its effective capabilities and its assigned tags.
pub fn offer(
    config: &DescriptorCatalogConfig,
    member: &DescriptorMember,
    client: &Client,
    tags: &FleetTags,
) -> Result<(), OfferError> {
    offer_flags(config, member, &tags.capabilities_of(client))
}

/// Check one member against a device's flags, as computed by
/// [`FleetTags::capabilities_of`].
fn offer_flags(
    config: &DescriptorCatalogConfig,
    member: &DescriptorMember,
    flags: &BTreeSet<String>,
) -> Result<(), OfferError> {
    let rule = coverage(config, member)?;
    if capabilities_satisfy(&rule.requires, &rule.any_of, flags) {
        Ok(())
    } else {
        Err(OfferError::NotMet)
    }
}

/// Check one model against the runtime members that the rules offer to a
/// device. A model has no offer rules of its own: the device is offered the
/// model if it is offered a runtime that can load it.
pub fn offer_model(model: &Model, offered: &[Runtime]) -> Result<(), OfferError> {
    if offered.iter().any(|runtime| is_compatible(model, runtime)) {
        Ok(())
    } else {
        Err(OfferError::NoLoadingRuntime {
            model_type: ModelType::of(model),
        })
    }
}

/// Find the offer rule for a member without testing a device's flags.
pub fn coverage<'a>(
    config: &'a DescriptorCatalogConfig,
    member: &DescriptorMember,
) -> Result<&'a OfferRule, OfferError> {
    let runtime_type = RuntimeType::of(member.runtime().ok_or(OfferError::ModelMember)?);
    let no_rule = || OfferError::NoRule {
        runtime_type,
        flavor: member.flavor().unwrap_or("<none>").to_owned(),
    };
    if DescriptorCatalogConfig::known_flavors(runtime_type).is_none() {
        return Err(OfferError::Unsupported { runtime_type });
    }
    let flavor = member.flavor().ok_or_else(no_rule)?;
    config.rule_for(runtime_type, flavor).ok_or_else(no_rule)
}

/// Return the first member, in the entry's stored order, that the rules offer
/// to `client`. The flags are the ones that job matching sees.
pub fn resolve<'a>(
    entry: &'a DescriptorEntry,
    config: &DescriptorCatalogConfig,
    client: &Client,
    tags: &FleetTags,
) -> Resolution<'a> {
    let flags = tags.capabilities_of(client);
    if let Some(member) = entry
        .members
        .iter()
        .find(|member| offer_flags(config, member, &flags).is_ok())
    {
        Resolution::Resolved(member)
    } else {
        Resolution::Withheld(
            entry
                .members
                .iter()
                .filter_map(|member| {
                    offer_flags(config, member, &flags)
                        .err()
                        .map(|reason| Withheld { member, reason })
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use chrono::Utc;
    use rstest::rstest;
    use serde_json::json;

    use pipette_core::client::ClientStatus;
    use pipette_plan_types::{
        CatalogName, ClientId, ContactEmail, DescriptorEntryCreator, DescriptorEntryKind,
        DescriptorEntryState, DeviceProfile, NonEmptyTrimmedString, NonEmptyVec, PublicKeyHex,
    };

    use self::Runtime::{AppleFoundation, Llamacpp, Ovms};
    use super::*;

    /// A member's runtime: llama.cpp or OVMS with a flavor, or Apple Foundation.
    #[derive(Debug, Clone, Copy)]
    enum Runtime {
        Llamacpp(&'static str),
        Ovms(&'static str),
        AppleFoundation,
    }

    /// The expected result of `resolve`: the offered flavor, or each reason.
    #[derive(Debug)]
    enum Outcome {
        Resolved(&'static str),
        Withheld(&'static [&'static str]),
    }

    fn member(runtime: Runtime) -> anyhow::Result<DescriptorMember> {
        let runtime = match runtime {
            Runtime::Llamacpp(flavor) => json!({
                "type": "llamacpp_cli_stock_tools", "source": "github_release",
                "repository_url": "github.com/ggml-org/llama.cpp", "repository_version": "b1",
                "flavor": flavor
            }),
            Runtime::Ovms(flavor) => json!({
                "type": "ovms", "source": "github_release",
                "repository_url": "github.com/openvinotoolkit/model_server",
                "repository_version": "v2026.4.0", "flavor": flavor
            }),
            Runtime::AppleFoundation => json!({"type": "apple_foundation"}),
        };
        Ok(DescriptorMember::new(
            serde_json::from_value(runtime)?,
            Utc::now(),
        )?)
    }

    fn entry(runtimes: &[Runtime]) -> anyhow::Result<DescriptorEntry> {
        let members = NonEmptyVec::try_new(
            runtimes
                .iter()
                .map(|runtime| member(*runtime))
                .collect::<anyhow::Result<Vec<_>>>()?,
        )?;
        let now = Utc::now();
        Ok(DescriptorEntry::new(
            pipette_plan_types::CatalogScope::Global,
            CatalogName::try_new("llama")?,
            DescriptorEntryKind::Stable,
            DescriptorEntryState::Active,
            now,
            now,
            DescriptorEntryCreator::ImporterLlamacpp,
            members,
        )?)
    }

    /// An approved client whose profile reports `os` and, if given, `chip`.
    fn client(os: &str, chip: Option<&str>) -> anyhow::Result<Client> {
        Ok(Client {
            client_id: ClientId::try_new("ev1_device")?,
            public_key: PublicKeyHex::try_new("ab".repeat(32))?,
            organization: NonEmptyTrimmedString::try_new("lab")?,
            client_details: NonEmptyTrimmedString::try_new("rig")?,
            contact_email: ContactEmail::try_new("ops@example.test")?,
            status: ClientStatus::Approved,
            registered_at: Utc::now(),
            device_profile: DeviceProfile {
                device_os_name: Some(NonEmptyTrimmedString::try_new(os)?),
                device_chip_model: chip.map(NonEmptyTrimmedString::try_new).transpose()?,
                ..Default::default()
            },
            capabilities: Default::default(),
        })
    }

    fn rules() -> anyhow::Result<DescriptorCatalogConfig> {
        Ok(toml::from_str(
            r#"
[[offer_rules]]
runtime_type = "llamacpp_cli_stock_tools"
flavor = "macos-arm64"
requires = ["os:macos"]
any_of = [["chip:applem1", "chip:applem1pro", "chip:applem1max", "chip:applem1ultra",
           "chip:applem2", "chip:applem2pro", "chip:applem2max", "chip:applem2ultra",
           "chip:applem3", "chip:applem3pro", "chip:applem3max", "chip:applem3ultra",
           "chip:applem4", "chip:applem4pro", "chip:applem4max", "chip:applem4ultra",
           "chip:applem5", "chip:applem5pro", "chip:applem5max", "chip:applem5ultra"]]
[[offer_rules]]
runtime_type = "llamacpp_cli_stock_tools"
flavor = "macos-x64"
requires = ["os:macos"]
[[offer_rules]]
runtime_type = "ovms"
flavor = "windows-x64"
requires = ["os:windows"]
"#,
        )?)
    }

    #[rstest]
    #[case::apple_silicon_mac(&[Llamacpp("macos-arm64"), Llamacpp("macos-x64")], ("macOS", Some("Apple M4")), Outcome::Resolved("macos-arm64"))]
    #[case::intel_mac(&[Llamacpp("macos-arm64"), Llamacpp("macos-x64")], ("macOS", None), Outcome::Resolved("macos-x64"))]
    #[case::linux_host_no_rule(&[Llamacpp("linux-x64-cpu"), Llamacpp("linux-arm64-cpu")], ("Linux", None), Outcome::Withheld(&["no offer rule for `llamacpp_cli_stock_tools` `linux-x64-cpu`", "no offer rule for `llamacpp_cli_stock_tools` `linux-arm64-cpu`"]))]
    #[case::unsupported_runtime(&[AppleFoundation], ("macOS", None), Outcome::Withheld(&["no offer rules exist for runtime type `apple_foundation`"]))]
    #[case::precedence_first_offered(&[Llamacpp("macos-x64"), Llamacpp("macos-arm64")], ("macOS", Some("Apple M4")), Outcome::Resolved("macos-x64"))]
    #[case::rule_not_met(&[Llamacpp("macos-arm64")], ("macOS", None), Outcome::Withheld(&["device does not meet the offer rule"]))]
    #[case::unruled_member_skipped(&[Llamacpp("linux-x64-cpu"), Llamacpp("macos-x64")], ("macOS", None), Outcome::Resolved("macos-x64"))]
    #[case::ovms_windows_host(&[Ovms("windows-x64")], ("Windows", None), Outcome::Resolved("windows-x64"))]
    #[case::ovms_rule_not_met(&[Ovms("windows-x64")], ("Linux", None), Outcome::Withheld(&["device does not meet the offer rule"]))]
    fn resolution_cases(
        #[case] runtimes: &[Runtime],
        #[case] (os, chip): (&str, Option<&str>),
        #[case] expected: Outcome,
    ) -> anyhow::Result<()> {
        let entry = entry(runtimes)?;
        let client = client(os, chip)?;
        match (
            resolve(&entry, &rules()?, &client, &FleetTags::default()),
            expected,
        ) {
            (Resolution::Resolved(member), Outcome::Resolved(flavor)) => {
                assert_eq!(member.flavor(), Some(flavor))
            }
            (Resolution::Withheld(withheld), Outcome::Withheld(reasons)) => assert_eq!(
                withheld
                    .iter()
                    .map(|item| item.reason.to_string())
                    .collect::<Vec<_>>(),
                reasons
            ),
            (actual, expected) => anyhow::bail!("expected {expected:?}, got {actual:?}"),
        }
        Ok(())
    }

    #[test]
    fn appending_a_member_keeps_the_earlier_resolution() -> anyhow::Result<()> {
        let mut entry = entry(&[Llamacpp("macos-x64")])?;
        let client = client("macOS", Some("Apple M4"))?;
        assert_eq!(
            resolve(&entry, &rules()?, &client, &FleetTags::default()),
            Resolution::Resolved(entry.members.head())
        );
        let first = entry.members.head().clone();
        entry.members = NonEmptyVec::try_new(vec![first, member(Llamacpp("macos-arm64"))?])?;
        assert_eq!(
            resolve(&entry, &rules()?, &client, &FleetTags::default()),
            Resolution::Resolved(entry.members.head())
        );
        Ok(())
    }

    #[rstest]
    #[case::covered(Runtime::Llamacpp("macos-arm64"), Ok(()))]
    #[case::no_rule(Runtime::Llamacpp("linux-x64-rocm"), Err("no_rule"))]
    #[case::unsupported(Runtime::AppleFoundation, Err("unsupported"))]
    fn coverage_cases(
        #[case] runtime: Runtime,
        #[case] expected: Result<(), &str>,
    ) -> anyhow::Result<()> {
        let member = member(runtime)?;
        let result = coverage(&rules()?, &member)
            .map(|_| ())
            .map_err(|error| match error {
                OfferError::NoRule { .. } => "no_rule",
                OfferError::Unsupported { .. } => "unsupported",
                OfferError::NotMet => "not_met",
                OfferError::ModelMember => "model_member",
                OfferError::NoLoadingRuntime { .. } => "no_loading_runtime",
            });
        assert_eq!(result, expected);
        Ok(())
    }

    /// A model descriptor of `model_type`, pinned on Hugging Face.
    fn model(model_type: &str) -> anyhow::Result<Model> {
        let mut fields = json!({
            "type": model_type, "source": "huggingface", "org": "acme", "repo_name": "AcmeLM-3B",
            "revision": "0123456789abcdef0123456789abcdef01234567"
        });
        if model_type == "gguf_text" {
            fields["path"] = json!("model.gguf");
        }
        Ok(serde_json::from_value(fields)?)
    }

    #[rstest]
    #[case::gguf_by_llamacpp("gguf_text", &[Llamacpp("macos-arm64")], None)]
    #[case::openvino_by_ovms("openvino", &[Llamacpp("macos-arm64"), Ovms("windows-x64")], None)]
    #[case::onnx_by_neither("onnx_genai", &[Llamacpp("macos-arm64"), Ovms("windows-x64")], Some("no offered runtime loads `onnx_genai`"))]
    #[case::mlx_by_neither("mlx", &[Llamacpp("macos-arm64")], Some("no offered runtime loads `mlx`"))]
    #[case::gguf_with_nothing_offered("gguf_text", &[], Some("no offered runtime loads `gguf_text`"))]
    fn model_offer_cases(
        #[case] model_type: &str,
        #[case] offered: &[Runtime],
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let offered = offered
            .iter()
            .map(|runtime| {
                member(*runtime)?
                    .runtime()
                    .cloned()
                    .context("a runtime member")
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let result = offer_model(&model(model_type)?, &offered).map_err(|e| e.to_string());
        assert_eq!(result.err().as_deref(), expected);
        Ok(())
    }

    #[test]
    fn a_model_member_has_no_offer_rule() -> anyhow::Result<()> {
        let model = serde_json::from_value(json!({
            "type": "mlx", "source": "huggingface", "org": "acme", "repo_name": "AcmeLM-3B",
            "revision": "0123456789abcdef0123456789abcdef01234567"
        }))?;
        let member = DescriptorMember::new_model(
            model,
            pipette_plan_types::QuantizationLabel::try_new("4bit")?,
            Utc::now(),
        )?;
        assert_eq!(
            coverage(&rules()?, &member).err(),
            Some(OfferError::ModelMember)
        );
        Ok(())
    }
}
