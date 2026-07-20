#!/bin/sh
# Validate the least-privilege filesystem and GVfs policy in Tributary's
# Flatpak manifest. This intentionally checks policy, not general YAML syntax.

set -eu
set -f

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
manifest=${1:-"$script_dir/io.github.tributary.Tributary.yml"}

if [ ! -f "$manifest" ]; then
    echo "Flatpak manifest not found: $manifest" >&2
    exit 2
fi

fail()
{
    echo "Flatpak permission policy violation: $*" >&2
    exit 1
}

# Accept only the manifest's canonical, unquoted one-argument list form. This
# keeps quoted values, inline comments, continuations, and other YAML spellings
# from bypassing the exact allowlist below.
finish_args=$(awk '
    /^[^[:space:]#]/ && $0 !~ /^[a-z][a-z0-9-]*:/ {
        print "Flatpak permission policy violation: noncanonical top-level YAML form: " $0 > "/dev/stderr"
        exit 2
    }
    /^[^[:space:]#].*finish-args/ && $0 != "finish-args:" {
        print "Flatpak permission policy violation: noncanonical finish-args key: " $0 > "/dev/stderr"
        exit 2
    }
    /^finish-args:$/ {
        finish_args_blocks++
        if (finish_args_blocks != 1) {
            print "Flatpak permission policy violation: duplicate finish-args block" > "/dev/stderr"
            exit 2
        }
        found_finish_args = 1
        in_finish_args = 1
        next
    }
    in_finish_args && /^[^[:space:]#]/ { in_finish_args = 0 }
    in_finish_args && /^[[:space:]]*#/ { next }
    in_finish_args && /^[[:space:]]*$/ { next }
    in_finish_args {
        if ($0 !~ /^  - --[^[:space:]#]+$/) {
            print "Flatpak permission policy violation: noncanonical finish-args entry: " $0 > "/dev/stderr"
            exit 2
        }
        sub(/^  - /, "")
        print
    }
    END {
        if (!found_finish_args || finish_args_blocks != 1) {
            print "Flatpak permission policy violation: missing canonical finish-args block" > "/dev/stderr"
            exit 2
        }
    }
' "$manifest")

require_once()
{
    count=$(printf '%s\n' "$finish_args" | grep -Fxc -- "$1" || true)
    [ "$count" -eq 1 ] || fail "expected exactly one '$1' entry (found $count)"
}

require_once "--socket=wayland"
require_once "--socket=fallback-x11"
require_once "--share=ipc"
require_once "--socket=pulseaudio"
require_once "--share=network"
require_once "--filesystem=xdg-music:rw"
require_once "--filesystem=/media:ro"
require_once "--filesystem=/run/media:ro"
require_once "--filesystem=/mnt:ro"
require_once "--talk-name=org.gtk.vfs.*"
require_once "--talk-name=org.freedesktop.secrets"
require_once "--own-name=org.mpris.MediaPlayer2.tributary"
require_once "--filesystem=xdg-data/themes:ro"
require_once "--filesystem=xdg-data/icons:ro"

# Every finish argument is reviewed here. Adding any filesystem, device, or
# bus grant therefore fails CI until this allowlist and its rationale change in
# the same review.
entry_count=0
for entry in $finish_args; do
    case "$entry" in
        "--socket=wayland" | \
        "--socket=fallback-x11" | \
        "--share=ipc" | \
        "--socket=pulseaudio" | \
        "--share=network" | \
        "--filesystem=xdg-music:rw" | \
        "--filesystem=/media:ro" | \
        "--filesystem=/run/media:ro" | \
        "--filesystem=/mnt:ro" | \
        "--talk-name=org.gtk.vfs.*" | \
        "--talk-name=org.freedesktop.secrets" | \
        "--own-name=org.mpris.MediaPlayer2.tributary" | \
        "--filesystem=xdg-data/themes:ro" | \
        "--filesystem=xdg-data/icons:ro")
            entry_count=$((entry_count + 1))
            ;;
        *)
            fail "unreviewed finish argument '$entry'"
            ;;
    esac
done

[ "$entry_count" -eq 14 ] || fail "expected exactly 14 reviewed finish arguments (found $entry_count)"

echo "Flatpak permission policy is valid: $manifest"
