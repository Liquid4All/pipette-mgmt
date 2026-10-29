// The arithmetic of the results report's hand-drawn SVG charts, after
// pipette-duckdb's dashboard-v1_1 export report (`export_report.html`).

/** Hues in the palette (`--series-1` … `--series-8`), then shapes before both repeat. */
export const HUES = 8;
export const SHAPES = 6;

/** The marker shapes after the circle, as `[sides, rotation in degrees]`. */
const POLYGONS: ([number, number] | null)[] = [null, [4, -45], [3, -90], [4, -90], [3, 90], [6, -90]];

export const finite = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

/** `v` mapped linearly from `[lo, hi]` onto `[p0, p1]`; an empty domain maps everything to `p0`. */
export const scale = (lo: number, hi: number, p0: number, p1: number) => (v: number): number =>
  hi <= lo ? p0 : p0 + ((v - lo) / (hi - lo)) * (p1 - p0);

/**
 * The x axis over rungs `xs`, onto `[p0, p1]`: linear from 0, so a doubling ladder's steps read as their sizes;
 * a lone rung, with no scale to show, mid-axis.
 */
export function xScale(xs: readonly number[], p0: number, p1: number): (v: number) => number {
  if (new Set(xs).size < 2) return () => (p0 + p1) / 2;
  return scale(0, Math.max(0, ...xs) * 1.03, p0, p1);
}

/** Which x labels to draw at `at` (ascending px): each `gap` past the last drawn, the first always, so none overlap. */
export function labelled(at: readonly number[], gap = 14): boolean[] {
  let last = -Infinity;
  return at.map((x) => {
    if (x - last < gap) return false;
    last = x;
    return true;
  });
}

export interface Ticks {
  step: number;
  ticks: number[];
}

/** About `n` ticks over `[lo, hi]` at 1, 2, 2.5 or 5 × 10^k, so every axis reads on one kind of scale. */
export function niceTicks(lo: number, hi: number, n = 4): Ticks {
  const span = hi - lo;
  if (!(span > 0)) return { step: 1, ticks: [lo] };
  const magnitude = 10 ** Math.floor(Math.log10(span / n));
  const norm = span / n / magnitude;
  const step = (norm <= 1 ? 1 : norm <= 2 ? 2 : norm <= 2.5 ? 2.5 : norm <= 5 ? 5 : 10) * magnitude;
  // By index and capped, not summed: a step below `lo`'s precision would never advance, or repeat a tick.
  const first = Math.ceil(lo / step - 1e-9);
  const ticks: number[] = [];
  for (let i = 0; i <= 4 * n + 2; i++) {
    const v = Number(((first + i) * step).toPrecision(12));
    if (v > hi + step * 1e-9) break;
    if (v !== ticks[ticks.length - 1]) ticks.push(v);
  }
  return { step, ticks: ticks.length ? ticks : [lo] };
}

export interface Axis extends Ticks {
  lo: number;
  hi: number;
}

export interface AxisOptions {
  /** Keep 0 in range. */
  zero?: boolean;
  /** Keep ±1.5×`band` in range, so a shaded tolerance reads as a strip around 0, not the whole plot. */
  band?: number | null;
  /** Values the range must hold too, such as a reference line's. */
  include?: number[];
}

/** A value axis over `values`' range, run out to the tick on either side of it, so every value reads between two. */
export function axis(values: readonly (number | null)[], options: AxisOptions = {}): Axis {
  const data = [...values.filter(finite), ...(options.include ?? [])];
  let lo = data.length ? Math.min(...data) : 0;
  let hi = data.length ? Math.max(...data) : 1;
  if (options.band) {
    lo = Math.min(lo, -1.5 * options.band);
    hi = Math.max(hi, 1.5 * options.band);
  }
  if (options.zero) {
    lo = Math.min(0, lo);
    hi = Math.max(0, hi);
  }
  // A range tickText cannot tell apart, as speedups a rounding off 1×, is widened like an empty one.
  if (hi - lo <= Math.max(Math.abs(lo), Math.abs(hi)) * 1e-6) hi = lo + Math.max(1, Math.abs(lo) * 0.1);
  const { step } = niceTicks(lo, hi, 5);
  const first = Math.floor(lo / step + 1e-9);
  const last = Math.ceil(hi / step - 1e-9);
  const ticks = Array.from({ length: last - first + 1 }, (_, i) => Number(((first + i) * step).toPrecision(12)));
  return { lo: ticks[0]!, hi: ticks[ticks.length - 1]!, step, ticks };
}

