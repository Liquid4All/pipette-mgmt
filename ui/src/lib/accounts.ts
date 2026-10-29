// The account pages (`users.html`, `user.html`, `new_user.html`): which roster
// a listing draws, what an account's page may offer its reader, and what its
// forms send. Each gate reads what the server says the reader may do; the routes still decide.

import { textOrder } from "./format";
import { administersOrg } from "./orgs";
import { EXPIRY_DAYS, mintBody } from "./preauth";
import type { BarTab } from "./bar";
import { legacyOrgFilter, pathOf, userLink, type Page } from "./routes";
import type { Choice, TableSpec } from "./table";
import type {
  AmendUserPermissionsRequest, CreateUserRequest, MintUserPreauthRequest, OwnUserView, UserAccountType, UserDetail, UserKey, UserPermission,
  UserStatus, UserView,
} from "./api-types";

/** Every user permission, in `UserPermission`'s declaration order, which the forms draw. */
const VOCABULARY: Record<UserPermission, true> = {
  submit_plans: true, submit_any_plans: true, read_plans: true, read_any_plans: true,
  cancel_plans: true, cancel_any_plans: true, read_clients: true, read_any_clients: true, read_orgs: true,
  read_any_orgs: true, manage_orgs: true, manage_any_orgs: true, manage_clients: true, manage_any_clients: true,
  edit_catalog: true, edit_any_catalog: true,
};

export const USER_PERMISSIONS = Object.keys(VOCABULARY) as UserPermission[];

const known = (name: string): name is UserPermission => Object.hasOwn(VOCABULARY, name);

/** Which accounts a roster listing draws (the classic `Branch`). */
export type RosterBranch =
  | { kind: "all" }
  | { kind: "server" }
  | { kind: "org"; org_id: string; slug: string };

/** The branch's accounts, by address, as the classic `accounts::browse` listed them. */
export const rosterRows = (rows: UserView[], branch: RosterBranch): UserView[] =>
  rows
    .filter((row) => branch.kind === "all" || (branch.kind === "server" ? !row.org_id : row.org_id === branch.org_id))
    .sort((a, b) => textOrder(a.contact_email, b.contact_email));

const STATUSES: UserStatus[] = ["active", "pending", "disabled"];
const ACCOUNT_TYPES: UserAccountType[] = ["human", "service"];

/** The roster's columns and filters; `orgOf` names an organization where the branch lists every one. */
export function rosterTable(branch: RosterBranch, orgOf: (org_id: string) => string | null): TableSpec<UserView> {
  const org = (row: UserView) => (row.org_id ? orgOf(row.org_id) ?? row.org_id : null);
  const all = branch.kind === "all";
  return {
    columns: [
      { key: "user_id", label: "user_id", raw: (row) => row.user_id, href: (row) => userLink(row.user_id) },
      { key: "contact_email", label: "contact_email", raw: (row) => row.contact_email },
      { key: "account_type", label: "account_type", raw: (row) => row.account_type },
      { key: "status", label: "status", raw: (row) => row.status, as: "state" },
      ...(all ? [{ key: "organization", label: "organization", raw: org }] : []),
      { key: "permissions", label: "permissions", as: "json" as const,
        raw: (row) => (row.permissions.length ? JSON.stringify(row.permissions) : null) },
      { key: "created_at", label: "created_at", raw: (row) => row.created_at, as: "time" },
    ],
    texts: [
      { key: "id", label: "user_id", texts: (row) => [row.user_id] },
      { key: "q", label: "contact_email", texts: (row) => [row.contact_email] },
    ],
    choices: [
      { key: "type", label: "account_type", values: (row) => [row.account_type], order: ACCOUNT_TYPES },
      { key: "status", label: "status", values: (row) => [row.status], order: STATUSES },
      ...(all ? [{ key: "organization", label: "organization", values: (row: UserView) => [row.org_id ?? "none"],
                   name: (id: string) => (id === "none" ? "none" : orgOf(id) ?? id) }] : []),
      { key: "permission", label: "permission", values: (row) => row.permissions },
    ],
  };
}

