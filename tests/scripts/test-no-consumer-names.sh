#!/usr/bin/env bash
# No tracked file names an application built on top of this crate.
#
# This crate is a standalone project (AGENTS.md: "Never mention a consuming
# application in the README, the source, or CLI help"). A name slips in the
# easy way -- a chores.yml header recalling where the build used to live, a
# test comment saying which product ships on which architecture -- and
# nothing else here would notice (#126). So this reads EVERY tracked file:
# source, tests, scripts, docs, workflows, the changelog. Comments included:
# a name in a comment is still a name in the repository.
#
# THE NAME LIST LIVES HERE AND NOWHERE ELSE. Each entry is a
# case-insensitive extended regex, so one entry covers the spellings of one
# name. This file is the one place allowed to spell them, and the only file
# the scan does not read.
#
#   bash tests/scripts/test-no-consumer-names.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SELF="$(basename "${BASH_SOURCE[0]}")"

NAMES=(
    'disk[-_ ]?jockey'
    'in[-_]?pace'
)

PATTERN="$(IFS='|'; echo "${NAMES[*]}")"

pass=0; fail=0
ok()  { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); printf '  FAIL  %s\n' "$*"; }

# Every tracked line under <root> (a git work tree) that names one of them.
scan() {
    git -C "$1" grep -nIiE -e "$PATTERN" -- . ":(exclude)tests/scripts/$SELF" || true
}

mkdir -p "$REPO/tmp"
SANDBOX="$(mktemp -d "$REPO/tmp/no-consumer-names.XXXXXX")"
trap 'rm -rf "$SANDBOX"' EXIT HUP INT TERM

printf 'no-consumer-names\n'

# --- 1. The scan recognises every name it refuses. -----------------------
#
# Without this a pattern that matched nothing -- a typo, a grep that reads
# the flags differently -- would pass the real tree having checked nothing.
git -C "$SANDBOX" init -q
mkdir -p "$SANDBOX/tests/scripts" "$SANDBOX/src" "$SANDBOX/docs"
cat > "$SANDBOX/src/lib.rs" <<'EOT'
//! Written for the DiskJockey app.
fn main() {}
EOT
cat > "$SANDBOX/docs/notes.md" <<'EOT'
Seen on an inpace.service unit.
disk-jockey and Disk Jockey and INPACE_HOME too.
Nothing to see on this line: a disk, a jockey, in place.
EOT
# An untracked file is not the repository's, and this file's own name is
# the one allowed mention.
printf 'DiskJockey\n' > "$SANDBOX/untracked.txt"
printf 'DiskJockey\n' > "$SANDBOX/tests/scripts/$SELF"
git -C "$SANDBOX" add src/lib.rs docs/notes.md "tests/scripts/$SELF"

found="$(scan "$SANDBOX")"
expect=(
    "src/lib.rs:1:"
    "docs/notes.md:1:"
    "docs/notes.md:2:"
)
for e in "${expect[@]}"; do
    if grep -qF "$e" <<<"$found"; then ok; else
        bad "the scan missed $e:"$'\n'"$found"
    fi
done
count="$(grep -c . <<<"$found")"
if [[ "$count" -eq ${#expect[@]} ]]; then ok; else
    bad "the scan found $count lines, expected ${#expect[@]}:"$'\n'"$found"
fi

# --- 2. The repository names none of them. --------------------------------
found="$(scan "$REPO")"
if [[ -z "$found" ]]; then ok; else
    bad "these name an application built on this crate; describe the scenario instead:"$'\n'"$found"
fi

printf '  %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