/** A bar axis from 0 to the largest value, or `atLeast` when that is larger (100 for a percentage). */
export function barAxis(values: readonly number[], atLeast = 0): Axis {
  const hi = Math.max(atLeast, ...values.filter(finite)) || 1;
  return { lo: 0, hi, ...niceTicks(0, hi, 4) };
}

/** Decimals `v` needs to print exactly, at most 6: 2.5 one, 20 none. */
function decimals(v: number): number {
  const text = Math.abs(v).toPrecision(12).replace(/0+$/, "").replace(/\.$/, "");
  const dot = text.indexOf(".");
  return dot < 0 ? 0 : Math.min(6, text.length - dot - 1);
}

/** A tick label at the precision the axis step needs: written out with its thousands apart, scaled only from a million. */
export function tickText(v: number, step: number): string {
  if (!finite(v)) return "—";
  if (v === 0) return "0";
  const size = Math.abs(step);
  const [k, suffix] = size >= 5e8 ? [1e9, "G"] : size >= 5e5 ? [1e6, "M"] : [1, ""];
  const places = decimals(step / k);
  return (v / k).toLocaleString("en-US", { minimumFractionDigits: places, maximumFractionDigits: places }) + suffix;
}

/** A panel's title with its unit, unless the title already names it (`… as % of …`). */
export const withUnit = (title: string, unit: string): string =>
  (title.split(" ").includes(unit) ? title : `${title} (${unit})`);

/** A value as the charts' tooltips and tables print it: whole from 10 000, else 1–3 decimals by size. */
export function valueText(v: number | null): string {
  if (!finite(v)) return "—";
  const size = Math.abs(v);
  if (size >= 1e4) return String(Math.round(v));
  if (size >= 100) return v.toFixed(1);
  if (size >= 1) return v.toFixed(2);
  return v.toFixed(3);
}

/** A count as printed: whole, or with the decimals a median of counts needs (`64.5`). */
export function exactText(v: number | null): string {
  return finite(v) ? v.toFixed(decimals(v)) : "—";
}

/** The median of the finite `values`, or `null` when there are none. */
export function median(values: readonly (number | null)[]): number | null {
  const sorted = values.filter(finite).sort((a, b) => a - b);
  if (!sorted.length) return null;
  const mid = sorted.length >> 1;
  return sorted.length % 2 ? sorted[mid]! : (sorted[mid - 1]! + sorted[mid]!) / 2;
}

/** A slot's hue (1–8, as `--series-{hue}` names it) and marker shape (0–5): eight hues, then the next shape. */
export const channel = (slot: number): { hue: number; shape: number } =>
  ({ hue: (slot % HUES) + 1, shape: Math.floor(slot / HUES) % SHAPES });

const at = (v: number) => v.toFixed(1);

/** A marker's path centred on `(x, y)`: a circle, then regular polygons a little larger so they weigh the same. */
export function markPath(shape: number, x: number, y: number, r: number): string {
  const polygon = POLYGONS[shape % SHAPES];
  if (!polygon) return `M${at(x - r)} ${at(y)}a${r} ${r} 0 1 0 ${2 * r} 0a${r} ${r} 0 1 0 ${-2 * r} 0Z`;
  const [sides, rotation] = polygon;
  const rr = r * 1.15;
  const points = Array.from({ length: sides }, (_, i) => {
    const angle = ((rotation + (i * 360) / sides) * Math.PI) / 180;
    return `${at(x + rr * Math.cos(angle))} ${at(y + rr * Math.sin(angle))}`;
  });
  return `M${points.join("L")}Z`;
}

/** A horizontal bar `w` long from `x0`: square at the baseline, rounded `r` at the value's end. */
export function barPath(x0: number, y: number, w: number, h: number, r: number): string {
  if (w <= 0) return `M${x0} ${y}`;
  const rr = Math.min(r, w, h / 2);
  const end = x0 + w;
  return `M${x0} ${y}H${at(end - rr)}Q${at(end)} ${y} ${at(end)} ${at(y + rr)}`
    + `V${at(y + h - rr)}Q${at(end)} ${at(y + h)} ${at(end - rr)} ${at(y + h)}H${x0}Z`;
}

/** Where a `w`×`h` tooltip goes for a pointer at `(x, y)`: below right, flipped to the other side where the viewport ends. */
export function tipPlace(x: number, y: number, w: number, h: number, vw: number, vh: number): { left: number; top: number } {
  const gap = 14;
  const left = x + gap + w <= vw - 8 ? x + gap : x - gap - w;
  const top = y + gap + h <= vh - 8 ? y + gap : y - gap - h;
  return { left: Math.max(8, left), top: Math.max(8, top) };
}
