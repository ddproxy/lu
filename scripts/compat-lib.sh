#!/bin/sh
# Pure helpers for scripts/compat-test.sh and scripts/test-compat.sh.
# These functions need no network and never run a release binary.

compat_error() {
    printf 'compat: %s\n' "$*" >&2
}

compat_die() {
    compat_error "$*"
    exit 1
}

# The stable version comes from the workspace, not from a git tag. Tests can
# override it with COMPAT_CURRENT_VERSION.
compat_current_version() {
    if [ -n "${COMPAT_CURRENT_VERSION:-}" ]; then
        printf '%s\n' "$COMPAT_CURRENT_VERSION"
        return 0
    fi
    root=${1:-.}
    [ -f "$root/Cargo.toml" ] || { compat_error "missing $root/Cargo.toml"; return 1; }
    version=$(sed -n '/^\[workspace\.package\]/,/^\[/{
        s/^version[[:space:]]*=[[:space:]]*"\([0-9][0-9.]*\)".*/\1/p
    }' "$root/Cargo.toml" | head -n 1)
    [ -n "$version" ] || { compat_error "no stable workspace version in $root/Cargo.toml"; return 1; }
    printf '%s\n' "$version"
}

# Releases publish archives for these two musl targets only.
compat_check_target() {
    case "${1:-}" in
        x86_64-unknown-linux-musl|aarch64-unknown-linux-musl) return 0 ;;
        *) compat_error "unsupported release target: ${1:-<empty>}"; return 1 ;;
    esac
}

compat_host_target() {
    case "$(uname -s):$(uname -m)" in
        Linux:x86_64|Linux:amd64) printf '%s\n' x86_64-unknown-linux-musl ;;
        Linux:aarch64|Linux:arm64) printf '%s\n' aarch64-unknown-linux-musl ;;
        *) compat_error "unsupported host $(uname -s)/$(uname -m); use --target"; return 1 ;;
    esac
}

# Read tags (one per line) on stdin and print the latest N stable tags strictly
# before the current version, newest first. Sorting is numeric by release
# component, so v0.10.0 sorts after v0.9.9.
compat_select_tags() {
    [ "$#" -eq 3 ] || { compat_error "usage: compat_select_tags <count> <current> <tags-file>"; return 1; }
    count=$1
    current=$2
    file=$3
    case "$count" in
        ''|*[!0-9]*) compat_error "invalid release count: $count"; return 1 ;;
    esac
    [ "$count" -gt 0 ] || { compat_error "release count must be positive: $count"; return 1; }
    case "$current" in
        ''|*[!0-9.]*) compat_error "invalid current version: $current"; return 1 ;;
    esac
    major=${current%%.*}
    rest=${current#*.}
    minor=${rest%%.*}
    patch=${rest#*.}
    for part in "$major" "$minor" "$patch"; do
        case "$part" in
            ''|*[!0-9]*) compat_error "invalid current version: $current"; return 1 ;;
        esac
    done
    [ -f "$file" ] || { compat_error "missing tags file: $file"; return 1; }

    awk -v cm="$major" -v cn="$minor" -v cp="$patch" '
        /^v[0-9]+\.[0-9]+\.[0-9]+$/ {
            tag = $0
            split(substr(tag, 2), p, ".")
            if (p[1]+0 < cm || (p[1]+0 == cm && p[2]+0 < cn) ||
                (p[1]+0 == cm && p[2]+0 == cn && p[3]+0 < cp))
                printf "%d %d %d %s\n", p[1]+0, p[2]+0, p[3]+0, tag
        }' "$file" |
        sort -u -k1,1nr -k2,2nr -k3,3nr |
        head -n "$count" |
        awk '{ print $4 }'
}

# Verify a release archive against its .sha256 file without trusting the file's
# path or running what it names. Print the verified digest on success.
compat_checksum_verify() {
    [ "$#" -eq 2 ] || { compat_error "usage: compat_checksum_verify <archive> <sumfile>"; return 1; }
    archive=$1
    sumfile=$2
    [ -f "$archive" ] || { compat_error "missing archive: $archive"; return 1; }
    [ -f "$sumfile" ] || { compat_error "missing checksum file: $sumfile"; return 1; }
    expected=${archive##*/}

    line=$(awk 'NF { if (++n == 1) print; } END { if (n != 1) exit 1; }' "$sumfile") || {
        compat_error "checksum file must contain exactly one entry: $sumfile"
        return 1
    }
    set -- $line
    [ "$#" -eq 2 ] || { compat_error "unexpected checksum format: $sumfile"; return 1; }
    want=$1
    name=$2
    case "$want" in
        ''|*[!0123456789abcdefABCDEF]*) compat_error "invalid checksum in $sumfile"; return 1 ;;
    esac
    [ "${#want}" -eq 64 ] || { compat_error "invalid checksum length in $sumfile"; return 1; }
    [ "$name" = "$expected" ] || {
        compat_error "checksum file names $name, expected $expected"
        return 1
    }

    actual=$(sha256sum "$archive") || { compat_error "cannot hash $archive"; return 1; }
    set -- $actual
    [ "$1" = "$want" ] || {
        compat_error "checksum mismatch for $expected: refusing to use the archive"
        return 1
    }
    printf '%s\n' "$want"
}
