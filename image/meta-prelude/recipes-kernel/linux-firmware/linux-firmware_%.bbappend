# linux-firmware-mediatek ships every MediaTek blob, most of them for PCIe and
# SoC parts. Split out only the files the mt76 USB drivers load.
PACKAGES:prepend = "${PN}-prelude-mt76-usb "

LICENSE:${PN}-prelude-mt76-usb = "Firmware-mediatek & Firmware-ralink_a_mediatek_company_firmware"
FILES:${PN}-prelude-mt76-usb = " \
    ${nonarch_base_libdir}/firmware/mediatek/mt7610u.bin* \
    ${nonarch_base_libdir}/firmware/mediatek/mt7663pr2h*.bin* \
    ${nonarch_base_libdir}/firmware/mediatek/mt7663_n9_*.bin* \
    ${nonarch_base_libdir}/firmware/mediatek/WIFI_MT7961_patch_mcu_*.bin* \
    ${nonarch_base_libdir}/firmware/mediatek/WIFI_RAM_CODE_MT7961_*.bin* \
    ${nonarch_base_libdir}/firmware/mediatek/WIFI_MT7922_patch_mcu_*.bin* \
    ${nonarch_base_libdir}/firmware/mediatek/WIFI_RAM_CODE_MT7922_*.bin* \
    ${nonarch_base_libdir}/firmware/mediatek/mt7925/WIFI_MT7925_PATCH_MCU_*.bin* \
    ${nonarch_base_libdir}/firmware/mediatek/mt7925/WIFI_RAM_CODE_MT7925_*.bin* \
"
RDEPENDS:${PN}-prelude-mt76-usb = "${PN}-mediatek-license ${PN}-mt76x-license"
