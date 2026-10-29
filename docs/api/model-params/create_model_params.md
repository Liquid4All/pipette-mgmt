# `POST /api/v1/model-params`

Takes `{model_key, total_millions, active_millions?, note?}`: `model_key` is
any name that table lookup reads (`LiquidAI/LFM2-700M-GGUF`, a vault id), and
the row is keyed by its normalized form. It is read under its former name
`model` too; a body naming both is a `400`. Who may write which row is
[who writes which row](../model-params.md#who-writes-which-row).

## Statuses

| Status | When |
|--------|------|
| 201 | The row |
| 400 | A non-positive count, an active count above the total, a note over 500 characters, a key outside `[a-z0-9._-]`, or both `model_key` and `model` |
| 403 | The row is outside the caller's reach, whether the key is a name, another owner's file or no file |
| 409 | The key has a row |
