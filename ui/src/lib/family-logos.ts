import { descriptorFamilyLogo } from "./api";
import type { Identity } from "./identity";
import { session } from "./session.svelte";

const pending = new WeakMap<Identity, Map<string, Promise<string | null>>>();

/**
 * Read the logo URI under the current signer's access. Draws for the same
 * signer and digest share a request while it is pending.
 */
export function familyLogo(sha256: string): Promise<string | null> {
  const signer = session.signer;
  let requests = pending.get(signer);
  if (!requests) {
    requests = new Map();
    pending.set(signer, requests);
  }
  const inFlight = requests.get(sha256);
  if (inFlight) return inFlight;
  const reading = descriptorFamilyLogo(signer, sha256)
    .then((found) => session.identity === signer ? found.logo_uri : null, () => null)
    .finally(() => { requests.delete(sha256); });
  requests.set(sha256, reading);
  return reading;
}
