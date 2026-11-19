# `PUT /api/v1/campaign-groups/{group_id}/policies/{kind}`

For `spread`, raising the threshold of the defaults the caller read:

```json
{
  "enabled": false,
  "max_reruns": 3,
  "settings": { "percent": 7.5, "short_repetition_ms": 1000, "short_repetition_percent": 10 },
  "base": {
    "enabled": false,
    "max_reruns": 3,
    "settings": { "percent": 5, "short_repetition_ms": 1000, "short_repetition_percent": 10 }
  }
}
```

Writes the group's policy for the check `kind`: `swap`, `spread`, `failed`,
`ttft_over_e2e`, `model_params` or `smaller_failed`
([policies](../campaign-groups.md#policies), which judge no eval's cell). The body replaces the policy
whole, so every field is required. `max_reruns` is a whole number from 0 to 15
for `swap`; 0 for `model_params` and `smaller_failed`, which only flag, and for
`failed`, whose `codes` holds its re-runs; and from 1 to 15 for `spread`, which
marks a cell failed once its re-runs are spent, and `ttft_over_e2e`, which
keeps the cell's result. In
`settings`, `spread` takes `percent`, a number above 0, and optionally
`short_repetition_ms` with `short_repetition_percent`, both numbers above 0,
for rows whose repetitions are shorter
([policies](../campaign-groups.md#policies)); `failed` takes `codes`, a map
from one or more failure codes it re-runs to each one's re-runs, from 1 to 15,
such as `{ "runtime_error": 2, "device_not_ready": 1 }`; the other checks take
`{}`. What becomes of a cell still flagged is the check's own, so the body
names no outcome.

`base` is the policy's `enabled`, `max_reruns` and `settings` as the caller read
them ([`GET …/policies`](list_campaign_group_policies.md)), the check's defaults
for a policy never written. `enabled` and `base.enabled` make three kinds of
write:

| Write | When | What it checks |
|---|---|---|
| Turn off | `enabled: false` over a `base` that is on | Nothing more: `max_reruns`, `settings` and the rest of `base` are not read |
| Leave on | `enabled: true`: Turn on, or Save changes on a policy that is on | A re-run's grants unless `kind` is `model_params` or `smaller_failed`, then `max_reruns`, `settings` and `base` |
| Save changes on a policy that is off | `enabled: false` over a `base` that is off | `max_reruns`, `settings` and `base` |

A Turn off turns the stored policy off and keeps its stored `max_reruns` and
`settings`, whatever the body sends. So it lands on any policy that is on, one
stored with values the route no longer takes included, and never undoes values
another writer saved. A policy found off already is left as it is, with
`changed: false`. A Turn off that meets another write reads the policy again, 3
tries in all, and answers `409` only when it loses all 3.

The other writes answer `409` when `base` is not the stored policy, or when the
policy changes between the route's read and its write. So Save changes on a
policy read as off answers `409` once another writer has turned it on, and so
does a write whose `base` holds defaults read before a release changed them.
Nothing is written, and the caller reads the policy again.

Every write takes the rights of adding members
([who may do what](../campaign-groups.md#who-may-do-what)). A policy that is on
re-runs the owner's campaigns, so a write that leaves one on also takes the
grants a re-run of them takes: `read_plans` and `cancel_plans` for the caller's
own campaigns or its organization's, `read_any_plans` and `cancel_any_plans`
for any other owner's, and `read_clients`. A missing grant is a `403` naming
it. A Turn off, Save changes on a policy that is off, and any write of
`model_params` or `smaller_failed`, which re-run and mark nothing, take none of
them.

A write that turns the policy on records the caller as `enabled_by` and the
time as `enabled_at`. A write that keeps it on keeps both, and one that turns it
off sets both to `null`. One asking for the policy the group already has,
stored or the defaults, writes nothing and answers `changed: false`. Answers
`200` with the policy as stored plus `changed`.

## Statuses

| Status | When |
|--------|------|
| 200 | The policy as stored, plus `changed` |
| 400 | `group_id` is not a group id or `kind` is not a check; the body is not JSON, leaves out a field (`base` included), names another field, or carries a malformed field; or, on a write other than a Turn off, `max_reruns` is outside the check's range or `settings` do not suit the check |
| 403 | The caller holds no `submit_plans`, or may not file into the group owner's campaigns ([who may do what](../campaign-groups.md#who-may-do-what)); or the write leaves a policy other than `model_params` and `smaller_failed` on and the caller lacks a re-run's grant, which the answer names |
| 404 | No such group, or the server has no `[database]` |
| 409 | `base` is not the stored policy, or the policy changed before the write landed; or a Turn off lost to other writes on all 3 tries |
