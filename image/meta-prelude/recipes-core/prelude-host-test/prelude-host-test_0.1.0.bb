SUMMARY = "Car Thing USB-host qualification runner"
DESCRIPTION = "Bounded, visible USB-host enumeration test with persistent diagnostics and automatic USB-device restoration."
LICENSE = "GPL-3.0-only"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891"

SRC_URI = " \
    file://prelude-host-test \
    file://prelude-host-test-display \
    file://prelude-host-test.service \
    file://prelude-host-test-display.service \
    file://preluded-host-test.conf \
"

S = "${UNPACKDIR}"

inherit systemd

RDEPENDS:${PN} = " \
    prelude-runtime \
    usbutils \
    weston \
"

SYSTEMD_SERVICE:${PN} = " \
    prelude-host-test-display.service \
    prelude-host-test.service \
"
SYSTEMD_AUTO_ENABLE:${PN} = "enable"

FILES:${PN}:append = " \
    ${systemd_system_unitdir}/preluded.service.d/host-test.conf \
"

do_install() {
    install -d ${D}${libexecdir}
    install -m 0755 ${UNPACKDIR}/prelude-host-test \
        ${D}${libexecdir}/prelude-host-test
    install -m 0755 ${UNPACKDIR}/prelude-host-test-display \
        ${D}${libexecdir}/prelude-host-test-display

    install -d ${D}${systemd_system_unitdir}
    install -m 0644 ${UNPACKDIR}/prelude-host-test.service \
        ${D}${systemd_system_unitdir}/
    install -m 0644 ${UNPACKDIR}/prelude-host-test-display.service \
        ${D}${systemd_system_unitdir}/

    install -d ${D}${systemd_system_unitdir}/preluded.service.d
    install -m 0644 ${UNPACKDIR}/preluded-host-test.conf \
        ${D}${systemd_system_unitdir}/preluded.service.d/host-test.conf
}
