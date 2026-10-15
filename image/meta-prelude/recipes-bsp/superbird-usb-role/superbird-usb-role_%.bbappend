FILESEXTRAPATHS:prepend := "${THISDIR}/files:"

# preludectl is the sole authority for persistent host mode.  Leaving the
# BSP persistence unit enabled would create a second, racy boot-role store.
SYSTEMD_AUTO_ENABLE = "disable"
SYSTEMD_AUTO_ENABLE:${PN} = "disable"
