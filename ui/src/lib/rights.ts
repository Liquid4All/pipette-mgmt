// What the reader may do, as `GET /api/v1/users/me` says the routes judge it.
// Drawn, not enforced: the route still decides.

import type { OwnUserView, Reach } from "./api-types";

/** Whether `reach` covers a record of organization `org_id`. */
export const reaches = (account: OwnUserView, reach: Reach, org_id: string | null | undefined) =>
  reach === "every" || (reach === "own" && !!org_id && org_id === account.org_id);

/** A permission form's ticked set after one box is ticked or cleared. */
export function toggled<T>(set: Set<T>, name: T, on: boolean): Set<T> {
  const next = new Set(set);
  if (on) next.add(name);
  else next.delete(name);
  return next;
}
