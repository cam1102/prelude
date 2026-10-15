# Remote builds must provide both variables through KAS/environment or another
# downstream include. Local builds remove this URI and use /monorepo through
# externalsrc, so no repository URL is invented here.
PRELUDE_MONOREPO_SRC_URI ?= ""
PRELUDE_MONOREPO_SRCREV ?= ""

SRC_URI = "${PRELUDE_MONOREPO_SRC_URI}"
SRCREV = "${PRELUDE_MONOREPO_SRCREV}"
S = "${WORKDIR}/git"

python () {
    if not d.getVar("EXTERNALSRC") and not d.getVar("PRELUDE_MONOREPO_SRC_URI"):
        bb.fatal("Set PRELUDE_MONOREPO_SRC_URI and PRELUDE_MONOREPO_SRCREV, or build kas/prelude-prod-local.yml")
    if not d.getVar("EXTERNALSRC") and not d.getVar("PRELUDE_MONOREPO_SRCREV"):
        bb.fatal("PRELUDE_MONOREPO_SRCREV must be an immutable revision")
}
