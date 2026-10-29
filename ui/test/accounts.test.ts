import { describe, expect, test } from "vitest";
import {
  SIGN_OUT, USER_PERMISSIONS, amendment, createRefusal, grantable, keysBeforeFirstSignIn, legacyRosterFilter,
  lookupRefusal, mayCreate, mintable, newAccount, newAccountAction, outranked, permissionChoices, scopeChoices,
  readsRoster, retype, rosterEmpty, rosterRows, rosterTable, standing, tickedAtFirst, unknownStored, userTabOf, userTabs,
  type NewAccountForm,
} from "../src/lib/accounts";
import { tableState, tableView } from "../src/lib/table";
import schema from "../src/lib/api-schema.json";
import { carry, take } from "../src/lib/flash";
import { mintBody } from "../src/lib/preauth";
import type { OwnUserView, UserDetail, UserPermission } from "../src/lib/api-types";
import { abilitiesOf } from "./abilities";

const reader = (permissions: UserPermission[], org_id?: string, user_id = "user_me"): OwnUserView => ({
  user_id, contact_email: "me@acme.example", account_type: "human", status: "active", created_at: "2026-10-01T00:00:00Z",
  permissions, keys: [], may: abilitiesOf(permissions, org_id), ...(org_id ? { org_id, org_slug: "acme" } : {}),
});

const account = (fields: Partial<UserDetail>): UserDetail => ({
  user_id: "user_x", contact_email: "x@acme.example", account_type: "human", status: "active", created_at: "2026-10-01T00:00:00Z",
  permissions: [], stored_permissions: [], may_administer: false, may_grant: [], may_revoke: [], ...fields,
});

const ORG_ADMIN: UserPermission[] = ["manage_orgs", "read_orgs", "read_plans", "read_clients"];

describe("the vocabulary", () => {
  test("is every user permission, in the order the schema declares them", () => {
    const declared = schema.$defs.UserPermission.oneOf.map((each) => each.const);
    expect(USER_PERMISSIONS).toEqual(declared);
  });
});

describe("the roster", () => {
  const rows = [
    account({ user_id: "u3", contact_email: "zed@acme.example", org_id: "org_acme" }),
    account({ user_id: "u1", contact_email: "Bob@example.test", account_type: "human", status: "pending" }),
    account({ user_id: "u2", contact_email: "amy@acme.example", org_id: "org_acme", status: "disabled" }),
    account({ user_id: "u4", contact_email: "eve@globex.example", org_id: "org_globex", account_type: "service" }),
  ];
  const ids = (listed: { user_id: string }[]) => listed.map((row) => row.user_id);

  test("narrows to the branch, by address as the server orders them", () => {
    expect(ids(rosterRows(rows, { kind: "all" }))).toEqual(["u1", "u2", "u4", "u3"]);
    expect(ids(rosterRows(rows, { kind: "server" }))).toEqual(["u1"]);
    expect(ids(rosterRows(rows, { kind: "org", org_id: "org_acme", slug: "acme" }))).toEqual(["u2", "u3"]);
  });

  test("filters by type, status, organization and permission, and sorts by any column", () => {
    const spec = rosterTable({ kind: "all" }, (id) => (id === "org_acme" ? "acme" : null));
    const view = (query: string) =>
      ids(tableView(spec, tableState(spec, new URLSearchParams(query)), rosterRows(rows, { kind: "all" })));
    expect(view("type=service")).toEqual(["u4"]);
    expect(view("status=pending&status=disabled")).toEqual(["u1", "u2"]);
    expect(view("organization=org_acme")).toEqual(["u2", "u3"]);
    expect(view("organization=none")).toEqual(["u1"]);
    expect(view("q=ACME")).toEqual(["u2", "u3"]);
    expect(view("sort=-contact_email")).toEqual(["u3", "u4", "u1", "u2"]);
    expect(view("sort=organization")).toEqual(["u2", "u3", "u4", "u1"]);
  });

  test("says why it is empty, as users.html words it", () => {
    const acme = { kind: "org", org_id: "org_acme", slug: "acme" } as const;
    expect(rosterEmpty(acme)).toBe("Nobody belongs to acme.");
    expect(rosterEmpty({ kind: "server" })).toBe("Every account belongs to an organization.");
    expect(rosterEmpty({ kind: "all" })).toBe("No accounts you may see.");
  });

  test("sends a link carrying the old org filter to that organization's members", () => {
    expect(legacyRosterFilter(new URLSearchParams("org=acme&status=pending&offset=50&cursor=x")))
      .toBe("/app/orgs/acme/members?status=pending&offset=50");
    expect(legacyRosterFilter(new URLSearchParams("status=pending"))).toBeNull();
  });
});

