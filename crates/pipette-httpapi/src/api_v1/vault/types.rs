//! The model and runtime types an upload takes, as the upload page and
//! `GET /api/v1/vault/types` offer them.

use pipette_plan_types::{
    LlamaCppFlavor, OvmsFlavor, TrtEdgellmFlavor, VaultFileRole, VaultType, VaultTypeFile,
    VaultTypes,
};

/// One model or runtime type an upload takes.
pub struct UploadType {
    pub type_name: &'static str,
    pub runtime: bool,
    pub description: &'static str,
    pub files: &'static [VaultTypeFile],
}

const fn required(role: VaultFileRole) -> VaultTypeFile {
    VaultTypeFile {
        role,
        optional: false,
        format: role.format(),
    }
}

const fn optional(role: VaultFileRole) -> VaultTypeFile {
    VaultTypeFile {
        optional: true,
        ..required(role)
    }
}

/// Every type an upload takes, written out for the page's wording; a test
/// holds it to the plan-types enums.
pub const UPLOAD_TYPES: &[UploadType] = {
    use VaultFileRole::{
        Archive, DirArchive, Draft, DraftDirArchive, Mmproj, Model, TargetDirArchive,
    };
    const RUNTIME: &str = "A prebuilt for the flavor, laid out as upstream's release archive for \
                           it: zip on Windows, tar.gz elsewhere.";
    &[
        UploadType {
            type_name: "gguf_text",
            runtime: false,
            description: "One .gguf.",
            files: &[required(Model)],
        },
        UploadType {
            type_name: "gguf_vision",
            runtime: false,
            description: "The weights and the projector, a .gguf each.",
            files: &[required(Model), required(Mmproj)],
        },
        UploadType {
            type_name: "gguf_vision_speculative",
            runtime: false,
            description: "As gguf_vision, plus the drafter's .gguf unless it is merged into the model.",
            files: &[required(Model), required(Mmproj), optional(Draft)],
        },
        UploadType {
            type_name: "mlx",
            runtime: false,
            description: "The MLX bundle directory (safetensors and config), packed.",
            files: &[required(DirArchive)],
        },
        UploadType {
            type_name: "mlx_speculative",
            runtime: false,
            description: "The target's and the drafter's MLX directories, each packed.",
            files: &[required(TargetDirArchive), required(DraftDirArchive)],
        },
        UploadType {
            type_name: "torch",
            runtime: false,
            description: "The safetensors checkpoint directory, packed.",
            files: &[required(DirArchive)],
        },
        UploadType {
            type_name: "torch_speculative",
            runtime: false,
            description: "The target's and the drafter's checkpoint directories, each packed.",
            files: &[required(TargetDirArchive), required(DraftDirArchive)],
        },
        UploadType {
            type_name: "openvino",
            runtime: false,
            description: "The OpenVINO IR directory, packed, with its tokenizer and detokenizer IRs.",
            files: &[required(DirArchive)],
        },
        UploadType {
            type_name: "onnx_genai",
            runtime: false,
            description: "The ONNX Runtime GenAI directory, packed: graph, weights, \
                   genai_config.json and tokenizer files.",
            files: &[required(DirArchive)],
        },
        UploadType {
            type_name: "llamacpp_cli_stock_tools",
            runtime: true,
            description: RUNTIME,
            files: &[required(Archive)],
        },
        UploadType {
            type_name: "ovms",
            runtime: true,
            description: RUNTIME,
            files: &[required(Archive)],
        },
        UploadType {
            type_name: "trt_edgellm",
            runtime: true,
            description: "The TensorRT Edge-LLM server archive for the flavor's SoC and CUDA \
                          major, as a tar.gz.",
            files: &[required(Archive)],
        },
    ]
};

