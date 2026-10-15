# preluded runs a DHCP-only dnsmasq bound to the projection access point. The
# stock system-wide instance must not also start.
SYSTEMD_AUTO_ENABLE:${PN} = "disable"