describe("what an account's page offers", () => {
  test("the refusal in its place only with neither a record nor the plans it filed", () => {
    const refused = { page: null, refused: "user lacks the read_plans permission" };
    const unaffiliated = reader([]);
    expect(lookupRefusal(unaffiliated, { record: null }, refused)).toBe("user lacks the read_plans permission");
    expect(lookupRefusal(unaffiliated, { record: account({}) }, refused)).toBe("");
    expect(lookupRefusal(unaffiliated, { record: null }, { page: { items: [] }, refused: "" })).toBe("");
    expect(lookupRefusal(unaffiliated, null, refused)).toBe("");
    expect(lookupRefusal(unaffiliated, { record: null }, null)).toBe("");
  });

  test("no refusal for a roster reader, whose page says no account has the id", () => {
    const refused = { page: null, refused: "user lacks the read_plans permission" };
    expect(lookupRefusal(reader(["read_any_orgs"]), { record: null }, refused)).toBe("");
    expect(lookupRefusal(reader(["read_orgs"], "org_acme"), { record: null }, refused)).toBe("");
    // `read_orgs` outside any organization reads no roster.
    expect(lookupRefusal(reader(["read_orgs"]), { record: null }, refused)).toBe("user lacks the read_plans permission");
  });

  test("a create to an administrator of some roster", () => {
    expect(mayCreate(reader(["manage_any_orgs", "manage_orgs"]))).toBe(true);
    expect(mayCreate(reader(ORG_ADMIN, "org_acme"))).toBe(true);
    expect(mayCreate(reader(ORG_ADMIN))).toBe(false);
    expect(mayCreate(reader(["read_orgs"], "org_acme"))).toBe(false);
    expect(newAccountAction(reader(ORG_ADMIN, "org_acme"))).toEqual({ href: "/app/users/new", label: "New account" });
    expect(newAccountAction(reader(ORG_ADMIN))).toBeNull();
  });

  test("the create form, refused to a reader administering no roster", () => {
    expect(createRefusal(reader(["read_orgs"], "org_acme"))).toBe("you administer no organization's accounts");
    expect(createRefusal(reader(ORG_ADMIN))).toBe("you administer no organization's accounts");
    expect(createRefusal(reader(ORG_ADMIN, "org_acme"))).toBe("");
    expect(createRefusal(reader(["manage_any_orgs", "manage_orgs"]))).toBe("");
  });

  test("the record to a reader of some roster", () => {
    expect(readsRoster(reader(["read_any_orgs"]))).toBe(true);
    expect(readsRoster(reader(["read_orgs"], "org_acme"))).toBe(true);
    expect(readsRoster(reader(["read_orgs"]))).toBe(false);
    expect(readsRoster(reader(["read_plans"], "org_acme"))).toBe(false);
  });

  test("says an account is outranked when its roster is the reader's and the server refuses the writes", () => {
    const lead = reader(ORG_ADMIN, "org_acme");
    expect(outranked(lead, account({ org_id: "org_acme", may_administer: false }))).toBe(true);
    expect(outranked(lead, account({ org_id: "org_acme", may_administer: true }))).toBe(false);
    // Another roster is not the reader's to administer at all, so it is not outranked there.
    expect(outranked(lead, account({ org_id: "org_globex" }))).toBe(false);
    expect(outranked(reader(["read_orgs"], "org_acme"), account({ org_id: "org_acme" }))).toBe(false);
  });

  test("retypes a service back to a person, and a person to a service only before a sign-in", () => {
    expect(retype("service", false)).toMatchObject({ to: "human", label: "Make it a person's account" });
    expect(retype("human", true)).toMatchObject({ to: "service", label: "Make it a service account" });
    expect(retype("human", false)).toMatchObject({ to: null });
    expect(retype("human", false).note).toContain("users set-type");
    expect(retype("service", false, true)).toMatchObject({ to: null });
  });

  test("mints for a service account at any time, and for a person's only before a sign-in", () => {
    expect(mintable("service", false)).toBe(true);
    expect(mintable("human", true)).toBe(true);
    expect(mintable("human", false)).toBe(false);
  });

  test("each standing its one step, and never the reader's own disable", () => {
    expect(standing("pending", false)).toMatchObject({ to: "active", label: "Admit this account" });
    expect(standing("disabled", true)).toMatchObject({ to: "active", label: "Enable this account" });
    expect(standing("active", false)).toMatchObject({ to: "disabled", danger: true });
    expect(standing("active", true)).toMatchObject({ to: null, note: "Active. You cannot disable your own account." });
  });
});

