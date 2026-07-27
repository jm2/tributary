#!/usr/bin/env bash

# Fail-closed validation for the app-owned icons shipped in the macOS bundle.
# The command variables are injectable so the policy can be tested on Linux;
# release builds pin them to the corresponding macOS system tools.

MACOS_ICON_POLICY_REASON=""
MACOS_ICON_PNG_WIDTH=""
MACOS_ICON_PNG_HEIGHT=""

macos_icon_policy_fail() {
  MACOS_ICON_POLICY_REASON="$1"
  return 1
}

macos_icon_read_png_dimensions() {
  local icon_path="$1"
  local sips_command="${MACOS_SIPS_COMMAND:-/usr/bin/sips}"
  local metadata width height

  MACOS_ICON_PNG_WIDTH=""
  MACOS_ICON_PNG_HEIGHT=""

  if [[ ! -x "$sips_command" ]]; then
    macos_icon_policy_fail "required PNG inspection tool is unavailable: ${sips_command}"
    return 1
  fi
  if ! metadata="$("$sips_command" -g pixelWidth -g pixelHeight "$icon_path" 2>/dev/null)"; then
    macos_icon_policy_fail "could not parse PNG icon: ${icon_path}"
    return 1
  fi

  width="$(printf '%s\n' "$metadata" \
    | awk '$1 == "pixelWidth:" { print $2; exit }')"
  height="$(printf '%s\n' "$metadata" \
    | awk '$1 == "pixelHeight:" { print $2; exit }')"
  if [[ ! "$width" =~ ^[0-9]+$ || ! "$height" =~ ^[0-9]+$ ]]; then
    macos_icon_policy_fail "PNG inspection returned invalid dimensions for ${icon_path}"
    return 1
  fi

  MACOS_ICON_PNG_WIDTH="$width"
  MACOS_ICON_PNG_HEIGHT="$height"
}

macos_icon_require_png() {
  local icon_path="$1"
  local expected_width="$2"
  local expected_height="$3"

  if [[ ! -f "$icon_path" || -L "$icon_path" || ! -s "$icon_path" ]]; then
    macos_icon_policy_fail "required PNG icon is missing, empty, or a symlink: ${icon_path}"
    return 1
  fi
  if ! macos_icon_read_png_dimensions "$icon_path"; then
    return 1
  fi
  if [[ "$MACOS_ICON_PNG_WIDTH" -ne "$expected_width" \
     || "$MACOS_ICON_PNG_HEIGHT" -ne "$expected_height" ]]; then
    macos_icon_policy_fail \
      "PNG icon ${icon_path} is ${MACOS_ICON_PNG_WIDTH}x${MACOS_ICON_PNG_HEIGHT}; expected ${expected_width}x${expected_height}"
    return 1
  fi
}

