import { describe, expect, test } from "vitest";
import type {
  DescriptorCatalogShortList, DescriptorDigestNames, DescriptorEntryListRow, DescriptorMemberParameters,
  DescriptorMemberView, DescriptorNameHit, ModelFormat,
} from "../src/lib/api-types";
import {
  CATALOG_TABS, catalogEntryPath, catalogKindOf, catalogListQuery, catalogPath, catalogSynonyms, catalogTable,
  catalogTabs, flavors, modelCatalogTable, modelMemberRows, offerWarning, parameterText, sharedParameters,
  shortListSections, withheldSummary,
} from "../src/lib/catalog";
import { tableState, tableView } from "../src/lib/table";

const row = (name = "llamacpp-v0.7.0"): DescriptorEntryListRow => ({
  name, scope: "global", display_name: name, descriptor_kind: "runtime",
  descriptor_types: ["llamacpp_cli_stock_tools"], kind: "stable", state: "active",
  release_time: "2026-10-01T00:00:00Z", created_at: "2026-10-02T00:00:00Z", created_by: "importer:llamacpp",
  member_count: 2,
  member_summaries: [
    { member_key: { kind: "runtime", flavor: "macos-arm64" }, selector: `${name}::macos-arm64`, coverage: "covered" },
    { member_key: { kind: "runtime", flavor: "linux-x64" }, selector: `${name}::linux-x64`, coverage: "no_rule" },
  ],
  members: [
    { member_key: { kind: "runtime", flavor: "macos-arm64" }, selector: `${name}::macos-arm64`,
      added_at: "2026-10-02T00:00:00Z", digest: "a".repeat(64), descriptor: { type: "apple_foundation" },
      plan_toml: "{ type = 'apple_foundation' }" },
    { member_key: { kind: "runtime", flavor: "linux-x64" }, selector: `${name}::linux-x64`,
      added_at: "2026-10-02T00:00:00Z", digest: "b".repeat(64), descriptor: { type: "apple_foundation" },
      plan_toml: "{ type = 'apple_foundation' }" },
  ],
});

const hit = (name: string, scope = "global", kind: DescriptorNameHit["kind"] = "daily"): DescriptorNameHit => ({
  name, scope, kind, descriptor_kind: "runtime", state: "active", display_name: name,
  member_key: { kind: "runtime", flavor: "macos-arm64" }, selector: `${name}::macos-arm64`,
});

describe("the global runtime catalog", () => {
  test("the server query limits scope and kind, and includes hidden only on request", () => {
    expect(Object.fromEntries(new URLSearchParams(catalogListQuery("  Llama  ", false, "runtime")))).toEqual({
      scope: "global", descriptor_kind: "runtime", state: "active,pinned", limit: "200", prefix: "Llama",
    });
    expect(new URLSearchParams(catalogListQuery("", true, "runtime")).get("state")).toBe("active,pinned,hidden");
  });

  test("the compact row gives flavors and warns about uncovered members", () => {
    expect(flavors(row())).toBe("macos-arm64, linux-x64");
    expect(offerWarning(row())).toBe("1 without an offer rule");
    expect(offerWarning({ ...row(), member_summaries: [row().member_summaries[0]!] })).toBeNull();
  });

  test("kind, state, and runtime type filters keep API order until sorted", () => {
    const entries = [
      { ...row("Z-newest"), kind: "daily" as const },
      { ...row("A-middle"), state: "hidden" as const },
      { ...row("B-oldest"), descriptor_types: ["apple_foundation"] },
    ];
    const view = (query: string) => tableView(catalogTable,
      tableState(catalogTable, new URLSearchParams(query)), entries).map((entry) => entry.name);
    expect(view("")).toEqual(["Z-newest", "A-middle", "B-oldest"]);
    expect(view("kind=daily")).toEqual(["Z-newest"]);
    expect(view("state=hidden")).toEqual(["A-middle"]);
    expect(view("type=apple_foundation")).toEqual(["B-oldest"]);
    expect(view("sort=name")).toEqual(["A-middle", "B-oldest", "Z-newest"]);
  });

  test("synonyms are global, distinct, and linked by bare entry name", () => {
    const entry = row();
    const names: DescriptorDigestNames = { names: {
      ["a".repeat(64)]: [hit(entry.name), hit("another-name", "org_1"), hit("daily-name"), hit("daily-name")],
      ["b".repeat(64)]: [hit("daily-name"), hit("hand-made", "global", "hand-made")],
    } };
    expect(catalogSynonyms(entry, names)).toEqual([
      { member: "macos-arm64", name: "daily-name", kind: "daily", href: catalogEntryPath("daily-name") },
      { member: "linux-x64", name: "daily-name", kind: "daily", href: catalogEntryPath("daily-name") },
      { member: "linux-x64", name: "hand-made", kind: "hand-made", href: catalogEntryPath("hand-made") },
    ]);
  });
});

