#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=macos-icon-bundle-policy.sh
source "${SCRIPT_DIR}/macos-icon-bundle-policy.sh"

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/tributary-macos-icon-policy.XXXXXX")"
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

assert_reason_contains() {
  [[ "$MACOS_ICON_POLICY_REASON" == *"$1"* ]] \
    || fail "diagnostic '${MACOS_ICON_POLICY_REASON}' did not contain '$1'"
}

assert_no_decode_directories() {
  local temporary
  for temporary in "$POLICY_TMPDIR"/tributary-macos-icons.*; do
    [[ ! -e "$temporary" ]] \
      || fail "ICNS inspection directory was not cleaned up: ${temporary}"
  done
}

write_png_fixture() {
  local path="$1"
  local width="$2"
  local height="$3"
  mkdir -p "$(dirname "$path")"
  printf 'test-png\n' > "$path"
  printf '%s %s\n' "$width" "$height" > "${path}.dimensions"
}

make_iconset() {
  local root="$1"
  local name size
  mkdir -p "$root"
  while IFS=':' read -r name size; do
    write_png_fixture "${root}/${name}" "$size" "$size"
  done <<'EOF'
icon_16x16.png:16
icon_16x16@2x.png:32
icon_32x32.png:32
icon_32x32@2x.png:64
icon_128x128.png:128
icon_128x128@2x.png:256
icon_256x256.png:256
icon_256x256@2x.png:512
icon_512x512.png:512
icon_512x512@2x.png:1024
EOF
}

make_hicolor_icons() {
  local root="$1"
  local bundle_id="$2"
  local size
  for size in 16 24 32 48 64 128 256 512; do
    write_png_fixture \
      "${root}/${size}x${size}/apps/${bundle_id}.png" "$size" "$size"
  done
}

FAKE_SIPS="${TEST_ROOT}/fake-sips"
FAKE_PLUTIL="${TEST_ROOT}/fake-plutil"
FAKE_ICONUTIL="${TEST_ROOT}/fake-iconutil"

printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'icon_path=""' \
  'for argument in "$@"; do icon_path="$argument"; done' \
  '[[ ! -e "${icon_path}.sips-fail" ]] || exit 65' \
  'read -r width height < "${icon_path}.dimensions"' \
  'printf "%s:\\n  pixelWidth: %s\\n  pixelHeight: %s\\n" "$icon_path" "$width" "$height"' \
  > "$FAKE_SIPS"

printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'plist_path=""' \
  'for argument in "$@"; do plist_path="$argument"; done' \
  '[[ ! -e "${plist_path}.plutil-fail" ]] || exit 66' \
  'cat "${plist_path}.icon-name"' \
  > "$FAKE_PLUTIL"

printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'conversion=""' \
  'output_path=""' \
  'input_path=""' \
  'while [[ $# -gt 0 ]]; do' \
  '  case "$1" in' \
  '    -c) conversion="$2"; shift 2 ;;' \
  '    -o) output_path="$2"; shift 2 ;;' \
  '    *) input_path="$1"; shift ;;' \
  '  esac' \
  'done' \
  '[[ "$conversion" == iconset && -n "$output_path" && -n "$input_path" ]] || exit 67' \
  'grep -Fq invalid-icns "$input_path" && exit 68' \
  'mkdir -p "$output_path"' \
  'while IFS=: read -r name size; do' \
  '  if [[ "$size" == 1024 ]] && grep -Fq no-1024 "$input_path"; then continue; fi' \
  '  if [[ "$name" == icon_16x16@2x.png ]] && grep -Fq no-16-retina "$input_path"; then continue; fi' \
  '  printf "decoded-png\\n" > "${output_path}/${name}"' \
  '  printf "%s %s\\n" "$size" "$size" > "${output_path}/${name}.dimensions"' \
  '  if [[ "$size" == 256 ]] && grep -Fq bad-decoded-png "$input_path"; then' \
  '    touch "${output_path}/${name}.sips-fail"' \
  '  fi' \
  'done <<EOF' \
  'icon_16x16.png:16' \
  'icon_16x16@2x.png:32' \
  'icon_32x32.png:32' \
  'icon_32x32@2x.png:64' \
  'icon_128x128.png:128' \
  'icon_128x128@2x.png:256' \
  'icon_256x256.png:256' \
  'icon_256x256@2x.png:512' \
  'icon_512x512.png:512' \
  'icon_512x512@2x.png:1024' \
  'EOF' \
  > "$FAKE_ICONUTIL"

chmod +x "$FAKE_SIPS" "$FAKE_PLUTIL" "$FAKE_ICONUTIL"
MACOS_SIPS_COMMAND="$FAKE_SIPS"
MACOS_PLUTIL_COMMAND="$FAKE_PLUTIL"
MACOS_ICONUTIL_COMMAND="$FAKE_ICONUTIL"
export MACOS_SIPS_COMMAND MACOS_PLUTIL_COMMAND MACOS_ICONUTIL_COMMAND

BUNDLE_ID="io.github.tributary.Tributary"
ICONSET="${TEST_ROOT}/tributary.iconset"
HICOLOR="${TEST_ROOT}/hicolor"
make_iconset "$ICONSET"
make_hicolor_icons "$HICOLOR" "$BUNDLE_ID"

assert_status 0 macos_validate_icon_sources "$ICONSET" "$HICOLOR" "$BUNDLE_ID"

