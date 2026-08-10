# Operations

## 1. Configuration

All settings live in a TOML config file. See [cli.md](cli.md) for the full
schema and flag reference.

`pipette-mgmt` exits immediately if `evals_server_url` is not set in the config.

`serve` holds no TLS and binds every interface by default. See
[§5.6](#56-network-exposure-and-tls) for the deployment shape that requires.

## 2. Running

```bash
# HTTP server
pipette-mgmt serve

# Fast submission pass
pipette-mgmt process-submissions

# Slow eval scorer
pipette-mgmt score-eval
```

### 2.1. Stopping the server

`serve` shuts down gracefully on `SIGTERM` (what a container runtime sends
before it kills the process) or `SIGINT` (`^C`). It stops accepting new
connections and lets the requests already in progress finish, so a rolling
restart does not cut a client off mid-submission. The log records
`shutdown requested` with the signal, then `server stopped` once the last
request completes.

A runtime that force-kills after a grace period will still interrupt anything
still running when that period expires; set the grace period above the slowest
request you expect to serve.

## 3. Cron setup

Scoring runs as **two** crons:

1. **`process-submissions`** (fast, frequent) — scores non-eval submissions,
   routes eval submissions into the score-queue, and finalizes evals already
   scored by the slow pass. `score` is a backward-compatible alias.

2. **`score-eval`** — drains the score-queue and calls the scoring service. It
   uses its own advisory lock, so overlapping cron ticks exit instead of
   double-scoring while leaving `process-submissions` unblocked.

```
# /etc/cron.d/pipette-mgmt-score
*/5 * * * * app /usr/local/bin/pipette-mgmt --config /etc/pipette-mgmt/config.toml process-submissions 2>&1 | logger -t pipette-mgmt-process-submissions
# required for eval benchmarks
*/5 * * * * app /usr/local/bin/pipette-mgmt --config /etc/pipette-mgmt/config.toml score-eval          2>&1 | logger -t pipette-mgmt-score-eval
```

`score-eval` is sequential; its `score-eval` lock means a second invocation
exits immediately rather than running concurrently, so overlapping cron ticks
are safe. For S3 backends, ensure AWS credentials are available in the cron
environment (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`), or
run on an EC2/ECS instance with an IAM role.

See [cli.md](cli.md) for per-command output format and exit codes.

Run `process-submissions`, `score-eval`, and `queue-maintenance` once per
schedule interval, regardless of server replica count. Use one cron host or one
Kubernetes CronJob. See [planner.md §Processes](planner.md#processes).

### 3.1. Job queue maintenance

A single cron job handles all stale state in the `todo/` queue:

**Expired leases** — `leased/` files whose `{lease_expiry}` timestamp in the
filename is in the past are renamed back to `avail/{job_id}.{expires_at}.json`,
making them eligible for reassignment. `queue-maintenance` reads the job body
once per recycled lease to recover `expires_at` for the rename target.

**Expired jobs** — `avail/` files whose `{expires_at}` filename component is
in the past (see [planner.md](planner.md) for the filename convention) are
treated as abandoned: a synthetic failure record is generated, the job file is
deleted from `avail/`, and all associated `denied/` and `eligible/` markers are
cleaned up. No body read is required to detect expiry.

**All-denied jobs** — `avail/` jobs with a `clients` list (and no `requires`
flags) whose every listed client has a `denied/` marker can never succeed and
are escalated the same way: synthetic failure record, `avail/` entry deleted,
markers cleaned up. This pass is the sole owner of the all-denied rule — the
submit path only records the denial — so an un-expiring job whose roster is
exhausted waits at most one cron interval, unclaimable in the meantime
(`claim` skips denied candidates). Job bodies are read only for jobs that
have at least one denial.

**Stale tmp/ files** — partial job files left behind by a crashed planner are
deleted once they are older than `todo_tmp_max_age_secs` (default 86400 —
24 hours).

Run every 1–5 minutes. The interval is a latency knob, not a correctness
requirement: every pass is idempotent and eventually consistent, and the
handlers enforce lease and expiry rules themselves, so nothing breaks at a
slower cadence — but the interval bounds how quickly stale state is
reconciled. Concretely, a crashed device's job becomes claimable again only
at lease expiry **plus** one cron interval (the interval is dead time added
to every silently-failed claim — if you shorten `plan_lease_duration_secs`
to speed up dead-device turnaround, tighten the cron to match), and new jobs
and profile changes are reflected in the eligible index within one interval
(§3.2). Every minute is the right default when claim latency matters:

```
# /etc/cron.d/pipette-mgmt-queue
* * * * * app flock -n /run/lock/pipette-mgmt-queue.lock /usr/local/bin/pipette-mgmt --config /etc/pipette-mgmt/config.toml queue-maintenance 2>&1 | logger -t pipette-mgmt-queue
```

`queue-maintenance` takes no lock of its own, so **serializing runs is a
correctness requirement, not just an optimization** — the `flock -n` above
makes a tick exit immediately while the previous run is still going, rather
than queue behind it. A skipped tick loses no work: each run reconciles
everything outstanding at that moment.

Overlapping runs *can* be destructive. The reconciliation sweep removes a
marker only after its storage key is seen orphaned in two consecutive runs, and
first sightings are persisted (`todo/.gc-candidates`) for the next run to read.
Each run consumes that file as it starts — reading then clearing it — so a run
that fails partway leaves no stale sightings behind for a later run to mistake
as consecutive; it merely delays GC by one interval. That rule is safe
only because the gap between sightings is a full cron interval — long enough
for an entity caught mid-transition (a job racing `leased/ → avail/`, a client
racing re-registration) to become visible. Two overlapping runs can both miss
the same in-transition entity and, because the second reads the first's
candidate file, count as the two "consecutive" sightings seconds apart —
deleting a live job's `eligible/`/`denied/` markers, which are never rebuilt
(the job's key sorts behind the eligible-index cursor), leaving it permanently
unclaimable. Overlap also lets an older run rewind the
eligible-index cursor a newer one wrote, causing wasteful re-indexing. If you
deploy as a **Kubernetes CronJob**, set `concurrencyPolicy: Forbid` to get the
same serialization the `flock -n` provides.

**Clock synchronization.** The expiry pass decides a job is overdue using the
maintenance host's clock, while the `claim`/`reclaim` gates in `serve` refuse
expired jobs using their own host's clock. If the maintenance host's clock runs
*ahead* of a serve host's, there is a window around each deadline where `serve`
still hands the job out while `queue-maintenance` expires it — which can write a
synthetic failure for a job a client is actively running, producing two
contradictory records. Keep all hosts NTP-synced. A maintenance clock that lags
is harmless (it only expires jobs slightly late); it is the *ahead* direction
that must be avoided, so single-host or shared-clock-domain deployments are
inherently safe.

On S3 backends, ensure AWS credentials are available in the cron environment.
The `tmp/` age check can be replaced with an S3 lifecycle rule expiring objects
under the `todo/tmp/` prefix after 1 day.

### 3.2. Eligible index maintenance

The `todo/eligible/` index is maintained exclusively by `queue-maintenance` —
`serve` replicas only read it. `queue-maintenance` updates it incrementally on
each run using two signals (see [planner.md](planner.md)):

- **New jobs**: `queue-maintenance` maintains a key cursor into `avail/`
  (the last processed `{job_id}.{expires_at}.json` key). Because `job_id`
  is `job-{UUIDv7}`, keys sort in arrival order; each run processes only keys
  past the cursor, skipping the body fetch and match evaluation for jobs already
  indexed. On the required Express One Zone backend the listing itself is not
  shrunk (Express has no server-side `start-after`, so the full `avail/` prefix
  is listed and filtered client-side); see [planner.md](planner.md).
- **Updated client profiles**: when `PATCH /clients/me` changes a device
  profile, the serve handler writes `todo/pending-reindex/{client_id}.{uuid}`
  markers — a distinct key per request. `queue-maintenance` lists
  `pending-reindex/` each run, re-evaluates each flagged client (reading its
  record fresh) against all current `avail/` jobs, and deletes exactly the
  flag keys it captured before the rebuild — so a profile change landing
  mid-run keeps its own flag and is re-evaluated on the next run.

- **Orphaned markers** (reconciliation sweep): `queue-maintenance` reconciles
  every per-entity marker tree — `eligible/clients/`, `denied/`, `suspended/`,
  `pending-reindex/`, `pending-reindex-jobs/`, and `leased/` — against two
  sources of truth: a job is
  live iff its id is in `avail/` **or** `leased/`, and a client is live iff it
  is in the auth roster (`list_clients()`). Any marker whose job **or** client
  no longer exists is collected, so the sweep is cause-agnostic — it eliminates
  detritus from a completed/expired/planner-deleted job, a deleted client
  (including a `clients delete` whose best-effort purge failed), or a lost race
  alike. One candidate set keyed by storage key covers every tree. A *leased*
  job is live, not orphaned: its `avail/` key sorts behind the new-jobs cursor,
  so markers dropped mid-lease would never be rebuilt and a recycled lease
  would leave the job permanently unclaimable. A lease held by a *deleted*
  client is **recycled** back to `avail/` (its job body preserved, the job
  claimable again), never deleted. Collection is confirmed before it acts: a
  marker is removed only after its storage key is seen orphaned in two
  consecutive runs (or its job was positively removed this run), so an entity
  that briefly races the listings never loses live markers.

A run with no new jobs, no pending-reindex flags, and no orphaned markers
touches only empty prefixes — essentially free. New jobs and profile updates
are reflected in the eligible index within one cron interval (§3.1) — a
freshly planted job cannot be claimed by anyone until the run after its
promotion to `avail/` builds its eligible markers.

## 4. Reprocessing

**Whole warehouse.** To re-score everything: move files from
`submissions/processed/` back to `submissions/incoming/`, clear all
scorer-owned output directories (`warehouse/results/` and
`warehouse/eval_sample_results/`), then run cron. On S3, use `aws s3 mv`
to move objects between the `processed/` and `incoming/` prefixes, and
delete the relevant warehouse and eval sample result objects.

**One eval benchmark (e.g. after a scorer fix).** Use
[`pipette-mgmt requeue-eval --benchmark-id <id>`](cli.md#pipette-mgmt-requeue-eval),
which validates the benchmark is a configured eval, identifies its jobs
from the warehouse metrics, and copies their processed bodies back into
`incoming/` under fresh `job_id`s with `submitted_at = now`. Then run
`score`. The next `score` writes new warehouse rows and a new
`processed/{new_job_id}` archive for each re-stage; original rows and
archives stay in place alongside the fresh ones. Because each re-stage is
brand-new, re-running over the whole benchmark doubles the set — scope
repeat runs with `--submitted-before <pre-migration time>` (the fresh
copies carry `submitted_at = now` and fall outside the window). Re-run
per dataset to cover an eval's other benchmarks. Works on both backends.

## 5. Docker deployment

The `liquidai/pipette-mgmt` image (multi-arch: `linux/amd64`, `linux/arm64`) ships
the `pipette-mgmt` binary. The default `CMD` is `serve` — override with
`process-submissions`, `score-eval`, or `clients <action>` to run other
subcommands. The image runs as
non-root uid `10001` and reads its config from `/etc/pipette-mgmt/config.toml`
(override with `PIPETTE_MGMT_CONFIG`).

The only env vars consumed at runtime are `PIPETTE_MGMT_CONFIG`, `RUST_LOG`,
and the `AWS_*` family (when using an S3-compatible backend). Everything else —
listen address, bucket names, prefixes, timeouts — comes from the TOML config.

### 5.1. Local filesystem (mounted volume)

`config.toml`:

```toml
evals_server_url = "http://evals:8080"
listen_addr = "0.0.0.0:3000"

[storage]
backend = "local_fs"
data_dir = "/data"

[auth_storage]
backend = "local_fs"
data_dir = "/data"
```

```bash
docker run -d --name pipette-mgmt \
  -p 3000:3000 \
  -v "$(pwd)/config.toml:/etc/pipette-mgmt/config.toml:ro" \
  -v "$(pwd)/data:/data" \
  liquidai/pipette-mgmt:<version>
```

The `/data` mount must be writable by uid `10001` (the image's `/data` is
already mode `1777`, so a fresh bind mount works as long as the host directory
itself permits writes from that uid — `chmod 1777 ./data` or
`chown 10001:10001 ./data` on the host).

### 5.2. AWS S3

`config.toml`:

```toml
evals_server_url = "http://evals:8080"
listen_addr = "0.0.0.0:3000"

[storage]
backend = "s3"
bucket = "my-pipette-data"
prefix = "v1/"
region = "us-east-1"

[auth_storage]
backend = "s3"
bucket = "my-pipette-auth"
region = "us-east-1"

# Required when using the planner. Must be a separate S3 Express One Zone bucket
# — the todo/ tree relies on atomic RenameObject, which is only available on
# Express One Zone. See docs/storage.md §9.
[todo_storage]
backend = "s3"
bucket = "my-pipette-todo--use1-az4--x-s3"  # Express One Zone bucket naming convention
region = "us-east-1"
```

```bash
docker run -d --name pipette-mgmt \
  -p 3000:3000 \
  -v "$(pwd)/config.toml:/etc/pipette-mgmt/config.toml:ro" \
  -e AWS_ACCESS_KEY_ID \
  -e AWS_SECRET_ACCESS_KEY \
  -e AWS_REGION=us-east-1 \
  liquidai/pipette-mgmt:<version>
```

On EC2/ECS/EKS, omit the access-key env vars and let the standard AWS
credential chain pick up the instance role / task role / IRSA token —
`object_store` resolves the same env vars and metadata endpoints as the AWS
SDK (`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`, `AWS_WEB_IDENTITY_TOKEN_FILE`,
EC2 IMDSv2). Use **separate buckets** for `[storage]`, `[auth_storage]`, and
`[todo_storage]` so benchmark/warehouse data, private client identities, and
the job queue can have different IAM policies. The `[todo_storage]` bucket must
be Express One Zone (see [storage.md §9](storage.md#todo-requires-s3-express-one-zone)).

A process that renames against `todo/` — `serve` (claim, heartbeat) and
`queue-maintenance` (lease recycle) — validates this at startup by probing
`RenameObject` and **refuses to start** if the bucket does not support it (a
general-purpose bucket returns `NotImplemented`). This catches a misconfigured
regular-S3 bucket before it can silently corrupt claims (a non-atomic
copy-then-delete would let two clients win the same job). The `clients` admin
commands only list and delete markers, never rename, so they skip the probe.

### 5.3. Cloudflare R2

R2 is S3-compatible. Set `region = "auto"` and point `endpoint` at the
account-scoped R2 URL. Generate an R2 API token (Account → R2 → Manage R2 API
Tokens) and pass it as the AWS keys.

`config.toml`:

```toml
evals_server_url = "http://evals:8080"
listen_addr = "0.0.0.0:3000"

[storage]
backend = "s3"
bucket = "pipette-data"
prefix = "v1/"
region = "auto"
endpoint = "https://<account_id>.r2.cloudflarestorage.com"

[auth_storage]
backend = "s3"
bucket = "pipette-auth"
region = "auto"
endpoint = "https://<account_id>.r2.cloudflarestorage.com"
```

```bash
docker run -d --name pipette-mgmt \
  -p 3000:3000 \
  -v "$(pwd)/config.toml:/etc/pipette-mgmt/config.toml:ro" \
  -e AWS_ACCESS_KEY_ID="<r2_access_key_id>" \
  -e AWS_SECRET_ACCESS_KEY="<r2_secret_access_key>" \
  liquidai/pipette-mgmt:<version>
```

R2 ignores `region` content but the AWS sigv4 signer requires *some* value;
`"auto"` is the conventional choice.

### 5.4. MinIO (and other self-hosted S3)

Point `endpoint` at the MinIO URL. HTTP endpoints are accepted because the
S3 client enables `allow_http` whenever `endpoint` is set in the TOML.

`config.toml`:

```toml
evals_server_url = "http://evals:8080"
listen_addr = "0.0.0.0:3000"

[storage]
backend = "s3"
bucket = "pipette-data"
prefix = "v1/"
region = "us-east-1"
endpoint = "http://minio:9000"

[auth_storage]
backend = "s3"
bucket = "pipette-auth"
region = "us-east-1"
endpoint = "http://minio:9000"
```

```bash
docker run -d --name pipette-mgmt \
  --network minio_net \
  -p 3000:3000 \
  -v "$(pwd)/config.toml:/etc/pipette-mgmt/config.toml:ro" \
  -e AWS_ACCESS_KEY_ID="<minio_access_key>" \
  -e AWS_SECRET_ACCESS_KEY="<minio_secret_key>" \
  -e AWS_REGION=us-east-1 \
  liquidai/pipette-mgmt:<version>
```

Both buckets must exist before startup (`mc mb local/pipette-data`,
`mc mb local/pipette-auth`). MinIO doesn't enforce a real region but the
sigv4 signer needs one — match whatever `MINIO_REGION` is configured with
(defaults to `us-east-1`).

### 5.5. Running other subcommands

Override the default `CMD` to run one-shots or admin commands against the
same image and config:

```bash
# Process pending submissions (cron pattern in §3 — run as a sidecar/job instead of cron in containerized envs)
docker run --rm \
  -v "$(pwd)/config.toml:/etc/pipette-mgmt/config.toml:ro" \
  -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_REGION \
  liquidai/pipette-mgmt:<version> process-submissions

# Score eval submissions
docker run --rm \
  -v "$(pwd)/config.toml:/etc/pipette-mgmt/config.toml:ro" \
  -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_REGION \
  liquidai/pipette-mgmt:<version> score-eval

# List clients
docker run --rm \
  -v "$(pwd)/config.toml:/etc/pipette-mgmt/config.toml:ro" \
  -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_REGION \
  liquidai/pipette-mgmt:<version> clients list
```

### 5.6. Network exposure and TLS

The server speaks plaintext HTTP; TLS terminates in the proxy placed in front of
it. `listen_addr` defaults to `0.0.0.0:3000`, so a container started from the
examples above is reachable on every interface it has.

**The required shape is a TLS-terminating proxy in front of the listen port,
with a network control that admits only that proxy.** Both halves are load
bearing. TLS on the public hop protects clients; the network control is what
keeps the plaintext hop between the proxy and this process from being reachable
directly, which would otherwise offer the same traffic unencrypted on a
different port.

What rides on it:

- `POST /clients/register` with `generate_key: true` returns a freshly generated
  **private key** in the response body — the one exchange in the API that
  carries a long-lived credential. (The supported clients generate their own
  keypair and send `public_key`, so this is a convenience path rather than the
  normal one, but it is reachable and unauthenticated.)
- Registration accepts a pre-auth key as a bearer secret, and every
  authenticated request carries its signature headers in the clear.

The server cannot see what sits in front of it, so it cannot check any of this.
What it does report is the moment a credential actually leaves it:

```
WARN client_id=ev1_a3f8... returned a server-generated private key in the registration response
```

Since the supported clients generate their own keypair, this names a caller
taking the one path where a long-lived secret transits.

How much weight a single line carries depends on who can reach the endpoint.
Under `require_preauth_key`, registration takes a minted secret, so each
occurrence is worth reading on its own. Under the default open policy, any
anonymous caller can produce these at request rate — read a burst as probing
rather than as many separate exposures, and note that each one also leaves a
durable pending client record behind it.

A reference deployment on AWS runs an Application Load Balancer holding the
certificate, listening on 443 with 80 redirecting to it, forwarding to the
container's port over plaintext HTTP inside the VPC. The tasks sit in a public
subnet with public IPs, so the security group admitting the load balancer's
security group on that port is the **only** thing keeping the plaintext port off
the internet — widening that rule exposes it directly, with no second control
behind it.

## 6. Client administration

Use the CLI for client lifecycle operations:

- `pipette-mgmt clients approve <client_id>` approves a pending client.
- `pipette-mgmt clients reject <client_id>` removes a pending registration that should not be approved.
- `pipette-mgmt clients delete <client_id>` deletes the client identity record for either a `pending` or `approved` client.

`pipette-mgmt clients delete` is an admin-only identity operation. In addition
to the client record it removes the client's `todo/suspended/{client_id}.json` marker
(if present) and all `eligible/clients/{client_id}/` markers from the job queue
index. It does not remove historical submissions, processed job records,
warehouse data, or eval sample results that already reference that `client_id`.

### 6.1. Auto-approve at registration

To skip the manual `clients approve` step, configure `[auto_approve]` rules
that match a client's `contact_email` at registration:

```toml
[auto_approve]
# Full addresses — case-insensitive exact match.
emails = ["alice@example.com"]
# Domains — case-insensitive match on the part after `@`.
domains = ["example.org"]
```

Both lists default to empty (auto-approve off). The rule is evaluated only at
registration and does not retroactively promote existing `pending` clients.

**This is not a security control.** `contact_email` is self-reported and never
verified, so anyone can register under an allowed address or domain and
auto-approve themselves. Use it only where unrestricted submission is
acceptable; otherwise leave it off and approve manually. See
[authentication.md §3.1](authentication.md#31-auto-approve-rules).
