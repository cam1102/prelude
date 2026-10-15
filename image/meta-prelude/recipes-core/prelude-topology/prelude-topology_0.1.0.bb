SUMMARY = "Prelude physical USB topology policies"
DESCRIPTION = "Systemd policy marker and recovery services for direct charge-through OTG images."
LICENSE = "GPL-3.0-only"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891"

SRC_URI = " \
    file://prelude-session-recover \
    file://prelude-otg-recovery.service \
    file://prelude-otg-recovery.path \
    file://otg-always-host.conf \
    file://otg-host-health.conf \
"
S = "${UNPACKDIR}"

inherit systemd

PACKAGES = "${PN}-otg"

SYSTEMD_PACKAGES = "${PN}-otg"
SYSTEMD_SERVICE:${PN}-otg = " \
    prelude-otg-recovery.service \
    prelude-otg-recovery.path \
"
SYSTEMD_AUTO_ENABLE:${PN}-otg = "enable"

FILES:${PN}-otg = " \
    ${libexecdir}/prelude-topology-otg \
    ${libexecdir}/prelude-session-recover \
    ${systemd_system_unitdir}/prelude-otg-recovery.service \
    ${systemd_system_unitdir}/prelude-otg-recovery.path \
    ${systemd_system_unitdir}/prelude-usb-host.service.d/20-otg-always-host.conf \
    ${systemd_system_unitdir}/prelude-host-health.service.d/20-otg-always-host.conf \
    ${systemd_system_unitdir}/prelude-host-health-failure.service.d/20-otg-always-host.conf \
"

do_install() {
    install -d ${D}${libexecdir}
    install -m 0444 /dev/null ${D}${libexecdir}/prelude-topology-otg
    install -m 0755 ${UNPACKDIR}/prelude-session-recover \
        ${D}${libexecdir}/prelude-session-recover

    install -d \
        ${D}${systemd_system_unitdir}/prelude-usb-host.service.d \
        ${D}${systemd_system_unitdir}/prelude-host-health.service.d \
        ${D}${systemd_system_unitdir}/prelude-host-health-failure.service.d
    install -m 0644 ${UNPACKDIR}/prelude-otg-recovery.service \
        ${D}${systemd_system_unitdir}/prelude-otg-recovery.service
    install -m 0644 ${UNPACKDIR}/prelude-otg-recovery.path \
        ${D}${systemd_system_unitdir}/prelude-otg-recovery.path
    install -m 0644 ${UNPACKDIR}/otg-always-host.conf \
        ${D}${systemd_system_unitdir}/prelude-usb-host.service.d/20-otg-always-host.conf
    install -m 0644 ${UNPACKDIR}/otg-host-health.conf \
        ${D}${systemd_system_unitdir}/prelude-host-health.service.d/20-otg-always-host.conf
    install -m 0644 ${UNPACKDIR}/otg-host-health.conf \
        ${D}${systemd_system_unitdir}/prelude-host-health-failure.service.d/20-otg-always-host.conf
}
