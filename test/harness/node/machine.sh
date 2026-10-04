#!/bin/sh
# Runs INSIDE a machine container: point the default route at the LAN router.
#   machine.sh <router_lan_ip>
set -eu
ip route del default 2>/dev/null || true
ip route replace default via "$1"
