#!/usr/bin/env bash
# Run little_helper in podman against a project directory.
#
#   ./run.sh /path/to/project                # TUI (default)
#   ./run.sh once "prompt" /path/to/project  # headless one-shot (new session)
#   ./run.sh resume "prompt" /path/to/project # headless turn in the latest session
#
# State (config + sessions + git) lives in $STATE_DIR on the host so it
# survives between runs. On first run run.sh seeds a config.toml pointing at
# the default backend, so you don't have to hand-edit it.
set -euo pipefail
cd "$(dirname "$0")"

IMAGE="${IMAGE:-little_helper}"
STATE_DIR="${STATE_DIR:-$HOME/little-helper-state}"
CPUS="${CPUS:-8}"
MEMORY="${MEMORY:-16g}"

# Host-specific values (backend URL, model, ...) belong in run.local.env
# (gitignored), e.g.:
#   BACKEND=https://your-llm-host/v1
#   MODEL=your-model-name
# Nothing sensitive is committed to the repo.
if [ -f run.local.env ]; then
    # shellcheck disable=SC1091
    source run.local.env
fi

# Backend: an OpenAI-compatible chat endpoint (self-signed cert setups
# are covered by HARNESS_TLS_VERIFY=0 below).
BACKEND="${BACKEND:-https://llm.example.lan/v1}"
MODEL="${MODEL:-default}"

# usage parsing
MODE="tui"
if [ "${1:-}" = "once" ] || [ "${1:-}" = "resume" ]; then
    MODE="$1"
    shift
    PROMPT="${1:?usage: ./run.sh ${MODE} \"prompt\" /path/to/project}"
    shift
fi
WORKSPACE="${1:?usage: ./run.sh [once|resume \"prompt\"] /path/to/project}"
NAME="little-helper-$(basename "$WORKSPACE")"
NETWORK_NAME="harness-net"
DB_CONTAINER_NAME='harness_postgres'

mkdir -p "$WORKSPACE" "$STATE_DIR"

# 1. Ensure the shared network exists
if ! podman network exists "$NETWORK_NAME"; then
    podman network create "$NETWORK_NAME" >/dev/null
fi

# 2. PostgreSQL companion container (local only, never published). The
# password is a RANDOM value persisted in $STATE_DIR/.pg_password
# (host-local state, gitignored) — no credential is committed to the
# repo. Supply PG_PASSWORD in run.local.env to pin your own.
DB_PASS_FILE="$STATE_DIR/.pg_password"
gen_pg_password() { head -c 16 /dev/urandom | base64 | tr -d '/+=' | cut -c1-24; }
db_running() { podman ps --format "{{.Names}}" | grep -q "^${DB_CONTAINER_NAME}$"; }

PG_ROTATE=0
if [ -f "$DB_PASS_FILE" ]; then
    PG_PASSWORD="$(cat "$DB_PASS_FILE")"
elif [ -n "${PG_PASSWORD:-}" ]; then
    : # supplied via env or run.local.env
elif ! db_running; then
    PG_PASSWORD="$(gen_pg_password)"
else
    # pre-existing container without a recorded password: generate one
    # and rotate the DB user to match (local socket is peer auth as
    # the postgres user)
    PG_PASSWORD="$(gen_pg_password)"
    PG_ROTATE=1
fi

if ! podman image exists docker.io/library/postgres:16-alpine; then
    echo "Pulling PostgreSQL image..."
    podman pull docker.io/library/postgres:16-alpine >/dev/null
fi

if ! db_running; then
    echo "Starting PostgreSQL companion container..."
    podman run -d --name "$DB_CONTAINER_NAME" \
        --network "$NETWORK_NAME" \
		--replace \
        -e POSTGRES_PASSWORD="$PG_PASSWORD" \
        -e POSTGRES_DB=little_helper \
        -v harness_pgdata:/var/lib/postgresql/data:Z \
        postgres:16-alpine >/dev/null

    echo "Waiting for PostgreSQL to start..."
    until podman exec "$DB_CONTAINER_NAME" pg_isready -U postgres >/dev/null 2>&1; do
        sleep 1
    done
else
    if [ "$PG_ROTATE" = 1 ]; then
        podman exec "$DB_CONTAINER_NAME" psql -U postgres -v ON_ERROR_STOP=1 \
            -c "ALTER USER postgres PASSWORD '${PG_PASSWORD}';"
    elif [ -n "${PG_PASSWORD:-}" ] && [ ! -f "$DB_PASS_FILE" ]; then
        echo "warning: PG_PASSWORD supplied but no recorded password for the existing DB; verify it matches" >&2
    fi
fi
if [ ! -f "$DB_PASS_FILE" ]; then
    printf '%s' "$PG_PASSWORD" > "$DB_PASS_FILE"
    chmod 600 "$DB_PASS_FILE"
fi


# First-run seed of the backend config (idempotent — never overwrite an
# existing config.toml; the binary itself seeds the state dir + repos).
if [ ! -f "$STATE_DIR/config.toml" ]; then
    cat > "$STATE_DIR/config.toml" <<EOF
# Harness configuration (seeded by run.sh). Git-tracked in /state.

[backend]
base_url = "$BACKEND"
api_key = "none"
model = "$MODEL"
temperature = 0.2
max_tokens = 256000

[context]
max_tokens = 240000
summarize = "auto"
summarize_threshold = 0.7
keep_recent_turns = 8

[workspace]
path = "/workspace"
auto_git = true

[tools]
enabled = true
EOF
    echo "Seeded $STATE_DIR/config.toml (backend $BACKEND, model $MODEL)"
fi

# The backend may be a LAN hostname the container can't resolve; derive
# the host from $BACKEND and pass a --add-host entry when it resolves
# on the host machine.
BACKEND_HOST="$(printf '%s' "$BACKEND" | sed -E 's#^[a-z+]+://([^/:]+)(:[0-9]+)?(/.*)?$#\1#')"
HOSTS_ARGS=()
IP="$(getent hosts "$BACKEND_HOST" 2>/dev/null | awk '{print $1}' | head -n1 || true)"
if [ -n "${IP:-}" ]; then
    HOSTS_ARGS=(--add-host "${BACKEND_HOST}:${IP}")
fi

if [ "$MODE" = "tui" ]; then
    ENTRY_ARGS=(tui)          # bare `little_helper` is headless; `tui` launches the TUI
elif [ "$MODE" = "resume" ]; then
    ENTRY_ARGS=(resume "$PROMPT")
else
    ENTRY_ARGS=("$PROMPT")
fi

podman run -it --rm \
    --init \
    --name "$NAME" \
	--network "$NETWORK_NAME" \
    --cpus "$CPUS" \
    --memory "$MEMORY" \
    -v "$WORKSPACE:/workspace:z" \
    -v "$STATE_DIR:/state:z" \
    "${HOSTS_ARGS[@]}" \
    -e HARNESS_STATE=/state \
    -e HARNESS_WORKSPACE=/workspace \
    -e HARNESS_TLS_VERIFY=0 \
	-e DATABASE_URL="postgres://postgres:${PG_PASSWORD}@${DB_CONTAINER_NAME}:5432/little_helper" \
    "$IMAGE" \
    "${ENTRY_ARGS[@]}"