/// Every type an upload takes, each with the known flavors a runtime suggests
/// (a custom one is typed in): the upload page's table, and
/// `GET /api/v1/vault/types`.
pub fn upload_types() -> VaultTypes {
    let (runtimes, models): (Vec<&UploadType>, Vec<&UploadType>) = UPLOAD_TYPES
        .iter()
        .partition(|upload_type| upload_type.runtime);
    let listed = |types: Vec<&UploadType>| types.into_iter().map(vault_type).collect();
    VaultTypes {
        models: listed(models),
        runtimes: listed(runtimes),
    }
}

fn vault_type(upload_type: &UploadType) -> VaultType {
    VaultType {
        type_name: upload_type.type_name.to_string(),
        description: upload_type.description.to_string(),
        files: upload_type.files.to_vec(),
        flavors: match upload_type.type_name {
            "llamacpp_cli_stock_tools" => LlamaCppFlavor::known().map(|f| f.to_string()).collect(),
            "ovms" => OvmsFlavor::known().map(|f| f.to_string()).collect(),
            "trt_edgellm" => TrtEdgellmFlavor::known().map(|f| f.to_string()).collect(),
            _ => Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use anyhow::Context;
    use serde_json::json;
    use strum::VariantNames as _;

    use pipette_plan_types::{
        UploadVaultResourcesRequest, VaultArtifact, VaultModel, VaultRuntime, VaultUploadFile,
    };

    use super::*;

    /// The page offers exactly the types an upload takes, each with exactly
    /// its files: all of them parse, and leaving one out parses only when the
    /// page calls it optional.
    #[test]
    fn the_upload_page_offers_every_type_with_its_files() -> anyhow::Result<()> {
        let offered: BTreeSet<&str> = UPLOAD_TYPES.iter().map(|t| t.type_name).collect();
        let runtimes: BTreeSet<&str> = VaultRuntime::<VaultUploadFile>::VARIANTS
            .iter()
            .copied()
            .collect();
        let taken: BTreeSet<&str> = VaultModel::<VaultUploadFile>::VARIANTS
            .iter()
            .copied()
            .chain(runtimes.iter().copied())
            .collect();
        assert_eq!(offered, taken);
        let offered_now = upload_types();
        let listed: BTreeSet<&str> = offered_now
            .runtimes
            .iter()
            .map(|t| t.type_name.as_str())
            .collect();
        assert_eq!(listed, runtimes, "the runtimes are listed as runtimes");
        let suggesting: BTreeSet<&str> = offered_now
            .models
            .iter()
            .chain(&offered_now.runtimes)
            .filter(|t| !t.flavors.is_empty())
            .map(|t| t.type_name.as_str())
            .collect();
        assert_eq!(suggesting, runtimes, "every runtime suggests its flavors");

        UPLOAD_TYPES.iter().try_for_each(|upload_type| {
            let file = json!({"filename": "LFM2-1.2B.gguf", "sha256": "a".repeat(64)});
            let mut body = serde_json::Map::new();
            body.insert("name".into(), json!("LFM2 1.2B"));
            body.insert("type".into(), json!(upload_type.type_name));
            if upload_type.runtime {
                body.insert("flavor".into(), json!("windows-x64"));
            }
            upload_type.files.iter().for_each(|field| {
                body.insert(field.role.to_string(), file.clone());
            });
            let upload: UploadVaultResourcesRequest =
                serde_json::from_value(body.clone().into()).with_context(|| format!("{body:?}"))?;
            assert_eq!(
                matches!(upload.artifact, VaultArtifact::Runtime(_)),
                upload_type.runtime,
                "{}",
                upload_type.type_name
            );
            upload_type.files.iter().for_each(|field| {
                let mut without = body.clone();
                without.remove(&field.role.to_string());
                assert_eq!(
                    serde_json::from_value::<UploadVaultResourcesRequest>(without.into()).is_ok(),
                    field.optional,
                    "{} without {}",
                    upload_type.type_name,
                    field.role
                );
            });
            anyhow::Ok(())
        })
    }
}
