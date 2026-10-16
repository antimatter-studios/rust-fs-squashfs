#!/usr/bin/env bash
# Tests for scripts/ci-plan.sh: the event-to-profile truth table and the
# jobs ci-ok may accept as skipped under each profile.
#
# Only a pull request may run the quick profile. The merge queue, a push, a
# manual dispatch, a workflow_call and an unknown event must all run the full
# pipeline, and an invalid plan must fail rather than skip anything.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PLAN="$ROOT/scripts/ci-plan.sh"

pass=0; fail=0
expect() {
    local name="$1" want="$2"; shift 2
    local got status
    got="$(bash "$PLAN" "$@" 2>/dev/null)"; status=$?
    if [ "$want" = FAIL ]; then
        if [ "$status" -ne 0 ]; then pass=$((pass + 1)); else fail=$((fail + 1)); echo "  FAIL $name: accepted, printed '$got'"; fi
    elif [ "$status" -eq 0 ] && [ "$got" = "$want" ]; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1)); echo "  FAIL $name: want '$want', got '$got' (status $status)"
    fi
}

expect "pull_request is quick"      quick profile pull_request
expect "merge_group is full"        full  profile merge_group
expect "push is full"               full  profile push
expect "workflow_dispatch is full"  full  profile workflow_dispatch
expect "workflow_call is full"      full  profile workflow_call
expect "an unknown event is full"   full  profile pull_request_target
expect "a tag push is full"         full  profile create
expect "no event fails"             FAIL  profile
expect "an empty event fails"       FAIL  profile ""

expect "full + code skips nothing"  ""    skippable full true
expect "full + docs skips the suites" "test test-arm64 suite-in-vm" skippable full false
expect "quick skips the full legs"  "test test-arm64 suite-in-vm cli" skippable quick true
expect "quick + docs skips the same" "test test-arm64 suite-in-vm cli" skippable quick false
expect "a missing profile fails"    FAIL  skippable "" true
expect "an invalid profile fails"   FAIL  skippable fast true
expect "a missing code answer fails" FAIL skippable full ""
expect "an invalid code answer fails" FAIL skippable full maybe
expect "an unknown subcommand fails" FAIL plan pull_request

printf '  %d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