const counts = (total: number, active = total): DescriptorMemberParameters =>
  ({ total_millions: total, active_millions: active });

const modelMember = (format: ModelFormat, quantization: string, model_key: string | null,
                     parameters: DescriptorMemberParameters | null = null): DescriptorMemberView => ({
  member_key: { kind: "model", format, quantization }, selector: `AcmeLM-3B::${format}::${quantization}`,
  added_at: "2026-10-02T00:00:00Z", digest: `${format}-${quantization}`.padEnd(64, "0"),
  descriptor: { type: "mlx", source: "huggingface", org: "acme", repo_name: "AcmeLM-3B-MLX-4bit",
                revision: "0123456789abcdef0123456789abcdef01234567" },
  plan_toml: "{ type = 'mlx' }", model_key, parameters,
});

const modelRow = (name = "AcmeLM-3B"): DescriptorEntryListRow => ({
  ...row(name), descriptor_kind: "model", descriptor_types: ["gguf_text", "mlx"], kind: "hand-made",
  member_count: 2,
  member_summaries: [
    { member_key: { kind: "model", format: "gguf", quantization: "q4_0" }, selector: `${name}::gguf::q4_0` },
    { member_key: { kind: "model", format: "mlx", quantization: "4bit" }, selector: `${name}::mlx::4bit` },
  ],
  members: [modelMember("gguf", "q4_0", "acmelm-3b"), modelMember("mlx", "4bit", "acmelm-3b")],
});

