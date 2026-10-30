//! Fixtures for the tests that file a gated HuggingFace model: a mock Hub that
//! holds the models, the plan documents that name them, and a check that no
//! stored byte holds a token.

use std::collections::HashMap;
use std::path::Path;

use pipette_hub_testkit::{MockFile, MockGate, MockHub, MockRepo};

/// The gguf file of the gated gguf repository.
pub const GATED_GGUF_FILE: &str = "LFM2-700M-Q4_0.gguf";

/// A file of the gated gguf repository whose download answers `500`.
pub const FAILING_GGUF_FILE: &str = "failing.gguf";

/// A file of the gated gguf repository whose download answers `429`.
pub const RATE_LIMITED_GGUF_FILE: &str = "rate-limited.gguf";

/// The model file of the gated vision repository.
pub const VISION_MODEL_FILE: &str = "LFM2-VL-450M-Q4_0.gguf";

/// The bytes of [`VISION_MODEL_FILE`].
pub const VISION_MODEL_BYTES: &[u8] = b"GGUF vision weights";

/// The projector file of the gated vision repository. Its download waits
/// while a test holds `MockHub::hold_files`.
pub const VISION_MMPROJ_FILE: &str = "mmproj-LFM2-VL-450M-F16.gguf";

/// The gated models that [`gated_hub`] holds.
#[derive(Debug, Clone, Copy)]
pub enum GatedModel {
    Gguf,
    FailingGguf,
    /// A file whose download HuggingFace rate-limits.
    RateLimitedGguf,
    /// A repository whose resolve HuggingFace rate-limits.
    RateLimitedRepo,
    Mlx,
    Vision,
    /// An mlx speculative pair: a public target with no token, and a gated
    /// draft that carries the token.
    Speculative,
}

/// A mock Hub that holds every [`GatedModel`].
pub async fn gated_hub() -> anyhow::Result<MockHub> {
    MockHub::start(HashMap::from([
        (
            "LiquidAI/LFM2-700M-GGUF".to_string(),
            MockRepo {
                commit: "a".repeat(40),
                public: false,
                rate_limited: false,
                gate: MockGate::Open,
                files: vec![
                    MockFile::lfs(GATED_GGUF_FILE, b"GGUF weights"),
                    MockFile::lfs(FAILING_GGUF_FILE, b"GGUF failing").failing(),
                    MockFile::lfs(RATE_LIMITED_GGUF_FILE, b"GGUF rate-limited").rate_limited(),
                ],
            },
        ),
        (
            "LiquidAI/LFM2-350M-GGUF".to_string(),
            MockRepo {
                commit: "f".repeat(40),
                public: false,
                rate_limited: true,
                gate: MockGate::Open,
                files: vec![MockFile::lfs(GATED_GGUF_FILE, b"GGUF weights")],
            },
        ),
        (
            "LiquidAI/LFM2.5-350M-MLX-4bit".to_string(),
            MockRepo {
                commit: "b".repeat(40),
                public: false,
                rate_limited: false,
                gate: MockGate::Open,
                files: vec![
                    MockFile::git("config.json", b"{}"),
                    MockFile::lfs("model.safetensors", &[7; 64]),
                ],
            },
        ),
        (
            "LiquidAI/LFM2.5-1.2B-MLX-4bit".to_string(),
            MockRepo {
                commit: "e".repeat(40),
                public: true,
                rate_limited: false,
                gate: MockGate::Open,
                files: vec![
                    MockFile::git("config.json", b"{}"),
                    MockFile::lfs("model.safetensors", &[5; 48]),
                ],
            },
        ),
        (
            "LiquidAI/LFM2.5-350M-MLX-draft".to_string(),
            MockRepo {
                commit: "d".repeat(40),
                public: false,
                rate_limited: false,
                gate: MockGate::Open,
                files: vec![
                    MockFile::git("config.json", b"{}"),
                    MockFile::lfs("model.safetensors", &[9; 32]),
                ],
            },
        ),
        (
            "LiquidAI/LFM2-VL-450M-GGUF".to_string(),
            MockRepo {
                commit: "c".repeat(40),
                public: false,
                rate_limited: false,
                gate: MockGate::Open,
                files: vec![
                    MockFile::lfs(VISION_MODEL_FILE, VISION_MODEL_BYTES),
                    MockFile::lfs(VISION_MMPROJ_FILE, b"GGUF projector").held(),
                ],
            },
        ),
    ]))
    .await
}

