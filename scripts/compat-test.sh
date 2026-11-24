#!/bin/sh
# Cross-version wire check: run a controller from one build and an agent from another on loopback
# and confirm the node registers, reports itself and can drain and undrain. An older agent
# against a newer controller and a newer agent against an older controller must both work.
#
# With binary directories, one pairing is run:
#   scripts/compat-test.sh <controller-bin-dir> <agent-bin-dir> [<db-seed-bin-dir>]
#
# With no binary directories, the previous stable releases are downloaded, verified and cached,
# and both pairings are run against the local build:
#   scripts/compat-test.sh [--previous N] [--target TARGET] [--dry-run]
#   scripts/compat-test.sh --tags-file FILE   # offline selection from a local tag list
#   scripts/compat-test.sh --skip-download    # explicit skip, exit 0
#
# Each directory holds the `lu` and `ting` binaries of one release. With a third directory, a
# controller from that release first creates and registers the node in the database, and the
# controller under test then starts on that existing database, which checks the in-place schema
# migration.
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
. "$script_dir/compat-lib.sh"

usage() {
    cat <<EOF
usage: $0 [options] [<controller-bin-dir> <agent-bin-dir> [<db-seed-bin-dir>]]

No directories: verify and cache the previous stable releases, then run both
compatibility directions. Directories: run one scenario using those binaries.

options:
  --previous N     previous stable releases to check (default 2)
  --target T       release target (default: host target)
  --tags-file F    read release tags from F instead of GitHub
  --dry-run        print selection and planned runs without downloading
  --skip-download  exit 0 with a message without running anything
  -h, --help       show this help
EOF
}

compat_write_remote_tags() {
    out=$1
    raw="$out.raw"
    if command -v gh >/dev/null 2>&1 &&
       gh release list --repo ddproxy/tuishou-lu --exclude-drafts --exclude-pre-releases \
           --limit 200 --json tagName --jq '.[].tagName' > "$raw"; then
        mv "$raw" "$out"
        return 0
    fi
    if curl -fsSL -o "$raw" \
        'https://api.github.com/repos/ddproxy/tuishou-lu/releases?per_page=100'; then
        sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$raw" > "$out"
        rm -f "$raw"
        return 0
    fi
    rm -f "$raw"
    return 1
}

compat_download_release() {
    tag=$1
    release_target=$2
    dir=$3
    archive=$4
    sumfile=$5
    asset="lu-$tag-$release_target"
    base="https://github.com/ddproxy/tuishou-lu/releases/download/$tag"
    downloaded=0
    if command -v gh >/dev/null 2>&1; then
        if gh release download "$tag" --repo ddproxy/tuishou-lu \
            --pattern "$asset.tar.gz*" --dir "$dir" --clobber &&
           [ -f "$archive" ] && [ -f "$sumfile" ]; then
            downloaded=1
        fi
    fi
    if [ "$downloaded" -eq 0 ]; then
        echo "compat: gh download unavailable for $tag; trying curl" >&2
        for ext in .tar.gz .tar.gz.sha256; do
            curl -fsSL -o "$dir/$asset$ext" "$base/$asset$ext" ||
                compat_die "download failed: $base/$asset$ext"
        done
    fi
    [ -f "$archive" ] && [ -f "$sumfile" ] ||
        compat_die "release $tag has no $asset archive and checksum"
}

compat_prepare_release() {
    tag=$1
    release_target=$2
    release_cache=$3
    dir="$release_cache/$tag"
    asset="lu-$tag-$release_target"
    archive="$dir/$asset.tar.gz"
    sumfile="$archive.sha256"
    bin="$dir/bin"
    marker="$dir/.sha256"

    mkdir -p "$dir"
    if [ ! -f "$archive" ] || [ ! -f "$sumfile" ]; then
        compat_download_release "$tag" "$release_target" "$dir" "$archive" "$sumfile"
    fi

    digest=$(compat_checksum_verify "$archive" "$sumfile") || exit 1
    if [ -f "$marker" ] && [ "$(cat "$marker")" = "$digest" ] &&
       [ -x "$bin/lu" ] && [ -x "$bin/ting" ]; then
        echo "compat: using cached $tag ($release_target)" >&2
    else
        staged=$(mktemp -d "$dir/extract.XXXXXX")
        if ! tar -xzf "$archive" -C "$staged" --strip-components=1; then
            rm -rf "$staged"
            compat_die "could not extract $archive"
        fi
        if [ ! -x "$staged/lu" ] || [ ! -x "$staged/ting" ]; then
            rm -rf "$staged"
            compat_die "archive $asset.tar.gz has no executable lu and ting"
        fi
        rm -rf "$bin"
        mv "$staged" "$bin"
        printf '%s\n' "$digest" > "$marker"
        echo "compat: verified and extracted $tag ($release_target)" >&2
    fi
    printf '%s\n' "$bin"
}

run_scenario() {
    (
        ctl_dir=$(CDPATH= cd -- "$1" && pwd) ||
            compat_die "missing binary directory: $1"
        agent_dir=$(CDPATH= cd -- "$2" && pwd) ||
            compat_die "missing binary directory: $2"
        for f in "$ctl_dir/lu" "$agent_dir/ting" "$agent_dir/lu"; do
            [ -x "$f" ] || compat_die "missing executable: $f"
        done
        seed_dir=""
        if [ "$#" -eq 3 ] && [ -n "$3" ]; then
            seed_dir=$(CDPATH= cd -- "$3" && pwd) ||
                compat_die "missing binary directory: $3"
            for f in "$seed_dir/lu" "$seed_dir/ting"; do
                [ -x "$f" ] || compat_die "missing executable: $f"
            done
        fi

        work=$(mktemp -d "${TMPDIR:-/tmp}/lu-compat.XXXXXX")
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

        if [ -n "$seed_dir" ]; then
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
    )
}

