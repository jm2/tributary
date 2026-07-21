#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=macos-package-policy.sh
source "${SCRIPT_DIR}/macos-package-policy.sh"

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/tributary-macos-policy.XXXXXX")"
POLICY_TMPDIR="${TEST_ROOT}/Policy Temp"
mkdir -p "$POLICY_TMPDIR"
TMPDIR="$POLICY_TMPDIR"
export TMPDIR
cleanup() {
  rm -rf "$TEST_ROOT"
}
trap cleanup EXIT

fail() {
  echo "not ok - $*" >&2
  exit 1
}

assert_status() {
  local expected="$1"
  shift
  local actual=0
  "$@" || actual=$?
  [[ "$actual" -eq "$expected" ]] \
    || fail "expected status ${expected}, got ${actual}: $*"
}

assert_prohibited_name() {
  macos_copy_control_path_is_prohibited "$1" \
    || fail "expected forbidden filename: $1"
}

assert_allowed_name() {
  if macos_copy_control_path_is_prohibited "$1"; then
    fail "ordinary runtime was overmatched by '${MACOS_PACKAGE_POLICY_MATCHED_TOKEN}': $1"
  fi
}

assert_no_policy_manifests() {
  local manifest
  for manifest in "$POLICY_TMPDIR"/tributary-macos-bundle-policy.*; do
    [[ ! -e "$manifest" ]] || fail "bundle-policy manifest was not cleaned up: $manifest"
  done
}

POLICY_FILE="$(macos_package_policy_default_file)"

EMPTY_POLICY="${TEST_ROOT}/empty-policy.txt"
INVALID_POLICY="${TEST_ROOT}/invalid-policy.txt"
DUPLICATE_POLICY="${TEST_ROOT}/duplicate-policy.txt"
MIXED_CASE_POLICY="${TEST_ROOT}/mixed-case-policy.txt"
printf '%s\n' '# comments do not constitute a policy' '   ' > "$EMPTY_POLICY"
printf '%s\n' 'dvdcss' 'not/a-token' > "$INVALID_POLICY"
printf '%s\n' 'aacs' 'AACS' > "$DUPLICATE_POLICY"
printf '%s\n' 'AaCs' > "$MIXED_CASE_POLICY"
assert_status 1 macos_package_policy_load "${TEST_ROOT}/missing-policy.txt"
assert_status 1 macos_package_policy_load "$EMPTY_POLICY"
assert_status 1 macos_package_policy_load "$INVALID_POLICY"
[[ "$MACOS_PACKAGE_POLICY_REASON" == *'invalid filename token'* ]] \
  || fail "invalid policy token did not produce a useful diagnostic"
assert_status 1 macos_package_policy_load "$DUPLICATE_POLICY"
[[ "$MACOS_PACKAGE_POLICY_REASON" == *'duplicate filename token'* ]] \
  || fail "case-insensitive duplicate did not produce a useful diagnostic"
assert_status 0 macos_package_policy_load "$MIXED_CASE_POLICY"
[[ "${MACOS_FORBIDDEN_COMPONENT_TOKENS[0]}" == aacs ]] \
  || fail "mixed-case policy token was not canonicalized"
assert_status 0 macos_package_policy_load "$POLICY_FILE"

for prohibited in \
  'libDVDcss.2.dylib' \
  'libdvd-pkg-installer' \
  'libdvdread.8.dylib' \
  'libdvdnav.4.dylib' \
  'libaacs.0.dylib' \
  'libbdplus.0.dylib' \
  'libgstbluray.dylib' \
  'libmmbd64.dylib' \
  'libgstresindvd.dylib' \
  'WidevineCDM.framework' \
  'FairPlayRuntime'; do
  assert_prohibited_name "$prohibited"
done

for ordinary_runtime in \
  'libgstlibav.dylib' \
  'libavcodec.61.dylib' \
  'libgstfdkaac.dylib' \
  'libgstaudioparsers.dylib' \
  'libgstdvdlpcmdec.dylib' \
  'libgstdvdsub.dylib' \
  'libbluray.2.dylib' \
  'libsoup-3.0.dylib' \
  'libssl.3.dylib' \
  'libcrypto.3.dylib'; do
  assert_allowed_name "$ordinary_runtime"
done

# The generic, non-decrypting libbluray formula path remains allowed even when
# every path component is checked; a denied source/dependency parent does not.
assert_allowed_name '/opt/homebrew/Cellar/libbluray/1.3.4/lib/libgstlibav.dylib'
if macos_copy_control_relative_path_is_prohibited \
  '/opt/homebrew/Cellar/libbluray/1.3.4/lib/libgstlibav.dylib'; then
  fail "generic libbluray formula path was overmatched"