macos_validate_iconset_sources() {
  local iconset_root="$1"
  local icon_name expected_size

  MACOS_ICON_POLICY_REASON=""
  if [[ ! -d "$iconset_root" || -L "$iconset_root" ]]; then
    macos_icon_policy_fail "macOS iconset directory is missing or a symlink: ${iconset_root}"
    return 1
  fi

  while IFS=':' read -r icon_name expected_size; do
    if ! macos_icon_require_png \
      "${iconset_root}/${icon_name}" "$expected_size" "$expected_size"; then
      return 1
    fi
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

macos_validate_hicolor_icons() {
  local hicolor_root="$1"
  local bundle_id="$2"
  local size icon_path

  MACOS_ICON_POLICY_REASON=""
  if [[ ! -d "$hicolor_root" || -L "$hicolor_root" ]]; then
    macos_icon_policy_fail "hicolor icon directory is missing or a symlink: ${hicolor_root}"
    return 1
  fi

  for size in 16 24 32 48 64 128 256 512; do
    icon_path="${hicolor_root}/${size}x${size}/apps/${bundle_id}.png"
    if ! macos_icon_require_png "$icon_path" "$size" "$size"; then
      return 1
    fi
  done
}

macos_validate_icon_sources() {
  local iconset_root="$1"
  local hicolor_root="$2"
  local bundle_id="$3"

  if ! macos_validate_iconset_sources "$iconset_root"; then
    return 1
  fi
  macos_validate_hicolor_icons "$hicolor_root" "$bundle_id"
}

macos_validate_app_icon_bundle() {
  local app_bundle="$1"
  local bundle_id="$2"
  local plist_path="${app_bundle}/Contents/Info.plist"
  local resources_path="${app_bundle}/Contents/Resources"
  local plutil_command="${MACOS_PLUTIL_COMMAND:-/usr/bin/plutil}"
  local iconutil_command="${MACOS_ICONUTIL_COMMAND:-/usr/bin/iconutil}"
  local icon_setting icon_filename icns_path
  local decode_root decoded_iconset decoded_reason

  MACOS_ICON_POLICY_REASON=""
  if [[ ! -f "$plist_path" || -L "$plist_path" || ! -s "$plist_path" ]]; then
    macos_icon_policy_fail "bundle Info.plist is missing, empty, or a symlink: ${plist_path}"
    return 1
  fi
  if [[ ! -x "$plutil_command" ]]; then
    macos_icon_policy_fail "required plist inspection tool is unavailable: ${plutil_command}"
    return 1
  fi
  if ! icon_setting="$(
    "$plutil_command" -extract CFBundleIconFile raw -o - "$plist_path" 2>/dev/null
  )"; then
    macos_icon_policy_fail "could not read CFBundleIconFile from ${plist_path}"
    return 1
  fi
  case "$icon_setting" in
    ""|"."|".."|*/*|*\\*)
      macos_icon_policy_fail \
        "CFBundleIconFile must be a nonempty resource filename, got: ${icon_setting:-<empty>}"
      return 1
      ;;
  esac

  case "$icon_setting" in
    *.icns) icon_filename="$icon_setting" ;;
    *) icon_filename="${icon_setting}.icns" ;;
  esac
  icns_path="${resources_path}/${icon_filename}"
  if [[ ! -f "$icns_path" || -L "$icns_path" || ! -s "$icns_path" ]]; then
    macos_icon_policy_fail \
      "CFBundleIconFile does not resolve to a nonempty regular ICNS resource: ${icns_path}"
    return 1
  fi
  if [[ ! -x "$iconutil_command" ]]; then
    macos_icon_policy_fail "required ICNS inspection tool is unavailable: ${iconutil_command}"
    return 1
  fi
  if ! decode_root="$(mktemp -d "${TMPDIR:-/tmp}/tributary-macos-icons.XXXXXX")"; then
    macos_icon_policy_fail "could not create a temporary directory for ICNS inspection"
    return 1
  fi
  decoded_iconset="${decode_root}/decoded.iconset"
  if ! "$iconutil_command" -c iconset -o "$decoded_iconset" "$icns_path" \
      >/dev/null 2>&1; then
    rm -rf "$decode_root"
    macos_icon_policy_fail "CFBundleIconFile is not a parseable ICNS resource: ${icns_path}"
    return 1
  fi

  # Require the canonical logical-size/scale entries, not merely one decoded
  # PNG at each pixel size. For example, icon_16x16@2x.png and
  # icon_32x32.png are both 32×32 pixels but are distinct ICNS
  # representations used in different display contexts.
  if ! macos_validate_iconset_sources "$decoded_iconset"; then
    decoded_reason="$MACOS_ICON_POLICY_REASON"
    rm -rf "$decode_root"
    macos_icon_policy_fail \
      "ICNS resource ${icns_path} is missing or has an invalid required representation: ${decoded_reason}"
    return 1
  fi
  rm -rf "$decode_root"

  macos_validate_hicolor_icons \
    "${resources_path}/share/icons/hicolor" "$bundle_id"
}
