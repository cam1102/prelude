# This package is installed only by the development image.  It must boot with
# CDC-NCM and ADB available because no UART is assumed.
SYSTEMD_AUTO_ENABLE:${PN} = "enable"
