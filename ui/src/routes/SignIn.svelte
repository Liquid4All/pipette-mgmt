<script lang="ts">
  // Signing in and signing up, each with the ways this server offers.
  import { register } from "../lib/api";
  import { CLERK_RETURN, loadClerk, returned, settings as clerkSettings } from "../lib/clerk";
  import { save, type Identity } from "../lib/identity";
  import { describeBrowser, downloadSeed, generateKey, type GeneratedKey } from "../lib/keys";
  import { pageStorage } from "../lib/legacy";
  import { here } from "../lib/route.svelte";
  import { BROWSER_KEY_DAYS, signInWithAddedKey, signInWithClerk, signInWithOwnKey } from "../lib/signin";
  import Command from "../lib/components/Command.svelte";
  import { Busy } from "../lib/components/work.svelte";

  let { onsignedin }: { onsignedin: (identity: Identity) => void } = $props();

  type Mode = "signin" | "signup";
  type Method = "agent" | "file" | "browser";

  let mode = $state<Mode>("signin");
  // With Clerk offered, its button leads; no key way opens until picked.
  let method = $state<Method | null>(clerkSettings ? null : "agent");
  /** How a new reader joins: Clerk makes the account at first sign-in; a token names one an operator made. */
  let joining = $state<"clerk" | "token" | null>(clerkSettings ? null : "token");
  const work = new Busy();

  let secret = $state("");
  let token = $state("");
  let details = $state("");
  /** The key this browser makes for `pipette-ctl auth keys add` to register. */
  let delegated = $state<GeneratedKey | null>(null);
  /** A key registered at sign-up, held until the reader has saved it. */
  let pending = $state<{ key: GeneratedKey; user_id: string } | null>(null);
  let downloaded = $state(false);

  const browserName = describeBrowser();
  // Arrived from `pipette-ctl auth login`: this browser signs in to approve that terminal, not instead of it.
  const approving = $derived(here.route.page === "login-cli");

  const addKeyCommand = $derived(delegated
    ? `pipette-ctl auth keys add --public-key ${delegated.publicKey} --key-details "${browserName}" --expires-in ${BROWSER_KEY_DAYS}d`
    : "");
  // `init` makes the workspace the identity lives in, and is a no-op in one.
  const registerCommand = $derived(`pipette-ctl init && PIPETTE_USER_PREAUTH_KEY=${
    token.trim() || "<pre-auth token>"} pipette-ctl auth register --server ${location.origin}`);

  function choose(next: Mode, nextMethod: Method | null) {
    mode = next;
    method = nextMethod;
    work.error = "";
  }

  function switchTo(next: Mode) {
    choose(next, next === "signup" || clerkSettings ? null : "agent");
    joining = clerkSettings ? null : "token";
  }

  // The agent path needs this browser's own key before it can print the command.
  $effect(() => {
    if (mode === "signin" && method === "agent" && !delegated) {
      generateKey().then((key) => (delegated = key)).catch((e) => (work.error = String(e)));
    }
  });

  async function finish(identity: Identity) {
    await save(identity);
    onsignedin(identity);
  }

  // ── sign in ──────────────────────────────────────────────────────────────

  const signInWithAgent = () => work.attempt(async () => {
    if (!delegated) throw new Error("This browser's key is still being made.");
    await finish(await signInWithAddedKey(delegated, browserName));
  });

  const signInWithFile = (event: SubmitEvent) => {
    event.preventDefault();
    return work.attempt(async () => {
      if (!secret.trim()) throw new Error("Paste the private key, or pick the file it is in.");
      const identity = await signInWithOwnKey(secret, browserName);
      secret = "";
      await finish(identity);
    });
  };

  async function pick(event: Event) {
    const file = (event.currentTarget as HTMLInputElement).files?.[0];
    if (file) secret = (await file.text()).trim();
  }

  const signInThroughClerk = () => work.attempt(async () => {
    const identity = await signInWithClerk(await loadClerk(), browserName, location.href, pageStorage());
    if (identity) await finish(identity);
  });

  // Back from Clerk's page: finish without a second click, and never go round again.
  if (clerkSettings && new URLSearchParams(location.search).has(CLERK_RETURN)) {
    // Deferred: `here.sync()` writes state the frame is rendering from, which Svelte refuses mid-render.
    queueMicrotask(() => {
      history.replaceState(null, "", returned(location.href));
      here.sync();
    });
    work.attempt(async () => {
      const clerk = await loadClerk();
      if (!clerk.isSignedIn) throw new Error("Clerk did not sign you in. Try again, or use a key.");
      const identity = await signInWithClerk(clerk, browserName, location.href, pageStorage());
      if (identity) await finish(identity);
    });
  }

  // ── sign up ──────────────────────────────────────────────────────────────

  const signUpInBrowser = (event: SubmitEvent) => {
    event.preventDefault();
    return work.attempt(async () => {
      if (!token.trim()) throw new Error("Paste the pre-auth token first.");
      if (!details.trim()) throw new Error("Say which machine this is, so the key is revocable.");
      const key = await generateKey();
      const receipt = await register({
        public_key: key.publicKey, user_preauth_key: token.trim(), key_details: details.trim(),
      });
      pending = { key, user_id: receipt.user_id };
    });
  };

  function download() {
    if (!pending) return;
    downloadSeed(pending.key.seed);
    downloaded = true;
  }

  async function finishSignUp() {
    if (!pending) return;
    const { key: { key_id, privateKey }, user_id } = pending;
    await finish({ key_id, privateKey, user_id, origin: "generated" });
  }
