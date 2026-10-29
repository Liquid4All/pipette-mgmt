<script lang="ts">
  // The classic console's frame (`static/templates/layout.html`): header, tree, footer.
  // The classic files the app's comments cite lived under `crates/pipette-console/static/`, and
  // the Rust items they call classic (its pages, forms and views) in `pipette-console`; both went
  // with the console: `git log --diff-filter=D -- crates/pipette-console/static` names the commit.
  import type { Component } from "svelte";
  import { settings as clerkSettings } from "./lib/clerk";
  import LegacyKey from "./lib/components/LegacyKey.svelte";
  import { pageStorage } from "./lib/legacy";
  import { Busy } from "./lib/components/work.svelte";
  import Tree from "./lib/components/Tree.svelte";
  import { follow, here, reveal } from "./lib/route.svelte";
  import { screenOf, type Page, type Route, type RouteOf } from "./lib/routes";
  import { session } from "./lib/session.svelte";
  import Account from "./routes/Account.svelte";
  import Benchmark from "./routes/Benchmark.svelte";
  import BenchmarkNew from "./routes/BenchmarkNew.svelte";
  import Benchmarks from "./routes/Benchmarks.svelte";
  import Campaign from "./routes/Campaign.svelte";
  import CampaignDocument from "./routes/CampaignDocument.svelte";
  import CampaignNew from "./routes/CampaignNew.svelte";
  import Campaigns from "./routes/Campaigns.svelte";
  import CliLogin from "./routes/CliLogin.svelte";
  import Client from "./routes/Client.svelte";
  import Fleet from "./routes/Fleet.svelte";
  import MyCampaigns from "./routes/MyCampaigns.svelte";
  import MyDevices from "./routes/MyDevices.svelte";
  import MyVault from "./routes/MyVault.svelte";
  import NotFound from "./routes/NotFound.svelte";
  import Org from "./routes/Org.svelte";
  import OrgBenchmarks from "./routes/OrgBenchmarks.svelte";
  import OrgCampaigns from "./routes/OrgCampaigns.svelte";
  import OrgDevices from "./routes/OrgDevices.svelte";
  import OrgAudit from "./routes/OrgAudit.svelte";
  import OrgMembers from "./routes/OrgMembers.svelte";
  import OrgNew from "./routes/OrgNew.svelte";
  import OrgVault from "./routes/OrgVault.svelte";
  import Orgs from "./routes/Orgs.svelte";
  import Plan from "./routes/Plan.svelte";
  import Plans from "./routes/Plans.svelte";
  import ServerDevices from "./routes/ServerDevices.svelte";
  import ServerCatalog from "./routes/ServerCatalog.svelte";
  import ServerCatalogEntry from "./routes/ServerCatalogEntry.svelte";
  import ServerModelParams from "./routes/ServerModelParams.svelte";
  import ServerAudit from "./routes/ServerAudit.svelte";
  import ServerUsers from "./routes/ServerUsers.svelte";
  import SignIn from "./routes/SignIn.svelte";
  import User from "./routes/User.svelte";
  import UserNew from "./routes/UserNew.svelte";
  import Users from "./routes/Users.svelte";
  import Vault from "./routes/Vault.svelte";
  import VaultNew from "./routes/VaultNew.svelte";
  import VaultResource from "./routes/VaultResource.svelte";

  type Entry<R> = { component: Component<{ route: R }>; title: string };

  /** Each page's component and title. */
  const PAGES: { [P in Page]: Entry<RouteOf<P>> } & { missing: Entry<Extract<Route, { page: "missing" }>> } = {
    account: { component: Account, title: "Account" },
    "account-keys": { component: Account, title: "Account keys" },
    "login-cli": { component: CliLogin, title: "Approve pipette-ctl" },
    orgs: { component: Orgs, title: "Organizations" },
    "server-users": { component: ServerUsers, title: "Unaffiliated users" },
    "server-devices": { component: ServerDevices, title: "Unclaimed devices" },
    "server-catalog": { component: ServerCatalog, title: "Catalog" },
    "server-catalog-entry": { component: ServerCatalogEntry, title: "Catalog entry" },
    "server-model-params": { component: ServerModelParams, title: "Model params" },
    "server-audit": { component: ServerAudit, title: "Audit log" },
    "vault-new": { component: VaultNew, title: "Upload to the vault" },
    vault: { component: Vault, title: "Vault" },
    "vault-runtimes": { component: Vault, title: "Vault runtimes" },
    "vault-resource": { component: VaultResource, title: "Vault resource" },
    "me-vault": { component: MyVault, title: "Vault" },
    "me-vault-runtimes": { component: MyVault, title: "Vault runtimes" },
    "org-vault": { component: OrgVault, title: "Vault" },
    "org-vault-runtimes": { component: OrgVault, title: "Vault runtimes" },
    clients: { component: Fleet, title: "Fleet" },
    client: { component: Client, title: "Client" },
    "me-devices": { component: MyDevices, title: "Devices" },
    "org-devices": { component: OrgDevices, title: "Devices" },
    users: { component: Users, title: "Users" },
    "user-new": { component: UserNew, title: "New account" },
    user: { component: User, title: "Account" },
    "user-plans": { component: User, title: "Account plans" },
    "user-permissions": { component: User, title: "Account permissions" },
    "user-keys": { component: User, title: "Account keys" },
    "user-preauth": { component: User, title: "Account pre-auth keys" },
    "user-settings": { component: User, title: "Account settings" },
    "org-members": { component: OrgMembers, title: "Members" },
    "org-audit": { component: OrgAudit, title: "Audit" },
    org: { component: Org, title: "Organization" },
    "org-new": { component: OrgNew, title: "New organization" },
    benchmarks: { component: Benchmarks, title: "Benchmarks" },
    "benchmark-new": { component: BenchmarkNew, title: "New benchmark" },
    benchmark: { component: Benchmark, title: "Benchmark" },
    "org-benchmarks": { component: OrgBenchmarks, title: "Benchmarks" },
    plans: { component: Plans, title: "Plans" },
    plan: { component: Plan, title: "Plan" },
    "plan-cells": { component: Plan, title: "Jobs" },
    "plan-results": { component: Plan, title: "Results" },
    "plan-analysis": { component: Plan, title: "Results" },
    "plan-analysis-report": { component: Plan, title: "Results" },
    "plan-reports": { component: Plan, title: "Results" },
    "plan-report": { component: Plan, title: "Results" },
    "plan-revisions": { component: Plan, title: "History" },
    "plan-settings": { component: Plan, title: "Settings" },
    campaigns: { component: Campaigns, title: "Campaigns" },
    "campaign-new": { component: CampaignNew, title: "New campaign" },
    campaign: { component: Campaign, title: "Campaign" },
    "campaign-plan": { component: Campaign, title: "Campaign" },
    "campaign-plan-cells": { component: Campaign, title: "Campaign jobs" },
    "campaign-plan-analysis": { component: Campaign, title: "Campaign results" },
    "campaign-plan-analysis-report": { component: Campaign, title: "Campaign results" },
    "campaign-plan-reports": { component: Campaign, title: "Campaign results" },
    "campaign-plan-report": { component: Campaign, title: "Campaign results" },
    "campaign-plan-revisions": { component: Campaign, title: "Campaign history" },
    "campaign-plan-settings": { component: Campaign, title: "Campaign settings" },
    "campaign-document": { component: CampaignDocument, title: "Campaign document" },
    "me-campaigns": { component: MyCampaigns, title: "Campaigns" },
    "org-campaigns": { component: OrgCampaigns, title: "Campaigns" },
    missing: { component: NotFound, title: "Not found" },
  };

  // Each entry takes its own route, which is the one `here.route` names it for.
  const shown = $derived(PAGES[here.route.page] as Entry<Route>);

  // `src/ui.rs` stamps the build into the page; the Vite dev server does not.
  const stamped = document.querySelector<HTMLMetaElement>('meta[name="pipette-version"]')?.content;
  const version = stamped && !stamped.startsWith("__") ? stamped : "dev";

  session.start();

  // A pending account reads nothing (`GET /api/v1/users/me` alone admits it), so the
  // frame says why once instead of each page refusing.
  const pending = $derived(session.account?.status === "pending");
  const retrying = new Busy();
  const treed = $derived(!!session.account && !pending);

  // Remembered per browser; a narrow window, where the tree stacks above the page, starts folded.
  const FOLDED_ENTRY = "pipette-mgmt.app.tree-folded";
  const storage = pageStorage();
  const stored = (() => { try { return storage?.getItem(FOLDED_ENTRY); } catch { return null; } })();
  let folded = $state(stored ? stored === "1" : matchMedia("(max-width: 760px)").matches);
  function fold() {
    folded = !folded;
    try { storage?.setItem(FOLDED_ENTRY, folded ? "1" : "0"); } catch { /* kept for this page only */ }
  }
  // `[` folds it too, as in Linear, unless the reader is typing.
  function key(event: KeyboardEvent) {
    const target = event.target instanceof HTMLElement ? event.target : null;
    if (event.key !== "[" || event.ctrlKey || event.metaKey || event.altKey || !treed
        || target?.isContentEditable || target?.closest("input, textarea, select")) return;
    event.preventDefault();
    fold();
  }

  $effect(() => {
    document.title = `${!session.identity ? "Sign in" : pending ? "Waiting for approval" : shown.title} · pipette-mgmt`;
  });

  // A page drawn without a read has its anchor at once.
  $effect(() => {
    void here.path;
    reveal();
  });