fi
macos_copy_control_relative_path_is_prohibited \
  '/opt/homebrew/Cellar/libdvdcss/1.4.3/lib/libinnocent.dylib' \
  || fail "forbidden source parent path was not detected"
macos_copy_control_relative_path_is_prohibited '../WidevineCDM/helper.dylib' \
  || fail "forbidden intermediate relative-path component was not detected"
if macos_copy_control_relative_path_is_prohibited '../ordinary-codecs/helper.dylib'; then
  fail "benign intermediate relative-path component was overmatched"
fi

FAKE_OTOOL="${TEST_ROOT}/fake-otool"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'mode="${1:-}"' \
  'artifact=""' \
  'for argument in "$@"; do artifact="$argument"; done' \
  '[[ -e "${artifact}.otool-fail" ]] && exit 1' \
  'if [[ "$mode" == -l ]]; then' \
  '  if [[ -f "${artifact}.load-commands" ]]; then' \
  '    cat "${artifact}.load-commands"' \
  '  else' \
  '    printf "Load command 0\\n      cmd LC_RPATH\\n     path /usr/lib (offset 12)\\n"' \
  '  fi' \
  '  exit 0' \
  'fi' \
  'printf "%s:\n" "$artifact"' \
  'if [[ -f "${artifact}.deps" ]]; then' \
  '  while IFS= read -r dependency || [[ -n "$dependency" ]]; do' \
  '    printf "\t%s (compatibility version 1.0.0, current version 1.0.0)\n" "$dependency"' \
  '  done < "${artifact}.deps"' \
  'else' \
  '  printf "\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1.0.0)\n"' \
  'fi' > "$FAKE_OTOOL"
chmod +x "$FAKE_OTOOL"
MACOS_OTOOL_COMMAND="$FAKE_OTOOL"
export MACOS_OTOOL_COMMAND

PLUGIN_SOURCE="${TEST_ROOT}/plugins"
PLUGIN_DEST="${TEST_ROOT}/staged"
mkdir -p "$PLUGIN_SOURCE" "$PLUGIN_DEST"

touch "${PLUGIN_SOURCE}/libgstordinary.dylib"
printf '%s\n' \
  '/opt/homebrew/lib/libavcodec.61.dylib' \
  '/opt/homebrew/lib/libcrypto.3.dylib' \
  > "${PLUGIN_SOURCE}/libgstordinary.dylib.deps"
assert_status 0 macos_stage_gstreamer_plugin \
  "${PLUGIN_SOURCE}/libgstordinary.dylib" "$PLUGIN_DEST"
[[ "$MACOS_PACKAGE_POLICY_RESULT" == copied ]] || fail "ordinary plugin was not copied"
[[ -f "${PLUGIN_DEST}/libgstordinary.dylib" ]] || fail "ordinary plugin copy is missing"

touch "${PLUGIN_SOURCE}/libgstaacs.dylib"
assert_status 0 macos_stage_gstreamer_plugin \
  "${PLUGIN_SOURCE}/libgstaacs.dylib" "$PLUGIN_DEST"
[[ "$MACOS_PACKAGE_POLICY_RESULT" == excluded ]] || fail "named decrypt plugin was not excluded"
[[ ! -e "${PLUGIN_DEST}/libgstaacs.dylib" ]] || fail "excluded plugin reached staging"

touch "${PLUGIN_SOURCE}/libgstinnocent.dylib"
printf '%s\n' '@rpath/libaacs.0.dylib' \
  > "${PLUGIN_SOURCE}/libgstinnocent.dylib.deps"
assert_status 0 macos_stage_gstreamer_plugin \
  "${PLUGIN_SOURCE}/libgstinnocent.dylib" "$PLUGIN_DEST"
[[ "$MACOS_PACKAGE_POLICY_RESULT" == excluded ]] || fail "decrypt-linked plugin was not excluded"
[[ ! -e "${PLUGIN_DEST}/libgstinnocent.dylib" ]] || fail "decrypt-linked plugin reached staging"

touch "${PLUGIN_SOURCE}/libgstinnocent-path.dylib"
printf '%s\n' '@rpath/WidevineCDM/libinnocent.dylib' \
  > "${PLUGIN_SOURCE}/libgstinnocent-path.dylib.deps"
assert_status 0 macos_stage_gstreamer_plugin \
  "${PLUGIN_SOURCE}/libgstinnocent-path.dylib" "$PLUGIN_DEST"
[[ "$MACOS_PACKAGE_POLICY_RESULT" == excluded ]] \
  || fail "plugin linked through a forbidden dependency path was not excluded"
[[ ! -e "${PLUGIN_DEST}/libgstinnocent-path.dylib" ]] \
  || fail "plugin linked through a forbidden dependency path reached staging"

