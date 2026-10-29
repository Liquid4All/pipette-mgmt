<script lang="ts">
  // The classic console's tree (`static/templates/layout.html`), leaf for leaf.
  import { administersOrg, ownOrg, type OrgRef } from "../orgs";
  import { here } from "../route.svelte";
  import { match, orgLink, orgOf, pathOf } from "../routes";
  import { session } from "../session.svelte";
  import { canonical, meLeaves, orgBranches, orgLeaves, serverLeaves, type Leaf } from "../tree";
  import type { OwnUserView } from "../api-types";
  import { latest } from "./guard";

  let { account, folded = false }: { account: OwnUserView; folded?: boolean } = $props();

  const reads = $derived({ roster: account.may.read_roster !== "none", fleet: account.may.read_fleet,
                           audit: false });
  // Per organization: a reader administers only its own, unless it administers every one.
  const readsOf = (org: OrgRef) => ({ ...reads, audit: administersOrg(account, org.org_id) });
  const everyAudit = $derived(account.may.administer_roster === "every");
  const everyOrg = $derived(account.may.read_roster === "every");
  const everyPlan = $derived(account.may.read_plans === "every");
  const own = $derived(ownOrg(account));

  // The organization the page sits under, resolved when the tree does not list it;
  // a record's page sits under its listing's.
  let at = $state<OrgRef | null>(null);
  const resolving = latest();
  $effect(() => {
    const reference = orgOf(here.under ? match(here.under) : here.route);
    const mine = resolving();
    const known = reference ? session.knownOrg(reference) : null;
    at = known;
    if (reference && !known) {
      // A failed read draws no visiting branch; the page itself says why.
      session.resolveOrg(reference).catch(() => null).then((found) => { if (mine()) at = found; });
    }
  });

  const branches = $derived(orgBranches(own, session.others, at));
  // A leaf is current when it opens this page, under whichever address it was reached.
  const shown = $derived(here.under ?? (here.route.page === "missing" ? null : pathOf(canonical(here.route, at))));
  const current = (leaf: Leaf) => (leaf.href === shown ? "true" : undefined);
</script>

{#snippet leaves(list: Leaf[])}
  {#each list as leaf (leaf.href)}
    <li><a href={leaf.href} aria-current={current(leaf)}>{leaf.label}</a></li>
  {/each}
{/snippet}

<nav class="tree" class:folded id="tree" aria-label="Console">
  <p class="branch">Me</p>
  <ul>{@render leaves(meLeaves(reads))}</ul>
  {#if branches.length}
    <p class="branch">Organizations</p>
    <ul>
      {#each branches as { org, open } (org.org_id)}
        {#if open}
          <li>
            <span class="org">{org.slug}</span>
            <ul>{@render leaves(orgLeaves(org, readsOf(org)))}</ul>
          </li>
        {:else}
          <li><a href={orgLink(org)}>{org.slug}</a></li>
        {/if}
      {/each}
    </ul>
  {/if}
  <p class="branch">Server</p>
  <ul>{@render leaves(serverLeaves(everyOrg, reads.fleet, everyPlan, everyAudit))}</ul>
</nav>
