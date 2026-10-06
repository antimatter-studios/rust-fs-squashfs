#!/usr/bin/env bash
# tier.sh LABEL LOG-NAME MAX-LINES MAX-BYTES -- COMMAND [ARG...]
#
# One test tier, run QUIETLY and under a budget. The whole run goes to
# tmp/logs/<LOG-NAME>.log; a pass prints one verdict line naming the log, a
# failure prints the tail of it, and a run that passed but printed more than
# its budget fails with status 65.
#
# WHY THE BUDGET IS PART OF THE TASK and not a CI-only check: the reader who
# pays most for a noisy suite is the one running it locally, and a rule that
# only CI enforces is a rule the tree drifts away from between pull requests.
# See the harness README's "Output: quiet by default, --verbose on request".
#
# The budgets themselves are in chores.yml, next to the command each one
# bounds, and every one of them was MEASURED -- see the table there. Raise one
# deliberately when a tier grows; a budget nobody can breach measures nothing.
#
# VERBOSE. `OUTPUT_BUDGET_VERBOSE=1`, or `--verbose`/`-v` in the chore
# invocation's CLI_ARGS (`chore test:oracle -- --verbose`), streams the run as
# it happens as well as logging it. It does NOT lift the budget: the log is the
# same size either way, and a tier that has outgrown its budget should say so
# whether or not anybody was watching.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# THE WRAPPER BELONGS TO rust-fs-core AND IS DELIBERATELY NOT COMMITTED HERE.
#
# A committed copy is a copy that drifts: measured on 2026-09-22 the family had
# three of them, reached four different ways, each repository internally
# consistent and nothing comparing them (rust-fs-core#153). So it is resolved
# for each run and deleted afterwards.
#
# CARGO IS ASKED WHERE CORE IS, rather than this script guessing. Cargo has
# already resolved the dependency, and its answer is right in both shapes this
# family uses: with `path = "../rust-fs-core"` it reports the developer's own
# checkout, so work in progress on the wrapper is exercised here on the next
# run; with a plain version requirement it reports the registry copy of the
# pinned release. There is no sibling-versus-crate decision to make, because
# cargo made it. A missing or too-old core is FATAL -- never a fallback to a
# copy of our own, which would hide exactly the drift this removes.
#
# tmp/ is gitignored and is where the tier logs already live.
CORE_DIR="$(cargo metadata --format-version 1 --locked --manifest-path "$REPO/Cargo.toml" \
    2>/dev/null | python3 -c '
import json, sys
packages = json.load(sys.stdin)["packages"]
print(next((p["manifest_path"].rsplit("/", 1)[0]
            for p in packages if p["name"] == "rust-fs-core"), ""))
')"
if [ -z "$CORE_DIR" ] || [ ! -f "$CORE_DIR/scripts/output-budget.sh" ]; then
    echo "tier.sh: cargo could not say where rust-fs-core is, or its copy has no" >&2
    echo "         scripts/output-budget.sh. The wrapper lives in rust-fs-core;" >&2
    echo "         check the rust-fs-core dependency resolves and is at a version" >&2
    echo "         that ships it (v0.2.11 or later) -- 'chore siblings' moves the" >&2
    echo "         ../rust-fs-core checkout to the ref chores.yml pins." >&2
    exit 1
fi

BUDGET="$REPO/tmp/output-budget.$$.sh"
mkdir -p "$REPO/tmp"
cp "$CORE_DIR/scripts/output-budget.sh" "$BUDGET"
trap 'rm -f "$BUDGET"' EXIT

[ $# -ge 5 ] || { echo "tier.sh: usage: tier.sh LABEL LOG MAX-LINES MAX-BYTES -- CMD..." >&2; exit 2; }
LABEL="$1"; LOG_NAME="$2"; MAX_LINES="$3"; MAX_BYTES="$4"; shift 4
[ "${1:-}" = "--" ] && shift
[ $# -gt 0 ] || { echo "tier.sh: no command" >&2; exit 2; }

# `chore test:oracle -- --verbose` arrives as CLI_ARGS. output-budget.sh reads
# OUTPUT_BUDGET_VERBOSE itself, so mapping the flag onto it is all that is
# needed -- and it means the environment variable and the flag cannot disagree.
case " ${CLI_ARGS:-} " in
    *" --verbose "*|*" -v "*) export OUTPUT_BUDGET_VERBOSE=1 ;;
esac

# NOT `exec`: the EXIT trap above is what removes the resolved copy, and an
# exec'd process never runs it. The wrapper's status is the tier's status --
# the command's own code, or 65 for a budget it outgrew -- so it is captured
# and re-raised rather than inherited.
status=0
bash "$BUDGET" \
    --log "$REPO/tmp/logs/$LOG_NAME.log" \
    --max-lines "$MAX_LINES" \
    --max-bytes "$MAX_BYTES" \
    --label "$LABEL" \
    -- "$@" || status=$?

# NOTHING SKIPS, AND THE GATE IS HERE SO IT CANNOT BE FORGOTTEN.
#
# A skipped test reads exactly like a passing one. `cargo test` prints its
# ignored count and exits 0, so a tier that stopped running half of itself is
# a green line -- and the executed-test floor cannot see it either, because a
# floor has margin and one more `#[ignore]` is well inside it.
#
# `tests/test_contract.rs` refuses an `#[ignore]` in the SOURCES, which is the
# same rule read at the other end. Both are kept: the source guard names the
# file to edit, and this one catches an ignore that arrives any other way --
# a `--ignored` flag, a cfg-gated attribute, a dependency's own tests.
#
# IN tier.sh RATHER THAN A SCRIPT EACH TASK CALLS, deliberately. A per-tier
# line in chores.yml is a line a new tier can be added without; every tier
# goes through here by construction, including the ones not written yet.
#
# ONLY WHEN THE RUN OTHERWISE PASSED. A command that already failed has a
# better story to tell than its ignored count, and overwriting its status
# would bury it.
LOG="$REPO/tmp/logs/$LOG_NAME.log"
if [ "$status" -eq 0 ] && [ -f "$LOG" ]; then
    # `test result: ok. 37 passed; 0 failed; 2 ignored; ...`, one line per test
    # binary. Anchored at the start of a line so a tool's output that happens
    # to quote the phrase cannot be read as a verdict, and the count is taken
    # from the field BEFORE the word rather than by position, so a future
    # libtest that reorders the summary does not silently read zero.
    ignored="$(awk '
        /^test result:/ {
            for (i = 1; i <= NF; i++) if ($i == "ignored;" || $i == "ignored") sum += $(i - 1)
        }
        END { print sum + 0 }' "$LOG")"
    if [ "$ignored" -gt 0 ]; then
        echo "::error::$ignored test(s) ignored in the $LOG_NAME tier; a skipped test is not a passing one"
        echo "tier.sh: the $LOG_NAME tier reported $ignored ignored test(s)." >&2
        echo "         A test that cannot run FAILS, naming the task that would" >&2
        echo "         provide what it needed. A suite that quietly declines to" >&2
        echo "         run is indistinguishable from one that passes." >&2
        echo "         The log is $LOG." >&2
        exit 66
    fi
fi
exit "$status"
