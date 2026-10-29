import { describe, expect, test } from "vitest";
import {
  ACTIVE_WINDOW_MS, activity, deviceTable, emptyNotice, fleetQuery, needsRequires, pagePairs, readFilters, standing, throughOwners, type DeviceBranch,
} from "../src/lib/fleet";
import type { ClientListRow } from "../src/lib/api-types";

const read = (search: string) => readFilters(new URLSearchParams(search));
const server: DeviceBranch = { kind: "server" };
const unclaimedQuery = (filters: ReturnType<typeof read>, cursor: string | null) => fleetQuery(server, filters, cursor);
const acme = (count: number | null): DeviceBranch => ({ kind: "org", org_id: "org_1", slug: "acme", count });

describe("fleet filters", () => {
  test("an untouched status reads approved, and an empty one every status", () => {
    expect(unclaimedQuery(read(""), null)).toBe("status=approved&owned=false");
    expect(unclaimedQuery(read("status="), null)).toBe("owned=false");
    expect(unclaimedQuery(read("status=pending"), null)).toBe("status=pending&owned=false");
  });

  test("the page keeps a cleared status apart from the default", () => {
    expect(pagePairs(read("status="))).toEqual([["status", ""], ["limit", ""]]);
    expect(pagePairs(read(""))).toEqual([["status", "approved"], ["limit", ""]]);
  });

  test("a page size the address gives goes to the route and stays on the page's links", () => {
    const sized = read("status=pending&tag=rack&limit=500");
    expect(fleetQuery({ kind: "all" }, sized, "ev1_x")).toBe("status=pending&tag=rack&limit=500&cursor=ev1_x");
    expect(pagePairs(sized)).toEqual([["status", "pending"], ["tag", "rack"], ["limit", "500"]]);
    expect(fleetQuery({ kind: "me" }, read(""), null)).toBe("status=approved&owner=mine");
  });

  test("each occurrence stays its own pair, and a blank one is dropped", () => {
    const filters = read("requires=os:ios&any_of=gpu:a17pro+gpu:a18pro&any_of=%20&any_of=runtime:mlx&tag=rack");
    expect(unclaimedQuery(filters, "client_x")).toBe(
      "status=approved&owned=false&tag=rack&requires=os%3Aios&any_of=gpu%3Aa17pro+gpu%3Aa18pro" +
      "&any_of=runtime%3Amlx&cursor=client_x");
  });

  test("a narrowing clause without requires matches nobody", () => {
    expect(needsRequires(read("excludes=gpu:a17pro"))).toBe(true);
    expect(needsRequires(read("excludes=gpu:a17pro&requires=os:ios"))).toBe(false);
    expect(needsRequires(read("tag=rack&requires=%20"))).toBe(false);
  });

  test("each branch narrows the query its own way", () => {
    expect(fleetQuery({ kind: "all" }, read(""), null)).toBe("status=approved");
    expect(fleetQuery({ kind: "me" }, read("status="), null)).toBe("owner=mine");
    expect(fleetQuery(acme(null), read("tag=rack"), "ev1_x")).toBe("status=approved&org_id=org_1&tag=rack&cursor=ev1_x");
  });

  test("an empty page says why, in its branch's words", () => {
    const none = read("status=");
    expect(emptyNotice({ kind: "all" }, none, null)).toBe("No clients registered.");
    expect(emptyNotice({ kind: "all" }, read(""), null)).toBe("No client is approved.");
    expect(emptyNotice({ kind: "me" }, none, null)).toBe("You own no devices.");
    expect(emptyNotice({ kind: "me" }, read("status=pending"), null)).toBe("You own no pending devices.");
    expect(emptyNotice(acme(null), none, null)).toBe("acme owns no devices, directly or through a member.");
    expect(emptyNotice(acme(2), read(""), null)).toBe("acme owns no approved devices.");
    expect(emptyNotice(server, none, null)).toBe("Every device has an owner.");
    expect(emptyNotice(server, read(""), null)).toBe("No unclaimed device is approved.");
  });

  test("a filter, a cursor or an empty organization outranks the branch's own words", () => {
    expect(emptyNotice({ kind: "me" }, read("tag=rack"), null)).toBe("No clients match these filters.");
    expect(emptyNotice(server, read("tag=rack"), null)).toBe("No unclaimed devices match these filters.");
    expect(emptyNotice({ kind: "all" }, read(""), "ev1_x")).toBe("No clients at this position. Go back to the first page.");
    expect(emptyNotice(acme(0), read("tag=rack"), "ev1_x")).toBe("acme owns no devices, directly or through a member.");
    expect(emptyNotice(acme(0), read("excludes=gpu:a17pro"), null)).toBe(
      "Capability filters need at least one requires flag. Other clauses only narrow the selection.");
  });
});

describe("fleet rows", () => {
  const row = (fields: Partial<ClientListRow>): ClientListRow => ({
    client_details: "", client_id: "client_1", contact_email: "", derived_capabilities: [], last_seen_at: null, backlog: null,
    organization: "", permissions: [], public_key: "", registered_at: "", reindex_pending: false, status: "approved",
    suspended: false, tags: [], ...fields,
  });

  test("a column is a field of the record, a list its JSON and an absent one nothing", () => {
    const raw = (key: string, fields: Partial<ClientListRow>) =>
      deviceTable(false, {}, 0).columns.find((column) => column.key === key)!.raw(row(fields));
    expect(raw("device_os_name", { device_os_name: "iOS" })).toBe("iOS");
    expect(raw("device_chip_model", {})).toBeNull();
    expect(raw("permissions", { permissions: ["perform_work"] })).toBe('["perform_work"]');
    expect(raw("tags", {})).toBeNull();
    // What a row is owned through is drawn only under an organization.
    expect(deviceTable(false, {}, 0).columns.map((column) => column.key)).not.toContain("owned");
    expect(deviceTable(true, {}, 0).columns.map((column) => column.key)).toContain("owned");
  });

  test("standing names what the claim path passes over, else ok", () => {
    expect(standing(row({}))).toBe("ok");
    expect(standing(row({ suspended: true, reindex_pending: true }))).toBe("suspended, reindexing");
  });

  test("activity is active through the window, inactive after it, and never without a last-seen time", () => {
    const NOW = Date.parse("2026-10-14T12:00:00Z");
    const seenAgo = (ms: number) => row({ last_seen_at: new Date(NOW - ms).toISOString() });
    expect(activity(seenAgo(9 * 60_000 + 59_000), NOW)).toBe("active");
    expect(activity(seenAgo(ACTIVE_WINDOW_MS), NOW)).toBe("active");
    expect(activity(seenAgo(10 * 60_000 + 1_000), NOW)).toBe("inactive");
    expect(activity(row({}), NOW)).toBe("never");
    // The column and the filter measure against the time the table was built for.
    const spec = deviceTable(false, {}, NOW);
    expect(spec.columns.find((column) => column.key === "activity")!.raw(seenAgo(0))).toBe("active");
    expect(spec.choices!.find((choice) => choice.key === "activity")!.values(row({}))).toEqual(["never"]);
  });

  test("the accounts a page is owned through are named once each", () => {
    const by = (user_id: string) => row({ owner: { type: "user", user_id } });
    const rows = [by("user_a"), row({ owner: { type: "org", org_id: "org_1" } }), by("user_b"), by("user_a"), row({})];
    expect(throughOwners(rows)).toEqual(["user_a", "user_b"]);
  });
});