describe("the permission form", () => {
  const admin = reader(ORG_ADMIN, "org_acme");
  // A record the reader administers: it grants what it holds, and revokes what is stored.
  const target = account({
    permissions: ["read_plans", "manage_orgs", "read_orgs", "submit_plans"],
    stored_permissions: ["read_plans", "manage_orgs", "submit_plans", "rescore_plans"],
    may_administer: true,
    may_grant: ["read_plans", "read_clients", "read_orgs", "manage_orgs"],
    may_revoke: ["read_plans", "manage_orgs", "submit_plans"],
  });
  const byName = (choices: ReturnType<typeof permissionChoices>) =>
    Object.fromEntries(choices.map((choice) => [choice.name, choice]));

  test("draws each name as the server says the reader may grant and revoke it", () => {
    const choices = byName(permissionChoices(target));
    expect(choices.read_plans).toEqual({ name: "read_plans", held: true, stored: true, offered: true, note: null });
    expect(choices.read_orgs).toMatchObject({ held: true, stored: false, offered: false, note: "implied" });
    // Stored and beyond the reader: still revocable.
    expect(choices.submit_plans).toMatchObject({ offered: true, note: null });
    expect(choices.read_clients).toMatchObject({ held: false, offered: true, note: null });
    expect(choices.cancel_plans).toMatchObject({ held: false, offered: false, note: "you may not grant it" });
  });

  test("keeps a stored name the server says the reader may not revoke", () => {
    const self = account({ ...target, user_id: "user_me", may_revoke: ["read_plans", "submit_plans"] });
    const choices = byName(permissionChoices(self));
    expect(choices.manage_orgs).toMatchObject({ offered: false, note: "kept" });
    expect(choices.read_plans).toMatchObject({ offered: true });
  });

  test("grants what is ticked and not held, and revokes what is stored and unticked", () => {
    const choices = permissionChoices(target);
    const ticked = tickedAtFirst(choices);
    expect([...ticked].sort()).toEqual(["manage_orgs", "read_plans", "submit_plans"]);
    expect(amendment(choices, ticked)).toBeNull();

    ticked.delete("read_plans");
    ticked.add("read_clients");
    expect(amendment(choices, ticked)).toEqual({ add: ["read_clients"], remove: ["read_plans"] });
  });

  test("asks nothing of a box it did not offer", () => {
    const choices = permissionChoices(target);
    // Ticked or not, a disabled box sends nothing: `read_orgs` is implied, `cancel_plans` out of reach.
    expect(amendment(choices, new Set([...tickedAtFirst(choices), "cancel_plans", "read_orgs"]))).toBeNull();
  });

  test("names the stored names this build does not know", () => {
    expect(unknownStored(target)).toEqual(["rescore_plans"]);
  });

  test("offers a new account the names the server says the reader may grant, in the vocabulary's order", () => {
    expect(grantable(admin, "human")).toEqual(["read_plans", "read_clients", "read_orgs", "manage_orgs"]);
    expect(grantable(admin, "service")).toEqual(["read_plans", "read_clients", "read_orgs"]);
    expect(grantable(admin, "service", true)).toEqual(["read_plans"]);
  });
});

