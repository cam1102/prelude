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
    for overlay_name in \
        carplay \
        carplay-unlock-phone \
        carplay-connecting \
        carplay-connected \
        carplay-missing-audio \
        android-auto \
        android-auto-connecting \
        android-auto-connected \
        android-auto-missing-audio; do
        overlay="${IMAGE_ROOTFS}${datadir}/prelude/${overlay_name}.nv12"
        if [ ! -f "$overlay" ] || [ "$(wc -c < "$overlay")" -ne 576000 ]; then
            bbfatal "production OTG image is missing its 800x480 ${overlay_name} overlay"
        fi
        case "$overlay_name" in
            carplay)
                expected_hash="a1a5f41a60fd909163ebbde83921b80d2f1f3f6c6703f97fad937ef3225ed055"
                ;;
            carplay-unlock-phone)
                expected_hash="357b9611a1618d5f6682a3896672fa5822219e66456e8c62631928a4e361204e"
                ;;
            carplay-connecting)
                expected_hash="2068c2f77a706e5082125f4881a88070db81e99be43297bdffb76181d97442b2"
                ;;
            carplay-connected)
                expected_hash="14bad31f686da1c4a5f00a741de6cfbca4d2edfc2e830e969e4d23b8eff4d24c"
                ;;
            carplay-missing-audio)
                expected_hash="114937189a8cab25acb9eefd861e2d107951a57eabd69ebe28b63fde113e3796"
                ;;
            android-auto)
                expected_hash="e2940971db3c0eaed42f844963f91d5056e9dc3e2b900df5603f25c2333c56e5"
                ;;
            android-auto-connecting)
                expected_hash="a4988d1235bb922f374dc3b81257a5af518cf8a68853c6f206dc029207827a40"
                ;;
            android-auto-connected)
                expected_hash="e6daf507c1872844299e70554d153821788c6c34ea24614024585949a4740824"
                ;;
            android-auto-missing-audio)
                expected_hash="5b27408cf3afa2f9d3f3e6fdb06ac5b278314af31b581d83f3cf0457287b7a9d"
                ;;
        esac
        if [ "$(sha256sum "$overlay" | cut -d' ' -f1)" != "$expected_hash" ]; then
            bbfatal "production OTG ${overlay_name} overlay does not match the supplied splash frame"
        fi
    done
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
