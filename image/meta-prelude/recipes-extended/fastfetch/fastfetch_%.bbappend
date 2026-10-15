FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

SRC_URI += " \
    file://prelude.jsonc \
    file://prelude-logo.txt \
"

do_install:append() {
    install -m 0644 ${UNPACKDIR}/prelude.jsonc    ${D}${sysconfdir}/fastfetch/config.jsonc
    install -m 0644 ${UNPACKDIR}/prelude-logo.txt ${D}${sysconfdir}/fastfetch/ascii-logo.txt
}