rm "${ICONSET}/icon_512x512@2x.png"
assert_status 1 macos_validate_icon_sources "$ICONSET" "$HICOLOR" "$BUNDLE_ID"
assert_reason_contains 'icon_512x512@2x.png'
write_png_fixture "${ICONSET}/icon_512x512@2x.png" 1024 1024

printf 'wrong dimensions\n' > "${ICONSET}/icon_512x512@2x.png.dimensions"
assert_status 1 macos_validate_icon_sources "$ICONSET" "$HICOLOR" "$BUNDLE_ID"
assert_reason_contains 'invalid dimensions'
printf '1024 1024\n' > "${ICONSET}/icon_512x512@2x.png.dimensions"

rm "${HICOLOR}/48x48/apps/${BUNDLE_ID}.png"
assert_status 1 macos_validate_icon_sources "$ICONSET" "$HICOLOR" "$BUNDLE_ID"
assert_reason_contains '48x48'
write_png_fixture "${HICOLOR}/48x48/apps/${BUNDLE_ID}.png" 48 48

printf '47 48\n' > "${HICOLOR}/48x48/apps/${BUNDLE_ID}.png.dimensions"
assert_status 1 macos_validate_icon_sources "$ICONSET" "$HICOLOR" "$BUNDLE_ID"
assert_reason_contains 'is 47x48; expected 48x48'
printf '48 48\n' > "${HICOLOR}/48x48/apps/${BUNDLE_ID}.png.dimensions"

APP_BUNDLE="${TEST_ROOT}/Tributary.app"
PLIST="${APP_BUNDLE}/Contents/Info.plist"
RESOURCES="${APP_BUNDLE}/Contents/Resources"
mkdir -p "$RESOURCES"
printf '<plist/>\n' > "$PLIST"
printf 'tributary\n' > "${PLIST}.icon-name"
printf 'valid-icns\n' > "${RESOURCES}/tributary.icns"
make_hicolor_icons "${RESOURCES}/share/icons/hicolor" "$BUNDLE_ID"

assert_status 0 macos_validate_app_icon_bundle "$APP_BUNDLE" "$BUNDLE_ID"
assert_no_decode_directories

printf 'tributary.icns\n' > "${PLIST}.icon-name"
assert_status 0 macos_validate_app_icon_bundle "$APP_BUNDLE" "$BUNDLE_ID"
printf 'tributary\n' > "${PLIST}.icon-name"

printf '\n' > "${PLIST}.icon-name"
assert_status 1 macos_validate_app_icon_bundle "$APP_BUNDLE" "$BUNDLE_ID"
assert_reason_contains 'nonempty resource filename'
printf 'tributary\n' > "${PLIST}.icon-name"

printf '../tributary\n' > "${PLIST}.icon-name"
assert_status 1 macos_validate_app_icon_bundle "$APP_BUNDLE" "$BUNDLE_ID"
assert_reason_contains 'resource filename'
printf 'tributary\n' > "${PLIST}.icon-name"

touch "${PLIST}.plutil-fail"
assert_status 1 macos_validate_app_icon_bundle "$APP_BUNDLE" "$BUNDLE_ID"
assert_reason_contains 'could not read CFBundleIconFile'
rm "${PLIST}.plutil-fail"

mv "${RESOURCES}/tributary.icns" "${RESOURCES}/saved.icns"
assert_status 1 macos_validate_app_icon_bundle "$APP_BUNDLE" "$BUNDLE_ID"
assert_reason_contains 'does not resolve'
mv "${RESOURCES}/saved.icns" "${RESOURCES}/tributary.icns"

: > "${RESOURCES}/tributary.icns"
assert_status 1 macos_validate_app_icon_bundle "$APP_BUNDLE" "$BUNDLE_ID"
assert_reason_contains 'nonempty regular ICNS'

printf 'invalid-icns\n' > "${RESOURCES}/tributary.icns"
assert_status 1 macos_validate_app_icon_bundle "$APP_BUNDLE" "$BUNDLE_ID"
assert_reason_contains 'not a parseable ICNS'
assert_no_decode_directories

printf 'no-1024\n' > "${RESOURCES}/tributary.icns"
assert_status 1 macos_validate_app_icon_bundle "$APP_BUNDLE" "$BUNDLE_ID"
assert_reason_contains 'icon_512x512@2x.png'
assert_no_decode_directories

# A 32px image still exists through icon_32x32.png, but that must not let the
# distinct 16pt Retina representation disappear unnoticed.
printf 'no-16-retina\n' > "${RESOURCES}/tributary.icns"
assert_status 1 macos_validate_app_icon_bundle "$APP_BUNDLE" "$BUNDLE_ID"
assert_reason_contains 'icon_16x16@2x.png'
assert_no_decode_directories

printf 'bad-decoded-png\n' > "${RESOURCES}/tributary.icns"
assert_status 1 macos_validate_app_icon_bundle "$APP_BUNDLE" "$BUNDLE_ID"
assert_reason_contains 'could not parse PNG icon'
assert_no_decode_directories

printf 'valid-icns\n' > "${RESOURCES}/tributary.icns"
rm "${RESOURCES}/share/icons/hicolor/256x256/apps/${BUNDLE_ID}.png"
assert_status 1 macos_validate_app_icon_bundle "$APP_BUNDLE" "$BUNDLE_ID"
assert_reason_contains '256x256'
assert_no_decode_directories

echo "ok - macOS app icons are complete, parseable, and linked from the bundle plist"
