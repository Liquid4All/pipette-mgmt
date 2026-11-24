<script lang="ts">
  // A campaign group's Campaigns tab once read: its record, and its member campaigns as the campaign listings draw them,
  // with how many every policy that is on cleared, an add from the owner's other campaigns and a remove of each, each a
  // write the page passes.
  import { mayFile, memberOffers, memberRows } from "../campaign-groups";
  import { campaignTable } from "../campaigns";
  import { when } from "../format";
  import { offeredChoices } from "../table";
  import type { CampaignGroupView, CampaignView, OwnUserView } from "../api-types";
  import { name as campaignName, policies, progress, progressHead, status } from "./CampaignListing.svelte";
  import DataTable from "./DataTable.svelte";
  import LabelChips from "./LabelChips.svelte";
  import MultiSelect from "./MultiSelect.svelte";
  import OwnerCell from "./OwnerCell.svelte";
  import { Busy } from "./work.svelte";

  let { group, listed, reader, names, listing, add, remove }: {
    group: CampaignGroupView;
    /** The campaigns read beside the group: the members' plans, and what the add offers. */
    listed: CampaignView[];
    reader: OwnUserView;
    names: Record<string, string>;
    /** The owner's listing, which a member's label opens narrowed to it. */
    listing?: string;
    add: (campaign_ids: string[]) => Promise<void>;
    remove: (campaign_id: string) => Promise<void>;
  } = $props();

  const files = $derived(mayFile(reader, group.owner));
  // The rows are read already, so the members' table filters in the page alone; they carry no models to search.
  // The group lists its members oldest first, where the listings come newest first.
  const table = campaignTable({}, false);
  const spec = { ...table, sort: "created_at", server: [],
                 texts: table.texts?.map((text) => ({ ...text, placeholder: "campaign id, name or label" })) };
  const members = $derived(memberRows(group.members, listed));
  // The members queue-maintenance has read under the group's policies.
  const checked = $derived(members.filter((row) => row.policy_checks));
  const cleared = $derived(checked.filter((row) => row.policy_checks?.every((check) => !check.triggered)).length);

  let picked = $state<string[]>([]);
  const adding = new Busy();
  async function addPicked() {
    if (adding.busy || !picked.length) return;
    await adding.attempt(async () => {
      const sent = picked;
      await add(sent);
      picked = picked.filter((id) => !sent.includes(id));
    });
  }

  const removing = new Busy();
  const removeRow = (row: CampaignView) =>
    removing.busy || void removing.attempt(() => remove(row.campaign_id), `Removing ${row.name}`);
</script>

{#snippet labels(row: CampaignView)}<LabelChips labels={row.labels} {listing} />{/snippet}
{#snippet removal(row: CampaignView)}
  <button type="button" aria-label="Remove {row.name} from the group" aria-disabled={removing.busy}
          onclick={() => removeRow(row)}>Remove</button>
{/snippet}

<dl class="summary">
  <div><dt>owner</dt><dd><OwnerCell owner={group.owner} {names} /></dd></div>
  <div><dt>created_at</dt><dd>{when(group.created_at)}</dd></div>
  <div><dt>updated_at</dt><dd>{when(group.updated_at)}</dd></div>
  <div><dt>group_id</dt><dd class="id">{group.group_id}</dd></div>
</dl>

{#if files}
  <div class="bar">
    <MultiSelect label="campaigns to add" options={offeredChoices(memberOffers(listed, group), picked, (value) => value)}
                 chosen={picked} none="none chosen" searchable
                 toggle={(value, on) => (picked = on ? [...picked, value] : picked.filter((each) => each !== value))} />
    <button type="button" class="primary" aria-disabled={adding.busy || !picked.length} onclick={addPicked}>{
      adding.busy ? "Adding…" : "Add to group"}</button>
  </div>
  <p class="muted">A campaign is in one group at most; one already in another group refuses the whole add.</p>
  {#if adding.error}<p class="err" role="alert">{adding.error}</p>{/if}
  {#if removing.error}<p class="err" role="alert">{removing.error}</p>{/if}
{:else}
  <p class="muted">Adding or removing its campaigns takes <code>submit_plans</code> for its owner.</p>
{/if}
{#if checked.length}<p>{cleared} of {checked.length} campaigns cleared on every policy that is on.</p>{/if}
<DataTable {spec} rows={members} id={(row) => row.campaign_id} noun="campaigns"
           empty="This group holds no campaign." cells={{ name: campaignName, status, policies, progress, labels }}
           heads={{ progress: progressHead }}
           action={files ? removal : undefined} />
