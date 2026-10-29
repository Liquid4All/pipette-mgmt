<script lang="ts">
  // The classic console's plan listing (`static/templates/plans.html`): every
  // page of `GET /api/v1/plans` newest first, narrowed by scope and by the submitter or
  // campaign a link carried, each submitter by address; the rest is filtered,
  // sorted and paged in `PlanTable`.
  import { plans } from "../lib/api";
  import Loaded from "../lib/components/Loaded.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import PlanTable from "../lib/components/PlanTable.svelte";
  import { Loader } from "../lib/components/work.svelte";
  import { filterPairs, planFilters, planQuery, takenScope } from "../lib/plans";
  import { here, href, navigate } from "../lib/route.svelte";
  import { campaignLink, pathOf, userLink, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import { ROUTE_PAGE_MAX, everyPage, withCursor, type CursorPage } from "../lib/table";
  import type { PlanView } from "../lib/api-types";

  let { route }: { route: RouteOf<"plans"> } = $props();

  const path = $derived(pathOf(route));
  // The status is the table's filter; the server reads the rest.
  const filters = $derived({ ...planFilters(here.query), status: "" });
  const kept = $derived(filterPairs(filters).filter(([key]) => key !== "status"));
  const asked = $derived(planQuery(filters, null, ROUTE_PAGE_MAX));
  const reader = $derived(session.account!);

  // Read again whenever the query it sends changes, not on a filter the table applies.
  const read = new Loader<CursorPage<PlanView>>();
  $effect(() => {
    const query = asked;
    read.run(() => everyPage((cursor) => plans(session.signer, withCursor(query, cursor))));
  });

  /** The listing at another scope, the table's filters and sort kept, from its first row. */
  function scoped(scope: string) {
    const query = new URLSearchParams(here.query);
    query.delete("offset");
    query.set("scope", scope);
    return href(path, [...query]);
  }
</script>

<PageHead title="Plans" />

<div class="bar">
  <!-- Only a reader of every plan has a scope to choose; one carried in a link stays clearable. -->
  {#if reader.may.read_plans === "every" || filters.scope}
    <label><span>scope</span>
      <select value={filters.scope} onchange={(e) => navigate(scoped(e.currentTarget.value))}>
        <option value="">widest I may see</option>
        <option value="mine">just mine</option>
        <option value="all">all plans</option>
      </select>
    </label>
  {/if}
  {#if !read.error}<span class="muted">scope: {takenScope(reader, filters.scope)}</span>{/if}
  {#if filters.campaign_id}
    <span class="muted">campaign <a class="id" href={campaignLink(filters.campaign_id)}>{filters.campaign_id}</a></span>
  {/if}
  {#if filters.submitted_by}
    <span class="muted">submitted by <a href={userLink(filters.submitted_by)}>{
      read.data?.names?.[filters.submitted_by] ?? filters.submitted_by}</a></span>
  {/if}
  {#if read.data?.unreadable}
    <span class="err">{read.data.unreadable} unreadable manifest(s)</span>
  {/if}
</div>

<Loaded of={read}>
  {#snippet children(listed)}
    <PlanTable plans={listed.items} names={listed.names ?? {}} submitters empty="No plans in this scope." {path} {kept} />
  {/snippet}
</Loaded>
