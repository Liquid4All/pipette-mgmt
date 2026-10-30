<script lang="ts">
  // The classic console's private benchmark (`static/templates/benchmark.html`):
  // found by key in `GET /api/v1/orgs/{org}/benchmarks`, so an entry that does not read
  // is still drawn, and its definition read as the catalog file for one that
  // does. Its administrators replace it (`PUT` with `If-Match: *`, so one
  // deleted meanwhile stays deleted) and delete it.
  import { benchmarkToml, deleteBenchmark, orgBenchmarks, putBenchmark } from "../lib/api";
  import { catalogOrg, editRefused, entryOf, pathBenchmarkId } from "../lib/benchmarks";
  import DangerRow from "../lib/components/DangerRow.svelte";
  import Loaded from "../lib/components/Loaded.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import SettingRow from "../lib/components/SettingRow.svelte";
  import DangerZone from "../lib/components/DangerZone.svelte";
  import SettingsSection from "../lib/components/SettingsSection.svelte";
  import { Busy, Loader } from "../lib/components/work.svelte";
  import { orgTrail } from "../lib/crumbs";
  import { DELETES } from "../lib/danger";
  import { administersOrg, type OrgRef } from "../lib/orgs";
  import { here, navigateFrom } from "../lib/route.svelte";
  import { orgLink, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route }: { route: RouteOf<"benchmark"> } = $props();

  interface Entry {
    org: OrgRef;
    benchmark_type: string | null;
    /** The definition as the catalog file holds it; `null` for an entry that does not read. */
    toml: string | null;
  }

  const page = new Loader<Entry>();
  const editing = new Busy();
  const read = async (): Promise<Entry> => {
    const signer = session.signer;
    const benchmark_id = pathBenchmarkId(route.benchmark_id);
    const catalog = await orgBenchmarks(signer, route.org);
    const { benchmark_type } = entryOf(catalog, benchmark_id);
    const toml = benchmark_type === null ? null : await benchmarkToml(signer, catalog.org_id, benchmark_id);
    const org = await catalogOrg(catalog, route.org, (org_id) => session.resolveOrg(org_id));
    return { org, benchmark_type, toml };
  };
  page.run(read);

  // The tree marks the organization's Benchmarks leaf, as `benchmark.html` sets its branch.
  $effect(() => { if (page.data) here.under = orgLink(page.data.org, "/benchmarks"); });

  // What the form drew, kept as typed after a refusal; the effect below redraws it
  // after a save, as the classic redirect to this page did.
  let definition = $state("");
  $effect(() => { definition = page.data?.toml ?? ""; });

  async function save(event: SubmitEvent, entry: Entry) {
    event.preventDefault();
    await editing.attempt(async () => {
      try {
        await putBenchmark(session.signer, entry.org.org_id, route.benchmark_id, definition, false);
      } catch (e) {
        const gone = editRefused(e, route.benchmark_id);
        throw gone ? new Error(gone) : e;
      }
      await page.run(read, true);
    });
  }

  // As the classic `delete_benchmark`, to the organization's listing.
  let from = "";
  const remove = async (entry: Entry) => {
    from = here.path;
    await deleteBenchmark(session.signer, entry.org.org_id, route.benchmark_id);
  };
</script>

<PageHead title={route.benchmark_id} trail={page.data
  ? [...orgTrail(page.data.org), { label: "Benchmarks", href: orgLink(page.data.org, "/benchmarks") }] : []} />

<Loaded of={page}>
  {#snippet children(entry)}
    <dl>
      <dt>benchmark_id</dt><dd class="id">{route.benchmark_id}</dd>
      <dt>organization</dt><dd>{entry.org.slug} <span class="muted id">{entry.org.org_id}</span></dd>
      <dt>benchmark_type</dt><dd>{entry.benchmark_type ?? "—"}</dd>
    </dl>

    {@const served = `GET /benchmarks/${route.benchmark_id}`}
    {#if !entry.toml}
      <p class="notice">
        This entry does not read as a definition, so no device is served it. It still
        holds its id and keeps the organization from being deleted. The server log
        says why. Write it again or delete it.
      </p>
    {/if}

    {#if session.account && administersOrg(session.account, entry.org.org_id)}
      <div class="settings">
        <SettingsSection title="General">
          <SettingRow title="Definition"
                      text={`What the catalog file holds, served to the organization's devices and members at ${served}. Saving replaces it under this id, held to the checks a catalog file is at load; devices are served the new one on their next request.`}>
            {#snippet children()}
              {#if editing.error}<p class="err code" role="alert">{editing.error}</p>{/if}
              <form onsubmit={(event) => save(event, entry)}>
                <textarea class="code" bind:value={definition} required spellcheck="false" aria-label="definition"></textarea>
                <div class="bar"><button type="submit" aria-disabled={editing.busy}>Save this definition</button></div>
              </form>
            {/snippet}
          </SettingRow>
        </SettingsSection>

        <DangerZone>
          <DangerRow title="Delete this benchmark" ask={`Delete ${route.benchmark_id}?`} pending="Deleting…"
                     phrase={DELETES.benchmark} run={() => remove(entry)} locked={editing.busy}
                     done={() => navigateFrom(from, orgLink(entry.org, "/benchmarks"))}
                     text="Removes the entry, so the organization's devices are no longer served it and a plan naming it is refused at ingestion. Its id is free to take again." />
        </DangerZone>
      </div>
    {:else}
      {#if entry.toml}
        <h2>Definition</h2>
        <pre><code>{entry.toml}</code></pre>
        <p class="muted">
          What the catalog file holds. The organization's devices and members are
          served it at <code>{served}</code>, and a plan one of its members files names
          it in its <code>benchmarks</code>.
        </p>
      {/if}
      <p class="muted">
        You may read this benchmark and not change it: changing your organization's
        takes <code>manage_orgs</code>.
      </p>
    {/if}
  {/snippet}
</Loaded>
