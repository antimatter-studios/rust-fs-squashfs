#!/usr/bin/env bash
# `chore lint` runs rust-fs-core's ci-gate outside GitHub Actions, where the
# workflow's top-level env is never exported. The profile-conditional jobs
# (`test`, `test-arm64`, `suite-in-vm`, `cli`) are declared non-gating only
# there, in ci.yml's `CI_GATE_NON_GATING`. A checker that reads that
# declaration from the process environment alone passes in CI and fails on
# every developer's machine (#184).
#
# This runs the checker exactly as `chore lint` does, with the variable
# REMOVED from the environment, because the CI job that runs this file
# inherits it from the workflow and would otherwise prove nothing. Then it
# proves the checker still refuses: a job dropped from the declaration, a new
# conditional job nobody declared, and an explicit empty override. A job
# gated only on changes.outputs.code is allowed without a declaration; keep
# that control separate from the undeclared profile-conditional job (#209).
#
#   bash tests/scripts/ci-gate-local.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GATE="$REPO/../rust-fs-core/scripts/ci-gate.sh"
WF=.github/workflows/ci.yml

fails=0
fail() { fails=$((fails + 1)); echo "FAIL  $*" >&2; }

[ -f "$GATE" ] || { echo "FAIL  $GATE is missing: run \`chore siblings\`" >&2; exit 1; }

# Runs the checker on the tree at $1 with no inherited declaration; any
# further arguments are env assignments. Prints its output; returns its status.
gate() {
    local root="$1"; shift
    env -u CI_GATE_NON_GATING -u CI_GATE_WORKFLOW -u CI_GATE_AGGREGATE \
        -u CI_GATE_GUARD -u CI_GATE_CHANGES \
        FS_CORE_CALLER="$root" "$@" bash "$GATE" 2>&1
}

SANDBOX="$(mktemp -d)"
trap 'rm -rf "$SANDBOX"' EXIT
mkdir -p "$SANDBOX/.github/workflows"
cp "$REPO/.github-guard" "$SANDBOX/.github-guard"
reset() { cp "$REPO/$WF" "$SANDBOX/$WF"; }

# The real workflow, read the way `chore lint` reads it.
out="$(gate "$REPO")" || fail "the unchanged workflow fails without CI's env: $out"

# The declaration the checker used is the workflow's: drop one job from it
# and that job is reported.
reset
sed -i.t 's/^\(  CI_GATE_NON_GATING: "[^"]*\) cli"/\1"/' "$SANDBOX/$WF" && rm -f "$SANDBOX/$WF.t"
if cmp -s "$SANDBOX/$WF" "$REPO/$WF"; then
    fail "the declaration was not edited, so the next case proves nothing"
fi
if out="$(gate "$SANDBOX")"; then
    fail "a conditional job dropped from the declaration passes"
elif ! grep -q "\`cli\`" <<<"$out"; then
    fail "dropping \`cli\` from the declaration failed without naming it: $out"
fi

# A job gated only on the code-change output is explicitly allowed by the
# pinned checker, provided ci-ok reads that output. It is a positive control,
# not evidence that an undeclared profile-conditional job must be refused.
reset
sed -i.t 's/^\(    needs: \[changes,\) unit,/\1 extra, unit,/' "$SANDBOX/$WF" && rm -f "$SANDBOX/$WF.t"
cat >>"$SANDBOX/$WF" <<'YAML'
  extra:
    needs: changes
    if: needs.changes.outputs.code == 'true'
    runs-on: ubuntu-latest
    steps:
      - run: "true"
YAML
grep -q 'needs: \[changes, extra,' "$SANDBOX/$WF" ||
    fail "\`extra\` was not added to ci-ok's needs, so the next case proves nothing"
out="$(gate "$SANDBOX")" || fail "an allowed path-gated job fails: $out"

# Keep the undeclared-job refusal. A profile condition is not the checker's
# exact code-change exception, so this job requires an explicit declaration.
sed -i.t "s/^    if: needs.changes.outputs.code == 'true'$/    if: needs.changes.outputs.profile == 'full'/" "$SANDBOX/$WF" && rm -f "$SANDBOX/$WF.t"
if grep -q "^    if: needs.changes.outputs.code == 'true'$" "$SANDBOX/$WF"; then
    fail "\`extra\` was not made profile-conditional, so the next case proves nothing"
fi
if out="$(gate "$SANDBOX")"; then
    fail "an undeclared conditional job passes"
elif ! grep -q "\`extra\`" <<<"$out"; then
    fail "the undeclared conditional job failed without naming it: $out"
elif ! grep -q 'without being declared non-gating' <<<"$out"; then
    fail "the undeclared conditional job failed for another reason: $out"
fi

# The process environment still wins, including empty: no exemptions at all.
if out="$(gate "$REPO" CI_GATE_NON_GATING=)"; then
    fail "an explicit empty CI_GATE_NON_GATING passes"
fi

[ "$fails" -eq 0 ] || exit 1
echo "ci-gate-local: all checks passed"
