#!/usr/bin/env bash
#
# Start the PostgreSQL server that CI runs the SQL tests against, in a local
# Docker container, and print the `TEST_DATABASE_URL` that points at it.
#
# The image comes from the `postgres` service in `.github/workflows/ci.yml`, so
# a local run uses the same server version as CI. The cluster flags match CI
# too: trust authentication, and a `C` locale, because listings sort in byte
# order.
#
# The container is named `pipette-test-postgres` and stays after the script
# exits. A second run reuses it. If CI moves to a different image, the next run
# removes the old container and starts a new one. Each test makes and drops a
# schema of its own, so the container needs no reset between runs.
#
# Only the export line goes to stdout; progress goes to stderr.
#
# Usage:
#   export_line=$(scripts/dev-postgres.sh) && eval "$export_line"
#   PORT=5433 scripts/dev-postgres.sh         # a port other than 5432
#   docker rm -f pipette-test-postgres        # remove the container
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

: "${PORT:=5432}"
NAME=pipette-test-postgres
WORKFLOW=.github/workflows/ci.yml

reject_container() {
  echo "$NAME does not match the CI PostgreSQL setup: $1" >&2
  echo "Remove it with: docker rm -f $NAME" >&2
  exit 1
}

command -v docker >/dev/null || { echo "docker is not on PATH" >&2; exit 1; }

image=$(sed -nE 's/^[[:space:]]+image:[[:space:]]+(postgres:[^[:space:]]+).*/\1/p' "$WORKFLOW" | head -n 1)
if [[ -z "$image" ]]; then
  echo "No 'image: postgres:...' line in $WORKFLOW; update this script to match it." >&2
  exit 1
fi

current=$(docker container inspect --format '{{.Config.Image}}' "$NAME" 2>/dev/null || true)
if [[ -n "$current" && "$current" != "$image" ]]; then
  echo "Removing $NAME: it runs $current, and CI runs $image." >&2
  docker rm -f "$NAME" >/dev/null
  current=""
fi

if [[ -n "$current" ]]; then
  container_env=$(docker container inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "$NAME")
  printf '%s\n' "$container_env" | grep -Fxq 'POSTGRES_HOST_AUTH_METHOD=trust' ||
    reject_container 'host authentication is not configured as trust'
  printf '%s\n' "$container_env" | grep -Fxq 'POSTGRES_INITDB_ARGS=--locale=C --encoding=UTF8' ||
    reject_container 'initialization flags do not request the C locale and UTF8 encoding'
fi

if [[ -z "$current" ]]; then
  echo "Starting $NAME from $image on port $PORT." >&2
  docker run --detach --name "$NAME" \
    --publish "127.0.0.1:$PORT:5432" \
    --env POSTGRES_HOST_AUTH_METHOD=trust \
    --env POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8" \
    "$image" >/dev/null
elif [[ "$(docker container inspect --format '{{.State.Running}}' "$NAME")" != "true" ]]; then
  echo "Starting the stopped container $NAME." >&2
  docker start "$NAME" >/dev/null
else
  echo "$NAME is already running." >&2
fi

# A reused container may publish a different port, but it must bind to loopback.
binding=$(docker port "$NAME" 5432/tcp) || reject_container 'port 5432 is not published'
[[ "$binding" =~ ^127\.0\.0\.1:[0-9]+$ ]] ||
  reject_container "port 5432 is not bound only to loopback ($binding)"
published=${binding##*:}

# Over TCP: the image's first-start init runs a server on the socket alone, and
# a socket probe would report that server ready before the real one starts.
for _ in $(seq 1 60); do
  if docker exec "$NAME" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1; then
    settings=$(docker exec --env PGPASSWORD= --env PGPASSFILE=/dev/null "$NAME" \
      psql -X -U postgres -h 127.0.0.1 -AtF '|' \
      -c "SELECT current_setting('lc_collate'), current_setting('lc_ctype'), current_setting('server_encoding')" \
      2>/dev/null) || reject_container 'a password-free TCP connection failed'
    [[ "$settings" == 'C|C|UTF8' ]] ||
      reject_container "the initialized locale or encoding differs ($settings)"
    echo "PostgreSQL is ready on port $published." >&2
    echo "export TEST_DATABASE_URL=postgres://postgres@127.0.0.1:$published/postgres"
    exit 0
  fi
  sleep 1
done

echo "PostgreSQL in $NAME did not become ready in 60 seconds. See: docker logs $NAME" >&2
exit 1
