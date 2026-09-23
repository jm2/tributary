#!/usr/bin/env bash
# Shared helpers for enforcing Tributary's macOS bundle component policy.
# This file is sourced by build-macos.sh and by its deterministic policy tests.

MACOS_PACKAGE_POLICY_REASON=""
MACOS_PACKAGE_POLICY_RESULT=""
MACOS_PACKAGE_POLICY_MATCHED_TOKEN=""
MACOS_FORBIDDEN_COMPONENT_TOKENS=()
MACOS_FORBIDDEN_COMPONENT_TOKEN_COUNT=0

# These files are required even when the build host can provide an ambient
# fallback. In particular, libsoup is loaded dynamically by recent Homebrew
# Soup plugins and does not appear in their linked dependency inventory.
macos_validate_audio_runtime_inventory() {
  local plugin_dir="$1"
  local frameworks_dir="$2"
  local plugin
  MACOS_PACKAGE_POLICY_REASON=""
  for plugin in libgstcoreelements libgstosxaudio libgstplayback libgstsoup; do
    if [[ ! -s "$plugin_dir/${plugin}.dylib" ]]; then
      MACOS_PACKAGE_POLICY_REASON="Missing required GStreamer audio plugin: ${plugin}"
      return 1
    fi
  done
  if [[ ! -s "$frameworks_dir/libsoup-3.0.0.dylib" ]]; then
    MACOS_PACKAGE_POLICY_REASON="Missing required bundled libsoup runtime: libsoup-3.0.0.dylib"
    return 1
  fi
}

macos_package_policy_default_file() {
  local helper_dir
  helper_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  printf '%s\n' "${helper_dir}/../build-aux/packaging/forbidden-bundled-components.txt"
}

