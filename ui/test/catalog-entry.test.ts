import { describe, expect, test } from "vitest";
import { ApiError } from "../src/lib/api";
import {
  CATALOG_NAME_MAX, DIGEST_BATCH, catalogNameError, catalogOrgOf, digestBatches, displayOf, entryOffer, entryTarget,
  memberTree, namedIn, offerReason, quantizationError, quantizationHints, storedName, writeFailure, type EntryTarget,
  type MemberNode,
} from "../src/lib/catalog-entry";
import type {
  DescriptorEntryListRow, DescriptorKind, DescriptorMemberKey, DescriptorMemberView, DescriptorNameHit, Runtime,
} from "../src/lib/api-types";

const ACME = "org_acme";
const DIGEST = "a".repeat(64);
const RUNTIME = JSON.stringify({
  type: "llamacpp_cli_stock_tools", source: "github_release", repository_url: "github.com/ggml-org/llama.cpp",
  repository_version: "b11669", flavor: "macos-arm64",
});
const VAULT = JSON.stringify({
  type: "llamacpp_cli_stock_tools", source: "vault_remote_archive", url: "/vault/resources/vault_1/content",
  principal_id: ACME, flavor: "linux-x64-rocm",
});

const COMMIT = "c".repeat(40);
const hf = (more: Record<string, unknown> = {}) => ({ source: "huggingface", org: "acme", repo_name: "AcmeLM-MLX", ...more });
const model = (value: Record<string, unknown>) => JSON.stringify(value);
const GGUF = model({ type: "gguf_text", source: "huggingface", org: "acme", repo_name: "AcmeLM-GGUF",
                     path: "AcmeLM-Q4_0.gguf", revision: COMMIT });
const modelHit = (scope: string, name: string): DescriptorNameHit => hit(scope, name, {
  descriptor_kind: "model", member_key: { kind: "model", format: "gguf", quantization: "q4_0" },
  selector: `${name}::gguf::q4_0`,
});

const hit = (scope: string, name: string, more: Partial<DescriptorNameHit> = {}): DescriptorNameHit => ({
  descriptor_kind: "runtime", display_name: name, kind: "hand-made", member_key: { kind: "runtime", flavor: "macos-arm64" },
  name, scope, selector: `${name}::macos-arm64`, state: "active", ...more,
});

