SUMMARY = "CT projection production image"
DESCRIPTION = "Wired and wireless CarPlay and Android Auto appliance image with persistent direct charge-through OTG host mode, automatic projection policy, and the Weston kiosk compositor."
LICENSE = "GPL-3.0-only"

require prelude-image-base.inc

IMAGE_OVERHEAD_FACTOR = "1.0"
IMAGE_ROOTFS_EXTRA_SPACE = "4096"

IMAGE_INSTALL:append = " \
    superbird-weston-init-kiosk \
    prelude-topology-otg \
    prelude-runtime-dev-config \
    superbird-usb-gadget \
    android-tools-adbd \
    fastfetch \
"

BAD_RECOMMENDATIONS += "adwaita-icon-theme-symbolic"

# Package postinsts enable these services for development images. Remove every
# target wants link after this rootfs is assembled so production starts the
# recovery gadget only when automatic-host-boot detects a held Back button.
prelude_disable_prod_otg_recovery_boot_units() {
    preset="${IMAGE_ROOTFS}${systemd_unitdir}/system-preset/00-prelude-prod-otg-recovery.preset"
    install -d "$(dirname "$preset")"
    printf '%s\n' \
        'disable superbird-usb-gadget.service' \
        'disable android-tools-adbd.service' > "$preset"

    for unit in superbird-usb-gadget.service android-tools-adbd.service; do
        for wants in "${IMAGE_ROOTFS}${sysconfdir}/systemd/system/"*.wants; do
            if [ -d "$wants" ]; then
                rm -f "$wants/$unit"
            fi
        done
    done
}

prelude_validate_prod_otg_topology() {
    marker="${IMAGE_ROOTFS}${libexecdir}/prelude-topology-otg"
    config="${IMAGE_ROOTFS}${sysconfdir}/prelude/config-dev.toml"
    daemon_unit="${IMAGE_ROOTFS}${systemd_system_unitdir}/preluded.service"
    dev_dropin="${IMAGE_ROOTFS}${systemd_system_unitdir}/preluded.service.d/10-dev.conf"

    if [ ! -f "$marker" ] || [ ! -f "$config" ]; then
        bbfatal "production OTG image is missing its topology marker or live configuration"
    fi
    if [ ! -f "$dev_dropin" ]; then
        bbfatal "production OTG image is missing the live daemon override"
    fi
    for overlay_name in carplay-logo android-auto-logo; do
        overlay="${IMAGE_ROOTFS}${datadir}/prelude/${overlay_name}.nv12"
        if [ ! -f "$overlay" ] || [ "$(wc -c < "$overlay")" -ne 576000 ]; then
            bbfatal "production OTG image is missing its 800x480 ${overlay_name} frame"
        fi
        case "$overlay_name" in
            carplay-logo)
                expected_hash="036c4d51198eaf0fbb2fd13c3d3b6cfab1eaff5b44ecf8c5d17ea5267075b124"
                ;;
            android-auto-logo)
                expected_hash="5c658f778f8265a02913e6c77d684d542fe95951225f33e53019f3ae3ae3fb52"
                ;;
        esac
        if [ "$(sha256sum "$overlay" | cut -d' ' -f1)" != "$expected_hash" ]; then
            bbfatal "production OTG ${overlay_name} frame does not match the supplied logo frame"
        fi
    done
    if [ ! -f "${IMAGE_ROOTFS}${datadir}/fonts/prelude/Inter-Regular.ttf" ]; then
        bbfatal "production OTG image is missing the Inter font used for status text"
    fi
    if ! grep -qx 'ExecStart=/usr/bin/preluded --config /etc/prelude/config-dev.toml' "$dev_dropin"; then
        bbfatal "production OTG image does not use config-dev.toml"
    fi
    for setting in \
        'mode = "automatic"' \
        '[android_auto]' \
        'enable_live_session = true' \
        'audio = "alsa_usb"' \
        'microphone = "built_in_superbird"' \
        'allow_first_time_pairing = true'; do
        if ! grep -Fqx "$setting" "$config"; then
            bbfatal "production OTG live configuration is missing: $setting"
        fi
    done
    for path in \
        "${systemd_system_unitdir}/prelude-usb-host.service.d/20-otg-always-host.conf" \
        "${systemd_system_unitdir}/prelude-host-health.service.d/20-otg-always-host.conf" \
        "${systemd_system_unitdir}/prelude-host-health-failure.service.d/20-otg-always-host.conf"; do
        if [ ! -e "${IMAGE_ROOTFS}$path" ]; then
            bbfatal "production OTG topology payload is missing: $path"
        fi
    done
    for unit in superbird-usb-gadget.service android-tools-adbd.service; do
        if [ ! -e "${IMAGE_ROOTFS}${systemd_system_unitdir}/$unit" ]; then
            bbfatal "production OTG USB recovery unit is missing: $unit"
        fi
        for wants in "${IMAGE_ROOTFS}${sysconfdir}/systemd/system/"*.wants; do
            if [ -e "$wants/$unit" ] || [ -L "$wants/$unit" ]; then
                bbfatal "production OTG USB recovery unit is enabled on the normal boot path: $unit"
            fi
        done
    done
    preset="${IMAGE_ROOTFS}${systemd_unitdir}/system-preset/00-prelude-prod-otg-recovery.preset"
    if [ "$(wc -l < "$preset")" -ne 2 ] || \
       ! grep -qx 'disable superbird-usb-gadget.service' "$preset" || \
       ! grep -qx 'disable android-tools-adbd.service' "$preset"; then
        bbfatal "production OTG USB recovery preset is missing or malformed"
    fi
    for path in \
        "${libexecdir}/prelude-usbip-client" \
        "${systemd_system_unitdir}/prelude-usbip-client.service" \
        "${sbindir}/usbip"; do
        if [ -e "${IMAGE_ROOTFS}$path" ]; then
            bbfatal "production OTG image contains unsupported USB/IP payload: $path"
        fi
    done
}
# Use an override-style append so this runs after OE's final preset-all pass.
ROOTFS_POSTPROCESS_COMMAND:append = " prelude_disable_prod_otg_recovery_boot_units; prelude_validate_prod_otg_topology;"
