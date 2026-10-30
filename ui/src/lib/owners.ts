// An owner field's choices (pipette-httpapi's `owner_choices::OwnerChoices`), shared by every record's
// owner lookup.

import type { ClientOwner, OwnerMatch, PrincipalOwner } from "./api-types";

/** The choice `typed` names exactly, by id, handle or address, case aside, as `OwnerChoices::matching` puts it first. */
export function exactChoice(choices: OwnerMatch[], typed: string): OwnerMatch | null {
  const wanted = typed.trim().toLowerCase();
  if (!wanted) return null;
  return choices.find((c) => [c.value, c.name, c.label].some((field) => field.toLowerCase() === wanted)) ?? null;
}

/** The owner a picked choice names, typed as both owner `PUT`s take it, so either schema drifting fails the check. */
export const ownerOf = (choice: OwnerMatch): ClientOwner & PrincipalOwner =>
  choice.type === "org" ? { type: "org", org_id: choice.value } : { type: "user", user_id: choice.value };
