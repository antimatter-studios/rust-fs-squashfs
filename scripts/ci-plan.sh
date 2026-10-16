#!/usr/bin/env bash
# Which CI profile an event gets, and which jobs ci-ok may accept as skipped.
#
#   ci-plan.sh profile <event>          prints quick or full
#   ci-plan.sh skippable <profile> <code>
#                                       prints the jobs that may be skipped,
#                                       space-separated (possibly nothing)
#
# A pull request -- every layer of a stack -- runs the quick profile: unit,
# lint, the shell tests and semver. Everything else runs the full pipeline:
# the merge queue, a push to main, a manual dispatch, a workflow_call, and
# any event this script has never heard of. A new trigger therefore costs
# time, never coverage.
#
# Anything else -- an unknown subcommand, a missing argument, a profile or a
# code answer that is not one of the expected words -- exits non-zero, so
# ci-ok fails rather than guess.
set -euo pipefail

FULL_ONLY="test test-arm64 suite-in-vm cli"
CODE_ONLY="test test-arm64 suite-in-vm"

usage() { echo "usage: ci-plan.sh profile <event> | skippable <profile> <code>" >&2; exit 2; }

case "${1:-}" in
  profile)
    [ $# -eq 2 ] && [ -n "$2" ] || usage
    case "$2" in
      pull_request) echo quick ;;
      *) echo full ;;
    esac ;;
  skippable)
    [ $# -eq 3 ] || usage
    case "$2" in quick|full) ;; *) echo "invalid profile: '$2'" >&2; exit 1 ;; esac
    case "$3" in true|false) ;; *) echo "invalid code answer: '$3'" >&2; exit 1 ;; esac
    skip=
    [ "$2" = quick ] && skip="$FULL_ONLY"
    [ "$3" = false ] && [ -z "$skip" ] && skip="$CODE_ONLY"
    echo "$skip" ;;
  *) usage ;;
esac
