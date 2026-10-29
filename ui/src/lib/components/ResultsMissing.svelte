<script lang="ts">
  // Missing data, a check's page under Results: a section per datum a measured row can lack. The parameter count is the model's, so its
  // section has a block per model descriptor, under its catalog name where it has one, saying where the model comes
  // from and how many jobs and rows lack it, which Jobs lists. Read off the rows, so a model drops off once its rows
  // are filled in.
  import { cellsWith } from "../jobs";
  import { vaultLink } from "../routes";
  import { storedJson, type Missing, type MissingRow, type NamedPart } from "../report";
  import { session } from "../session.svelte";
  import type { TabBase } from "../tabs";
  import CatalogName from "./CatalogName.svelte";
  import DescriptorNames from "./DescriptorNames.svelte";

  let { base, missing, names = {} }: {
    base: TabBase;
    missing: Missing;
    names?: Record<string, string>;
  } = $props();
  const plural = (n: number, noun: string) => `${n} ${noun}${n === 1 ? "" : "s"}`;
  // The page a link would open is for a reader of every plan.
  const readsRows = $derived(session.account?.may.read_plans === "every");

  interface Model {
    descriptor: string;
    /** Its catalog name, as the charts draw it; null for a model that has none. */
    name: NamedPart | null;
    origin: [string, string, string | null][];
    jobs: Set<string>;
    rows: number;
  }

  /** Where a model comes from, as the model parameter table would key it: a vault resource and its digest, or a repo and path. */
  function originOf(descriptor: unknown): [string, string, string | null][] {
    const v = storedJson(descriptor);
    if (typeof v !== "object" || v === null || Array.isArray(v)) return [];
    const d = v as Record<string, unknown>;
    const text = (key: string) => (typeof d[key] === "string" ? (d[key] as string) : null);
    const vault = /\/vault\/resources\/(vault_[^/]+)\//.exec(text("url") ?? "")?.[1] ?? null;
    const repo = text("repo_name") ? `${text("org") ?? ""}/${text("repo_name")}` : null;
    const facts: [string, string | null, string | null][] = [
      ["vault resource", vault, vault ? vaultLink(vault) : null], ["sha256", text("sha256"), null], ["repo", repo, null],
      ["path", text("path"), null],
    ];
    return facts.filter((fact): fact is [string, string, string | null] => fact[1] !== null);
  }

  function models(rows: MissingRow[]): Model[] {
    const byModel = new Map<string, Model>();
    for (const { row, descriptor, name } of rows) {
      let model = byModel.get(descriptor);
      if (!model) {
        model = { descriptor, name, origin: originOf(row.model_descriptor), jobs: new Set(), rows: 0 };
        byModel.set(descriptor, model);
      }
      model.rows++;
      model.jobs.add(row.job_id);
    }
    return [...byModel.values()].sort((a, b) => b.rows - a.rows);
  }
  const lacking = $derived(models(missing.params));
  const jobs = $derived(new Set(missing.params.map(({ row }) => row.job_id)).size);
</script>

<h2>Missing model params</h2>
<p>{plural(lacking.length, "model")} {lacking.length === 1 ? "has" : "have"} no parameter count: {plural(jobs, "job")},
  {missing.params.length} of {missing.measured} measured rows.</p>
<p class="muted">A server administrator adds a row for the model on
  {#if readsRows}<a href="/app/server/model-params">Model params</a>{:else}Model params{/if}. Rows scored after
  that store its count, and these read it from there until <code>fix-model-param</code> stores it on them.</p>

{#each lacking as model (model.descriptor)}
  <section class="model">
    <CatalogName name={model.name?.name ?? null}><DescriptorNames descriptor={model.descriptor} {names} /><code
      class="descriptor">{model.descriptor}</code></CatalogName>
    <dl class="origin">
      {#each model.origin as [name, value, href] (name)}
        <dt>{name}</dt><dd>{#if href}<a class="id" {href}>{value}</a>{:else}<span class="id">{value}</span>{/if}</dd>
      {/each}
      <dt>affects</dt>
      <dd>{plural(model.jobs.size, "job")}, {plural(model.rows, "row")} ·
        <a href={cellsWith(base, { issue: ["params"], model_descriptor: [model.descriptor] })}>See them on Jobs</a></dd>
    </dl>
  </section>
{/each}

<style>
  .model { margin: 16px 0; padding-top: 12px; border-top: 1px solid var(--border); }
  .model code.descriptor { width: auto; }
  .origin { margin: 8px 0; padding: 0; background: none; border: 0; border-radius: 0; }
</style>
