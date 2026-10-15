FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

do_install:append() {
    sed -i 's|@@VERSION@@|${DISTRO_VERSION}|g' "${D}${sysconfdir}/motd"
}