describe("the global model catalog", () => {
  test("the Models tab lives in the query, and each tab keeps its own columns", () => {
    expect(catalogKindOf(new URLSearchParams("kind=model"))).toBe("model");
    expect(catalogKindOf(new URLSearchParams(""))).toBe("runtime");
    expect(catalogKindOf(new URLSearchParams("kind=other"))).toBe("runtime");
    expect(catalogTabs("model").map((tab) => [tab.label, tab.href, tab.current])).toEqual([
      ["Runtimes", catalogPath, false], ["Models", `${catalogPath}?kind=model`, true],
    ]);
    expect(CATALOG_TABS.model.columnsKey).not.toBe(CATALOG_TABS.runtime.columnsKey);
    expect(new URLSearchParams(catalogListQuery("", false, "model")).get("descriptor_kind")).toBe("model");
  });

  test("the model table lists format and quantization labels and filters by model type", () => {
    expect(modelCatalogTable.columns.map((column) => column.key)).not.toContain("offer_rule");
    expect(flavors(modelRow())).toBe("gguf::q4_0, mlx::4bit");
    const entries = [modelRow("AcmeLM-3B"), { ...modelRow("AcmeLM-1B"), descriptor_types: ["onnx_genai"] }];
    const view = (query: string) => tableView(modelCatalogTable,
      tableState(modelCatalogTable, new URLSearchParams(query)), entries).map((entry) => entry.name);
    expect(view("type=onnx_genai")).toEqual(["AcmeLM-1B"]);
  });

  test("members group by format in the order each format first appears", () => {
    const rows = modelMemberRows([
      modelMember("gguf", "q4_0", null), modelMember("mlx", "4bit", null), modelMember("gguf", "q8_0", null),
    ]);
    expect(rows.map((row) => `${row.format}::${row.quantization}`)).toEqual(["gguf::q4_0", "gguf::q8_0", "mlx::4bit"]);
  });

  test.each([
    { name: "every key agrees", members: [modelMember("gguf", "q4_0", "acmelm-3b", counts(3100)),
      modelMember("mlx", "4bit", "acmelm-3b", counts(3100))],
      expected: { model_key: "acmelm-3b", parameters: counts(3100) } },
    { name: "the keys differ", members: [modelMember("gguf", "q4_0", "acmelm-3b", counts(3100)),
      modelMember("openvino", "int4", "acmelm-3b-ov")], expected: null },
    { name: "one member has no key", members: [modelMember("gguf", "q4_0", "acmelm-3b"),
      modelMember("mlx", "4bit", null)], expected: null },
    { name: "the shared key has no row", members: [modelMember("gguf", "q4_0", "acmelm-3b"),
      modelMember("mlx", "4bit", "acmelm-3b")], expected: { model_key: "acmelm-3b", parameters: null } },
  ])("parameter counts show once only when $name", ({ members, expected }) => {
    expect(sharedParameters(members)).toEqual(expected);
  });

  test("parameter counts read as millions, with the active count of a sparse model", () => {
    expect(parameterText(counts(3100))).toBe("3,100M");
    expect(parameterText(counts(8300, 1500))).toBe("8,300M total, 1,500M active");
    expect(parameterText(null)).toBe("no parameter counts");
  });

  test("model synonyms keep model hits and drop runtime hits", () => {
    const entry = modelRow();
    const modelHit: DescriptorNameHit = { ...hit("AcmeLM-3B-q4"), descriptor_kind: "model",
      member_key: { kind: "model", format: "gguf", quantization: "q4_0" }, selector: "AcmeLM-3B-q4::gguf::q4_0" };
    const digest = entry.members![0]!.digest;
    expect(catalogSynonyms(entry, { names: { [digest]: [modelHit, hit("llamacpp-daily")] } }).map((s) => s.name))
      .toEqual(["AcmeLM-3B-q4"]);
  });
});

describe("a device's short list", () => {
  const list: DescriptorCatalogShortList = {
    items: [
      { name: "llamacpp-v0.7.0", release_time: "2026-10-15T00:00:00Z", descriptor_kind: "runtime",
        members: [row().members![0]!] },
      { name: "AcmeLM-3B", release_time: "2026-10-01T00:00:00Z", descriptor_kind: "model",
        members: [modelMember("gguf", "q4_0", null), modelMember("openvino", "int4", null)] },
    ],
    next_cursor: null, unreadable: 0,
    withheld: { member_count: 0, entry_count: 0, members: [] },
  };

  test("splits its items into runtime and model entries", () => {
    const sections = shortListSections(list);
    expect(sections.runtime.map((item) => item.name)).toEqual(["llamacpp-v0.7.0"]);
    expect(sections.model.map((item) => item.members.length)).toEqual([2]);
  });

  test.each([
    { member_count: 0, entry_count: 0, expected: null },
    { member_count: 1, entry_count: 0, expected: "1 member withheld" },
    { member_count: 3, entry_count: 1, expected: "3 members withheld; 1 entry offers no member" },
    { member_count: 4, entry_count: 2, expected: "4 members withheld; 2 entries offer no member" },
  ])("summarizes $member_count withheld members in $entry_count entries", ({ member_count, entry_count, expected }) => {
    expect(withheldSummary({ member_count, entry_count, members: [] })).toBe(expected);
  });
});
