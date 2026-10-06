#!/usr/bin/env bash
# Tests for the ignored-test gate this repository asks rust-fs-core's
# tier.sh for (`--refuse-ignored`, run in place): a tier whose log reports an
# ignored test must FAIL, and one that reports none must not.
#
# A gate that cannot fail is indistinguishable from no gate, and this one
# guards the quietest failure there is: `cargo test` prints its ignored
# count and exits 0, so a tier that stopped running half of itself is a
# green line. Measured before the gate existed: a run reporting two
# ignored tests exited 0.
#
# Note how `check` captures `$?` from a plain command and never from a
# pipe: `$?` after a pipe is the status of the pipe's LAST command, so
# `... | grep -q` would report every failure as a pass.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT" || exit 1

pass=0; fail=0
LOGS="$ROOT/tmp/logs"
mkdir -p "$LOGS"
trap 'rm -f "$LOGS/skipgate.log"' EXIT

# One tier run whose command prints $1. Budgets generous: this is about
# the skip gate, not the output budget.
run_tier() {
    bash ../rust-fs-core/scripts/tier.sh --refuse-ignored "skip-gate self test" skipgate 100 9000 -- \
        bash -c "printf '%s\n' \"\$@\"" _ "$@" >/dev/null 2>&1
}

check() {
    local name="$1" want="$2"; shift 2
    run_tier "$@"
    local got=$?
    local ok
    case "$want" in
        zero) [ "$got" -eq 0 ] && ok=y ;;
        *)    [ "$got" -ne 0 ] && ok=y ;;
    esac
    if [ "${ok:-}" = y ]; then pass=$((pass + 1))
    else fail=$((fail + 1)); printf '  FAIL  %s (exit %s, wanted %s)\n' "$name" "$got" "$want"; fi
    unset ok
}

printf 'tier-skip-gate\n'

OK='test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out'
IGN='test result: ok. 5 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out'
ONE='test result: ok. 5 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out'

check "a tier that ignored nothing passes"            zero    "$OK"
check "a tier that ignored two tests fails"           nonzero "$IGN"
check "one ignored test is enough to fail"            nonzero "$ONE"
# The count is summed across test binaries, so an ignore in the second of
# them is caught as surely as one in the first.
check "an ignore in a later binary is caught"         nonzero "$OK" "$IGN"
# THE PHRASE IS ONLY A VERDICT AT THE START OF A LINE. A test that prints
# the summary of something else -- this suite's own fixtures do -- must
# not be read as one.
check "the phrase quoted mid-line is not a verdict"   zero \
    "  the oracle said: test result: ok. 1 passed; 0 failed; 9 ignored"
# AND A REAL FAILURE KEEPS ITS OWN STATUS. The command's story is better
# than its ignored count, so the gate must not overwrite a failure -- it
# only refuses a pass. Run directly: `check` cannot express a command
# that prints and then fails.
bash ../rust-fs-core/scripts/tier.sh --refuse-ignored "skip-gate self test" skipgate 100 9000 -- \
    bash -c "printf '%s\n' '$IGN'; exit 3" >/dev/null 2>&1
got=$?
if [ "$got" -eq 3 ]; then pass=$((pass + 1))
else fail=$((fail + 1))
     printf '  FAIL  %s (exit %s, wanted 3)\n' "a failing command keeps its own status" "$got"
fi

printf '  %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
