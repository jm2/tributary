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
# Every read is complete and structural: list endpoints are paginated to
# exhaustion, every page must parse to the expected JSON shape, and a required
# read that fails — a non-404 error, a malformed document, a referenced
# ruleset that cannot be read, or an inventory that terminates short of its
# declared total — fails the validation closed. An unreadable observation is
# never treated as proof that a prerequisite is absent or unset.
#
# The environment policy is validated in full: the deployment branch policy
# must select custom branch policies naming exactly `main` (no wildcards, no
# other branches, no tag policies) rather than the `protected_branches` flag,
# which authorizes every protected branch, and the complete `protection_rules`
# list must contain no required-reviewer or wait-timer gate that the unattended
# publisher cannot satisfy.
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
#   activation.json                    repos/<repo>/actions/variables/BOT_REVIEW_GATE_ACTIVATION
#   app-id-variable.json               repos/<repo>/actions/variables/<gate app id variable>
#   environment.json                   repos/<repo>/environments/<environment>
#   environment-secrets.json           repos/<repo>/environments/<environment>/secrets
#   dependabot-secrets.json            repos/<repo>/dependabot/secrets
#   branch-rules.json                  repos/<repo>/rules/branches/main
#   deployment-branch-policies.json    repos/<repo>/environments/<environment>/deployment-branch-policies
#   rulesets/<id>.json                 repos/<repo>/rulesets/<id>

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

scratch="$(mktemp -d "${TMPDIR:-/var/tmp}/bot-review-gate-preflight.XXXXXX")"
live_src=""
cleanup() {
  rm -rf "$scratch"
  if [ -n "$live_src" ]; then
    rm -rf "$live_src"
  fi
}
trap cleanup EXIT

failures=0
note() { printf '%s\n' "$*"; }
missing() { printf 'MISSING: %s\n' "$*"; failures=$((failures + 1)); }
incompatible() { printf 'INCOMPATIBLE: %s\n' "$*"; failures=$((failures + 1)); }

# Parse failures are recorded to a file (not a variable) because several
# readers run inside command substitutions, whose `failures` mutation would
# be lost. They are folded into the failure count before the result is printed.
parse_errors="$scratch/parse-errors"
: > "$parse_errors"
parse_error() { printf 'PARSE-FAILED: %s\n' "$*" >> "$parse_errors"; }
read_failed() { printf 'READ-FAILED: %s\n' "$*" >&2; }

# ── Manifest validation (fail closed: a malformed manifest is a config error) ─
manifest_get() {
  local value
  if ! value="$(jq -r "$1" "$manifest" 2>"$scratch/manifest.err")"; then
    printf 'MANIFEST-INVALID: could not read %s from %s: %s\n' \
      "$1" "$manifest" "$(sed -n '1p' "$scratch/manifest.err")" >&2
    exit 2
  fi
  printf '%s' "$value"
}

if ! jq -e 'type=="object"' "$manifest" >/dev/null 2>&1; then
  printf 'MANIFEST-INVALID: %s is not a JSON object\n' "$manifest" >&2
  exit 2
fi

env_name="$(manifest_get '.environment.name')"
manifest_custom="$(manifest_get '.environment.custom_branch_policies // false')"
manifest_protected="$(manifest_get '.environment.protected_branches // false')"
activation_variable="$(manifest_get '.activation.variable')"
gate_context="$(manifest_get '.required_status_checks[] | select((.integration // "") | tostring | startswith("variable:")) | .context')"
gate_variable="$(manifest_get '.required_status_checks[] | select((.integration // "") | tostring | startswith("variable:")) | .integration | sub("^variable:"; "")')"
resolution_required="$(manifest_get '.require_conversation_resolution // false')"

# The manifest must request the exact-main custom-branch-policy contract. The
# `protected_branches` flag authorizes every protected branch, not an exact
# allowlist, so a manifest that still encodes it is rejected rather than
# validated as if it were main-only.
if [ "$manifest_custom" != "true" ] || [ "$manifest_protected" != "false" ]; then
  printf 'MANIFEST-INVALID: the environment must set custom_branch_policies=true and protected_branches=false to encode the exact-main allowlist\n' >&2
  exit 2
fi
if [ -z "$env_name" ] || [ "$env_name" = "null" ]; then
  printf 'MANIFEST-INVALID: environment.name is missing\n' >&2
  exit 2
