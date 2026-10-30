<script lang="ts">
  // A device's catalog short list: the pinned and active global entries that
  // offer it a member, and the members that it is not offered, with the reason.
  import { deviceCatalog } from "../api";
  import type { DescriptorCatalogShortList } from "../api-types";
  import { catalogEntryPath, shortListSections, withheldSummary } from "../catalog";
  import { when } from "../format";
  import { session } from "../session.svelte";
  import Loaded from "./Loaded.svelte";
  import { Loader } from "./work.svelte";

  let { clientId }: { clientId: string } = $props();
  const read = new Loader<DescriptorCatalogShortList>();
  $effect(() => { const id = clientId; read.run(() => deviceCatalog(session.signer, id)); });
</script>

<section class="catalog">
  <h2>Catalog short list</h2>
  <p class="muted">The catalog names that this device can use in a plan now.</p>
  <Loaded of={read}>
    {#snippet children(list)}
      {@const sections = shortListSections(list)}
      {@const summary = withheldSummary(list.withheld)}
      {#if list.unreadable}<p class="err">{list.unreadable} unreadable catalog entry(s)</p>{/if}
      <h3>Runtimes</h3>
      {#if sections.runtime.length}
        <table>
          <thead><tr><th>name</th><th>member</th><th>release</th></tr></thead>
          <tbody>
            {#each sections.runtime as item (item.name)}
              <tr>
                <td><a href={catalogEntryPath(item.name)}>{item.name}</a></td>
                <td>{#each item.members as member (member.digest)}<code>{member.selector}</code>{/each}</td>
                <td>{when(item.release_time)}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {:else}<p class="muted">No runtime entry offers this device a member.</p>{/if}
      <h3>Models</h3>
      {#if sections.model.length}
        <table>
          <thead><tr><th>name</th><th>members</th><th>release</th></tr></thead>
          <tbody>
            {#each sections.model as item (item.name)}
              <tr>
                <td><a href={catalogEntryPath(item.name)}>{item.name}</a></td>
                <td><ul class="selectors">
                  {#each item.members as member (member.digest)}<li><code>{member.selector}</code></li>{/each}
                </ul></td>
                <td>{when(item.release_time)}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      {:else}<p class="muted">No model entry offers this device a member.</p>{/if}
      {#if summary}
        <details class="withheld">
          <summary>{summary}</summary>
          <table>
            <thead><tr><th>member</th><th>reason</th></tr></thead>
            <tbody>
              {#each list.withheld.members as member (member.selector)}
                <tr><td><code>{member.selector}</code></td><td>{member.reason}</td></tr>
              {/each}
            </tbody>
          </table>
        </details>
      {/if}
    {/snippet}
  </Loaded>
</section>

<style>
  .catalog { margin: 24px 0; }
  .catalog td { vertical-align: top; }
  .selectors { list-style: none; margin: 0; padding: 0; }
  .withheld { margin-top: 12px; }
  .withheld summary { cursor: pointer; }
</style>
