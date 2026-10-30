import { describe, expect, test } from "vitest";
import { rosterTable } from "../src/lib/accounts";
import { benchmarkTable } from "../src/lib/benchmarks";
import { campaignTable, progressText } from "../src/lib/campaigns";
import { deviceTable } from "../src/lib/fleet";
import { ORG_TABLE } from "../src/lib/orgs";
import { planTable } from "../src/lib/plans";
import { vaultTable } from "../src/lib/vault";
import {
  choicesOf, cleared, compareRaw, drawnColumns, everyPage, filtering, nextSort, sortBy, statePairs, tableState, tableView,
  toggledIn, withCursor, type CursorPage, type Picks, type TableSpec,
} from "../src/lib/table";
import { PickedColumns } from "../src/lib/picked.svelte";
import type { CampaignView, ClientListRow, OrgView, PlanView } from "../src/lib/api-types";
import { drawnKeys, stubStorage } from "./picks";

interface Row { id: string; name: string; size: number | null; tags: string[] }
const SPEC: TableSpec<Row> = {
  columns: [
    { key: "name", label: "name", raw: (row) => row.name },
    { key: "size", label: "size", raw: (row) => row.size },
  ],
  texts: [{ key: "q", label: "row", texts: (row) => [row.id, row.name] }],
  choices: [{ key: "tag", label: "tag", values: (row) => row.tags, order: ["b", "a"] }],
};
const ROWS: Row[] = [
  { id: "r1", name: "box 10", size: 3, tags: ["a"] },
  { id: "r2", name: "Box 9", size: null, tags: ["a", "b"] },
  { id: "r3", name: "crate", size: 12, tags: [] },
];
const view = (query: string) => tableView(SPEC, tableState(SPEC, new URLSearchParams(query)), ROWS).map((r) => r.id);

describe("a table read in the browser", () => {
  test("reads its state from the address, a sort by a missing column dropped, and writes it back", () => {
    const state = tableState(SPEC, new URLSearchParams("q=%20box%20&tag=a&tag=a&tag=&sort=-size&offset=50"));
    expect(state).toEqual({ texts: { q: "box" }, choices: { tag: ["a"] }, sort: "-size" });
    expect(statePairs(state)).toEqual([["q", "box"], ["tag", "a"], ["sort", "-size"]]);
    expect(tableState(SPEC, new URLSearchParams("sort=nope")).sort).toBe("");
    expect([filtering(state), filtering(cleared(state)), cleared(state).sort]).toEqual([true, false, "-size"]);
    expect(toggledIn(toggledIn(state, "tag", "b", true), "tag", "a", false).choices).toEqual({ tag: ["b"] });
  });

  test("keeps a row whose text holds the box's, case aside, and any of a select's choices", () => {
    expect(view("q=BOX")).toEqual(["r1", "r2"]);
    expect(view("tag=b")).toEqual(["r2"]);
    expect(view("tag=a&tag=b&q=crate")).toEqual([]);
    expect(view("")).toEqual(["r1", "r2", "r3"]);
  });

  test("sorts numbers by size and text with its numbers by size, none last either way", () => {
    expect(view("sort=name")).toEqual(["r2", "r1", "r3"]);
    expect(view("sort=-name")).toEqual(["r3", "r1", "r2"]);
    expect(view("sort=size")).toEqual(["r1", "r3", "r2"]);
    expect(view("sort=-size")).toEqual(["r3", "r1", "r2"]);
    expect([nextSort("", "size"), nextSort("size", "size"), nextSort("-size", "size"), nextSort("name", "size")])
      .toEqual(["size", "-size", "", "size"]);
    expect([compareRaw(null, 1, 1), compareRaw(null, 1, -1), compareRaw("", "", 1)]).toEqual([1, 1, 0]);
    expect(sortBy([{ v: 2 }, { v: 1 }], [{ key: "v", raw: (x) => x.v }], "")).toEqual([{ v: 2 }, { v: 1 }]);
  });

  test("offers each select's values counted, in the order it fixes, else by name", () => {
    expect(choicesOf(SPEC, ROWS, {}).tag).toEqual([{ value: "b", label: "b", count: 1 }, { value: "a", label: "a", count: 2 }]);
  });

  test("reads every page of a cursor listing, the names and unreadable records of all", async () => {
    const pages: Record<string, CursorPage<number>> = {
      "": { items: [1, 2], next_cursor: "c1", unreadable: 1, names: { a: "A" } },
      c1: { items: [3], next_cursor: null, unreadable: 2, names: { b: "B" } },
    };
    const asked: (string | null)[] = [];
    const whole = await everyPage(async (cursor) => {
      asked.push(cursor);
      return pages[cursor ?? ""]!;
    });
    expect(whole).toEqual({ items: [1, 2, 3], next_cursor: null, unreadable: 3, names: { a: "A", b: "B" } });
    expect(asked).toEqual([null, "c1"]);
    expect([withCursor("order=newest", "c 1"), withCursor("", "c"), withCursor("x=1", null)])
      .toEqual(["order=newest&cursor=c+1", "cursor=c", "x=1"]);
  });
});