/// A `[[variants]]` block of one macOS cell whose model is `model` with the
/// token `token` ([`gated_variant`]).
pub fn gated_macos_variant(model: GatedModel, token: &str) -> String {
    gated_variant(model, token, r#"["os:macos"]"#, "macos-arm64")
}

/// A plan document of one macOS cell whose model is `model` with the token
/// `token`.
pub fn gated_macos_document(model: GatedModel, token: &str) -> String {
    gated_document(&[gated_macos_variant(model, token)])
}

/// A `[[variants]]` block whose model is `model` with the token `token`. A
/// gguf model runs on a llama.cpp runtime for `flavor`, an mlx one on macOS.
pub fn gated_variant(model: GatedModel, token: &str, requires: &str, flavor: &str) -> String {
    let gguf_in = |repo_name: &str, path: &str| {
        format!(
            r#"{{ type = "gguf_text", source = "huggingface", org = "LiquidAI", repo_name = "{repo_name}", path = "{path}", auth_token = "{token}" }}"#
        )
    };
    let gguf = |path: &str| gguf_in("LFM2-700M-GGUF", path);
    let llamacpp = format!(
        r#"{{ type = "llamacpp_cli_stock_tools", source = "github_release", repository_url = "github.com/ggml-org/llama.cpp", version = "b9050", flavor = "{flavor}" }}"#
    );
    let mlx_runtime = r#"{ type = "mlx_macos_pipette", version = "0.31.3", flavor = "macos-arm64", source = { type = "pip_requirements_text", contents = "mlx-lm==0.31.3\n" } }"#;
    let (model, runtime) = match model {
        GatedModel::Gguf => (gguf(GATED_GGUF_FILE), llamacpp),
        GatedModel::FailingGguf => (gguf(FAILING_GGUF_FILE), llamacpp),
        GatedModel::RateLimitedGguf => (gguf(RATE_LIMITED_GGUF_FILE), llamacpp),
        GatedModel::RateLimitedRepo => (gguf_in("LFM2-350M-GGUF", GATED_GGUF_FILE), llamacpp),
        GatedModel::Mlx => (
            format!(
                r#"{{ type = "mlx", source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-4bit", auth_token = "{token}" }}"#
            ),
            mlx_runtime.to_owned(),
        ),
        GatedModel::Vision => (
            format!(
                r#"{{ type = "gguf_vision", source = "huggingface", org = "LiquidAI", repo_name = "LFM2-VL-450M-GGUF", model = "{VISION_MODEL_FILE}", mmproj = "{VISION_MMPROJ_FILE}", auth_token = "{token}" }}"#
            ),
            llamacpp,
        ),
        GatedModel::Speculative => (
            format!(
                r#"{{ type = "mlx_speculative", target = {{ source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-1.2B-MLX-4bit" }}, draft = {{ source = "huggingface", org = "LiquidAI", repo_name = "LFM2.5-350M-MLX-draft", auth_token = "{token}" }} }}"#
            ),
            mlx_runtime.to_owned(),
        ),
    };
    format!("\n[[variants]]\nrequires = {requires}\nmodels   = [{model}]\nruntimes = [{runtime}]\n")
}

/// A plan document of `variants`, each a [`gated_variant`] block. It names the
/// benchmark that `setup_benchmarks` writes for [`crate::ONE_CELL_PLAN`].
pub fn gated_document(variants: &[String]) -> String {
    format!(
        "benchmarks = [\"prefill_throughput_256\"]\n{}",
        variants.concat()
    )
}

/// No file under `dir` holds `auth_token` or a mock token. Every mock token
/// ends in `SECRET`.
pub fn assert_no_token_stored(dir: &Path) -> anyhow::Result<()> {
    let mut pending = vec![dir.to_path_buf()];
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            // A directory that cannot be read fails the check: a skipped
            // directory could hide a token.
            pending.extend(
                std::fs::read_dir(&path)?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            continue;
        }
        let text = String::from_utf8_lossy(&std::fs::read(&path)?).into_owned();
        if let Some(secret) = ["auth_token", "SECRET"]
            .into_iter()
            .find(|secret| text.contains(secret))
        {
            anyhow::bail!("{secret} is stored in {}", path.display());
        }
    }
    Ok(())
}
