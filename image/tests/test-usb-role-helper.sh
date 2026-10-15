#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
helper="$root/meta-prelude/recipes-bsp/superbird-usb-role/files/superbird-usb-role"
fixtures="$root/tests/fixtures"
test_dir=$(mktemp -d)
trap 'rm -rf "$test_dir"' EXIT HUP INT TERM

role="$test_dir/role"
timer="$test_dir/timer-active"
printf 'device' > "$role"

export PRELUDE_ROLE_SWITCH_PATH="$role"
export PRELUDE_SYSTEMCTL="$fixtures/fake-systemctl"
export PRELUDE_SYSTEMD_RUN="$fixtures/fake-systemd-run"
export CT_TEST_TIMER_STATE="$timer"

if "$helper" to-host --revert-secs 30 >/dev/null 2>&1; then
    echo "unauthorized helper mutation unexpectedly succeeded" >&2
    exit 1
fi
[ "$(cat "$role")" = device ]

export PRELUDE_ROLE_CONTROLLER=1
export CT_TEST_SYSTEMD_RUN_FAIL=1
if "$helper" to-host --revert-secs 30 >/dev/null 2>&1; then
    echo "host transition succeeded after timer creation failed" >&2
    exit 1
fi
[ "$(cat "$role")" = device ]
[ ! -e "$timer" ]

export CT_TEST_SYSTEMD_RUN_FAIL=0
"$helper" to-host --revert-secs 30 >/dev/null
[ "$(cat "$role")" = host ]
[ -f "$timer" ]

"$helper" confirm >/dev/null
[ "$(cat "$role")" = host ]
[ ! -e "$timer" ]

"$helper" to-device >/dev/null
[ "$(cat "$role")" = device ]

invalid_role="$test_dir/not-a-role-file"
mkdir "$invalid_role"
export PRELUDE_ROLE_SWITCH_PATH="$invalid_role"
if "$helper" to-host --revert-secs 30 >/dev/null 2>&1; then
    echo "host transition succeeded with an invalid role switch" >&2
    exit 1
fi
[ ! -e "$timer" ]

echo "superbird-usb-role fault-injection checks passed"
