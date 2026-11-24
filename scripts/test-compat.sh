#!/bin/sh
# Offline tests for compat version selection and checksum verification.
# No release binary is run and no network call is made here.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$here/compat-lib.sh"

work=$(mktemp -d "${TMPDIR:-/tmp}/lu-compat-test.XXXXXX")
trap 'rm -rf "$work"' EXIT INT TERM

check_eq() {
    [ "$1" = "$2" ] || { printf 'FAIL: [%s] != [%s]\n' "$1" "$2" >&2; exit 1; }
}

expect_fail() {
    if "$@"; then
        printf 'FAIL: expected failure from %s\n' "$*" >&2
        exit 1
    fi
}

mkdir -p "$work/repo"
cat > "$work/repo/Cargo.toml" <<'EOF'
[workspace.package]
version = "1.2.0"
edition = "2021"

[workspace.dependencies]
serde = { version = "1" }
EOF
check_eq "$(compat_current_version "$work/repo")" "1.2.0"

target=$(compat_host_target)
compat_check_target "$target"
expect_fail compat_check_target "unknown-unknown"

cat > "$work/tags" <<'EOF'
v1.1.0
v1.2.0
v0.99.0
v0.6.2-dev.1
v1.2.1
v1.1.5
v1.0.9
v1.0.9
not-a-tag
EOF
selected=$(compat_select_tags 2 1.2.0 "$work/tags")
check_eq "$selected" "v1.1.5
v1.1.0"

cat > "$work/order-tags" <<'EOF'
v1.2.0
v1.9.9
v1.10.0
v1.10.1
v1.9.10
EOF
selected=$(compat_select_tags 2 1.10.0 "$work/order-tags")
check_eq "$selected" "v1.9.10
v1.9.9"

printf 'v0.6.0\n' > "$work/few-tags"
set -- $(compat_select_tags 4 1.2.0 "$work/few-tags")
[ "$#" -eq 1 ] || { printf 'FAIL: expected one tag\n' >&2; exit 1; }

printf 'archive contents for a checksum test\n' > "$work/archive.bin"
sum=$(sha256sum "$work/archive.bin" | cut -d' ' -f1)
printf '%s  archive.bin\n' "$sum" > "$work/archive.bin.sha256"
check_eq "$(compat_checksum_verify "$work/archive.bin" "$work/archive.bin.sha256")" "$sum"

printf 'different archive contents\n' > "$work/archive.bin"
expect_fail compat_checksum_verify "$work/archive.bin" "$work/archive.bin.sha256"
printf 'archive contents for a checksum test\n' > "$work/archive.bin"

printf '%s  other.bin\n' "$sum" > "$work/wrong-name.sha256"
expect_fail compat_checksum_verify "$work/archive.bin" "$work/wrong-name.sha256"
printf 'not a checksum\n' > "$work/malformed.sha256"
expect_fail compat_checksum_verify "$work/archive.bin" "$work/malformed.sha256"
printf '%s  archive.bin\n%s  archive.bin\n' "$sum" "$sum" > "$work/two.sha256"
expect_fail compat_checksum_verify "$work/archive.bin" "$work/two.sha256"

printf 'v0.6.0\nv0.5.9\n' > "$work/repo-tags"
out=$(COMPAT_CURRENT_VERSION=0.6.1 "$here/compat-test.sh" --dry-run \
    --tags-file "$work/repo-tags" --previous 2 --target "$target")
printf '%s\n' "$out" | grep -q 'compat dry run: v0.6.0' ||
    { printf 'FAIL: dry run did not select v0.6.0\n' >&2; exit 1; }
printf '%s\n' "$out" | grep -q 'compat dry run: v0.5.9' ||
    { printf 'FAIL: dry run did not select v0.5.9\n' >&2; exit 1; }

printf 'v0.6.0\n' > "$work/one-tag"
if COMPAT_CURRENT_VERSION=0.6.1 "$here/compat-test.sh" --dry-run \
    --tags-file "$work/one-tag" --previous 2 --target "$target" > "$work/out" 2>&1; then
    printf 'FAIL: expected an error for too few releases\n' >&2
    exit 1
fi
grep -q 'need 2' "$work/out" ||
    { printf 'FAIL: unclear error for too few releases\n' >&2; exit 1; }

check_eq "$("$here/compat-test.sh" --skip-download)" \
    "compat: skipped (--skip-download)"

echo "compat shell tests passed"
