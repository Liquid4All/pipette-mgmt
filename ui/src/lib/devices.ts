// A device's page (`client.html`) and an organization's device pre-auth keys
// (`clients.html`'s `#device-keys`): where the page sits, what its forms send,
// and the wording they draw.

import { ME, SERVER, orgTrail, type Crumb } from "./crumbs";
import type { DeviceBranch } from "./fleet";
import { handle } from "./format";
import type { OrgRef } from "./orgs";
import { mintBody } from "./preauth";
import { reaches } from "./rights";
import { campaignLink, pathOf } from "./routes";
import type {
  AmendClientPermissionsRequest, ClientDetail, ClientPermission, ClientPreauthUsage,
  MintClientPreauthRequest, OwnUserView,
} from "./api-types";

/**
 * The branch a device's page sits under, as the classic `device_branch` decided it:
 * the server's for a device nobody owns, the reader's for one it owns, an
 * organization's the tree draws for a reader of its roster, else the fleet.
 * `owning` is the organization `owning_org_id` names, when the tree holds it.
 */
export function deviceBranch(record: ClientDetail, account: OwnUserView, owning: OrgRef | null): DeviceBranch {
  const owner = record.owner;
  if (!owner) return { kind: "server" };
  if (owner.type === "user" && owner.user_id === account.user_id) return { kind: "me" };
  if (owning && owning.org_id === record.owning_org_id && reaches(account, account.may.read_roster, owning.org_id)) {
    return { kind: "org", org_id: owning.org_id, slug: owning.slug, count: null };
  }
  return { kind: "all" };
}

/** The listing a branch draws its devices on. */
export function listingPath(branch: DeviceBranch): string {
  switch (branch.kind) {
    case "all": return pathOf({ page: "clients" });
    case "me": return pathOf({ page: "me-devices" });
    case "org": return pathOf({ page: "org-devices", org: handle(branch) });
    case "server": return pathOf({ page: "server-devices" });
  }
}

/**
 * Whether the page names the owner: for a reader whose roster read covers the
 * organization the device rolls up to, as `GET /api/v1/clients/{client_id}` gives its `owning_org_id`, so
 * no other asks `GET /api/v1/users` or `GET /api/v1/orgs` a question it is refused.
 */
export function namesOwner(record: ClientDetail, account: OwnUserView): boolean {
  return reaches(account, account.may.read_roster, record.owning_org_id);
}

/** The back link's words for a branch's listing. */
export function listingName(branch: DeviceBranch): string {
  switch (branch.kind) {
    case "all": return "Fleet";
    case "server": return "Unclaimed devices";
    case "me": case "org": return "Devices";
  }
}

/** The trail of a device's page: its branch's section, then its listing. */
export function deviceTrail(branch: DeviceBranch): readonly Crumb[] {
  const section = branch.kind === "me" ? ME : branch.kind === "server" ? SERVER
    : branch.kind === "org" ? orgTrail(branch) : [];
  return [...section, { label: listingName(branch), href: listingPath(branch) }];
}

/** The flags that the device declares and that the server derives from its profile, each once, sorted. */
export function capabilityFlags(record: ClientDetail): string[] {
  return [...new Set([...(record.capabilities ?? []), ...record.derived_capabilities])].sort();
}

/** One row of a device's backlog by campaign. Only a campaign that the reader may read has a link. */
export interface BacklogRow {
  key: string;
  label: string;
  jobs: number;
  href?: string;
}

/**
 * The rows of a device's backlog by campaign, in this order: each readable
 * campaign, the jobs outside every campaign, and the jobs in campaigns that the
 * reader may not read. The function omits a row that has no jobs.
 */
export function backlogRows(record: ClientDetail): BacklogRow[] {
  const backlog = record.backlog_by_campaign;
  if (!backlog) return [];
  const sums: BacklogRow[] = [
    { key: "outside", label: "not in a campaign", jobs: backlog.outside_campaign_job_count },
    { key: "unreadable", label: "in campaigns you cannot read", jobs: backlog.unreadable_job_count },
  ];
  return [
    ...backlog.campaigns.map((campaign) => ({
      key: campaign.campaign_id, label: campaign.name, jobs: campaign.job_count, href: campaignLink(campaign.campaign_id),
    })),
    ...sums.filter((row) => row.jobs > 0),
  ];
}

/**
 * Whether the backlog by campaign comes from a different count than the
 * backlog total. The two have separate writes, so a failed write or a read
 * between the writes pairs a total with a breakdown from another run.
 */
export function backlogDisagrees(record: ClientDetail): boolean {
  const byCampaign = record.backlog_by_campaign;
  return !!byCampaign && byCampaign.counted_at !== record.backlog?.counted_at;
}

/** What each client permission does, as the classic `ClientPermissionChoice::new` worded it; in the vocabulary's order. */
export const PERMISSION_NOTES: Record<ClientPermission, string> = {
  submit_results: "submits a result it ran on its own",
  perform_work: "claims queued jobs, whichever organization filed them, and fetches what its owner owns",
};

export const CLIENT_PERMISSIONS = Object.keys(PERMISSION_NOTES) as ClientPermission[];

/**
 * The amend the permission form sends, as the classic form posted it: each name
 * ticked, and each held and unticked, so a grant made since the page was drawn
 * stands. `null` when it would ask for nothing, which the route refuses.
 */
export function amendment(held: string[], ticked: ReadonlySet<ClientPermission>): AmendClientPermissionsRequest | null {
  const add = CLIENT_PERMISSIONS.filter((name) => ticked.has(name));
  const remove = CLIENT_PERMISSIONS.filter((name) => held.includes(name) && !ticked.has(name));
  return add.length || remove.length ? { add, remove } : null;
}

/** Whether the reader may mint a device pre-auth key for `org_id`: its device reach covers it. */
export const mayMintFor = (account: OwnUserView, org_id: string): boolean =>
  reaches(account, account.may.administer_fleet, org_id);

/** The mint's body, its expiry and usage always sent and a blank note left out. */
export const mintRequest = (expires_in_days: number, multiUse: boolean, note: string): MintClientPreauthRequest =>
  ({ ...mintBody(expires_in_days, note), usage: multiUse ? "multi_use" : "single_use" });

/** A key's usage as the listing prints it. */
export const usage = (value: ClientPreauthUsage) => value.replace("_", "-");

/** The hand-over line under a minted key (`client_preauth_minted.html`). */
export const handOver = (value: ClientPreauthUsage): string =>
  value === "multi_use"
    ? "It registers any number of devices until it expires, so revoke it on the organization's devices " +
      "page once the rack is up."
    : "It registers one device and is gone. If it goes astray before then, revoke it on the " +
      "organization's devices page.";
