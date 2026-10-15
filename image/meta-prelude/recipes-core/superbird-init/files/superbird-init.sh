#!/bin/sh
# Re-render device metadata and preserve the efuse-derived serial number on the
# writable data partition. SSH host-key persistence remains provided by the
# parent recipe.

set -eu

TEMPLATE="/usr/share/superbird/meta.json.in"
META_DIR="/var/lib/superbird"
META_PATH="$META_DIR/meta.json"
EFUSE_CELLS="/sys/bus/nvmem/devices/efuse0/cells"

mkdir -p "$META_DIR"
cp "$TEMPLATE" "$META_PATH"
chmod 0644 "$META_PATH"

SERIAL_FILE=""
for candidate in "$EFUSE_CELLS"/serial-number@*; do
    [ -r "$candidate" ] || continue
    SERIAL_FILE="$candidate"
    break
done
if [ -z "$SERIAL_FILE" ]; then
    echo "superbird-init: serial-number efuse cell not present, leaving metadata unpatched" >&2
    exit 0
fi

full_serial=$(cat "$SERIAL_FILE")
case "$full_serial" in
    ""|*[!A-Za-z0-9._-]*)
        echo "superbird-init: serial-number efuse cell is invalid, leaving metadata unpatched" >&2
        exit 0
        ;;
esac

sed -i "s|\"serialNumber\": \"\"|\"serialNumber\": \"$full_serial\"|" "$META_PATH"
