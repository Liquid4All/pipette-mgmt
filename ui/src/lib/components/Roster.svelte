<script lang="ts">
  // The roster listing, as the classic `users.html` drew it under any branch.
  // `GET /api/v1/users` answers the whole roster the reader reads, so `DataTable`
  // filters, sorts and pages it here.
  import { users } from "../api";
  import { rosterEmpty, rosterRows, rosterTable, type RosterBranch } from "../accounts";
  import { orgLink } from "../routes";
  import { session } from "../session.svelte";
  import type { UserView } from "../api-types";
  import DataTable from "./DataTable.svelte";
  import Loaded from "./Loaded.svelte";
  import { Loader } from "./work.svelte";

  let { branch, path }: {
    branch: RosterBranch;
    /** The page's own address, which the filters, the sort and the pager keep. */
    path: string;
  } = $props();

  const roster = new Loader<UserView[]>();
  roster.run(() => users(session.signer));

  // Every account's listing names a row's organization from those the tree lists.
  const orgOf = (org_id: string) => session.knownOrg(org_id);
  const spec = $derived(rosterTable(branch, (org_id) => orgOf(org_id)?.slug ?? null));
  const listed = $derived(rosterRows(roster.data ?? [], branch));
</script>

{#snippet organization(row: UserView)}
  {@const org = row.org_id ? orgOf(row.org_id) : null}
  {#if org}<a href={orgLink(org)} title={org.org_id}>{org.slug}</a>
  {:else if row.org_id}<span class="id">{row.org_id}</span>
  {:else}<span class="muted">none</span>{/if}
{/snippet}

<!-- Over a refusal the page draws the error alone: the filter runs in the browser. -->
<Loaded of={roster}>
  {#snippet children()}
    <DataTable {spec} rows={listed} id={(row) => row.user_id} noun="accounts" {path} empty={rosterEmpty(branch)}
               columnsKey="pipette.users.columns" cells={{ organization }} />
  {/snippet}
</Loaded>
