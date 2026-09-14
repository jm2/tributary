#!/usr/bin/env bash
# Regression coverage for the macOS rpath-repair helpers in build-macos.sh.
#
# The helpers must keep exactly the same policy decisions while doing far less
# repeated work: each distinct immutable source is inspected once per build,
# and the equivalent install_name_tool edits for one binary are batched into a
# single invocation. The last scenario proves the batching is still fail
# closed: a rejected source aborts before any edit is applied.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_SCRIPT="${SCRIPT_DIR}/build-macos.sh"

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/tributary-macos-rpath.XXXXXX")"
cleanup() {
  rm -rf "$TEST_ROOT"
}
trap cleanup EXIT

fail() {
  echo "not ok - $*" >&2
  exit 1
}

pass_count=0
ok() {
  pass_count=$((pass_count + 1))
  echo "ok - $*"
}

# Extract the rpath helpers from the build script without executing it.
HELPERS="${TEST_ROOT}/helpers.sh"
sed -n '/^# >>> macOS rpath helpers$/,/^# <<< macOS rpath helpers$/p' \
  "$BUILD_SCRIPT" > "$HELPERS"
[[ -s "$HELPERS" ]] || fail "could not extract the macOS rpath helpers"
# shellcheck source=/dev/null
source "$HELPERS"
grep -q '^fix_rpaths()' "$HELPERS" || fail "extracted helpers are missing fix_rpaths"
grep -q '^copy_dylib()' "$HELPERS" || fail "extracted helpers are missing copy_dylib"

# ── Tool fixtures ────────────────────────────────────────────────────────────
FAKE_BIN="${TEST_ROOT}/tools"
mkdir -p "$FAKE_BIN"

# `otool -L <artifact>` prints the dependency list recorded beside the fixture.
cat > "${FAKE_BIN}/otool" <<'EOF'
#!/usr/bin/env bash
artifact="${!#}"
printf '%s:\n' "$artifact"
if [[ -f "${artifact}.deps" ]]; then
  while IFS= read -r dependency || [[ -n "$dependency" ]]; do
    printf '\t%s (compatibility version 1.0.0, current version 1.0.0)\n' "$dependency"
  done < "${artifact}.deps"
fi
EOF

# Record every install_name_tool invocation so the test can count processes.
cat > "${FAKE_BIN}/install_name_tool" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${INSTALL_NAME_LOG}"
EOF
chmod +x "${FAKE_BIN}/otool" "${FAKE_BIN}/install_name_tool"
export PATH="${FAKE_BIN}:${PATH}"

FRAMEWORKS_DIR="${TEST_ROOT}/Frameworks"
BREW_PREFIX="${TEST_ROOT}/opt/homebrew"
mkdir -p "$FRAMEWORKS_DIR" "${BREW_PREFIX}/lib"

VALIDATION_LOG="${TEST_ROOT}/validations.log"
INSTALL_NAME_LOG="${TEST_ROOT}/install-name.log"
REASON_CAPTURE_FILE="${TEST_ROOT}/policy-reason.txt"
export INSTALL_NAME_LOG
VALIDATION_REJECT=""

# Count each source inspection. The real helper wraps
# macos_validate_macho_copy_control, so overriding it here observes exactly the
# number of policy inspections the build performs.
macos_validate_macho_copy_control() {
  local artifact="$1"
  printf '%s\n' "$artifact" >> "$VALIDATION_LOG"
  if [[ -n "$VALIDATION_REJECT" && "$artifact" == "$VALIDATION_REJECT" ]]; then
    MACOS_PACKAGE_POLICY_REASON="fixture rejection for ${artifact}"
    # Mirror the reason through the same variable the production helper sets,
    # capturing it to a fixture-side file rather than stdout: scenario 3 asserts
    # that copy_dylib's production error call propagates this exact reason, so the
    # rejection output must come only from that path (a fixture that printed the
    # reason too would mask a weakened error call that dropped it). Reading the
    # variable for the capture also consumes the assignment for static analysis.
    printf '%s' "$MACOS_PACKAGE_POLICY_REASON" > "$REASON_CAPTURE_FILE"
    return 1
  fi
  return 0
}

error() {
  printf 'fixture error: %s\n' "$*" >&2
  exit 1
}

reset_fixture_state() {
  : > "$VALIDATION_LOG"
  : > "$INSTALL_NAME_LOG"
  : > "$REASON_CAPTURE_FILE"
  VALIDATION_REJECT=""
  MACOS_VALIDATED_SOURCE_CACHE=$'\n'
  MACOS_SOURCE_VALIDATIONS=0
  MACOS_SOURCE_CACHE_HITS=0
}

make_library() {
  local source="$1"
  mkdir -p "$(dirname "$source")"
  touch "$source"
}

make_binary() {
  local binary="$1"
  shift
  touch "$binary"
  printf '%s\n' "$@" > "${binary}.deps"
}

# ── Scenario 1: a shared dependency is inspected once across consumers ───────
reset_fixture_state
make_library "${BREW_PREFIX}/lib/libshared.dylib"
make_library "${BREW_PREFIX}/lib/libalpha.dylib"
make_library "${BREW_PREFIX}/lib/libbeta.dylib"
PLUGIN_A="${TEST_ROOT}/libgstalpha.dylib"
PLUGIN_B="${TEST_ROOT}/libgstbeta.dylib"
make_binary "$PLUGIN_A" '@rpath/libshared.dylib' '@rpath/libalpha.dylib'
make_binary "$PLUGIN_B" '@rpath/libshared.dylib' '@rpath/libbeta.dylib'

fix_rpaths "$PLUGIN_A"
fix_rpaths "$PLUGIN_B"

