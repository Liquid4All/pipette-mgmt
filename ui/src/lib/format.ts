// The classic console's template filters, for the same cells.

import type { Counts, PlanView } from "./api-types";

/**
 * A timestamp as the classic console's `when` filter printed it: UTC, to the
 * second, or an em-rule when there is none. One it cannot read shows as it is.
 */
export function when(iso: string | null | undefined): string {
  if (!iso) return "—";
  const at = new Date(iso);
  return Number.isNaN(at.getTime()) ? iso : at.toISOString().slice(0, 19).replace("T", " ") + "Z";
}

/**
 * Milliseconds in at most two units, cut down to the whole second, the second
 * unit padded: `42s`, `3m 04s`, `2h 05m`. A duration below one second prints
 * as `0s`, and no duration prints as an em-rule. Unlike `span`, it keeps
 * seconds, because one device run often takes less than a minute.
 */
export function duration(ms: number | null): string {
  if (ms === null) return "—";
  const seconds = Math.floor(ms / 1000);
  const [hours, minutes] = [Math.floor(seconds / 3600), Math.floor(seconds / 60) % 60];
  const pad = (n: number) => String(n).padStart(2, "0");
  if (hours > 0) return `${hours}h ${pad(minutes)}m`;
  if (minutes > 0) return `${minutes}m ${pad(seconds % 60)}s`;
  return `${seconds}s`;
}

/** The path segment an organization is addressed by in the app. */
export const handle = (org: { slug: string; org_id: string }): string =>
  // `/app/orgs/new` is the create form, so an organization whose handle is `new` goes by its id.
  org.slug === "new" ? org.org_id : org.slug;

/** The jobs that are done: answered, a failure too, which leaves nothing to wait for. */
export const doneOf = (counts: { finished?: number; failed?: number }): number =>
  (counts.finished ?? 0) + (counts.failed ?? 0);

/**
 * A plan's counts as one line, the zeros other than `done` left out, the failed
 * among the done beside them. A plan no `queue-maintenance` pass has counted yet
 * says so, rather than `0 done (of 0)`.
 */
export const progress = (plan: Pick<PlanView, "progress_snapshot">): string =>
  progressOf(plan.progress_snapshot?.counts ?? null);

/** `progress` over the counts themselves, as a narrow plan row carries them. */
export function progressOf(counts: Counts | null): string {
  if (!counts) return "not computed yet";
  const failed = counts.failed > 0 ? `, ${counts.failed} failed` : "";
  const rest = ([["running", "running"], ["available", "queued"], ["cancelled", "cancelled"]] as const)
    .filter(([field]) => counts[field] > 0)
    .map(([field, word]) => ` · ${counts[field]} ${word}`)
    .join("");
  return `${doneOf(counts)} done${failed}${rest} (of ${counts.total})`;
}

/** `a` against `b` code unit by code unit, as Rust orders a `str`, where `localeCompare` folds case and punctuation. */
export const textOrder = (a: string, b: string): number => (a < b ? -1 : a > b ? 1 : 0);

/** `at` against `now`, in one floored unit: "5 min ago", "in 6 d", "just now" within a minute either way. */
export function relative(at: number, now: number): string {
  const gap = Math.abs(now - at);
  if (gap < 60_000) return "just now";
  const minutes = Math.floor(gap / 60_000);
  const amount = minutes < 60 ? `${minutes} min` : minutes < 48 * 60 ? `${Math.floor(minutes / 60)} h`
    : `${Math.floor(minutes / (24 * 60))} d`;
  return at <= now ? `${amount} ago` : `in ${amount}`;
}

/** An RFC 3339 time against `now`, as `relative` puts it; one that does not parse as given. */
export function ago(iso: string, now: number): string {
  const at = Date.parse(iso);
  return Number.isNaN(at) ? iso : relative(at, now);
}
