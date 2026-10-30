<script lang="ts">
  // A campaign reader's campaigns: its organization's, of those the reader
  // reads, since the server refuses a scope naming any other.
  import { campaigns } from "../api";
  import { scopeChoices } from "../accounts";
  import Loaded from "./Loaded.svelte";
  import MultiSelect from "./MultiSelect.svelte";
  import { Loader } from "./work.svelte";
  import { session } from "../session.svelte";
  import { ROUTE_PAGE_MAX, everyPage, withCursor } from "../table";
  import type { NamedCampaignView } from "../api-types";

  let { org, chosen, toggle }: {
    /** The account's organization. */
    org: string;
    chosen: string[];
    toggle: (value: string, on: boolean) => void;
  } = $props();

  const listed = new Loader<NamedCampaignView[]>();
  $effect(() => {
    const query = new URLSearchParams({ order: "newest", owner: "org", org_id: org, limit: ROUTE_PAGE_MAX });
    listed.run(async () => (await everyPage((cursor) =>
      campaigns(session.signer, withCursor(query.toString(), cursor)))).items);
  });
</script>

<Loaded of={listed}>
  {#snippet children(readable)}
    <MultiSelect label="campaigns it reads" options={scopeChoices(readable, chosen)} {chosen} {toggle}
                 none="none chosen" searchable />
  {/snippet}
</Loaded>
