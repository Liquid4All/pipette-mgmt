<script lang="ts">
  // An owner's campaign groups, its campaign listing's Groups tab: every page of `GET /api/v1/campaign-groups` narrowed
  // to the branch, each linking to its page and its audit, and a create for a reader who may file into the owner's
  // campaigns.
  import { campaignGroups, createCampaignGroup } from "../api";
  import { auditAction, auditLog } from "../audit";
  import { groupsPath, mayFile } from "../campaign-groups";
  import { nameProblem } from "../campaigns";
  import { ownedEmpty, ownedQuery, type OwnedBranch } from "../owned";
  import { here, navigateFrom } from "../route.svelte";
  import { campaignGroupLink } from "../routes";
  import { session } from "../session.svelte";
  import { ROUTE_PAGE_MAX, everyPage, withCursor, type CursorPage, type TableSpec } from "../table";
  import type { CampaignGroup } from "../api-types";
  import DataTable from "./DataTable.svelte";
  import Loaded from "./Loaded.svelte";
  import { Busy, Loader } from "./work.svelte";

  let { branch, base }: {
    branch: Exclude<OwnedBranch, { kind: "all" }>;
    /** The owner's campaign listing, as the tree names it: its campaigns are there, its groups under it. */
    base: string;
  } = $props();

  // The groups sit under the listing's leaf of the tree.
  $effect(() => { here.under = base; });

  const SPEC: TableSpec<CampaignGroup> = {
    columns: [
      { key: "name", label: "name", raw: (row) => row.name },
      { key: "created_at", label: "created_at", raw: (row) => row.created_at, as: "time" },
      { key: "updated_at", label: "updated_at", raw: (row) => row.updated_at, as: "time" },
    ],
  };

  const asked = $derived(ownedQuery(branch, [], null, ROUTE_PAGE_MAX));
  const read = new Loader<CursorPage<CampaignGroup>>();
  const load = (query: string) =>
    read.run(() => everyPage((cursor) => campaignGroups(session.signer, withCursor(query, cursor))));
  $effect(() => { void load(asked); });

  const owner = $derived(branch.kind === "org" ? { type: "org" as const, org_id: branch.org_id }
    : { type: "user" as const, user_id: session.account!.user_id });
  let name = $state("");
  const creating = new Busy();

  async function create(event: SubmitEvent) {
    event.preventDefault();
    if (creating.busy) return;
    const from = here.path;
    await creating.attempt(async () => {
      const problem = nameProblem(name);
      if (problem) throw new Error(problem);
      // The route's owner is the reader when the body names none.
      const created = await createCampaignGroup(session.signer,
                                                { name: name.trim(), ...(branch.kind === "org" ? { owner: branch.org_id } : {}) });
      navigateFrom(from, campaignGroupLink(created.group_id));
    });
  }
</script>

{#snippet groupName(row: CampaignGroup)}<a href={campaignGroupLink(row.group_id)}>{row.name}</a>{/snippet}
{#snippet audit(row: CampaignGroup)}<a href={auditAction(session.account!, row.group_id)?.href} aria-label="Audit {row.name}">Audit</a>{/snippet}

{#if session.account && mayFile(session.account, owner)}
  <form class="bar" novalidate onsubmit={create}>
    <label><span>new group</span>
      <input type="text" autocomplete="off" placeholder="Per-device study" bind:value={name}>
    </label>
    <button type="submit" aria-disabled={creating.busy || !!nameProblem(name)}>{creating.busy ? "Creating…" : "Create group"}</button>
  </form>
  {#if creating.error}<p class="err" role="alert">{creating.error}</p>{/if}
{:else}
  <p class="muted">Creating a group here takes <code>submit_plans</code> for its owner.</p>
{/if}
<Loaded of={read}>
  {#snippet children(listed)}
    {#if listed.unreadable}
      <div class="bar"><span class="err">{listed.unreadable} unreadable campaign group record(s)</span></div>
    {/if}
    <DataTable spec={SPEC} rows={listed.items} id={(row) => row.group_id} noun="groups" path={groupsPath(base)}
               empty={ownedEmpty(branch, "campaign groups")} cells={{ name: groupName }}
               action={session.account && auditLog(session.account) ? audit : undefined} actionHead="audit" />
  {/snippet}
</Loaded>
