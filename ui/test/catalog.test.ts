import { describe, expect, test } from "vitest";
import type {
  DescriptorCatalogShortList, DescriptorDigestNames, DescriptorEntryListRow, DescriptorEntryState,
  DescriptorMemberParameters, DescriptorMemberView, DescriptorNameHit, ModelFormat, Reach,
} from "../src/lib/api-types";
import {
  ANY_DEVICE_LABEL, anyDeviceText, catalogEntryPath, catalogKindOf, catalogListQuery, catalogPath, catalogRowId,
  catalogSynonyms, catalogTable, catalogTabSpecs, catalogTabs, flavors, formatGroups, formatText, mayEdit,
  modelCatalogTable, modelMemberRows, offerWarning, parameterText, runText, scopeLabel, sharedParameters,
  shortListSections, thisDeviceText, withheldSummary,
  type Reader,
} from "../src/lib/catalog";
import { relabelError, renameError, stateActions } from "../src/lib/catalog-entry";
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

const hit = (name: string, scope = "global", kind: DescriptorNameHit["kind"] = "daily",
             display_name = name): DescriptorNameHit => ({
  name, scope, kind, descriptor_kind: "runtime", state: "active", display_name,
  member_key: { kind: "runtime", flavor: "macos-arm64" }, selector: `${display_name}::macos-arm64`,
});

const reader = (edit_catalog: Reach = "own", org_id: string | null = "org_acme"): Reader => ({
  org_id, org_slug: org_id ? "acme" : null,
  may: { administer_fleet: "none", administer_roster: "none", create_orgs: false, edit_catalog, file_plans: false,
         grant: [], grant_service: [], read_fleet: false, read_plans: "none", read_roster: "none" },
});
const SCOPE = (item: { scope: string; display_name: string }) => scopeLabel(item, reader());

describe("the runtime catalog", () => {
  test("the server query reads every scope, limits the kind, and includes hidden only on request", () => {
    expect(Object.fromEntries(new URLSearchParams(catalogListQuery("  Llama  ", false, "runtime")))).toEqual({
      descriptor_kind: "runtime", state: "active,pinned", limit: "200", prefix: "Llama",
    });
    expect(new URLSearchParams(catalogListQuery("", true, "runtime")).get("state")).toBe("active,pinned,hidden");
  });

  test("the compact row gives flavors and warns about uncovered members", () => {
    expect(flavors(row())).toBe("macos-arm64, linux-x64");
    expect(offerWarning(row())).toBe("1 without an offer rule");
    expect(offerWarning({ ...row(), member_summaries: [row().member_summaries[0]!] })).toBeNull();
  });

  test("scope, kind, state, and runtime type filters keep API order until sorted", () => {
    const table = catalogTable(SCOPE);
    const entries = [
      { ...row("Z-newest"), kind: "daily" as const },
      { ...row("A-middle"), state: "hidden" as const },
      { ...row("B-oldest"), descriptor_types: ["apple_foundation"] },
      { ...row("C-org"), scope: "org_acme", kind: "hand-made" as const },
    ];
    const view = (query: string) => tableView(table,
      tableState(table, new URLSearchParams(query)), entries).map((entry) => entry.name);
    expect(view("")).toEqual(["Z-newest", "A-middle", "B-oldest", "C-org"]);
    expect(view("kind=daily")).toEqual(["Z-newest"]);
    expect(view("state=hidden")).toEqual(["A-middle"]);
    expect(view("type=apple_foundation")).toEqual(["B-oldest"]);
    expect(view("scope=acme")).toEqual(["C-org"]);
    expect(view("scope=global")).toEqual(["Z-newest", "A-middle", "B-oldest"]);
    expect(view("sort=name")).toEqual(["A-middle", "B-oldest", "C-org", "Z-newest"]);
  });

  test("a row shows and links the name as the reader sees it, keyed by scope and name", () => {
    const shadowed = { ...row("my-fork"), display_name: "global/my-fork" };
    const name = catalogTable(SCOPE).columns.find((column) => column.key === "name")!;
    expect(name.raw(shadowed)).toBe("global/my-fork");
    expect(name.href?.(shadowed)).toBe(catalogEntryPath("global/my-fork"));
    expect(name.href?.(row("unshadowed"))).toBe(catalogEntryPath("global/unshadowed"));
    expect(catalogEntryPath("global/my-fork")).toBe("/app/server/catalog/global%2Fmy-fork");
    expect(catalogRowId(shadowed)).not.toBe(catalogRowId({ ...row("my-fork"), scope: "org_acme" }));
  });

  test("synonyms come from every scope, distinct, without the entry itself, linked by display name", () => {
    const entry = row();
    const names: DescriptorDigestNames = { names: {
      ["a".repeat(64)]: [hit(entry.name), hit(entry.name, "org_acme", "hand-made"), hit("daily-name"),
                         hit("daily-name"), hit("their-fork", "org_beta", "hand-made", "beta/their-fork")],
      ["b".repeat(64)]: [hit("daily-name"), hit("hand-made", "global", "hand-made")],
    } };
    expect(catalogSynonyms(entry, names, SCOPE)).toEqual([
      { member: "macos-arm64", name: entry.name, scope: "acme", kind: "hand-made", href: catalogEntryPath(entry.name) },
      { member: "macos-arm64", name: "daily-name", scope: "global", kind: "daily", href: catalogEntryPath("global/daily-name") },
      { member: "macos-arm64", name: "beta/their-fork", scope: "beta", kind: "hand-made",
        href: catalogEntryPath("beta/their-fork") },
      { member: "linux-x64", name: "daily-name", scope: "global", kind: "daily", href: catalogEntryPath("global/daily-name") },
      { member: "linux-x64", name: "hand-made", scope: "global", kind: "hand-made", href: catalogEntryPath("global/hand-made") },
    ]);
  });
});