inspect_count="$(wc -l < "$VALIDATION_LOG" | tr -d ' ')"
[[ "$inspect_count" -eq 3 ]] \
  || fail "expected 3 distinct source inspections, saw ${inspect_count}"
[[ "$MACOS_SOURCE_VALIDATIONS" -eq "$inspect_count" ]] \
  || fail "validator counter recorded ${MACOS_SOURCE_VALIDATIONS} of ${inspect_count} distinct-source inspections"
[[ "$MACOS_SOURCE_CACHE_HITS" -eq 1 ]] \
  || fail "expected 1 cached reuse, saw ${MACOS_SOURCE_CACHE_HITS}"
[[ "$(grep -Fc "${BREW_PREFIX}/lib/libshared.dylib" "$VALIDATION_LOG")" -eq 1 ]] \
  || fail "shared source was inspected more than once"
[[ "$MACOS_VALIDATED_SOURCE_CACHE" == *$'\n'"${BREW_PREFIX}/lib/libshared.dylib"$'\n'* ]] \
  || fail "validation cache did not record the exact inspected source path"

change_lines="$(grep -c -- '-change' "$INSTALL_NAME_LOG" || true)"
change_tokens="$(grep -o -- '-change' "$INSTALL_NAME_LOG" | wc -l | tr -d ' ')"
[[ "$change_lines" -eq 2 ]] \
  || fail "expected one batched install_name_tool -change call per binary, saw ${change_lines}"
[[ "$change_tokens" -eq 4 ]] \
  || fail "expected 4 total -change edits, saw ${change_tokens}"
ok "shared sources are inspected once and edits are batched per binary"

# ── Scenario 2: the cache keys on the exact source, not the basename ─────────
reset_fixture_state
DUP_ONE="${TEST_ROOT}/opt/homebrew/lib/one/libdup.dylib"
DUP_TWO="${TEST_ROOT}/opt/homebrew/lib/two/libdup.dylib"
make_library "$DUP_ONE"
make_library "$DUP_TWO"
PLUGIN_C="${TEST_ROOT}/libgstdup.dylib"
make_binary "$PLUGIN_C" "$DUP_ONE" "$DUP_TWO"

fix_rpaths "$PLUGIN_C"

[[ "$(grep -Fc "$DUP_ONE" "$VALIDATION_LOG")" -eq 1 ]] \
  || fail "first exact source was not inspected exactly once"
[[ "$(grep -Fc "$DUP_TWO" "$VALIDATION_LOG")" -eq 1 ]] \
  || fail "a same-basename source was skipped by an inexact cache key"
[[ "$MACOS_SOURCE_VALIDATIONS" -eq 2 ]] \
  || fail "expected 2 distinct-source validations, saw ${MACOS_SOURCE_VALIDATIONS}"
[[ "$MACOS_VALIDATED_SOURCE_CACHE" == *$'\n'"$DUP_ONE"$'\n'* ]] \
  || fail "validation cache did not key on the first exact source path"
[[ "$MACOS_VALIDATED_SOURCE_CACHE" == *$'\n'"$DUP_TWO"$'\n'* ]] \
  || fail "validation cache did not key on the second exact source path"
[[ "$MACOS_SOURCE_CACHE_HITS" -eq 0 ]] \
  || fail "distinct sources must not register as cache hits"
ok "validation cache keys on the exact source path, not the basename"

# ── Scenario 3: a rejected source still aborts before any edit ───────────────
reset_fixture_state
make_library "${BREW_PREFIX}/lib/libok.dylib"
make_library "${BREW_PREFIX}/lib/libbad.dylib"
PLUGIN_D="${TEST_ROOT}/libgstmixed.dylib"
make_binary "$PLUGIN_D" '@rpath/libok.dylib' '@rpath/libbad.dylib'

set +e
rejection_output="$(
  VALIDATION_REJECT="${BREW_PREFIX}/lib/libbad.dylib"
  fix_rpaths "$PLUGIN_D" 2>&1
)"
rejection_status=$?
set -e

[[ "$rejection_status" -ne 0 ]] \
  || fail "a prohibited source must abort the build"
[[ ! -f "${FRAMEWORKS_DIR}/libbad.dylib" ]] \
  || fail "a prohibited source reached Frameworks/"
change_edits="$(grep -c -- '-change' "$INSTALL_NAME_LOG" || true)"
[[ "$change_edits" -eq 0 ]] \
  || fail "no batched -change edit may run once a source is rejected"
# The fixture no longer prints the reason, so the only possible source of the
# complete production message is copy_dylib's
# `error "Refusing recursive dylib dependency: ${MACOS_PACKAGE_POLICY_REASON}"`
# call. Assert the whole message, including the exact rejected source path, so a
# weakened error call that drops the reason (or the source identity) fails.
expected_rejection="Refusing recursive dylib dependency: fixture rejection for ${BREW_PREFIX}/lib/libbad.dylib"
[[ "$rejection_output" == *"$expected_rejection"* ]] \
  || fail "rejection did not propagate the complete production error message for the exact rejected source"
# The fixture mirrors the rejected source's reason through the production
# variable and captures it beside the build. Assert that captured value so the
# fixture genuinely consumes MACOS_PACKAGE_POLICY_REASON, while the output
# assertion above remains the sole proof that copy_dylib's production error call
# propagates the reason.
[[ "$(cat "$REASON_CAPTURE_FILE")" == "fixture rejection for ${BREW_PREFIX}/lib/libbad.dylib" ]] \
  || fail "fixture did not mirror the rejected source's policy reason through the production variable"
ok "a rejected source aborts before any batched edit is applied"

echo "1..${pass_count}"
