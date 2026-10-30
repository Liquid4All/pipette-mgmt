import { afterEach, describe, expect, test, vi } from "vitest";
import { ApiError, benchmarkToml, putBenchmark, type Signer } from "../src/lib/api";
import {
  benchmarkRows, benchmarksEmpty, catalogOrg, createRefused, editRefused, entryOf, legacyCatalogFilter,
  newBenchmarkAction, pathBenchmarkId, typedBenchmarkId, writeOrgs,
} from "../src/lib/benchmarks";
import { administersOrg, ownOrg } from "../src/lib/orgs";
import { generateKey } from "../src/lib/keys";
import { match } from "../src/lib/routes";
import type { OrgBenchmarks, OwnUserView, UserPermission } from "../src/lib/api-types";
import { abilitiesOf } from "./abilities";

const acme = { org_id: "org_acme", slug: "acme" };
const beta = { org_id: "org_beta", slug: "beta" };
const odd = { org_id: "org_new", slug: "new" };

const reader = (permissions: UserPermission[], member = true): OwnUserView => ({
  user_id: "user_me", contact_email: "me@acme.example", account_type: "human", status: "active", created_at: "2026-10-01T00:00:00Z",
  permissions, keys: [], may: abilitiesOf(permissions, member ? "org_acme" : undefined),
  ...(member ? { org_id: "org_acme", org_slug: "acme" } : {}),
});

const catalog = (org_id: string, readable: string[], unreadable: string[] = []): OrgBenchmarks => ({
  org_id,
  benchmarks: readable.map((benchmark_id) => ({ benchmark_id, benchmark_type: "eval", org_id, parameter_max_tokens: 1 })),
  unreadable_benchmark_ids: unreadable,
});

/** `manage_any_orgs` as `GET /api/v1/users/me` resolves it, with what it implies. */
const EVERY: UserPermission[] = ["read_orgs", "read_any_orgs", "manage_orgs", "manage_any_orgs"];

afterEach(() => vi.unstubAllGlobals());

