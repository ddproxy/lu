#!/bin/sh
# Cross-version wire check: run a controller from one build and an agent from another on loopback
# and confirm the node registers, reports itself and can drain and undrain. An older agent
# against a newer controller and a newer agent against an older controller must both work.
#
#   scripts/compat-test.sh <controller-bin-dir> <agent-bin-dir> [<db-seed-bin-dir>]
#
# Each directory holds the `lu` and `ting` binaries of one release. With a third directory, a
# controller from that release first creates and registers the node in the database, and the
# controller under test then starts on that existing database, which checks the in-place schema
# migration.
set -eu

[ "$#" -eq 2 ] || [ "$#" -eq 3 ] || { echo "usage: $0 <controller-bin-dir> <agent-bin-dir> [<db-seed-bin-dir>]" >&2; exit 2; }
ctl_dir=$(CDPATH= cd -- "$1" && pwd)
agent_dir=$(CDPATH= cd -- "$2" && pwd)
for f in "$ctl_dir/lu" "$agent_dir/ting" "$agent_dir/lu"; do
    [ -x "$f" ] || { echo "missing executable: $f" >&2; exit 2; }
done

work=$(mktemp -d)
pids=""
cleanup() {
    for pid in $pids; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT INT TERM

port=$((20000 + $$ % 20000))
umask 077
mkdir -p "$work/artifacts" "$work/ting-work"
agent_token=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')
op_token=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')
printf '%s' "$agent_token" > "$work/ting.token"
printf '%s' "$op_token" > "$work/op.token"
agent_hash=$(printf '%s' "$agent_token" | sha256sum | cut -d' ' -f1)
op_hash=$(printf '%s' "$op_token" | sha256sum | cut -d' ' -f1)

cat > "$work/lu.toml" <<CFG
[controller]
listen = "127.0.0.1:$port"
db = "$work/lu.db"
artifacts_dir = "$work/artifacts"

[[controller.agents]]
name = "compat-a"
token_sha256 = "$agent_hash"

[[controller.operators]]
name = "ops"
token_sha256 = "$op_hash"

[log]
level = "warn"
format = "text"
stderr = true
CFG

cat > "$work/ting.toml" <<CFG
[agent]
name = "compat-a"
controller_url = "http://127.0.0.1:$port"
token_file = "$work/ting.token"
work_dir = "$work/ting-work"
harness = "opencode"
heartbeat_secs = 2

[log]
level = "warn"
format = "text"
stderr = true
CFG

LU_URL="http://127.0.0.1:$port"
LU_TOKEN_FILE="$work/op.token"
export LU_URL LU_TOKEN_FILE

wait_healthy() {
    i=0
    until curl -fs "$LU_URL/v1/health" > /dev/null 2>&1; do
        i=$((i + 1))
        [ "$i" -le 50 ] || { echo "controller did not start"; cat "$work/controller.log"; exit 1; }
        sleep 0.2
    done
}

if [ "$#" -eq 3 ]; then
    seed_dir=$(CDPATH= cd -- "$3" && pwd)
    "$seed_dir/lu" serve --config "$work/lu.toml" > "$work/seed.log" 2>&1 &
    seed_pid=$!
    wait_healthy
    "$seed_dir/ting" --config "$work/ting.toml" > "$work/seed-agent.log" 2>&1 &
    seed_agent=$!
    i=0
    until "$seed_dir/lu" nodes --json 2> /dev/null | grep -q '"compat-a"'; do
        i=$((i + 1))
        [ "$i" -le 100 ] || { echo "seed node never registered"; exit 1; }
        sleep 0.2
    done
    kill "$seed_agent" "$seed_pid" 2> /dev/null || true
    wait "$seed_agent" "$seed_pid" 2> /dev/null || true
fi

"$ctl_dir/lu" serve --config "$work/lu.toml" > "$work/controller.log" 2>&1 &
pids="$pids $!"
wait_healthy

"$agent_dir/ting" --config "$work/ting.toml" > "$work/agent.log" 2>&1 &
pids="$pids $!"

i=0
until "$ctl_dir/lu" nodes --json 2> /dev/null | grep -q '"compat-a"'; do
    i=$((i + 1))
    [ "$i" -le 100 ] || { echo "node never registered"; cat "$work/agent.log" "$work/controller.log"; exit 1; }
    sleep 0.2
done

self=$("$agent_dir/ting" status --json --config "$work/ting.toml")
echo "$self" | grep -q '"online":true' || { echo "agent not online: $self"; exit 1; }

# The write controls are additive. An older controller must ignore the new headers and query
# parameter; a newer one must accept them. Use a config file so the generated token never appears
# in a process argument.
cat > "$work/write-controls.cfg" <<CFG
header = "Authorization: Bearer $op_token"
header = "If-Match: *"
header = "Idempotency-Key: compat-$$"
CFG
code=$(curl -sS -o "$work/probe.out" -w '%{http_code}' --config "$work/write-controls.cfg" \
    -X PUT -H 'Content-Type: application/json' --data '{"max_running":1}' \
    "$LU_URL/v1/groups/compat/limit?dry_run=1")
[ "$code" = 200 ] || { echo "optional write controls rejected: $code"; cat "$work/probe.out"; exit 1; }
code=$(curl -sS -o "$work/probe.out" -w '%{http_code}' --config "$work/write-controls.cfg" \
    -X POST -H 'Content-Type: application/json' --data '{}' \
    "$LU_URL/v1/jobs?dry_run=1")
[ "$code" = 422 ] || { echo "dry-run validation changed: $code"; cat "$work/probe.out"; exit 1; }

"$agent_dir/ting" drain --config "$work/ting.toml" > /dev/null
"$ctl_dir/lu" nodes --json | grep -q '"draining":true' || { echo "drain not visible to the controller"; exit 1; }
"$agent_dir/ting" undrain --config "$work/ting.toml" > /dev/null
"$ctl_dir/lu" nodes --json | grep -q '"draining":false' || { echo "undrain not visible to the controller"; exit 1; }

schema=$("$ctl_dir/lu" status --json | tr -d ' \n' | grep -o '"schema_version":[0-9]*' || true)
ctl_version=$("$ctl_dir/lu" --version | tr -d '\n')
agent_version=$("$agent_dir/ting" --version | tr -d '\n')
echo "compat ok: controller [$ctl_version] with agent [$agent_version] ${schema:+($schema)}"
