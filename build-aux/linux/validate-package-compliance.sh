#!/usr/bin/env bash
# Linux artifact policy: Tributary may use ordinary distro/runtime codecs, but
# its own payload must not bundle or link dedicated optical-disc copy-control
# decrypt components. This is intentionally Linux-only; other platforms own
# their independent package layouts and validation.

set -euo pipefail
set -f

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
policy_file="$repository_root/build-aux/packaging/forbidden-bundled-components.txt"

usage()
{
    cat >&2 <<'EOF'
Usage:
  validate-package-compliance.sh --tree DIRECTORY
  validate-package-compliance.sh --elf FILE
  validate-package-compliance.sh --deb FILE.deb
  validate-package-compliance.sh --rpm FILE.rpm
  validate-package-compliance.sh --arch FILE.pkg.tar.zst
  validate-package-compliance.sh --metadata FILE...
EOF
    exit 2
}

fail()
{
    echo "Linux package compliance violation: $*" >&2
    exit 1
}

require_command()
{
    command -v "$1" >/dev/null 2>&1 || {
        echo "Linux package compliance validator requires '$1'" >&2
        exit 2
    }
}

load_policy()
{
    [ -f "$policy_file" ] || {
        echo "Required bundled-component policy is missing: $policy_file" >&2
        exit 2
    }

    policy_tokens=
    while IFS= read -r line || [ -n "$line" ]; do
        token=$(printf '%s' "$line" | tr -d '\r')
        token=$(printf '%s' "$token" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
        case "$token" in
            '' | \#*) continue ;;
        esac
        case "$token" in
            [A-Za-z0-9]* ) ;;
            *)
                echo "Bundled-component policy contains an invalid filename token: $token" >&2
                exit 2
                ;;
        esac
        case "$token" in
            *[!A-Za-z0-9._+-]*)
                echo "Bundled-component policy contains an invalid filename token: $token" >&2
                exit 2
                ;;
        esac
        token=$(printf '%s' "$token" | tr '[:upper:]' '[:lower:]')
        for existing in $policy_tokens; do
            [ "$existing" != "$token" ] || {
                echo "Bundled-component policy contains a duplicate filename token: $token" >&2
                exit 2
            }
        done
        policy_tokens="${policy_tokens}${policy_tokens:+ }${token}"
    done < "$policy_file"
    [ -n "$policy_tokens" ] || {
        echo "Bundled-component policy contains no filename tokens: $policy_file" >&2
        exit 2
    }
}

forbidden_component()
{
    component=${1##*/}
    component=${component,,}
    for token in $policy_tokens; do
        case "$component" in
            *"$token"*) return 0 ;;
        esac
    done
    return 1
}

forbidden_path_component()
{
    path=${1,,}
    for token in $policy_tokens; do
        case "$path" in
            *"$token"*) return 0 ;;
        esac
    done
    return 1
}

check_component()
{
    forbidden_component "$1" && fail "prohibited component '$1'"
    return 0
}

check_path_components()
{
    forbidden_path_component "$1" && fail "prohibited component reference '$1'"
    return 0
}

check_dependency_text()
(
    input=$1
    # Inspect tokenized package relationships and plain-text installer
    # metadata, never arbitrary binary contents. The shared reviewed list is
    # the sole negative source; ordinary codecs and general-purpose crypto
    # remain eligible unless that policy changes.
    tokens=$(mktemp)
    trap 'rm -f "$tokens"' EXIT HUP INT TERM
    if ! LC_ALL=C tr -s '[:space:],()[]<>=:;|"' '\n' < "$input" > "$tokens"; then
        fail "could not tokenize dependency metadata: $input"
    fi
    while IFS= read -r token || [ -n "$token" ]; do
        [ -z "$token" ] || check_path_components "$token"
    done < "$tokens"
)

elf_inspector()
{
    if command -v readelf >/dev/null 2>&1; then
        printf '%s\n' readelf
    elif command -v eu-readelf >/dev/null 2>&1; then
        printf '%s\n' eu-readelf
    else
        echo "Linux package compliance validator requires readelf or eu-readelf" >&2
        exit 2
    fi
}

is_elf()
{
    [ -f "$1" ] || return 1
    if ! magic=$(LC_ALL=C od -An -tx1 -N4 -- "$1" 2>/dev/null | tr -d ' \n'); then
        return 2
    fi
    [ "$magic" = 7f454c46 ]
}

check_elf()
{
    file=$1
    required=${2:-false}
    if is_elf "$file"; then
        :
    else
        magic_status=$?
        [ "$magic_status" -eq 1 ] || fail "could not inspect file magic: $file"
        [ "$required" = false ] || fail "expected an ELF artifact: $file"
        return 0
    fi

    inspector=$(elf_inspector)
    dynamic=$(mktemp)
    if ! LC_ALL=C "$inspector" -d -- "$file" > "$dynamic" 2>/dev/null; then
        rm -f "$dynamic"
        fail "could not inspect ELF dependencies: $file"
    fi
    dependencies=$(mktemp)
    LC_ALL=C sed -n 's/.*Shared library: \[\([^]]*\)\].*/\1/p' "$dynamic" > "$dependencies"
    check_dependency_text "$dependencies"
    rm -f "$dynamic" "$dependencies"
}

check_entry()
{
    entry=$1
    check_component "$entry"
    if [ -L "$entry" ]; then
        target=$(readlink -- "$entry") || fail "could not inspect symlink: $entry"
        check_path_components "$target"
    elif [ -f "$entry" ]; then
        check_elf "$entry" false
    fi
}

