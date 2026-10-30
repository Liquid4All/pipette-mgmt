<script lang="ts">
  // The key the classic console kept in this browser (its sign-in page's "This
  // browser's key"): dropped unasked where losing it costs nothing, else saved
  // as a file where it may be the only copy, then signed in with.
  import { whoami } from "../api";
  import { save, type Identity } from "../identity";
  import { describeBrowser, downloadSeed } from "../keys";
  import { adoptLegacy, dropLegacy, mustSaveFirst, pageStorage, readLegacy, standing, type Standing } from "../legacy";
  import { land } from "../route.svelte";
  import { signInWithOwnKey } from "../signin";
  import ConfirmDialog from "./ConfirmDialog.svelte";
  import { Busy } from "./work.svelte";

  /** Without `onsignedin`, the browser is signed in already, and the key is only saved or forgotten. */
  let { onsignedin }: { onsignedin?: (identity: Identity) => void } = $props();

  const storage = pageStorage();
  const held = readLegacy(storage);
  let saved = $state(false);
  /** `null` hides the section, as it does until the key's standing is known. */
  let shown = $state<Standing | null>(null);
  const work = new Busy();
  const uid = $props.id();

  if (held) {
    void standing(held, whoami).then((found) => {
      if (found === "dead" || (found === "spare" && !onsignedin)) dropLegacy(storage);
      else shown = found;
    });
  }

  // The page cannot see a download finish, so saving keeps the entry and the button.
  function download() {
    if (!held) return;
    downloadSeed(held.private_key);
    saved = true;
  }

  let confirm = $state<ReturnType<typeof ConfirmDialog>>();
  function drop() {
    dropLegacy(storage);
    shown = null;
    // The button that held the focus is gone with the section.
    land();
  }
  function forget() {
    if (!shown || !mustSaveFirst(shown, saved)) return drop();
    void confirm?.ask({
      title: "Forget this key?", text: "This browser made this key, and this may be its only copy.",
      action: "Forget it anyway", pending: "Forgetting…", run: async () => null, done: drop,
    });
  }

  const signIn = () => work.attempt(async () => {
    if (!held || !onsignedin) return;
    const identity = await adoptLegacy(held, storage, async (privateKey) => {
      const signedIn = await signInWithOwnKey(privateKey, describeBrowser());
      await save(signedIn);
      return signedIn;
    });
    shown = null;
    onsignedin(identity);
  });
</script>

{#if held && shown}
  <!-- Drawn above the page's h1, so its title names a region rather than taking a place in the outline. -->
  <section class="narrow" aria-labelledby={uid}>
    <p class="h2" id={uid}>This browser's key</p>
    <p class="err" role="alert" hidden={!work.error}>{work.error}</p>
    {#if !onsignedin}
      <p class="muted">
        The classic console left the key it made here for <span class="id">{held.user_id}</span> in
        this browser's storage, where any script on this page can read it. This app signs with a key
        of its own and does not need it, but it may be the only copy, so save it as
        <code>id_ed25519</code> before you forget it.
      </p>
    {:else if shown === "sole"}
      <p class="muted">
        The classic console kept a key it made here for <span class="id">{held.user_id}</span>. It
        may be the only copy, and this browser forgets it once you sign in with it, so save it as
        <code>id_ed25519</code> first; then sign in with it, or later with its file under
        <strong>Private key file</strong>.
      </p>
    {:else}
      <p class="muted">
        The classic console kept the key <span class="id">{held.user_id}</span> last signed in with
        here. This browser forgets it once you sign in with it.
      </p>
    {/if}
    <div class="bar">
      {#if shown === "sole"}
        <button type="button" class:primary={!saved} onclick={download}>Save as id_ed25519</button>
      {/if}
      {#if onsignedin}
        <button type="button" class:primary={!mustSaveFirst(shown, saved)} onclick={signIn}
                disabled={mustSaveFirst(shown, saved)} aria-disabled={work.busy}>Sign in with it</button>
      {/if}
      <button type="button" onclick={forget} aria-disabled={work.busy}>Forget this key</button>
    </div>
  </section>
{/if}
<!-- Outside the section, which a forgotten key takes with it. -->
<ConfirmDialog bind:this={confirm} />
