#!/usr/bin/env bash
# Deterministic positive/negative coverage for the Linux payload scanner. No
# real package manager, public network, media runtime, or forbidden component
# is required.

set -euo pipefail
set -f

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
validator="$script_dir/validate-package-compliance.sh"
policy="$repository_root/build-aux/packaging/forbidden-bundled-components.txt"
temp_dir=$(mktemp -d)
trap 'rm -rf "$temp_dir"' EXIT HUP INT TERM

expect_status()
{
    expected=$1
    shift
    set +e
    "$@" > /dev/null 2>&1
    actual=$?
    set -e
    [ "$actual" -eq "$expected" ] || {
        echo "Expected status $expected, got $actual: $*" >&2
        exit 1
    }
}

first_token=$(awk '
    /^[[:space:]]*#/ { next }
    /^[[:space:]]*$/ { next }
    { gsub(/^[[:space:]]+|[[:space:]]+$/, ""); print tolower($0); exit }
' "$policy")
[ -n "$first_token" ] || {
    echo "Shared bundled-component policy unexpectedly empty" >&2
    exit 1
}

# Ordinary codecs, generic crypto, and similarly prefixed but unrelated names
# remain eligible. The shared list is deliberately the only negative source.
mkdir -p "$temp_dir/allowed/lib/gstreamer-1.0"
touch "$temp_dir/allowed/lib/gstreamer-1.0/libgstlibav.so"
touch "$temp_dir/allowed/lib/libavcodec.so.62"
touch "$temp_dir/allowed/lib/libcrypto.so.3"
touch "$temp_dir/allowed/lib/libblurhash.so"
"$validator" --tree "$temp_dir/allowed"

mkdir -p "$temp_dir/rejected"
while IFS= read -r line || [ -n "$line" ]; do
    token=$(printf '%s' "$line" | tr -d '\r')
    token=$(printf '%s' "$token" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
    case "$token" in
        '' | \#*) continue ;;
    esac
    candidate="$temp_dir/rejected/prefix-${token}-suffix.so"
    touch "$candidate"
    expect_status 1 "$validator" --tree "$temp_dir/rejected"
    rm -f "$candidate"
done < "$policy"

uppercase=$(printf '%s' "$first_token" | tr '[:lower:]' '[:upper:]')
touch "$temp_dir/rejected/${uppercase}.SO"
expect_status 1 "$validator" --tree "$temp_dir/rejected"
rm -f "$temp_dir/rejected/${uppercase}.SO"

ln -s "../${first_token}.so" "$temp_dir/rejected/innocent-link.so"
expect_status 1 "$validator" --tree "$temp_dir/rejected"
rm -f "$temp_dir/rejected/innocent-link.so"

ln -s "../${first_token}/libinnocent.so" "$temp_dir/rejected/innocent-link.so"
expect_status 1 "$validator" --tree "$temp_dir/rejected"
rm -f "$temp_dir/rejected/innocent-link.so"

# A traversal error after producing partial output must fail the scan. This is
# the regression case that process substitution would otherwise conceal.
mkdir -p "$temp_dir/find-tools"
printf '%s\n' \
    '#!/bin/sh' \
    'printf "%s\\0" "$1/lib/gstreamer-1.0/libgstlibav.so"' \
    'exit 7' \
    > "$temp_dir/find-tools/find"
chmod +x "$temp_dir/find-tools/find"
expect_status 1 env PATH="$temp_dir/find-tools:$PATH" \
    "$validator" --tree "$temp_dir/allowed"

# A partial magic read followed by an od error must fail even during a tree
# scan; it must not silently reclassify the regular file as non-ELF.
mkdir -p "$temp_dir/od-tools"
printf '%s\n' \
    '#!/bin/sh' \
    'printf " 7f 45"' \
    'exit 7' \
    > "$temp_dir/od-tools/od"
chmod +x "$temp_dir/od-tools/od"
expect_status 1 env PATH="$temp_dir/od-tools:$PATH" \
    "$validator" --tree "$temp_dir/allowed"

printf 'depends = gstreamer1.0-plugins-good\n' > "$temp_dir/allowed-metadata"
"$validator" --metadata "$temp_dir/allowed-metadata"
printf 'depends = %s-runtime\n' "$first_token" > "$temp_dir/rejected-metadata"
expect_status 1 "$validator" --metadata "$temp_dir/rejected-metadata"

# Tokenizer failure after partial allowed output must not become a policy pass.
real_tr=$(command -v tr)
mkdir -p "$temp_dir/tr-tools"
printf '%s\n' \
    '#!/bin/sh' \
    'if [ "$1" = -s ]; then' \
    '  echo gstreamer1.0-plugins-good' \
    '  exit 7' \
    'fi' \
    'exec "$TEST_REAL_TR" "$@"' \
    > "$temp_dir/tr-tools/tr"
chmod +x "$temp_dir/tr-tools/tr"
expect_status 1 env PATH="$temp_dir/tr-tools:$PATH" TEST_REAL_TR="$real_tr" \
    "$validator" --metadata "$temp_dir/allowed-metadata"

# Drive ELF dependency parsing through a fixed fake inspector. This covers a
# renamed payload whose basename is harmless but DT_NEEDED is prohibited.
mkdir -p "$temp_dir/tools"
printf '%s\n' \
    '#!/bin/sh' \
    'last=' \
    'for argument in "$@"; do last=$argument; done' \
    'case "$last" in' \
    '  *rejected-elf) dependency=${TEST_FORBIDDEN_TOKEN}.so.1 ;;' \
    '  *) dependency=libgstreamer-1.0.so.0 ;;' \
    'esac' \
    'printf " 0x0000000000000001 (NEEDED) Shared library: [%s]\\n" "$dependency"' \
    > "$temp_dir/tools/readelf"
chmod +x "$temp_dir/tools/readelf"
printf '\177ELFfixture' > "$temp_dir/allowed-elf"
printf '\177ELFfixture' > "$temp_dir/rejected-elf"
PATH="$temp_dir/tools:$PATH" TEST_FORBIDDEN_TOKEN="$first_token" \
    "$validator" --elf "$temp_dir/allowed-elf"
expect_status 1 env PATH="$temp_dir/tools:$PATH" TEST_FORBIDDEN_TOKEN="$first_token" \
    "$validator" --elf "$temp_dir/rejected-elf"

# Exercise each native archive boundary with deterministic fake package tools.
# The validator must reject both a declared dependency and a file introduced
# only while extracting the completed package payload.
mkdir -p "$temp_dir/archive-tools"
printf '%s\n' \
    '#!/bin/sh' \
    'case "$1" in' \
    '  --control)' \
    '    destination=$3' \
    '    mkdir -p "$destination"' \
    '    printf "Package: tributary\\nDepends: gstreamer1.0-plugins-good\\n" > "$destination/control"' \
    '    printf "#!/bin/sh\\n/sbin/ldconfig\\n" > "$destination/postinst"' \
    '    case "${TEST_ARCHIVE_MODE:-allowed}" in' \
    '      forbidden-dependency)' \
    '        printf "Recommends: %s-runtime\\n" "$TEST_FORBIDDEN_TOKEN" >> "$destination/control"' \
    '        ;;' \
    '      forbidden-script)' \
    '        printf "curl https://example.invalid/%s/install.sh\\n" "$TEST_FORBIDDEN_TOKEN" >> "$destination/postinst"' \
    '        ;;' \
    '      binary-control)' \
    '        printf "\\000\\001binary" > "$destination/blob"' \
    '        ;;' \
    '    esac' \
    '    ;;' \
    '  --extract)' \
    '    destination=$3' \
    '    mkdir -p "$destination/usr/lib"' \
    '    if [ "${TEST_ARCHIVE_MODE:-allowed}" = forbidden-payload ]; then' \
    '      touch "$destination/usr/lib/${TEST_FORBIDDEN_TOKEN}.so"' \
    '    else' \
    '      touch "$destination/usr/lib/libgstlibav.so"' \
    '    fi' \
    '    ;;' \
    '  *) exit 2 ;;' \
    'esac' \
    > "$temp_dir/archive-tools/dpkg-deb"
printf '%s\n' \
    '#!/bin/sh' \
    'case "$2:${TEST_ARCHIVE_MODE:-allowed}" in' \
    '  --recommends:forbidden-dependency)' \
    '    echo "${TEST_FORBIDDEN_TOKEN}-runtime"' \
    '    ;;' \
    '  --scripts:forbidden-script)' \
    '    echo "curl https://example.invalid/${TEST_FORBIDDEN_TOKEN}/install.sh"' \
    '    ;;' \
    '  --requires:*|--recommends:*|--suggests:*|--supplements:*|--enhances:*)' \
    '    echo gstreamer1-plugins-good' \
    '    ;;' \
    '  *) echo none ;;' \
    'esac' \
    > "$temp_dir/archive-tools/rpm"
printf '%s\n' '#!/bin/sh' 'exit 0' > "$temp_dir/archive-tools/rpm2cpio"
printf '%s\n' \
    '#!/bin/sh' \
    'mkdir -p usr/lib' \
    'if [ "${TEST_ARCHIVE_MODE:-allowed}" = forbidden-payload ]; then' \
    '  touch "usr/lib/${TEST_FORBIDDEN_TOKEN}.so"' \
    'else' \
    '  touch usr/lib/libgstlibav.so' \
    'fi' \
    > "$temp_dir/archive-tools/cpio"
printf '%s\n' \
    '#!/bin/sh' \
    'case "$1" in' \
    '  -xOf)' \
    '    if [ "${TEST_ARCHIVE_MODE:-allowed}" = forbidden-dependency ]; then' \
    '      echo "depend = ${TEST_FORBIDDEN_TOKEN}-runtime"' \
    '    else' \
    '      echo "depend = gst-plugins-good"' \
    '    fi' \
    '    ;;' \
    '  -xf)' \
    '    destination=$4' \
    '    mkdir -p "$destination/usr/lib"' \
    '    if [ "${TEST_ARCHIVE_MODE:-allowed}" = forbidden-payload ]; then' \
    '      touch "$destination/usr/lib/${TEST_FORBIDDEN_TOKEN}.so"' \
    '    else' \
    '      touch "$destination/usr/lib/libgstlibav.so"' \
    '    fi' \
    '    if [ "${TEST_ARCHIVE_MODE:-allowed}" = forbidden-script ]; then' \
    '      printf "curl https://example.invalid/%s/install.sh\\n" "$TEST_FORBIDDEN_TOKEN" > "$destination/.INSTALL"' \
    '    fi' \
    '    ;;' \
    '  *) exit 2 ;;' \
    'esac' \
    > "$temp_dir/archive-tools/bsdtar"
chmod +x \
    "$temp_dir/archive-tools/dpkg-deb" \
    "$temp_dir/archive-tools/rpm" \
    "$temp_dir/archive-tools/rpm2cpio" \
    "$temp_dir/archive-tools/cpio" \
    "$temp_dir/archive-tools/bsdtar"
touch "$temp_dir/fixture.deb" "$temp_dir/fixture.rpm" "$temp_dir/fixture.pkg.tar.zst"
for package_mode in deb rpm arch; do
    package="$temp_dir/fixture.$package_mode"
    [ "$package_mode" != arch ] || package="$temp_dir/fixture.pkg.tar.zst"
    PATH="$temp_dir/archive-tools:$PATH" TEST_FORBIDDEN_TOKEN="$first_token" \
        "$validator" "--$package_mode" "$package"
    archive_modes="forbidden-dependency forbidden-payload"
    case "$package_mode" in
        deb) archive_modes="$archive_modes forbidden-script binary-control" ;;
        rpm) archive_modes="$archive_modes forbidden-script" ;;
        arch) archive_modes="$archive_modes forbidden-script" ;;
    esac
    for archive_mode in $archive_modes; do
        expect_status 1 env PATH="$temp_dir/archive-tools:$PATH" \
            TEST_FORBIDDEN_TOKEN="$first_token" TEST_ARCHIVE_MODE="$archive_mode" \
            "$validator" "--$package_mode" "$package"
    done
