// A model or runtime in the compact `scheme://key=value&…` form `pipette-ctl`
// takes for `--model` and `--runtime` (pipette-clients,
// `pipette-cli/src/model_uri.rs` and `runtime_uri.rs`): the scheme is the type
// with `-` for `_`, the keys and their order the CLI's. What the CLI cannot
// address (on-device apps, vault, local paths) is spelt the same way from its
// own fields. The cases follow the generated types, so a variant without one
// fails the type check; one met at run time (a tab older than a deploy) is
// spelt as its scheme alone.

import type {
  GgufVision, GgufVisionSpeculative, LlamacppCliStockTools, Model, ModelSource, Ovms, Runtime, SourceRepository,
  TrtEdgellm,
} from "./api-types";

/** A key and its value; a pair without one is left out. */
type Pairs = [string, string | null | undefined][];

const LLAMACPP_UPSTREAM = "github.com/ggml-org/llama.cpp";
const OVMS_UPSTREAM = "github.com/openvinotoolkit/model_server";
const VAULT = /^\/vault\/resources\/([^/]+)\/content$/;

/** A vault path by its resource, `vault:{id}`; any other path or URL as is. */
function located(path: string | null | undefined): string | null | undefined {
  const vault = path ? VAULT.exec(path) : null;
  return vault ? `vault:${vault[1]}` : path;
}

const repo = (r: { org: string; repo_name: string }) => `${r.org}/${r.repo_name}`;

/** A switch's arm for a variant this build has no case for: no pairs, so the URI is the scheme alone. */
const unnamed = (_: never): null => null;

function ggufVision(m: GgufVision): Pairs | null {
  const files: Pairs = [["model", located(m.model)], ["mmproj", located(m.mmproj)]];
  switch (m.source) {
    case "huggingface":
      return [["repo", repo(m)], ...files, ["rev", m.revision], ["model_sha256", m.model_sha256],
              ["mmproj_sha256", m.mmproj_sha256]];
    case "url":
    case "vault_url":
      return [...files, ["model_sha256", m.model_sha256], ["mmproj_sha256", m.mmproj_sha256]];
    case "relative_files":
    case "absolute_files":
      return files;
    default:
      return unnamed(m);
  }
}

/** A GGUF vision model with its separate drafter, where it has one. */
function ggufVisionSpeculative(m: GgufVisionSpeculative): Pairs | null {
  const files: Pairs = [["model", located(m.model)], ["mmproj", located(m.mmproj)], ["draft", located(m.draft)]];
  switch (m.source) {
    case "huggingface":
      return [["repo", repo(m)], ...files, ["rev", m.revision], ["model_sha256", m.model_sha256],
              ["mmproj_sha256", m.mmproj_sha256], ["draft_sha256", m.draft_sha256]];
    case "url":
    case "vault_url":
      return [...files, ["model_sha256", m.model_sha256], ["mmproj_sha256", m.mmproj_sha256],
              ["draft_sha256", m.draft_sha256]];
    case "relative_files":
    case "absolute_files":
      return files;
    default:
      return unnamed(m);
  }
}

/** A directory model's source under `prefix`-named keys (`repo`, or `draft_repo` for a drafter). */
function directory(s: ModelSource, prefix = ""): Pairs | null {
  switch (s.source) {
    case "huggingface":
      return [[`${prefix}repo`, repo(s)], [`${prefix}prefix`, s.prefix], [`${prefix}rev`, s.revision]];
    case "vault_archive":
      return [[`${prefix}url`, located(s.url)]];
    case "relative_dir":
    case "absolute_dir":
      return [[`${prefix}dir`, s.dir]];
    default:
      return unnamed(s);
  }
}

function modelPairs(m: Model): Pairs | null {
  switch (m.type) {
    case "gguf_text":
      switch (m.source) {
        case "huggingface":
          return [["repo", repo(m)], ["path", m.path], ["rev", m.revision], ["sha256", m.sha256]];
        case "url":
        case "vault_url":
          return [["url", located(m.url)], ["sha256", m.sha256]];
        case "relative_file":
        case "absolute_file":
          return [["path", m.path]];
        default:
          return unnamed(m);
      }
    case "gguf_vision":
      return ggufVision(m);
    case "gguf_vision_speculative":
      return ggufVisionSpeculative(m);
    case "mlx":
    case "torch":
    case "openvino":
    case "onnx_genai":
      return directory(m);
    case "mlx_speculative":
    case "torch_speculative": {
      const [target, draft] = [directory(m.target), directory(m.draft, "draft_")];
      return target && draft && [...target, ...draft];
    }
    case "apple_foundation_text":
      return [];
    default:
      return unnamed(m);
  }
}

