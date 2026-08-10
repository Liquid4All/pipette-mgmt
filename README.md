# pipette-mgmt

A Rust/axum HTTP service that manages benchmarks for edge devices. It serves a benchmark catalog, accepts measurement submissions, scores eval completions via an upstream evals server, and writes results to Parquet.

## Quick start

```bash
cargo build --release

# Copy the example config and seed the benchmark catalog
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
[examples/config.toml](examples/config.toml) for a local starter config and
[docs/cli.md](docs/cli.md) / [docs/operations.md](docs/operations.md) for
production configuration and scheduling.

## Running Benchmarks
This is just the server. To run benchmarks, you want to use the client. See the [pipette-clients repo](https://github.com/Liquid4All/pipette-clients) for that.

## Documentation

- [architecture.md](docs/architecture.md) -- system design
- [httpapi.md](docs/httpapi.md), [cli.md](docs/cli.md), [operations.md](docs/operations.md) -- API, CLI, deployment
- [benchmarks.md](docs/benchmarks.md), [storage.md](docs/storage.md), [scoring-service.md](docs/scoring-service.md) -- data contracts
- [planner.md](docs/planner.md), [plan-ingestion.md](docs/plan-ingestion.md), [client-integration.md](docs/client-integration.md) -- planned jobs
- [authentication.md](docs/authentication.md), [development.md](docs/development.md), [docs/methodology/](docs/methodology/) -- auth, development, methodology

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