describe("a catalog entry from a campaign's progress table", () => {
  test.each([
    ["my-fork", null],
    ["B11669.rc_1", null],
    ["7zip", null],
    ["x".repeat(CATALOG_NAME_MAX), null],
    ["", "Type a name."],
    ["-fork", "Start the name with a letter or a digit."],
    [".fork", "Start the name with a letter or a digit."],
    ["my fork", null],
    ["LFM2.5 Audio 1.5B", null],
    [" fork", "Start the name with a letter or a digit."],
    ["my  fork", "Do not put two spaces together in the name."],
    ["fork ", "Do not end the name with a space."],
    ["my\tfork", "Use only letters, digits, spaces, '.', '_' and '-' in the name."],
    ["my\u00a0fork", "Use only letters, digits, spaces, '.', '_' and '-' in the name."],
    ["acme/fork", "Use only letters, digits, spaces, '.', '_' and '-' in the name."],
    ["fork::macos-arm64", "Use only letters, digits, spaces, '.', '_' and '-' in the name."],
    ["café", "Use only letters, digits, spaces, '.', '_' and '-' in the name."],
    ["x".repeat(CATALOG_NAME_MAX + 1), `Use at most ${CATALOG_NAME_MAX} characters in the name.`],
  ])("checks the name %j with the server's grammar", (text, error) => {
    expect(catalogNameError(text)).toBe(error);
  });

  test("shows a digest's names as the jobs read does: the first over the others as synonyms", () => {
    const org = hit(ACME, "my-fork");
    const global = hit("global", "llamacpp-v0.7.0", { display_name: "global/llamacpp-v0.7.0",
                                                         selector: "global/llamacpp-v0.7.0::macos-arm64" });
    expect(displayOf([org, global])).toEqual({
      display: "my-fork::macos-arm64", name: "my-fork", synonyms: ["global/llamacpp-v0.7.0::macos-arm64"],
    });
    expect(displayOf([])).toBeNull();
  });

  test("counts only a name of the same kind in the organization itself", () => {
    expect(namedIn([hit(ACME, "my-fork")], ACME, "runtime")).toBe(true);
    expect(namedIn([hit("global", "llamacpp-v0.7.0")], ACME, "runtime")).toBe(false);
    expect(namedIn([hit("org_beta", "their-fork")], ACME, "runtime")).toBe(false);
    expect(namedIn([modelHit(ACME, "a-model")], ACME, "runtime")).toBe(false);
    expect(namedIn([modelHit(ACME, "a-model")], ACME, "model")).toBe(true);
    expect(namedIn([], ACME, "model")).toBe(false);
  });

  test.each([
    ["q4_0", null],
    ["4bit", null],
    ["", "Type a quantization label, such as q4_0."],
    ["Q4_0", "Use only lowercase letters, digits and '_' in the quantization label."],
    ["q4-0", "Use only lowercase letters, digits and '_' in the quantization label."],
  ])("checks the quantization label %j with the server's grammar", (text, error) => {
    expect(quantizationError(text)).toBe(error);
  });

  test.each([
    ["an unnamed runtime", RUNTIME, DIGEST, new Map(), "create"],
    ["a runtime with a global name only", RUNTIME, DIGEST, new Map([[DIGEST, [hit("global", "b11669")]]]), "create"],
    ["a runtime that the organization names", RUNTIME, DIGEST, new Map([[DIGEST, [hit(ACME, "my-fork")]]]), null],
    ["a vault runtime", VAULT, DIGEST, new Map(), "vault"],
    ["a runtime without a digest", RUNTIME, undefined, new Map(), null],
    ["a runtime before the names are read", RUNTIME, DIGEST, null, null],
  ])("offers %s: %j", (_, descriptor, digest, names, offer) => {
    expect(entryOffer(descriptor, "runtime", digest, names, ACME)).toBe(offer);
  });

  test.each([
    ["a pinned Hugging Face model", GGUF, new Map(), "create"],
    ["a model with a global name only", GGUF, new Map([[DIGEST, [modelHit("global", "acmelm")]]]), "create"],
    ["a model that the organization names", GGUF, new Map([[DIGEST, [modelHit(ACME, "acmelm")]]]), null],
    ["a model with only a runtime name in the organization", GGUF, new Map([[DIGEST, [hit(ACME, "my-fork")]]]), "create"],
    ["a vault model", model({ type: "gguf_text", source: "vault_url", url: "/vault/resources/vault_1/content",
                              principal_id: ACME }), new Map(), "create"],
    ["a model with vault and Hugging Face sources",
     model({ type: "mlx_speculative", target: hf({ revision: COMMIT }),
             draft: { source: "vault_archive", url: "/vault/resources/vault_1/content", principal_id: ACME } }),
     new Map(), "foreign"],
    ["a model at a URL", model({ type: "gguf_text", source: "url", url: "https://example.com/m.gguf" }), new Map(),
     "foreign"],
    ["a model in a local directory", model({ type: "mlx", source: "relative_dir", dir: "models/m" }), new Map(),
     "foreign"],
    ["apple_foundation_text", model({ type: "apple_foundation_text" }), new Map(), "foreign"],
    ["a model with no revision", model({ type: "mlx", ...hf() }), new Map(), "unpinned"],
    ["a model at a branch", model({ type: "mlx", ...hf({ revision: "main" }) }), new Map(), "unpinned"],
    ["a model at an uppercase commit", model({ type: "mlx", ...hf({ revision: COMMIT.toUpperCase() }) }), new Map(),
     "unpinned"],
    ["a speculative pair with one pinned repository",
     model({ type: "mlx_speculative", target: hf({ revision: COMMIT }), draft: hf({ repo_name: "AcmeLM-Draft" }) }),
     new Map(), "unpinned"],
    ["a speculative pair with each repository pinned",
     model({ type: "mlx_speculative", target: hf({ revision: COMMIT }),
             draft: hf({ repo_name: "AcmeLM-Draft", revision: COMMIT }) }),
     new Map(), "create"],
    ["a model before the names are read", GGUF, null, null],
  ])("offers %s", (_, descriptor, names, offer) => {
    expect(entryOffer(descriptor, "model", DIGEST, names, ACME)).toBe(offer);
  });

  test("says why each disabled button cannot create an entry", () => {
    expect(offerReason("vault", "runtime")).toBe("The catalog cannot name a vault runtime yet.");
    expect(offerReason("foreign", "model")).toBe("The catalog names models on Hugging Face or in the organization's vault.");
    expect(offerReason("unpinned", "model")).toMatch(/^This model names no commit\./);
  });

  test("reads the digests in batches the route takes, each digest once", () => {
    const digests = Array.from({ length: DIGEST_BATCH + 1 }, (_, i) => i.toString(16).padStart(64, "0"));
    expect(digestBatches([...digests, digests[0]!]).map((batch) => batch.length)).toEqual([DIGEST_BATCH, 1]);
    expect(digestBatches([])).toEqual([]);
  });

  test("labels the campaign's catalog organization by its slug, else its name in the overview, else its id", () => {
    const slugOf = (id: string) => (id === ACME ? "acme" : null);
    expect(catalogOrgOf({ catalog_org_id: ACME, names: {} }, slugOf)).toEqual({ id: ACME, label: "acme" });
    expect(catalogOrgOf({ catalog_org_id: "org_beta", names: { org_beta: "beta" } }, slugOf))
      .toEqual({ id: "org_beta", label: "beta" });
    expect(catalogOrgOf({ catalog_org_id: "org_beta", names: null }, slugOf)).toEqual({ id: "org_beta", label: "org_beta" });
    expect(catalogOrgOf({ catalog_org_id: null }, slugOf)).toBeNull();
  });

  test.each([
    [new ApiError(409, "catalog name my-fork is already held by a active entry"), "entry",
     "The server refused the entry: 409: catalog name my-fork is already held by a active entry"],
    [new ApiError(502, "bad gateway"), "entry",
     "The server failed (502: bad gateway). Read the page again to see whether the server created the entry."],
    [new TypeError("Failed to fetch"), "entry",
     "No answer from the server (Failed to fetch). Read the page again to see whether the server created the entry."],
    [new ApiError(409, "duplicate member format and quantization gguf::q4_0"), "member",
     "The server refused the member: 409: duplicate member format and quantization gguf::q4_0"],
    [new ApiError(409, "duplicate member flavor linux-x64-vulkan"), "member",
     "The server refused the member: 409: duplicate member flavor linux-x64-vulkan"],
    [new ApiError(502, "bad gateway"), "member",
     "The server failed (502: bad gateway). Read the page again to see whether the server added the member."],
  ] as const)("says why a write failed: %s (%s)", (error, noun, said) => {
    expect(writeFailure(error, noun)).toBe(said);
  });
});

