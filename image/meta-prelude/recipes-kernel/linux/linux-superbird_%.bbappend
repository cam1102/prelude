FILESEXTRAPATHS:prepend := "${THISDIR}/linux-superbird:"

SRC_URI:append = " \
    file://prelude-usb.cfg \
    file://prelude-wireless.cfg \
"