fi
if [ -z "$activation_variable" ] || [ "$activation_variable" = "null" ]; then
  printf 'MANIFEST-INVALID: activation.variable is missing\n' >&2
  exit 2
fi
if [ -z "$gate_context" ] || [ -z "$gate_variable" ]; then
  printf 'MANIFEST-INVALID: no required status check is bound to a repository variable integration\n' >&2
  exit 2
fi
if [ "$resolution_required" != "true" ] && [ "$resolution_required" != "false" ]; then
  printf 'MANIFEST-INVALID: require_conversation_resolution must be a boolean\n' >&2
  exit 2
fi
if ! jq -e '.required_status_checks | type == "array"' "$manifest" >/dev/null 2>&1; then
  printf 'MANIFEST-INVALID: required_status_checks must be an array\n' >&2
  exit 2
fi
if ! jq -e '.apps | type == "array"' "$manifest" >/dev/null 2>&1; then
  printf 'MANIFEST-INVALID: apps must be an array\n' >&2
  exit 2
fi

# ── Live reads (paginated, structural, fail-closed) ─────────────────────────
gh_err="$scratch/gh.err"

# Read a single API document. A clean 404 records absence; any other failure
# leaves a `.failed` sentinel that fails validation.
gather_doc() {
  local out="$1"; shift
  if gh api "$@" > "$out" 2>"$gh_err"; then
    if ! jq -e . "$out" >/dev/null 2>&1; then
      parse_error "malformed JSON from gh api $*"
    fi
    return 0
  fi
  if grep -Eq 'HTTP 404|Not Found' "$gh_err"; then
    rm -f "$out"
    return 0
  fi
  read_failed "gh api $*: $(sed -n '1p' "$gh_err")"
  rm -f "$out"
  : > "$out.failed"
  return 0
}

# Read a paginated endpoint whose pages are top-level JSON arrays, then
# normalize the combined inventory into one array. A page that is not an array,
# or a read failure on any page, fails closed; pagination stops on an empty
# page and is bounded so a hostile stub cannot spin forever.
gather_array() {
  local out="$1" endpoint="$2"
  local page=1 n page_out combined
  combined="$(mktemp "$scratch/array.XXXXXX")"
  : > "$combined"
  while [ "$page" -le 200 ]; do
    page_out="$(mktemp "$scratch/page.XXXXXX")"
    if ! gh api "${endpoint}?per_page=100&page=${page}" > "$page_out" 2>"$gh_err"; then
      if [ "$page" -eq 1 ] && grep -Eq 'HTTP 404|Not Found' "$gh_err"; then
        rm -f "$page_out"
        return 0
      fi
      read_failed "gh api ${endpoint} page ${page}: $(sed -n '1p' "$gh_err")"
      rm -f "$page_out"
      : > "$out.failed"
      return 0
    fi
    if ! jq -e 'type == "array"' "$page_out" >/dev/null 2>&1; then
      parse_error "unexpected non-array response from ${endpoint} page ${page}"
      : > "$out.failed"
      return 0
    fi
    n="$(jq 'length' "$page_out")"
    if [ "$n" -eq 0 ]; then
      break
    fi
    jq -c '.[]' "$page_out" >> "$combined"
    page=$((page + 1))
  done
  if ! jq -s '.' "$combined" > "$out" 2>/dev/null; then
    parse_error "could not normalize ${endpoint} pages"
  fi
  return 0
}