const entry = (name: string, more: Partial<DescriptorEntryListRow> = {}): DescriptorEntryListRow => ({
  created_at: "2026-10-28T00:00:00Z", created_by: "user_kyle", descriptor_kind: "model", descriptor_types: ["gguf_text"],
  display_name: name, kind: "hand-made", member_count: 1, member_summaries: [], name, release_time: "2026-10-28T00:00:00Z",
  scope: ACME, state: "active", ...more,
});

const LLAMACPP = { repository_url: "github.com/ggml-org/llama.cpp", repository_version: "b11669", source: "github_release",
                  type: "llamacpp_cli_stock_tools" };
const runtimeMember = (descriptor: Record<string, unknown>): DescriptorMemberView => ({
  added_at: "2026-10-28T00:00:00Z", descriptor: descriptor as Runtime, digest: DIGEST,
  plan_toml: "{ type = 'llamacpp_cli_stock_tools' }",
  member_key: { kind: "runtime", flavor: String(descriptor.flavor) }, selector: `my-build::${String(descriptor.flavor)}`,
});

describe("the target of a typed name in the dialog", () => {
  const model = entry("acme-lm");
  const hidden = entry("acme-lm-old", { state: "hidden" });
  const build = entry("my-build", {
    descriptor_kind: "runtime", descriptor_types: ["llamacpp_cli_stock_tools"],
    members: [runtimeMember({ ...LLAMACPP, flavor: "macos-arm64" })],
  });
  const entries = [model, hidden, build];
  const aModel = { kind: "model", value: { type: "gguf_text" } } as const;
  const sameBuild = { kind: "runtime", value: { ...LLAMACPP, flavor: "linux-x64-vulkan" } } as const;
  const otherBuild = { kind: "runtime", value: { ...LLAMACPP, repository_version: "b11670", flavor: "linux-x64" } } as const;

  test.each<[string, string, DescriptorEntryListRow[], { kind: DescriptorKind; value: unknown }, EntryTarget]>([
    ["a model, no entries", "acme-lm", [], aModel, { to: "create" }],
    ["a model, its entry", "acme-lm", entries, aModel, { to: "add", entry: model }],
    ["a model, its entry in another case", "ACME-LM", entries, aModel, { to: "add", entry: model }],
    ["a model, a hidden entry", "acme-lm-old", entries, aModel, { to: "add", entry: hidden }],
    ["a model, a runtime entry", "my-build", entries, aModel, { to: "taken", why: "kind", entry: build }],
    ["a model, a free name", "acme-lm-2", entries, aModel, { to: "create" }],
    ["a runtime, a free name", "my-build-2", entries, sameBuild, { to: "create" }],
    ["a runtime, a flavor of the build", "my-build", entries, sameBuild, { to: "add", entry: build }],
    ["a runtime, another build", "my-build", entries, otherBuild, { to: "taken", why: "build", entry: build }],
    ["a runtime, a model entry", "acme-lm", entries, sameBuild, { to: "taken", why: "kind", entry: model }],
  ])("%s", (_, name, rows, descriptor, expected) => {
    expect(entryTarget(name, rows, descriptor)).toEqual(expected);
  });
});