main() {
    previous=2
    target=""
    tags_arg=""
    dry_run=0
    skip_download=0

    while [ "$#" -gt 0 ]; do
        case "$1" in
            --previous)
                [ "$#" -ge 2 ] || compat_die "--previous needs a count"
                previous=$2
                shift 2 ;;
            --previous=*)
                previous=${1#--previous=}
                shift ;;
            --target)
                [ "$#" -ge 2 ] || compat_die "--target needs a target triple"
                target=$2
                shift 2 ;;
            --target=*)
                target=${1#--target=}
                shift ;;
            --tags-file)
                [ "$#" -ge 2 ] || compat_die "--tags-file needs a path"
                tags_arg=$2
                shift 2 ;;
            --tags-file=*)
                tags_arg=${1#--tags-file=}
                shift ;;
            --dry-run)
                dry_run=1
                shift ;;
            --skip-download)
                skip_download=1
                shift ;;
            -h|--help)
                usage
                exit 0 ;;
            --)
                shift
                break ;;
            -*)
                compat_die "unknown option: $1" ;;
            *)
                break ;;
        esac
    done

    case "$previous" in
        ''|*[!0-9]*) compat_die "--previous must be a positive integer" ;;
    esac
    [ "$previous" -gt 0 ] || compat_die "--previous must be a positive integer"

    if [ "$skip_download" -eq 1 ]; then
        echo "compat: skipped (--skip-download)"
        exit 0
    fi

    if [ -z "$target" ]; then
        target=$(compat_host_target) || exit 1
    fi
    compat_check_target "$target" || exit 1

    if [ "$#" -gt 0 ]; then
        [ "$#" -eq 2 ] || [ "$#" -eq 3 ] || {
            usage >&2
            exit 2
        }
        [ "$dry_run" -eq 0 ] && [ -z "$tags_arg" ] && [ "$previous" -eq 2 ] ||
            compat_die "download options are only valid without binary directories"
        run_scenario "$1" "$2" "${3:-}"
        return 0
    fi

    cache="$repo_root/target/compat-cache/$target"
    cache_tags="$cache/tags.txt"
    current_version=$(compat_current_version "$repo_root") || exit 1

    tag_work=""
    if [ -n "$tags_arg" ]; then
        [ -f "$tags_arg" ] || compat_die "missing tags file: $tags_arg"
        tags_file=$tags_arg
    else
        tag_work=$(mktemp -d "${TMPDIR:-/tmp}/lu-compat-tags.XXXXXX")
        trap 'rm -rf "$tag_work"' EXIT INT TERM
        tags_file="$tag_work/tags"
        if ! compat_write_remote_tags "$tags_file"; then
            [ -f "$cache_tags" ] || compat_die \
                "cannot list releases from GitHub and no cached tags at $cache_tags"
            echo "compat: release lookup failed; using cached tags" >&2
            tags_file=$cache_tags
        fi
    fi

    selected=$(compat_select_tags "$previous" "$current_version" "$tags_file") || exit 1
    available=$(printf '%s\n' "$selected" | grep -c '.' || true)
    [ "$available" -eq "$previous" ] || compat_die \
        "found $available stable release(s) before $current_version; need $previous (use --previous N)"

    if [ "$dry_run" -eq 1 ]; then
        echo "compat dry run: current $current_version, target $target, previous $previous"
        echo "compat dry run: cache $cache"
        for tag in $selected; do
            if [ -x "$cache/$tag/bin/lu" ] && [ -x "$cache/$tag/bin/ting" ] &&
               [ -f "$cache/$tag/.sha256" ]; then
                state=cached
            else
                state=download
            fi
            printf 'compat dry run: %s (%s)\n' "$tag" "$state"
            printf 'compat dry run:   run older agent %s with new controller and seed\n' "$tag"
            printf 'compat dry run:   run new agent with older controller %s\n' "$tag"
        done
        return 0
    fi

    mkdir -p "$cache"

    current_dir="${CARGO_TARGET_DIR:-$repo_root/target}/debug"
    case "$current_dir" in
        /*) ;;
        *) current_dir="$repo_root/$current_dir" ;;
    esac
    if [ ! -x "$current_dir/lu" ] || [ ! -x "$current_dir/ting" ]; then
        echo "compat: building current binaries in $current_dir"
        (cd "$repo_root" && cargo build --locked --bins)
    fi
    [ -x "$current_dir/lu" ] && [ -x "$current_dir/ting" ] ||
        compat_die "current build did not produce lu and ting in $current_dir"

    for tag in $selected; do
        old_dir=$(compat_prepare_release "$tag" "$target" "$cache")
        run_scenario "$current_dir" "$old_dir" "$old_dir"
        run_scenario "$old_dir" "$current_dir"
    done

    if [ "$tags_file" != "$cache_tags" ]; then
        cp "$tags_file" "$cache_tags"
    fi
    if [ "$previous" -eq 1 ]; then
        noun=release
    else
        noun=releases
    fi
    echo "compat ok: current build with $previous previous $noun"
}

main "$@"
