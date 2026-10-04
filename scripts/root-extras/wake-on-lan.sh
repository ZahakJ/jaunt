#!/bin/sh
# jaunt host mode, optional extra C: let a sleeping laptop be woken by a magic packet from
# another machine ON THE SAME LAN (the packet cannot cross the relay or the internet).
# Uses NetworkManager, so the setting persists. On Arch a local wheel user may not need
# sudo for this; elsewhere it needs admin rights. WoL must also be enabled in the
# firmware (BIOS/UEFI) setup, which no script can do. Wi-Fi wake (WoWLAN) depends on the
# chip and often does not work from s2idle sleep.
#
#   sh wake-on-lan.sh status                  show each connection's wake setting
#   sh wake-on-lan.sh install <connection>    turn magic-packet wake on for it
#   sh wake-on-lan.sh undo <connection>       set it back to the default
#
# To wake it later, from a machine on the same LAN: wakeonlan <MAC> (or: wol <MAC>).
set -eu

usage() {
    sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

confirm() {
    printf '%s\nType yes to go on: ' "$1"
    read -r answer </dev/tty || answer=""
    if [ "$answer" != "yes" ]; then
        echo "nothing changed"
        exit 1
    fi
}

need_nmcli() {
    if ! command -v nmcli >/dev/null 2>&1; then
        echo "nmcli (NetworkManager) is not installed; see docs/awake.md for the ethtool/udev way" >&2
        exit 1
    fi
}

# Prints the property that holds the wake setting for a connection's type.
wake_property() {
    kind=$(nmcli -g connection.type connection show "$1") || {
        echo "no connection called '$1' (see: nmcli connection show)" >&2
        exit 2
    }
    case "$kind" in
        802-3-ethernet) echo 802-3-ethernet.wake-on-lan ;;
        802-11-wireless) echo 802-11-wireless.wake-on-wlan ;;
        *) echo "connection '$1' is of type $kind; only wired and Wi-Fi can wake the machine" >&2; exit 2 ;;
    esac
}

set_wake() {
    conn=$1 value=$2 verb=$3
    prop=$(wake_property "$conn")
    now=$(nmcli -g "$prop" connection show "$conn")
    echo "This will change connection '$conn': $prop from '$now' to '$value', then re-activate it"
    echo "(the network drops for a moment)."
    if [ "$verb" = install ]; then
        echo "To undo: sh $0 undo '$conn'"
    fi
    confirm "Change it?"
    nmcli connection modify "$conn" "$prop" "$value"
    nmcli connection up "$conn" >/dev/null
    echo "$conn: $prop = $(nmcli -g "$prop" connection show "$conn")"
    if [ "$verb" = install ]; then
        dev=$(nmcli -g GENERAL.DEVICES connection show "$conn" | head -n 1)
        if [ -n "$dev" ] && [ -r "/sys/class/net/$dev/address" ]; then
            echo "MAC address to wake: $(cat "/sys/class/net/$dev/address")"
        fi
    fi
}

case "${1:-}" in
    status)
        need_nmcli
        nmcli -g NAME,TYPE connection show | while IFS=: read -r name kind; do
            case "$kind" in
                802-3-ethernet) echo "$name (wired): $(nmcli -g 802-3-ethernet.wake-on-lan connection show "$name")" ;;
                802-11-wireless) echo "$name (Wi-Fi): $(nmcli -g 802-11-wireless.wake-on-wlan connection show "$name")" ;;
            esac
        done
        ;;
    install)
        need_nmcli
        [ -n "${2:-}" ] || usage
        set_wake "$2" magic install
        ;;
    undo)
        need_nmcli
        [ -n "${2:-}" ] || usage
        set_wake "$2" default undo
        ;;
    *)
        usage
        ;;
esac