describe("the scope of an entry", () => {
  test.each([
    { name: "a global entry", scope: "global", display_name: "global/x", who: reader(), label: "global" },
    { name: "the reader's own org", scope: "org_acme", display_name: "x", who: reader(), label: "acme" },
    { name: "another org", scope: "org_beta", display_name: "beta/x", who: reader("every", null), label: "beta" },
    { name: "an org with no known slug", scope: "org_beta", display_name: "x", who: null, label: "org_beta" },
  ])("labels $name", ({ scope, display_name, who, label }) => {
    expect(scopeLabel({ scope, display_name }, who)).toBe(label);
  });

  test.each([
    { name: "a global entry, even to an editor of every org", scope: "global", who: reader("every"), may: false },
    { name: "an entry of the reader's org", scope: "org_acme", who: reader("own"), may: true },
    { name: "an entry of another org, with own reach", scope: "org_beta", who: reader("own"), may: false },
    { name: "an entry of another org, with every reach", scope: "org_beta", who: reader("every", null), may: true },
    { name: "an entry of the reader's org, without the permission", scope: "org_acme", who: reader("none"), may: false },
    { name: "an entry, signed out", scope: "org_acme", who: null, may: false },
  ])("gates $name", ({ scope, who, may }) => {
    expect(mayEdit(who, { scope })).toBe(may);
  });
});

