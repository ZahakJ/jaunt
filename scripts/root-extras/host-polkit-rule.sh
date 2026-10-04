#!/bin/sh
# jaunt host mode, optional root extra A (preferred): a polkit rule that lets ONE user take
# logind's lid and sleep inhibitors without a local login (after a reboot, or with only
# SSH sessions). jaunt's guard still decides when to hold them: only on mains power and
# below the heat limit. Nothing changes while jaunt is not running.
#
#   sudo sh host-polkit-rule.sh install <user>    write the rule
#   sudo sh host-polkit-rule.sh undo              remove it
#   sh host-polkit-rule.sh show <user>            print the rule, change nothing
#
# Risk: any process of <user> can then block sleep and the lid switch at any time. That is
# already true while <user> is logged in locally.
set -eu

RULE=/etc/polkit-1/rules.d/49-jaunt-host.rules

usage() {
    sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

rule_text() {
    cat <<EOF
// Installed by jaunt's scripts/root-extras/host-polkit-rule.sh. Remove this file to undo.
// Lets $1 hold logind's lid-switch and sleep inhibitors without a local session.
polkit.addRule(function(action, subject) {
    if ((action.id == "org.freedesktop.login1.inhibit-handle-lid-switch" ||
         action.id == "org.freedesktop.login1.inhibit-block-sleep") &&
        subject.user == "$1") {
        return polkit.Result.YES;
    }
});
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
        echo "run this with sudo (it writes under /etc)" >&2
        exit 1
    fi
}

valid_user() {
    case "$1" in
        "" | *[!a-z0-9_-]* | [!a-z_]*) echo "'$1' does not look like a user name" >&2; exit 2 ;;
    esac
    if ! id -u "$1" >/dev/null 2>&1; then
        echo "no such user: $1" >&2
        exit 2
    fi
}

cmd=${1:-}
case "$cmd" in
    show)
        valid_user "${2:-}"
        rule_text "$2"
        ;;
    install)
        need_root
        valid_user "${2:-}"
        echo "This will write $RULE:"
        echo
        rule_text "$2"
        echo
        echo "polkitd reads rules.d by itself; if the rule does not take effect: systemctl restart polkit"
        echo "To undo: sudo sh $0 undo   (or: sudo rm $RULE)"
        confirm "Install it?"
        tmp=$(mktemp "$RULE.XXXXXX")
        rule_text "$2" >"$tmp"
        chmod 0644 "$tmp"
        mv "$tmp" "$RULE"
        echo "installed $RULE"
        ;;
    undo)
        need_root
        if [ ! -e "$RULE" ]; then
            echo "$RULE is not there; nothing to undo"
            exit 0
        fi
        echo "This will delete $RULE. Afterwards jaunt can take the lid lock only while you are logged in locally."
        confirm "Remove it?"
        rm -f "$RULE"
        echo "removed $RULE"
        ;;
    *)
        usage
        ;;
esac