done

# Exercise the complete Flatpak app-commit boundary without requiring
# Flatpak/OSTree on the unit-test host. The production workflow still imports
# and checks out the real completed bundle with those tools before upload.
mkdir -p "$temp_dir/flatpak-tools"
printf '%s\n' \
    '#!/bin/sh' \
    'exit 0' \
    > "$temp_dir/flatpak-tools/flatpak"
printf '%s\n' \
    '#!/bin/sh' \
    'operation=' \
    'last=' \
    'for argument in "$@"; do' \
    '  last=$argument' \
    '  case "$argument" in init|refs|checkout) operation=$argument ;; esac' \
    'done' \
    'case "$operation" in' \
    '  init) exit 0 ;;' \
    '  refs)' \
    '    if [ "${TEST_BUNDLE_MODE:-allowed}" = unexpected-ref ]; then' \
    '      echo runtime/org.example.Unexpected/x86_64/master' \
    '    else' \
    '      echo app/io.github.tributary.Tributary/x86_64/master' \
    '    fi' \
    '    ;;' \
    '  checkout)' \
    '    mkdir -p "$last/files" "$last/export/share/applications"' \
    '    touch "$last/files/libgstlibav.so"' \
    '    touch "$last/export/share/applications/io.github.tributary.Tributary.desktop"' \
    '    printf "[Application]\\nruntime=org.gnome.Platform/x86_64/49\\n" > "$last/metadata"' \
    '    case "${TEST_BUNDLE_MODE:-allowed}" in' \
    '      forbidden-export)' \
    '        touch "$last/export/share/applications/${TEST_FORBIDDEN_TOKEN}.desktop"' \
    '        ;;' \
    '      forbidden-metadata)' \
    '        printf "extension=%s-runtime\\n" "$TEST_FORBIDDEN_TOKEN" >> "$last/metadata"' \
    '        ;;' \
    '    esac' \
    '    ;;' \
    '  *) exit 2 ;;' \
    'esac' \
    > "$temp_dir/flatpak-tools/ostree"