describe("the columns picked", () => {
  const COLUMNS = [{ key: "a" }, { key: "b", shown: false }, { key: "c" }];

  test.each([
    ["no pick draws the defaults", null, ["a", "c"]],
    ["a column picked is drawn", { b: true }, ["a", "b", "c"]],
    ["a column let go of is hidden", { a: false }, ["c"]],
    ["a pick of a column the table lacks changes nothing", { z: false }, ["a", "c"]],
  ])("%s", (_, picks: Picks | null, keys: string[]) => {
    expect(drawnColumns(COLUMNS, picks).map((column) => column.key)).toEqual(keys);
  });

  test("a column named as a property every object has is drawn at its default", () => {
    expect(drawnColumns([{ key: "constructor", shown: false }], {}).map((column) => column.key)).toEqual([]);
  });

  test("a pick kept as a list of keys draws exactly those, and the next pick keeps each column's toggle", () => {
    const kept = stubStorage();
    kept.set("pick", JSON.stringify(["b"]));
    expect(drawnKeys("pick", COLUMNS)).toEqual(["b"]);
    new PickedColumns("pick", COLUMNS).pick("c", true);
    expect(JSON.parse(kept.get("pick") ?? "null")).toEqual({ a: false, b: true, c: true });
    expect(drawnKeys("pick", COLUMNS)).toEqual(["b", "c"]);
  });

  test.each(["not json", "3", '"a"', "null", '{"a":"no"}'])("storage that holds %s draws the defaults", (held) => {
    stubStorage().set("pick", held);
    expect(new PickedColumns("pick", COLUMNS).picks).toBeNull();
    expect(drawnKeys("pick", COLUMNS)).toEqual(["a", "c"]);
  });

  test("the default columns let go of every pick", () => {
    const kept = stubStorage();
    const picked = new PickedColumns("pick", COLUMNS);
    picked.pick("a", false);
    picked.reset();
    expect([picked.picks, kept.has("pick"), drawnKeys("pick", COLUMNS)]).toEqual([null, false, ["a", "c"]]);
  });
});

