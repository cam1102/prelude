# Product images must not install the composite USB gadget. The development
# packagegroup opts it back in for direct-USB recovery. The BSP's Bluetooth
# bring-up stays for wireless projection.
RDEPENDS:${PN}:remove = "superbird-usb-gadget"
