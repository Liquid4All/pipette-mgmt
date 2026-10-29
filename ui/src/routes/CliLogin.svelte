<script lang="ts">
  // Approving the key `pipette-ctl auth login` asks for. The code derives from the key, so it does not
  // vouch for the link: it shows the key here is the one the reader's own terminal printed.
  import { approve, cliRequest, codeComplete, codeMatches, EXPIRY_DAYS, keyNames, type Approval,
           type Expiry, type KeyNames } from "../lib/clilogin";
  import PageHead from "../lib/components/PageHead.svelte";
  import { describe } from "../lib/components/guard";
  import { Busy } from "../lib/components/work.svelte";
  import { ME } from "../lib/crumbs";
  import { when } from "../lib/format";
  import { here } from "../lib/route.svelte";
  import type { RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route: _ }: { route: RouteOf<"login-cli"> } = $props();

  const request = $derived(cliRequest(here.query));
  const account = $derived(session.account!);
  const work = new Busy();
  let names = $state<KeyNames | null>(null);
  let typed = $state("");
  let expiry = $state<Expiry>("never");
  let approval = $state<Approval | null>(null);

  $effect(() => {
    const key = request?.public_key;
    names = null;
    if (!key) return;
    keyNames(key).then((read) => { if (request?.public_key === key) names = read; },
                       (e) => (work.error = describe(e)));
  });

  const matches = $derived(!!names && codeMatches(typed, names.code));
  // Wrong only once a whole code is typed, so the field is not red while typing.
  const wrong = $derived(!!names && !matches && codeComplete(typed));
  const held = $derived(names ? account.keys.find((key) => key.key_id === names!.key_id) : undefined);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (!request || !matches || work.busy) return;
    approval = null;
    await work.attempt(async () => {
      // No `session.refresh()` after: a failed one would draw its error frame over "Approved".
      approval = await approve(session.signer, request, expiry);
    });
  }
</script>

<div class="narrow">
<PageHead trail={ME} title="Approve pipette-ctl" />
{#if !request}
  <p class="err" role="alert">
    This link does not name a key to approve: it needs a <code>key</code> of 64 hex characters and
    the machine's <code>details</code>. Open the whole link <code>pipette-ctl auth login</code> printed,
    or run it again.
  </p>
{:else if approval === "approved"}
  <p class="ok" role="status">Approved. Return to your terminal.</p>
{:else}
  <p class="notice">
    Only continue if you just ran <code>pipette-ctl auth login</code> yourself. Approving lets whoever
    holds this key act as you, and anyone can send you a link like this one.
  </p>
  <dl>
    <dt>approving as</dt><dd>{account.contact_email} <span class="muted id">{account.user_id}</span></dd>
    <dt>machine</dt><dd>{request.key_details} <span class="muted">(as the requester says)</span></dd>
    <dt>fingerprint</dt><dd class="id">{names?.fingerprint ?? "…"}</dd>
    <dt>key_id</dt><dd class="id">{names?.key_id ?? "…"}</dd>
  </dl>
  {#if held}
    <p class="muted">
      This key is already on your account{!held.expires_at ? ""
        : Date.parse(held.expires_at) <= Date.now() ? `, which expired ${when(held.expires_at)}`
        : `, expiring ${when(held.expires_at)}`}.
      Approving it again sets its expiry to the one picked below.
    </p>
  {/if}

  {#if approval === "taken"}
    <p class="err" role="alert">
      This key is registered to another account or to a device, so it cannot be approved here. In
      your terminal, run <code>pipette-ctl auth login --generate</code> to make a key for
      <code>pipette-ctl</code> alone.
    </p>
  {/if}
  {#if work.error}<p class="err" role="alert">{work.error}</p>{/if}

  <form onsubmit={submit}>
    <label><span>the code <code>pipette-ctl auth login</code> printed in your terminal</span>
      <input type="text" bind:value={typed} autocomplete="off" spellcheck="false" placeholder="XXXX-XXXX"
             aria-invalid={wrong} aria-describedby={wrong ? "code-hint" : undefined}>
    </label>
    {#if wrong}<p class="hint err" id="code-hint">That is not this key's code. Check the terminal.</p>{/if}
    <fieldset>
      <legend>expires</legend>
      <label class="check"><input type="radio" name="expiry" value="never" bind:group={expiry}>
        No expiry, a machine key as <code>pipette-ctl auth register</code> makes</label>
      <label class="check"><input type="radio" name="expiry" value="30d" bind:group={expiry}>
        {EXPIRY_DAYS} days</label>
    </fieldset>
    <button type="submit" class="primary" disabled={!matches} aria-disabled={work.busy}>Approve this key</button>
  </form>
{/if}
</div>