/** A release's version, and its repository only off `upstream`. */
const release = (r: SourceRepository, upstream: string): Pairs =>
  [["repo", r.repository_url === upstream ? null : r.repository_url], ["version", r.repository_version]];

/** A llama.cpp, OVMS or TensorRT Edge-LLM build: its release, archive or directory. */
function build(r: LlamacppCliStockTools | Ovms | TrtEdgellm, upstream: string): Pairs | null {
  switch (r.source) {
    case "github_release":
      return release(r, upstream);
    case "remote_archive":
    case "vault_remote_archive":
      return [["url", located(r.url)]];
    case "relative_dir":
    case "absolute_dir":
      return [["dir", r.dir]];
    default:
      return unnamed(r);
  }
}

/** Each package's pinned version, by the package's name. */
const pinned = <K extends string>(packages: Record<K, SourceRepository>, names: K[]): Pairs =>
  names.map((name) => [name, packages[name].repository_version]);

/** The private-API reads an iOS build compiled in, as written: `private_memory` only when it differs. */
const privateApis = (r: { private_thermal?: boolean; private_memory?: boolean | null }): Pairs => [
  ["private_thermal", r.private_thermal ? "true" : null],
  ["private_memory", r.private_memory == null ? null : String(r.private_memory)],
];

function runtimePairs(r: Runtime): Pairs | null {
  switch (r.type) {
    case "llamacpp_cli_stock_tools": {
      const pairs = build(r, LLAMACPP_UPSTREAM);
      return pairs && [...pairs, ["asset", r.source === "github_release" ? r.asset : null], ["flavor", r.flavor]];
    }
    case "ovms": {
      const pairs = build(r, OVMS_UPSTREAM);
      return pairs && [...pairs, ["flavor", r.flavor]];
    }
    case "trt_edgellm": {
      // No upstream release: an archive or a directory alone.
      const pairs = build(r, "");
      return pairs && [...pairs, ["flavor", r.flavor]];
    }
    case "llamacpp_apk_pipette":
      return [...release(r, LLAMACPP_UPSTREAM), ["flavor", r.flavor]];
    case "llamacpp_ios_pipette":
      return [...release(r, LLAMACPP_UPSTREAM), ["flavor", r.flavor], ...privateApis(r)];
    case "mlx_macos_pipette":
      return [["version", r.version], ["flavor", r.flavor]];
    case "mlx_ios_pipette":
      return [...pinned(r.packages, ["mlx_swift", "mlx_swift_lm", "swift_transformers"]), ["flavor", r.flavor],
              ...privateApis(r)];
    case "onnx_genai_ios_pipette":
      return [...pinned(r.packages, ["onnxruntime", "onnxruntime_genai"]), ["flavor", r.flavor], ...privateApis(r)];
    case "docker_vllm":
    case "docker_sglang":
      return [["image", r.image_name], ["tag", r.image_tag], ["flavor", r.flavor]];
    case "uv_vllm":
    case "uv_sglang":
      return [["server", r.server_version], ["build", r.build], ["python", r.python_version]];
    case "uv_openvino":
      return [["version", r.server_version]];
    case "uv_onnx_genai":
      return [["version", r.server_version], ["ep", r.execution_provider]];
    case "apple_foundation":
      return privateApis(r);
    default:
      return unnamed(r);
  }
}

function uri(type: string, pairs: Pairs | null): string {
  const body = (pairs ?? []).flatMap(([key, v]) => (v ? [`${key}=${v}`] : []));
  return `${type.replaceAll("_", "-")}://${body.join("&")}`;
}

/** A model as `pipette-ctl --model` spells it. */
export const modelUri = (m: Model): string => uri(m.type, modelPairs(m));

/** A runtime as `pipette-ctl --runtime` spells it. */
export const runtimeUri = (r: Runtime): string => uri(r.type, runtimePairs(r));
