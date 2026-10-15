#!/bin/sh
set -eu

root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT INT TERM
image_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
helper=$image_dir/meta-prelude/recipes-core/prelude-topology/files/prelude-session-recover
calls=$root/calls
marker=$root/otg
failure=$root/last-wired-failure
blocked_failure=$failure.ambiguous-topology
devices=$root/devices
mkdir -p "$devices/3-1"
: >"$marker"
: >"$failure"
: >"$calls"
printf '%s\n' 05ac >"$devices/3-1/idVendor"
printf '%s\n' 12a8 >"$devices/3-1/idProduct"
printf '%s\n' iPhone >"$devices/3-1/product"
printf '%s\n' 6 >"$devices/3-1/bConfigurationValue"

cat >"$root/systemctl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >>"$CALLS"
if [ "${FAIL_START:-}" = 1 ] && [ "$*" = "start preluded.service" ]; then
	exit 1
fi
exit 0
EOF
chmod 0755 "$root/systemctl"

run_helper() {
	CALLS="$calls" \
		PRELUDE_SESSION_FAILURE_FILE="$failure" \
		PRELUDE_SESSION_USB_DEVICES="$devices" \
		PRELUDE_SESSION_SYSTEMCTL="$root/systemctl" \
		PRELUDE_SESSION_TOPOLOGY_MARKER="$marker" \
		FAIL_START="${FAIL_START:-}" \
		"$helper"
}

run_helper
expected='stop preluded.service
start preluded.service'
if [ "$(cat "$calls")" != "$expected" ]; then
	echo "unexpected successful direct-OTG recovery sequence" >&2
	exit 1
fi
if [ -e "$failure" ] || [ "$(cat "$devices/3-1/bConfigurationValue")" != 6 ]; then
	echo "successful direct-OTG recovery disturbed the stable USB configuration" >&2
	exit 1
fi

: >"$calls"
: >"$failure"
mkdir -p "$devices/3-2"
printf '%s\n' 05ac >"$devices/3-2/idVendor"
printf '%s\n' 110a >"$devices/3-2/idProduct"
printf '%s\n' 'USB Audio' >"$devices/3-2/product"
run_helper
if [ -e "$failure" ] || [ "$(cat "$calls")" != "$expected" ]; then
	echo "Apple USB audio adapter blocked iPhone recovery" >&2
	exit 1
fi

: >"$calls"
: >"$failure"
printf '%s\n' 030d >"$devices/3-2/idProduct"
printf '%s\n' Keyboard >"$devices/3-2/product"
run_helper
if [ -e "$failure" ] || [ "$(cat "$calls")" != "$expected" ]; then
	echo "Apple keyboard was misclassified as a second iPhone" >&2
	exit 1
fi

: >"$calls"
: >"$failure"
printf '%s\n' 12a8 >"$devices/3-2/idProduct"
printf '%s\n' iPad >"$devices/3-2/product"
run_helper
if [ -e "$failure" ] || [ "$(cat "$calls")" != "$expected" ]; then
	echo "iPad was misclassified as a second iPhone" >&2
	exit 1
fi

: >"$calls"
: >"$failure"
printf '%s\n' iPhone >"$devices/3-2/product"
if ! run_helper || [ -e "$failure" ] || [ ! -e "$blocked_failure" ] || [ -s "$calls" ]; then
	echo "ambiguous iPhone recovery did not preserve bounded evidence" >&2
	exit 1
fi
rm -rf "$devices/3-2"

: >"$calls"
: >"$failure"
mkdir -p "$devices/3-3"
printf '%s\n' 18d1 >"$devices/3-3/idVendor"
printf '%s\n' 2d00 >"$devices/3-3/idProduct"
if ! run_helper || [ -e "$failure" ] || [ ! -e "$blocked_failure" ] || [ -s "$calls" ]; then
	echo "mixed iPhone and Android Auto recovery did not fail closed" >&2
	exit 1
fi
rm -f "$devices/3-1/idVendor" "$devices/3-1/idProduct" "$devices/3-1/product" "$devices/3-1/bConfigurationValue"

: >"$calls"
: >"$failure"
rm -f "$blocked_failure"
run_helper
if [ -e "$failure" ] || [ "$(cat "$calls")" != "$expected" ]; then
	echo "single Android Auto accessory did not permit daemon recovery" >&2
	exit 1
fi
rm -rf "$devices/3-3"

: >"$calls"
: >"$failure"
if FAIL_START=1 run_helper || [ ! -e "$failure" ]; then
	echo "failed daemon restart did not preserve the recovery marker" >&2
	exit 1
fi
if [ "$(cat "$calls")" != "$expected" ]; then
	echo "failed daemon restart used an unexpected recovery sequence" >&2
	exit 1
fi
FAIL_START=

: >"$calls"
run_helper
if [ -e "$failure" ] || [ -e "$blocked_failure" ] || [ "$(tail -n 1 "$calls")" != "start preluded.service" ]; then
	echo "unplugged-phone recovery did not resume device scanning" >&2
	exit 1
fi

echo "prelude direct-OTG session recovery checks passed"