FORBIDDEN_PLUGIN_SOURCE="${TEST_ROOT}/libdvdcss-source/libgstsource-path.dylib"
mkdir -p "$(dirname "$FORBIDDEN_PLUGIN_SOURCE")"
touch "$FORBIDDEN_PLUGIN_SOURCE"
assert_status 0 macos_stage_gstreamer_plugin \
  "$FORBIDDEN_PLUGIN_SOURCE" "$PLUGIN_DEST"
[[ "$MACOS_PACKAGE_POLICY_RESULT" == excluded ]] \
  || fail "plugin from a forbidden source path was not excluded"

touch "${PLUGIN_SOURCE}/libgstload-command.dylib"
printf '%s\n' \
  'Load command 1' \
  '          cmd LC_RPATH' \
  '      cmdsize 48' \
  '         path @loader_path/../WidevineCDM (offset 12)' \
  > "${PLUGIN_SOURCE}/libgstload-command.dylib.load-commands"
assert_status 0 macos_stage_gstreamer_plugin \
  "${PLUGIN_SOURCE}/libgstload-command.dylib" "$PLUGIN_DEST"
[[ "$MACOS_PACKAGE_POLICY_RESULT" == excluded ]] \
  || fail "plugin with a forbidden Mach-O load-command path was not excluded"

touch "${PLUGIN_SOURCE}/libgstuninspectable.dylib"
touch "${PLUGIN_SOURCE}/libgstuninspectable.dylib.otool-fail"
assert_status 2 macos_stage_gstreamer_plugin \
  "${PLUGIN_SOURCE}/libgstuninspectable.dylib" "$PLUGIN_DEST"
[[ ! -e "${PLUGIN_DEST}/libgstuninspectable.dylib" ]] || fail "uninspectable plugin reached staging"

make_bundle() {
  local root="$1"
  mkdir -p \
    "$root/Contents/MacOS" \
    "$root/Contents/Frameworks" \
    "$root/Contents/Resources/lib/gstreamer-1.0"
  printf '%s\n' '#!/bin/bash' 'exit 0' > "$root/Contents/MacOS/$(basename "${root%.app}")"
}

SAFE_BUNDLE="${TEST_ROOT}/Safe.app"
make_bundle "$SAFE_BUNDLE"
touch "$SAFE_BUNDLE/Contents/MacOS/Safe-bin"
touch "$SAFE_BUNDLE/Contents/Frameworks/libavcodec.61.dylib"
touch "$SAFE_BUNDLE/Contents/Resources/lib/gstreamer-1.0/libgstlibav.dylib"
mkdir -p "$SAFE_BUNDLE/Contents/Resources/ordinary-codecs"
touch "$SAFE_BUNDLE/Contents/Resources/ordinary-codecs/helper.dat"
ln -s 'ordinary-codecs/helper.dat' \
  "$SAFE_BUNDLE/Contents/Resources/ordinary-runtime-link"
assert_status 0 macos_validate_bundle_copy_control "$SAFE_BUNDLE"
assert_no_policy_manifests

# A checkout/build ancestor is not part of the shipped bundle. Source-copy
# validation remains component-wise, while the final gate uses bundle-relative
# member paths and must not overmatch an external parent directory name.
SAFE_PARENT_BUNDLE="${TEST_ROOT}/libdvdcss-parent/SafeParent.app"
make_bundle "$SAFE_PARENT_BUNDLE"
touch "$SAFE_PARENT_BUNDLE/Contents/Frameworks/libgstlibav.dylib"
assert_status 0 macos_validate_bundle_copy_control "$SAFE_PARENT_BUNDLE"
assert_no_policy_manifests

REAL_FIND_COMMAND="$(command -v find)"
FAKE_FIND="${TEST_ROOT}/fake-find"
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'root="${1:-}"' \
  'if [[ "${MACOS_FIND_FAIL_SCOPE:-}" == all ]]; then exit 71; fi' \
  'if [[ "${MACOS_FIND_FAIL_SCOPE:-}" == imports && "$root" == */Contents ]]; then exit 72; fi' \
  'exec "$REAL_FIND_COMMAND" "$@"' > "$FAKE_FIND"
chmod +x "$FAKE_FIND"
export REAL_FIND_COMMAND
MACOS_FIND_COMMAND="$FAKE_FIND"
export MACOS_FIND_COMMAND

MACOS_FIND_FAIL_SCOPE=all
export MACOS_FIND_FAIL_SCOPE
assert_status 2 macos_validate_bundle_copy_control "$SAFE_BUNDLE"
[[ "$MACOS_PACKAGE_POLICY_REASON" == *'could not enumerate macOS bundle members'* ]] \
  || fail "failed member enumeration did not produce a useful diagnostic"
