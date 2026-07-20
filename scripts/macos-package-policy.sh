#!/usr/bin/env bash
# Shared helpers for enforcing Tributary's macOS bundle component policy.
# This file is sourced by build-macos.sh and by its deterministic policy tests.

MACOS_PACKAGE_POLICY_REASON=""
MACOS_PACKAGE_POLICY_RESULT=""
MACOS_PACKAGE_POLICY_MATCHED_TOKEN=""
MACOS_FORBIDDEN_COMPONENT_TOKENS=()

macos_package_policy_default_file() {
  local helper_dir
  helper_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  printf '%s\n' "${helper_dir}/../build-aux/packaging/forbidden-bundled-components.txt"
}

macos_package_policy_load() {
  local policy_file="${1:-${TRIBUTARY_FORBIDDEN_COMPONENTS_FILE:-}}"
  local line token canonical_token known_token
  local LC_ALL=C

  if [[ -z "$policy_file" ]]; then
    policy_file="$(macos_package_policy_default_file)"
  fi

  MACOS_FORBIDDEN_COMPONENT_TOKENS=()
  MACOS_PACKAGE_POLICY_REASON=""
  MACOS_PACKAGE_POLICY_RESULT=""
  MACOS_PACKAGE_POLICY_MATCHED_TOKEN=""

  if [[ ! -f "$policy_file" ]]; then
    MACOS_PACKAGE_POLICY_REASON="Required bundled-component policy is missing: ${policy_file}"
    MACOS_PACKAGE_POLICY_RESULT="error"
    return 1
  fi

  while IFS= read -r line || [[ -n "$line" ]]; do
    token="$(printf '%s' "$line" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
    [[ -z "$token" || "$token" == \#* ]] && continue

    if [[ ! "$token" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]]; then
      MACOS_FORBIDDEN_COMPONENT_TOKENS=()
      MACOS_PACKAGE_POLICY_REASON="Bundled-component policy contains an invalid filename token: '${token}'"
      MACOS_PACKAGE_POLICY_RESULT="error"
      return 1
    fi

    canonical_token="$(printf '%s' "$token" | tr '[:upper:]' '[:lower:]')"
    for known_token in "${MACOS_FORBIDDEN_COMPONENT_TOKENS[@]}"; do
      if [[ "$known_token" == "$canonical_token" ]]; then
        MACOS_FORBIDDEN_COMPONENT_TOKENS=()
        MACOS_PACKAGE_POLICY_REASON="Bundled-component policy contains a duplicate filename token: '${token}'"
        MACOS_PACKAGE_POLICY_RESULT="error"
        return 1
      fi
    done
    MACOS_FORBIDDEN_COMPONENT_TOKENS+=("$canonical_token")
  done < "$policy_file"

  if [[ ${#MACOS_FORBIDDEN_COMPONENT_TOKENS[@]} -eq 0 ]]; then
    MACOS_PACKAGE_POLICY_REASON="Bundled-component policy contains no filename tokens: ${policy_file}"
    MACOS_PACKAGE_POLICY_RESULT="error"
    return 1
  fi

  MACOS_PACKAGE_POLICY_RESULT="loaded"
  return 0
}

macos_copy_control_path_is_prohibited() {
  local path="$1"
  local filename token

  filename="$(basename "$path" | tr '[:upper:]' '[:lower:]')"
  MACOS_PACKAGE_POLICY_MATCHED_TOKEN=""

  for token in "${MACOS_FORBIDDEN_COMPONENT_TOKENS[@]}"; do
    if [[ "$filename" == *"$token"* ]]; then
      MACOS_PACKAGE_POLICY_MATCHED_TOKEN="$token"
      return 0
    fi
  done

  return 1
}

macos_copy_control_relative_path_is_prohibited() {
  local relative_path="$1"
  local remaining component

  remaining="$relative_path"
  while :; do
    component="${remaining%%/*}"
    if [[ -n "$component" ]] \
      && macos_copy_control_path_is_prohibited "$component"; then
      return 0
    fi

    [[ "$remaining" == */* ]] || break
    remaining="${remaining#*/}"
  done

  return 1
}

macos_validate_macho_copy_control() {
  local artifact="$1"
  local otool_output line dependency

  MACOS_PACKAGE_POLICY_REASON=""
  MACOS_PACKAGE_POLICY_RESULT=""

  if [[ ${#MACOS_FORBIDDEN_COMPONENT_TOKENS[@]} -eq 0 ]]; then
    MACOS_PACKAGE_POLICY_REASON="bundled-component policy has not been loaded"
    MACOS_PACKAGE_POLICY_RESULT="uninspectable"
    return 2
  fi

  if macos_copy_control_path_is_prohibited "$artifact"; then
    MACOS_PACKAGE_POLICY_REASON="$(basename "$artifact") matches forbidden token '${MACOS_PACKAGE_POLICY_MATCHED_TOKEN}'"
    MACOS_PACKAGE_POLICY_RESULT="prohibited"
    return 1
  fi

  if ! otool_output="$("${MACOS_OTOOL_COMMAND:-otool}" -L "$artifact" 2>&1)"; then
    MACOS_PACKAGE_POLICY_REASON="could not inspect Mach-O imports for ${artifact}"
    MACOS_PACKAGE_POLICY_RESULT="uninspectable"
    return 2
  fi

  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ "$line" == *' (compatibility version '* ]] || continue

    dependency="$line"
    while [[ "$dependency" == ' '* || "$dependency" == $'\t'* ]]; do
      dependency="${dependency#?}"
    done
    dependency="${dependency%% \(*}"
    [[ -z "$dependency" ]] && continue

    if macos_copy_control_path_is_prohibited "$dependency"; then
      MACOS_PACKAGE_POLICY_REASON="$(basename "$artifact") imports forbidden component $(basename "$dependency") (token '${MACOS_PACKAGE_POLICY_MATCHED_TOKEN}')"
      MACOS_PACKAGE_POLICY_RESULT="prohibited"
      return 1
    fi
  done <<< "$otool_output"

  MACOS_PACKAGE_POLICY_RESULT="allowed"
  return 0
}

macos_stage_gstreamer_plugin() {
  local source="$1"
  local destination_dir="$2"
  local validation_status=0

  macos_validate_macho_copy_control "$source" || validation_status=$?
  case "$validation_status" in
    0)
      ;;
    1)
      MACOS_PACKAGE_POLICY_RESULT="excluded"
      return 0
      ;;
    *)
      MACOS_PACKAGE_POLICY_RESULT="error"
      return "$validation_status"
      ;;
  esac

  if ! cp "$source" "$destination_dir/"; then
    MACOS_PACKAGE_POLICY_REASON="could not copy GStreamer plugin ${source}"
    MACOS_PACKAGE_POLICY_RESULT="error"
    return 1
  fi

  MACOS_PACKAGE_POLICY_RESULT="copied"
  return 0
}

macos_bundle_artifact_requires_import_scan() {
  local bundle_root="$1"
  local artifact="$2"
  local bundle_name basename magic_output magic

  bundle_name="$(basename "$bundle_root")"
  bundle_name="${bundle_name%.app}"
  basename="$(basename "$artifact")"

  case "$basename" in
    *.dylib|*.so)
      return 0
      ;;
  esac

  case "$artifact" in
    "$bundle_root"/Contents/MacOS/*)
      # build-macos.sh creates this one shell wrapper. Every other regular
      # executable in Contents/MacOS is a copied or built Mach-O artifact.
      [[ "$artifact" == "$bundle_root/Contents/MacOS/$bundle_name" ]] && return 1
      return 0
      ;;
    "$bundle_root"/Contents/Frameworks/*)
      [[ -x "$artifact" ]]
      return
      ;;
  esac

  # Mach-O payloads are not required to use a conventional extension or
  # executable bit. Recognize thin and universal binaries by their on-disk
  # magic so an allowed-named helper under Resources cannot hide an import.
  [[ -f "$artifact" && ! -L "$artifact" ]] || return 1
  if ! magic_output="$("${MACOS_OD_COMMAND:-od}" -An -tx1 -N4 < "$artifact" 2>/dev/null)"; then
    MACOS_PACKAGE_POLICY_REASON="could not inspect file magic for ${artifact}"
    MACOS_PACKAGE_POLICY_RESULT="uninspectable"
    return 2
  fi
  if ! magic="$(printf '%s' "$magic_output" | tr -d '[:space:]')"; then
    MACOS_PACKAGE_POLICY_REASON="could not normalize file magic for ${artifact}"
    MACOS_PACKAGE_POLICY_RESULT="uninspectable"
    return 2
  fi
  magic="$(printf '%s' "$magic" | tr '[:upper:]' '[:lower:]')"
  case "$magic" in
    feedface|cefaedfe|feedfacf|cffaedfe|cafebabe|bebafeca|cafebabf|bfbafeca)
      return 0
      ;;
  esac

  return 1
}

macos_validate_bundle_member_manifest() {
  local bundle_root="$1"
  local manifest="$2"
  local artifact relative_path link_target

  # Check every member name, including directories and symlinks. This catches
  # forbidden frameworks, plugin directories, versioned dylibs, and CDMs even
  # when a member is not itself a Mach-O executable.
  while IFS= read -r -d '' artifact; do
    relative_path="${artifact#"$bundle_root"/}"
    if macos_copy_control_relative_path_is_prohibited "$relative_path"; then
      MACOS_PACKAGE_POLICY_REASON="bundle member ${relative_path} has a path component matching forbidden token '${MACOS_PACKAGE_POLICY_MATCHED_TOKEN}'"
      MACOS_PACKAGE_POLICY_RESULT="prohibited"
      return 1
    fi

    if [[ -L "$artifact" ]]; then
      if ! link_target="$(readlink "$artifact")"; then
        MACOS_PACKAGE_POLICY_REASON="could not inspect bundle symlink ${artifact#"$bundle_root"/}"
        MACOS_PACKAGE_POLICY_RESULT="uninspectable"
        return 2
      fi
      if macos_copy_control_relative_path_is_prohibited "$link_target"; then
        MACOS_PACKAGE_POLICY_REASON="bundle symlink ${relative_path} targets a path with forbidden component ${link_target} (token '${MACOS_PACKAGE_POLICY_MATCHED_TOKEN}')"
        MACOS_PACKAGE_POLICY_RESULT="prohibited"
        return 1
      fi
    fi
  done < "$manifest"

  return 0
}

macos_validate_bundle_import_manifest() {
  local bundle_root="$1"
  local manifest="$2"
  local artifact candidate_status validation_status

  # Filename filtering alone is insufficient: an innocuously named plugin can
  # import a prohibited library. Inspect all copied dylibs/plugins and every
  # executable that the bundle will launch. Failure to inspect is fatal.
  while IFS= read -r -d '' artifact; do
    candidate_status=0
    macos_bundle_artifact_requires_import_scan "$bundle_root" "$artifact" \
      || candidate_status=$?
    case "$candidate_status" in
      0) ;;
      1) continue ;;
      *) return "$candidate_status" ;;
    esac

    validation_status=0
    macos_validate_macho_copy_control "$artifact" || validation_status=$?
    if [[ $validation_status -ne 0 ]]; then
      return "$validation_status"
    fi
  done < "$manifest"

  return 0
}

macos_validate_bundle_copy_control() {
  local bundle_root="$1"
  local manifest_dir members_before imports members_after validation_status

  MACOS_PACKAGE_POLICY_REASON=""
  MACOS_PACKAGE_POLICY_RESULT=""

  if [[ ${#MACOS_FORBIDDEN_COMPONENT_TOKENS[@]} -eq 0 ]]; then
    MACOS_PACKAGE_POLICY_REASON="bundled-component policy has not been loaded"
    MACOS_PACKAGE_POLICY_RESULT="uninspectable"
    return 2
  fi

  if [[ ! -d "$bundle_root/Contents" ]]; then
    MACOS_PACKAGE_POLICY_REASON="macOS bundle Contents directory does not exist: ${bundle_root}/Contents"
    MACOS_PACKAGE_POLICY_RESULT="error"
    return 1
  fi

  if ! manifest_dir="$(mktemp -d "${TMPDIR:-/tmp}/tributary-macos-bundle-policy.XXXXXX")"; then
    MACOS_PACKAGE_POLICY_REASON="could not create a private macOS bundle-policy manifest directory"
    MACOS_PACKAGE_POLICY_RESULT="uninspectable"
    return 2
  fi
  members_before="${manifest_dir}/members-before.nul"
  imports="${manifest_dir}/imports.nul"
  members_after="${manifest_dir}/members-after.nul"

  # Do not consume find through process substitution: Bash cannot observe that
  # producer's status. Materialize each NUL-delimited pass privately, check the
  # traversal itself, and only then consume its complete result.
  if ! "${MACOS_FIND_COMMAND:-find}" "$bundle_root" -mindepth 1 -print0 > "$members_before"; then
    rm -rf "$manifest_dir"
    MACOS_PACKAGE_POLICY_REASON="could not enumerate macOS bundle members: ${bundle_root}"
    MACOS_PACKAGE_POLICY_RESULT="uninspectable"
    return 2
  fi

  validation_status=0
  macos_validate_bundle_member_manifest "$bundle_root" "$members_before" \
    || validation_status=$?
  if [[ $validation_status -ne 0 ]]; then
    rm -rf "$manifest_dir"
    return "$validation_status"
  fi

  if ! "${MACOS_FIND_COMMAND:-find}" "$bundle_root/Contents" -type f -print0 > "$imports"; then
    rm -rf "$manifest_dir"
    MACOS_PACKAGE_POLICY_REASON="could not enumerate macOS bundle import candidates: ${bundle_root}/Contents"
    MACOS_PACKAGE_POLICY_RESULT="uninspectable"
    return 2
  fi

  validation_status=0
  macos_validate_bundle_import_manifest "$bundle_root" "$imports" \
    || validation_status=$?
  if [[ $validation_status -ne 0 ]]; then
    rm -rf "$manifest_dir"
    return "$validation_status"
  fi

  # A second checked snapshot makes concurrent additions, removals, or renames
  # fail closed. Recheck names and symlink targets from that snapshot as well.
  if ! "${MACOS_FIND_COMMAND:-find}" "$bundle_root" -mindepth 1 -print0 > "$members_after"; then
    rm -rf "$manifest_dir"
    MACOS_PACKAGE_POLICY_REASON="could not re-enumerate macOS bundle members: ${bundle_root}"
    MACOS_PACKAGE_POLICY_RESULT="uninspectable"
    return 2
  fi

  validation_status=0
  macos_validate_bundle_member_manifest "$bundle_root" "$members_after" \
    || validation_status=$?
  if [[ $validation_status -ne 0 ]]; then
    rm -rf "$manifest_dir"
    return "$validation_status"
  fi

  if ! cmp -s "$members_before" "$members_after"; then
    rm -rf "$manifest_dir"
    MACOS_PACKAGE_POLICY_REASON="macOS bundle changed during component-policy validation: ${bundle_root}"
    MACOS_PACKAGE_POLICY_RESULT="uninspectable"
    return 2
  fi

  rm -rf "$manifest_dir"

  MACOS_PACKAGE_POLICY_RESULT="allowed"
  return 0
}