describe("benchmarks", () => {
  test("rows interleave what reads and what does not, by organization then id, each linking its page", () => {
    const rows = benchmarkRows([
      { org: beta, catalog: catalog("org_beta", ["b_1"]) },
      { org: acme, catalog: catalog("org_acme", ["Z_upper", "eval_b"], ["eval_a", "eval_c"]) },
    ]);
    expect(rows.map((row) => [row.org.slug, row.benchmark_id, row.benchmark_type])).toEqual([
      ["acme", "Z_upper", "eval"], ["acme", "eval_a", null], ["acme", "eval_b", "eval"], ["acme", "eval_c", null],
      ["beta", "b_1", "eval"],
    ]);
    expect(rows[1]?.href).toBe("/app/benchmarks/acme/eval_a");
    const dotted = benchmarkRows([{ org: odd, catalog: catalog("org_new", ["eval_ifbench_2026.06.1"]) }]);
    expect(dotted[0]?.href).toBe("/app/benchmarks/org_new/eval_ifbench_2026.06.1");
    expect(match(dotted[0]!.href)).toEqual(
      { page: "benchmark", org: "org_new", benchmark_id: "eval_ifbench_2026.06.1" });
  });

  test("an empty listing says why, as benchmarks.html words it", () => {
    expect(benchmarksEmpty({ kind: "all" }, 0))
      .toBe("You belong to no organization, so no private benchmark is yours to read.");
    expect(benchmarksEmpty({ kind: "org", slug: "acme" }, 1)).toBe("acme keeps no private benchmarks.");
    expect(benchmarksEmpty({ kind: "all" }, 2)).toBe("No private benchmarks you may see.");
  });

  test("an administrator of a catalog it reads is offered the form, narrowed by slug", () => {
    expect(newBenchmarkAction(reader(["manage_orgs"]), [acme], acme))
      .toEqual({ href: "/app/benchmarks/new?org=acme", label: "New benchmark" });
    expect(newBenchmarkAction(reader(["manage_orgs", "read_any_orgs"]), [beta, acme], null)?.href)
      .toBe("/app/benchmarks/new");
    // Visiting another's catalog, an administrator of its own is offered the form, which preselects nothing then.
    expect(newBenchmarkAction(reader(["manage_orgs", "read_any_orgs"]), [beta, acme], beta)?.href)
      .toBe("/app/benchmarks/new?org=beta");
    expect(newBenchmarkAction(reader(EVERY), [], null)).toBeNull();
    expect(newBenchmarkAction(reader(["read_any_orgs"]), [acme, beta], null)).toBeNull();
    expect(newBenchmarkAction(reader(["manage_orgs"], false), [], null)).toBeNull();
  });

  test("an organization's catalog is its administrators' or every organization's", () => {
    expect(administersOrg(reader(["manage_orgs"]), "org_acme")).toBe(true);
    expect(administersOrg(reader(["manage_orgs"]), "org_beta")).toBe(false);
    expect(administersOrg(reader(EVERY, false), "org_beta")).toBe(true);
    expect(administersOrg(reader(["read_any_orgs"]), "org_acme")).toBe(false);
    expect(ownOrg(reader([]))).toEqual(acme);
    expect(ownOrg(reader([], false))).toBeNull();
  });

  test("the form offers the catalogs the reader writes, or says why none", () => {
    expect(writeOrgs(reader(EVERY), [beta, acme])).toEqual({ orgs: [acme, beta], refusal: "" });
    expect(writeOrgs(reader(["manage_orgs"]), [beta])).toEqual({ orgs: [acme], refusal: "" });
    const nowhere = "you administer no organization's benchmarks";
    expect(writeOrgs(reader(["read_any_orgs"]), [beta])).toEqual({ orgs: [], refusal: nowhere });
    expect(writeOrgs(reader(["manage_orgs"], false), [])).toEqual({ orgs: [], refusal: nowhere });
    expect(writeOrgs(reader(EVERY, false), [])).toEqual({ orgs: [], refusal: "" });
  });

  test("the old org filter lands on that organization's listing, its paging kept", () => {
    expect(legacyCatalogFilter(new URLSearchParams("org=acme&offset=50&limit=10&status=pending")))
      .toBe("/app/orgs/acme/benchmarks?limit=10&offset=50");
    expect(legacyCatalogFilter(new URLSearchParams("org=org_acme"))).toBe("/app/orgs/org_acme/benchmarks");
    expect(legacyCatalogFilter(new URLSearchParams("offset=50"))).toBeNull();
  });

  test("an entry is found by key, one that does not read included", () => {
    const listed = catalog("org_acme", ["eval_ok"], ["eval_broken"]);
    expect(entryOf(listed, "eval_ok")).toEqual({ benchmark_type: "eval" });
    expect(entryOf(listed, "eval_broken")).toEqual({ benchmark_type: null });
    expect(() => entryOf(listed, "eval_gone")).toThrow("404: organization org_acme keeps no benchmark eval_gone");
  });

  test("an entry's address is held to BenchmarkId's grammar untrimmed, a 400 before any read", () => {
    expect(pathBenchmarkId("acme.dotted.v1")).toBe("acme.dotted.v1");
    expect(() => pathBenchmarkId(" eval_x")).toThrow(
      "400: invalid benchmark_id: benchmark id must start with [A-Za-z0-9], not ' '");
    expect(() => pathBenchmarkId("eval x")).toThrow(ApiError);
    expect(() => pathBenchmarkId("eval x")).toThrow("benchmark id contains ' '; allowed: [A-Za-z0-9][A-Za-z0-9_.-]*");
  });

  test("a catalog's organization is the one the reader holds, else the address's handle stands in", async () => {
    const listed = catalog("org_acme", []);
    expect(await catalogOrg(listed, "org_acme", async () => acme)).toEqual(acme);
    expect(await catalogOrg(listed, "acme", async () => null)).toEqual(acme);
    expect(await catalogOrg(listed, "acme", async () => { throw new ApiError(500, "x"); })).toEqual(acme);
  });

  test("the id is trimmed and held to BenchmarkId's grammar in its words", () => {
    expect(typedBenchmarkId("  eval_acme_v3.1-a \n")).toBe("eval_acme_v3.1-a");
    expect(() => typedBenchmarkId("  ")).toThrow("invalid benchmark_id: benchmark id must not be empty");
    expect(() => typedBenchmarkId("_x")).toThrow("benchmark id must start with [A-Za-z0-9], not '_'");
    expect(() => typedBenchmarkId("eval/x")).toThrow(
      "benchmark id contains '/'; allowed: [A-Za-z0-9][A-Za-z0-9_.-]*");
  });

  test("a failed precondition reads in the classic words, and any other refusal is left as it is", () => {
    const failed = new ApiError(412, "acme already keeps a benchmark eval_x");
    expect(createRefused(failed, acme, "eval_x"))
      .toEqual({ message: "acme already keeps a benchmark eval_x", kept: "/app/benchmarks/acme/eval_x" });
    expect(createRefused(new ApiError(409, "the public catalog already holds a benchmark x"), acme, "x")).toBeNull();
    expect(editRefused(failed, "eval_x"))
      .toBe("eval_x was deleted meanwhile, so nothing was written: create it again from New benchmark.");
    expect(editRefused(new ApiError(400, "bad"), "eval_x")).toBeNull();
  });
});

