// The model-params rows: the model parameter table, one editable row of parameter
// counts per model.

import { userLink } from "./routes";
import type { TableSpec } from "./table";
import type { ModelParams } from "./api-types";

export const MODEL_PARAMS_TABLE: TableSpec<ModelParams> = {
  columns: [
    { key: "model_key", label: "model_key", raw: (row) => row.model_key, as: "id" },
    { key: "total_millions", label: "total_millions", raw: (row) => row.total_millions, num: true },
    { key: "active_millions", label: "active_millions", raw: (row) => row.active_millions, num: true },
    { key: "note", label: "note", raw: (row) => row.note },
    { key: "updated_at", label: "updated_at", raw: (row) => row.updated_at, as: "time" },
    { key: "updated_by", label: "updated_by", raw: (row) => row.updated_by,
      href: (row) => (row.updated_by ? userLink(row.updated_by) : null) },
  ],
  texts: [{ key: "q", label: "model_key", texts: (row) => [row.model_key] }],
};

/** A form's note as a request's: left out when blank, which the server reads as none. */
export const noted = (note: string): { note?: string } => (note.trim() ? { note: note.trim() } : {});

/** A form's counts, as number inputs bind them, as a request's: an empty active is the total's, a dense model's. */
export function counts(total: number | null, active: number | null | undefined):
  { total_millions: number; active_millions?: number } {
  const total_millions = total ?? 0;
  return active == null ? { total_millions } : { total_millions, active_millions: active };
}

/** Asked before deleting a row. */
export const DELETE_ROW = (model_key: string) => ({
  title: `Delete ${model_key}?`,
  text: "Rows scored from now on store no count for this model, and the ones the warehouse holds none for read none again.",
  action: "Delete this row", pending: "Deleting…",
});
