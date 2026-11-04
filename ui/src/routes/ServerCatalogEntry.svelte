<script lang="ts">
  // Read the entry selected by the address and show controls for a live, editable org entry.
  import { catalogEntry, descriptorNamesByDigest } from "../lib/api";
  import type { DescriptorDigestNames, DescriptorEntryListRow } from "../lib/api-types";
  import { digestBatches } from "../lib/catalog-entry";
  import {
    ANY_DEVICE_LABEL, anyDeviceText, catalogPath, catalogSynonyms, mayEdit, memberLabel, modelMemberRows,
    parameterText, scopeLabel, sharedParameters, type CatalogSynonym,
  } from "../lib/catalog";
  import CatalogEntryControls from "../lib/components/CatalogEntryControls.svelte";
  import Command from "../lib/components/Command.svelte";
  import Loaded from "../lib/components/Loaded.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import StatusPill from "../lib/components/StatusPill.svelte";
  import { Loader } from "../lib/components/work.svelte";
  import { SERVER } from "../lib/crumbs";
  import { when } from "../lib/format";
  import { here } from "../lib/route.svelte";
  import type { RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route }: { route: RouteOf<"server-catalog-entry"> } = $props();
  interface Detail { entry: DescriptorEntryListRow | null; synonyms: CatalogSynonym[] | null }
  const read = new Loader<Detail>();
  let controls = $state<ReturnType<typeof CatalogEntryControls>>();
  $effect(() => { here.under = catalogPath; });
  /** Read the entry; `keep` leaves the drawn entry up until the answer replaces it. */
  const load = (keep = false) => read.run(async () => {
    const found = await catalogEntry(session.signer, route.name, session.account?.may.read_fleet ?? false);
    const entry = found.entries[0] ?? null;
    if (!entry) return { entry: null, synonyms: [] };
    const batches = digestBatches((entry.members ?? []).map((member) => member.digest));
    let answers: DescriptorDigestNames[];
    try {
      answers = await Promise.all(batches.map((digests) => descriptorNamesByDigest(session.signer, digests)));
    } catch {
      return { entry, synonyms: null };
    }
    const names: DescriptorDigestNames = { names: Object.assign({}, ...answers.map((answer) => answer.names)) };
    return { entry, synonyms: catalogSynonyms(entry, names, (hit) => scopeLabel(hit, session.account)) };
  }, keep);
  void load();
  const listPath = $derived(read.data?.entry?.descriptor_kind === "model" ? `${catalogPath}?kind=model` : catalogPath);
</script>