check_tree()
(
    root=$1
    [ -d "$root" ] || {
        echo "Linux package payload directory not found: $root" >&2
        exit 2
    }

    # Process substitution hides find's exit status from the parent shell.
    # Enumerate first and inspect only after a complete, successful traversal
    # so unreadable or disappearing payload paths can never yield a partial
    # policy pass.
    entries=$(mktemp)
    trap 'rm -f "$entries"' EXIT HUP INT TERM
    if ! find "$root" -mindepth 1 -print0 > "$entries"; then
        fail "could not enumerate package payload tree: $root"
    fi
    while IFS= read -r -d '' entry; do
        check_entry "$entry"
    done < "$entries"
)

check_text_metadata_file()
{
    entry=$1
    [ -f "$entry" ] && [ ! -L "$entry" ] || \
        fail "package control metadata is not a regular file: $entry"
    # Installer metadata is defined as text. Reject an unexpected binary
    # member instead of substring-scanning arbitrary bytes as script tokens.
    if [ -s "$entry" ] && ! LC_ALL=C grep -Iq '' "$entry"; then
        fail "package control metadata is not plain text: $entry"
    fi
    check_dependency_text "$entry"
}

check_text_metadata_tree()
(
    root=$1
    entries=$(mktemp)
    trap 'rm -f "$entries"' EXIT HUP INT TERM
    if ! find "$root" -type f -print0 > "$entries"; then
        fail "could not enumerate package control metadata: $root"
    fi
    while IFS= read -r -d '' entry; do
        check_text_metadata_file "$entry"
    done < "$entries"
)

extract_deb()
{
    package=$1
    require_command dpkg-deb
    temp_dir=$(mktemp -d)
    trap 'rm -rf "$temp_dir"' EXIT HUP INT TERM
    dpkg-deb --control "$package" "$temp_dir/control" || \
        fail "could not extract Debian control metadata"
    check_tree "$temp_dir/control"
    check_text_metadata_tree "$temp_dir/control"
    dpkg-deb --extract "$package" "$temp_dir/payload" || fail "could not extract Debian package"
    check_tree "$temp_dir/payload"
}

extract_rpm()
{
    package=$1
    require_command rpm
    require_command rpm2cpio
    require_command cpio
    temp_dir=$(mktemp -d)
    trap 'rm -rf "$temp_dir"' EXIT HUP INT TERM
    : > "$temp_dir/header-metadata"
    for query in \
        --requires --recommends --suggests --supplements --enhances \
        --conflicts --obsoletes --provides \
        --scripts --triggers --filetriggers
    do
        rpm -qp "$query" "$package" >> "$temp_dir/header-metadata" 2>/dev/null || \
            fail "could not read RPM header metadata ($query)"
    done
    check_dependency_text "$temp_dir/header-metadata"
    rpm2cpio "$package" > "$temp_dir/payload.cpio" || fail "could not decode RPM payload"
    mkdir "$temp_dir/payload"
    (cd "$temp_dir/payload" && cpio -idm --quiet < "$temp_dir/payload.cpio") || \
        fail "could not extract RPM payload"
    check_tree "$temp_dir/payload"
}

extract_arch()
{
    package=$1
    require_command bsdtar
    temp_dir=$(mktemp -d)
    trap 'rm -rf "$temp_dir"' EXIT HUP INT TERM
    bsdtar -xOf "$package" .PKGINFO > "$temp_dir/pkginfo" || \
        fail "could not read Arch package metadata"
    check_dependency_text "$temp_dir/pkginfo"
    mkdir "$temp_dir/payload"
    bsdtar -xf "$package" -C "$temp_dir/payload" || fail "could not extract Arch package"
    check_tree "$temp_dir/payload"
    install_script="$temp_dir/payload/.INSTALL"
    if [ -e "$install_script" ] || [ -L "$install_script" ]; then
        check_text_metadata_file "$install_script"
    fi
}

load_policy

[ "$#" -ge 2 ] || usage
mode=$1
shift

case "$mode" in
    --tree)
        [ "$#" -eq 1 ] || usage
        check_tree "$1"
        ;;
    --elf)
        [ "$#" -eq 1 ] || usage
        [ -f "$1" ] || {
            echo "Linux ELF artifact not found: $1" >&2
            exit 2
        }
        check_component "$1"
        check_elf "$1" true
        ;;
    --deb)
        [ "$#" -eq 1 ] || usage
        [ -f "$1" ] || {
            echo "Debian artifact not found: $1" >&2
            exit 2
        }
        extract_deb "$1"
        ;;
    --rpm)
        [ "$#" -eq 1 ] || usage
        [ -f "$1" ] || {
            echo "RPM artifact not found: $1" >&2
            exit 2
        }
        extract_rpm "$1"
        ;;
    --arch)
        [ "$#" -eq 1 ] || usage
        [ -f "$1" ] || {
            echo "Arch artifact not found: $1" >&2
            exit 2
        }
        extract_arch "$1"
        ;;
    --metadata)
        [ "$#" -ge 1 ] || usage
        for metadata in "$@"; do
            [ -f "$metadata" ] || {
                echo "Linux packaging metadata not found: $metadata" >&2
                exit 2
            }
            check_dependency_text "$metadata"
        done
        ;;
    --entry)
        [ "$#" -eq 1 ] || usage
        check_entry "$1"
        ;;
    *)
        usage
        ;;
esac
