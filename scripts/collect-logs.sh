#!/usr/bin/env bash
set -euo pipefail

boot="${1:--1}"
if [[ ! "$boot" =~ ^-?[0-9]+$ ]]; then
    echo "usage: $0 [boot offset, default -1 for the previous boot]" >&2
    exit 2
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if command -v adb >/dev/null 2>&1; then
    adb="adb"
elif [[ -x "$script_dir/adb" ]]; then
    adb="$script_dir/adb"
else
    echo "adb was not found. Install Android platform-tools and make sure adb is on your PATH." >&2
    exit 1
fi

echo "Waiting for the Car Thing (30 s)..."
for _ in $(seq 30); do
    if [[ "$("$adb" get-state 2>/dev/null)" == "device" ]]; then
        break
    fi
    sleep 1
done
if [[ "$("$adb" get-state 2>&1)" != "device" ]]; then
    echo "No Car Thing found over adb. Make sure it booted into recovery mode and is the only adb device connected." >&2
    "$adb" devices -l >&2 || true
    exit 1
fi

timestamp="$(date +%Y-%m-%d_%H-%M-%S)"
output="$PWD/prelude-logs-$timestamp.txt"
remote_script="/tmp/prelude-collect-logs.sh"
remote_output="/tmp/prelude-logs.txt"
local_script="$(mktemp)"
trap 'rm -f "$local_script"' EXIT

cat > "$local_script" <<'REMOTE'
boot="$1"
collected_at="$2"

section() {
    printf '\n===== %s =====\n' "$1"
}

run() {
    section "$*"
    "$@" 2>&1
}

{
    printf 'Prelude logs\ncollected at (host time): %s\nboot offset: %s\n' "$collected_at" "$boot"
    run date -u
    run cat /etc/os-release
    run uname -a
    run cat /proc/cmdline
    run uptime
    run journalctl --list-boots --no-pager
    run preludectl doctor --json

    section "settings"
    for file in /var/lib/prelude/settings.toml /var/lib/prelude/audio-route; do
        printf -- '--- %s ---\n' "$file"
        cat "$file" 2>&1
    done

    section "/etc/prelude/config-dev.toml (passphrase redacted)"
    sed 's/^\(passphrase[[:space:]]*=\).*/\1 "<redacted>"/' /etc/prelude/config-dev.toml 2>&1

    section "crash dumps (pstore)"
    for dir in /var/lib/systemd/pstore /sys/fs/pstore; do
        [ -d "$dir" ] || continue
        find "$dir" -type f | sort | while read -r file; do
            printf -- '--- %s ---\n' "$file"
            cat "$file" 2>&1
        done
    done

    run journalctl -b "$boot" -o short-precise --no-pager
    run systemctl --failed --no-pager
    run df -h
    run free
    run ls -laR /var/lib/prelude
    run ls -laR /var/lib/bluetooth
} > /tmp/prelude-logs.txt 2>&1
exit 0
REMOTE

echo "Collecting logs..."
"$adb" push "$local_script" "$remote_script" >/dev/null
"$adb" shell sh "$remote_script" "$boot" "'$(date '+%Y-%m-%d %H:%M:%S %z')'"
"$adb" pull "$remote_output" "$output" >/dev/null
"$adb" shell rm -f "$remote_script" "$remote_output"

echo "Saved $output"
