#!/bin/sh
# Pin the inputs which can add Linux payloads or direct package dependencies.
# Ordinary distro GStreamer packages stay permitted; the shared policy is
# applied only to explicit component names in these reviewed build inputs.

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)
validator="$script_dir/validate-package-compliance.sh"

"$validator" --metadata \
    "$repository_root/Cargo.toml" \
    "$repository_root/Cargo.lock" \
    "$repository_root/build-aux/flatpak/io.github.tributary.Tributary.yml" \
    "$repository_root/build-aux/arch/PKGBUILD" \
    "$repository_root/build-aux/rpm/tributary.spec"

echo "Linux packaging inputs contain no forbidden direct components"
