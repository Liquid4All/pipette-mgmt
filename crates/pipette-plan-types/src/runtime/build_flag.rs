//! The capability flag that names one iOS app runtime build. The catalog's build
//! check reads this flag.

use super::Runtime;
use crate::{CapabilityFlag, CapabilityFlagError, descriptor_digest};

/// The namespace and key at the start of a [`runtime_build_flag`]. The digest
/// follows this prefix.
const RUNTIME_BUILD_FLAG_PREFIX: &str = "runtime:descriptor:";

/// The error that [`runtime_build_flag`] returns when it cannot form the flag.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeBuildFlagError {
    /// The descriptor does not serialize to JSON.
    #[error("runtime descriptor did not serialize for its digest: {0}")]
    Digest(#[from] serde_json::Error),
    /// The flag does not have the canonical form of a capability flag.
    #[error("runtime build flag is not canonical: {0}")]
    Flag(#[from] CapabilityFlagError),
}

/// Returns `runtime:descriptor:<digest>` for a runtime that the iOS app includes.
/// `<digest>` is the [`descriptor_digest`] of the full descriptor. The private
/// APIs and every package pin are part of the build identity.
///
/// Returns `None` for every other runtime type. A device gets those builds by
/// other means. The catalog offers them with its offer rules only.
pub fn runtime_build_flag(
    runtime: &Runtime,
) -> Result<Option<CapabilityFlag>, RuntimeBuildFlagError> {
    match runtime {
        Runtime::LlamacppIosPipette(_)
        | Runtime::MlxIosPipette(_)
        | Runtime::OnnxGenaiIosPipette(_) => {
            let digest = descriptor_digest(runtime)?;
            Ok(Some(CapabilityFlag::try_new(format!(
                "{RUNTIME_BUILD_FLAG_PREFIX}{digest}"
            ))?))
        }
        Runtime::LlamacppCliStockTools(_)
        | Runtime::LlamacppApkPipette(_)
        | Runtime::MlxMacosPipette(_)
        | Runtime::AppleCoreAiMacosPipette(_)
        | Runtime::DockerVllm(_)
        | Runtime::DockerSglang(_)
        | Runtime::UvVllm(_)
        | Runtime::UvSglang(_)
        | Runtime::UvOpenvino(_)
        | Runtime::UvOnnxGenai(_)
        | Runtime::Ovms(_)
        | Runtime::AppleFoundation(_)
        | Runtime::TrtEdgellm(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    /// These are fixed test vectors. Clients copy these values into their own
    /// tests. If a value changes, every reported build flag changes.
    #[rstest]
    #[case::llamacpp(
        json!({
            "type": "llamacpp_ios_pipette",
            "repository_url": "github.com/ggml-org/llama.cpp",
            "repository_version": "b10216",
            "flavor": "ios-arm64",
        }),
        Some("runtime:descriptor:6cef53d3f274d3f2a4c449b830556b5c7abadbe43b88c90dac8600d4a6442dc8")
    )]
    #[case::llamacpp_private_thermal(
        json!({
            "type": "llamacpp_ios_pipette",
            "repository_url": "github.com/ggml-org/llama.cpp",
            "repository_version": "b10216",
            "flavor": "ios-arm64",
            "private_thermal": true,
        }),
        Some("runtime:descriptor:98bbe4ff3a7c6ef337aa303dca5fbcc178f0e087fd006d6f4af152fcc4a06971")
    )]
    // A `private_memory` that equals `private_thermal` is not part of the digest.
    #[case::llamacpp_private_memory_redundant(
        json!({
            "type": "llamacpp_ios_pipette",
            "repository_url": "github.com/ggml-org/llama.cpp",
            "repository_version": "b10216",
            "flavor": "ios-arm64",
            "private_thermal": true,
            "private_memory": true,
        }),
        Some("runtime:descriptor:98bbe4ff3a7c6ef337aa303dca5fbcc178f0e087fd006d6f4af152fcc4a06971")
    )]
    #[case::llamacpp_private_thermal_only(
        json!({
            "type": "llamacpp_ios_pipette",
            "repository_url": "github.com/ggml-org/llama.cpp",
            "repository_version": "b10216",
            "flavor": "ios-arm64",
            "private_thermal": true,
            "private_memory": false,
        }),
        Some("runtime:descriptor:960f8f831d806976bacf55a0d9a6821b668a7c7661f041b35dd9c9f64bbdd2e7")
    )]
    #[case::mlx(
        json!({
            "type": "mlx_ios_pipette",
            "packages": {
                "mlx_swift": {
                    "repository_url": "github.com/ml-explore/mlx-swift",
                    "repository_version": "0.32.3",
                },
                "mlx_swift_lm": {
                    "repository_url": "github.com/ml-explore/mlx-swift-lm",
                    "repository_version": "3.32.3",
                },
                "swift_transformers": {
                    "repository_url": "github.com/huggingface/swift-transformers",
                    "repository_version": "1.3.4",
                },
            },
            "flavor": "ios-arm64",
        }),
        Some("runtime:descriptor:071a9c8beeb12f0fb9e961bab573e2d0ee6a3292ab97887b4cf49f19b2053466")
    )]
    // `descriptor.rs` pins the same digest for this descriptor.
    #[case::onnx_genai(
        json!({
            "type": "onnx_genai_ios_pipette",
            "packages": {
                "onnxruntime": {
                    "repository_url": "github.com/microsoft/onnxruntime",
                    "repository_version": "v1.26.0",
                },
                "onnxruntime_genai": {
                    "repository_url": "github.com/microsoft/onnxruntime-genai",
                    "repository_version": "v0.15.2",
                },
            },
            "flavor": "ios-arm64",
        }),
        Some("runtime:descriptor:529c5b33969fd906c9cae6a75bc714486be88834cc946421724df20000821653")
    )]
    #[case::apple_foundation(json!({"type": "apple_foundation"}), None)]
    #[case::llamacpp_apk(
        json!({
            "type": "llamacpp_apk_pipette",
            "repository_url": "github.com/ggml-org/llama.cpp",
            "repository_version": "b10216",
            "flavor": "android-arm64-v8",
        }),
        None
    )]
    fn only_an_ios_app_runtime_has_a_build_flag(
        #[case] descriptor: serde_json::Value,
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let runtime: Runtime = serde_json::from_value(descriptor)?;
        let flag = runtime_build_flag(&runtime)?.map(|flag| flag.into_inner());
        assert_eq!(flag.as_deref(), expected);
        Ok(())
    }
}
