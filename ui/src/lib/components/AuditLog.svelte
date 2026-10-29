<script lang="ts">
  // One page of the audit log, newest first: an organization's
  // (`GET /api/v1/orgs/{org}/audit`), or the whole log (`GET /api/v1/audit`),
  // under filters the server applies and the address keeps.
  import { audit, orgAudit } from "../api";
  import {
    ACTOR_KINDS, OUTCOMES, actionShown, actorShown, auditFilters, auditLink, auditQuery, effectiveActors, offered,
    targetShown, toggled, type AuditFilters, type Shown,
  } from "../audit";
  import { when } from "../format";
  import { here, redirect } from "../route.svelte";
  import { session } from "../session.svelte";
  import { Typing } from "../typing";
  import Loaded from "./Loaded.svelte";
  import MultiSelect from "./MultiSelect.svelte";
  import Pager from "./Pager.svelte";
  import StatusPill from "./StatusPill.svelte";
  import { Loader } from "./work.svelte";
  import type { NamedAuditEventPage } from "../api-types";

  let { org_id = null, path }: { org_id?: string | null; path: string } = $props();

  const filters = $derived(auditFilters(here.query));
  const actors = $derived(effectiveActors(filters));
  const cursor = $derived(here.query.get("cursor"));

  // Read again whenever the query it sends changes.
  const asked = $derived(auditQuery(filters, cursor));
  const page = new Loader<NamedAuditEventPage>();
  $effect(() => {
    const query = asked;
    const org = org_id;
    page.run(() => (org ? orgAudit(session.signer, org, query) : audit(session.signer, query)));
  });

  // A filter changed starts over from the newest event.
  const apply = (to: AuditFilters) => redirect(auditLink(path, to));
  const toggle = (key: "outcome" | "actor") => (value: string, on: boolean) => apply(toggled(filters, key, value, on));
  const typing = new Typing();
  $effect(() => () => typing.clear());
</script>

{#snippet cell(shown: Shown)}
  {#if shown.href}<a class:id={shown.id} href={shown.href} title={shown.title}>{shown.text}</a>
  {:else}<span class:id={shown.id} title={shown.title}>{shown.text}</span>{/if}
{/snippet}

<!-- Outside `Loaded`, so a refused id stays in the box to correct. -->
<div class="bar filters">
  <MultiSelect label="outcome" options={offered(OUTCOMES, filters.outcome)} chosen={filters.outcome}
               toggle={toggle("outcome")} />
  <MultiSelect label="actor" options={offered(ACTOR_KINDS, actors)} chosen={actors} toggle={toggle("actor")} />
  <label><span>target</span>
    <input type="text" value={filters.target} placeholder="any record id" autocomplete="off" spellcheck="false"
           oninput={(e) => {
             const value = e.currentTarget.value;
             typing.later("target", () => apply({ ...filters, target: value.trim() }));
           }}
           onchange={() => typing.flush("target")} /></label>
</div>

<Loaded of={page}>
  {#snippet children(view)}
    {@const names = view.names ?? {}}
    {#if view.unreadable}<p class="muted">{view.unreadable} event(s) did not read.</p>{/if}
    {#if view.items.length}
      <div class="wide">
        <table>
          <thead><tr><th>created_at</th><th>outcome</th><th>actor</th><th>action</th><th>targets</th></tr></thead>
          <tbody>
            {#each view.items as event (event.event_id)}
              {@const action = actionShown(event)}
              <tr>
                <td title={event.event_id}>{when(event.created_at)}</td>
                <td><StatusPill status={event.outcome} /></td>
                <td>{@render cell(actorShown(event.actor, names))}</td>
                <td title={action.title}>{action.text}{#if action.status !== null}{" "}<span
                  class="muted">→ {action.status}</span>{/if}</td>
                <td>{#each event.targets as target, index (index)}{#if index}{", "}{/if}{@render
                  cell(targetShown(target, names))}{/each}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {:else}
      <p class="muted">No {cursor ? "older " : ""}events match these filters.</p>
    {/if}
    <Pager {cursor} first={auditLink(path, filters)}
           next={view.next_cursor ? auditLink(path, filters, view.next_cursor) : null} />
  {/snippet}
</Loaded>

<style>
  .filters input { width: 22em; }
</style>