</script>

<div class="narrow">
<div class="card">
{#if mode === "signin"}
  <h1>Sign in to Pipette Console</h1>
  <p class="muted lead">
    Whichever way you pick, this browser gets a key of its own that expires in {BROWSER_KEY_DAYS} days.
    Signing out revokes it sooner.
  </p>
  {#if approving}
    <p class="notice">Sign in to approve the key <code>pipette-ctl auth login</code> asked for.</p>
  {/if}
{:else}
  <h1>Create an account</h1>
  {#if !clerkSettings}
    <p class="muted lead">You need a pre-auth token from an operator. It names the account you create.</p>
  {/if}
{/if}

<p class="err" role="alert" hidden={!work.error}>{work.error}</p>

{#if mode === "signin"}
  {#if clerkSettings}
    <button type="button" class="primary wide-button" aria-disabled={work.busy}
            onclick={signInThroughClerk}>Continue with Clerk</button>
    <p class="muted small">With your organization's account, such as Google. Clerk opens its own page and sends you back.</p>
    <div class="or" role="separator">or sign in with a key</div>
  {/if}

  <fieldset class="choices">
    <legend class="sr">Where your key is</legend>
    <label class="choice">
      <input type="radio" name="signin-method" value="agent" bind:group={method}>
      <span><strong>pipette-ctl on this machine</strong>
        <span class="muted">Run one command where <code>pipette-ctl</code> already signs as you, ssh-agent keys included.</span></span>
    </label>
    <label class="choice">
      <input type="radio" name="signin-method" value="file" bind:group={method}>
      <span><strong>Private key file</strong>
        <span class="muted">Pick or paste an <code>id_ed25519</code>, such as the one this page saved at sign-up.</span></span>
    </label>
  </fieldset>

  {#if method === "agent"}
    <div class="panel">
      {#if delegated}
        <p class="step">1. Run this where <code>pipette-ctl</code> signs as you: the directory you file plans from,
          or pass <code>--work-dir</code>.</p>
        <Command command={addKeyCommand} />
        <p class="step">2. Then come back here.</p>
        <button type="button" class="primary" aria-disabled={work.busy} onclick={signInWithAgent}>I ran it, sign in</button>
        {#if !approving}
          <p class="muted small">To sign <code>pipette-ctl</code> in rather than this browser, run <code>pipette-ctl auth login</code>.</p>
        {/if}
        <details>
          <summary>What the command does</summary>
          <p class="muted">
            It adds this browser's own key to your account, signed by the key <code>pipette-ctl</code>
            already uses, which stays where it is, untouched. <code>pipette-ctl auth me</code> shows the
            account it signs as. Raise <code>--expires-in</code> to keep the browser's key up to 30 days.
            An older <code>pipette-ctl</code> (or <code>pipette-plan</code>, its former name) that does not
            know <code>--expires-in</code>: drop it, and signing in swaps the key for one that expires.
          </p>
        </details>
      {:else}
        <p class="muted">Making this browser's key…</p>
      {/if}
    </div>
  {:else if method === "file"}
    <form class="panel" onsubmit={signInWithFile}>
      <label><span>Key file</span><input type="file" onchange={pick}></label>
      <label><span>or paste the private key (64 hex characters)</span>
        <input type="password" bind:value={secret} placeholder="64 hex characters"
               autocomplete="off" spellcheck="false">
      </label>
      <button type="submit" class="primary" aria-disabled={work.busy}>Sign in with this key</button>
      <p class="muted small">
        The key is read here and sent nowhere; this browser keeps only a key of its own, and yours
        stays registered. A key held by an ssh-agent cannot be read out: use <strong>pipette-ctl on this machine</strong>.
      </p>
    </form>
  {/if}

  <p class="switch">New here? <button type="button" class="again" onclick={() => switchTo("signup")}>Create an account</button></p>
{:else if pending}
  <h2>Save your key</h2>
  <p class="muted">
    This browser registered a key for <span class="id">{pending.user_id}</span>. It keeps the key
    where no script can read it out, so this is the only time it can be saved. Without a copy,
    signing out locks you out until an operator lets you back in.
  </p>
  <div class="bar">
    <button type="button" class="primary" onclick={download}>Save as id_ed25519</button>
    <button type="button" onclick={finishSignUp} disabled={!downloaded}>Continue</button>
  </div>
{:else}
  {#if clerkSettings}
    <fieldset class="choices">
      <legend class="muted small">How are you joining?</legend>
      <label class="choice">
        <input type="radio" name="joining" value="clerk" bind:group={joining}>
        <span><strong>With my organization's account</strong>
          <span class="muted">Sign in through Clerk, such as with Google. Your account is made at the first sign-in; no token needed.</span></span>
      </label>
      <label class="choice">
        <input type="radio" name="joining" value="token" bind:group={joining}>
        <span><strong>With a pre-auth token</strong>
          <span class="muted">An operator sent you a <code>user_preauth_…</code> token for an account they made.</span></span>
      </label>
    </fieldset>
  {/if}

  {#if joining === "clerk"}
    <div class="panel">
      <button type="button" class="primary wide-button" aria-disabled={work.busy}
              onclick={signInThroughClerk}>Continue with Clerk</button>
    </div>
  {:else if joining === "token"}
    <ol class="steps">
      <li>
        <label><span class="step-title">Paste your pre-auth token</span>
          <input type="text" bind:value={token} placeholder="user_preauth_…" autocomplete="off" spellcheck="false">
        </label>
      </li>
      <li>
        <fieldset class="choices">
          <legend class="step-title">Choose where your key will live</legend>
          <label class="choice">
            <input type="radio" name="signup-method" value="agent" bind:group={method}>
            <span><strong>pipette-ctl on this machine</strong>
              <span class="muted">Register a key the machine already has, in its ssh-agent or <code>~/.ssh</code>.</span></span>
          </label>
          <label class="choice">
            <input type="radio" name="signup-method" value="browser" bind:group={method}>
            <span><strong>This browser</strong>
              <span class="muted">Make a key here, and download a copy to sign in with later.</span></span>
          </label>
        </fieldset>
      </li>
      {#if method === "agent"}
        <li>
          <p class="step-title">Run this in the directory you will file plans from</p>
          <p class="muted small">It carries your token, and <code>pipette-ctl</code> keeps the identity in that directory.</p>
          <Command command={registerCommand} />
          <details>
            <summary>Several keys, or none yet</summary>
            <ul class="muted">
              <li>Several keys: <code>pipette-ctl auth keys available</code> lists them; add
                <code>--ssh-key</code> with the fingerprint or comment of the one to use.</li>
              <li>No key yet: <code>ssh-keygen -t ed25519</code> then <code>ssh-add</code>, or add
                <code>--generate</code> to make one for <code>pipette-ctl</code> alone.</li>
            </ul>
          </details>
        </li>
        <li>
          <p class="step-title">Sign in with the same key</p>
          <button type="button" class="primary" onclick={() => choose("signin", "agent")}>I registered, sign in</button>
        </li>
      {:else if method === "browser"}
        <li>
          <form onsubmit={signUpInBrowser}>
            <label><span class="step-title">Name this machine</span>
              <input type="text" bind:value={details} placeholder="work laptop, Chrome" autocomplete="off"
                     spellcheck="false">
            </label>
            <p class="muted small">So you can tell the key apart and revoke it. You save a copy of the key next.</p>
            <button type="submit" class="primary" aria-disabled={work.busy}>Create account</button>
          </form>
        </li>
      {/if}
    </ol>
  {/if}

  <p class="switch">Already have an account? <button type="button" class="again" onclick={() => switchTo("signin")}>Sign in</button></p>
{/if}
</div>
</div>

<style>
  /* One box, so the page reads as a sign-in rather than as a page of the console. */
  .card { margin: 24px 0; padding: 20px 24px 8px; border: 1px solid var(--border); border-radius: 8px; }
  h1 { margin-top: 0; }
  .lead { margin-top: -4px; }
  .small { font-size: 12px; }
  .wide-button { width: 100%; padding: 8px 12px; font-size: 13px; }
  .or { display: flex; gap: 10px; align-items: center; margin: 20px 0 6px; color: var(--text-muted); font-size: 12px; }
  .or::before, .or::after { content: ""; flex: 1; border-top: 1px solid var(--border); }
  /* A pick of one, drawn as cards so none reads as a button to press. */
  .choices { display: grid; gap: 8px; margin: 12px 0; padding: 0; border: 0; }
  .choices legend { padding: 0; margin-bottom: 6px; }
  .choice { display: flex; gap: 10px; align-items: flex-start; margin: 0; padding: 10px 12px; cursor: pointer;
            border: 1px solid var(--border); border-radius: var(--radius); }
  /* Not a field's caption, which `label > span` draws small and muted. */
  .choice > span { margin: 0; color: var(--text); font-size: 13px; }
  .choice:hover { border-color: var(--border-strong); }
  .choice:has(input:checked) { border-color: var(--accent); background: var(--selected); }
  .choice input { margin: 3px 0 0; accent-color: var(--accent); }
  .choice strong { font-weight: 500; }
  .choice .muted { display: block; font-size: 12px; }
  .panel { margin: 4px 0 12px; }
  /* Numbered steps, each opening with its title rather than a field's caption. */
  .steps { margin: 16px 0 12px; padding: 0; list-style: none; counter-reset: step; }
  .steps > li { position: relative; padding: 0 0 14px 32px; counter-increment: step; }
  .steps > li::before { content: counter(step); position: absolute; left: 0; top: 0; width: 20px; height: 20px;
                        border-radius: 50%; background: var(--surface-2); color: var(--text-muted);
                        font-size: 11px; font-weight: 500; line-height: 20px; text-align: center; }
  .steps label, .steps .choices { margin: 0; }
  .steps .choices legend { margin-bottom: 8px; }
  .step-title, label > span.step-title { display: block; margin: 0 0 6px; color: var(--text); font-size: 13px;
                                         font-weight: 500; }
  .steps .small { margin: -2px 0 8px; }
  .step { margin: 12px 0 6px; }
  .switch { margin: 18px 0 12px; padding-top: 14px; border-top: 1px solid var(--border); color: var(--text-muted); }
</style>
