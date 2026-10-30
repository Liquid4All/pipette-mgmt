// A text filter applies once the typing in its box rests, not on each key.

/** How long a box rests before its filter applies. */
export const TYPING_MS = 300;

export class Typing {
  private waiting = new Map<string, { timer: ReturnType<typeof setTimeout>; apply: () => void }>();

  /** `apply` once `key`'s box has rested; typing in it again puts that off. */
  later(key: string, apply: () => void, ms = TYPING_MS) {
    this.drop(key);
    this.waiting.set(key, { timer: setTimeout(() => this.flush(key), ms), apply });
  }

  /** What `key`'s box waits to apply, at once: Enter, or leaving the box. */
  flush(key: string) {
    const held = this.waiting.get(key);
    if (!held) return;
    this.drop(key);
    held.apply();
  }

  /** Forget everything waiting, as a page that is gone must: its filter would pull the reader back. */
  clear() {
    for (const key of [...this.waiting.keys()]) this.drop(key);
  }

  private drop(key: string) {
    clearTimeout(this.waiting.get(key)?.timer);
    this.waiting.delete(key);
  }
}
