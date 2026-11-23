<script lang="ts">
  // A device's catalog short list: the pinned and active entries of the global
  // scope and the reader's organization that offer it a member, and the members that it is not offered, with the reason.
  // Each member shows its run evidence. The evidence never selects a member.
  import { deviceCatalog } from "../api";
  import type { DescriptorCatalogShortList, DescriptorCatalogShortListItem, DescriptorMemberView } from "../api-types";
  import {
    ANY_DEVICE_LABEL, anyDeviceText, catalogRowId, entryHref, shortListSections, thisDeviceText, withheldSummary,
  } from "../catalog";
  import { when } from "../format";
  import { session } from "../session.svelte";
  import Loaded from "./Loaded.svelte";
  import { Loader } from "./work.svelte";

  let { clientId }: { clientId: string } = $props();
  const read = new Loader<DescriptorCatalogShortList>();
  $effect(() => { const id = clientId; read.run(() => deviceCatalog(session.signer, id)); });
</script>

{#snippet evidenceHead(member: string, family: boolean)}
  <tr>
    <th>name</th>{#if family}<th>family</th>{/if}<th>{member}</th><th>last success on this device</th><th>last failure on this device</th>
    <th>{ANY_DEVICE_LABEL}</th><th>release</th>
  </tr>
{/snippet}

{#snippet evidenceRow(item: DescriptorCatalogShortListItem, first: boolean, member: DescriptorMemberView)}
  <tr>
    <td>{#if first}<a href={entryHref(item)}>{item.display_name}</a>{/if}</td>
    {#if item.descriptor_kind === "model"}<td>{#if first}{item.family ?? ""}{/if}</td>{/if}
    <td><code>{member.selector}</code></td>
    <td>{thisDeviceText(member, "last_success")}</td>
    <td>{thisDeviceText(member, "last_failure")}</td>
    <td class="muted">{anyDeviceText(member.evidence) ?? "unavailable"}</td>
    <td>{#if first}{when(item.release_time)}{/if}</td>
  </tr>
{/snippet}

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
        <div class="scroll"><table>
          <thead>{@render evidenceHead("member", false)}</thead>
          <tbody>
            {#each sections.runtime as item (catalogRowId(item))}
              {#each item.members as member (member.digest)}
                {@render evidenceRow(item, true, member)}
              {/each}
            {/each}
          </tbody>
        </table></div>
      {:else}<p class="muted">No runtime entry offers this device a member.</p>{/if}
      <h3>Models</h3>
      {#if sections.model.length}
        <div class="scroll"><table>
          <thead>{@render evidenceHead("members", true)}</thead>
          <tbody>
            {#each sections.model as item (catalogRowId(item))}
              {#each item.members as member, index (member.digest)}
                {@render evidenceRow(item, index === 0, member)}
              {/each}
            {/each}
          </tbody>
        </table></div>
      {:else}<p class="muted">No model entry offers this device a member.</p>{/if}
      {#if summary}
        <details class="withheld">
          <summary>{summary}</summary>
          <table>
            <thead><tr><th>member</th><th>reason</th></tr></thead>
            <tbody>
              {#each list.withheld.members as member (`${member.scope}/${member.selector}`)}
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
  /* A short list can hold many entries. Each table scrolls inside its box, and its header stays in view. */
  .scroll { max-height: 24em; overflow-y: auto; margin: 8px 0 14px; }
  .scroll table { margin: 0; }
  .scroll th { position: sticky; top: 0; z-index: 1; }
  .withheld { margin-top: 12px; }
  .withheld summary { cursor: pointer; }
</style>
