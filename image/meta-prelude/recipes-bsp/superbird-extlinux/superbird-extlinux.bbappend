# The 115200 baud console blocks every printk and systemd status line for ~9ms,
# which costs seconds on the boot critical path. Keep warnings and failures on
# the console; the full log stays in dmesg and the journal.
do_deploy:append() {
    sed -i 's/ rootwait / rootwait quiet systemd.show_status=error /' ${DEPLOYDIR}/extlinux.conf
    grep -q ' rootwait quiet systemd.show_status=error ' ${DEPLOYDIR}/extlinux.conf
}