/** What an empty listing says, as `users.html` words it. */
export function rosterEmpty(branch: RosterBranch): string {
  switch (branch.kind) {
    case "org": return `Nobody belongs to ${branch.slug}.`;
    case "server": return "Every account belongs to an organization.";
    case "all": return "No accounts you may see.";
  }
}

/** Where a link carrying the old `org` filter lands (the classic `users`): that organization's members, the other filters kept. */
export const legacyRosterFilter = (query: URLSearchParams) =>
  legacyOrgFilter(query, "org-members", ["status", "limit", "offset"]);

/** An administrator of some roster, whom `POST /api/v1/users` admits. */
export const mayCreate = (reader: OwnUserView) => reader.may.administer_roster !== "none";

/** A listing's "New account" action, for a reader who may create one. */
export const newAccountAction = (reader: OwnUserView) =>
  mayCreate(reader) ? { href: "/app/users/new", label: "New account" } : null;

/** Why the new-account form is refused, or empty when it is not. */
export const createRefusal = (reader: OwnUserView): string =>
  mayCreate(reader) ? "" : "you administer no organization's accounts";

/** Whether the reader reads some roster, so asks for a record at all. */
export const readsRoster = (reader: OwnUserView) => reader.may.read_roster !== "none";

/**
 * The page in place of an account a reader with no roster read cannot list plans for,
 * as the classic `user_context` refused it; "" until both reads settle.
 */
export const lookupRefusal = (reader: OwnUserView, drawn: { record: object | null } | null,
                              filed: { page: object | null; refused: string } | null): string =>
  !readsRoster(reader) && drawn && !drawn.record && filed && !filed.page ? filed.refused : "";

/** Whether the reader administers the roster `target` is on and its rank refuses the writes. */
export const outranked = (reader: OwnUserView, target: UserDetail) =>
  administersOrg(reader, target.org_id) && !target.may_administer;

/** What the reader may grant a new account of `type`, in order, as the route's ceiling and the type allow. */
export const grantable = (reader: OwnUserView, type: UserAccountType, campaignReader = false) => {
  const offered = type === "service" ? reader.may.grant_service : reader.may.grant;
  // A campaign reader holds `read_plans` alone.
  return USER_PERMISSIONS.filter((name) => offered.includes(name) && (!campaignReader || name === "read_plans"));
};

/** One box of the permission form, as the classic `PermissionChoice::new` drew it. */
export interface PermissionChoice {
  name: UserPermission;
  /** In the resolved set: drawn ticked. */
  held: boolean;
  /** In the stored set: only such a name is revoked by an untick. */
  stored: boolean;
  /** The reader may change it: drawn enabled. */
  offered: boolean;
  note: string | null;
}

/** Every name's box for `target`, as the server says the reader may grant and revoke it. */
export function permissionChoices(target: UserDetail): PermissionChoice[] {
  return USER_PERMISSIONS.map((name) => {
    const held = target.permissions.includes(name);
    const stored = (target.stored_permissions ?? []).includes(name);
    const revocable = stored && target.may_revoke.includes(name);
    const offered = revocable || (!held && target.may_grant.includes(name));
    const note = stored && !revocable ? "kept"
      : held && !stored ? "implied"
      : !offered && !held ? "you may not grant it"
      : null;
    return { name, held, stored, offered, note };
  });
}

/** Stored names this build does not know: they grant nothing, and stay. */
export const unknownStored = (target: UserView) =>
  (target.stored_permissions ?? []).filter((name) => !known(name));

/** The boxes drawn ticked and enabled, which the form starts from. */
export const tickedAtFirst = (choices: PermissionChoice[]) =>
  new Set(choices.filter((choice) => choice.offered && choice.held).map((choice) => choice.name));

/**
 * The amend the form sends, as the classic `set_user_permissions` worked it out:
 * grant each ticked name not held, revoke each stored name left unticked.
 * Only an enabled box counts. `null` when it would ask for nothing, which
 * the route refuses.
 */
export function amendment(choices: PermissionChoice[], ticked: ReadonlySet<UserPermission>): AmendUserPermissionsRequest | null {
  // An amend over the names drawn, so a grant made since the page was drawn stands.
  const offered = choices.filter((choice) => choice.offered);
  const add = offered.filter((choice) => ticked.has(choice.name) && !choice.held).map((choice) => choice.name);
  const remove = offered.filter((choice) => choice.stored && !ticked.has(choice.name)).map((choice) => choice.name);
  return add.length || remove.length ? { add, remove } : null;
}