describe("the listings' tables", () => {
  test("a campaign's plan id and state are the server's: kept in links and cleared, never matched against a row", () => {
    const spec = campaignTable({}, false);
    const state = tableState(spec, new URLSearchParams("q=nightly&plan_id=plan-1&state=archived"));
    expect(state.texts).toEqual({ q: "nightly", plan_id: "plan-1", state: "archived" });
    expect(statePairs(state)).toEqual(expect.arrayContaining([["plan_id", "plan-1"], ["state", "archived"]]));
    expect(cleared(state).texts).toEqual({ q: "", plan_id: "", state: "" });
    const row = { campaign_id: "campaign_1", name: "nightly" } as CampaignView;
    expect(tableView(spec, state, [row])).toEqual([row]);
  });

  test("campaigns sort by progress and filter by status, a campaign not started included", () => {
    const row = (campaign_id: string, finished: number | null, status?: PlanView["status"]): CampaignView => ({
      campaign_id, name: campaign_id, created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-01T00:00:00Z",
      owner: { type: "org", org_id: "org_a" },
      ...(status ? { current_plan_id: `plan-${campaign_id}`, plan: { plan_id: `plan-${campaign_id}`, status,
        created_at: "2026-10-01T00:00:00Z", cancel_requested: false,
        counts: finished === null ? null : { finished, total: 10, available: 0, cancelled: 0, failed: 0, running: 0 } } }
        : {}),
    });
    const rows = [row("c1", 5, "active"), row("c2", null), row("c3", 10, "complete")];
    const spec = campaignTable({ org_a: "Acme" }, true);
    const ids = (query: string) => tableView(spec, tableState(spec, new URLSearchParams(query)), rows).map((r) => r.campaign_id);
    expect(ids("sort=-progress")).toEqual(["c3", "c1", "c2"]);
    // Progress reads as the plan's jobs finished of all; a campaign with no plan, or none counted, has none to print.
    expect(rows.map(progressText)).toEqual(["5/10", "—", "10/10"]);
    // One box searches the id, the name and the labels; a campaign has no id box of its own.
    const named = [{ ...row("campaign_1", 5, "active"), name: "nightly" }, { ...row("campaign_2", 5, "active"), name: "24b-perf" }];
    const found = (query: string) => tableView(spec, tableState(spec, new URLSearchParams(query)), named).map((r) => r.campaign_id);
    expect([found("q=NIGHT"), found("q=paign_2"), found("id=campaign_1")])
      .toEqual([["campaign_1"], ["campaign_2"], ["campaign_1", "campaign_2"]]);
    expect(spec.texts).toMatchObject([{ key: "q", label: "search", placeholder: "campaign id, name or label", wide: true }]);
    expect(ids("status=not_started")).toEqual(["c2"]);
    expect(choicesOf(spec, rows, {}).status!.map((c) => c.label)).toEqual(["active", "complete", "not started"]);
    expect(choicesOf(spec, rows, {}).owner).toEqual([{ value: "org_a", label: "Acme", count: 3 }]);
    // A choice the address holds that no row has is offered under its name, so it can be unchecked.
    const started = choicesOf(spec, [rows[0]!, rows[2]!], { status: ["active", "not_started"], owner: ["org_gone"] });
    expect(started.status!.map((c) => [c.label, c.count])).toEqual([["active", 1], ["complete", 1], ["not started", 0]]);
    expect(started.owner).toEqual([{ value: "org_a", label: "Acme", count: 2 }, { value: "org_gone", label: "org_gone", count: 0 }]);
  });

  test("plans, organizations and devices name their submitters, counts and makes", () => {
    const plan = (plan_id: string, submitted_by: string | null) => ({ plan_id, submitted_by, status: "active",
      created_at: "2026-10-01T00:00:00Z", progress_snapshot: null }) as unknown as PlanView;
    const plans = planTable({ u1: "amy@example.test" }, true);
    expect(tableView(plans, tableState(plans, new URLSearchParams("by=operator")), [plan("p1", "u1"), plan("p2", null)])
      .map((p) => p.plan_id)).toEqual(["p2"]);
    const org = (slug: string, member_count: number) => ({ slug, member_count }) as OrgView;
    expect(tableView(ORG_TABLE, tableState(ORG_TABLE, new URLSearchParams("sort=-member_count")),
                     [org("a", 2), org("b", 9)]).map((o) => o.slug)).toEqual(["b", "a"]);
    const device = (client_id: string, device_os_name: string) =>
      ({ client_id, device_os_name, permissions: [], suspended: false, reindex_pending: false }) as unknown as ClientListRow;
    const devices = deviceTable(false, {}, 0);
    expect(choicesOf(devices, [device("c1", "iOS"), device("c2", "iOS"), device("c3", "Android")], {}).os)
      .toEqual([{ value: "Android", label: "Android", count: 1 }, { value: "iOS", label: "iOS", count: 2 }]);
  });

  test("every listing opens on its record's id, which links to its page and has a filter box of its own but a campaign's", () => {
    const specs: Record<string, TableSpec<never>> = {
      campaign_id: campaignTable({}, true), plan_id: planTable({}, true), org_id: ORG_TABLE,
      client_id: deviceTable(true, {}, 0), user_id: rosterTable({ kind: "all" }, () => null),
      resource_id: vaultTable({}, "model"), benchmark_id: benchmarkTable(true),
    };
    for (const [id, spec] of Object.entries(specs)) {
      expect(spec.columns[0]).toMatchObject({ key: id, label: id });
      expect(spec.columns[0]!.href, id).toBeTypeOf("function");
      // A campaign's id is hidden until picked: its name links in its place.
      if (id !== "campaign_id") expect(spec.columns[0]!.shown, id).not.toBe(false);
      // A campaign's id is searched with its name, in the one box.
      if (id !== "campaign_id") expect(spec.texts![0], id).toMatchObject({ key: "id", label: id });
      // A sort and a pick go by key, so no two columns share one.
      expect(new Set(spec.columns.map((column) => column.key)).size, id).toBe(spec.columns.length);
    }
  });
});
