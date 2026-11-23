# Campaign groups

A **campaign group** lists campaigns of one owner together, such as the
per-device campaigns of one study, so they can be read and checked as one set.
A group changes nothing in its campaigns: each keeps its plans, its owner and
its history, whether it joins a group, leaves one or outlives one.

The database keeps groups
([where records live](../storage.md#11-where-records-live)).

[`POST /api/v1/campaign-groups`](campaign-groups/create_campaign_group.md)
creates a group, [`POST …/members`](campaign-groups/add_campaign_group_members.md)
adds campaigns to it, and
[`DELETE …/members/{campaign_id}`](campaign-groups/remove_campaign_group_member.md)
takes one out. [`GET …/{group_id}`](campaign-groups/get_campaign_group.md) reads
a group with its members. A group also keeps its data-quality
[policies](#policies).

## The campaign group record

`CampaignGroup` ([the OpenAPI document](../httpapi.md#15-the-openapi-document))
carries `group_id` (`group_` and a UUIDv7), `name`, `owner`, `created_at` and
`updated_at`. Only a rename moves `updated_at`: the members are not part of the
record.

A campaign is in at most one group, and joins only a group of its own owner.
Handing a campaign to another owner takes it out of its group. Deleting a
campaign takes it out of its group, and deleting a group keeps every campaign.

A rename replaces the record with no version check, so two renames at once are
last-writer-wins. A rename to the current name
writes nothing.

## Policies

A group keeps one policy per check. A policy re-runs a cell the check flags
up to `max_reruns` times; `failed` up to the re-runs its `codes` give the
failure's code. What becomes of a cell still flagged after that is
the check's own: `swap` and `spread` mark it failed, and the others keep their
result. A check that keeps the result takes at least 1 re-run, `failed` for
each code it lists, as with none it would do nothing; `spread` takes at least 1
too, so a cell is re-run before it is marked. `model_params`, `smaller_failed`
and `vault_resource` only flag: they re-run and mark nothing, so they take no
re-run. Every
`queue-maintenance` run applies the policies that are on
([when a policy acts](#when-a-policy-acts)), and reads no `smaller_failed`.
[`GET …/policy-preview`](campaign-groups/preview_campaign_group_policies.md)
shows what they would re-run and mark in each member campaign now, the result
rows that lack model params, and the failed jobs below a clean larger run.

The policies judge performance results alone. A cell of an eval, whose
benchmark the plan typed `eval`, `vlm_eval` or `asr_eval` when it was filed,
is never flagged, re-run or marked by any policy, and the preview, the job
states and the re-run results leave its jobs out. Every other benchmark type is
judged.

| `kind` | Flags a cell when | `max_reruns` | Then the cell | `settings` |
|---|---|---|---|---|
| `swap` | its run swapped | 5, from 0 to 15 | is marked failed | `{}` |
| `spread` | its result's standard deviation is more than `percent` of its value | 3, from 1 to 15 | is marked failed | `{ "percent": 5, "short_repetition_ms": 1000, "short_repetition_percent": 10 }` |
| `failed` | its device reported the run failed with one of `codes` | 0, only 0: `codes` holds each code's re-runs | keeps its result | `{ "codes": { "runtime_error": 2, "device_error": 2, "unspecified": 2, "device_not_ready": 1, "harness_error": 1, "out_of_memory": 1 } }` |
| `ttft_over_e2e` | its time to first token is over its end-to-end latency | 3, from 1 to 15 | keeps its result | `{}` |
| `model_params` | a result row's model has no entry in the [model params table](model-params.md), so the pages show the row without a total or active parameter count | 0, only 0 | keeps its result | `{}` |
| `smaller_failed` | it failed, for any reason, while a cell of its family at larger sizes has rows and no failure | 0, only 0 | keeps its result | `{}` |
| `vault_resource` | its model or runtime is a vault resource: its job source names a `vault_*` source under `spec.model` or `spec.runtime`, a gated Hugging Face model copied into the vault included | 0, only 0 | keeps its result | `{}` |

A cell's family is the campaign's cells that differ from it only in their
sizes, as the queue groups them to hand out smaller runs first: prompt and
decode tokens and batch size for the text benchmarks, image sides, image count,
text and decode tokens for VL, and audio length, text and decode tokens for
ASR (`BenchmarkParams::sizes`). One cell is larger than another when no size is
smaller and at least one is larger, so a failed `end_to_end_latency_1024_256`
is flagged against a clean `end_to_end_latency_4096_256`, but not against a
clean `end_to_end_latency_4096_128` or `prefill_throughput_4096`.

The `max_reruns` and `settings` values are each check's defaults, which a
group has until a policy is written, with `enabled: false`.

`spread` also takes `short_repetition_ms` and `short_repetition_percent`,
both or neither. A row whose repetitions take under `short_repetition_ms` on
average is then weighed against `short_repetition_percent` instead of
`percent`, so a measurement of a few hundred milliseconds can vary more before
it is re-run. A row's repetition time comes from the row: a time in `ms` is its
own value, and a rate is its count over its value, the count being
`parameter_prefill_tokens` (or `observation_vl_throughput_prefill_tokens` and
`observation_asr_throughput_prefill_tokens` for VL and ASR) for
`prefill_throughput`, `parameter_decode_tokens` for `decode_throughput` and
`parameter_batch_size` for `decision_throughput`. Any other row is weighed
against `percent`. With `{ "percent": 5, "short_repetition_ms": 1000,
"short_repetition_percent": 10 }`, a decode of 100 tokens at 125 tokens/sec
(800 ms a repetition) is re-run over 10%, and one at 40 tokens/sec (2,500 ms)
over 5%.

`failed` takes `codes`: a map from one or more of the
[failure codes](../httpapi.md#272-failure-variant) a device reports to that
code's re-runs, from 1 to 15. A run its device reported failed with a listed
code is re-run, and a run that failed with another code keeps its failure. The
runs in a row that failed with any listed code count as attempts, and a run is
re-run while that count is within the re-runs of its own code. With `{ "codes":
{ "device_not_ready": 1, "runtime_error": 2 } }`, a readiness wait that timed
out is re-run; if the re-run crashes, that is 2 attempts, within
`runtime_error`'s 2, so it is re-run again; if that one times out on readiness,
3 attempts are over `device_not_ready`'s 1, and the cell keeps the failure. An
`out_of_memory` failure is not re-run there. The defaults re-run
`out_of_memory` once, as another process may have held the memory, and leave
out `attempt_timeout`, since every device has the same limit.

[`GET …/policies`](campaign-groups/list_campaign_group_policies.md) reads every
check's policy, and
[`PUT …/policies/{kind}`](campaign-groups/set_campaign_group_policy.md) writes
one. `CampaignGroupPolicy` carries `group_id`, `kind`, `enabled`, `max_reruns`,
`settings`, `enabled_by`, the user id of whoever turned the policy on, and
`enabled_at`, when they did; both are `null` while the policy is off. Deleting
a group deletes its policies.

### When a policy acts

Every [`queue-maintenance`](../cli.md#pipette-mgmt-queue-maintenance) run
applies the policies that are on to each member campaign's current plan, job
by job as results land. A job counts once its outcome is recorded and its rows
are in pipette-duckdb, and a job that finished before the policy was turned on
counts too. So a policy acts on the plan a campaign runs when it is turned on,
and on every later one.

- **A re-run does not wait for the rows.** If a job has a recorded outcome but
  no rows in pipette-duckdb, the run reads its submission body. It builds rows
  as the scorer does.
  If those rows give a re-run, the run reads the body again before filing.
  If the body changed or cannot be read, the job waits for the next run.
  Otherwise, the re-run records the value that the body gives. Any other
  outcome waits for the rows in
  pipette-duckdb: a mark, a kept result, a job left as it is, and the outcome
  of a re-run. The earlier runs of a re-run chain are read from
  pipette-duckdb only, so a job whose earlier run has no rows yet waits. The
  run does not read the body of an eval's job or of a held submission.

- **One revision per campaign per run.** It re-runs each flagged job that a
  policy flagging it has re-runs left for, and marks failed each other flagged
  job that swap or spread flags. Jobs still queued or running carry over with
  their ids and leases.
- **One at a time.** A campaign gets no new revision while a job that the
  policies' last revision of it queued still waits for a device.
- **Spaced by the work left.** After a revision, queue maintenance defers the
  campaign's next revision until an interval ends. The interval is one minute
  for each whole job that waits per busy device. The minimum is one minute and
  the maximum is 30 minutes. Jobs that no device can run do not count. The
  next revision includes the re-runs and marks that still qualify. Queue
  maintenance files without waiting if the plan's earliest deadline comes at
  or before one maintenance tick after the interval ends. The campaign's [overview](campaigns/get_campaign_overview.md)
  shows what waits.
- **16 runs at most.** A job whose re-run chain holds 16 runs, itself and 15
  earlier, is not re-run again: swap and spread mark it failed, and the others
  keep its result.
- **Left as they are.** A member of another owner than the group's, and a
  campaign that is archived or runs no plan with a document. A plan still
  `creating` is read once it is filed.
- **A stopped plan is left alone.** A plan someone stops is not revised. If it
  is stopped while its revision is being filed, the revision is cancelled and
  the campaign keeps the stopped plan.
- **A plan that cannot be filed again is reported.** Its document is gone, no
  longer matches the plan, or is refused, such as for a deadline that has
  passed. The first run that meets it fails, and later runs log it and do not
  try it again while the plan, the policies and the release stay the same.
  The first run of a new release tries it once more, as the release may fix
  the cause.
- **A kept job is read once.** It is not read again until a policy changes.
  Turning a policy on or off, or saving changes to one that is on, makes the
  next run read every answered job again under the new values, and so does the
  first run of a release that changes what a policy does with a job it flags,
  once per campaign. A Turn off stops every later revision, and one being filed
  as it lands still goes through.
- **A group with no policy on is cleared.** Once a group's last policy is
  turned off, the next run clears what it kept of each member campaign: the
  policies it read the campaign under, which of them triggered, a refused
  plan, a deferred revision, and its job states, so the campaign listing shows
  no policy for it.
  The campaign's last filing stays, so a policy turned on again later still
  holds the campaign while that filing's jobs wait; until that hold ends, the
  listing shows no policy for the campaign.
- **Missing model params are logged.** While `model_params` is on, each run
  logs the result rows it reads that lack model params, by the key an entry
  for their model would take, with the row and job counts. It files nothing.
  Turning it on makes the next run read every answered job's rows once, so it
  logs every key; later runs read only the jobs answered since.
- **Vault resources are read from the job.** `vault_resource` reads each job's
  source, the body it was queued with, so it flags a job whose device reported
  a failure, though such rows record no model or runtime. Like the other
  checks, it flags a job once its rows are in pipette-duckdb.
- **Each policy is kept as cleared or triggered.** A run keeps, for each job
  it reads, which checks of the policies that are on flag it. A policy is
  triggered while it flags a job of the campaign's plan, or while a re-run it
  filed into the campaign waits for its rows, and cleared otherwise. A kept
  job's `model_params` flag is checked against the model params table on each
  run, so adding the entry clears it without reading the rows again, and
  `smaller_failed` is taken over every kept job of the plan. The campaign
  listing shows them as each row's `policy_checks`
  ([`GET /api/v1/campaigns`](campaigns/list_campaigns.md)), as of the last run
  that read the campaign.

Each revision names its group and policies. Its `change` carries `group_id`
and `policies`, its note reads `Campaign group {group_id} policies {kinds}:
re-run {n}, marked {m} failed.`, and each mark's reason names the policy that
marked it, the first of swap and spread that flags the job, and the group. Each
revision also adds a `group_policy` event to
[the audit log](orgs.md#the-audit-log).

### Re-run results

The run that files a revision records each re-run it queued, once the
campaign moves onto the revision: one row per job it re-runs and per policy
that re-runs it, with the policy's runs in a row on the job (`attempt`), the
cell, and the check's value on the run it replaces with what the check weighs
it against. Once the re-run's rows are in pipette-duckdb, the run that reads
them records the check's value on them and the outcome, at the value the check
was filed at: `flagged` when the check still flags them, else `failed` when a
row of them failed, else `cleared`. A re-run that the campaign no longer runs
and that has neither rows nor a recorded result, as when a later revision
dropped it or a person marked it, is `dropped`. A row is not written again
after that. A re-run flagged again and re-run again gets its own row, with the
next `attempt`.

A revision the campaign does not move onto records nothing, the preview
records nothing, and `model_params` re-runs nothing. The rows stay when a
later revision drops their jobs, and go with their group. Re-runs filed before
the rows were recorded get theirs from `pipette-mgmt db backfill-rerun-results`
([cli.md](../cli.md#pipette-mgmt-db-backfill-rerun-results-group_id)).
[`GET …/rerun-results`](campaign-groups/list_campaign_group_rerun_results.md)
reads them with a summary per policy.

## The audit log

Every write to a group names it in [the audit log](orgs.md#the-audit-log), as
do a hand-off that takes a campaign out of it, a delete of a member campaign
(a refused one too, such as a `409` while a plan runs) and each `group_policy`
event. An event that names a group is in the log of its owner's
organization, so `target={group_id}` there reads the group's history: who
created, renamed or deleted it, who changed its members or its policies, and
what `queue-maintenance` filed for it. A write to the group also names its
owner. The host commands `pipette-mgmt campaigns owner` and `campaigns delete`
name the campaign alone.

## Who may do what

There are no new permission names. A group takes the rights of its owner's
campaigns ([who may do what](campaigns.md#who-may-do-what)):

| Route | Gate | Who |
|---|---|---|
| Create | `submit_plans` | Whoever may create a campaign for the owner |
| List, read, read the policies | `read_plans` or `manage_orgs` | Whoever reads the owner's campaigns. A campaign reader reads no group |
| Preview the policies, read their re-run results | `read_plans` and `read_clients` | Whoever reads the owner's campaigns, over the members whose plans it reads |
| Rename, delete | `submit_plans` or `manage_orgs` | An organization's members holding `submit_plans` (not a service account) and its administrators for its group, and `manage_any_orgs` for any |
| Add or remove members, write a policy | `submit_plans` | An organization's members for its group, and `manage_any_orgs` for any. A policy write that leaves a policy other than `model_params`, `smaller_failed` and `vault_resource` on, and adding members while such a policy is on, also take a re-run's grants: `read_plans` and `cancel_plans` (`read_any_plans` and `cancel_any_plans` for an owner other than the caller's organization) and `read_clients` |

So an organization's members fill, rename and delete its groups, as they do
with its campaigns. A group outside the caller's
scope is a `403`, as a campaign is: a group id is minted, so a caller that
names one was given it.
