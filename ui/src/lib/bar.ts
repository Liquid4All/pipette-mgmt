// What `TabBar` draws: a bar of links, the open one marked.

/** A link of a bar. */
export interface BarTab {
  key: string;
  label: string;
  /** What the label is short for, shown on hover. */
  title?: string;
  href: string;
  current: boolean;
  /** It opens a group, set apart from the one before. */
  start: boolean;
}
