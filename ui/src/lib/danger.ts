// A danger zone's confirmation: the phrase a reader types to unlock a write.

/** Each plan-wide danger-zone write's phrase; a transfer's is the campaign's name. */
export const PHRASES = { evals: "recalculate all", all: "re-run all", cancel: "cancel plan" } as const;

/** Each record's delete. */
export const DELETES = {
  resource: "delete resource", device: "delete device",
  benchmark: "delete benchmark",
} as const;

/** An account's disable, and a pending device's reject, which deletes its registration. */
export const DISABLE_ACCOUNT = "disable account";
/** Making a service account a campaign reader, which nothing undoes. */
export const MAKE_READER = "make campaign reader";
export const REJECT_DEVICE = "reject device";

/** An organization's rename, its disable, and the enable that undoes it. */
export const ORG = {
  rename: "rename organization", disabled: "disable organization", active: "enable organization",
} as const;

/** Whether `typed` is `phrase`, spaces around it aside. */
export const unlocks = (typed: string, phrase: string): boolean => typed.trim() === phrase.trim();