/** The standing form an account's status offers (`user.html`), or the note in its place. */
export function standing(status: UserStatus, self: boolean):
  { note: string; to: UserStatus | null; label: string; danger: boolean } {
  switch (status) {
    case "pending":
      return { note: "This account asked for itself and waits for an administrator to admit it.",
               to: "active", label: "Admit this account", danger: false };
    case "disabled":
      return { note: "Disabled: every request from it is refused, and its record stands.",
               to: "active", label: "Enable this account", danger: false };
    case "active":
      return self
        ? { note: "Active. You cannot disable your own account.", to: null, label: "", danger: false }
        : { note: "Active. Disabling refuses every request from it and keeps its record, so the plans it " +
                  "filed stay attributed to an account that exists.",
            to: "disabled", label: "Disable this account", danger: true };
  }
}

/** Asked before revoking one of an account's keys. */
export const REVOKE_KEY = (key_id: string) => ({
  title: `Revoke ${key_id}?`, text: "The machine that holds it can no longer sign; the account's other keys keep signing.",
  action: "Revoke this key", pending: "Revoking…",
});

/** Asked before revoking the key this browser signs with, the one revocation that signs it out. */
export const SIGN_OUT = {
  title: "Revoke this browser's key?", text: "This is the key this browser signs with. Revoking it signs this browser out.",
  action: "Revoke and sign out", pending: "Revoking…",
} as const;

/**
 * The retype an account's type offers, or the note in its place. A person's account becomes a service only
 * while nobody has signed in as it (`awaits`), since its administrator may mint a service account keys.
 */
export function retype(type: UserAccountType, awaits: boolean, campaignReader = false):
  { note: string; to: UserAccountType | null; label: string } {
  if (campaignReader) {
    return { note: "A campaign reader, which is a service account and stays one: a person's account is a new account.",
             to: null, label: "" };
  }
  if (type === "service") {
    return { note: "A service account: a job signs as it with its own keys, never through Clerk, and you may " +
                   "mint it a pre-auth key at any time.", to: "human", label: "Make it a person's account" };
  }
  return awaits
    ? { note: "A person's account. Nobody has signed in as it yet, so it may become a service account.",
        to: "service", label: "Make it a service account" }
    : { note: "A person's account. Somebody has signed in as it, so it stays a person's here; " +
              "`pipette-mgmt users set-type` on the host covers the rest.", to: null, label: "" };
}

/** Whether a pre-auth key may be minted here: any time for a service account, else before the first sign-in. */
export const mintable = (type: UserAccountType, awaits: boolean) => type === "service" || awaits;

/** The new-account form as typed. */
export interface NewAccountForm {
  contact_email: string;
  /** Empty is none for a reader over every organization; a scoped one sends none. */
  org_id: string;
  account_type: UserAccountType;
  permissions: UserPermission[];
  /** The campaigns a campaign reader reads; `null` for any other account. */
  campaign_scope: string[] | null;
  invite: boolean;
  expires_in_days: number;
}

/**
 * The create and the first key's mint, each held to the checks the routes
 * make on the body before anything is written, as the classic `NewUserForm` held them:
 * a key refused for its days after the create would strand the account.
 */