macos_package_policy_load() {
  local policy_file="${1:-${TRIBUTARY_FORBIDDEN_COMPONENTS_FILE:-}}"
  local line token canonical_token known_token
  local LC_ALL=C
  export LC_ALL

  if [[ -z "$policy_file" ]]; then
    policy_file="$(macos_package_policy_default_file)"
  fi

  MACOS_FORBIDDEN_COMPONENT_TOKENS=()
  MACOS_FORBIDDEN_COMPONENT_TOKEN_COUNT=0
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
    for known_token in ${MACOS_FORBIDDEN_COMPONENT_TOKENS[@]+"${MACOS_FORBIDDEN_COMPONENT_TOKENS[@]}"}; do
      if [[ "$known_token" == "$canonical_token" ]]; then
        MACOS_FORBIDDEN_COMPONENT_TOKENS=()
        MACOS_FORBIDDEN_COMPONENT_TOKEN_COUNT=0
        MACOS_PACKAGE_POLICY_REASON="Bundled-component policy contains a duplicate filename token: '${token}'"
        MACOS_PACKAGE_POLICY_RESULT="error"
        return 1
      fi
    done
    MACOS_FORBIDDEN_COMPONENT_TOKENS+=("$canonical_token")
    MACOS_FORBIDDEN_COMPONENT_TOKEN_COUNT=$((MACOS_FORBIDDEN_COMPONENT_TOKEN_COUNT + 1))
  done < "$policy_file"

  if [[ $MACOS_FORBIDDEN_COMPONENT_TOKEN_COUNT -eq 0 ]]; then
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
  local LC_ALL=C
  export LC_ALL

  filename="$(basename "$path" | tr '[:upper:]' '[:lower:]')"
  MACOS_PACKAGE_POLICY_MATCHED_TOKEN=""

  for token in ${MACOS_FORBIDDEN_COMPONENT_TOKENS[@]+"${MACOS_FORBIDDEN_COMPONENT_TOKENS[@]}"}; do
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
  local inspect_source_path="${2:-true}"
  local otool_output otool_load_commands line dependency load_reference
  local LC_ALL=C
  export LC_ALL

  MACOS_PACKAGE_POLICY_REASON=""
  MACOS_PACKAGE_POLICY_RESULT=""

  if [[ $MACOS_FORBIDDEN_COMPONENT_TOKEN_COUNT -eq 0 ]]; then
    MACOS_PACKAGE_POLICY_REASON="bundled-component policy has not been loaded"
    MACOS_PACKAGE_POLICY_RESULT="uninspectable"
    return 2
  fi

  if [[ "$inspect_source_path" == true ]] \
    && macos_copy_control_relative_path_is_prohibited "$artifact"; then
    MACOS_PACKAGE_POLICY_REASON="source path ${artifact} matches forbidden token '${MACOS_PACKAGE_POLICY_MATCHED_TOKEN}'"
    MACOS_PACKAGE_POLICY_RESULT="prohibited"
    return 1
  fi
  if [[ "$inspect_source_path" != true ]] \
    && macos_copy_control_path_is_prohibited "$artifact"; then
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

    if macos_copy_control_relative_path_is_prohibited "$dependency"; then
      MACOS_PACKAGE_POLICY_REASON="$(basename "$artifact") imports forbidden component path ${dependency} (token '${MACOS_PACKAGE_POLICY_MATCHED_TOKEN}')"
      MACOS_PACKAGE_POLICY_RESULT="prohibited"
      return 1
    fi
  done <<< "$otool_output"

  # -L covers linked dylibs, while -l also exposes LC_RPATH,
  # LC_LOAD_DYLINKER, and other load-command name/path fields that can redirect
  # an allowed basename through a recognizable denied directory.
  if ! otool_load_commands="$("${MACOS_OTOOL_COMMAND:-otool}" -l "$artifact" 2>&1)"; then
    MACOS_PACKAGE_POLICY_REASON="could not inspect Mach-O load commands for ${artifact}"
    MACOS_PACKAGE_POLICY_RESULT="uninspectable"
    return 2
  fi

  while IFS= read -r line || [[ -n "$line" ]]; do
    while [[ "$line" == ' '* || "$line" == $'\t'* ]]; do
      line="${line#?}"
    done
    case "$line" in
      name\ *\ \(offset\ *|path\ *\ \(offset\ *)
        load_reference="${line#* }"
        load_reference="${load_reference%% \(offset *}"
        [[ -z "$load_reference" ]] && continue
        if macos_copy_control_relative_path_is_prohibited "$load_reference"; then
          MACOS_PACKAGE_POLICY_REASON="$(basename "$artifact") contains forbidden Mach-O load path ${load_reference} (token '${MACOS_PACKAGE_POLICY_MATCHED_TOKEN}')"
          MACOS_PACKAGE_POLICY_RESULT="prohibited"
          return 1
        fi
        ;;
    esac
  done <<< "$otool_load_commands"

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
  local LC_ALL=C
  export LC_ALL

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
      # file in Contents/MacOS is a copied or built Mach-O artifact. Let the
      # wrapper fall through to magic inspection so a future Mach-O wrapper
      # cannot bypass import validation merely by retaining the same name.
      [[ "$artifact" == "$bundle_root/Contents/MacOS/$bundle_name" ]] || return 0
      ;;
    "$bundle_root"/Contents/Frameworks/*)
      # Framework members are not guaranteed to retain an executable bit.
      # Scan executable members directly and use magic detection for all
      # remaining regular files.
      [[ -x "$artifact" ]] && return 0
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
    macos_validate_macho_copy_control "$artifact" false || validation_status=$?
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

  if [[ $MACOS_FORBIDDEN_COMPONENT_TOKEN_COUNT -eq 0 ]]; then
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

# The release carries only allowlisted GStreamer plugins. Entries are
# "<name> [windows|macos]"; malformed, duplicate, and forbidden names fail the
# load so the list cannot silently widen the bundle. Load the bundled-component
# policy first.
MACOS_GSTREAMER_PLUGIN_NAMES=()

macos_gstreamer_allowlist_load() {
  local allowlist="$1"
  local platform="$2"
  local line entry name entry_platform

  MACOS_GSTREAMER_PLUGIN_NAMES=()
  MACOS_PACKAGE_POLICY_REASON=""
  if [[ $MACOS_FORBIDDEN_COMPONENT_TOKEN_COUNT -eq 0 ]]; then
    MACOS_PACKAGE_POLICY_REASON="bundled-component policy has not been loaded"
    return 1
  fi
  if [[ ! -f "$allowlist" ]]; then
    MACOS_PACKAGE_POLICY_REASON="Required GStreamer plugin allowlist is missing: ${allowlist}"
    return 1
  fi

  local seen=$'\n'
  while IFS= read -r line || [[ -n "$line" ]]; do
    entry="$(printf '%s' "$line" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
    [[ -z "$entry" || "$entry" == \#* ]] && continue
    if [[ ! "$entry" =~ ^([a-z0-9_]+)([[:space:]]+(windows|macos))?$ ]]; then
      MACOS_GSTREAMER_PLUGIN_NAMES=()
      MACOS_PACKAGE_POLICY_REASON="GStreamer plugin allowlist contains an invalid entry: '${entry}'"
      return 1
    fi
    name="${BASH_REMATCH[1]}"
    entry_platform="${BASH_REMATCH[3]}"
    if [[ "$seen" == *$'\n'"${name}"$'\n'* ]]; then
      MACOS_GSTREAMER_PLUGIN_NAMES=()
      MACOS_PACKAGE_POLICY_REASON="GStreamer plugin allowlist lists '${name}' more than once"
      return 1
    fi
    seen+="${name}"$'\n'
    if macos_copy_control_path_is_prohibited "libgst${name}"; then
      MACOS_GSTREAMER_PLUGIN_NAMES=()
      MACOS_PACKAGE_POLICY_REASON="GStreamer plugin allowlist names a forbidden component: '${name}'"
      return 1
    fi
    if [[ -z "$entry_platform" || "$entry_platform" == "$platform" ]]; then
      MACOS_GSTREAMER_PLUGIN_NAMES+=("$name")
    fi
  done < "$allowlist"

  if [[ ${#MACOS_GSTREAMER_PLUGIN_NAMES[@]} -eq 0 ]]; then
    MACOS_PACKAGE_POLICY_REASON="GStreamer plugin allowlist contains no ${platform} plugins: ${allowlist}"
    return 1
  fi
  return 0
}

macos_gstreamer_plugin_is_allowlisted() {
  local filename="$1"
  local name known
  [[ "$filename" =~ ^libgst([a-z0-9_]+)\.dylib$ ]] || return 1
  name="${BASH_REMATCH[1]}"
  for known in ${MACOS_GSTREAMER_PLUGIN_NAMES[@]+"${MACOS_GSTREAMER_PLUGIN_NAMES[@]}"}; do
    [[ "$known" == "$name" ]] && return 0
  done
  return 1
}

# Fail unless every member of the plugin directory is a regular, allowlisted
# plugin file directly inside it.
macos_validate_gstreamer_plugin_allowlist() {
  local plugin_dir="$1"
  local member relative listing

  MACOS_PACKAGE_POLICY_REASON=""
  if [[ ! -d "$plugin_dir" ]]; then
    MACOS_PACKAGE_POLICY_REASON="GStreamer plugin directory is missing: ${plugin_dir}"
    return 1
  fi
  if ! listing="$("${MACOS_FIND_COMMAND:-find}" "$plugin_dir" -mindepth 1 -print)"; then
    MACOS_PACKAGE_POLICY_REASON="could not enumerate GStreamer plugin directory: ${plugin_dir}"
    return 2
  fi
  while IFS= read -r member; do
    [[ -n "$member" ]] || continue
    relative="${member#"$plugin_dir"/}"
    if [[ "$relative" == */* || -L "$member" || ! -f "$member" ]] \
      || ! macos_gstreamer_plugin_is_allowlisted "$relative"; then
      MACOS_PACKAGE_POLICY_REASON="GStreamer plugin directory contains a member outside the allowlist: ${relative}"
      return 1
    fi
  done <<< "$listing"
  return 0
}

