import { describe, expect, test } from "vitest";
import { ownedEmpty, ownerBranch, principalOf } from "../src/lib/owned";
import { PickedColumns } from "../src/lib/picked.svelte";
import { tableState, tableView } from "../src/lib/table";
import { match, pathOf } from "../src/lib/routes";
import {
  VAULT_COLUMNS, VAULT_PAGES, artifactFiles, entryKind, jsonText, kindOfPage, listedUnder, storedCell,
  vaultIds, namedLabel, namedAs, vaultQuery, vaultTable, vaultTabs,
} from "../src/lib/vault";
import type { Model, Runtime, VaultArtifact, VaultResourceDetail } from "../src/lib/api-types";
import { drawnKeys, stubStorage } from "./picks";

const me = { type: "user", user_id: "user_me" } as const;
const colleague = { type: "user", user_id: "user_other" } as const;
const acme = { type: "org", org_id: "org_acme" } as const;
const rival = { type: "org", org_id: "org_rival" } as const;

describe("the vault", () => {
  test("each branch asks the newest first with the file state and the owners' names", () => {
    expect(vaultQuery({ kind: "all" }, null, "")).toBe("order=newest&object=true&names=true");
    expect(vaultQuery({ kind: "me" }, "vault_9", "")).toBe("order=newest&object=true&names=true&owner=mine&cursor=vault_9");
    expect(vaultQuery({ kind: "org", org_id: "org_acme", slug: "acme" }, null, "5"))
      .toBe("order=newest&object=true&names=true&owner=org&org_id=org_acme&limit=5");
  });

  test("an empty branch says so in the classic words", () => {
    expect(ownedEmpty({ kind: "me" }, "vault resources")).toBe("You own no vault resources.");
    expect(ownedEmpty({ kind: "org", org_id: "org_acme", slug: "acme" }, "vault resources"))
      .toBe("acme owns no vault resources you may see.");
    expect(ownedEmpty({ kind: "all" }, "vault resources")).toBe("No vault resources you may see.");
  });

  test("an upload's files are its roles, the rest of the artifact is not", () => {
    const artifact: VaultArtifact = {
      type: "gguf_vision",
      model: { resource_id: "vault_1", filename: "m.gguf", sha256: "a".repeat(64) },
      mmproj: { resource_id: "vault_2", filename: "p.gguf", sha256: "b".repeat(64) },
    };
    expect(artifactFiles(artifact).map(({ role, file }) => [role, file.filename]))
      .toEqual([["model", "m.gguf"], ["mmproj", "p.gguf"]]);
    expect(artifactFiles({ type: "ovms", flavor: "windows-x64" })).toEqual([]);
    expect(artifactFiles(null)).toEqual([]);
  });

  test("a vault opens on its models, its runtimes a tab under it", () => {
    for (const base of ["/app/vault", "/app/me/vault", "/app/orgs/acme/vault"]) {
      const tabs = vaultTabs(base, "runtime");
      expect(tabs.map((tab) => [tab.label, tab.href, tab.current])).toEqual([
        ["Models", base, false], ["Runtimes", `${base}/runtimes`, true],
      ]);
      expect(tabs.map((tab) => kindOfPage(match(tab.href).page))).toEqual(["model", "runtime"]);
      for (const tab of tabs) expect(pathOf(match(tab.href))).toBe(tab.href);
    }
    // The runtimes' page is not taken for a resource's.
    expect(match("/app/vault/vault_1").page).toBe("vault-resource");
  });

  test("models and runtimes filter apart, a build sorts by flavor, the descriptor and alias match", () => {
    const sha256 = "a".repeat(64);
    const runtimeRow = (id: string, flavor: string): VaultResourceDetail => ({
      resource_id: id, name: `llama.cpp b1 ${flavor}`, owner: acme, sha256, bytes: 1, created_at: "2026-10-09T00:00:00Z",
      may_administer: false,
      artifact: { type: "llamacpp_cli_stock_tools", flavor,
                  archive: { resource_id: id, filename: `llama-${flavor}.tar.gz`, sha256 } },
      entry: { runtime: { type: "llamacpp_cli_stock_tools", source: "vault_remote_archive",
                          url: `/vault/resources/${id}/content`, principal_id: "org_acme", flavor } as Runtime },
    });
    const model: VaultResourceDetail = {
      resource_id: "vault_9", name: "LFM2 Q4_0", owner: acme, sha256, bytes: 1, created_at: "2026-10-09T00:00:00Z",
      may_administer: false,
      artifact: { type: "gguf_text", model: { resource_id: "vault_9", filename: "LFM2-Q4_0.gguf", sha256 } },
      entry: { model: { type: "gguf_text", source: "vault_url", url: "/vault/resources/vault_9/content", sha256,
                        principal_id: "org_acme" } as Model },
      alias: { type: "gguf_text", source: "huggingface", org: "LiquidAI", repo_name: "LFM2-GGUF",
               path: "LFM2-Q4_0.gguf", revision: "b".repeat(40) } as Model,
    };
    const legacy: VaultResourceDetail = { ...model, resource_id: "vault_0", artifact: null, entry: undefined, alias: null };
    const rows = [runtimeRow("vault_1", "macos-arm64"), runtimeRow("vault_2", "android-arm64-v8a"), model, legacy];
    expect(rows.map((r) => entryKind(r.entry))).toEqual(["runtime", "runtime", "model", ""]);
    // A record from before uploads has no entry, and is listed with the models rather than nowhere.
    expect(rows.map(listedUnder)).toEqual(["runtime", "runtime", "model", "model"]);

    const view = (kind: "model" | "runtime", query: string) => {
      const spec = vaultTable({}, kind);
      return tableView(spec, tableState(spec, new URLSearchParams(query)), rows.filter((r) => listedUnder(r) === kind))
        .map((r) => r.resource_id);
    };
    expect(view("runtime", "sort=flavor")).toEqual(["vault_2", "vault_1"]);
    expect(view("runtime", "q=vault_2/content")).toEqual(["vault_2"]);
    // The id has its own box: the other one reads it only inside a descriptor.
    expect(view("runtime", "id=vault_2")).toEqual(["vault_2"]);
    expect(view("runtime", "id=llama")).toEqual([]);
    expect(view("runtime", "q=llama-macos-arm64.tar.gz")).toEqual(["vault_1"]);
    expect(view("model", "q=LFM2-GGUF")).toEqual(["vault_9"]);
    expect(view("model", "q=LFM2 Q4_0&id=vault_9")).toEqual(["vault_9"]);

    // Objects are their JSON as sent; the rest of a field is the field.
    const raw = (key: string, r: VaultResourceDetail) =>
      vaultTable({}, listedUnder(r)).columns.find((c) => c.key === key)!.raw(r);
    expect(raw("descriptor", rows[0]!)).toBe(JSON.stringify((rows[0]!.entry as { runtime: Runtime }).runtime));
    expect(raw("alias", model)).toBe(JSON.stringify(model.alias));
    expect(raw("artifact", model)).toBe(JSON.stringify(model.artifact));
    expect(raw("origin", model)).toBeNull();
    expect(raw("created_at", model)).toBe("2026-10-09T00:00:00Z");
    expect(raw("resource_id", model)).toBe("vault_9");
    expect(jsonText(undefined)).toBeNull();

    // A model has no flavor, a runtime no alias and no parameter counts.
    const keys = (kind: "model" | "runtime") => vaultTable({}, kind).columns.map((c) => c.key);
    expect(keys("model")).toEqual(["resource_id", "name", "alias", "owner", "total_millions", "active_millions", "type",
                                   "artifact", "sha256", "bytes", "object", "created_at", "origin", "descriptor"]);
    const counted = { ...model, model_params: { model_key: "vault_9", may_set: false, row: {
      model_key: "vault_9", total_millions: 8340, active_millions: 1500, note: null, updated_at: "2026-10-11T00:00:00Z",
      updated_by: null } } };
    expect([raw("total_millions", counted), raw("active_millions", counted)]).toEqual([8340, 1500]);
    expect(raw("total_millions", model)).toBeNull();
    expect(keys("runtime")).toEqual(["resource_id", "name", "owner", "type", "flavor", "artifact", "sha256", "bytes",
                                     "object", "created_at", "origin", "descriptor"]);
    expect(vaultTable({}, "model").choices!.map((c) => c.key)).toEqual(["type", "object"]);
    expect(vaultTable({}, "runtime").choices!.map((c) => c.key)).toEqual(["type", "flavor", "object"]);
    // Only every owner's vault offers the owners to pick among.
    expect(vaultTable({}, "model", true).choices!.map((c) => c.key)).toEqual(["type", "object", "owner"]);
  });

  test("a column let go on one tab stays on the other, which keeps what only it has", () => {
    stubStorage();
    const drawn = (kind: "model" | "runtime") => drawnKeys(VAULT_COLUMNS[kind], vaultTable({}, kind).columns);
    new PickedColumns(VAULT_COLUMNS.model, vaultTable({}, "model").columns).pick("sha256", false);
    expect(drawn("model")).not.toContain("sha256");
    expect(drawn("model")).toContain("alias");
    expect(drawn("runtime")).toEqual(vaultTable({}, "runtime").columns.map((c) => c.key));
  });

  test("a file's state reads as the listing's cell", () => {
    expect(storedCell({ state: "present" })).toEqual({ word: "present", fault: false, muted: false });
    expect(storedCell({ state: "resized", bytes: 3 })).toEqual({ word: "resized", fault: true, muted: false });
    expect(storedCell({ state: "missing" })).toEqual({ word: "missing", fault: true, muted: false });
    expect(storedCell({ state: "unknown" })).toEqual({ word: "unknown", fault: false, muted: true });
    expect(storedCell(undefined)).toEqual({ word: "unknown", fault: false, muted: true });
  });

  test("a resource's page sits under its owner's listing, as pages::owner_branch picks it", () => {
    const known = [{ org_id: "org_acme", slug: "acme" }, null];
    expect(ownerBranch({ user_id: "user_me" }, me, known, VAULT_PAGES))
      .toEqual({ href: "/app/me/vault", section: [{ label: "Me" }] });
    expect(ownerBranch({ user_id: "user_me" }, acme, known, VAULT_PAGES))
      .toEqual({ href: "/app/orgs/acme/vault", section: [{ label: "Organizations", href: "/app/orgs" }, { label: "acme", href: "/app/orgs/acme" }] });
    expect(ownerBranch({ user_id: "user_me" }, rival, known, VAULT_PAGES)).toEqual({ href: "/app/vault", section: [] });
    expect(ownerBranch({ user_id: "user_me" }, colleague, known, VAULT_PAGES)).toEqual({ href: "/app/vault", section: [] });
  });

  test("a descriptor's vault sources read under their labels, or the names the read gives their resources", () => {
    const at = (id: string) => `/vault/resources/${id}/content`;
    const vision = { type: "gguf_vision", source: "vault_url", model: at("vault_w"), mmproj: at("vault_p"),
                     principal_id: "org_acme" };
    const pair = { type: "mlx_speculative", target: { source: "vault_archive", url: at("vault_t") },
                   draft: { source: "vault_archive", url: at("vault_d") } };
    // As an object or as its stored text, each id once, and no other URL.
    expect(vaultIds(vision)).toEqual(["vault_w", "vault_p"]);
    expect(vaultIds(JSON.stringify(pair))).toEqual(["vault_t", "vault_d"]);
    expect(vaultIds({ type: "gguf_text", source: "url", url: `https://mgmt.example${at("vault_x")}` })).toEqual([]);
    expect(vaultIds(null)).toEqual([]);
    // A label is text, whatever it spells.
    expect(vaultIds({ ...vision, custom_label: at("vault_x") })).toEqual(["vault_w", "vault_p"]);
    // The files of one upload share its name, which is drawn once, linking to the first.
    const names = { vault_w: "acme VL q4", vault_p: "acme VL q4", vault_t: "acme target" };
    expect(namedAs(vision, names)).toEqual([{ id: "vault_w", name: "acme VL q4" }]);
    // A resource outside the reader's reach has no name, and the descriptor stands alone.
    expect(namedAs(pair, names)).toEqual([{ id: "vault_t", name: "acme target" }]);
    expect(namedAs(vision, {})).toEqual([]);
    expect(namedLabel(JSON.stringify(vision), names)).toBe(`acme VL q4 ${JSON.stringify(vision)}`);
    expect(namedLabel("{}", names)).toBe("{}");
    // A source's own `custom_label` needs no name from the read, and stands over one. Both halves of a pair
    // carry their upload's, drawn once.
    const labelled = { ...vision, custom_label: "VL preview" };
    expect(namedAs(JSON.stringify(labelled), {})).toEqual([{ id: "vault_w", name: "VL preview" }]);
    expect(namedAs(labelled, names)).toEqual([{ id: "vault_w", name: "VL preview" }]);
    const labelledPair = { ...pair, target: { ...pair.target, custom_label: "acme pair" },
                           draft: { ...pair.draft, custom_label: "acme pair" } };
    expect(namedAs(labelledPair, names)).toEqual([{ id: "vault_t", name: "acme pair" }]);
    // The label of a source that names no vault path, and text that is no JSON, read as nothing.
    expect(namedAs({ type: "mlx", source: "huggingface", custom_label: "Tiny MLX" }, names)).toEqual([]);
    expect(namedAs("not json", names)).toEqual([]);
  });

  test("a record from before uploads names its owner as the principal", () => {
    expect(principalOf(acme)).toBe("org_acme");
    expect(principalOf(me)).toBe("user_me");
  });
});