assert_no_policy_manifests

MACOS_FIND_FAIL_SCOPE=imports
export MACOS_FIND_FAIL_SCOPE
assert_status 2 macos_validate_bundle_copy_control "$SAFE_BUNDLE"
[[ "$MACOS_PACKAGE_POLICY_REASON" == *'could not enumerate macOS bundle import candidates'* ]] \
  || fail "failed import enumeration did not produce a useful diagnostic"
assert_no_policy_manifests

unset MACOS_FIND_FAIL_SCOPE
MACOS_FIND_COMMAND="$REAL_FIND_COMMAND"

NAMED_BUNDLE="${TEST_ROOT}/Named.app"
make_bundle "$NAMED_BUNDLE"
touch "$NAMED_BUNDLE/Contents/Frameworks/libDVDcss.2.dylib"
assert_status 1 macos_validate_bundle_copy_control "$NAMED_BUNDLE"

DIRECTORY_BUNDLE="${TEST_ROOT}/Directory.app"
make_bundle "$DIRECTORY_BUNDLE"
mkdir -p "$DIRECTORY_BUNDLE/Contents/Resources/WidevineCDM"
touch "$DIRECTORY_BUNDLE/Contents/Resources/WidevineCDM/helper.dat"
assert_status 1 macos_validate_bundle_copy_control "$DIRECTORY_BUNDLE"

IMPORTED_BUNDLE="${TEST_ROOT}/Imported.app"
make_bundle "$IMPORTED_BUNDLE"
touch "$IMPORTED_BUNDLE/Contents/Frameworks/libinnocent.dylib"
printf '%s\n' '@rpath/libbdplus.0.dylib' \
  > "$IMPORTED_BUNDLE/Contents/Frameworks/libinnocent.dylib.deps"
assert_status 1 macos_validate_bundle_copy_control "$IMPORTED_BUNDLE"

RESOURCE_MACHO_BUNDLE="${TEST_ROOT}/ResourceMachO.app"
make_bundle "$RESOURCE_MACHO_BUNDLE"
printf '\xcf\xfa\xed\xfe' \
  > "$RESOURCE_MACHO_BUNDLE/Contents/Resources/allowed-helper.dat"
printf '%s\n' '@rpath/libaacs.0.dylib' \
  > "$RESOURCE_MACHO_BUNDLE/Contents/Resources/allowed-helper.dat.deps"
assert_status 1 macos_validate_bundle_copy_control "$RESOURCE_MACHO_BUNDLE"

FRAMEWORK_MACHO_BUNDLE="${TEST_ROOT}/FrameworkMachO.app"
make_bundle "$FRAMEWORK_MACHO_BUNDLE"
printf '\xcf\xfa\xed\xfe' \
  > "$FRAMEWORK_MACHO_BUNDLE/Contents/Frameworks/allowed-helper.dat"
printf '%s\n' '@rpath/libbdplus.0.dylib' \
  > "$FRAMEWORK_MACHO_BUNDLE/Contents/Frameworks/allowed-helper.dat.deps"
assert_status 1 macos_validate_bundle_copy_control "$FRAMEWORK_MACHO_BUNDLE"

WRAPPER_MACHO_BUNDLE="${TEST_ROOT}/WrapperMachO.app"
make_bundle "$WRAPPER_MACHO_BUNDLE"
printf '\xcf\xfa\xed\xfe' \
  > "$WRAPPER_MACHO_BUNDLE/Contents/MacOS/WrapperMachO"
printf '%s\n' '@rpath/libaacs.0.dylib' \
  > "$WRAPPER_MACHO_BUNDLE/Contents/MacOS/WrapperMachO.deps"
assert_status 1 macos_validate_bundle_copy_control "$WRAPPER_MACHO_BUNDLE"

SYMLINK_BUNDLE="${TEST_ROOT}/Symlink.app"
make_bundle "$SYMLINK_BUNDLE"
ln -s '../WidevineCDM/helper.dylib' "$SYMLINK_BUNDLE/Contents/Resources/runtime-link"
assert_status 1 macos_validate_bundle_copy_control "$SYMLINK_BUNDLE"

UNINSPECTABLE_BUNDLE="${TEST_ROOT}/Uninspectable.app"
make_bundle "$UNINSPECTABLE_BUNDLE"
touch "$UNINSPECTABLE_BUNDLE/Contents/Frameworks/libordinary.dylib"
touch "$UNINSPECTABLE_BUNDLE/Contents/Frameworks/libordinary.dylib.otool-fail"
assert_status 2 macos_validate_bundle_copy_control "$UNINSPECTABLE_BUNDLE"

echo "ok - macOS packaging policy rejects decrypt components without hiding ordinary codecs"