</script>

<svelte:document onclick={follow} onkeydown={key} />

<header>
  {#if treed}
    <button type="button" class="fold" aria-controls="tree" aria-expanded={!folded}
            title="{folded ? 'Show' : 'Hide'} the menu ([)" onclick={fold}>
      <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
        <rect x="1.5" y="2.5" width="13" height="11" rx="1.5" fill="none" stroke="currentColor" />
        <line x1="6" y1="2.5" x2="6" y2="13.5" stroke="currentColor" />
      </svg>
      <span class="sr">{folded ? "Show the menu" : "Hide the menu"}</span>
    </button>
  {/if}
  <a class="brand" href="/app/">Pipette Console</a>
  {#if session.account}
    <a class="who" href="/app/account" title={session.account.user_id}>{session.account.contact_email}</a>
  {/if}
</header>

<div class="frame">
  {#if treed && session.account}<Tree account={session.account} {folded} />{/if}
  <main>
    {#if !session.ready}
      <p class="muted">Loading…</p>
    {:else if session.identity}
      {#if session.error}
        <p class="err" role="alert">{session.error}</p>
        <div class="bar">
          {#if session.lapsedKey && session.identity.origin === "clerk" && clerkSettings}
            <button type="button" class="primary" aria-disabled={retrying.busy}
                    onclick={() => retrying.attempt(() => session.renewWithClerk())}>Sign in again with Clerk</button>
          {/if}
          <button type="button" aria-disabled={retrying.busy}
                  onclick={() => retrying.attempt(() => session.refresh())}>Try again</button>
          <button type="button" aria-disabled={retrying.busy} onclick={() => session.leave()}>Sign out</button>
        </div>
      {:else if pending}
        <h1>Waiting for approval</h1>
        <p class="notice">
          This account is pending an operator's approval, so it reads nothing here yet. Once an
          operator approves it, check again.
        </p>
        <div class="bar">
          <button type="button" aria-disabled={retrying.busy}
                  onclick={() => retrying.attempt(() => session.refresh())}>Check again</button>
          <button type="button" aria-disabled={retrying.busy} onclick={() => session.leave()}>Sign out</button>
        </div>
      {:else if session.account}
        <LegacyKey />
        <!-- One plan's tabs are one screen, so moving between them keeps what the page read. -->
        {#key screenOf(here.path)}
          <shown.component route={here.route} />
        {/key}
      {/if}
    {:else}
      {#if session.error}<p class="err" role="alert">{session.error}</p>{/if}
      <LegacyKey onsignedin={(next) => session.signedIn(next)} />
      <SignIn onsignedin={(next) => session.signedIn(next)} />
    {/if}
  </main>
</div>

<footer>pipette-mgmt <code>{version}</code></footer>
