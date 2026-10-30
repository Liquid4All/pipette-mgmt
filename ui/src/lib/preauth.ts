// What a pre-auth key's mint form offers, for an account's keys and a device's alike.

import type { UserPreauthKeyView } from "./api-types";

/** What a listing draws of an outstanding key, an account's or a device's. */
export type PreauthKeyRow = Pick<UserPreauthKeyView, "key_id" | "expires_at" | "expired" | "note" | "created_at" | "minted_by">;

/** The expiry choices and default, as the classic forms offered them; the routes default to 90. */
export const EXPIRY_DAYS = [1, 7, 30, 90];
export const EXPIRY_DEFAULT = 7;

export const days = (n: number) => `${n} day${n === 1 ? "" : "s"}`;

/** Asked before revoking a pre-auth key. */
export const REVOKE_PREAUTH = (key_id: string) => ({
  title: `Revoke ${key_id}?`, text: "It registers nothing afterwards. A machine it already registered keeps its key.",
  action: "Revoke this pre-auth key", pending: "Revoking…",
});

/** A mint's shared body: its expiry always sent (the routes default to 90) and a blank note left out. */
export function mintBody(expires_in_days: number, note = ""): { expires_in_days: number; note?: string } {
  const trimmed = note.trim();
  return { expires_in_days, ...(trimmed ? { note: trimmed } : {}) };
}
