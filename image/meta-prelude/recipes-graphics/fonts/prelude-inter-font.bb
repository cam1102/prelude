SUMMARY = "Inter font for the Prelude host settings UI"
DESCRIPTION = "The same Inter family used by the Nocturne UI settings surface."
HOMEPAGE = "https://rsms.me/inter/"
LICENSE = "OFL-1.1"
LIC_FILES_CHKSUM = "file://LICENSE.txt;md5=082d8cc7db39783f598b7eda8c911f9d"

SRC_URI = "https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip"
SRC_URI[sha256sum] = "9883fdd4a49d4fb66bd8177ba6625ef9a64aa45899767dde3d36aa425756b11e"

S = "${UNPACKDIR}"

inherit allarch fontcache

FONTS_DIR = "${datadir}/fonts/prelude"

do_install() {
    install -d ${D}${FONTS_DIR}
    install -m 0644 ${S}/extras/ttf/Inter-Bold.ttf ${D}${FONTS_DIR}/
    install -m 0644 ${S}/extras/ttf/Inter-Regular.ttf ${D}${FONTS_DIR}/
    install -m 0644 ${S}/extras/ttf/Inter-SemiBold.ttf ${D}${FONTS_DIR}/
}

FILES:${PN} = "${FONTS_DIR}"
RDEPENDS:${PN} = "fontconfig"
