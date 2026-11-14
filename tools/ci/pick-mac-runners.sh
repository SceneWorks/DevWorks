#!/usr/bin/env bash
# Assign each macOS CI job to an idle self-hosted Mac or to GitHub-hosted macos-latest.
#
# Counts online, idle self-hosted runners carrying MAC_RUNNER_LABEL in the owner's
# organization and in the repository, then gives that many jobs (longest first) to
# the self-hosted pool. Every other job, every fork PR, and every run where the
# runners cannot be listed uses macos-latest. Only put MAC_RUNNER_LABEL on runners
# this repository may use.
#
# The fork check is routing, not isolation: a pull_request run uses the PR's own copy of
# this script and of ci.yml, so a fork can rewrite either and target the self-hosted Macs.
# Keeping fork code off those Macs depends on the repository setting that requires
# maintainer approval before workflows run for all outside contributors.
#
# Inputs (environment): GH_TOKEN (may be empty), OWNER, REPO (owner/name),
# IS_FORK (true/false), MAC_RUNNER_LABEL (default devworks-mac).
# Output: one line per job, job=<runs-on JSON>, appended to GITHUB_OUTPUT (stdout locally).
set -uo pipefail

label=${MAC_RUNNER_LABEL:-devworks-mac}
jobs=(foundation engine desktop clippy) # longest first
hosted='"macos-latest"'
self_hosted="[\"self-hosted\",\"$label\"]"
output=${GITHUB_OUTPUT:-/dev/stdout}

idle=0
if [[ "${IS_FORK:-false}" == true ]]; then
  echo "Fork pull request: every macOS job uses macos-latest."
elif [[ -z "${GH_TOKEN:-}" ]]; then
  echo "::warning::MAC_RUNNER_QUERY_TOKEN is not set; every macOS job uses macos-latest."
else
  filter="[.runners[] | select(.status == \"online\" and (.busy | not) and any(.labels[]; .name == \"$label\"))] | length"
  for scope in "orgs/${OWNER:?}" "repos/${REPO:?}"; do
    if counts=$(gh api --paginate "$scope/actions/runners" --jq "$filter" 2>&1); then
      idle=$((idle + $(awk '{ s += $1 } END { print s + 0 }' <<<"$counts")))
    else
      echo "::warning::Could not list runners for $scope: $counts"
    fi
  done
  echo "Idle self-hosted runners labelled $label: $idle"
fi

for i in "${!jobs[@]}"; do
  if ((i < idle)); then runner=$self_hosted; else runner=$hosted; fi
  echo "${jobs[$i]} -> $runner"
  echo "${jobs[$i]}=$runner" >>"$output"
done
