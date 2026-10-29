import { describe, expect, test } from "vitest";
import { modelUri, runtimeUri } from "../src/lib/artifact-uri";
import type { Model, Runtime, UvRuntimeSource } from "../src/lib/api-types";

const hf = { source: "huggingface", org: "LiquidAI", repo_name: "LFM2.5-1.2B-Instruct-GGUF" } as const;
const vault = "/vault/resources/res_0123/content";
const principal = "org_1";

describe("a model as pipette-ctl --model spells it", () => {
  test.each<[string, Model, string]>([
    ["a GGUF file in a repo, pinned",
     { type: "gguf_text", ...hf, path: "LFM2.5-1.2B-Instruct-Q4_0.gguf", revision: "b421ad1d" },
     "gguf-text://repo=LiquidAI/LFM2.5-1.2B-Instruct-GGUF&path=LFM2.5-1.2B-Instruct-Q4_0.gguf&rev=b421ad1d"],
    ["a GGUF file at a URL", { type: "gguf_text", source: "url", url: "https://files.example/m-Q4_0.gguf" },
     "gguf-text://url=https://files.example/m-Q4_0.gguf"],
    ["a GGUF file in the vault", { type: "gguf_text", source: "vault_url", url: vault, principal_id: principal },
     "gguf-text://url=vault:res_0123"],
    ["a local GGUF file", { type: "gguf_text", source: "relative_file", path: "models/m.gguf" },
     "gguf-text://path=models/m.gguf"],
    ["a GGUF file on the host", { type: "gguf_text", source: "absolute_file", path: "/models/m.gguf" },
     "gguf-text://path=/models/m.gguf"],
    ["a GGUF vision model",
     { type: "gguf_vision", source: "huggingface", org: "ggml-org", repo_name: "gemma-3-4b-it-GGUF",
       model: "gemma-3-4b-it-Q4_K_M.gguf", mmproj: "mmproj-model-f16.gguf" },
     "gguf-vision://repo=ggml-org/gemma-3-4b-it-GGUF&model=gemma-3-4b-it-Q4_K_M.gguf&mmproj=mmproj-model-f16.gguf"],
    ["a GGUF vision model at URLs", { type: "gguf_vision", source: "url", model: "https://f.example/m.gguf",
                                      mmproj: "https://f.example/p.gguf", model_sha256: "ab" },
     "gguf-vision://model=https://f.example/m.gguf&mmproj=https://f.example/p.gguf&model_sha256=ab"],
    ["a GGUF vision model in the vault",
     { type: "gguf_vision", source: "vault_url", model: vault, mmproj: "/vault/resources/res_9/content",
       principal_id: principal },
     "gguf-vision://model=vault:res_0123&mmproj=vault:res_9"],
    ["local GGUF vision files", { type: "gguf_vision", source: "relative_files", model: "m.gguf", mmproj: "p.gguf" },
     "gguf-vision://model=m.gguf&mmproj=p.gguf"],
    ["GGUF vision files on the host", { type: "gguf_vision", source: "absolute_files", model: "/m.gguf",
                                        mmproj: "/p.gguf" },
     "gguf-vision://model=/m.gguf&mmproj=/p.gguf"],
    ["a speculative GGUF vision model with a separate drafter",
     { type: "gguf_vision_speculative", source: "huggingface", org: "o", repo_name: "VLM-GGUF", model: "m.gguf",
       mmproj: "p.gguf", draft: "d.gguf", revision: "r1" },
     "gguf-vision-speculative://repo=o/VLM-GGUF&model=m.gguf&mmproj=p.gguf&draft=d.gguf&rev=r1"],
    ["a speculative GGUF vision model at URLs, its drafter merged",
     { type: "gguf_vision_speculative", source: "url", model: "https://f.example/m.gguf",
       mmproj: "https://f.example/p.gguf", mmproj_sha256: "cd" },
     "gguf-vision-speculative://model=https://f.example/m.gguf&mmproj=https://f.example/p.gguf&mmproj_sha256=cd"],
    ["a speculative GGUF vision model in the vault",
     { type: "gguf_vision_speculative", source: "vault_url", model: vault, mmproj: "/vault/resources/res_9/content",
       draft: "/vault/resources/res_d/content", draft_sha256: "ef", principal_id: principal },
     "gguf-vision-speculative://model=vault:res_0123&mmproj=vault:res_9&draft=vault:res_d&draft_sha256=ef"],
    ["local speculative GGUF vision files",
     { type: "gguf_vision_speculative", source: "relative_files", model: "m.gguf", mmproj: "p.gguf", draft: "d.gguf" },
     "gguf-vision-speculative://model=m.gguf&mmproj=p.gguf&draft=d.gguf"],
    ["speculative GGUF vision files on the host",
     { type: "gguf_vision_speculative", source: "absolute_files", model: "/m.gguf", mmproj: "/p.gguf" },
     "gguf-vision-speculative://model=/m.gguf&mmproj=/p.gguf"],
    ["an MLX repo", { type: "mlx", source: "huggingface", org: "LiquidAI", repo_name: "LFM2.5-350M-MLX-4bit" },
     "mlx://repo=LiquidAI/LFM2.5-350M-MLX-4bit"],
    ["an OpenVINO variant inside a repo",
     { type: "openvino", source: "huggingface", org: "LiquidAI", repo_name: "LFM2.5-350M-ov", prefix: "int4-sym-cw" },
     "openvino://repo=LiquidAI/LFM2.5-350M-ov&prefix=int4-sym-cw"],
    ["an ONNX GenAI model in the vault",
     { type: "onnx_genai", source: "vault_archive", url: vault, principal_id: principal },
     "onnx-genai://url=vault:res_0123"],
    ["a local torch directory", { type: "torch", source: "relative_dir", dir: "models/llama" },
     "torch://dir=models/llama"],
    ["a torch directory on the host", { type: "torch", source: "absolute_dir", dir: "/models/llama" },
     "torch://dir=/models/llama"],
    ["an MLX speculative pair",
     { type: "mlx_speculative", target: { source: "huggingface", org: "LiquidAI", repo_name: "VL-3B-MLX-8bit" },
       draft: { source: "huggingface", org: "LiquidAI", repo_name: "VL-3B-DSpark", revision: "r2" } },
     "mlx-speculative://repo=LiquidAI/VL-3B-MLX-8bit&draft_repo=LiquidAI/VL-3B-DSpark&draft_rev=r2"],
    ["a torch speculative pair from the vault and a local drafter",
     { type: "torch_speculative", target: { source: "vault_archive", url: vault, principal_id: principal },
       draft: { source: "relative_dir", dir: "draft" } },
     "torch-speculative://url=vault:res_0123&draft_dir=draft"],
    ["Apple's on-device model", { type: "apple_foundation_text" }, "apple-foundation-text://"],
  ])("%s", (_, model, expected) => {
    expect(modelUri(model)).toBe(expected);
  });

  test("a type or source this build has no case for is spelt as its scheme alone", () => {
    // What a tab older than a deploy may read; the types cannot name it.
    const unseen = (value: object) => value as Model;
    expect(modelUri(unseen({ type: "gguf_audio", source: "huggingface", org: "o", repo_name: "r" })))
      .toBe("gguf-audio://");
    expect(modelUri(unseen({ type: "gguf_text", source: "s3", path: "m.gguf" }))).toBe("gguf-text://");
    expect(modelUri(unseen({ type: "mlx_speculative", target: hf, draft: { source: "s3", dir: "d" } })))
      .toBe("mlx-speculative://");
  });
});

