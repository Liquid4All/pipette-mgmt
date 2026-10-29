<script lang="ts">
  // The classic console's new-benchmark form (`static/templates/new_benchmark.html`):
  // `PUT /api/v1/orgs/{org}/benchmarks/{id}` with `If-None-Match: *`, so an id the
  // organization keeps already is refused rather than replaced, then the entry.
  import { orgs, putBenchmark } from "../lib/api";
  import { createRefused, typedBenchmarkId, writeOrgs } from "../lib/benchmarks";
  import { describe, latest } from "../lib/components/guard";
  import Loaded from "../lib/components/Loaded.svelte";
  import { Busy, Loader } from "../lib/components/work.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { readTextFile } from "../lib/document";
  import type { OrgRef } from "../lib/orgs";
  import { here, navigateFrom } from "../lib/route.svelte";
  import { benchmarkLink, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route: _ }: { route: RouteOf<"benchmark-new"> } = $props();

  const reader = session.account!;
  const gate = new Loader<{ orgs: OrgRef[]; refusal: string }>();
  gate.run(async () => writeOrgs(reader, reader.may.administer_roster === "every" ? await orgs(session.signer, { counts: false }) : []));
  const refusal = $derived(gate.data?.refusal ?? "");

  // By slug, preselected from `?org=` where it names one exactly, else the first.
  let slug = $state(here.query.get("org") ?? "");
  $effect(() => {
    const offered = gate.data?.orgs ?? [];
    if (offered.length && !offered.some((org) => org.slug === slug)) slug = offered[0]!.slug;
  });
  let benchmarkId = $state("");
  let definition = $state("");
  /** The entry an id refused for being kept already opens on, to edit it instead. */
  let kept = $state<string | null>(null);
  /** Why the file picked last was not read. */
  let fileError = $state("");
  const work = new Busy();

  // The file picked last wins, however long an earlier one takes to read.
  const picking = latest();
  async function read(event: Event & { currentTarget: HTMLInputElement }) {
    const file = event.currentTarget.files?.[0];
    if (!file) return;
    const mine = picking();
    fileError = "";
    try {
      const text = await readTextFile(file);
      if (mine()) definition = text;
    } catch (e) {
      if (mine()) fileError = describe(e);
    }
  }

  async function create(event: SubmitEvent, offered: OrgRef[]) {
    event.preventDefault();
    kept = null;
    await work.attempt(async () => {
      const from = here.path;
      const org = offered.find((each) => each.slug === slug);
      if (!org) throw new Error("Pick an organization");
      const id = typedBenchmarkId(benchmarkId);
      try {
        await putBenchmark(session.signer, org.org_id, id, definition, true);
      } catch (e) {
        const refused = createRefused(e, org, id);
        if (!refused) throw e;
        kept = refused.kept;
        throw new Error(refused.message);
      }
      navigateFrom(from, benchmarkLink(org, id));
    });
  }
</script>

<PageHead trail={gate.data && !refusal ? [{ label: "Benchmarks", href: "/app/benchmarks" }] : []} title="New benchmark" />
<Loaded of={gate}>
  {#snippet children({ orgs: offered })}
    {#if refusal}
      <p class="err" role="alert">{refusal}</p>
    {:else}
      <p class="muted">
        A benchmark the organization keeps to itself, as <code>PUT
        /orgs/{"{org}"}/benchmarks/{"{id}"}</code> takes it: the TOML a catalog file holds,
        its <code>benchmark_type</code> and parameters, with the id beside it rather
        than in it. Ids are one namespace across the server, so an id the public
        catalog or another organization holds is refused. So is one this
        organization already keeps: edit that one on its page.
      </p>
      {#if work.error}
        <p class="err code" role="alert">{work.error}{#if kept}{" "}<a href={kept}>Edit it on its page</a>{/if}</p>
      {/if}
      <form onsubmit={(event) => create(event, offered)}>
        <label><span>organization</span>
          <select bind:value={slug}>
            {#each offered as org (org.org_id)}<option value={org.slug}>{org.slug}</option>{/each}
          </select>
        </label>
        <label><span>benchmark id</span>
          <input type="text" bind:value={benchmarkId} required autocomplete="off" spellcheck="false"
                 placeholder="eval_acme_internal_v3">
        </label>
        <label><span>definition</span>
          <textarea bind:value={definition} required spellcheck="false" oninput={() => (fileError = "")} placeholder={'benchmark_type = "eval"\nparameter_eval_id = "acme_internal"\nparameter_dataset_name = "v3"\nparameter_max_tokens = 4096'}></textarea>
        </label>
        <label><span>or read it from a catalog file</span>
          <!-- Cleared as it opens, so the same file picked again reads again and the one read stays named. -->
          <input type="file" accept=".toml,application/toml" onchange={read}
                 onclick={(event) => (event.currentTarget.value = "")}>
        </label>
        {#if fileError}<div class="hint err" role="alert">{fileError}</div>{/if}
        <button type="submit" class="primary" aria-disabled={work.busy}>Create this benchmark</button>
      </form>
    {/if}
  {/snippet}
</Loaded>
