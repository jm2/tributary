#!/usr/bin/env bash
#
# Read-only rollout/preflight validator for the Tributary Bot Review Gate.
#
# This script only READS repository configuration through `gh api` (or, in
# `--offline` mode, reads recorded API documents from a directory). It never
# mutates settings, environments, secrets, variables, or rulesets, and it never
# reads a secret value: the manifest names credentials, it does not carry them.
#
# It validates three phases:
#   * STAGED INACTIVE — the default-off activation flag is unset or
#     `inactive`. The publisher publishes nothing, so the App-bound context
#     must NOT yet be required by the live `main` ruleset (a required context
#     that is never published would block every merge). The App/environment/
#     secret prerequisites are reported as future activation work.
#   * ACTIVE — the flag is `active`. The protected environment, its secret
#     NAMES, the numeric App-id variable, every required check binding, and
#     require-conversation-resolution must all be present, or the preflight
#     fails closed.
#   * Any other value is INCOMPATIBLE and fails closed.
#
# A prior green head, a token-mint failure, and an unreported/missed refresh
# are never merge authority: only a fresh App-authored verdict bound to the
# evaluated head is. This validator reports prerequisites; it grants nothing.
#
# Usage:
#   scripts/preflight_bot_review_gate.sh [--repo OWNER/REPO]
#                                        [--manifest PATH]
#                                        [--offline DIR]
#
# --offline DIR reads these files if present (missing = absent/empty):
#   activation.json            repos/<repo>/actions/variables/BOT_REVIEW_GATE_ACTIVATION
#   app-id-variable.json       repos/<repo>/actions/variables/<gate app id variable>
#   environment.json           repos/<repo>/environments/<environment>
#   environment-secrets.json   repos/<repo>/environments/<environment>/secrets
#   dependabot-secrets.json    repos/<repo>/dependabot/secrets
#   branch-rules.json          repos/<repo>/rules/branches/main
#   rulesets/<id>.json         repos/<repo>/rulesets/<id>

set -euo pipefail

repo="${GITHUB_REPOSITORY:-jm2/tributary}"
manifest=".github/bot-review-gate-rollout.json"
offline=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --repo) repo="$2"; shift 2 ;;
    --manifest) manifest="$2"; shift 2 ;;
    --offline) offline="$2"; shift 2 ;;
    -h|--help)
      printf 'usage: %s [--repo OWNER/REPO] [--manifest PATH] [--offline DIR]\n' "$0"
      exit 0
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

if ! command -v jq >/dev/null 2>&1; then
  printf 'jq is required for the preflight validator\n' >&2
  exit 2
fi
if [ ! -f "$manifest" ]; then
  printf 'manifest not found: %s\n' "$manifest" >&2
  exit 2
fi

env_name="$(jq -r '.environment.name' "$manifest")"
activation_variable="$(jq -r '.activation.variable' "$manifest")"
gate_context="$(jq -r '.required_status_checks[] | select((.integration // "") | tostring | startswith("variable:")) | .context' "$manifest")"
gate_variable="$(jq -r '.required_status_checks[] | select((.integration // "") | tostring | startswith("variable:")) | .integration | sub("^variable:"; "")' "$manifest")"
resolution_required="$(jq -r '.require_conversation_resolution' "$manifest")"

failures=0
note() { printf '%s\n' "$*"; }
missing() { printf 'MISSING: %s\n' "$*"; failures=$((failures + 1)); }
incompatible() { printf 'INCOMPATIBLE: %s\n' "$*"; failures=$((failures + 1)); }

# ── Gather the live (or recorded) configuration ─────────────────────────────
if [ -n "$offline" ]; then
  if [ ! -d "$offline" ]; then
    printf 'offline fixture directory not found: %s\n' "$offline" >&2
    exit 2
  fi
  src="$offline"
