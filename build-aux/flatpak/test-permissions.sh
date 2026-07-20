#!/bin/sh
# Exercise both sides of the Flatpak finish-args contract. In particular, keep
# alternate YAML spellings from bypassing the exact allowlist.

set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
manifest="$script_dir/io.github.tributary.Tributary.yml"
validator="$script_dir/validate-permissions.sh"
temp_dir=$(mktemp -d)
trap 'rm -rf "$temp_dir"' EXIT HUP INT TERM

"$validator" "$manifest" >/dev/null

insert_and_reject()
{
    name=$1
    entry=$2
    fixture="$temp_dir/$name.yml"

    awk -v entry="$entry" '
        { print }
        $0 == "  - --filesystem=/mnt:ro" {
            print entry
            inserted = 1
        }
        END { if (!inserted) exit 2 }
    ' "$manifest" > "$fixture"

    if "$validator" "$fixture" >/dev/null 2>&1; then
        echo "Flatpak permission negative test unexpectedly passed: $name" >&2
        exit 1
    fi
}

append_block_and_reject()
{
    name=$1
    key=$2
    fixture="$temp_dir/$name.yml"

    cp "$manifest" "$fixture"
    printf '\n%s\n  - --filesystem=host:rw\n' "$key" >> "$fixture"
    if "$validator" "$fixture" >/dev/null 2>&1; then
        echo "Flatpak permission negative test unexpectedly passed: $name" >&2
        exit 1
    fi
}

replace_and_reject()
{
    name=$1
    old_entry=$2
    new_entry=$3
    fixture="$temp_dir/$name.yml"

    awk -v old_entry="$old_entry" -v new_entry="$new_entry" '
        $0 == old_entry {
            print new_entry
            replaced = 1
            next
        }
        { print }
        END { if (!replaced) exit 2 }
    ' "$manifest" > "$fixture"
    if "$validator" "$fixture" >/dev/null 2>&1; then
        echo "Flatpak permission negative test unexpectedly passed: $name" >&2
        exit 1
    fi
}

insert_and_reject quoted-home '  - "--filesystem=home:ro"'
insert_and_reject commented-host '  - --filesystem=host:rw # hidden from a naive parser'
insert_and_reject arbitrary-root '  - --filesystem=/etc:rw'
insert_and_reject arbitrary-xdg-root '  - --filesystem=xdg-documents:rw'
insert_and_reject writable-media '  - --filesystem=/media:rw'
insert_and_reject raw-device '  - --device=all'
insert_and_reject system-bus '  - --system-talk-name=org.freedesktop.UDisks2'
insert_and_reject session-bus '  - --talk-name=org.example.Unreviewed'
insert_and_reject broad-secret-bus '  - --talk-name=org.freedesktop.*'
append_block_and_reject duplicate-block 'finish-args:'
append_block_and_reject quoted-key '"finish-args":'
append_block_and_reject escaped-key '"finish\u002dargs":'
replace_and_reject missing-plus-duplicate \
    '  - --socket=wayland' '  - --share=ipc'

echo "Flatpak permission policy positive and negative tests passed"