describe("the controls of an org entry", () => {
  test.each([
    ["active", ["pin", "hide"]],
    ["pinned", ["unpin"]],
    ["hidden", ["unhide"]],
    ["tombstoned", []],
  ] as [DescriptorEntryState, string[]][])("a %s entry offers %j", (state, actions) => {
    expect(stateActions(state).map((offered) => offered.action)).toEqual(actions);
  });

  test.each([
    ["AcmeLM", "acme-lm", null],
    ["acmelm", "AcmeLM", null],
    ["AcmeLM", "AcmeLM", "Type a name that differs from the current name."],
    ["acme/lm", "AcmeLM", "Use only letters, digits, spaces, '.', '_' and '-' in the name."],
    ["", "AcmeLM", "Type a name."],
  ])("checks the new name %j of %j", (typed, current, error) => {
    expect(renameError(typed, current)).toBe(error);
  });

  test.each([
    ["q4_0", "f16_q4_0", null],
    ["q4_0", "q4_0", "Type a label that differs from the current label."],
    ["Q4_0", "f16_q4_0", "Use only lowercase letters, digits and '_' in the quantization label."],
  ])("checks the new label %j of %j", (typed, current, error) => {
    expect(relabelError(typed, current)).toBe(error);
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

describe("the model catalog", () => {
  test("the Models tab lives in the query, and each tab keeps its own columns", () => {
    expect(catalogKindOf(new URLSearchParams("kind=model"))).toBe("model");
    expect(catalogKindOf(new URLSearchParams(""))).toBe("runtime");
    expect(catalogKindOf(new URLSearchParams("kind=other"))).toBe("runtime");
    expect(catalogTabs("model").map((tab) => [tab.label, tab.href, tab.current])).toEqual([
      ["Runtimes", catalogPath, false], ["Models", `${catalogPath}?kind=model`, true],
    ]);
    const specs = catalogTabSpecs(SCOPE);
    expect(specs.model.columnsKey).not.toBe(specs.runtime.columnsKey);
    expect(new URLSearchParams(catalogListQuery("", false, "model")).get("descriptor_kind")).toBe("model");
  });

  test("the model table lists format and quantization labels and filters by model type", () => {
    const table = modelCatalogTable(SCOPE);
    expect(table.columns.map((column) => column.key)).not.toContain("offer_rule");
    expect(table.columns.map((column) => column.key)).toContain("scope");
    expect(formatText(modelRow())).toBe("gguf q4_0 · mlx 4bit");
    const entries = [modelRow("AcmeLM-3B"), { ...modelRow("AcmeLM-1B"), descriptor_types: ["onnx_genai"] },
                     { ...modelRow("AcmeLM-4B"), scope: "org_acme" }];
    const view = (query: string) => tableView(table,
      tableState(table, new URLSearchParams(query)), entries).map((entry) => entry.name);
    expect(view("type=onnx_genai")).toEqual(["AcmeLM-1B"]);
    expect(view("scope=acme")).toEqual(["AcmeLM-4B"]);
  });

  test("members sort by format, then by quantization, with numbers by size", () => {
    const rows = modelMemberRows([
      modelMember("mlx", "8bit", null), modelMember("gguf", "q8_0", null), modelMember("mlx", "16bit", null),
      modelMember("gguf", "q4_0", null), modelMember("mlx", "4bit", null),
    ]);
    expect(rows.map((row) => `${row.format}::${row.quantization}`))
      .toEqual(["gguf::q4_0", "gguf::q8_0", "mlx::4bit", "mlx::8bit", "mlx::16bit"]);
  });

  test("a list row groups its quantizations under each format, both in order", () => {
    const summary = (format: ModelFormat, quantization: string) =>
      ({ member_key: { kind: "model" as const, format, quantization }, selector: `AcmeLM-3B::${format}::${quantization}` });
    const listed = { ...modelRow(), member_summaries: [
      summary("mlx", "16bit"), summary("gguf", "q8_0"), summary("mlx", "4bit"), summary("gguf", "q4_0"),
    ] };
    expect(formatGroups(listed)).toEqual([
      { format: "gguf", quantizations: ["q4_0", "q8_0"] }, { format: "mlx", quantizations: ["4bit", "16bit"] },
    ]);
    expect(formatText(listed)).toBe("gguf q4_0 q8_0 · mlx 4bit 16bit");
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
    expect(catalogSynonyms(entry, { names: { [digest]: [modelHit, hit("llamacpp-daily")] } }, SCOPE)
      .map((s) => s.name))
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

  test("labels the evidence on every device as weaker", () => {
    expect(ANY_DEVICE_LABEL).toBe("any device (weaker evidence)");
  });

  test.each([
    { run: null, expected: "none" },
    { run: { at: "2026-10-30T12:00:00Z", client_version: null }, expected: "2026-10-30 12:00:00Z" },
    { run: { at: "2026-10-30T12:00:00Z", client_version: "2026.10.05-e925778" },
      expected: "2026-10-30 12:00:00Z, client 2026.10.05-e925778" },
  ])("writes the run $run with its client version", ({ run, expected }) => {
    expect(runText(run)).toBe(expected);
  });

  test("shows the resolved member's own runs on the device, and the runs on every device", () => {
    const member: DescriptorMemberView = {
      ...row().members![0]!,
      evidence: {
        this_device: { last_success: null, last_failure: { at: "2026-10-30T12:00:00Z", client_version: "v2" } },
        any_device: { last_success_at: "2026-10-29T08:00:00Z", devices_with_success: 2 },
      },
    };
    expect(thisDeviceText(member, "last_success")).toBe("none");
    expect(thisDeviceText(member, "last_failure")).toBe("2026-10-30 12:00:00Z, client v2");
    expect(anyDeviceText(member.evidence)).toBe("last success 2026-10-29 08:00:00Z; a success on 2 devices");
  });

  test.each([
    { evidence: undefined, device: "unavailable", any: null },
    { evidence: { any_device: { last_success_at: null, devices_with_success: 0 } }, device: "unavailable",
      any: "no success" },
    { evidence: { any_device: { last_success_at: "2026-10-29T08:00:00Z", devices_with_success: 1 } },
      device: "unavailable", any: "last success 2026-10-29 08:00:00Z; a success on 1 device" },
  ])("writes partial evidence $evidence", ({ evidence, device, any }) => {
    const member: DescriptorMemberView = { ...row().members![0]!, evidence };
    expect(thisDeviceText(member, "last_success")).toBe(device);
    expect(anyDeviceText(member.evidence)).toBe(any);
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
