import type { CatalogImportOutcome, CatalogImportRun, CatalogImportRuns } from "./api-types";

export const IMPORT_STALE_HOURS = 36;
export const IMPORT_STALE_MS = IMPORT_STALE_HOURS * 60 * 60 * 1000;
/** A run that is still `running` after this time stopped before it recorded its end. */
export const IMPORT_INTERRUPTED_HOURS = 6;
export const IMPORT_INTERRUPTED_MS = IMPORT_INTERRUPTED_HOURS * 60 * 60 * 1000;

export type ImportResult = CatalogImportOutcome | "interrupted";

/** How the strip shows a result: its wording, the warning style, and the run's counts. */
export interface ResultLook { label: string; attention: boolean; counts: boolean }

export const IMPORT_RESULTS: Record<ImportResult, ResultLook> = {
  running: { label: "running", attention: false, counts: false },
  interrupted: { label: "did not finish", attention: true, counts: false },
  success: { label: "succeeded", attention: false, counts: true },
  unclassified: { label: "finished with unclassified assets", attention: true, counts: true },
  failed: { label: "failed", attention: true, counts: true },
};

/** The server's outcome, with a `running` run past the cutoff reported as `interrupted`. */
export function importResult(run: CatalogImportRun, now: number): ImportResult {
  if (run.outcome === "running" && now - Date.parse(run.started_at) > IMPORT_INTERRUPTED_MS) return "interrupted";
  return run.outcome;
}

/** The newest run, how to show it, and whether the strip takes the warning style. */
export function importStatus(runs: CatalogImportRuns, now: number): {
  latest: CatalogImportRun | null;
  result: ImportResult | null;
  look: ResultLook | null;
  stale: boolean;
  attention: boolean;
} {
  const latest = runs.items[0] ?? null;
  const last = runs.last_successful_import_at;
  const result = latest ? importResult(latest, now) : null;
  const look = result ? IMPORT_RESULTS[result] : null;
  const stale = last === null || now - Date.parse(last) > IMPORT_STALE_MS;
  return { latest, result, look, stale, attention: stale || (look?.attention ?? false) };
}
