# preluded registers its own Hands-Free profile for the wireless Android Auto
# bootstrap. BlueZ's native HFP plugin would claim and auto-connect the same UUID.
PACKAGECONFIG:remove = "hfp-profiles"

# A phone that forgot the Car Thing re-pairs with just-works while BlueZ still
# holds the old bond; the default "never" rejects it without asking the agent.
do_install:append() {
    sed -i 's/^#JustWorksRepairing = never$/JustWorksRepairing = always/' \
        ${D}${sysconfdir}/bluetooth/main.conf
    grep -q '^JustWorksRepairing = always$' ${D}${sysconfdir}/bluetooth/main.conf
}