describe("the new-account form", () => {
  const form = (fields: Partial<NewAccountForm>): NewAccountForm => ({
    contact_email: " hire@acme.example ", org_id: "", account_type: "human", permissions: ["read_plans"],
    campaign_scope: null, invite: true, expires_in_days: 7, ...fields,
  });

  test("creates with the address trimmed, and mints with its days always sent", () => {
    expect(newAccount(form({}))).toEqual({
      create: { contact_email: "hire@acme.example", account_type: "human", permissions: ["read_plans"] },
      mint: { expires_in_days: 7 },
    });
    expect(newAccount(form({ org_id: "org_acme", account_type: "service", invite: false }))).toEqual({
      create: { contact_email: "hire@acme.example", account_type: "service", permissions: ["read_plans"], org_id: "org_acme" },
      mint: null,
    });
  });

  test("sends a campaign reader's scope, and only a service account's", () => {
    const reader = newAccount(form({ account_type: "service", campaign_scope: ["campaign_a"] }));
    expect(reader.create.campaign_scope).toEqual(["campaign_a"]);
    expect(newAccount(form({})).create).not.toHaveProperty("campaign_scope");
    expect(() => newAccount(form({ campaign_scope: ["campaign_a"] }))).toThrow("a service account's");
    expect(() => newAccount(form({ account_type: "service", campaign_scope: [] }))).toThrow("choose the campaigns");
  });

  test("offers the campaigns the reader reads, and keeps a named one it does not", () => {
    const listed = [{ campaign_id: "campaign_a", name: "Nightly" }];
    expect(scopeChoices(listed, ["campaign_a", "campaign_z"])).toEqual([
      { value: "campaign_a", label: "Nightly" },
      { value: "campaign_z", label: "campaign_z (not readable, or deleted)" },
    ]);
  });

  test("refuses before anything is written", () => {
    expect(() => newAccount(form({ contact_email: "nobody" }))).toThrow("invalid contact_email");
    expect(() => newAccount(form({ contact_email: '"a/b"@acme.example' }))).toThrow("invalid contact_email");
    expect(() => newAccount(form({ expires_in_days: 91 }))).toThrow("expires_in_days must be between 1 and 90");
    expect(() => newAccount(form({ expires_in_days: 0 }))).toThrow("between 1 and 90");
    expect(newAccount(form({ expires_in_days: 0, invite: false })).mint).toBeNull();
  });

  test("a mint on the account's page leaves a blank note out", () => {
    expect(mintBody(30, "  ")).toEqual({ expires_in_days: 30 });
    expect(mintBody(1, " for the hire ")).toEqual({ expires_in_days: 1, note: "for the hire" });
  });
});

describe("a refusal carried to the next page", () => {
  test("is shown there once, and nowhere else", () => {
    carry("/app/users/user_x", "409: the account has signed in");
    expect(take("/app/users/user_x")).toBe("409: the account has signed in");
    expect(take("/app/users/user_x")).toBe("");
    carry("/app/users/user_x", "refused");
    expect(take("/app/users/user_y")).toBe("");
    expect(take("/app/users/user_x")).toBe("");
  });
});

describe("revoking a key", () => {
  test("the browser's own key asks before signing it out", () => {
    expect(SIGN_OUT.text).toContain("signs this browser out");
  });

  test("the keys held at the first Clerk sign-in are marked while the account holds them", () => {
    const key = (key_id: string) => ({ key_id, key_details: key_id, public_key: "00", added_at: "2026-10-01T00:00:00Z" });
    const keys = [key("userkey_planted"), key("userkey_browser")];
    expect(keysBeforeFirstSignIn({ keys })).toEqual([]);
    const first_clerk_sign_in = { at: "2026-10-02T00:00:00Z", prior_keys: ["userkey_planted", "userkey_revoked"] };
    expect(keysBeforeFirstSignIn({ keys, first_clerk_sign_in })).toEqual(["userkey_planted"]);
    expect(keysBeforeFirstSignIn({ keys: [key("userkey_browser")], first_clerk_sign_in })).toEqual([]);
  });
});

describe("an account's tabs", () => {
  const keys = (tabs: { key: string }[]) => tabs.map((tab) => tab.key);

  test("its administrator has its keys and Settings, set apart; any other reader its Overview and Plans", () => {
    expect(keys(userTabs("user_1", "overview", "administers")))
      .toEqual(["overview", "plans", "permissions", "keys", "preauth", "settings"]);
    expect(userTabs("user_1", "overview", "administers").find((tab) => tab.key === "settings")).toMatchObject(
      { href: "/app/users/user_1/settings", start: true });
    // A reader of the record reads its permissions; a reader of its filings alone reads those.
    expect(keys(userTabs("user_1", "plans", "record"))).toEqual(["overview", "plans", "permissions"]);
    expect(keys(userTabs("user_1", "plans", "filed"))).toEqual(["overview", "plans"]);
  });

  test("the open tab is listed whatever, so a link to it still says where the reader is", () => {
    expect(keys(userTabs("user_1", "settings", "filed"))).toEqual(["overview", "plans", "settings"]);
    expect(userTabs("user_1", "settings", "filed").filter((tab) => tab.current).map((tab) => tab.key)).toEqual(["settings"]);
  });

  test("each tab is the page its address names", () => {
    expect(userTabOf("user")).toBe("overview");
    expect(userTabOf("user-preauth")).toBe("preauth");
    expect(userTabOf("user-permissions")).toBe("permissions");
    expect(userTabs("user/1", "keys", "administers").find((tab) => tab.key === "keys")!.href).toBe("/app/users/user%2F1/keys");
  });
});
