<script lang="ts">
  // The classic console's unclaimed devices (`static/templates/clients.html`
  // under the server's branch): `GET /api/v1/clients?owned=false`, each row claimable
  // for any organization under `manage_any_clients`.
  import { orgs, setClientOwner } from "../lib/api";
  import DeviceListing from "../lib/components/DeviceListing.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { Busy } from "../lib/components/work.svelte";
  import { SERVER } from "../lib/crumbs";
  import { bySlug } from "../lib/orgs";
  import { pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import type { ClientListRow, OrgView } from "../lib/api-types";

  let { route }: { route: RouteOf<"server-devices"> } = $props();

  let listing = $state<ReturnType<typeof DeviceListing>>();

  // As `owner_choices::for_device` for a device in no organization: every
  // organization the reader reads, and only with `manage_any_clients`.
  let choices = $state<OrgView[]>([]);
  let picked = $state<Record<string, string>>({});
  const claiming = new Busy();

  $effect(() => {
    const account = session.account;
    if (account?.may.administer_fleet !== "every") return;
    orgs(session.signer, { counts: false })
      .then((listed) => (choices = listed.sort(bySlug)))
      .catch(() => (choices = []));
  });

  const claim = (row: ClientListRow, org_id: string | undefined) => {
    if (!org_id) return;
    return claiming.attempt(async () => {
      await setClientOwner(session.signer, row.client_id, { owner: { type: "org", org_id } });
      await listing?.reload();
    }, row.device_name ?? row.client_id);
  };
</script>

{#snippet claimCell(row: ClientListRow)}
  {#if choices.length === 1}
    <button type="button" aria-disabled={claiming.busy}
            aria-label="Claim {row.device_name ?? row.client_id} for {choices[0]?.slug}"
            onclick={() => claim(row, choices[0]?.org_id)}>Claim for {choices[0]?.slug}</button>
  {:else}
    <select bind:value={picked[row.client_id]} aria-label="claim {row.device_name ?? row.client_id} for">
      <option value="">organization…</option>
      {#each choices as org (org.org_id)}<option value={org.org_id}>{org.slug}</option>{/each}
    </select>
    <button type="button" disabled={!picked[row.client_id]} aria-disabled={claiming.busy}
            aria-label="Claim {row.device_name ?? row.client_id}"
            onclick={() => claim(row, picked[row.client_id])}>Claim</button>
  {/if}
{/snippet}

<PageHead trail={SERVER} title="Unclaimed devices" />

<DeviceListing bind:this={listing} branch={{ kind: "server" }} path={pathOf(route)}
               claim={choices.length ? claimCell : undefined} error={claiming.error} />
