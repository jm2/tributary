#!/bin/sh
# Import the completed single-file bundle into an isolated OSTree repository
# and inspect the complete app commit: its /app files, exported metadata/assets,
# and commit metadata. Flatpak bundles omit the referenced platform runtime, so
# ordinary runtime codecs remain outside Tributary's app-owned boundary.

set -eu
set -f

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
validator="$script_dir/../linux/validate-package-compliance.sh"
bundle=${1:-}

if [ -z "$bundle" ] || [ "$#" -ne 1 ]; then
    echo "Usage: validate-bundle-compliance.sh FILE.flatpak" >&2
    exit 2
fi
if [ ! -f "$bundle" ]; then
    echo "Flatpak bundle not found: $bundle" >&2
    exit 2
fi
for command in flatpak ostree; do
    command -v "$command" >/dev/null 2>&1 || {
        echo "Flatpak bundle validator requires '$command'" >&2
        exit 2
    }
done

temp_dir=$(mktemp -d)
trap 'rm -rf "$temp_dir"' EXIT HUP INT TERM
repository="$temp_dir/repository"
checkout="$temp_dir/checkout"
refs="$temp_dir/refs"

ostree --repo="$repository" init --mode=bare-user-only
flatpak build-import-bundle "$repository" "$bundle" >/dev/null
ostree --repo="$repository" refs > "$refs"

app_ref=
ref_count=0
while IFS= read -r ref || [ -n "$ref" ]; do
    [ -n "$ref" ] || continue
    case "$ref" in
        app/io.github.tributary.Tributary/*/*)
            ref_count=$((ref_count + 1))
            app_ref=$ref
            ;;
        *)
            echo "Flatpak bundle contains an unexpected ref: $ref" >&2
            exit 1
            ;;
    esac
done < "$refs"

[ "$ref_count" -eq 1 ] || {
    echo "Flatpak bundle must contain exactly one Tributary app ref (found $ref_count)" >&2
    exit 1
}

ostree --repo="$repository" checkout "$app_ref" "$checkout"
[ -d "$checkout/files" ] || {
    echo "Flatpak app commit does not contain a files payload" >&2
    exit 1
}
[ -f "$checkout/metadata" ] || {
    echo "Flatpak app commit does not contain application metadata" >&2
    exit 1
}
"$validator" --metadata "$checkout/metadata"
"$validator" --tree "$checkout"

echo "Flatpak app commit complies with bundled-component policy: $bundle"
