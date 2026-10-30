<script lang="ts">
  // The classic console's account page (`static/templates/user.html`): the
  // plans the account filed, for any reader of them, and the record for a
  // reader of the roster it is on, with the forms that administer it for its
  // administrator. Each write reads the account again, as the classic form
  // redirects.
  import { untrack } from "svelte";
  import {
    ApiError, account, amendPermissions, mintPreauthKey, orgs, plans, preauthKeys, revokePreauthKey,
    revokeUserKey, setUserOrg, updateUser, userKeys,
  } from "../lib/api";
  import {
    amendment, lookupRefusal, mintable, outranked, permissionChoices,
    readsRoster, retype, standing, tickedAtFirst, unknownStored, userTabOf, userTabs,
  } from "../lib/accounts";
  import { auditAction } from "../lib/audit";
  import CampaignScope from "../lib/components/CampaignScope.svelte";
  import DangerRow from "../lib/components/DangerRow.svelte";
  import KeyTable from "../lib/components/KeyTable.svelte";
  import Loaded from "../lib/components/Loaded.svelte";
  import MintForm from "../lib/components/MintForm.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import PermissionBoxes from "../lib/components/PermissionBoxes.svelte";
  import PlanTable from "../lib/components/PlanTable.svelte";
  import PreauthKeyTable from "../lib/components/PreauthKeyTable.svelte";
  import SettingRow from "../lib/components/SettingRow.svelte";
  import DangerZone from "../lib/components/DangerZone.svelte";
  import SettingsSection from "../lib/components/SettingsSection.svelte";
  import StatusPill from "../lib/components/StatusPill.svelte";
  import TabBar from "../lib/components/TabBar.svelte";
  import UserMinted from "../lib/components/UserMinted.svelte";
  import { Busy, Loader } from "../lib/components/work.svelte";
  import { USERS } from "../lib/crumbs";
  import { DISABLE_ACCOUNT, MAKE_READER } from "../lib/danger";
  import { take } from "../lib/flash";
  import { when } from "../lib/format";
  import { bySlug, type OrgRef } from "../lib/orgs";
  import { filedEmpty, filedQuery, takenScope } from "../lib/plans";
  import { mintBody } from "../lib/preauth";
  import { toggled } from "../lib/rights";
  import { campaignLink, orgLink, pathOf, userLink, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import { ROUTE_PAGE_MAX, everyPage, withCursor, type CursorPage } from "../lib/table";
  import type {
    OrgView, PlanView, UserAccountType, UserDetail, UserKey, UserPermission, UserPreauthKeyMinted,
    UserPreauthKeyView, UserStatus,
  } from "../lib/api-types";

  let { route }: { route: RouteOf<"user" | "user-plans" | "user-permissions" | "user-keys" | "user-preauth" | "user-settings"> } = $props();

  interface Drawn {
    /** `null` for an account on no roster the reader reads. */
    record: UserDetail | null;
    org: OrgRef | null;
    keys: UserKey[];
    preauth: UserPreauthKeyView[];
    /** Where a reader over every organization may move it. */
    orgs: OrgView[];
  }

  const id = $derived(route.user_id);
  const reader = $derived(session.account!);
  const self = $derived(id === reader.user_id);
  const page = new Loader<Drawn>();
  const work = new Busy();
  // A refusal the step that opened this page carried, such as a mint after a create.
  work.error = take(untrack(() => userLink(route.user_id)));

  async function read(): Promise<Drawn> {
    const signer = session.signer;
    const none: Drawn = { record: null, org: null, keys: [], preauth: [], orgs: [] };
    // A reader of no roster is refused the record, so it is not asked.
    if (!readsRoster(reader)) return none;
    let record: UserDetail;
    try {
      record = await account(signer, id);
    } catch (e) {
      // Absent and outside the reader's roster read alike.
      if (e instanceof ApiError && (e.status === 403 || e.status === 404)) return none;
      throw e;
    }
    const admin = record.may_administer;
    const [org, keys, preauth, listed] = await Promise.all([
      record.org_id ? session.resolveOrg(record.org_id) : null,
      admin ? userKeys(signer, id) : [],
      admin ? preauthKeys(signer, id) : [],
      admin && reader.may.administer_roster === "every" ? orgs(signer, { counts: false }) : [],
    ]);
    return { record, org, keys, preauth, orgs: listed.sort(bySlug) };
  }
  page.run(read);

  const record = $derived(page.data?.record ?? null);
  const tab = $derived(userTabOf(route.page));
  const admin = $derived(!!record?.may_administer);

  // The plans it filed, every page, newest first; `null`, with the refusal, for a reader holding no plan grant.
  const filed = new Loader<{ page: CursorPage<PlanView> | null; refused: string }>();
  $effect(() => {
    const query = filedQuery(id, null, ROUTE_PAGE_MAX);
    filed.run(async () => {
      // A reader without the grant is refused, so it is not asked.
      if (reader.may.read_plans === "none") return { page: null, refused: "user lacks the read_plans permission" };
      try {
        return { page: await everyPage((cursor) => plans(session.signer, withCursor(query, cursor))), refused: "" };
      } catch (e) {
        if (e instanceof ApiError && e.status === 403) return { page: null, refused: e.message };
        throw e;
      }
    });
  });
  const refused = $derived(lookupRefusal(reader, page.data, filed.data));
  // Named only from a plan it filed that the page lists, so an id names no account the reader cannot see.
  const address = $derived(filed.data?.page?.names?.[id] ?? null);
  const choices = $derived(record ? permissionChoices(record) : []);
  let ticked = $state(new Set<UserPermission>());
  $effect(() => { ticked = tickedAtFirst(choices); });
  const tick = (name: UserPermission, on: boolean) => { ticked = toggled(ticked, name, on); };

  let moveTo = $state("");
  $effect(() => { moveTo = record?.org_id ?? ""; });

  let scope = $state<string[]>([]);
  $effect(() => { scope = record?.campaign_scope ?? []; });
  const pick = (campaign_id: string, on: boolean) => {
    scope = on ? [...scope, campaign_id] : scope.filter((each) => each !== campaign_id);
  };

  let minted = $state<UserPreauthKeyMinted | null>(null);

  // A write to the reader's own account may change what the frame draws.
  const act = (write: () => Promise<unknown>, what = "") => work.attempt(async () => {
    await write();
    await page.run(read, true);
    if (self) await session.refresh();
  }, what);

  const setStatus = (status: UserStatus) => act(() => updateUser(session.signer, id, { status }));
  const setType = (account_type: UserAccountType) => act(() => updateUser(session.signer, id, { account_type }));

  async function saveScope(event: SubmitEvent) {
    event.preventDefault();
    await act(() => updateUser(session.signer, id, { campaign_scope: scope }));
  }

  async function savePermissions(event: SubmitEvent) {
    event.preventDefault();
    const body = amendment(choices, ticked);
    // A form that changes nothing comes back as it was.
    await act(() => (body ? amendPermissions(session.signer, id, body) : Promise.resolve()));
  }

  async function move(event: SubmitEvent, org_id: string | null) {
    event.preventDefault();
    await act(() => setUserOrg(session.signer, id, { org_id }));
  }

  async function mint(form: { days: number; note: string }) {
    await act(async () => { minted = await mintPreauthKey(session.signer, id, mintBody(form.days, form.note)); });
    return !work.error;
  }

  async function revoke(key_id: string) {
    await revokePreauthKey(session.signer, id, key_id);
    if (minted?.key_id === key_id) minted = null;
    await page.run(read, true);
  }

  // This browser's own key signs it out rather than reading the account again with a key that no longer signs.
  async function revokeKey(key_id: string) {
    await revokeUserKey(session.signer, id, key_id);
    if (key_id === session.signer.key_id) await session.signOut();
    else await page.run(read, true);
  }

  // A danger-zone write throws its refusal into its dialog rather than above the page.
  const disable = async () => {
    await updateUser(session.signer, id, { status: "disabled" });
    await page.run(read, true);
  };
  const makeReader = async () => {
    if (!scope.length) throw new Error("choose the campaigns it reads");
    await updateUser(session.signer, id, { campaign_scope: scope });
    await page.run(read, true);
  };
</script>

<PageHead trail={record ? [USERS] : []}
          title={record?.contact_email ?? address ?? (page.data && !refused ? "Account lookup" : "Account")}
          action={auditAction(reader, id)} />
{#if refused}
  <p class="err" role="alert">{refused}</p>
{:else}
<p class="id">{id}{#if self}{" "}<span class="muted">(you)</span>{/if}</p>
{#if work.error}<p class="err" role="alert">{work.error}</p>{/if}

<TabBar label="Account" items={userTabs(id, tab, admin ? "administers" : record ? "record" : "filed")} />

<!-- Why the record, or a write on it, is not drawn for this reader. -->
{#snippet withheld(drawn: Drawn)}
  {#if drawn.record}
    {#if outranked(reader, drawn.record)}
      <p class="muted">
        It holds server-wide names you do not, so you read it and a server admin administers it.
      </p>
    {:else}
      <p class="muted">You read this account on the roster and do not administer it: that takes <code>manage_orgs</code>.</p>
    {/if}
  {:else if reader.may.read_roster === "every"}
    <p class="muted">No account has this id.</p>
  {:else}
    <p class="muted">
      What this account has filed is on its Plans tab. Its record (its standing, its permissions, its
      keys) is drawn for a reader of the roster it is on, which takes
      <code>read_orgs</code>{#if self}; your own is on the
      <a href="/app/account">account page</a>{/if}.
    </p>
  {/if}
{/snippet}

{#if tab === "plans"}
  <Loaded of={filed}>
    {#snippet children(listed)}
      {#if listed.page}
        <PlanTable plans={listed.page.items} submitters={false} empty={filedEmpty(takenScope(reader, ""))}
                   path={pathOf(route)} />
      {:else}
        <p class="muted">The plans it filed are drawn for a reader holding <code>read_plans</code>.</p>
      {/if}
    {/snippet}
  </Loaded>
{:else}
<Loaded of={page}>
  {#snippet children(drawn)}
    {@const shown = drawn.record}
    {#if tab === "overview"}
      {#if shown}
        <dl>
          <dt>account_type</dt><dd>{shown.account_type}</dd>
          <dt>status</dt><dd><StatusPill status={shown.status} /></dd>
          <dt>organization</dt><dd>
            {#if drawn.org}<a href={orgLink(drawn.org)}>{drawn.org.slug}</a> <span class="muted id">{drawn.org.org_id}</span>
            {:else if shown.org_id}<span class="id">{shown.org_id}</span>
            {:else}<span class="muted">none</span>{/if}
          </dd>
          <dt>created_at</dt><dd>{when(shown.created_at)}</dd>
          <dt>permissions</dt><dd>{shown.permissions.join(", ") || "none granted"}</dd>
          {#if shown.campaign_scope}
            <dt>campaign_scope</dt><dd>
              {#each shown.campaign_scope as campaign_id, i (campaign_id)}{i ? ", " : ""}<a class="id"
                href={campaignLink(campaign_id)}>{campaign_id}</a>{:else}<span class="muted">none</span>{/each}
            </dd>
          {/if}
        </dl>
      {/if}
      {#if !admin}{@render withheld(drawn)}{/if}
    {:else if tab === "permissions" && shown}
      <!-- Read-only for a reader who does not administer it: none offered, and no "implied", since the server
           withholds which names are stored. -->
      {@const boxes = admin ? choices.map((choice) => ({ ...choice,
        checked: choice.offered ? ticked.has(choice.name) : choice.held }))
        : choices.map((choice) => ({ ...choice, offered: false, checked: choice.held, note: null }))}
      {#if admin}
        <p class="muted">
          Tick what the account holds. You grant only names you hold yourself, and a name held through a wider one is
          implied: it goes when that one does.
        </p>
      {:else}{@render withheld(drawn)}{/if}
      <form onsubmit={savePermissions}>
        <PermissionBoxes {boxes} toggle={tick} />
        {#if unknownStored(shown).length}
          <p class="muted">Also stored, and granting nothing to this build: {unknownStored(shown).join(", ")}.</p>
        {/if}
        {#if admin}<button type="submit" aria-disabled={work.busy}>Save permissions</button>{/if}
      </form>
    {:else if !shown || !admin}
      {@render withheld(drawn)}
    {:else if tab === "keys"}
      {#if shown.account_type === "service"}
        <p class="muted">
          The machines the job signs from. It adds none itself: each comes from a
          pre-auth key you mint on its Pre-auth keys tab. Revoking one stops it at once and leaves the
          others signing; disabling the account stops them all.
        </p>
      {:else}
        <p class="muted">
          The machines the account signs in from. Revoking one stops it at once and
          leaves the others signing. Its holder may register another, so to stop the
          person, disable the account: that stops every key until it is enabled.
        </p>
      {/if}
      <KeyTable keys={drawn.keys} current={session.signer.key_id} onrevoke={revokeKey} busy={work.busy} />

    {:else if tab === "preauth"}
      {#if minted}
        <UserMinted account={shown} {minted} ondone={() => (minted = null)} />
      {/if}
      {#if shown.account_type === "service"}
        <p class="muted">
          A pre-auth key registers a machine for the job: it passes the key to
          <code>pipette-ctl auth register --user-preauth-key</code>. A service account is
          its organization's, so you may mint one at any time, for an account holding
          nothing you do not. Minting one revokes any other an administrator minted.
        </p>
      {:else}
        <p class="muted">
          A pre-auth key registers the account's first machine: its holder pastes it on
          the sign-in page, or passes it to
          <code>pipette-ctl auth register --user-preauth-key</code>. Whoever holds one
          signs in as the account, so it is minted only for an account nobody has signed
          in as yet, holding nothing you do not, and it admits nothing once somebody
          has. Minting one revokes any other an administrator minted.
        </p>
      {/if}
      <PreauthKeyTable keys={drawn.preauth} busy={work.busy} onrevoke={revoke}
                       empty="None outstanding. One is gone once it registers a machine." />
      {#if !mintable(shown.account_type, !!shown.awaits_sign_in)}
        <p class="muted">
          Somebody has signed in as this account, or it was created before first
          sign-ins were recorded, so there is no first machine left to admit: its holder
          adds machines itself, and one who lost every key is let back in on the host.
        </p>
      {:else}
        <MintForm action="Mint a pre-auth key" noteLabel="note: who it is for, shown in this listing"
                  placeholder="for the new hire" busy={work.busy} onmint={mint} />
      {/if}

    {:else}
      {@const step = standing(shown.status, self)}
      {@const kind = retype(shown.account_type, !!shown.awaits_sign_in, !!shown.campaign_scope)}
      <div class="settings">
        <SettingsSection title="General">
          <!-- Disabling is the danger zone's; admitting and enabling are not. -->
          {#if !step.danger}
            <SettingRow title="Standing" text={step.note}>
              {#snippet control(ids)}
                {#if step.to}
                  {@const to = step.to}
                  <button type="button" aria-describedby={ids.text} aria-disabled={work.busy}
                          onclick={() => setStatus(to)}>{step.label}</button>
                {/if}
              {/snippet}
            </SettingRow>
          {/if}
          <SettingRow title="Type" text={kind.note}>
            {#snippet control(ids)}
              {#if kind.to}
                {@const to = kind.to}
                <button type="button" aria-describedby={ids.text} aria-disabled={work.busy}
                        onclick={() => setType(to)}>{kind.label}</button>
              {/if}
            {/snippet}
          </SettingRow>
          {#if shown.campaign_scope}
            <SettingRow title="Campaigns" text="A campaign reader: it reads only these campaigns and holds read_plans alone. Saving replaces the list; nothing makes it a member of its organization again.">
              {#snippet control(ids)}
                <form class="bar" onsubmit={saveScope}>
                  <CampaignScope org={shown.org_id ?? ""} chosen={scope} toggle={pick} />
                  <button type="submit" aria-describedby={ids.text} aria-disabled={work.busy}>Save campaigns</button>
                </form>
              {/snippet}
            </SettingRow>
          {/if}
          <SettingRow title="Organization" text={drawn.orgs.length ? "The organization the account belongs to."
            : self ? "You cannot take your own account out of the organization you administer from here."
            : "Taking the account out of your organization leaves it in none, where only a server admin reaches it: you no longer administer it afterwards."}>
            {#snippet control(ids)}
              {#if drawn.orgs.length}
                <form class="bar" onsubmit={(event) => move(event, moveTo || null)}>
                  <select bind:value={moveTo} aria-label="organization" aria-describedby={ids.text}>
                    <option value="">no organization</option>
                    {#each drawn.orgs as each (each.org_id)}<option value={each.org_id}>{each.slug}</option>{/each}
                  </select>
                  <button type="submit" aria-disabled={work.busy}>Move</button>
                </form>
              {:else if !self}
                <form onsubmit={(event) => move(event, null)}>
                  <button type="submit" aria-describedby={ids.text} aria-disabled={work.busy}>Remove from {
                    drawn.org?.slug ?? "the organization"}</button>
                </form>
              {/if}
            {/snippet}
          </SettingRow>
        </SettingsSection>
      </div>

      <!-- Offboarding is a disable: nothing here deletes an account, and nobody disables their own. -->
      {@const convertible = shown.account_type === "service" && !shown.campaign_scope}
      {#if step.danger || convertible}
        <div class="settings">
          <DangerZone>
            {#if convertible}
              <DangerRow title="Make it a campaign reader" ask={`Make ${shown.contact_email} a campaign reader?`}
                         text="For a job that publishes results: it reads only the campaigns chosen here and holds read_plans alone. It must hold nothing else first, and stays a reader: a wider account is a new account."
                         pending="Saving…" phrase={MAKE_READER} run={makeReader} locked={work.busy}>
                {#snippet fields()}<CampaignScope org={shown.org_id ?? ""} chosen={scope} toggle={pick} />{/snippet}
              </DangerRow>
            {/if}
            {#if step.danger}
              <DangerRow title={step.label} ask={`Disable ${shown.contact_email}?`} text={step.note}
                         pending="Disabling…" phrase={DISABLE_ACCOUNT} run={disable} locked={work.busy} />
            {/if}
          </DangerZone>
        </div>
      {/if}
    {/if}
  {/snippet}
</Loaded>
{/if}
{/if}