# Print the physical path of an existing file or directory, resolving
# symlinks in its final component and in every parent directory.
macos_physical_path() {
  local path="$1"
  local target parent hops=0
  while [[ -L "$path" ]]; do
    target="$(readlink "$path")" || return 1
    [[ "$target" == /* ]] || target="$(dirname -- "$path")/${target}"
    path="$target"
    hops=$((hops + 1))
    [[ $hops -le 40 ]] || return 1
  done
  [[ -e "$path" ]] || return 1
  parent="$(cd -P -- "$(dirname -- "$path")" 2>/dev/null && pwd -P)" || return 1
  printf '%s/%s\n' "$parent" "$(basename -- "$path")"
}

# Set MACOS_HOMEBREW_KEG to "<formula>/<version>" when a bundled source path
# resolves into a keg below the given physical Cellar directory.
MACOS_HOMEBREW_KEG=""
macos_homebrew_keg() {
  local source="$1"
  local cellar="$2"
  local physical rest formula
  MACOS_HOMEBREW_KEG=""
  physical="$(macos_physical_path "$source")" || return 1
  [[ "$physical" == "$cellar"/*/*/* ]] || return 1
  rest="${physical#"$cellar"/}"
  formula="${rest%%/*}"
  rest="${rest#*/}"
  MACOS_HOMEBREW_KEG="${formula}/${rest%%/*}"
}

# Print the first string value of a key in a Homebrew keg's SBOM, which
# Homebrew writes one key per line.
macos_keg_sbom_value() {
  local sbom="$1"
  local key="$2"
  [[ -f "$sbom" ]] || return 0
  sed -n "s/^[[:space:]]*\"${key}\":[[:space:]]*\"\\(.*\\)\",\\{0,1\\}[[:space:]]*\$/\\1/p" "$sbom" \
    | sed -n '1p'
}

# Attribute every bundled Homebrew file to its keg, copy each keg's license
# files, and write THIRD-PARTY-NOTICES.txt into the app's Resources.
# MACOS_BUNDLED_BINARY_SOURCES lists copied Mach-O sources, each of which must
# belong to a keg; MACOS_BUNDLED_DATA_SOURCES lists copied trees whose
# linked members are attributed when they come from a keg.
MACOS_BUNDLED_BINARY_SOURCES=()
MACOS_BUNDLED_DATA_SOURCES=()

macos_write_third_party_notices() {
  local resources_dir="$1"
  local cellar="$2"
  local repository_root="$3"
  local notices="${resources_dir}/THIRD-PARTY-NOTICES.txt"
  local licenses="${resources_dir}/licenses"
  local kegs=$'\n' source tree member members keg formula version keg_dir
  local license download license_file license_files license_list header text

  # Callers test this function in a condition, where errexit does not apply,
  # so every write checks its own status.
  MACOS_PACKAGE_POLICY_REASON="could not write third-party notices into ${resources_dir}"
  rm -rf "$licenses" "$notices" || return 1

  for source in ${MACOS_BUNDLED_BINARY_SOURCES[@]+"${MACOS_BUNDLED_BINARY_SOURCES[@]}"}; do
    if ! macos_homebrew_keg "$source" "$cellar"; then
      MACOS_PACKAGE_POLICY_REASON="no Homebrew keg owns bundled binary ${source}"
      return 1
    fi
    [[ "$kegs" == *$'\n'"${MACOS_HOMEBREW_KEG}"$'\n'* ]] || kegs+="${MACOS_HOMEBREW_KEG}"$'\n'
  done
  for tree in ${MACOS_BUNDLED_DATA_SOURCES[@]+"${MACOS_BUNDLED_DATA_SOURCES[@]}"}; do
    [[ -e "$tree" ]] || continue
    if ! members="$("${MACOS_FIND_COMMAND:-find}" "$tree" \( -type l -o -type f \) -print)"; then
      MACOS_PACKAGE_POLICY_REASON="could not enumerate bundled data source ${tree}"
      return 2
    fi
    while IFS= read -r member; do
      [[ -n "$member" ]] || continue
      macos_homebrew_keg "$member" "$cellar" || continue
      [[ "$kegs" == *$'\n'"${MACOS_HOMEBREW_KEG}"$'\n'* ]] || kegs+="${MACOS_HOMEBREW_KEG}"$'\n'
    done <<< "$members"
  done

  MACOS_PACKAGE_POLICY_REASON="could not write third-party notices into ${resources_dir}"
  mkdir -p "${licenses}/common" || return 1
  cp "${repository_root}/LICENSE" "${licenses}/common/GPL-3.0.txt" || return 1
  for text in "${repository_root}/build-aux/packaging/licenses/"*.txt; do
    cp "$text" "${licenses}/common/" || return 1
  done

  header="$(cat "${repository_root}/build-aux/packaging/THIRD-PARTY-NOTICES.header.txt")" \
    || return 1
  header="${header//@PACKAGER@/Homebrew (https://brew.sh)}"
  header="${header//@RECIPES@/https://github.com/Homebrew/homebrew-core}"
  printf '%s\n' "$header" > "$notices" || return 1

  while IFS= read -r keg; do
    [[ -n "$keg" ]] || continue
    formula="${keg%%/*}"
    version="${keg#*/}"
    keg_dir="${cellar}/${keg}"
    license="$(macos_keg_sbom_value "${keg_dir}/sbom.spdx.json" licenseConcluded)"
    if [[ -z "$license" || "$license" == NOASSERTION ]]; then
      license="$(macos_keg_sbom_value "${keg_dir}/sbom.spdx.json" licenseDeclared)"
    fi
    [[ -n "$license" && "$license" != NOASSERTION ]] || license="not recorded in the keg"
    download="$(macos_keg_sbom_value "${keg_dir}/sbom.spdx.json" downloadLocation)"
    [[ -n "$download" && "$download" != NOASSERTION ]] \
      || download="https://formulae.brew.sh/formula/${formula}"

    # Homebrew installs a formula's top-level license files into its keg.
    if ! license_files="$("${MACOS_FIND_COMMAND:-find}" "$keg_dir" -maxdepth 1 -type f \( \
      -iname 'COPYING*' -o -iname 'LICENSE*' -o -iname 'LICENCE*' \
      -o -iname 'NOTICE*' -o -iname 'COPYRIGHT*' \) -print)"; then
      MACOS_PACKAGE_POLICY_REASON="could not enumerate license files in keg ${keg}"
      return 2
    fi
    license_list=""
    while IFS= read -r license_file; do
      [[ -n "$license_file" ]] || continue
      mkdir -p "${licenses}/${formula}" || return 1
      cp "$license_file" "${licenses}/${formula}/" || return 1
      license_list+="${license_list:+, }licenses/${formula}/$(basename -- "$license_file")"
    done <<< "$(printf '%s\n' "$license_files" | LC_ALL=C sort)"
    [[ -n "$license_list" ]] || license_list="none shipped by the keg; see its source"

    printf '\n%s %s\n  License: %s\n  Source: %s\n  Build recipe: %s\n  License files: %s\n' \
      "$formula" "$version" "$license" "$download" \
      "https://formulae.brew.sh/formula/${formula}" "$license_list" >> "$notices" || return 1
  done <<< "$(printf '%s' "$kegs" | LC_ALL=C sort)"
  MACOS_PACKAGE_POLICY_REASON=""
  return 0
}

# Fail closed unless the app holds only allowlisted plugins and its
# third-party notices, license texts, and source offer.
macos_validate_release_contents() {
  local app_bundle="$1"
  local resources="${app_bundle}/Contents/Resources"
  if ! macos_validate_gstreamer_plugin_allowlist "${resources}/lib/gstreamer-1.0"; then
    return 1
  fi
  if [[ ! -f "${resources}/licenses/common/GPL-3.0.txt" ]] \
    || ! grep -q 'Source code offer' "${resources}/THIRD-PARTY-NOTICES.txt" 2>/dev/null; then
    MACOS_PACKAGE_POLICY_REASON="macOS bundle is missing its third-party notices or license texts"
    return 1
  fi
  return 0
}