# Read a paginated endpoint whose pages are objects with an array under `key`
# (secrets listings, deployment branch policies). The combined document is
# re-shaped as {total_count, <key>: [...]}; an inventory that terminates before
# its declared total_count is incomplete and fails closed.
gather_paged_object() {
  local out="$1" endpoint="$2" key="$3"
  local page=1 n total="" collected=0 page_out combined final_count
  combined="$(mktemp "$scratch/object.XXXXXX")"
  : > "$combined"
  while [ "$page" -le 200 ]; do
    page_out="$(mktemp "$scratch/page.XXXXXX")"
    if ! gh api "${endpoint}?per_page=100&page=${page}" > "$page_out" 2>"$gh_err"; then
      if [ "$page" -eq 1 ] && grep -Eq 'HTTP 404|Not Found' "$gh_err"; then
        rm -f "$page_out"
        return 0
      fi
      read_failed "gh api ${endpoint} page ${page}: $(sed -n '1p' "$gh_err")"
      rm -f "$page_out"
      : > "$out.failed"
      return 0
    fi
    if ! jq -e --arg k "$key" 'type == "object" and (.[$k] | type) == "array"' "$page_out" >/dev/null 2>&1; then
      parse_error "unexpected response shape from ${endpoint} page ${page}"
      : > "$out.failed"
      return 0
    fi
    if [ -z "$total" ]; then
      total="$(jq -r '.total_count // empty' "$page_out" 2>/dev/null || true)"
      if [ -n "$total" ] && ! printf '%s' "$total" | grep -Eq '^[0-9]+$'; then
        parse_error "non-numeric total_count from ${endpoint}"
        total=""
      fi
    fi
    n="$(jq --arg k "$key" '.[$k] | length' "$page_out")"
    jq -c --arg k "$key" '.[$k][]' "$page_out" >> "$combined"
    collected=$((collected + n))
    if [ "$n" -eq 0 ]; then
      break
    fi
    if [ -n "$total" ] && [ "$collected" -ge "$total" ]; then
      break
    fi
    page=$((page + 1))
  done
  if ! jq -s --arg k "$key" '{total_count: length} + {($k): .}' "$combined" > "$out" 2>/dev/null; then
    parse_error "could not normalize ${endpoint}"
    return 0
  fi
  final_count="$(jq --arg k "$key" '.[$k] | length' "$out")"
  if [ -n "$total" ] && [ "$final_count" -ne "$total" ]; then
    parse_error "incomplete inventory from ${endpoint}: declared ${total}, read ${final_count}"
  fi
  return 0
}

# Read every document the validation needs. Every reference must resolve: a
# ruleset named by the applicable branch rules that cannot be read makes the
# inventory incomplete and leaves a nested `.failed` sentinel.
fetch_live() {
  local src="$1" id
  mkdir -p "$src/rulesets"
  gather_doc "$src/activation.json" "repos/$repo/actions/variables/$activation_variable"
  gather_doc "$src/app-id-variable.json" "repos/$repo/actions/variables/$gate_variable"
  gather_doc "$src/environment.json" "repos/$repo/environments/$env_name"
  gather_paged_object "$src/environment-secrets.json" "repos/$repo/environments/$env_name/secrets" secrets
  gather_paged_object "$src/dependabot-secrets.json" "repos/$repo/dependabot/secrets" secrets
  gather_array "$src/branch-rules.json" "repos/$repo/rules/branches/main"
  gather_paged_object "$src/deployment-branch-policies.json" "repos/$repo/environments/$env_name/deployment-branch-policies" branch_policies
  if [ -f "$src/branch-rules.json" ] && jq -e 'type == "array"' "$src/branch-rules.json" >/dev/null 2>&1; then
    while IFS= read -r id; do
      [ -n "$id" ] || continue
      gather_doc "$src/rulesets/$id.json" "repos/$repo/rulesets/$id"
      if [ ! -f "$src/rulesets/$id.json" ]; then
        read_failed "referenced ruleset $id could not be read; the applicable ruleset inventory is incomplete."
        : > "$src/rulesets/$id.json.failed"
      fi
    done < <(jq -r '.[] | select(type == "object") | .ruleset_id | select(type == "number") | tostring' "$src/branch-rules.json")
  fi
}

if [ -n "$offline" ]; then
  if [ ! -d "$offline" ]; then
    printf 'offline fixture directory not found: %s\n' "$offline" >&2
    exit 2
  fi
  src="$offline"
else
  live_src="$(mktemp -d "${TMPDIR:-/var/tmp}/bot-review-gate-live.XXXXXX")"
  src="$live_src"
  fetch_live "$src"
fi

