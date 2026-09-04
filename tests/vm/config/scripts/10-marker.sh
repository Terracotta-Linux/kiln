#!/bin/bash
# A build script with a visible, checkable effect.
#
# It writes into /usr, so the changeset lands in the commit and the probe can
# read it back from a read-only /usr on a booted system. Deliberately not /var:
# that would be drained and restored by tmpfiles, which would prove the drain
# rather than the script.
set -euo pipefail
mkdir -p /usr/share/kiln-boot
printf 'script ran in %s generation %s\n' "$KILN_IMAGE" "$KILN_GENERATION" \
    > /usr/share/kiln-boot/marker