<PageHead trail={[...SERVER, { label: "Catalog", href: listPath }]} title={read.data?.entry?.display_name ?? route.name} />
<Loaded of={read}>
  {#snippet children(detail)}
    {#if !detail.entry}
      <p class="muted">No catalog entry has this name.</p>
    {:else}
      {@const entry = detail.entry}
      {@const model = entry.descriptor_kind === "model"}
      {@const shared = model ? sharedParameters(entry.members ?? []) : null}
      {@const editable = entry.state !== "tombstoned" && mayEdit(session.account, entry)}
      <dl class="facts">
        <div><dt>scope</dt><dd><StatusPill status={scopeLabel(entry, session.account)} /></dd></div>
        <div><dt>kind</dt><dd><StatusPill status={entry.kind} /></dd></div>
        <div><dt>state</dt><dd>{#if entry.state === "active"}active{:else}<StatusPill status={entry.state} />{/if}</dd></div>
        <div><dt>release time</dt><dd>{when(entry.release_time)}</dd></div>
        <div><dt>created</dt><dd>{when(entry.created_at)} by {entry.created_by}</dd></div>
        <div><dt>{model ? "model type" : "runtime type"}</dt><dd>{entry.descriptor_types.join(", ")}</dd></div>
        {#if shared}
          <div><dt>parameters</dt><dd>{parameterText(shared.parameters)}
            <span class="muted">(<code>{shared.model_key}</code>)</span></dd></div>
        {/if}
      </dl>
      {#if entry.scope === "global"}
        <p class="muted">Managed with <code>pipette-mgmt catalog</code>.</p>
      {:else if editable}
        <CatalogEntryControls bind:this={controls} {entry} onchanged={() => load(true)} />
      {/if}

      <h2>Members</h2>
      {#if model}
        <table class="models">
          <thead>
            <tr>
              <th>format</th><th>quantization</th>
              {#if !shared}<th>model key</th><th>parameters</th>{/if}
              <th>added</th>
              {#if session.account?.may.read_fleet}<th>{ANY_DEVICE_LABEL}</th>{/if}
              <th>descriptor</th>
            </tr>
          </thead>
          <tbody>
            {#each modelMemberRows(entry.members ?? []) as row (row.member.digest)}
              <tr>
                <td>{row.format}</td>
                <td>{row.quantization}
                  {#if editable}
                    <button type="button" class="again"
                            onclick={() => controls?.relabel(row.format, row.quantization)}>change</button>
                  {/if}</td>
                {#if !shared}
                  <td>{#if row.member.model_key}<code>{row.member.model_key}</code>{:else}—{/if}</td>
                  <td>{parameterText(row.member.parameters)}</td>
                {/if}
                <td>{when(row.member.added_at)}</td>
                {#if session.account?.may.read_fleet}
                  <td class="muted">{anyDeviceText(row.member.evidence) ?? "Unavailable"}</td>
                {/if}
                <td>
                  <details>
                    <summary><code>{row.member.selector}</code></summary>
                    <pre><code>{JSON.stringify(row.member.descriptor, null, 2)}</code></pre>
                  </details>
                  <Command command={row.member.plan_toml} copyLabel="Copy as plan TOML" />
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {:else}
        {#each entry.members ?? [] as member, index (member.digest)}
          <section class="member">
            <h3>{index + 1}. {memberLabel(member.member_key)}</h3>
            <dl>
              <dt>added</dt><dd>{when(member.added_at)}</dd>
              <dt>offer rule</dt><dd>
                {#if member.coverage === "covered"}<StatusPill status="covered" />
                {:else if member.coverage === "no_rule"}<span class="err">no offer rule</span>
                {:else}<span class="err">unsupported</span>{/if}
              </dd>
              <dt>approved devices offered</dt><dd>{session.account?.may.read_fleet
                ? member.offered_device_count ?? "Unavailable" : "Requires fleet-read permission"}</dd>
              <dt>{ANY_DEVICE_LABEL}</dt><dd class="muted">{session.account?.may.read_fleet
                ? anyDeviceText(member.evidence) ?? "Unavailable" : "Requires fleet-read permission"}</dd>
              <dt>descriptor</dt><dd><pre><code>{JSON.stringify(member.descriptor, null, 2)}</code></pre></dd>
              <dt>in a plan</dt><dd><Command command={member.plan_toml} copyLabel="Copy as plan TOML" /></dd>
            </dl>
          </section>
        {/each}
      {/if}

      <h2>Other names of these descriptors</h2>
      {#if detail.synonyms === null}
        <p class="muted">Other names are unavailable right now.</p>
      {:else if detail.synonyms.length}
        <ul class="synonyms">
          {#each detail.synonyms as synonym (`${synonym.member}/${synonym.name}`)}
            <li><span class="muted">{synonym.member}</span>
              <a href={synonym.href}>{synonym.name}</a> <StatusPill status={synonym.scope} />
              <StatusPill status={synonym.kind} /></li>
          {/each}
        </ul>
      {:else}<p class="muted">No other names.</p>{/if}
    {/if}
  {/snippet}
</Loaded>

<style>
  .member { max-width: 72em; margin: 16px 0 24px; padding: 14px 18px;
            border: 1px solid var(--border); border-radius: var(--radius); }
  .member h3 { margin: 0 0 12px; }
  .member pre { max-width: 60em; }
  .models { margin: 16px 0 24px; }
  .models td { vertical-align: top; }
  .models pre { max-width: 48em; }
  .synonyms { list-style: none; padding: 0; }
  .synonyms li { display: flex; align-items: baseline; gap: 10px; margin: 6px 0; }
</style>