# Every sentinel — top-level and nested — makes the observation incomplete.
for sentinel in "$src"/*.failed "$src"/rulesets/*.failed; do
  [ -e "$sentinel" ] || continue
  read_failed "one or more GitHub API reads failed ($(basename "$sentinel")); the configuration cannot be validated."
  failures=$((failures + 1))
done

value_of() {
  # Prints the `.value` of a JSON document file. An absent file means the
  # (optional) prerequisite is unset; a present but malformed document, or one
  # whose `.value` is missing/null, is a failed read and fails closed rather
  # than reading as unset.
  [ -f "$1" ] || return 0
  local value
  if ! value="$(jq -er 'select(type == "object" and (.value != null)) | .value | tostring' "$1" 2>/dev/null)"; then
    parse_error "malformed value document: $1"
    return 0
  fi
  printf '%s' "$value"
}

names_of() {
  # Prints the `.secrets[].name` names of a secrets listing file, or nothing if
  # the file is absent. A malformed listing or malformed entries fail closed.
  [ -f "$1" ] || return 0
  if ! jq -e 'type == "object" and (.secrets | type) == "array"' "$1" >/dev/null 2>&1; then
    parse_error "malformed secrets document: $1"
    return 0
  fi
  if ! jq -r '.secrets[] | select(type == "object") | .name | select(type == "string")' "$1" 2>/dev/null; then
    parse_error "malformed secret entries: $1"
  fi
}

# ── Live main required contexts, as "context|integration_id" ────────────────
live_contexts="$scratch/live-contexts.txt"
: > "$live_contexts"
if [ -f "$src/branch-rules.json" ] && ! jq -e 'type == "array"' "$src/branch-rules.json" >/dev/null 2>&1; then
  parse_error "malformed branch-rules inventory: $src/branch-rules.json"
fi
for detail in "$src"/rulesets/*.json; do
  [ -e "$detail" ] || continue
  if ! jq -e 'type == "object" and ((.rules // null) | type == "array" or . == null)' "$detail" >/dev/null 2>&1; then
    parse_error "malformed ruleset detail: $detail"
    continue
  fi
  if ! jq -r 'if (.rules | type) == "array" then
      .rules[]
      | select(type == "object" and .type == "required_status_checks")
      | (.parameters.required_status_checks // [])
      | if type == "array" then .[] else empty end
      | select(type == "object")
      | "\(.context // "")|\(.integration_id // "")"
    else empty end' "$detail" >> "$live_contexts" 2>>"$scratch/jq.err"; then
    parse_error "could not read required contexts from $detail"
  fi
done
sort -u -o "$live_contexts" "$live_contexts"

resolution_enforced=0
for detail in "$src"/rulesets/*.json; do
  [ -e "$detail" ] || continue
  jq -e 'type == "object"' "$detail" >/dev/null 2>&1 || continue
  enforcement="$(jq -r '
    [.rules[]?
      | select(type == "object" and .type == "pull_request")
      | .parameters.required_review_thread_resolution]
    | if length == 0 then "absent"
      elif all then "enforced"
      else "off" end' "$detail" 2>/dev/null || printf 'absent')"
  [ "$enforcement" = "enforced" ] && resolution_enforced=1
done

# Validate the complete reviewed environment policy: exact-main custom branch
# policies (never the protected-branches flag, which authorizes every protected
# branch), the full deployment branch-policy list, and the complete
# protection_rules list.
validate_environment() {
  local env_doc="$src/environment.json"
  if [ ! -f "$env_doc" ]; then
    missing "protected environment '$env_name' does not exist (or could not be read); the publisher cannot mint its App token."
    return 0
  fi
  if ! jq -e 'type == "object" and (.deployment_branch_policy | type) == "object"' "$env_doc" >/dev/null 2>&1; then
    parse_error "malformed environment document: $env_doc"
    return 0
  fi

  local custom protected
  custom="$(jq -r '.deployment_branch_policy.custom_branch_policies // false' "$env_doc")"
  protected="$(jq -r '.deployment_branch_policy.protected_branches // false' "$env_doc")"
  if [ "$custom" != "true" ]; then
    incompatible "environment '$env_name' does not select custom branch policies; the exact-main deployment allowlist cannot be verified."
  fi
  if [ "$protected" = "true" ]; then
    incompatible "environment '$env_name' authorizes every protected branch (protected_branches=true), not an exact-main allowlist."
  fi

  if ! jq -e 'has("protection_rules") and (.protection_rules | type) == "array"' "$env_doc" >/dev/null 2>&1; then
    missing "environment '$env_name' did not report a protection_rules list; the complete protection policy cannot be reviewed."
  else
    local rule_type
    while IFS= read -r rule_type; do
      case "$rule_type" in
        required_reviewers)
          incompatible "environment '$env_name' requires reviewers; the unattended publisher cannot satisfy required reviewers." ;;
        wait_timer)
          incompatible "environment '$env_name' has a wait timer; a wait gate is incompatible with the unattended publisher." ;;
        branch_policy)
          : ;;
        *)
          incompatible "environment '$env_name' reports an unrecognized protection rule '${rule_type:-<missing-type>}'." ;;
      esac
    done < <(jq -r '.protection_rules[] | if type == "object" then (.type // "<missing-type>") else "<malformed-rule>" end' "$env_doc" 2>/dev/null)
  fi

  local policies="$src/deployment-branch-policies.json"
  if [ ! -f "$policies" ]; then
    missing "deployment branch policies for environment '$env_name' could not be read; exact-main access is unproven."
    return 0
  fi
  if ! jq -e 'type == "object" and (.branch_policies | type) == "array"' "$policies" >/dev/null 2>&1; then
    parse_error "malformed deployment branch policy document: $policies"
    return 0
  fi
  local count ptype pname tags
  count="$(jq '.branch_policies | length' "$policies")"
  tags="$(jq '[.branch_policies[] | select(type == "object" and .type == "tag")] | length' "$policies")"
  if [ "$count" -ne 1 ]; then
    incompatible "environment '$env_name' has $count deployment branch policies; the accepted policy is exactly one branch policy for main."
  else
    ptype="$(jq -r '.branch_policies[0].type // ""' "$policies")"
    pname="$(jq -r '.branch_policies[0].name // ""' "$policies")"
    case "$ptype:$pname" in
      branch:main) : ;;
      *) incompatible "environment '$env_name' deployment policy is '${ptype}:${pname}', not exactly the 'main' branch (no tags, no other branches)." ;;
    esac
    case "$pname" in
      *'*'*) incompatible "environment '$env_name' deployment policy name '$pname' contains a wildcard." ;;
    esac
  fi
  if [ "$tags" -ne 0 ]; then
    incompatible "environment '$env_name' deployment policy includes tag policies; tags are not permitted."
  fi
}

activation="$(value_of "$src/activation.json")"

# ── Phase validation ────────────────────────────────────────────────────────
case "${activation:-}" in
  ''|inactive)
    note "Phase: STAGED INACTIVE (${activation_variable}=${activation:-<unset>})"
    note "The publisher publishes no App-authored verdict and claims no merge authority while inactive."
    note "Activation prerequisites (not required while inactive): environment '$env_name' restricted to exactly the main branch via custom deployment branch policies, its App secret names, and a numeric repository variable $gate_variable."
    note "Offline/read-only diagnostic secrets for the Dependabot readiness check: $(jq -r '[.apps[] | select(.credential_store == "dependabot") | .secret_names[]] | join(", ")' "$manifest")"
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

    validate_environment

    env_secret_names="$(names_of "$src/environment-secrets.json")"
    while IFS= read -r secret_name; do
      [ -n "$secret_name" ] || continue
      if ! grep -Fxq "$secret_name" <<< "$env_secret_names"; then
        missing "environment secret '$secret_name' is not present on environment '$env_name' (name only; no value is read)."
      fi
    done < <(jq -r '.apps[] | select(.credential_store | startswith("environment:")) | .secret_names[]' "$manifest")

    dep_secret_names="$(names_of "$src/dependabot-secrets.json")"
    while IFS= read -r secret_name; do
      [ -n "$secret_name" ] || continue
      if ! grep -Fxq "$secret_name" <<< "$dep_secret_names"; then
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
if [ -s "$parse_errors" ]; then
  cat "$parse_errors" >&2
  parse_errors_seen="$(grep -c . "$parse_errors" || true)"
  failures=$((failures + parse_errors_seen))
fi

if [ "$failures" -ne 0 ]; then
  note ""
  note "PREFLIGHT RESULT: $failures prerequisite(s) missing, incompatible, or unreadable."
  note "A prior green head, a token-mint failure, and an unreported or missed refresh are never merge authority; only a fresh App-authored verdict bound to the evaluated head is."
  exit 1
fi
note ""
note "PREFLIGHT RESULT: configuration is consistent."
note "This validator only reads; it verifies no credentials and mutates no settings."
exit 0