chmod +x "$temp_dir/flatpak-tools/flatpak" "$temp_dir/flatpak-tools/ostree"
touch "$temp_dir/tributary.flatpak"
PATH="$temp_dir/flatpak-tools:$PATH" \
    "$repository_root/build-aux/flatpak/validate-bundle-compliance.sh" \
    "$temp_dir/tributary.flatpak" > /dev/null
for bundle_mode in forbidden-export forbidden-metadata; do
    expect_status 1 env PATH="$temp_dir/flatpak-tools:$PATH" \
        TEST_BUNDLE_MODE="$bundle_mode" TEST_FORBIDDEN_TOKEN="$first_token" \
        "$repository_root/build-aux/flatpak/validate-bundle-compliance.sh" \
        "$temp_dir/tributary.flatpak"
done
expect_status 1 env PATH="$temp_dir/flatpak-tools:$PATH" \
    TEST_BUNDLE_MODE=unexpected-ref \
    "$repository_root/build-aux/flatpak/validate-bundle-compliance.sh" \
    "$temp_dir/tributary.flatpak"

# Missing, empty, malformed, and duplicate shared policy data are all setup
# failures, never an implicit allow-all fallback.
for fixture in missing empty malformed duplicate; do
    mkdir -p "$temp_dir/$fixture/build-aux/linux" "$temp_dir/$fixture/build-aux/packaging"
    cp "$validator" "$temp_dir/$fixture/build-aux/linux/validate-package-compliance.sh"
