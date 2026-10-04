#!/bin/sh
# jaunt host mode, optional root extra B: tell logind to ignore the lid whenever the
# machine is on mains power. Needs no jaunt guard at all, but it also has NO heat
# protection: a closed laptop on mains stays awake however hot it gets. Prefer the polkit
# rule (host-polkit-rule.sh) unless you know why you want this.
#
#   sudo sh logind-lid-on-ac.sh install     write the drop-in and reload logind
#   sudo sh logind-lid-on-ac.sh undo        remove it and reload logind
#   sh logind-lid-on-ac.sh show             print the drop-in, change nothing
#
# On battery the lid still suspends (HandleLidSwitch is left alone). logind is reloaded,
# never restarted (a restart can end sessions).
set -eu

DROPIN_DIR=/etc/systemd/logind.conf.d
DROPIN=$DROPIN_DIR/50-jaunt-lid-on-ac.conf

usage() {
    sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

dropin_text() {
    cat <<'EOF'
# Installed by jaunt's scripts/root-extras/logind-lid-on-ac.sh. Remove this file and run
# 'systemctl reload systemd-logind' to undo.
[Login]
HandleLidSwitchExternalPower=ignore
EOF
}

confirm() {
    printf '%s\nType yes to go on: ' "$1"
    read -r answer </dev/tty || answer=""
    if [ "$answer" != "yes" ]; then
        echo "nothing changed"
        exit 1
    fi
}

need_root() {
    if [ "$(id -u)" -ne 0 ]; then
        echo "run this with sudo (it writes under /etc and reloads logind)" >&2
        exit 1
    fi
}

case "${1:-}" in
    show)
        dropin_text
        ;;
    install)
        need_root
        echo "This will write $DROPIN:"
        echo
        dropin_text
        echo
        echo "and then run: systemctl reload systemd-logind"
        echo "Effect: on mains power, closing the lid does nothing. There is no heat limit."
        echo "To undo: sudo sh $0 undo"
        confirm "Install it?"
        mkdir -p "$DROPIN_DIR"
        tmp=$(mktemp "$DROPIN.XXXXXX")
        dropin_text >"$tmp"
        chmod 0644 "$tmp"
        mv "$tmp" "$DROPIN"
        systemctl reload systemd-logind
        echo "installed $DROPIN and reloaded logind"
        ;;
    undo)
        need_root
        if [ ! -e "$DROPIN" ]; then
            echo "$DROPIN is not there; nothing to undo"
            exit 0
        fi
        echo "This will delete $DROPIN and run: systemctl reload systemd-logind"
        confirm "Remove it?"
        rm -f "$DROPIN"
        systemctl reload systemd-logind
        echo "removed $DROPIN and reloaded logind"
        ;;
    *)
        usage
        ;;
esac
