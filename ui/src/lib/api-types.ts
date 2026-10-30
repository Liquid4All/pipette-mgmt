// Generated from api-schema.json (pipette_httpapi::api_schema) by `npm run types`. Do not edit.

/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "Model".
 */
export type Model =
  | (GgufText & {
      type: "gguf_text";
    })
  | (GgufVision & {
      type: "gguf_vision";
    })
  | (GgufVisionSpeculative & {
      type: "gguf_vision_speculative";
    })
  | (Mlx & {
      type: "mlx";
    })
  | (SpeculativePair & {
      type: "mlx_speculative";
    })
  | (Torch & {
      type: "torch";
    })
  | (SpeculativePair & {
      type: "torch_speculative";
    })
  | (Openvino & {
      type: "openvino";
    })
  | (OnnxGenai & {
      type: "onnx_genai";
    })
  | {
      type: "apple_foundation_text";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "GgufText".
 */
export type GgufText =
  | {
      org: string;
      path: string;
      repo_name: string;
      revision?: string | null;
      sha256?: string | null;
      source: "huggingface";
    }
  | {
      path: string;
      source: "relative_file";
    }
  | {
      path: string;
      source: "absolute_file";
    }
  | {
      sha256?: string | null;
      source: "url";
      url: string;
    }
  | {
      custom_label?: string | null;
      principal_id: VaultPrincipalId;
      sha256?: string | null;
      source: "vault_url";
      url: VaultPath;
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultPrincipalId".
 */
export type VaultPrincipalId = string;
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultPath".
 */
export type VaultPath = string;
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "GgufVision".
 */
export type GgufVision =
  | (GgufVisionHfFiles & {
      source: "huggingface";
    })
  | (GgufVisionRelativeFiles & {
      source: "relative_files";
    })
  | (GgufVisionAbsoluteFiles & {
      source: "absolute_files";
    })
  | (GgufVisionUrlFiles & {
      source: "url";
    })
  | (GgufVisionVaultUrlFiles & {
      source: "vault_url";
    });
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "GgufVisionSpeculative".
 */
export type GgufVisionSpeculative =
  | {
      draft?: string | null;
      draft_sha256?: string | null;
      mmproj: string;
      mmproj_sha256?: string | null;
      model: string;
      model_sha256?: string | null;
      org: string;
      repo_name: string;
      revision?: string | null;
      source: "huggingface";
    }
  | {
      draft?: string | null;
      mmproj: string;
      model: string;
      source: "relative_files";
    }
  | {
      draft?: string | null;
      mmproj: string;
      model: string;
      source: "absolute_files";
    }
  | {
      draft?: string | null;
      draft_sha256?: string | null;
      mmproj: string;
      mmproj_sha256?: string | null;
      model: string;
      model_sha256?: string | null;
      source: "url";
    }
  | {
      custom_label?: string | null;
      draft?: VaultPath | null;
      draft_sha256?: string | null;
      mmproj: VaultPath;
      mmproj_sha256?: string | null;
      model: VaultPath;
      model_sha256?: string | null;
      principal_id: VaultPrincipalId;
      source: "vault_url";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "Mlx".
 */
export type Mlx =
  | {
      org: string;
      prefix?: string | null;
      repo_name: string;
      revision?: string | null;
      source: "huggingface";
    }
  | {
      custom_label?: string | null;
      principal_id: VaultPrincipalId;
      sha256?: string | null;
      source: "vault_archive";
      url: VaultPath;
    }
  | {
      dir: string;
      source: "relative_dir";
    }
  | {
      dir: string;
      source: "absolute_dir";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ModelSource".
 */
export type ModelSource =
  | {
      org: string;
      prefix?: string | null;
      repo_name: string;
      revision?: string | null;
      source: "huggingface";
    }
  | {
      custom_label?: string | null;
      principal_id: VaultPrincipalId;
      sha256?: string | null;
      source: "vault_archive";
      url: VaultPath;
    }
  | {
      dir: string;
      source: "relative_dir";
    }
  | {
      dir: string;
      source: "absolute_dir";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "Torch".
 */
export type Torch =
  | {
      org: string;
      prefix?: string | null;
      repo_name: string;
      revision?: string | null;
      source: "huggingface";
    }
  | {
      custom_label?: string | null;
      principal_id: VaultPrincipalId;
      sha256?: string | null;
      source: "vault_archive";
      url: VaultPath;
    }
  | {
      dir: string;
      source: "relative_dir";
    }
  | {
      dir: string;
      source: "absolute_dir";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "Openvino".
 */
export type Openvino =
  | {
      org: string;
      prefix?: string | null;
      repo_name: string;
      revision?: string | null;
      source: "huggingface";
    }
  | {
      custom_label?: string | null;
      principal_id: VaultPrincipalId;
      sha256?: string | null;
      source: "vault_archive";
      url: VaultPath;
    }
  | {
      dir: string;
      source: "relative_dir";
    }
  | {
      dir: string;
      source: "absolute_dir";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OnnxGenai".
 */
export type OnnxGenai =
  | {
      org: string;
      prefix?: string | null;
      repo_name: string;
      revision?: string | null;
      source: "huggingface";
    }
  | {
      custom_label?: string | null;
      principal_id: VaultPrincipalId;
      sha256?: string | null;
      source: "vault_archive";
      url: VaultPath;
    }
  | {
      dir: string;
      source: "relative_dir";
    }
  | {
      dir: string;
      source: "absolute_dir";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "Runtime".
 */
export type Runtime =
  | (LlamacppCliStockTools & {
      type: "llamacpp_cli_stock_tools";
    })
  | (LlamacppApkPipette & {
      type: "llamacpp_apk_pipette";
    })
  | (LlamacppIosPipette & {
      type: "llamacpp_ios_pipette";
    })
  | (MlxMacosPipette & {
      type: "mlx_macos_pipette";
    })
  | (MlxIosPipette & {
      type: "mlx_ios_pipette";
    })
  | (DockerVllm & {
      type: "docker_vllm";
    })
  | (DockerSglang & {
      type: "docker_sglang";
    })
  | (UvVllm & {
      type: "uv_vllm";
    })
  | (UvSglang & {
      type: "uv_sglang";
    })
  | {
      python_version: string;
      server_version: string;
      source: UvRuntimeSource;
      type: "uv_openvino";
    }
  | {
      execution_provider: string;
      python_version: string;
      server_version: string;
      source: UvRuntimeSource;
      type: "uv_onnx_genai";
    }
  | (Ovms & {
      type: "ovms";
    })
  | (OnnxGenaiIosPipette & {
      type: "onnx_genai_ios_pipette";
    })
  | (AppleFoundation & {
      type: "apple_foundation";
    })
  | (TrtEdgellm & {
      type: "trt_edgellm";
    });
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "LlamacppCliStockTools".
 */
export type LlamacppCliStockTools = {
  flavor: string;
} & LlamacppCliStockTools1;
export type LlamacppCliStockTools1 =
  | (LlamacppCliStockToolsRelease & {
      source: "github_release";
    })
  | {
      source: "remote_archive";
      url: string;
    }
  | {
      custom_label?: string | null;
      principal_id: VaultPrincipalId;
      source: "vault_remote_archive";
      url: VaultPath;
    }
  | {
      dir: string;
      source: "relative_dir";
    }
  | {
      dir: string;
      source: "absolute_dir";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "LlamacppApkPipetteFlavor".
 */
export type LlamacppApkPipetteFlavor = "android-arm64-v8";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "LlamacppIosPipetteFlavor".
 */
export type LlamacppIosPipetteFlavor = "ios-arm64";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "MlxMacosPipetteFlavor".
 */
export type MlxMacosPipetteFlavor = "macos-arm64";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UvRuntimeSource".
 */
export type UvRuntimeSource =
  | {
      contents: string;
      install_flags?: string[] | null;
      type: "pip_requirements_text";
    }
  | {
      contents: string;
      custom_label?: string | null;
      install_flags?: string[] | null;
      principal_id: VaultPrincipalId;
      type: "vault_wheelhouse";
      url: VaultPath;
    }
  | {
      dir: string;
      type: "relative_preinstalled";
    }
  | {
      dir: string;
      type: "absolute_preinstalled";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "MlxIosPipetteFlavor".
 */
export type MlxIosPipetteFlavor = "ios-arm64";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VllmFlavor".
 */
export type VllmFlavor = "nvidia_gpu" | "amd_gpu" | "cpu";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "SglangFlavor".
 */
export type SglangFlavor = "nvidia_gpu" | "amd_gpu" | "cpu";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "Ovms".
 */
export type Ovms = {
  flavor: string;
} & Ovms1;
export type Ovms1 =
  | {
      repository_url: string;
      repository_version: string;
      source: "github_release";
    }
  | {
      source: "remote_archive";
      url: string;
    }
  | {
      custom_label?: string | null;
      principal_id: VaultPrincipalId;
      source: "vault_remote_archive";
      url: VaultPath;
    }
  | {
      dir: string;
      source: "relative_dir";
    }
  | {
      dir: string;
      source: "absolute_dir";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OnnxGenaiIosPipetteFlavor".
 */
export type OnnxGenaiIosPipetteFlavor = "ios-arm64";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "TrtEdgellm".
 */
export type TrtEdgellm = {
  flavor: string;
} & TrtEdgellm1;
export type TrtEdgellm1 =
  | {
      source: "remote_archive";
      url: string;
    }
  | {
      custom_label?: string | null;
      principal_id: VaultPrincipalId;
      source: "vault_remote_archive";
      url: VaultPath;
    }
  | {
      dir: string;
      source: "relative_dir";
    }
  | {
      dir: string;
      source: "absolute_dir";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ClientPermission".
 */
export type ClientPermission = "submit_results" | "perform_work";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserPermission".
 */
export type UserPermission =
  | "submit_plans"
  | "submit_any_plans"
  | "read_plans"
  | "read_any_plans"
  | "cancel_plans"
  | "cancel_any_plans"
  | "read_clients"
  | "read_any_clients"
  | "read_orgs"
  | "read_any_orgs"
  | "manage_orgs"
  | "manage_any_orgs"
  | "manage_clients"
  | "manage_any_clients"
  | "edit_catalog"
  | "edit_any_catalog";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "Answered".
 */
export type Answered = "failed" | "all";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "AnyValueIngestEvent".
 */
export type AnyValueIngestEvent =
  | {
      bytes_done: number;
      bytes_total: number;
      event: "progress";
      file: string;
      hf_ref: string;
    }
  | {
      event: "keepalive";
    }
  | {
      body: unknown;
      event: "result";
      status: number;
    }
  | {
      error: string;
      event: "error";
      status: number;
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "AuditAction".
 */
export type AuditAction =
  | {
      method: string;
      path: string;
      route: string;
      status: number;
      type: "http";
    }
  | {
      args: string[];
      command: string;
      type: "cli";
    }
  | {
      prior_keys: UserKey[];
      type: "first_clerk_sign_in";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "AuditActor".
 */
export type AuditActor =
  | {
      key_id: string;
      type: "user";
      user_id: string;
    }
  | {
      client_id: string;
      type: "client";
    }
  | {
      claimed_id: string | null;
      type: "unverified";
    }
  | {
      hostname: string;
      os_user: string;
      type: "host";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "AuditOutcome".
 */
export type AuditOutcome = ("succeeded" | "failed") | "refused";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "AuditTarget".
 */
export type AuditTarget =
  | {
      org_id: string;
      type: "org";
    }
  | {
      type: "user";
      user_id: string;
    }
  | {
      client_id: string;
      type: "client";
    }
  | {
      campaign_id: string;
      type: "campaign";
    }
  | {
      plan_id: string;
      type: "plan";
    }
  | {
      job_id: string;
      type: "job";
    }
  | {
      resource_id: string;
      type: "vault_resource";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "BenchmarkFlags".
 */
export type BenchmarkFlags =
  | {
      benchmark_type: "prefill_throughput";
      model_type: "gguf_text";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_cli_stock_tools";
    }
  | {
      benchmark_type: "decode_throughput";
      model_type: "gguf_text";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_cli_stock_tools";
    }
  | {
      benchmark_type: "end_to_end_latency";
      http_timeout_seconds?: number | null;
      model_type: "gguf_text";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_cli_stock_tools";
    }
  | {
      benchmark_type: "eval";
      http_timeout_seconds?: number | null;
      model_type: "gguf_text";
      runtime_type: "llamacpp_cli_stock_tools";
    }
  | {
      benchmark_type: "vl_throughput";
      http_timeout_seconds?: number | null;
      model_type: "gguf_vision";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_cli_stock_tools";
    }
  | {
      benchmark_type: "asr_throughput";
      http_timeout_seconds?: number | null;
      model_type: "gguf_vision";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_cli_stock_tools";
    }
  | {
      bench_timeout_seconds?: number | null;
      benchmark_type: "vl_max_memory";
      http_timeout_seconds?: number | null;
      model_type: "gguf_vision";
      runtime_type: "llamacpp_cli_stock_tools";
    }
  | {
      bench_timeout_seconds?: number | null;
      benchmark_type: "asr_max_memory";
      http_timeout_seconds?: number | null;
      model_type: "gguf_vision";
      runtime_type: "llamacpp_cli_stock_tools";
    }
  | {
      benchmark_type: "vlm_eval";
      http_timeout_seconds?: number | null;
      model_type: "gguf_vision";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_cli_stock_tools";
    }
  | {
      benchmark_type: "asr_eval";
      http_timeout_seconds?: number | null;
      model_type: "gguf_vision";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_cli_stock_tools";
    }
  | {
      bench_timeout_seconds?: number | null;
      benchmark_type: "vl_spec_decode";
      http_timeout_seconds?: number | null;
      model_type: "gguf_vision";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_cli_stock_tools";
    }
  | {
      bench_timeout_seconds?: number | null;
      benchmark_type: "vl_spec_decode";
      http_timeout_seconds?: number | null;
      model_type: "gguf_vision_speculative";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_cli_stock_tools";
    }
  | {
      bench_timeout_seconds?: number | null;
      benchmark_type: "max_memory_usage";
      model_type: "gguf_text";
      runtime_type: "llamacpp_cli_stock_tools";
    }
  | {
      benchmark_type: "prefill_throughput";
      model_type: "mlx";
      readiness?: ReadinessOverrides | null;
      runtime_type: "mlx_macos_pipette";
    }
  | {
      benchmark_type: "decode_throughput";
      model_type: "mlx";
      readiness?: ReadinessOverrides | null;
      runtime_type: "mlx_macos_pipette";
    }
  | {
      benchmark_type: "end_to_end_latency";
      model_type: "mlx";
      readiness?: ReadinessOverrides | null;
      runtime_type: "mlx_macos_pipette";
    }
  | {
      benchmark_type: "eval";
      http_timeout_seconds?: number | null;
      model_type: "mlx";
      runtime_type: "mlx_macos_pipette";
    }
  | {
      benchmark_type: "vl_throughput";
      http_timeout_seconds?: number | null;
      model_type: "mlx";
      readiness?: ReadinessOverrides | null;
      runtime_type: "mlx_macos_pipette";
    }
  | {
      bench_timeout_seconds?: number | null;
      benchmark_type: "vl_spec_decode";
      http_timeout_seconds?: number | null;
      model_type: "mlx";
      readiness?: ReadinessOverrides | null;
      runtime_type: "mlx_macos_pipette";
    }
  | {
      bench_timeout_seconds?: number | null;
      benchmark_type: "vl_spec_decode";
      http_timeout_seconds?: number | null;
      model_type: "mlx_speculative";
      readiness?: ReadinessOverrides | null;
      runtime_type: "mlx_macos_pipette";
    }
  | {
      benchmark_type: "prefill_throughput";
      model_type: "gguf_text";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_ios_pipette";
    }
  | {
      benchmark_type: "vl_spec_decode";
      model_type: "gguf_vision";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_ios_pipette";
    }
  | {
      benchmark_type: "vl_spec_decode";
      model_type: "gguf_vision_speculative";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_ios_pipette";
    }
  | {
      benchmark_type: "decode_throughput";
      model_type: "gguf_text";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_ios_pipette";
    }
  | {
      benchmark_type: "end_to_end_latency";
      model_type: "gguf_text";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_ios_pipette";
    }
  | {
      benchmark_type: "vl_throughput";
      model_type: "gguf_vision";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_ios_pipette";
    }
  | {
      benchmark_type: "asr_throughput";
      model_type: "gguf_vision";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_ios_pipette";
    }
  | {
      benchmark_type: "prefill_throughput";
      model_type: "mlx";
      readiness?: ReadinessOverrides | null;
      runtime_type: "mlx_ios_pipette";
    }
  | {
      benchmark_type: "decode_throughput";
      model_type: "mlx";
      readiness?: ReadinessOverrides | null;
      runtime_type: "mlx_ios_pipette";
    }
  | {
      benchmark_type: "end_to_end_latency";
      model_type: "mlx";
      readiness?: ReadinessOverrides | null;
      runtime_type: "mlx_ios_pipette";
    }
  | {
      benchmark_type: "prefill_throughput";
      model_type: "gguf_text";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_apk_pipette";
    }
  | {
      benchmark_type: "decode_throughput";
      model_type: "gguf_text";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_apk_pipette";
    }
  | {
      benchmark_type: "end_to_end_latency";
      model_type: "gguf_text";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_apk_pipette";
    }
  | {
      benchmark_type: "vl_throughput";
      model_type: "gguf_vision";
      readiness?: ReadinessOverrides | null;
      runtime_type: "llamacpp_apk_pipette";
    }
  | {
      benchmark_type: "prefill_throughput";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      readiness?: ReadinessOverrides | null;
      runtime_type: "docker_vllm";
    }
  | {
      benchmark_type: "decode_throughput";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      readiness?: ReadinessOverrides | null;
      runtime_type: "docker_vllm";
    }
  | {
      benchmark_type: "prefill_throughput";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      readiness?: ReadinessOverrides | null;
      runtime_type: "uv_vllm";
    }
  | {
      benchmark_type: "decode_throughput";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      readiness?: ReadinessOverrides | null;
      runtime_type: "uv_vllm";
    }
  | {
      benchmark_type: "prefill_throughput";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      readiness?: ReadinessOverrides | null;
      runtime_type: "docker_sglang";
    }
  | {
      benchmark_type: "decode_throughput";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      readiness?: ReadinessOverrides | null;
      runtime_type: "docker_sglang";
    }
  | {
      benchmark_type: "prefill_throughput";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      readiness?: ReadinessOverrides | null;
      runtime_type: "uv_sglang";
    }
  | {
      benchmark_type: "decode_throughput";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      readiness?: ReadinessOverrides | null;
      runtime_type: "uv_sglang";
    }
  | {
      benchmark_type: "end_to_end_latency";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      readiness?: ReadinessOverrides | null;
      runtime_type: "docker_vllm";
    }
  | {
      benchmark_type: "end_to_end_latency";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      readiness?: ReadinessOverrides | null;
      runtime_type: "uv_vllm";
    }
  | {
      benchmark_type: "end_to_end_latency";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      readiness?: ReadinessOverrides | null;
      runtime_type: "docker_sglang";
    }
  | {
      benchmark_type: "end_to_end_latency";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      readiness?: ReadinessOverrides | null;
      runtime_type: "uv_sglang";
    }
  | {
      benchmark_type: "eval";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      runtime_type: "docker_vllm";
    }
  | {
      benchmark_type: "eval";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      runtime_type: "uv_vllm";
    }
  | {
      benchmark_type: "vlm_eval";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      runtime_type: "uv_vllm";
    }
  | {
      benchmark_type: "asr_eval";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      runtime_type: "uv_vllm";
    }
  | {
      benchmark_type: "eval";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      runtime_type: "docker_sglang";
    }
  | {
      benchmark_type: "eval";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      runtime_type: "uv_sglang";
    }
  | {
      bench_timeout_seconds?: number | null;
      benchmark_type: "vl_spec_decode";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      readiness?: ReadinessOverrides | null;
      runtime_type: "docker_sglang";
    }
  | {
      bench_timeout_seconds?: number | null;
      benchmark_type: "vl_spec_decode";
      http_timeout_seconds?: number | null;
      model_type: "torch_speculative";
      readiness?: ReadinessOverrides | null;
      runtime_type: "docker_sglang";
    }
  | {
      bench_timeout_seconds?: number | null;
      benchmark_type: "vl_spec_decode";
      http_timeout_seconds?: number | null;
      model_type: "torch";
      readiness?: ReadinessOverrides | null;
      runtime_type: "uv_sglang";
    }
  | {
      bench_timeout_seconds?: number | null;
      benchmark_type: "vl_spec_decode";
      http_timeout_seconds?: number | null;
      model_type: "torch_speculative";
      readiness?: ReadinessOverrides | null;
      runtime_type: "uv_sglang";
    }
  | {
      benchmark_type: "prefill_throughput";
      model_type: "openvino";
      readiness?: ReadinessOverrides | null;
      runtime_type: "uv_openvino";
    }
  | {
      benchmark_type: "decode_throughput";
      model_type: "openvino";
      readiness?: ReadinessOverrides | null;
      runtime_type: "uv_openvino";
    }
  | {
      benchmark_type: "end_to_end_latency";
      model_type: "openvino";
      readiness?: ReadinessOverrides | null;
      runtime_type: "uv_openvino";
    }
  | {
      benchmark_type: "prefill_throughput";
      http_timeout_seconds?: number | null;
      model_type: "openvino";
      readiness?: ReadinessOverrides | null;
      runtime_type: "ovms";
    }
  | {
      benchmark_type: "decode_throughput";
      http_timeout_seconds?: number | null;
      model_type: "openvino";
      readiness?: ReadinessOverrides | null;
      runtime_type: "ovms";
    }
  | {
      benchmark_type: "end_to_end_latency";
      http_timeout_seconds?: number | null;
      model_type: "openvino";
      readiness?: ReadinessOverrides | null;
      runtime_type: "ovms";
    }
  | {
      benchmark_type: "prefill_throughput";
      model_type: "onnx_genai";
      readiness?: ReadinessOverrides | null;
      runtime_type: "uv_onnx_genai";
    }
  | {
      benchmark_type: "decode_throughput";
      model_type: "onnx_genai";
      readiness?: ReadinessOverrides | null;
      runtime_type: "uv_onnx_genai";
    }
  | {
      benchmark_type: "end_to_end_latency";
      model_type: "onnx_genai";
      readiness?: ReadinessOverrides | null;
      runtime_type: "uv_onnx_genai";
    }
  | {
      benchmark_type: "prefill_throughput";
      model_type: "onnx_genai";
      readiness?: ReadinessOverrides | null;
      runtime_type: "onnx_genai_ios_pipette";
    }
  | {
      benchmark_type: "decode_throughput";
      model_type: "onnx_genai";
      readiness?: ReadinessOverrides | null;
      runtime_type: "onnx_genai_ios_pipette";
    }
  | {
      benchmark_type: "end_to_end_latency";
      model_type: "onnx_genai";
      readiness?: ReadinessOverrides | null;
      runtime_type: "onnx_genai_ios_pipette";
    }
  | {
      benchmark_type: "vl_throughput";
      http_timeout_seconds?: number | null;
      model_type: "gguf_vision";
      readiness?: ReadinessOverrides | null;
      runtime_type: "trt_edgellm";
    }
  | {
      bench_timeout_seconds?: number | null;
      benchmark_type: "vl_max_memory";
      http_timeout_seconds?: number | null;
      model_type: "gguf_vision";
      runtime_type: "trt_edgellm";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "BenchmarkType".
 */
export type BenchmarkType =
  | (
      | "prefill_throughput"
      | "decode_throughput"
      | "end_to_end_latency"
      | "max_memory_usage"
      | "eval"
      | "vl_throughput"
      | "vl_max_memory"
    )
  | "vlm_eval"
  | "asr_eval"
  | "vl_spec_decode"
  | "asr_throughput"
  | "asr_max_memory";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "PrincipalOwner".
 */
export type PrincipalOwner =
  | {
      org_id: string;
      type: "org";
    }
  | {
      type: "user";
      user_id: string;
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "RevisionChange".
 */
export type RevisionChange =
  | {
      added_variant_count: number;
      carry_earlier?: boolean;
      inherit_from?: string[];
      removed_variant_count: number;
      restored?: RestoredVariant[];
      type: "variants";
    }
  | {
      carry_earlier?: boolean;
      inherit_from?: string[];
      type: "document";
    }
  | {
      answered?: Answered | null;
      job_count: number;
      type: "rerun";
    }
  | {
      all_evals?: boolean;
      job_count: number;
      type: "recalculate";
    }
  | {
      to_plan_id: string;
      type: "revert";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "FleetDetail".
 */
export type FleetDetail = "full" | "withheld";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "WarningKind".
 */
export type WarningKind = "fleet" | "uncatalogued_model" | string;
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CampaignState".
 */
export type CampaignState = "active" | "archived";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "PlanStatus".
 */
export type PlanStatus = "creating" | "active" | "pending_clients" | "cancelling" | "complete" | "cancelled";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CancelOutcome".
 */
export type CancelOutcome = "requested" | "already_cancelling" | "already_cancelled";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CatalogImportCommand".
 */
export type CatalogImportCommand = "import" | "refresh";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CatalogImportOutcome".
 */
export type CatalogImportOutcome = "running" | "success" | "unclassified" | "failed";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CatalogImportResult".
 */
export type CatalogImportResult = "running" | "success" | "error";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorEntryAction".
 */
export type DescriptorEntryAction = "pin" | "unpin" | "hide" | "unhide" | "tombstone";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserAccountType".
 */
export type UserAccountType = "human" | "service";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserStatus".
 */
export type UserStatus = ("active" | "disabled") | "pending";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OrgStatus".
 */
export type OrgStatus = "active" | "disabled";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserPermissionSet".
 */
export type UserPermissionSet = string[];
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ClientStatus".
 */
export type ClientStatus = "pending" | "approved";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ClientOwner".
 */
export type ClientOwner =
  | {
      org_id: string;
      type: "org";
    }
  | {
      type: "user";
      user_id: string;
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ClientPermissionSet".
 */
export type ClientPermissionSet = string[];
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ClientPreauthUsage".
 */
export type ClientPreauthUsage = "single_use" | "multi_use";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorKind".
 */
export type DescriptorKind = "runtime" | "model";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorMemberCoverage".
 */
export type DescriptorMemberCoverage = "covered" | "no_rule" | "unsupported";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorMemberDescriptor".
 */
export type DescriptorMemberDescriptor = Runtime | Model;
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorMemberKey".
 */
export type DescriptorMemberKey =
  | {
      flavor: string | null;
      kind: "runtime";
    }
  | {
      format: ModelFormat;
      kind: "model";
      quantization: string;
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ModelFormat".
 */
export type ModelFormat = "gguf" | "mlx" | "torch" | "openvino" | "onnx";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorWithheldReasonCode".
 */
export type DescriptorWithheldReasonCode = "no_rule" | "unsupported" | "not_met" | "no_runtime";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorEntryKind".
 */
export type DescriptorEntryKind = "stable" | "daily" | "hand-made";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorEntryState".
 */
export type DescriptorEntryState = "pinned" | "active" | "hidden" | "tombstoned";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorMember".
 */
export type DescriptorMember =
  | {
      added_at: string;
      digest: string;
      flavor: string | null;
      runtime: Runtime;
    }
  | {
      added_at: string;
      digest: string;
      format: ModelFormat;
      model: Model;
      quantization: string;
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DockerIpcMode".
 */
export type DockerIpcMode = "host" | "private" | "shareable" | "none";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DocumentState".
 */
export type DocumentState = ("present" | "collected") | "none" | "mismatch";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "FailureSource".
 */
export type FailureSource = "client" | "expired" | "all_clients_denied" | "unmatchable" | "lease_lost";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "IssueSeverity".
 */
export type IssueSeverity = "error" | "warning";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "JobResult".
 */
export type JobResult =
  | {
      earlier_runs: ParentRun[];
      recalculable: boolean;
      rescored_from: ParentJob | null;
      rows: ResultRow[];
      status: "scored";
    }
  | {
      status: "not_run";
    }
  | {
      status: "unknown";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UnknownTag".
 */
export type UnknownTag = "unknown";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ModelFlags".
 */
export type ModelFlags =
  | {
      benchmark_type: "eval";
      enable_thinking?: boolean | null;
      model_type: "gguf_text";
      reasoning_effort?: ReasoningEffort | null;
    }
  | {
      benchmark_type: "eval";
      enable_thinking?: boolean | null;
      model_type: "gguf_vision";
    }
  | {
      benchmark_type: "eval";
      enable_thinking?: boolean | null;
      model_type: "mlx";
    }
  | {
      benchmark_type: "eval";
      enable_thinking?: boolean | null;
      model_type: "torch";
      reasoning_effort?: ReasoningEffort | null;
    }
  | {
      benchmark_type: "vlm_eval";
      enable_thinking?: boolean | null;
      model_type: "gguf_vision";
    }
  | {
      benchmark_type: "vlm_eval";
      enable_thinking?: boolean | null;
      model_type: "torch";
    }
  | {
      benchmark_type: "asr_eval";
      enable_thinking?: boolean | null;
      model_type: "gguf_vision";
    }
  | {
      benchmark_type: "asr_eval";
      enable_thinking?: boolean | null;
      model_type: "torch";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ReasoningEffort".
 */
export type ReasoningEffort = "low" | "medium" | "high";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "RuntimeFlags".
 */
export type RuntimeFlags =
  | {
      benchmark_type: "prefill_throughput";
      envs?: string[];
      flash_attention?: LlamacppFlashAttention | null;
      load_mode?: LlamacppLoadMode | null;
      mmap?: boolean | null;
      model_type: "gguf_text";
      number_gpu_layers?: number | null;
      raw?: string[];
      runtime_type: "llamacpp_cli_stock_tools";
      threads?: number | null;
    }
  | {
      benchmark_type: "decode_throughput";
      envs?: string[];
      flash_attention?: LlamacppFlashAttention | null;
      load_mode?: LlamacppLoadMode | null;
      mmap?: boolean | null;
      model_type: "gguf_text";
      number_gpu_layers?: number | null;
      raw?: string[];
      runtime_type: "llamacpp_cli_stock_tools";
      threads?: number | null;
    }
  | {
      benchmark_type: "max_memory_usage";
      envs?: string[];
      flash_attention?: LlamacppFlashAttention | null;
      load_mode?: LlamacppLoadMode | null;
      mmap?: boolean | null;
      model_type: "gguf_text";
      number_gpu_layers?: number | null;
      raw?: string[];
      runtime_type: "llamacpp_cli_stock_tools";
      threads?: number | null;
    }
  | {
      benchmark_type: "end_to_end_latency";
      ctx_size?: number | null;
      envs?: string[];
      flash_attention?: LlamacppFlashAttention | null;
      load_mode?: LlamacppLoadMode | null;
      mmap?: boolean | null;
      model_type: "gguf_text";
      no_cache?: boolean | null;
      number_gpu_layers?: number | null;
      raw?: string[];
      runtime_type: "llamacpp_cli_stock_tools";
      threads?: number | null;
    }
  | {
      benchmark_type: "eval";
      ctx_size?: number | null;
      envs?: string[];
      flash_attention?: LlamacppFlashAttention | null;
      load_mode?: LlamacppLoadMode | null;
      mmap?: boolean | null;
      model_type: "gguf_text";
      no_cache?: boolean | null;
      number_gpu_layers?: number | null;
      parallel?: number | null;
      raw?: string[];
      runtime_type: "llamacpp_cli_stock_tools";
      threads?: number | null;
    }
  | {
      benchmark_type: "vl_throughput";
      ctx_size?: number | null;
      envs?: string[];
      flash_attention?: LlamacppFlashAttention | null;
      load_mode?: LlamacppLoadMode | null;
      mmap?: boolean | null;
      model_type: "gguf_vision";
      no_cache?: boolean | null;
      number_gpu_layers?: number | null;
      raw?: string[];
      runtime_type: "llamacpp_cli_stock_tools";
      threads?: number | null;
    }
  | {
      benchmark_type: "asr_throughput";
      ctx_size?: number | null;
      envs?: string[];
      flash_attention?: LlamacppFlashAttention | null;
      load_mode?: LlamacppLoadMode | null;
      mmap?: boolean | null;
      model_type: "gguf_vision";
      no_cache?: boolean | null;
      number_gpu_layers?: number | null;
      raw?: string[];
      runtime_type: "llamacpp_cli_stock_tools";
      threads?: number | null;
    }
  | {
      benchmark_type: "vlm_eval";
      ctx_size?: number | null;
      envs?: string[];
      flash_attention?: LlamacppFlashAttention | null;
      load_mode?: LlamacppLoadMode | null;
      mmap?: boolean | null;
      model_type: "gguf_vision";
      no_cache?: boolean | null;
      number_gpu_layers?: number | null;
      parallel?: number | null;
      raw?: string[];
      runtime_type: "llamacpp_cli_stock_tools";
      threads?: number | null;
    }
  | {
      benchmark_type: "asr_eval";
      ctx_size?: number | null;
      envs?: string[];
      flash_attention?: LlamacppFlashAttention | null;
      load_mode?: LlamacppLoadMode | null;
      mmap?: boolean | null;
      model_type: "gguf_vision";
      no_cache?: boolean | null;
      number_gpu_layers?: number | null;
      parallel?: number | null;
      raw?: string[];
      runtime_type: "llamacpp_cli_stock_tools";
      threads?: number | null;
    }
  | {
      benchmark_type: "vl_max_memory";
      ctx_size?: number | null;
      envs?: string[];
      flash_attention?: LlamacppFlashAttention | null;
      load_mode?: LlamacppLoadMode | null;
      mmap?: boolean | null;
      model_type: "gguf_vision";
      no_cache?: boolean | null;
      number_gpu_layers?: number | null;
      raw?: string[];
      runtime_type: "llamacpp_cli_stock_tools";
      threads?: number | null;
    }
  | {
      benchmark_type: "asr_max_memory";
      ctx_size?: number | null;
      envs?: string[];
      flash_attention?: LlamacppFlashAttention | null;
      load_mode?: LlamacppLoadMode | null;
      mmap?: boolean | null;
      model_type: "gguf_vision";
      no_cache?: boolean | null;
      number_gpu_layers?: number | null;
      raw?: string[];
      runtime_type: "llamacpp_cli_stock_tools";
      threads?: number | null;
    }
  | {
      benchmark_type: "vl_spec_decode";
      ctx_size?: number | null;
      envs?: string[];
      flash_attention?: LlamacppFlashAttention | null;
      load_mode?: LlamacppLoadMode | null;
      mmap?: boolean | null;
      model_type: "gguf_vision";
      no_cache?: boolean | null;
      number_gpu_layers?: number | null;
      raw?: string[];
      runtime_type: "llamacpp_cli_stock_tools";
      speculative_enabled?: boolean | null;
      threads?: number | null;
    }
  | {
      benchmark_type: "vl_spec_decode";
      ctx_size?: number | null;
      envs?: string[];
      flash_attention?: LlamacppFlashAttention | null;
      load_mode?: LlamacppLoadMode | null;
      mmap?: boolean | null;
      model_type: "gguf_vision_speculative";
      no_cache?: boolean | null;
      number_gpu_layers?: number | null;
      raw?: string[];
      runtime_type: "llamacpp_cli_stock_tools";
      speculative_enabled?: boolean | null;
      threads?: number | null;
    }
  | {
      benchmark_type: "prefill_throughput";
      dtype?: VllmDtype | null;
      envs?: string[];
      gpus?: string | null;
      ipc?: DockerIpcMode | null;
      max_model_len?: number | null;
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "docker_vllm";
      shm_size?: string | null;
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "decode_throughput";
      dtype?: VllmDtype | null;
      envs?: string[];
      gpus?: string | null;
      ipc?: DockerIpcMode | null;
      max_model_len?: number | null;
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "docker_vllm";
      shm_size?: string | null;
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "end_to_end_latency";
      dtype?: VllmDtype | null;
      envs?: string[];
      gpus?: string | null;
      ipc?: DockerIpcMode | null;
      max_model_len?: number | null;
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "docker_vllm";
      shm_size?: string | null;
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "eval";
      dtype?: VllmDtype | null;
      envs?: string[];
      gpus?: string | null;
      ipc?: DockerIpcMode | null;
      max_model_len?: number | null;
      model_type: "torch";
      parallel?: number | null;
      prefix_caching?: boolean | null;
      raw?: string[];
      repetition_detection?: VllmRepetitionDetection | null;
      runtime_type: "docker_vllm";
      shm_size?: string | null;
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "max_memory_usage";
      dtype?: VllmDtype | null;
      envs?: string[];
      gpus?: string | null;
      ipc?: DockerIpcMode | null;
      max_model_len?: number | null;
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "docker_vllm";
      shm_size?: string | null;
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "prefill_throughput";
      dtype?: VllmDtype | null;
      envs?: string[];
      max_model_len?: number | null;
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "uv_vllm";
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "decode_throughput";
      dtype?: VllmDtype | null;
      envs?: string[];
      max_model_len?: number | null;
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "uv_vllm";
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "end_to_end_latency";
      dtype?: VllmDtype | null;
      envs?: string[];
      max_model_len?: number | null;
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "uv_vllm";
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "eval";
      dtype?: VllmDtype | null;
      envs?: string[];
      max_model_len?: number | null;
      model_type: "torch";
      parallel?: number | null;
      prefix_caching?: boolean | null;
      raw?: string[];
      repetition_detection?: VllmRepetitionDetection | null;
      runtime_type: "uv_vllm";
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "vlm_eval";
      dtype?: VllmDtype | null;
      envs?: string[];
      max_model_len?: number | null;
      model_type: "torch";
      parallel?: number | null;
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "uv_vllm";
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "asr_eval";
      dtype?: VllmDtype | null;
      envs?: string[];
      max_model_len?: number | null;
      model_type: "torch";
      parallel?: number | null;
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "uv_vllm";
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "max_memory_usage";
      dtype?: VllmDtype | null;
      envs?: string[];
      max_model_len?: number | null;
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "uv_vllm";
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "prefill_throughput";
      envs?: string[];
      gpus?: string | null;
      ipc?: DockerIpcMode | null;
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "docker_sglang";
      shm_size?: string | null;
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "decode_throughput";
      envs?: string[];
      gpus?: string | null;
      ipc?: DockerIpcMode | null;
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "docker_sglang";
      shm_size?: string | null;
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "end_to_end_latency";
      envs?: string[];
      gpus?: string | null;
      ipc?: DockerIpcMode | null;
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "docker_sglang";
      shm_size?: string | null;
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "eval";
      envs?: string[];
      gpus?: string | null;
      ipc?: DockerIpcMode | null;
      model_type: "torch";
      parallel?: number | null;
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "docker_sglang";
      shm_size?: string | null;
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "max_memory_usage";
      envs?: string[];
      gpus?: string | null;
      ipc?: DockerIpcMode | null;
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "docker_sglang";
      shm_size?: string | null;
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "prefill_throughput";
      envs?: string[];
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "uv_sglang";
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "decode_throughput";
      envs?: string[];
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "uv_sglang";
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "end_to_end_latency";
      envs?: string[];
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "uv_sglang";
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "eval";
      envs?: string[];
      model_type: "torch";
      parallel?: number | null;
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "uv_sglang";
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "max_memory_usage";
      envs?: string[];
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "uv_sglang";
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "vl_spec_decode";
      envs?: string[];
      gpus?: string | null;
      ipc?: DockerIpcMode | null;
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "docker_sglang";
      shm_size?: string | null;
      speculative_enabled?: boolean | null;
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "vl_spec_decode";
      envs?: string[];
      gpus?: string | null;
      ipc?: DockerIpcMode | null;
      model_type: "torch_speculative";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "docker_sglang";
      shm_size?: string | null;
      speculative_enabled?: boolean | null;
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "vl_spec_decode";
      envs?: string[];
      model_type: "torch";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "uv_sglang";
      speculative_enabled?: boolean | null;
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "vl_spec_decode";
      envs?: string[];
      model_type: "torch_speculative";
      prefix_caching?: boolean | null;
      raw?: string[];
      runtime_type: "uv_sglang";
      speculative_enabled?: boolean | null;
      tensor_parallel_size?: number | null;
    }
  | {
      benchmark_type: "prefill_throughput";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_text";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_ios_pipette";
      swa_full?: boolean | null;
      threads?: number | null;
    }
  | {
      benchmark_type: "decode_throughput";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_text";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_ios_pipette";
      swa_full?: boolean | null;
      threads?: number | null;
    }
  | {
      benchmark_type: "max_memory_usage";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_text";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_ios_pipette";
      swa_full?: boolean | null;
      threads?: number | null;
    }
  | {
      benchmark_type: "end_to_end_latency";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_text";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_ios_pipette";
      swa_full?: boolean | null;
      threads?: number | null;
    }
  | {
      benchmark_type: "eval";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_text";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_ios_pipette";
      swa_full?: boolean | null;
      threads?: number | null;
    }
  | {
      benchmark_type: "vl_throughput";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_vision";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_ios_pipette";
      swa_full?: boolean | null;
      threads?: number | null;
    }
  | {
      benchmark_type: "vl_max_memory";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_vision";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_ios_pipette";
      swa_full?: boolean | null;
      threads?: number | null;
    }
  | {
      benchmark_type: "asr_throughput";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_vision";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_ios_pipette";
      swa_full?: boolean | null;
      threads?: number | null;
    }
  | {
      benchmark_type: "asr_max_memory";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_vision";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_ios_pipette";
      swa_full?: boolean | null;
      threads?: number | null;
    }
  | {
      benchmark_type: "vl_spec_decode";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_vision";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_ios_pipette";
      speculative_enabled?: boolean | null;
      swa_full?: boolean | null;
      threads?: number | null;
    }
  | {
      benchmark_type: "vl_spec_decode";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_vision_speculative";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_ios_pipette";
      speculative_enabled?: boolean | null;
      swa_full?: boolean | null;
      threads?: number | null;
    }
  | {
      benchmark_type: "vl_spec_decode";
      model_type: "mlx";
      runtime_type: "mlx_macos_pipette";
      speculative_enabled?: boolean | null;
    }
  | {
      benchmark_type: "vl_spec_decode";
      model_type: "mlx_speculative";
      runtime_type: "mlx_macos_pipette";
      speculative_enabled?: boolean | null;
    }
  | {
      benchmark_type: "prefill_throughput";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_text";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_apk_pipette";
    }
  | {
      benchmark_type: "decode_throughput";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_text";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_apk_pipette";
    }
  | {
      benchmark_type: "max_memory_usage";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_text";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_apk_pipette";
    }
  | {
      benchmark_type: "end_to_end_latency";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_text";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_apk_pipette";
    }
  | {
      benchmark_type: "eval";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_text";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_apk_pipette";
    }
  | {
      benchmark_type: "vl_throughput";
      ctx_size?: number | null;
      load_mode?: LlamacppLoadMode | null;
      model_type: "gguf_vision";
      n_ubatch?: number | null;
      number_gpu_layers?: number | null;
      runtime_type: "llamacpp_apk_pipette";
    }
  | {
      benchmark_type: "prefill_throughput";
      model_type: "mlx";
      n_ubatch?: number | null;
      runtime_type: "mlx_ios_pipette";
    }
  | {
      benchmark_type: "decode_throughput";
      model_type: "mlx";
      n_ubatch?: number | null;
      runtime_type: "mlx_ios_pipette";
    }
  | {
      benchmark_type: "max_memory_usage";
      model_type: "mlx";
      n_ubatch?: number | null;
      runtime_type: "mlx_ios_pipette";
    }
  | {
      benchmark_type: "end_to_end_latency";
      model_type: "mlx";
      n_ubatch?: number | null;
      runtime_type: "mlx_ios_pipette";
    }
  | {
      benchmark_type: "eval";
      model_type: "mlx";
      n_ubatch?: number | null;
      runtime_type: "mlx_ios_pipette";
    }
  | {
      benchmark_type: "prefill_throughput";
      device?: string | null;
      generate_hint?: OpenvinoGenerateHint | null;
      max_prompt_len?: number | null;
      min_response_len?: number | null;
      model_type: "openvino";
      runtime_type: "uv_openvino";
    }
  | {
      benchmark_type: "decode_throughput";
      device?: string | null;
      generate_hint?: OpenvinoGenerateHint | null;
      max_prompt_len?: number | null;
      min_response_len?: number | null;
      model_type: "openvino";
      runtime_type: "uv_openvino";
    }
  | {
      benchmark_type: "end_to_end_latency";
      device?: string | null;
      generate_hint?: OpenvinoGenerateHint | null;
      max_prompt_len?: number | null;
      min_response_len?: number | null;
      model_type: "openvino";
      runtime_type: "uv_openvino";
    }
  | {
      benchmark_type: "max_memory_usage";
      device?: string | null;
      generate_hint?: OpenvinoGenerateHint | null;
      max_prompt_len?: number | null;
      min_response_len?: number | null;
      model_type: "openvino";
      runtime_type: "uv_openvino";
    }
  | {
      benchmark_type: "prefill_throughput";
      device?: string | null;
      generate_hint?: OpenvinoGenerateHint | null;
      max_prompt_len?: number | null;
      min_response_len?: number | null;
      model_type: "openvino";
      prefix_caching?: boolean | null;
      runtime_type: "ovms";
    }
  | {
      benchmark_type: "decode_throughput";
      device?: string | null;
      generate_hint?: OpenvinoGenerateHint | null;
      max_prompt_len?: number | null;
      min_response_len?: number | null;
      model_type: "openvino";
      prefix_caching?: boolean | null;
      runtime_type: "ovms";
    }
  | {
      benchmark_type: "end_to_end_latency";
      device?: string | null;
      generate_hint?: OpenvinoGenerateHint | null;
      max_prompt_len?: number | null;
      min_response_len?: number | null;
      model_type: "openvino";
      prefix_caching?: boolean | null;
      runtime_type: "ovms";
    }
  | {
      benchmark_type: "max_memory_usage";
      device?: string | null;
      generate_hint?: OpenvinoGenerateHint | null;
      max_prompt_len?: number | null;
      min_response_len?: number | null;
      model_type: "openvino";
      prefix_caching?: boolean | null;
      runtime_type: "ovms";
    }
  | {
      benchmark_type: "vl_throughput";
      envs?: string[];
      model_type: "gguf_vision";
      precision?: TrtEdgellmPrecision | null;
      raw?: string[];
      runtime_type: "trt_edgellm";
    }
  | {
      benchmark_type: "vl_max_memory";
      envs?: string[];
      model_type: "gguf_vision";
      precision?: TrtEdgellmPrecision | null;
      raw?: string[];
      runtime_type: "trt_edgellm";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "LlamacppFlashAttention".
 */
export type LlamacppFlashAttention = "auto" | "on" | "off";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "LlamacppLoadMode".
 */
export type LlamacppLoadMode = "none" | "mmap" | "mlock" | "mmap_mlock" | "direct_io";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VllmDtype".
 */
export type VllmDtype = "auto" | "half" | "float16" | "bfloat16" | "float" | "float32";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OpenvinoGenerateHint".
 */
export type OpenvinoGenerateHint = "best-perf" | "fast-compile";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "TrtEdgellmPrecision".
 */
export type TrtEdgellmPrecision = "fp16" | "bf16" | "w8a16";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "JobState".
 */
export type JobState = "available" | "running" | "finished" | "failed" | "cancelled" | "unknown";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ResultsRead".
 */
export type ResultsRead = "ok" | "off" | "timed_out" | "failed";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultEntry".
 */
export type VaultEntry =
  | {
      model: Model;
    }
  | {
      runtime: Runtime;
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ObjectState".
 */
export type ObjectState =
  | {
      state: "present";
    }
  | {
      bytes: number;
      state: "resized";
    }
  | {
      state: "missing";
    }
  | {
      state: "unknown";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultOrigin".
 */
export type VaultOrigin =
  | {
      commit: string;
      org: string;
      path?: string | null;
      repo_name: string;
      source: "huggingface";
    }
  | {
      asset: string;
      commit: string;
      owner: string;
      repo: string;
      source: "github_release";
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "Reach".
 */
export type Reach = ("none" | "every") | "own" | "listed";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OwnerType".
 */
export type OwnerType = "org" | "user";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "PlannedImport".
 */
export type PlannedImport = {
  hf_ref: string;
} & PlannedImport1;
export type PlannedImport1 =
  | {
      action: "reuse";
      resource_ids: string[];
    }
  | {
      action: "fetch";
      bytes: number;
    };
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserPreauthUsage".
 */
export type UserPreauthUsage = "single_use" | "multi_use";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultFileFormat".
 */
export type VaultFileFormat = "gguf" | "archive";
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultFileRole".
 */
export type VaultFileRole =
  "model" | "mmproj" | "draft" | "dir_archive" | "target_dir_archive" | "draft_dir_archive" | "archive";

export interface ApiSchema {
  [k: string]: unknown;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "AddDescriptorMemberRequest".
 */
export interface AddDescriptorMemberRequest {
  model?: Model | null;
  quantization?: string | null;
  runtime?: Runtime | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "GgufVisionHfFiles".
 */
export interface GgufVisionHfFiles {
  mmproj: string;
  mmproj_sha256?: string | null;
  model: string;
  model_sha256?: string | null;
  org: string;
  repo_name: string;
  revision?: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "GgufVisionRelativeFiles".
 */
export interface GgufVisionRelativeFiles {
  mmproj: string;
  model: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "GgufVisionAbsoluteFiles".
 */
export interface GgufVisionAbsoluteFiles {
  mmproj: string;
  model: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "GgufVisionUrlFiles".
 */
export interface GgufVisionUrlFiles {
  mmproj: string;
  mmproj_sha256?: string | null;
  model: string;
  model_sha256?: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "GgufVisionVaultUrlFiles".
 */
export interface GgufVisionVaultUrlFiles {
  custom_label?: string | null;
  mmproj: VaultPath;
  mmproj_sha256?: string | null;
  model: VaultPath;
  model_sha256?: string | null;
  principal_id: VaultPrincipalId;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "SpeculativePair".
 */
export interface SpeculativePair {
  draft: ModelSource;
  target: ModelSource;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "LlamacppCliStockToolsRelease".
 */
export interface LlamacppCliStockToolsRelease {
  asset?: string | null;
  repository_url: string;
  repository_version: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "LlamacppApkPipette".
 */
export interface LlamacppApkPipette {
  flavor: LlamacppApkPipetteFlavor;
  repository_url: string;
  repository_version: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "LlamacppIosPipette".
 */
export interface LlamacppIosPipette {
  flavor: LlamacppIosPipetteFlavor;
  private_memory?: boolean | null;
  private_thermal?: boolean;
  repository_url: string;
  repository_version: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "MlxMacosPipette".
 */
export interface MlxMacosPipette {
  flavor: MlxMacosPipetteFlavor;
  source: UvRuntimeSource;
  version: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "MlxIosPipette".
 */
export interface MlxIosPipette {
  flavor: MlxIosPipetteFlavor;
  packages: MlxSwiftStack;
  private_memory?: boolean | null;
  private_thermal?: boolean;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "MlxSwiftStack".
 */
export interface MlxSwiftStack {
  mlx_swift: SourceRepository;
  mlx_swift_lm: SourceRepository;
  swift_transformers: SourceRepository;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "SourceRepository".
 */
export interface SourceRepository {
  repository_url: string;
  repository_version: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DockerVllm".
 */
export interface DockerVllm {
  flavor: VllmFlavor;
  image_name: string;
  image_tag: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DockerSglang".
 */
export interface DockerSglang {
  flavor: SglangFlavor;
  image_name: string;
  image_tag: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UvVllm".
 */
export interface UvVllm {
  build: string;
  python_version: string;
  server_version: string;
  source: UvRuntimeSource;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UvSglang".
 */
export interface UvSglang {
  build: string;
  python_version: string;
  server_version: string;
  source: UvRuntimeSource;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OnnxGenaiIosPipette".
 */
export interface OnnxGenaiIosPipette {
  flavor: OnnxGenaiIosPipetteFlavor;
  packages: OnnxGenaiStack;
  private_memory?: boolean | null;
  private_thermal?: boolean;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OnnxGenaiStack".
 */
export interface OnnxGenaiStack {
  onnxruntime: SourceRepository;
  onnxruntime_genai: SourceRepository;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "AppleFoundation".
 */
export interface AppleFoundation {
  private_memory?: boolean | null;
  private_thermal?: boolean;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "AddUserKeyRequest".
 */
export interface AddUserKeyRequest {
  expires_at?: string | null;
  key_details: string;
  public_key: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "AmendClientPermissionsRequest".
 */
export interface AmendClientPermissionsRequest {
  add?: ClientPermission[];
  remove?: ClientPermission[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "AmendUserPermissionsRequest".
 */
export interface AmendUserPermissionsRequest {
  add?: UserPermission[];
  remove?: UserPermission[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserKey".
 */
export interface UserKey {
  added_at: string;
  expires_at?: string | null;
  key_details: string;
  key_id: string;
  public_key: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "AuditEvent".
 */
export interface AuditEvent {
  action: AuditAction;
  actor: AuditActor;
  created_at: string;
  event_id: string;
  org_ids: string[];
  outcome: AuditOutcome;
  summary?: string | null;
  targets: AuditTarget[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "Backlog".
 */
export interface Backlog {
  counted_at: string;
  job_count: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "BacklogByCampaign".
 */
export interface BacklogByCampaign {
  campaigns: BacklogCampaign[];
  counted_at: string;
  outside_campaign_job_count: number;
  unreadable_job_count: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "BacklogCampaign".
 */
export interface BacklogCampaign {
  campaign_id: string;
  job_count: number;
  name: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ReadinessOverrides".
 */
export interface ReadinessOverrides {
  max_wait_secs?: number | null;
  skin_below_c?: number | null;
  skip_thermal?: boolean | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CampaignCreated".
 */
export interface CampaignCreated {
  campaign_id: string;
  created_at: string;
  current_plan_id?: string | null;
  labels?: {
    [k: string]: string;
  };
  name: string;
  owner: PrincipalOwner;
  plan: PlanFiled;
  state?: CampaignState;
  updated_at: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "PlanFiled".
 */
export interface PlanFiled {
  change?: RevisionChange | null;
  fleet_detail: FleetDetail;
  job_count: number;
  job_ids: {
    [k: string]: string;
  };
  merged_job_count: number;
  note?: string | null;
  plan_id: string;
  plan_warnings?: string[];
  warnings: Warning[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "RestoredVariant".
 */
export interface RestoredVariant {
  plan_id: string;
  variant_index: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "Warning".
 */
export interface Warning {
  job_ids: string[];
  kind: WarningKind & string;
  message: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CampaignDocument".
 */
export interface CampaignDocument {
  base: RevisionBase;
  document: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "RevisionBase".
 */
export interface RevisionBase {
  change?: RevisionChange | null;
  note?: string | null;
  plan_id: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CampaignPlanRow".
 */
export interface CampaignPlanRow {
  plan: PlanSummary | null;
  plan_id: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "PlanSummary".
 */
export interface PlanSummary {
  campaign_id?: string | null;
  cancel_requested: boolean;
  change?: RevisionChange | null;
  counts: Counts | null;
  created_at: string;
  note?: string | null;
  plan_id: string;
  status: PlanStatus;
  submitted_by?: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "Counts".
 */
export interface Counts {
  available: number;
  cancelled: number;
  failed: number;
  finished: number;
  running: number;
  total: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CampaignRevised".
 */
export interface CampaignRevised {
  cancel_requested: boolean;
  carried_job_count: number;
  dropped_job_count: number;
  plan: PlanFiled;
  previous_plan_id: string;
  recalculated_job_count: number;
  rerun_job_count: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CampaignVariants".
 */
export interface CampaignVariants {
  base: RevisionBase;
  variants: string[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CampaignView".
 */
export interface CampaignView {
  campaign_id: string;
  created_at: string;
  current_plan_id?: string | null;
  labels?: {
    [k: string]: string;
  };
  name: string;
  owner: PrincipalOwner;
  plan?: PlanSummary | null;
  state?: CampaignState;
  updated_at: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CampaignViewHighlights".
 */
export interface CampaignViewHighlights {
  first: CampaignView[];
  total: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CatalogImportRun".
 */
export interface CatalogImportRun {
  command: CatalogImportCommand;
  entries_created: number;
  entries_hidden: number;
  error_summary: string | null;
  finished_at: string | null;
  id: string;
  members_added: number;
  outcome: CatalogImportOutcome;
  result: CatalogImportResult;
  source: string;
  started_at: string;
  target_name: string | null;
  unclassified_assets: string[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CatalogImportRuns".
 */
export interface CatalogImportRuns {
  items: CatalogImportRun[];
  last_successful_import_at: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ChangeDescriptorEntryStateRequest".
 */
export interface ChangeDescriptorEntryStateRequest {
  action: DescriptorEntryAction;
  confirmation?: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ChangedModelParams".
 */
export interface ChangedModelParams {
  active_millions: number;
  changed: boolean;
  model_key: string;
  note: string | null;
  total_millions: number;
  updated_at: string;
  updated_by: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ChangedOrgUserRow".
 */
export interface ChangedOrgUserRow {
  account_type: UserAccountType & string;
  campaign_scope?: string[] | null;
  changed: boolean;
  contact_email: string;
  created_at: string;
  org_id?: string | null;
  status: UserStatus;
  user_id: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ChangedOrgView".
 */
export interface ChangedOrgView {
  changed: boolean;
  client_count?: number | null;
  created_at: string;
  display_name: string;
  member_count?: number | null;
  org_id: string;
  slug: string;
  status: OrgStatus;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ChangedPlanView".
 */
export interface ChangedPlanView {
  campaign_id?: string | null;
  cancel_requested: boolean;
  change?: RevisionChange | null;
  changed: boolean;
  created_at: string;
  fleet_detail: FleetDetail;
  job_expiry: JobExpiry | null;
  note?: string | null;
  plan_id: string;
  plan_warnings: string[];
  previous_plan_id?: string | null;
  progress_snapshot: ProgressSnapshot | null;
  status: PlanStatus;
  submitted_by?: string | null;
  terminal_at: string | null;
  warnings: Warning[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "JobExpiry".
 */
export interface JobExpiry {
  earliest_expires_at: string | null;
  latest_expires_at: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ProgressSnapshot".
 */
export interface ProgressSnapshot {
  computed_at: string;
  counts: Counts;
  starved_groups: StarvedGroup[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "StarvedGroup".
 */
export interface StarvedGroup {
  any_of?: string[][];
  clients?: string[];
  exclude_clients?: string[];
  excludes?: string[];
  job_ids: string[];
  not_all_of?: string[][];
  requires?: string[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ChangedUserView".
 */
export interface ChangedUserView {
  account_type: UserAccountType & string;
  campaign_scope?: string[] | null;
  changed: boolean;
  contact_email: string;
  created_at: string;
  org_id?: string | null;
  permissions: UserPermissionSet;
  status: UserStatus;
  stored_permissions?: UserPermissionSet | null;
  user_id: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "Client".
 */
export interface Client {
  capabilities?: string[];
  client_details: string;
  client_id: string;
  contact_email?: string;
  device_chip_model?: string | null;
  device_cpu_arch?: string | null;
  device_form_factor?: string | null;
  device_gpu_model?: string | null;
  device_gpu_vram_bytes?: number | null;
  device_name?: string | null;
  device_npu_model?: string | null;
  device_npu_vram_bytes?: number | null;
  device_os_family?: string | null;
  device_os_name?: string | null;
  device_os_version?: string | null;
  device_ram_bytes?: number | null;
  organization: string;
  public_key: string;
  registered_at: string;
  status: ClientStatus;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ClientDetail".
 */
export interface ClientDetail {
  backlog: Backlog | null;
  backlog_by_campaign: BacklogByCampaign | null;
  capabilities?: string[];
  client_details: string;
  client_id: string;
  contact_email?: string;
  derived_capabilities: string[];
  device_chip_model?: string | null;
  device_cpu_arch?: string | null;
  device_form_factor?: string | null;
  device_gpu_model?: string | null;
  device_gpu_vram_bytes?: number | null;
  device_name?: string | null;
  device_npu_model?: string | null;
  device_npu_vram_bytes?: number | null;
  device_os_family?: string | null;
  device_os_name?: string | null;
  device_os_version?: string | null;
  device_ram_bytes?: number | null;
  last_seen_at: string | null;
  may_administer: boolean;
  organization: string;
  owner?: ClientOwner | null;
  owning_org_id?: string | null;
  permissions: ClientPermissionSet;
  public_key: string;
  registered_at: string;
  reindex_pending: boolean;
  status: ClientStatus;
  suspended: boolean;
  tags: string[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ClientHighlights".
 */
export interface ClientHighlights {
  first: Client[];
  total: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ClientListRow".
 */
export interface ClientListRow {
  backlog: Backlog | null;
  capabilities?: string[];
  client_details: string;
  client_id: string;
  contact_email?: string;
  derived_capabilities: string[];
  device_chip_model?: string | null;
  device_cpu_arch?: string | null;
  device_form_factor?: string | null;
  device_gpu_model?: string | null;
  device_gpu_vram_bytes?: number | null;
  device_name?: string | null;
  device_npu_model?: string | null;
  device_npu_vram_bytes?: number | null;
  device_os_family?: string | null;
  device_os_name?: string | null;
  device_os_version?: string | null;
  device_ram_bytes?: number | null;
  last_seen_at: string | null;
  organization: string;
  owner?: ClientOwner | null;
  permissions: ClientPermissionSet;
  public_key: string;
  registered_at: string;
  reindex_pending: boolean;
  status: ClientStatus;
  suspended: boolean;
  tags: string[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ClientListRowPage".
 */
export interface ClientListRowPage {
  items: ClientListRow[];
  next_cursor: string | null;
  unreadable: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ClientPreauthKeyMinted".
 */
export interface ClientPreauthKeyMinted {
  created_at: string;
  default_organization?: string | null;
  default_permissions?: ClientPermissionSet | null;
  default_tags?: string[];
  expired: boolean;
  expires_at: string | null;
  key_id: string;
  minted_by?: string | null;
  note: string | null;
  org_id: string;
  token: string;
  usage: ClientPreauthUsage;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ClientPreauthKeyView".
 */
export interface ClientPreauthKeyView {
  created_at: string;
  default_organization?: string | null;
  default_permissions?: ClientPermissionSet | null;
  default_tags?: string[];
  expired: boolean;
  expires_at: string | null;
  key_id: string;
  minted_by?: string | null;
  note: string | null;
  org_id: string;
  usage: ClientPreauthUsage;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ClientPreauthKeyViewPage".
 */
export interface ClientPreauthKeyViewPage {
  items: ClientPreauthKeyView[];
  next_cursor: string | null;
  unreadable: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CreateDescriptorEntryRequest".
 */
export interface CreateDescriptorEntryRequest {
  /**
   * @minItems 1
   */
  models?: [ModelMemberRequest, ...ModelMemberRequest[]] | null;
  name: string;
  release_time?: string | null;
  /**
   * @minItems 1
   */
  runtimes?: [Runtime, ...Runtime[]] | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ModelMemberRequest".
 */
export interface ModelMemberRequest {
  model: Model;
  quantization: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CreateModelParamsRequest".
 */
export interface CreateModelParamsRequest {
  active_millions?: number | null;
  model_key: string;
  note?: string | null;
  total_millions: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CreateOrgRequest".
 */
export interface CreateOrgRequest {
  display_name?: string | null;
  slug: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "CreateUserRequest".
 */
export interface CreateUserRequest {
  account_type?: UserAccountType & string;
  campaign_scope?: string[] | null;
  contact_email: string;
  org_id?: string | null;
  permissions?: UserPermission[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorCatalogShortList".
 */
export interface DescriptorCatalogShortList {
  items: DescriptorCatalogShortListItem[];
  next_cursor: string | null;
  unreadable: number;
  withheld: DescriptorWithheldReport;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorCatalogShortListItem".
 */
export interface DescriptorCatalogShortListItem {
  descriptor_kind: DescriptorKind;
  members: DescriptorMemberView[];
  name: string;
  release_time: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorMemberView".
 */
export interface DescriptorMemberView {
  added_at: string;
  coverage?: DescriptorMemberCoverage | null;
  descriptor: DescriptorMemberDescriptor;
  digest: string;
  member_key: DescriptorMemberKey;
  model_key?: string | null;
  offered_device_count?: number | null;
  parameters?: DescriptorMemberParameters | null;
  plan_toml: string;
  selector: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorMemberParameters".
 */
export interface DescriptorMemberParameters {
  active_millions: number;
  total_millions: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorWithheldReport".
 */
export interface DescriptorWithheldReport {
  entry_count: number;
  member_count: number;
  members: DescriptorWithheldMember[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorWithheldMember".
 */
export interface DescriptorWithheldMember {
  member_key: DescriptorMemberKey;
  name: string;
  reason: string;
  reason_code: DescriptorWithheldReasonCode;
  selector: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorDigestNames".
 */
export interface DescriptorDigestNames {
  names: {
    [k: string]: DescriptorNameHit[];
  };
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorNameHit".
 */
export interface DescriptorNameHit {
  descriptor_kind: DescriptorKind;
  display_name: string;
  kind: DescriptorEntryKind;
  member_key: DescriptorMemberKey;
  name: string;
  scope: string;
  selector: string;
  state: DescriptorEntryState;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorDisplay".
 */
export interface DescriptorDisplay {
  display: string;
  name: string;
  synonyms: string[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorEntry".
 */
export interface DescriptorEntry {
  created_at: string;
  created_by: string;
  kind: DescriptorEntryKind;
  /**
   * @minItems 1
   */
  members: [DescriptorMember, ...DescriptorMember[]];
  name: string;
  release_time: string;
  scope: string;
  state: DescriptorEntryState;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorEntryListRow".
 */
export interface DescriptorEntryListRow {
  created_at: string;
  created_by: string;
  descriptor_kind: DescriptorKind;
  descriptor_types: string[];
  display_name: string;
  kind: DescriptorEntryKind;
  member_count: number;
  member_summaries: DescriptorMemberSummary[];
  members?: DescriptorMemberView[] | null;
  name: string;
  release_time: string;
  scope: string;
  state: DescriptorEntryState;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorMemberSummary".
 */
export interface DescriptorMemberSummary {
  coverage?: DescriptorMemberCoverage | null;
  member_key: DescriptorMemberKey;
  selector: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorEntryListRowPage".
 */
export interface DescriptorEntryListRowPage {
  items: DescriptorEntryListRow[];
  next_cursor: string | null;
  unreadable: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DescriptorEntryLookup".
 */
export interface DescriptorEntryLookup {
  entries: DescriptorEntryListRow[];
  unknown: string[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "DeviceTimeTotal".
 */
export interface DeviceTimeTotal {
  device_time_ms: number | null;
  unreported_job_count: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ExpiredJob".
 */
export interface ExpiredJob {
  expires_at: string;
  recorded_at: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "FirstClerkSignIn".
 */
export interface FirstClerkSignIn {
  at: string;
  prior_keys: string[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "HeldJob".
 */
export interface HeldJob {
  body_index: number;
  job_id: string;
  plan_id: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "JobInPlan".
 */
export interface JobInPlan {
  lapsed: ExpiredJob | null;
  rerun_history: ParentJob[];
  rerun_of: ParentJob | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ParentJob".
 */
export interface ParentJob {
  job_id: string;
  plan_id: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "JobQueue".
 */
export interface JobQueue {
  expires_at: string | null;
  lease: Lease | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "Lease".
 */
export interface Lease {
  client_id: string | null;
  expires_at: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ParentRun".
 */
export interface ParentRun {
  job: ParentJob;
  rows: ResultRow[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ResultRow".
 */
export interface ResultRow {
  benchmark_flags: (BenchmarkFlags | UnknownValue) | null;
  benchmark_id: string;
  benchmark_type: string;
  client_extras:
    | (
        | {
            [k: string]: unknown;
          }
        | UnknownValue
      )
    | null;
  client_id: string | null;
  client_version: string | null;
  device: {
    [k: string]: unknown;
  };
  device_name: string | null;
  device_time_ms: number | null;
  eval_metadata:
    | (
        | {
            [k: string]: unknown;
          }
        | UnknownValue
      )
    | null;
  failed: boolean | null;
  failed_reason: string | null;
  failure_source: FailureSource | null;
  job_id: string;
  metric: string;
  model_descriptor: (Model | UnknownValue) | null;
  model_descriptor_sha256: string | null;
  model_flags: (ModelFlags | UnknownValue) | null;
  model_params_active_millions: number | null;
  model_params_total_millions: number | null;
  model_vault_descriptor: (Model | UnknownValue) | null;
  observations: {
    [k: string]: unknown;
  };
  parameters: {
    [k: string]: unknown;
  };
  requested_benchmark_flags: (BenchmarkFlags | UnknownValue) | null;
  requested_model_flags: (ModelFlags | UnknownValue) | null;
  requested_runtime_flags: (RuntimeFlags | UnknownValue) | null;
  rescored_from: string | null;
  run_submitted_at: string | null;
  runtime_descriptor: (Runtime | UnknownValue) | null;
  runtime_descriptor_sha256: string | null;
  runtime_flags: (RuntimeFlags | UnknownValue) | null;
  score_runtime_version: string | null;
  scored_at: string | null;
  submitted_at: string | null;
  unit: string;
  value: number | null;
  value_stddev: number | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UnknownValue".
 */
export interface UnknownValue {
  type: UnknownTag;
  value: unknown;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VllmRepetitionDetection".
 */
export interface VllmRepetitionDetection {
  max_pattern_size: number;
  min_count: number;
  min_pattern_size: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "MintClientPreauthRequest".
 */
export interface MintClientPreauthRequest {
  expires_in_days?: number | null;
  note?: string | null;
  usage?: ClientPreauthUsage | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "MintUserPreauthRequest".
 */
export interface MintUserPreauthRequest {
  expires_in_days?: number | null;
  note?: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ModelParams".
 */
export interface ModelParams {
  active_millions: number;
  model_key: string;
  note: string | null;
  total_millions: number;
  updated_at: string;
  updated_by: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ModelParamsDetail".
 */
export interface ModelParamsDetail {
  may_set: boolean;
  model_key: string;
  row: ModelParams | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "ModelParamsPage".
 */
export interface ModelParamsPage {
  items: ModelParams[];
  next_cursor: string | null;
  unreadable: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "NamedAuditEventPage".
 */
export interface NamedAuditEventPage {
  items: AuditEvent[];
  names?: {
    [k: string]: string;
  } | null;
  next_cursor: string | null;
  unreadable: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "NamedCampaignOverview".
 */
export interface NamedCampaignOverview {
  campaign_id: string;
  catalog_org_id: string | null;
  created_at: string;
  current_plan_id?: string | null;
  device_time_all_plans: DeviceTimeTotal | null;
  device_time_current_plan: DeviceTimeTotal | null;
  history: CampaignPlanRow[];
  history_truncated: boolean;
  labels?: {
    [k: string]: string;
  };
  may_administer: boolean;
  may_edit_variants: boolean;
  may_rerun: boolean;
  name: string;
  names?: {
    [k: string]: string;
  } | null;
  owner: PrincipalOwner;
  state?: CampaignState;
  updated_at: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "NamedCampaignView".
 */
export interface NamedCampaignView {
  campaign_id: string;
  created_at: string;
  current_plan_id?: string | null;
  labels?: {
    [k: string]: string;
  };
  name: string;
  names?: {
    [k: string]: string;
  } | null;
  owner: PrincipalOwner;
  plan?: PlanSummary | null;
  state?: CampaignState;
  updated_at: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "NamedCampaignViewPage".
 */
export interface NamedCampaignViewPage {
  items: CampaignView[];
  names?: {
    [k: string]: string;
  } | null;
  next_cursor: string | null;
  unreadable: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "NamedPlanJobs".
 */
export interface NamedPlanJobs {
  cancel_permission?: UserPermission | null;
  descriptor_names?: {
    [k: string]: DescriptorDisplay;
  } | null;
  document_state?: DocumentState | null;
  fleet_detail: FleetDetail;
  jobs: PlanJob[];
  limit: number;
  may_cancel?: boolean | null;
  may_edit_note?: boolean | null;
  names?: {
    [k: string]: string;
  } | null;
  offset: number;
  plan_id: string;
  rerun_campaign_id?: string | null;
  results_read?: ResultsRead | null;
  total: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "PlanJob".
 */
export interface PlanJob {
  job_id: string;
  plan: JobInPlan;
  planned: PlanCell | null;
  planned_model_sha256?: string | null;
  planned_runtime_sha256?: string | null;
  queue: JobQueue | null;
  result?: JobResult | null;
  state: JobState;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "PlanCell".
 */
export interface PlanCell {
  benchmark: string;
  benchmark_flags: BenchmarkFlags | null;
  benchmark_type: BenchmarkType | null;
  model: Model;
  model_flags: ModelFlags | null;
  runtime: Runtime;
  runtime_flags: RuntimeFlags | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "NamedPlanView".
 */
export interface NamedPlanView {
  campaign_id?: string | null;
  cancel_requested: boolean;
  change?: RevisionChange | null;
  created_at: string;
  fleet_detail: FleetDetail;
  job_expiry: JobExpiry | null;
  names?: {
    [k: string]: string;
  } | null;
  note?: string | null;
  plan_id: string;
  plan_warnings: string[];
  previous_plan_id?: string | null;
  progress_snapshot: ProgressSnapshot | null;
  status: PlanStatus;
  submitted_by?: string | null;
  terminal_at: string | null;
  warnings: Warning[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "NamedPlanViewPage".
 */
export interface NamedPlanViewPage {
  items: PlanView[];
  names?: {
    [k: string]: string;
  } | null;
  next_cursor: string | null;
  unreadable: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "PlanView".
 */
export interface PlanView {
  campaign_id?: string | null;
  cancel_requested: boolean;
  change?: RevisionChange | null;
  created_at: string;
  fleet_detail: FleetDetail;
  job_expiry: JobExpiry | null;
  note?: string | null;
  plan_id: string;
  plan_warnings: string[];
  previous_plan_id?: string | null;
  progress_snapshot: ProgressSnapshot | null;
  status: PlanStatus;
  submitted_by?: string | null;
  terminal_at: string | null;
  warnings: Warning[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "NamedVaultResourceDetail".
 */
export interface NamedVaultResourceDetail {
  alias?: Model | null;
  artifact?: VaultArtifact | null;
  bytes: number;
  created_at: string;
  entry?: VaultEntry;
  may_administer?: boolean | null;
  model_params?: ModelParamsDetail | null;
  name: string;
  names?: {
    [k: string]: string;
  } | null;
  object?: ObjectState | null;
  origin?: VaultOrigin | null;
  owner: PrincipalOwner;
  resource_id: string;
  sha256: string;
  toml?: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultArtifact".
 */
export interface VaultArtifact {
  archive?: VaultStoredFile;
  dir_archive?: VaultStoredFile;
  draft?: VaultStoredFile;
  draft_dir_archive?: VaultStoredFile;
  flavor?: string;
  mmproj?: VaultStoredFile;
  model?: VaultStoredFile;
  target_dir_archive?: VaultStoredFile;
  type: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultStoredFile".
 */
export interface VaultStoredFile {
  filename: string;
  resource_id: string;
  sha256: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "NamedVaultResourceDetailPage".
 */
export interface NamedVaultResourceDetailPage {
  items: VaultResourceDetail[];
  names?: {
    [k: string]: string;
  } | null;
  next_cursor: string | null;
  unreadable: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultResourceDetail".
 */
export interface VaultResourceDetail {
  alias?: Model | null;
  artifact?: VaultArtifact | null;
  bytes: number;
  created_at: string;
  entry?: VaultEntry;
  may_administer?: boolean | null;
  model_params?: ModelParamsDetail | null;
  name: string;
  object?: ObjectState | null;
  origin?: VaultOrigin | null;
  owner: PrincipalOwner;
  resource_id: string;
  sha256: string;
  toml?: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OrgBenchmark".
 */
export interface OrgBenchmark {
  benchmark_id: string;
  benchmark_type: string;
  org_id: string;
  [k: string]: unknown;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OrgBenchmarks".
 */
export interface OrgBenchmarks {
  benchmarks: OrgBenchmark[];
  org_id: string;
  unreadable_benchmark_ids: string[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OrgOverview".
 */
export interface OrgOverview {
  benchmark_count: number | null;
  campaigns: CampaignViewHighlights | null;
  client_count: number;
  client_preauth_key_count: number | null;
  client_status_counts: {
    approved?: number;
    pending?: number;
  } | null;
  created_at: string;
  display_name: string;
  may_administer: boolean;
  may_delete: boolean;
  member_count: number;
  member_status_counts: {
    [k: string]: number;
  };
  org_id: string;
  pending_clients: ClientHighlights | null;
  pending_members: UserHighlights;
  slug: string;
  status: OrgStatus;
  suspended_clients: ClientHighlights | null;
  vault_resource_count: number | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserHighlights".
 */
export interface UserHighlights {
  first: User[];
  total: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "User".
 */
export interface User {
  account_type: UserAccountType & string;
  campaign_scope?: string[] | null;
  contact_email: string;
  created_at: string;
  status: UserStatus;
  user_id: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OrgView".
 */
export interface OrgView {
  client_count?: number | null;
  created_at: string;
  display_name: string;
  member_count?: number | null;
  org_id: string;
  slug: string;
  status: OrgStatus;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OrgViewPage".
 */
export interface OrgViewPage {
  items: OrgView[];
  next_cursor: string | null;
  unreadable: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OwnUserView".
 */
export interface OwnUserView {
  account_type: UserAccountType & string;
  campaign_scope?: string[] | null;
  contact_email: string;
  created_at: string;
  first_clerk_sign_in?: FirstClerkSignIn | null;
  keys: UserKey[];
  may: UserAbilities;
  org_id?: string | null;
  org_slug?: string | null;
  permissions: UserPermissionSet;
  status: UserStatus;
  user_id: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserAbilities".
 */
export interface UserAbilities {
  administer_fleet: Reach;
  administer_roster: Reach;
  create_orgs: boolean;
  edit_catalog: Reach;
  file_plans: boolean;
  grant: UserPermission[];
  grant_service: UserPermission[];
  read_fleet: boolean;
  read_plans: Reach;
  read_roster: Reach;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OwnerMatch".
 */
export interface OwnerMatch {
  label: string;
  name: string;
  type: OwnerType;
  value: VaultPrincipalId;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "OwnerMatches".
 */
export interface OwnerMatches {
  owners: OwnerMatch[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "PlanCancelRequested".
 */
export interface PlanCancelRequested {
  message: string;
  outcome: CancelOutcome;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "PlanDryRun".
 */
export interface PlanDryRun {
  bodies: unknown[];
  imports: PlannedImport[];
  plan_warnings: string[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "PlanIssue".
 */
export interface PlanIssue {
  column: number | null;
  line: number | null;
  message: string;
  severity: IssueSeverity;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "PlanValidation".
 */
export interface PlanValidation {
  gated_model_count: number | null;
  issues: PlanIssue[];
  job_count: number | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "PlanVariants".
 */
export interface PlanVariants {
  variants: string[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "PlanViewPage".
 */
export interface PlanViewPage {
  items: PlanView[];
  next_cursor: string | null;
  unreadable: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "RecalculateJobsRequest".
 */
export interface RecalculateJobsRequest {
  all_evals?: boolean;
  job_ids?: string[] | null;
  note?: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "RegisterClerkUserKeyRequest".
 */
export interface RegisterClerkUserKeyRequest {
  key_details: string;
  public_key: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "RegisterUserKeyRequest".
 */
export interface RegisterUserKeyRequest {
  contact_email?: string | null;
  key_details: string;
  public_key: string;
  user_preauth_key?: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "RerunJobsRequest".
 */
export interface RerunJobsRequest {
  answered?: Answered | null;
  job_ids?: string[] | null;
  note?: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "RevertCampaignRequest".
 */
export interface RevertCampaignRequest {
  note?: string | null;
  plan_id: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "RevisionDryRun".
 */
export interface RevisionDryRun {
  bodies: unknown[];
  carried_job_count: number;
  document: string;
  dropped_job_ids: string[];
  held_jobs: HeldJob[];
  imports: PlannedImport[];
  inherited_job_count: number;
  new_job_count: number;
  plan_warnings: string[];
  recalculated_job_count: number;
  repeated_variants: number[];
  requirements_rerun_job_count: number;
  rerun_job_count: number;
  restored_job_count: number;
  satisfied_job_count: number;
  unchanged: boolean;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "SetCampaignOwnerRequest".
 */
export interface SetCampaignOwnerRequest {
  owner: PrincipalOwner;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "SetClientOwnerRequest".
 */
export interface SetClientOwnerRequest {
  owner: ClientOwner | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "SetUserOrgRequest".
 */
export interface SetUserOrgRequest {
  org_id: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "SetUserPermissionsRequest".
 */
export interface SetUserPermissionsRequest {
  permissions: UserPermission[];
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UpdateCampaignRequest".
 */
export interface UpdateCampaignRequest {
  labels?: {
    [k: string]: string;
  } | null;
  name?: string | null;
  state?: CampaignState | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UpdateModelParamsRequest".
 */
export interface UpdateModelParamsRequest {
  active_millions?: number | null;
  note?: string | null;
  total_millions: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UpdateOrgRequest".
 */
export interface UpdateOrgRequest {
  display_name?: string | null;
  status?: OrgStatus | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UpdatePlanRequest".
 */
export interface UpdatePlanRequest {
  note?: string | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UpdateUserRequest".
 */
export interface UpdateUserRequest {
  account_type?: UserAccountType | null;
  campaign_scope?: string[] | null;
  status?: UserStatus | null;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UploadVaultResourcesRequest".
 */
export interface UploadVaultResourcesRequest {
  archive?: VaultUploadFile;
  dir_archive?: VaultUploadFile;
  draft?: VaultUploadFile;
  draft_dir_archive?: VaultUploadFile;
  flavor?: string;
  mmproj?: VaultUploadFile;
  model?: VaultUploadFile;
  name: string;
  owner?: string;
  target_dir_archive?: VaultUploadFile;
  type: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultUploadFile".
 */
export interface VaultUploadFile {
  filename: string;
  sha256: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserDetail".
 */
export interface UserDetail {
  account_type: UserAccountType & string;
  awaits_sign_in?: boolean | null;
  campaign_scope?: string[] | null;
  contact_email: string;
  created_at: string;
  may_administer: boolean;
  may_grant: UserPermission[];
  may_revoke: UserPermission[];
  org_id?: string | null;
  permissions: UserPermissionSet;
  status: UserStatus;
  stored_permissions?: UserPermissionSet | null;
  user_id: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserKeyPage".
 */
export interface UserKeyPage {
  items: UserKey[];
  next_cursor: string | null;
  unreadable: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserKeyRegistered".
 */
export interface UserKeyRegistered {
  key_id: string;
  status: UserStatus;
  user_id: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserPermissionsAmended".
 */
export interface UserPermissionsAmended {
  account_type: UserAccountType & string;
  campaign_scope?: string[] | null;
  changed: boolean;
  contact_email: string;
  created_at: string;
  org_id?: string | null;
  permissions: UserPermissionSet;
  status: UserStatus;
  still_granted: UserPermission[];
  stored_permissions?: UserPermissionSet | null;
  user_id: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserPreauthKeyMinted".
 */
export interface UserPreauthKeyMinted {
  created_at: string;
  expired: boolean;
  expires_at: string | null;
  key_id: string;
  minted_by?: string | null;
  note: string | null;
  token: string;
  usage: UserPreauthUsage;
  user_id: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserPreauthKeyView".
 */
export interface UserPreauthKeyView {
  created_at: string;
  expired: boolean;
  expires_at: string | null;
  key_id: string;
  minted_by?: string | null;
  note: string | null;
  usage: UserPreauthUsage;
  user_id: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserPreauthKeyViewPage".
 */
export interface UserPreauthKeyViewPage {
  items: UserPreauthKeyView[];
  next_cursor: string | null;
  unreadable: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserView".
 */
export interface UserView {
  account_type: UserAccountType & string;
  campaign_scope?: string[] | null;
  contact_email: string;
  created_at: string;
  org_id?: string | null;
  permissions: UserPermissionSet;
  status: UserStatus;
  stored_permissions?: UserPermissionSet | null;
  user_id: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "UserViewPage".
 */
export interface UserViewPage {
  items: UserView[];
  next_cursor: string | null;
  unreadable: number;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultResource".
 */
export interface VaultResource {
  artifact?: VaultArtifact | null;
  bytes: number;
  created_at: string;
  name: string;
  origin?: VaultOrigin | null;
  owner: PrincipalOwner;
  resource_id: string;
  sha256: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultResourcesUploaded".
 */
export interface VaultResourcesUploaded {
  entry: VaultEntry;
  resources: VaultResource[];
  toml: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultType".
 */
export interface VaultType {
  description: string;
  files: VaultTypeFile[];
  flavors: string[];
  type: string;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultTypeFile".
 */
export interface VaultTypeFile {
  format: VaultFileFormat;
  optional: boolean;
  role: VaultFileRole;
}
/**
 * This interface was referenced by `ApiSchema`'s JSON-Schema
 * via the `definition` "VaultTypes".
 */
export interface VaultTypes {
  models: VaultType[];
  runtimes: VaultType[];
}