done
rm -f "$temp_dir/missing/build-aux/packaging/forbidden-bundled-components.txt"
: > "$temp_dir/empty/build-aux/packaging/forbidden-bundled-components.txt"
printf 'valid-token\nbad token\n' \
    > "$temp_dir/malformed/build-aux/packaging/forbidden-bundled-components.txt"
printf 'duplicate\nDUPLICATE\n' \
    > "$temp_dir/duplicate/build-aux/packaging/forbidden-bundled-components.txt"
for fixture in missing empty malformed duplicate; do
    expect_status 2 "$temp_dir/$fixture/build-aux/linux/validate-package-compliance.sh" \
        --tree "$temp_dir/allowed"
done

"$script_dir/validate-package-metadata.sh" > /dev/null

require_literal()
{
    needle=$1
    file=$2
    grep -Fq -- "$needle" "$file" || {
        echo "Missing Linux package compliance contract '$needle' in $file" >&2
        exit 1
    }
}

assert_before()
{
    earlier=$1
    later=$2
    file=$3
    earlier_line=$(grep -nF -- "$earlier" "$file" | cut -d: -f1)
    later_line=$(grep -nF -- "$later" "$file" | cut -d: -f1)
    case "$earlier_line:$later_line" in
        *$'\n'* | :* | *:)
            echo "Ordering contract is missing or ambiguous in $file: $earlier / $later" >&2
            exit 1
            ;;
    esac
    [ "$earlier_line" -lt "$later_line" ] || {
        echo "Compliance validation must precede artifact upload: $earlier / $later" >&2
        exit 1
    }
}

