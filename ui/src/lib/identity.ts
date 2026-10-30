// The identity this browser signs in as, kept in IndexedDB: the one store that
// holds a CryptoKey as-is, so the private key stays non-extractable.

import type { SigningKey } from "./keys";

/**
 * Where the key came from, which decides what signing out does with it:
 * `generated` may be the only copy; `browser` (signed in by a key the account
 * already holds, `lib/signin.ts`) and `clerk` exist for this browser alone and
 * expire, and signing out revokes them.
 */
export type KeyOrigin = "generated" | "browser" | "clerk";

/** Whether `origin` names a key made for this browser alone. */
export const browserOnly = (origin: KeyOrigin) => origin === "browser" || origin === "clerk";

export interface Identity extends SigningKey {
  user_id: string;
  origin: KeyOrigin;
}

const DB = "pipette-console";
const STORE = "identity";
const SLOT = "current";

function open(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(STORE);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

async function run<T>(mode: IDBTransactionMode, op: (s: IDBObjectStore) => IDBRequest): Promise<T> {
  const db = await open();
  try {
    return await new Promise<T>((resolve, reject) => {
      const req = op(db.transaction(STORE, mode).objectStore(STORE));
      req.onsuccess = () => resolve(req.result as T);
      req.onerror = () => reject(req.error);
    });
  } finally {
    db.close();
  }
}

export async function load(): Promise<Identity | null> {
  return (await run<Identity | undefined>("readonly", (s) => s.get(SLOT))) ?? null;
}

export const save = (identity: Identity): Promise<void> =>
  run("readwrite", (s) => s.put(identity, SLOT));

export const clear = (): Promise<void> => run("readwrite", (s) => s.delete(SLOT));