describe("a runtime as pipette-ctl --runtime spells it", () => {
  const pip: UvRuntimeSource = { type: "pip_requirements_text", contents: "mlx-lm==0.31.3\n" };
  test.each<[string, Runtime, string]>([
    ["an upstream llama.cpp release",
     { type: "llamacpp_cli_stock_tools", source: "github_release", repository_url: "github.com/ggml-org/llama.cpp",
       repository_version: "b9305", flavor: "macos-arm64" },
     "llamacpp-cli-stock-tools://version=b9305&flavor=macos-arm64"],
    ["a llama.cpp fork's release asset",
     { type: "llamacpp_cli_stock_tools", source: "github_release", repository_url: "github.com/acme/llama.cpp",
       repository_version: "v1", asset: "llama-v1-bin.zip", flavor: "linux-x64-cpu" },
     "llamacpp-cli-stock-tools://repo=github.com/acme/llama.cpp&version=v1&asset=llama-v1-bin.zip&flavor=linux-x64-cpu"],
    ["a llama.cpp archive",
     { type: "llamacpp_cli_stock_tools", source: "remote_archive", url: "https://f.example/llama.tar.gz",
       flavor: "macos-arm64" },
     "llamacpp-cli-stock-tools://url=https://f.example/llama.tar.gz&flavor=macos-arm64"],
    ["a llama.cpp archive in the vault",
     { type: "llamacpp_cli_stock_tools", source: "vault_remote_archive", url: vault, principal_id: principal,
       flavor: "macos-arm64" },
     "llamacpp-cli-stock-tools://url=vault:res_0123&flavor=macos-arm64"],
    ["an installed llama.cpp", { type: "llamacpp_cli_stock_tools", source: "absolute_dir", dir: "/opt/llama",
                                 flavor: "macos-arm64" },
     "llamacpp-cli-stock-tools://dir=/opt/llama&flavor=macos-arm64"],
    ["a llama.cpp from a relative directory", { type: "llamacpp_cli_stock_tools", source: "relative_dir",
                                                 dir: "llama", flavor: "macos-arm64" },
     "llamacpp-cli-stock-tools://dir=llama&flavor=macos-arm64"],
    ["the Android app's llama.cpp",
     { type: "llamacpp_apk_pipette", repository_url: "github.com/ggml-org/llama.cpp", repository_version: "b9050",
       flavor: "android-arm64-v8" },
     "llamacpp-apk-pipette://version=b9050&flavor=android-arm64-v8"],
    ["the iOS app's llama.cpp behind the thermal gate",
     { type: "llamacpp_ios_pipette", repository_url: "github.com/ggml-org/llama.cpp", repository_version: "b9050",
       flavor: "ios-arm64", private_thermal: true },
     "llamacpp-ios-pipette://version=b9050&flavor=ios-arm64&private_thermal=true"],
    ["the iOS app's llama.cpp with the thermal read alone",
     { type: "llamacpp_ios_pipette", repository_url: "github.com/ggml-org/llama.cpp", repository_version: "b9050",
       flavor: "ios-arm64", private_thermal: true, private_memory: false },
     "llamacpp-ios-pipette://version=b9050&flavor=ios-arm64&private_thermal=true&private_memory=false"],
    ["MLX on a Mac", { type: "mlx_macos_pipette", version: "0.31.3", flavor: "macos-arm64", source: pip },
     "mlx-macos-pipette://version=0.31.3&flavor=macos-arm64"],
    ["the iOS app's MLX",
     { type: "mlx_ios_pipette", flavor: "ios-arm64",
       packages: { mlx_swift: { repository_url: "github.com/ml-explore/mlx-swift", repository_version: "0.25.6" },
                   mlx_swift_lm: { repository_url: "github.com/ml-explore/mlx-swift-lm", repository_version: "2.25" },
                   swift_transformers: { repository_url: "github.com/huggingface/swift-transformers",
                                         repository_version: "1.0.0" } } },
     "mlx-ios-pipette://mlx_swift=0.25.6&mlx_swift_lm=2.25&swift_transformers=1.0.0&flavor=ios-arm64"],
    ["vLLM in docker", { type: "docker_vllm", image_name: "vllm/vllm-openai", image_tag: "v0.20.2",
                         flavor: "nvidia_gpu" },
     "docker-vllm://image=vllm/vllm-openai&tag=v0.20.2&flavor=nvidia_gpu"],
    ["SGLang in docker", { type: "docker_sglang", image_name: "lmsysorg/sglang", image_tag: "v0.5.15-cu130",
                           flavor: "amd_gpu" },
     "docker-sglang://image=lmsysorg/sglang&tag=v0.5.15-cu130&flavor=amd_gpu"],
    ["vLLM in a uv venv", { type: "uv_vllm", server_version: "0.10.0", build: "cu121", python_version: "3.12",
                            source: { type: "pip_requirements_text", contents: "vllm==0.10.0" } },
     "uv-vllm://server=0.10.0&build=cu121&python=3.12"],
    ["SGLang in a uv venv", { type: "uv_sglang", server_version: "0.5.12", build: "cu121", python_version: "3.12",
                              source: { type: "relative_preinstalled", dir: "venvs/sglang" } },
     "uv-sglang://server=0.5.12&build=cu121&python=3.12"],
    ["OpenVINO GenAI", { type: "uv_openvino", server_version: "2026.2.1", python_version: "3.11", source: pip },
     "uv-openvino://version=2026.2.1"],
    ["ONNX GenAI on CUDA", { type: "uv_onnx_genai", server_version: "0.16.0", execution_provider: "cuda",
                             python_version: "3.11", source: pip },
     "uv-onnx-genai://version=0.16.0&ep=cuda"],
    ["an upstream OVMS release",
     { type: "ovms", source: "github_release", repository_url: "github.com/openvinotoolkit/model_server",
       repository_version: "v2025.4", flavor: "windows" },
     "ovms://version=v2025.4&flavor=windows"],
    ["an OVMS archive", { type: "ovms", source: "remote_archive", url: "https://f.example/ovms.zip", flavor: "linux" },
     "ovms://url=https://f.example/ovms.zip&flavor=linux"],
    ["a TensorRT Edge-LLM archive from the vault",
     { type: "trt_edgellm", source: "vault_remote_archive", url: "/vault/resources/vault_1/content",
       principal_id: "org_1", flavor: "jetson-orin-cuda-13" },
     "trt-edgellm://url=vault:vault_1&flavor=jetson-orin-cuda-13"],
    ["a TensorRT Edge-LLM directory", { type: "trt_edgellm", source: "relative_dir", dir: "trt", flavor: "custom" },
     "trt-edgellm://dir=trt&flavor=custom"],
    ["the iOS app's ONNX GenAI",
     { type: "onnx_genai_ios_pipette", flavor: "ios-arm64", private_thermal: true,
       packages: { onnxruntime: { repository_url: "github.com/microsoft/onnxruntime", repository_version: "v1.23.0" },
                   onnxruntime_genai: { repository_url: "github.com/microsoft/onnxruntime-genai",
                                        repository_version: "v0.10.0" } } },
     "onnx-genai-ios-pipette://onnxruntime=v1.23.0&onnxruntime_genai=v0.10.0&flavor=ios-arm64&private_thermal=true"],
    ["Apple Foundation behind the thermal gate", { type: "apple_foundation", private_thermal: true },
     "apple-foundation://private_thermal=true"],
    ["Apple Foundation with the memory reads alone", { type: "apple_foundation", private_memory: true },
     "apple-foundation://private_memory=true"],
    ["Apple Foundation", { type: "apple_foundation" }, "apple-foundation://"],
  ])("%s", (_, runtime, expected) => {
    expect(runtimeUri(runtime)).toBe(expected);
  });

  test("a type or source this build has no case for is spelt as its scheme alone", () => {
    const unseen = (value: object) => value as Runtime;
    expect(runtimeUri(unseen({ type: "uv_llamafile", server_version: "0.9" }))).toBe("uv-llamafile://");
    expect(runtimeUri(unseen({ type: "ovms", source: "s3", url: "s3://b/ovms.zip", flavor: "linux" })))
      .toBe("ovms://");
  });
});