build_linux="$repository_root/scripts/build-linux.sh"
flatpak_manifest="$repository_root/build-aux/flatpak/io.github.tributary.Tributary.yml"
flatpak_validator="$repository_root/build-aux/flatpak/validate-bundle-compliance.sh"
ci_workflow="$repository_root/.github/workflows/ci.yml"
release_workflow="$repository_root/.github/workflows/release.yml"
arch_pkgbuild="$repository_root/build-aux/arch/PKGBUILD"
rpm_spec="$repository_root/build-aux/rpm/tributary.spec"

require_literal 'validate-package-compliance.sh --tree /app' "$flatpak_manifest"
require_literal '"$validator" --metadata "$checkout/metadata"' "$flatpak_validator"
require_literal '"$validator" --tree "$checkout"' "$flatpak_validator"
require_literal 'LC_ALL=C "$inspector" -d -- "$file"' "$validator"
require_literal '"$PACKAGE_VALIDATOR" --elf target/release/tributary' "$build_linux"
require_literal 'require_elf_inspector' "$build_linux"
require_literal 'require_validator_tool ostree' "$build_linux"
require_literal 'require_validator_tool dpkg-deb' "$build_linux"
require_literal 'require_validator_tool rpm' "$build_linux"
require_literal 'require_validator_tool rpm2cpio' "$build_linux"
require_literal 'require_validator_tool cpio' "$build_linux"
require_literal 'require_validator_tool bsdtar' "$build_linux"
assert_before 'preflight_artifact_tools # fail before build work' \
    '# ── Rust Build' "$build_linux"
require_literal '"$PACKAGE_VALIDATOR" --deb "$DEB_FILE"' "$build_linux"
require_literal '"$PACKAGE_VALIDATOR" --rpm "$RPM_FILE"' "$build_linux"
require_literal '"$PACKAGE_VALIDATOR" --arch "dist/$PKG_FILE"' "$build_linux"
require_literal '"$FLATPAK_BUNDLE_VALIDATOR" tributary.flatpak' "$build_linux"
require_literal 'validate-package-compliance.sh --elf target/release/tributary' "$arch_pkgbuild"
require_literal 'validate-package-compliance.sh --tree "$pkgdir"' "$arch_pkgbuild"
require_literal 'validate-package-compliance.sh --elf target/release/tributary' "$rpm_spec"
require_literal 'validate-package-compliance.sh --tree "%{buildroot}"' "$rpm_spec"
assert_before 'validate-package-compliance.sh --tree "%{buildroot}"' \
    '%check' "$rpm_spec"

require_literal 'build-aux/linux/test-package-compliance.sh' "$ci_workflow"
require_literal 'scripts/test-macos-package-policy.sh' "$ci_workflow"
require_literal 'validate-package-compliance.sh --elf target/release/tributary' "$ci_workflow"
require_literal 'command -v ostree' "$ci_workflow"
require_literal 'command -v readelf || command -v eu-readelf' "$ci_workflow"
assert_before 'name: Validate completed Flatpak bundle payload' \
    'name: Upload Flatpak bundle' "$ci_workflow"

require_literal 'flatpak flatpak-builder elfutils ostree python3-pip' "$release_workflow"
require_literal 'cpio elfutils gcc rpm-build' "$release_workflow"
require_literal 'binutils git libarchive' "$release_workflow"
require_literal 'validate-bundle-compliance.sh' "$release_workflow"
require_literal 'validate-package-compliance.sh --deb' "$release_workflow"
require_literal 'validate-package-compliance.sh --rpm' "$release_workflow"
require_literal 'validate-package-compliance.sh --arch' "$release_workflow"
assert_before 'name: Validate completed Flatpak bundle payload' \
    'name: Upload Flatpak as workflow artifact' "$release_workflow"
assert_before 'name: Validate completed .deb payload' \
    'name: Upload .deb as workflow artifact' "$release_workflow"
assert_before 'name: Validate completed .rpm payload' \
    'name: Upload .rpm as workflow artifact' "$release_workflow"
assert_before 'name: Validate completed Arch package payload' \
    'name: Upload Arch package as workflow artifact' "$release_workflow"

echo "Linux package compliance positive and negative tests passed"
