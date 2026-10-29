<script lang="ts">
  // A user pre-auth key just minted (`preauth_minted.html`): its token, shown
  // this once from the answer held in the page's own state, and how to hand it
  // over. On its own after a create; inside the account's pre-auth keys, with
  // `ondone` hiding it, after a mint there.
  import { USERS } from "../crumbs";
  import { when } from "../format";
  import { userLink } from "../routes";
  import type { UserPreauthKeyMinted } from "../api-types";
  import Crumbs from "./Crumbs.svelte";
  import MintedToken from "./MintedToken.svelte";

  let { account, minted, ondone }: {
    account: { user_id: string; contact_email: string };
    minted: UserPreauthKeyMinted;
    ondone?: () => void;
  } = $props();
</script>

{#if !ondone}
  <Crumbs trail={[USERS, { label: account.contact_email, href: userLink(account.user_id) }]} />
{/if}
<MintedToken token={minted.token} {ondone}>
  {#snippet facts()}
    <dt>account</dt><dd>{account.contact_email} <span class="muted id">{account.user_id}</span></dd>
    <dt>key_id</dt><dd class="id">{minted.key_id}</dd>
    <dt>expires</dt><dd>{when(minted.expires_at)}</dd>
    {#if minted.note}<dt>note</dt><dd>{minted.note}</dd>{/if}
  {/snippet}
  {#snippet handover()}
    <p>
      Send it to {account.contact_email} over a channel only they read. They open
      this console's sign-in page and paste it under <strong>Create an account</strong>: their
      browser generates a keypair, registers the public half against the account,
      and signs in. From a terminal,
      <code>pipette-ctl auth register --user-preauth-key</code> takes it too.
    </p>
    <p class="muted">
      It registers one machine, the account's first, and nothing once the account
      holds a key: if someone else registers with it first, the person you meant it
      for is refused and will tell you. If it goes astray before then, revoke it on
      the account's page.
    </p>
    <p class="muted">Once signed in here, they can add a terminal with <code>pipette-ctl auth login</code>, which needs no token.</p>
  {/snippet}
</MintedToken>