describe("the stored case of a typed name", () => {
  const entries = [entry("AcmeLM"), entry("my-build", { descriptor_kind: "runtime" })];

  test.each([
    ["the stored case", "AcmeLM", "AcmeLM"],
    ["another case", "acmelm", "AcmeLM"],
    ["a name that an entry of another kind holds", "MY-BUILD", "my-build"],
    ["a free name", "ACMELM-2", "ACMELM-2"],
    ["a prefix of a name", "acme", "acme"],
  ])("%s", (_, typed, shown) => {
    expect(storedName(typed, entries)).toBe(shown);
  });
});

describe("the members of an entry as a tree", () => {
  const leaf = (label: string): MemberNode => ({ label, children: [] });
  const summary = (member_key: DescriptorMemberKey) => ({ member_key, selector: "" });
  const model = (format: "gguf" | "mlx", quantization: string) => summary({ kind: "model", format, quantization });
  const runtime = (flavor: string | null) => summary({ kind: "runtime", flavor });

  test.each<[string, DescriptorEntryListRow["member_summaries"], MemberNode[]]>([
    ["no members", [], []],
    ["a model's quantizations under each format, in member order",
     [model("gguf", "q4_0"), model("mlx", "4bit"), model("gguf", "q8_0")],
     [{ label: "gguf", children: [leaf("q4_0"), leaf("q8_0")] }, { label: "mlx", children: [leaf("4bit")] }]],
    ["a runtime's flavors, with default for no flavor", [runtime(null), runtime("macos-arm64")],
     [leaf("default"), leaf("macos-arm64")]],
  ])("%s", (_, member_summaries, tree) => {
    expect(memberTree(entry("acme-lm", { member_summaries }))).toEqual(tree);
  });
});

describe("the quantizations that a model descriptor shows", () => {
  const huggingface = (more: Record<string, unknown>) => ({ source: "huggingface", org: "acme", revision: COMMIT, ...more });

  test.each<[string, unknown, string[]]>([
    ["a GGUF file", huggingface({ type: "gguf_text", repo_name: "AcmeLM-GGUF", path: "AcmeLM-Q4_0.gguf" }), ["q4_0"]],
    ["a K-quant", huggingface({ type: "gguf_text", repo_name: "LFM2.5-350M-GGUF", path: "LFM2.5-350M-Q4_K_M.gguf" }),
     ["q4_k_m"]],
    ["an importance quant", huggingface({ type: "gguf_text", repo_name: "X-GGUF", path: "X-IQ4_XS.gguf" }), ["iq4_xs"]],
    ["a name with a Q that is not a quantization",
     huggingface({ type: "gguf_text", repo_name: "Qwen3.5-0.8B-GGUF", path: "Qwen3.5-0.8B-Q4_0.gguf" }), ["q4_0"]],
    ["a vision model and its projector",
     huggingface({ type: "gguf_vision", repo_name: "VL-GGUF", model: "VL-450M-Q4_0.gguf", mmproj: "mmproj-VL-450M-F16.gguf" }),
     ["q4_0", "f16"]],
    ["a speculative pair, each label once",
     huggingface({ type: "gguf_vision_speculative", repo_name: "VL-GGUF", model: "VL-3B-Q8_0.gguf",
                   mmproj: "mmproj-VL-3B-BF16.gguf", draft: "VL-3B-Draft-Q8_0.gguf" }), ["q8_0", "bf16"]],
    ["an MLX repository", huggingface({ type: "mlx", repo_name: "LFM2-1.2B-MLX-4bit" }), ["4bit"]],
    ["an OpenVINO scheme", huggingface({ type: "openvino", repo_name: "LFM2.5-OV", prefix: "LFM2.5-350M/int4-asym-gq128" }),
     ["int4_asym_gq128"]],
    ["an ONNX scheme", huggingface({ type: "onnx_genai", repo_name: "X-ONNX", prefix: "cpu_and_mobile/cpu-int4-rtn-block-32" }),
     ["int4_rtn_block_32"]],
    ["a float prefix", huggingface({ type: "openvino", repo_name: "LFM2.5-OV", prefix: "LFM2.5-350M/fp16" }), ["fp16"]],
    ["a vault file and a hash", { type: "gguf_text", source: "vault_url", principal_id: ACME,
                                  url: "/vault/resources/vault_1/content", sha256: "f16a".repeat(16) }, []],
    ["no descriptor", null, []],
  ])("%s", (_, descriptor, hints) => {
    expect(quantizationHints(descriptor)).toEqual(hints);
    for (const hint of hints) expect(quantizationError(hint)).toBeNull();
  });
});