export function newAccount(form: NewAccountForm): { create: CreateUserRequest; mint: MintUserPreauthRequest | null } {
  const contact_email = form.contact_email.trim();
  // The shape `ContactEmail` takes at its simplest; the route judges the rest, before writing.
  if (!/^[^\s@"/]+@[^\s@]+\.[^\s@]+$/.test(contact_email)) {
    throw new Error("invalid contact_email: contact_email is not a valid email address");
  }
  const days = form.expires_in_days;
  const max = Math.max(...EXPIRY_DAYS);
  if (form.invite && !(Number.isInteger(days) && days >= 1 && days <= max)) {
    throw new Error(`expires_in_days must be between 1 and ${max}`);
  }
  const scope = form.campaign_scope;
  if (scope && form.account_type !== "service") throw new Error("a campaign scope is a service account's");
  if (scope && !scope.length) throw new Error("choose the campaigns it reads");
  const org_id = form.org_id.trim();
  return {
    create: {
      contact_email, account_type: form.account_type, permissions: form.permissions, ...(org_id ? { org_id } : {}),
      ...(scope ? { campaign_scope: scope } : {}),
    },
    mint: form.invite ? mintBody(days) : null,
  };
}

/**
 * What a campaign scope may name: the campaigns the reader reads, newest first, then each one `named` already
 * names that the reader does not, by its id, so it can be dropped.
 */
export function scopeChoices(listed: { campaign_id: string; name: string }[], named: string[]): Choice[] {
  const read = new Set(listed.map((campaign) => campaign.campaign_id));
  return [
    ...listed.map((campaign) => ({ value: campaign.campaign_id, label: campaign.name })),
    ...named.filter((id) => !read.has(id)).map((id) => ({ value: id, label: `${id} (not readable, or deleted)` })),
  ];
}

/** An account's registered keys: their columns and the text box over their ids and details. */
export const USER_KEY_TABLE: TableSpec<UserKey> = {
  columns: [
    { key: "key_id", label: "key_id", raw: (key) => key.key_id, as: "id" },
    { key: "key_details", label: "key_details", raw: (key) => key.key_details },
    { key: "added_at", label: "added_at", raw: (key) => key.added_at, as: "time" },
    { key: "expires_at", label: "expires_at", raw: (key) => key.expires_at ?? null },
  ],
  texts: [{ key: "q", label: "key_id or key_details", texts: (key) => [key.key_id, key.key_details] }],
};

/** The keys held at the first Clerk sign-in that the account still holds. */
export function keysBeforeFirstSignIn(account: Pick<OwnUserView, "keys" | "first_clerk_sign_in">): string[] {
  const prior = new Set(account.first_clerk_sign_in?.prior_keys);
  return account.keys.map((key) => key.key_id).filter((key_id) => prior.has(key_id));
}

/** An account page's tabs, each an address of its own. */
export type UserTab = "overview" | "plans" | "permissions" | "keys" | "preauth" | "settings";

const USER_TABS: Record<UserTab, { page: Page; label: string }> = {
  overview: { page: "user", label: "Overview" }, plans: { page: "user-plans", label: "Plans" },
  permissions: { page: "user-permissions", label: "Permissions" },
  keys: { page: "user-keys", label: "Keys" }, preauth: { page: "user-preauth", label: "Pre-auth keys" },
  settings: { page: "user-settings", label: "Settings" },
};

/** The tab `page` draws. */
export const userTabOf = (page: Page): UserTab =>
  (Object.entries(USER_TABS).find(([, tab]) => tab.page === page)?.[0] ?? "overview") as UserTab;

/** How far the reader reaches an account: its filings alone, its record on the roster, or its administration. */
export type Reach = "filed" | "record" | "administers";

/**
 * An account's tabs: its Overview and Plans for any reader, its Permissions for a reader of its record, its keys for
 * its administrator, and set apart its Settings. The open tab is listed whatever, so the bar always says where the
 * reader is.
 */
export function userTabs(user_id: string, current: UserTab, reach: Reach): BarTab[] {
  const administers = reach === "administers";
  const tab = (name: UserTab, start = false): BarTab => ({
    key: name, label: USER_TABS[name].label, current: current === name, start,
    href: pathOf({ page: USER_TABS[name].page, user_id } as never),
  });
  return [
    tab("overview"), tab("plans"),
    ...(reach !== "filed" || current === "permissions" ? [tab("permissions")] : []),
    ...(["keys", "preauth"] as const).filter((name) => administers || current === name).map((name) => tab(name)),
    ...(administers || current === "settings" ? [tab("settings", true)] : []),
  ];
}

/** The reader's own account's tabs: its record and session, and its keys. */
export const accountTabs = (keys: boolean): BarTab[] => [
  { key: "overview", label: "Overview", href: pathOf({ page: "account" }), current: !keys, start: false },
  { key: "keys", label: "Keys", href: pathOf({ page: "account-keys" }), current: keys, start: false },
];
