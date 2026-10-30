<script lang="ts">
  // The classic console's new-account form (`static/templates/new_user.html`),
  // then its first pre-auth key (`preauth_minted.html`). Two calls where the
  // classic form posted once, so both bodies are checked before the create, and
  // a mint refused after it opens the new account's page with the refusal.
  import { createUser, mintPreauthKey, orgs } from "../lib/api";
  import { createRefusal, grantable, newAccount } from "../lib/accounts";
  import CampaignScope from "../lib/components/CampaignScope.svelte";
  import { describe } from "../lib/components/guard";
  import Loaded from "../lib/components/Loaded.svelte";
  import UserMinted from "../lib/components/UserMinted.svelte";
  import { Busy, Loader } from "../lib/components/work.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { USERS } from "../lib/crumbs";
  import { carry } from "../lib/flash";
  import { bySlug } from "../lib/orgs";
  import { EXPIRY_DAYS, EXPIRY_DEFAULT, days } from "../lib/preauth";
  import { here, navigateFrom } from "../lib/route.svelte";
  import { pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import type { OrgView, UserAccountType, UserPermission, UserPreauthKeyMinted, UserView } from "../lib/api-types";

  let { route: _ }: { route: RouteOf<"user-new"> } = $props();

  const reader = $derived(session.account!);
  const refusal = $derived(createRefusal(reader));
  const fleet = $derived(reader.may.administer_roster === "every");

  const offered = new Loader<OrgView[]>();
  $effect(() => {
    if (!refusal && fleet) {
      offered.run(async () => (await orgs(session.signer, { counts: false })).sort(bySlug));
    }
  });

  let contactEmail = $state("");
  let orgId = $state("");
  let accountType = $state<UserAccountType>("human");
  let ticked = $state<UserPermission[]>([]);
  let campaignReader = $state(false);
  let scope = $state<string[]>([]);
  const reads = $derived(accountType === "service" && campaignReader);
  const offeredNames = $derived(grantable(reader, accountType, reads));
  // Another organization's campaigns are none it may read.
  $effect(() => { void orgId; scope = []; });
  let invite = $state(true);
  let expiresInDays = $state(EXPIRY_DEFAULT);
  let created = $state<{ account: UserView; minted: UserPreauthKeyMinted } | null>(null);
  const work = new Busy();

  const tick = (name: UserPermission, on: boolean) => {
    ticked = on ? [...ticked, name] : ticked.filter((each) => each !== name);
  };
  const pick = (id: string, on: boolean) => {
    scope = on ? [...scope, id] : scope.filter((each) => each !== id);
  };
  // A reader with nothing ticked would read nothing, so it starts with the one name it may hold.
  const becomeReader = (on: boolean) => {
    campaignReader = on;
    if (on && !ticked.includes("read_plans")) ticked = [...ticked, "read_plans"];
  };

  async function create(event: SubmitEvent) {
    event.preventDefault();
    await work.attempt(async () => {
      const { create, mint } = newAccount({
        contact_email: contactEmail, org_id: fleet ? orgId : "", account_type: accountType,
        permissions: offeredNames.filter((name) => ticked.includes(name)), campaign_scope: reads ? scope : null,
        invite, expires_in_days: expiresInDays,
      });
      const from = here.path;
      const account = await createUser(session.signer, create);
      const page = pathOf({ page: "user", user_id: account.user_id });
      if (!mint) return navigateFrom(from, page);
      // The account stands either way, so a refused mint is shown on its page.
      try {
        created = { account, minted: await mintPreauthKey(session.signer, account.user_id, mint) };
      } catch (e) {
        carry(page, describe(e));
        navigateFrom(from, page);
      }
    });
  }
</script>

{#if created}
  <UserMinted account={created.account} minted={created.minted} />
{:else}
  <PageHead trail={refusal ? [] : [USERS]} title="New account" />
  {#if refusal}
    <p class="err" role="alert">{refusal}</p>
  {:else}
    <p class="muted">
      The account is <code>active</code> from the moment it exists, and holds what
      you tick here, never more than you hold yourself. It holds no key yet: it
      registers its first one with a pre-auth key, which this form can mint in the
      same step for you to hand to the person.
    </p>
    {#if work.error}<p class="err" role="alert">{work.error}</p>{/if}
    <form onsubmit={create}>
      <label><span>contact email: the address the account is known by, one account per address</span>
        <input type="text" bind:value={contactEmail} required autocomplete="off" spellcheck="false"
               placeholder="person@example.test">
      </label>
      {#if fleet}
        <Loaded of={offered}>
          {#snippet children(listed)}
            <label><span>organization</span>
              <select bind:value={orgId}>
                <option value="">no organization</option>
                {#each listed as each (each.org_id)}<option value={each.org_id}>{each.slug}</option>{/each}
              </select>
            </label>
          {/snippet}
        </Loaded>
      {:else if reader.org_slug}
        <p>It joins <strong>{reader.org_slug}</strong>, the organization you administer.</p>
      {/if}

      <fieldset class="choices">
        <legend>type</legend>
        <label class="check"><input type="radio" bind:group={accountType} value="human"> a person's</label>
        <label class="check"><input type="radio" bind:group={accountType} value="service">
          a service's: a job signs as it with its own keys, never through Clerk</label>
        {#if accountType === "service"}
          <label class="check"><input type="checkbox" checked={campaignReader}
            onchange={(e) => becomeReader(e.currentTarget.checked)}>
            a campaign reader: it reads only the campaigns chosen here and holds <code>read_plans</code> alone,
            for a job that publishes them</label>
          {#if reads}
            {@const org = fleet ? orgId : (reader.org_id ?? "")}
            {#if org}<CampaignScope {org} chosen={scope} toggle={pick} />
            {:else}<p class="muted">Choose its organization first: it reads that organization's campaigns.</p>{/if}
          {/if}
        {/if}
      </fieldset>

      <fieldset class="choices">
        <legend>permissions</legend>
        {#each offeredNames as name (name)}
          <label class="check"><input type="checkbox" checked={ticked.includes(name)}
            onchange={(e) => tick(name, e.currentTarget.checked)}> <code>{name}</code></label>
        {/each}
      </fieldset>

      <fieldset class="choices">
        <legend>pre-auth key</legend>
        <label class="check"><input type="checkbox" bind:checked={invite}>
          Mint a single-use pre-auth key for it now</label>
        <label><span>which expires in</span>
          <select bind:value={expiresInDays}>
            {#each EXPIRY_DAYS as n (n)}<option value={n}>{days(n)}</option>{/each}
          </select>
        </label>
      </fieldset>
      <button type="submit" class="primary" aria-disabled={work.busy}>Create this account</button>
    </form>
  {/if}
{/if}
