// The plan document editor's logic, kept free of runes for vitest.

import { describe } from "./components/guard";
import type { PlanIssue, PlanValidation } from "./api-types";

/** The editor's height in rows: the document and a line to type on, within 14 to 40. */
export const editorRows = (text: string): number => Math.min(40, Math.max(14, text.split("\n").length + 1));

/** The offsets of 1-based `line` in `text`, for a selection; the last line when past the end. */
export function lineRange(text: string, line: number): [number, number] {
  const lines = text.split("\n");
  const index = Math.min(Math.max(line, 1), lines.length) - 1;
  const start = lines.slice(0, index).reduce((sum, each) => sum + each.length + 1, 0);
  return [start, start + lines[index]!.length];
}

/**
 * A chosen file's text, for this editor and the benchmark form: refused where the
 * server would refuse it; a leading BOM is kept, as `String::from_utf8` keeps it.
 */
export async function readTextFile(file: Blob): Promise<string> {
  if (file.size === 0) throw new Error("The chosen file is empty");
  try {
    return new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(await file.arrayBuffer());
  } catch {
    throw new Error("The chosen file is not UTF-8 text");
  }
}

/** Whether two documents read the same; a textarea turns a CRLF document into LF on first input. */
export const sameDocument = (a: string, b: string): boolean => a.replace(/\r\n?/g, "\n") === b.replace(/\r\n?/g, "\n");

/** Whether replacing `text` would lose something typed since `baseline` was put in. */
export const isDirty = (text: string, baseline: string): boolean => text.trim() !== "" && !sameDocument(text, baseline);

/** The first issue that refuses the document, if `verdict` has one. */
export const firstError = (verdict: PlanValidation | null): PlanIssue | null =>
  verdict?.issues.find((issue) => issue.severity === "error") ?? null;

const counted = (n: number, noun: string) => `${n.toLocaleString("en")} ${noun}${n === 1 ? "" : "s"}`;

export interface NoteInput {
  text: string;
  /** The document an edit started from; unset where the document is new. */
  original?: string;
  /** The last check's answer, which may be for an earlier text while a newer one is pending. */
  verdict: PlanValidation | null;
  /** Why the last check could not be made. */
  error: string;
  /** A blank document was refused, so its line reads as an error. */
  required?: boolean;
}

/** The editor's status line. */
export function validationNote(input: NoteInput): { tone: "ok" | "err" | "muted"; text: string } {
  const { text, original, verdict, error } = input;
  if (!text.trim()) {
    return original === undefined
      ? { tone: input.required ? "err" : "muted", text: "Paste a plan document, open a file, or start from an example." }
      : { tone: "err", text: "The document is empty." };
  }
  if (original !== undefined && sameDocument(text, original)) return { tone: "muted", text: "Unchanged." };
  if (error) return { tone: "muted", text: `Couldn't check the document: ${error}` };
  if (!verdict) return { tone: "muted", text: "Checking…" };
  const jobs = verdict.job_count === null ? null : counted(verdict.job_count, "job");
  if (firstError(verdict)) return { tone: "err", text: jobs ? `Not valid · expands to ${jobs}` : "Not valid" };
  const warnings = verdict.issues.length;
  return { tone: "ok", text: ["Valid", jobs, warnings ? counted(warnings, "warning") : null].filter(Boolean).join(" · ") };
}

/** What the scheduler last learned, and whether a check is under way. */
export interface Checked {
  /** The text `verdict` or `error` answers; `null` before any answer. */
  on: string | null;
  verdict: PlanValidation | null;
  error: string;
  /** A check is waiting out the debounce or the server. */
  pending: boolean;
}

export interface SchedulerOptions {
  ask: (text: string, signal: AbortSignal) => Promise<PlanValidation>;
  onChange: (state: Checked) => void;
}

const DELAY = 500;

/**
 * Checks of a document as it is typed: one request at a time, sent once typing
 * pauses for `DELAY` ms or at once by `now`, and none for the text last answered
 * unless `now` finds it refused.
 */
export function validationScheduler({ ask, onChange }: SchedulerOptions) {
  let state: Checked = { on: null, verdict: null, error: "", pending: false };
  let timer: ReturnType<typeof setTimeout> | null = null;
  let asking: { text: string; controller: AbortController; answer: Promise<PlanValidation | null> } | null = null;

  const change = (next: Partial<Checked>) => {
    state = { ...state, ...next };
    onChange(state);
  };
  const answered = (text: string) => state.on === text && !!state.verdict;
  /** Forget the debounce and the request in flight, whose answer is dropped. */
  const drop = () => {
    if (timer !== null) clearTimeout(timer);
    timer = null;
    asking?.controller.abort();
    asking = null;
  };

  function send(text: string): Promise<PlanValidation | null> {
    drop();
    const controller = new AbortController();
    change({ pending: true });
    // Every drop aborts the request in flight, so an aborted one's answer is stale.
    const answer = ask(text, controller.signal).then(
      (verdict) => {
        if (!controller.signal.aborted) {
          asking = null;
          change({ on: text, verdict, error: "", pending: false });
        }
        return verdict;
      },
      (e: unknown) => {
        if (!controller.signal.aborted) {
          asking = null;
          change({ on: text, verdict: null, error: describe(e), pending: false });
        }
        return null;
      },
    );
    asking = { text, controller, answer };
    return answer;
  }

  /** Stop checking and forget the answer: the document is blank, or the one an edit started from. */
  function cancel() {
    drop();
    if (state.on !== null || state.verdict || state.error || state.pending) {
      change({ on: null, verdict: null, error: "", pending: false });
    }
  }

  return {
    /** Check `text` once typing pauses; the answer on screen stays until the new one replaces it. */
    schedule(text: string) {
      if (!text.trim()) return cancel();
      if (asking?.text === text) return;
      drop();
      if (answered(text)) {
        if (state.pending) change({ pending: false });
        return;
      }
      change({ pending: true });
      timer = setTimeout(() => {
        timer = null;
        void send(text);
      }, DELAY);
    },
    /** Check `text` at once; `null` for a blank text or a check that could not be made. */
    now(text: string): Promise<PlanValidation | null> {
      if (!text.trim()) {
        cancel();
        return Promise.resolve(null);
      }
      if (asking?.text === text) return asking.answer;
      // A refusal is asked again: what it lacked, a benchmark or a vault resource, may since exist.
      if (answered(text) && !firstError(state.verdict)) {
        drop();
        if (state.pending) change({ pending: false });
        return Promise.resolve(state.verdict);
      }
      return send(text);
    },
    cancel,
    /** Stop checking, keeping what is on screen: the editor is gone. */
    stop: drop,
  };
}
