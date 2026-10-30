// What `GET /api/v1/users/me` answers in `may` for a set of names, so a test
// states a reader by the names it holds. The server's rules, restated for
// fixtures only: the app itself reads `may`.

import type { Reach, UserAbilities, UserPermission } from "../src/lib/api-types";

const IMPLIED: Partial<Record<UserPermission, UserPermission[]>> = {
  submit_any_plans: ["submit_plans"],
  submit_plans: ["read_plans"],
  read_any_plans: ["read_plans"],
  cancel_any_plans: ["cancel_plans"],
  read_any_clients: ["read_clients"],
  manage_clients: ["read_clients"],
  manage_any_clients: ["manage_clients"],
  manage_orgs: ["read_orgs"],
  read_any_orgs: ["read_orgs"],
  manage_any_orgs: ["manage_orgs", "read_any_orgs"],
  edit_any_catalog: ["edit_catalog"],
};

// `UserPermission::is_admin`: what a service account never holds.
const ADMIN: UserPermission[] = [
  "manage_orgs", "manage_clients", "submit_any_plans", "read_any_plans", "cancel_any_plans",
  "read_any_clients", "read_any_orgs", "manage_any_orgs", "manage_any_clients",
  "edit_any_catalog",
];

const resolved = (names: UserPermission[]): Set<UserPermission> => {
  const held = new Set<UserPermission>();
  const visit = (name: UserPermission) => {
    if (held.has(name)) return;
    held.add(name);
    (IMPLIED[name] ?? []).forEach(visit);
  };
  names.forEach(visit);
  return held;
};

export function abilitiesOf(names: UserPermission[], org_id?: string): UserAbilities {
  const held = resolved(names);
  const reach = (bare: UserPermission, any: UserPermission): Reach =>
    held.has(any) ? "every" : held.has(bare) && org_id ? "own" : "none";
  return {
    read_roster: reach("read_orgs", "read_any_orgs"),
    administer_roster: reach("manage_orgs", "manage_any_orgs"),
    read_fleet: held.has("read_clients"),
    administer_fleet: reach("manage_clients", "manage_any_clients"),
    edit_catalog: reach("edit_catalog", "edit_any_catalog"),
    read_plans: !held.has("read_plans") ? "none" : held.has("read_any_plans") ? "every" : "own",
    file_plans: held.has("submit_plans"),
    create_orgs: held.has("manage_any_orgs"),
    grant: [...held],
    grant_service: [...held].filter((name) => !ADMIN.includes(name)),
  };
}
