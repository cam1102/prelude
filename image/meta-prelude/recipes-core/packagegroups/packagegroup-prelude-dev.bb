SUMMARY = "Prelude development tools"
DESCRIPTION = "USB, Ethernet, audio, graphics, and system diagnostics excluded from production."
LICENSE = "GPL-3.0-only"
LIC_FILES_CHKSUM = "file://${COMMON_LICENSE_DIR}/GPL-3.0-only;md5=c79ff39f19dfec6d293b95dea7b07891"

inherit packagegroup

PACKAGES = "${PN}"

RDEPENDS:${PN} = " \
    prelude-runtime-dev-config \
    superbird-usb-gadget \
    superbird-weston-init-desktop \
    weston-vnc-backend \
    wayland-utils \
    libdrm-tests \
    kmscube \
    \
    i2c-tools \
    usbutils \
    evtest \
    jq \
    iproute2 \
    lsof \
    strace \
    tcpdump \
    tmux \
    fastfetch \
"
