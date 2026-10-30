import { describe, expect, test } from "vitest";
import { IMPORT_INTERRUPTED_MS, IMPORT_STALE_MS, importResult, importStatus } from "../src/lib/catalog-import-status";
import type { CatalogImportRun, CatalogImportRuns } from "../src/lib/api-types";

const NOW = Date.parse("2026-10-28T18:00:00Z");
const run = (over: Partial<CatalogImportRun> = {}): CatalogImportRun => ({
  id: "run-1", started_at: "2026-10-28T17:00:00Z", finished_at: "2026-10-28T17:01:00Z",
  command: "import", source: "llamacpp", target_name: null, result: "success", outcome: "success",
  entries_created: 2, entries_hidden: 1, members_added: 4, unclassified_assets: [], error_summary: null,
  ...over,
});
const history = (items: CatalogImportRun[], last: string | null): CatalogImportRuns =>
  ({ items, last_successful_import_at: last });

describe("catalog importer status", () => {
  test("shows success, running, and an empty history", () => {
    expect(importStatus(history([run()], run().finished_at), NOW)).toMatchObject({ result: "success", stale: false });
    expect(importResult(run({ result: "running", outcome: "running", finished_at: null }), NOW)).toBe("running");
    expect(importStatus(history([], null), NOW)).toEqual({ latest: null, result: null, look: null, stale: true, attention: true });
  });

  test("reports a running run past the cutoff as interrupted", () => {
    const startedAt = (ms: number) => new Date(NOW - ms).toISOString();
    const running = { result: "running", outcome: "running", finished_at: null } as const;
    expect(importResult(run({ ...running, started_at: startedAt(IMPORT_INTERRUPTED_MS) }), NOW)).toBe("running");
    expect(importResult(run({ ...running, started_at: startedAt(IMPORT_INTERRUPTED_MS + 1) }), NOW)).toBe("interrupted");
  });

  test("passes a finished run's outcome through", () => {
    expect(importResult(run({ result: "error", outcome: "unclassified" }), NOW)).toBe("unclassified");
    expect(importResult(run({ result: "error", outcome: "failed" }), NOW)).toBe("failed");
  });

  test("takes the warning style for a problem result or a stale history", () => {
    const fresh = run().finished_at;
    const attention = (over: Partial<CatalogImportRun>, last: string | null = fresh) =>
      importStatus(history([run(over)], last), NOW).attention;
    expect(attention({})).toBe(false);
    expect(attention({ result: "running", outcome: "running", finished_at: null })).toBe(false);
    expect(attention({ result: "running", outcome: "running", finished_at: null,
      started_at: new Date(NOW - IMPORT_INTERRUPTED_MS - 1).toISOString() })).toBe(true);
    expect(attention({ result: "error", outcome: "unclassified" })).toBe(true);
    expect(attention({ result: "error", outcome: "failed" })).toBe(true);
    expect(attention({}, null)).toBe(true);
  });

  test("warns only past 36 hours since a successful import", () => {
    const atBoundary = new Date(NOW - IMPORT_STALE_MS).toISOString();
    expect(importStatus(history([run({ result: "error" })], atBoundary), NOW).stale).toBe(false);
    expect(importStatus(history([run({ result: "error" })], new Date(NOW - IMPORT_STALE_MS - 1).toISOString()), NOW).stale).toBe(true);
  });
});