else
  src="$(mktemp -d "${TMPDIR:-/var/tmp}/bot-review-gate-preflight.XXXXXX")"
  trap 'rm -rf "$src"' EXIT

  # A read failure that is not a clean 404 is recorded as a sentinel so the
  # validator fails closed instead of reading a network failure as "absent".
  gather() {
    out="$1"; shift
    err="$src/.gh.err"
    if ! gh api "$@" > "$out" 2>"$err"; then
      if grep -Eq 'HTTP 404|Not Found' "$err"; then
        rm -f "$out"
      else
        printf 'READ-FAILED: gh api %s: %s\n' "$*" "$(sed -n '1p' "$err")" >&2
        rm -f "$out"
        : > "$out.failed"
      fi
    fi
  }

  gather "$src/activation.json" "repos/$repo/actions/variables/BOT_REVIEW_GATE_ACTIVATION"
  gather "$src/app-id-variable.json" "repos/$repo/actions/variables/$gate_variable"
  gather "$src/environment.json" "repos/$repo/environments/$env_name"
  gather "$src/environment-secrets.json" "repos/$repo/environments/$env_name/secrets"
  gather "$src/dependabot-secrets.json" "repos/$repo/dependabot/secrets"
  gather "$src/branch-rules.json" "repos/$repo/rules/branches/main"
  mkdir -p "$src/rulesets"
  if [ -f "$src/branch-rules.json" ]; then
    for id in $(jq -r '.[].ruleset_id' "$src/branch-rules.json" 2>/dev/null | sort -u); do
      [ -n "$id" ] || continue
      gather "$src/rulesets/$id.json" "repos/$repo/rulesets/$id"
    done
  fi
  for sentinel in "$src"/*.failed; do
    [ -e "$sentinel" ] || continue
    printf 'READ-FAILED: one or more GitHub API reads failed; the live configuration cannot be validated.\n' >&2
    failures=$((failures + 1))
  done
fi

value_of() {
  # Prints the `.value` of a JSON document file, or nothing if the file is
  # absent or has no value.
  if [ -f "$1" ]; then
    jq -r '.value // empty' "$1" 2>/dev/null || true
  fi
}

names_of() {
  # Prints the `.secrets[].name` names of a secrets listing file, or nothing.
  if [ -f "$1" ]; then
    jq -r '.secrets[]?.name' "$1" 2>/dev/null || true
  fi
}

# ── Live main required contexts, as "context|integration_id" ────────────────
live_contexts="$src/live-contexts.txt"
: > "$live_contexts"
for detail in "$src"/rulesets/*.json; do
  [ -e "$detail" ] || continue
  jq -r '.rules[]?
    | select(.type == "required_status_checks")
    | .parameters.required_status_checks[]
    | "\(.context)|\(.integration_id // "")"' "$detail" >> "$live_contexts" 2>/dev/null || true
done
sort -u -o "$live_contexts" "$live_contexts"

resolution_enforced=0
for detail in "$src"/rulesets/*.json; do
  [ -e "$detail" ] || continue
  enforcement="$(jq -r '
    [.rules[]?
      | select(.type == "pull_request")
      | .parameters.required_review_thread_resolution]
    | if length == 0 then "absent"
      elif all then "enforced"
      else "off" end' "$detail" 2>/dev/null || printf 'absent')"
  [ "$enforcement" = "enforced" ] && resolution_enforced=1
done

activation="$(value_of "$src/activation.json")"

# ── Phase validation ────────────────────────────────────────────────────────
case "${activation:-}" in
  ''|inactive)
    note "Phase: STAGED INACTIVE (${activation_variable}=${activation:-<unset>})"
    note "The publisher publishes no App-authored verdict and claims no merge authority while inactive."
    note "Activation prerequisites (not required while inactive): environment '$env_name' restricted to main, its App secret names, and a numeric repository variable $gate_variable."
    note "Offline/read-only diagnostic secrets for the Dependabot readiness check: $(jq -r '[.apps[] | select(.credential_store == "dependabot") | .secret_names[]] | join(", ")' "$manifest")."
    gate_live="$(grep -F "${gate_context}|" "$live_contexts" | sed -n '1p' || true)"
    if [ -n "$gate_live" ]; then
      incompatible "the live main ruleset requires '${gate_live}' while the publisher is staged inactive; the App context is never published, so every main merge is blocked. Activate publication or remove the requirement."
    else
      note "Consistent: the live main ruleset does not require the App-bound '${gate_context}' context yet."
    fi
    ;;
  active)
    note "Phase: ACTIVE (${activation_variable}=active)"
    app_id="$(value_of "$src/app-id-variable.json")"
    case "${app_id:-}" in
      ''|*[!0-9]*)
        missing "repository variable $gate_variable is not set to a numeric App id (found: '${app_id:-<unset>}')."
        ;;
    esac

    if [ -f "$src/environment.json" ]; then
      protected="$(jq -r '.deployment_branch_policy.protected_branches // false' "$src/environment.json")"
      custom="$(jq -r '.deployment_branch_policy.custom_branch_policies // false' "$src/environment.json")"
      if [ "$protected" != "true" ]; then
        incompatible "environment '$env_name' does not restrict deployments to protected branches; the App key could be reachable from a non-main deployment."
      fi
      if [ "$custom" = "true" ]; then
        incompatible "environment '$env_name' has custom branch policies enabled; main-only scope is ambiguous."
      fi
    else
      missing "protected environment '$env_name' does not exist (or could not be read); the publisher cannot mint its App token."
    fi

    env_secret_names="$(names_of "$src/environment-secrets.json")"
    while IFS= read -r secret_name; do
      [ -n "$secret_name" ] || continue
      if ! printf '%s\n' "$env_secret_names" | grep -Fxq "$secret_name"; then
        missing "environment secret '$secret_name' is not present on environment '$env_name' (name only; no value is read)."
      fi
    done < <(jq -r '.apps[] | select(.credential_store | startswith("environment:")) | .secret_names[]' "$manifest")

    dep_secret_names="$(names_of "$src/dependabot-secrets.json")"
    while IFS= read -r secret_name; do
      [ -n "$secret_name" ] || continue
      if ! printf '%s\n' "$dep_secret_names" | grep -Fxq "$secret_name"; then
        missing "Dependabot secret '$secret_name' is not present (name only; no value is read)."
      fi
    done < <(jq -r '.apps[] | select(.credential_store == "dependabot") | .secret_names[]' "$manifest")

    while IFS= read -r index; do
      [ -n "$index" ] || continue
      context="$(jq -r ".required_status_checks[$index].context" "$manifest")"
      integration="$(jq -r ".required_status_checks[$index].integration // \"\"" "$manifest")"
      case "$integration" in
        variable:*) expected="${app_id:-}" ;;
        *) expected="$integration" ;;
      esac
      if ! grep -Fxq "${context}|${expected}" "$live_contexts"; then
        missing "live main ruleset does not require context '$context' with integration '${expected:-unbound}'."
      fi
    done < <(jq -r '.required_status_checks | keys[]' "$manifest")

    if [ "$resolution_required" = "true" ] && [ "$resolution_enforced" -ne 1 ]; then
      missing "no applicable main ruleset enforces require-conversation-resolution."
    fi
    ;;
  *)
    incompatible "${activation_variable} has unrecognized value '${activation}'; it must be unset, 'inactive', or 'active'."
    ;;
esac

# ── Result ──────────────────────────────────────────────────────────────────
if [ "$failures" -ne 0 ]; then
  note ""
  note "PREFLIGHT RESULT: $failures prerequisite(s) missing or incompatible."
  note "A prior green head, a token-mint failure, and an unreported or missed refresh are never merge authority; only a fresh App-authored verdict bound to the evaluated head is."
  exit 1
fi
note ""
note "PREFLIGHT RESULT: configuration is consistent."
note "This validator only reads; it verifies no credentials and mutates no settings."
exit 0
