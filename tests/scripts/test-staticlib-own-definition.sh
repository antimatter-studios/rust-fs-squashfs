#!/usr/bin/env bash
# Editing staticlib's own cmds must invalidate its fingerprint (#222).
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
command -v chore >/dev/null 2>&1 || { echo "FAIL  chore is not on PATH (scripts/ci-install-chore.sh)" >&2; exit 1; }

work="$(mktemp -d "$REPO/tmp/staticlib-definition.XXXXXX")" || exit 1
trap 'rm -rf "$work"' EXIT
crate="$work/rust-fs-squashfs"
mkdir -p "$crate/src" "$crate/include" "$work/rust-fs-core/include" "$work/bin"
cp "$REPO/chores.yml" "$crate/"
cp "$REPO/include/fs_squashfs.h" "$crate/include/"
: > "$work/rust-fs-core/include/fs_core.h"
cat > "$crate/Cargo.toml" <<'MANIFEST'
[package]
name = "rust-fs-squashfs"
version = "0.0.0"
edition = "2021"

[workspace]
MANIFEST
export STANDIN_CRATE="$crate"

# Keep this a test of the real task definition without downloading a target or
# building the full crate. The stand-ins produce the exact archive metadata and
# path the task expects; every other command in the task runs as written.
cat > "$work/bin/rustup" <<'RUSTUP'
#!/bin/sh
exit 0
RUSTUP
cat > "$work/bin/cargo" <<'CARGO'
#!/bin/sh
case "$1" in
    metadata)
        printf '{"target_directory":"%s/target"}\n' "$STANDIN_CRATE"
        ;;
    build)
        target=""
        while [ "$#" -gt 0 ]; do
            if [ "$1" = --target ]; then shift; target="$1"; fi
            shift
        done
        [ -n "$target" ] || exit 2
        mkdir -p "$STANDIN_CRATE/target/$target/release"
        printf 'stand-in archive\n' > "$STANDIN_CRATE/target/$target/release/libfs_squashfs.a"
        ;;
    *) exit 2 ;;
esac
CARGO
chmod +x "$work/bin/rustup" "$work/bin/cargo"
export PATH="$work/bin:$PATH"
triple="$(rustc -vV | sed -n 's/^host: //p')"
[ -n "$triple" ] || { echo "FAIL  rustc -vV names no host triple" >&2; exit 1; }

run_staticlib() { chore -C "$crate" staticlib "TRIPLE=$triple"; }
if ! output="$(run_staticlib 2>&1)"; then
    printf '%s\n' "$output" >&2
    echo "FAIL  initial staticlib build" >&2
    exit 1
fi
if ! output="$(run_staticlib 2>&1)"; then
    printf '%s\n' "$output" >&2
    echo "FAIL  unchanged staticlib run" >&2
    exit 1
fi
case "$output" in
    *"task: staticlib is up to date"*) ;;
    *) printf '%s\n' "$output" >&2; echo "FAIL  unchanged task was not up to date" >&2; exit 1 ;;
esac

# Add a command to the task definition after the existing final copy.
sed -i.bak '/cp "\.\.\/rust-fs-core\/include\/fs_core\.h"/a\
      - '\''touch "{{.OUT}}/definition-changed"'\''
' "$crate/chores.yml"
rm -f "$crate/chores.yml.bak"
if ! output="$(run_staticlib 2>&1)"; then
    printf '%s\n' "$output" >&2
    echo "FAIL  staticlib after its cmds changed" >&2
    exit 1
fi
case "$output" in
    *"task: staticlib is up to date"*) printf '%s\n' "$output" >&2; echo "FAIL  edit to cmds left staticlib up to date" >&2; exit 1 ;;
esac
[ -f "$crate/dist/definition-changed" ] || { echo "FAIL  added cmds step did not run" >&2; exit 1; }
echo "PASS  staticlib rebuilds after an edit to its own cmds"