describe("the catalog file over the API", () => {
  const signer = async (): Promise<Signer> => {
    const key = await generateKey();
    return { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_x" };
  };

  test("an entry is asked for as TOML, and a refusal is thrown with its envelope's words", async () => {
    const sent: { path: string; init: RequestInit }[] = [];
    vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
      sent.push({ path, init });
      return sent.length === 1
        ? new Response('benchmark_type = "eval"\n', { status: 200 })
        : new Response(JSON.stringify({ error: "internal server error" }), { status: 500 });
    }));
    const by = await signer();
    expect(await benchmarkToml(by, "acme", "eval_2026.06.1")).toBe('benchmark_type = "eval"\n');
    expect(sent[0]?.path).toBe("/api/v1/orgs/acme/benchmarks/eval_2026.06.1");
    const headers = sent[0]?.init.headers as Record<string, string>;
    expect(headers.Accept).toBe("application/toml");
    expect(headers["Signature"]).toMatch(/^sig=:[A-Za-z0-9+/]+=*:$/);
    await expect(benchmarkToml(by, "acme", "eval_broken")).rejects.toThrow("500: internal server error");
  });

  test("a write sends the file and names whether it creates or replaces", async () => {
    const sent: RequestInit[] = [];
    vi.stubGlobal("fetch", vi.fn(async (_: string, init: RequestInit) => {
      sent.push(init);
      return sent.length < 3
        ? new Response("{}", { status: 200 })
        : new Response(JSON.stringify({ error: "acme already keeps a benchmark x" }), { status: 412 });
    }));
    const by = await signer();
    await putBenchmark(by, "org_acme", "x", "toml", true);
    await putBenchmark(by, "org_acme", "x", "toml", false);
    const [create, replace] = sent.map((init) => init.headers as Record<string, string>);
    expect([create?.["If-None-Match"], create?.["If-Match"], create?.["Content-Type"]])
      .toEqual(["*", undefined, "application/toml"]);
    expect([replace?.["If-None-Match"], replace?.["If-Match"]]).toEqual([undefined, "*"]);
    expect(sent[0]?.body).toBe("toml");
    const refused = putBenchmark(by, "org_acme", "x", "toml", true);
    await expect(refused).rejects.toBeInstanceOf(ApiError);
    await expect(refused).rejects.toMatchObject({ status: 412 });
  });
});
