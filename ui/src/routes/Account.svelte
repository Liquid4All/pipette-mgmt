<script lang="ts">
  // The classic console's account page (`static/templates/account.html`): the
  // reader's record and session, and on a tab of its own its keys.
  import { accountTabs, keysBeforeFirstSignIn } from "../lib/accounts";
  import { addKey, revokeKey } from "../lib/api";
  import Command from "../lib/components/Command.svelte";
  import KeyTable from "../lib/components/KeyTable.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import StatusPill from "../lib/components/StatusPill.svelte";
  import TabBar from "../lib/components/TabBar.svelte";
  import { Busy } from "../lib/components/work.svelte";
  import { ME } from "../lib/crumbs";
  import { when } from "../lib/format";
  import { browserOnly } from "../lib/identity";
  import { pastedPublicKey } from "../lib/keys";
  import { here } from "../lib/route.svelte";
  import { pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route }: { route: RouteOf<"account" | "account-keys"> } = $props();

  const keys = $derived(route.page === "account-keys");
  // The tree's Account leaf opens either tab.
  $effect(() => { if (keys) here.under = pathOf({ page: "account" }); });

  const identity = $derived(session.signer);
  const account = $derived(session.account!);
  const prior = $derived(keysBeforeFirstSignIn(account));
  const work = new Busy();
  let publicKey = $state("");
  let keyDetails = $state("");
  /** The id the last key this form added registered under. */
  let added = $state<string | null>(null);

  // No passphrase: a program signing with the key cannot type one.
  const KEYGEN = 'ssh-keygen -t ed25519 -N "" -f pipette_ed25519';

  async function add(event: SubmitEvent) {
    event.preventDefault();
    added = null;
    await work.attempt(async () => {
      const { key_id } = await addKey(identity, {
        public_key: pastedPublicKey(publicKey),
        key_details: keyDetails.trim(),
      });
      publicKey = keyDetails = "";
      added = key_id;
      await session.refresh();
    });
  }

  // This browser's own key signs it out rather than reading the account again with a key that no longer signs.
  async function revoke(key_id: string) {
    await revokeKey(identity, key_id);
    if (key_id === added) added = null;
    if (key_id === identity.key_id) await session.signOut();
    else await session.refresh();
  }
</script>

<PageHead trail={ME} title="Account" />
<TabBar label="Account" items={accountTabs(keys)} />
{#if prior.length && account.first_clerk_sign_in}
  <p class="notice" role="status">
    {prior.length === 1 ? "One key was" : `${prior.length} keys were`} registered to this account before
    {when(account.first_clerk_sign_in.at)}, the first sign-in this server recorded, maybe by an administrator. Each
    still signs as you. Revoke any you do not recognise{#if !keys}{" "}on the
    <a href={pathOf({ page: "account-keys" })}>Keys tab</a>{/if}.
  </p>
{/if}

{#if keys}
<p class="muted">
  One key per machine. <code>key_details</code> is what makes revocation
  decidable, because a key id is 64 hex characters and nothing else here says
  which machine it belongs to.
</p>
<KeyTable keys={account.keys} current={identity.key_id} {prior} onrevoke={revoke} busy={work.busy} />
<p class="muted">
  Revoking the last key, or the one this browser signs with, is allowed: a
  holder whose only key is compromised has to be able to retire it at once, and
  a fresh pre-auth key registers another.
</p>

<h2>Add a key</h2>
<p class="muted">
  For another machine. Generate the keypair there, and paste its public half: a
  private key is not something to carry between machines, and this form never
  asks for one. A keypair already registered to any account is refused.
</p>
<p class="muted">For <code>pipette-ctl</code>, run <code>pipette-ctl auth login</code> there instead: it opens a page here to approve its key.</p>
<details>
  <summary>Make a key for a program</summary>
  <p class="muted">On the machine the program runs on:</p>
  <Command command={KEYGEN} />
  <p class="muted">
    Paste the line in <code>pipette_ed25519.pub</code> below. The program signs with
    <code>pipette_ed25519</code>, which stays on that machine. It is in OpenSSH's format, which
    not every Ed25519 library reads.
  </p>
</details>
{#if work.error}<p class="err" role="alert">{work.error}</p>{/if}
{#if added}
  <p class="ok" role="status">Registered as <span class="id">{added}</span>, the <code>keyid</code> its signatures name.</p>
{/if}
<form onsubmit={add}>
  <label><span>public key: 64 hex characters, or an <code>ssh-ed25519</code> line as <code>ssh-add -L</code> prints it</span>
    <input type="text" bind:value={publicKey} autocomplete="off" spellcheck="false"
           placeholder="c8a1… or ssh-ed25519 AAAA…">
  </label>
  <label><span>key details — which machine this is, so the key is revocable</span>
    <input type="text" bind:value={keyDetails} autocomplete="off" spellcheck="false"
           placeholder="build box, us-east-1">
  </label>
  <button type="submit" aria-disabled={work.busy}>Register this key</button>
</form>

<h2>Sign with a key</h2>
<p class="muted">
  A program calls the API as this account by signing each request with one of these keys, as an
  RFC 9421 HTTP Message Signature that names the key's <code>key_id</code> as its
  <code>keyid</code>. The API description lists every route and spells out the signature, with a
  request signed under it: <a href="/docs/">the API reference</a>, or
  <a href="/api/v1/openapi.json">/api/v1/openapi.json</a> for a program.
</p>
{:else}
<dl>
  <dt>user_id</dt><dd class="id">{account.user_id}</dd>
  <dt>contact_email</dt><dd>{account.contact_email}</dd>
  <dt>status</dt><dd><StatusPill status={account.status} /></dd>
  <dt>created_at</dt><dd>{when(account.created_at)}</dd>
  <dt>permissions</dt><dd>{account.permissions.join(", ") || "none granted"}</dd>
</dl>

<h2>This session</h2>
<p class="muted">
  This app signs every request with this browser's key, which it keeps where no
  script can read it out.
  {#if browserOnly(identity.origin)}
    The key was made for this browser alone and expires on its own, and signing out
    revokes it as well as erasing it{identity.origin === "clerk" ? ", and signs this browser out of Clerk" : ""}.
  {:else}
    Signing out erases it from this browser.
  {/if}
</p>
<div class="bar">
  <button type="button" onclick={() => session.leave()}>Sign out</button>
</div>
{/if}
