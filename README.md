# pipette-mgmt

A Rust/axum HTTP service that runs benchmarks of models on edge devices. It
serves the benchmark catalog, files **plans** (the jobs a benchmark sweep
expands to) into **campaigns** that keep them revisable, queues the jobs, and
hands them to registered **devices**, which run them and submit measurements.
It scores eval completions through an upstream scoring service and writes the
results to a Parquet warehouse.

**Users** sign in, belong to **organizations**, and are granted permissions;
devices are owned by a user or an organization. The **vault** holds private
artifacts, such as a customer's own model weights, and serves them only to their
owner's devices. The **console app** at `/app/` is the web UI over the same API
([ui/README.md](ui/README.md)). Records
(accounts, organizations, devices, plans, campaigns, vault records) live on
PostgreSQL; the queue, submissions, warehouse and blobs live on an object store.

This is the server. The devices run the client, from the
[pipette-clients repo](https://github.com/Liquid4All/pipette-clients).

## Quick start

```bash
cargo build --release

# Copy the example config and seed the benchmark catalog. The asr_eval
# entries are organization data, not in this repository: a deployment puts
# each into its organization's private catalog with
# `pipette-mgmt orgs benchmarks put <org> <file>`.
cp examples/config.toml config.toml
mkdir -p sample_data
cp -r examples/benchmarks sample_data/benchmarks

# Start the HTTP server
./target/release/pipette-mgmt --config config.toml serve

# Process pending submissions (run via cron)
./target/release/pipette-mgmt --config config.toml process-submissions

# Score eval submissions, if enabled (run via cron)
./target/release/pipette-mgmt --config config.toml score-eval
```

The config path can also be set via `PIPETTE_MGMT_CONFIG` env var. See
[examples/config.toml](examples/config.toml) for a local starter config,
[crates/pipette-core/src/config.rs](crates/pipette-core/src/config.rs) for every
key, and [docs/operations.md](docs/operations.md) for production configuration
and scheduling.

## Documentation

Contracts, after [architecture.md](docs/architecture.md) (system design) and
[topology.md](docs/topology.md) (who exists, owns and may do what):

- [httpapi.md](docs/httpapi.md): the device protocol and every route; the
  OpenAPI document is served at `GET /api/v1/openapi.json`, and rendered as
  the API reference at `/docs/`
- [authentication.md](docs/authentication.md): identity, request signing, sign-in, permissions
- [plan-ingestion.md](docs/plan-ingestion.md): plan expansion, ingestion rules and capability matching
- [queue.md](docs/queue.md): the job queue and its claim protocol
- [storage.md](docs/storage.md): object-store layout, records, submissions and the warehouse
- [benchmarks.md](docs/benchmarks.md): the benchmarks, their payloads and how to read the results
- [scoring-service.md](docs/scoring-service.md): the scoring service contract
- [client-integration.md](docs/client-integration.md): what a client implements

Running and changing it:

- [cli.md](docs/cli.md): CLI reference; the config keys are documented in
  [config.rs](crates/pipette-core/src/config.rs)
- [operations.md](docs/operations.md): deployment, scheduling and runbooks
- [ui/README.md](ui/README.md): the console app
- [development.md](docs/development.md): building, testing and CI;
  [ci/README.md](ci/README.md) for the checks
- [adding-a-metric.md](docs/adding-a-metric.md): adding a measured field and its metric, here and in the clients
- [third-party-attribution.md](docs/third-party-attribution.md): the license notices a release ships

Eval methodology ([docs/methodology/](docs/methodology/)):
[GPQA Diamond](docs/methodology/gpqa_diamond-2026.06.1.md),
[IFBench](docs/methodology/ifbench-2026.06.1.md),
[IFStruct](docs/methodology/ifstruct-release_v1_0.md),
[MATH-500](docs/methodology/math_500-2026.06.1.md), and how
[vl_spec_decode rows pair](docs/methodology/vl-spec-decode.md).

Design notes, the why behind decisions ([docs/design/](docs/design/)):
[the vault](docs/design/vault-resources.md),
[records on PostgreSQL](docs/design/sql-record-store.md),
[device telemetry](docs/design/device-telemetry.md),
[the descriptor catalog](docs/design/descriptor-catalog.md),
[vl_spec_decode](docs/design/vl-spec-decode.md) and its
[dataset delivery](docs/design/vl-spec-decode-dataset-delivery.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for local development, CI, and pull
request conventions. Please also review the [Code of Conduct](CODE_OF_CONDUCT.md)
and [Security Policy](SECURITY.md) before participating.

## License

Copyright 2026 Liquid AI, Inc.

Licensed under the Apache License, Version 2.0 (the "License"). You may not use
this project except in compliance with the License. You may obtain a copy of the
License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software distributed
under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
CONDITIONS OF ANY KIND, either express or implied. See the License for the
specific language governing permissions and limitations under the License.

See also [LICENSE](LICENSE), [NOTICE](NOTICE), and
[THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md).
