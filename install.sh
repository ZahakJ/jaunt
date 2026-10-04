#!/bin/sh
# jaunt installer. No root: puts one static binary in ~/.local/bin and nothing else
# unless asked.
#
#   curl -fsSL https://raw.githubusercontent.com/ZahakJ/jaunt/main/install.sh | sh
#   sh install.sh --relay https://relay.example.net --name fern
#   sh install.sh --from-file ./jaunt-x86_64-linux-musl     # no network
#
# Options:
#   --version TAG     release to install (default: the latest)
#   --relay URL       write the relay URL into jaunt's config
#   --name NAME       write this machine's name into jaunt's config
#   --up              also install and start the user service ('jaunt up')
#   --no-service      do not suggest the service; the daemon then starts on first use
#   --from-file FILE  install this binary instead of downloading (FILE.sha256 is checked if present)
#   --bin-dir DIR     where to put the binary (default ~/.local/bin)
# Env: JAUNT_REPO (owner/name on GitHub), JAUNT_BASE_URL (download base, file:// works).
#
# The download is checked against the release's .sha256 file. A private repository is
# reached through 'gh release download' when the plain download answers 404.
# What the checksum does and does not do: it comes from the same place as the binary, so it
# catches a truncated or corrupted download, not a malicious release or a compromised
# repository. Downloads are HTTPS only (no plain http://, no redirect to http://);
# --from-file without a FILE.sha256 installs an unchecked binary (and says so).
# POSIX sh. Everything is inside main() so a truncated download runs nothing.

main() {
	set -u
	REPO=${JAUNT_REPO:-ZahakJ/jaunt}
	VERSION=
	RELAY=
	NAME=
	UP=
	NO_SERVICE=
	FROM_FILE=
	BIN_DIR=${JAUNT_BIN_DIR:-$HOME/.local/bin}
	ASSET=

	while [ $# -gt 0 ]; do
		case $1 in
		--version) VERSION=${2:?--version needs a tag, e.g. v0.1.0}; shift ;;
		--relay) RELAY=${2:?--relay needs a URL}; shift ;;
		--name) NAME=${2:?--name needs a name}; shift ;;
		--up) UP=1 ;;
		--no-service) NO_SERVICE=1 ;;
		--from-file) FROM_FILE=${2:?--from-file needs a file}; shift ;;
		--bin-dir) BIN_DIR=${2:?--bin-dir needs a directory}; shift ;;
		-h | --help)
			sed -n '2,21p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'
			exit 0
			;;
		*) fail "unknown option: $1 (see --help)" ;;
		esac
		shift
	done

	[ -n "$UP" ] && [ -n "$NO_SERVICE" ] && fail "--up and --no-service contradict each other"
	[ "$(id -u)" = 0 ] && fail "run this as the user who will use jaunt, not as root"
	case $HOME in /*) ;; *) fail "HOME is not set to an absolute path" ;; esac
	if [ -n "$NAME" ]; then
		valid_name "$NAME" || fail "'$NAME' is not a valid name: lowercase letters, digits and '-', at most 32"
	fi
	if [ -n "$RELAY" ]; then
		valid_relay "$RELAY" || fail "'$RELAY' does not look like a relay URL (https://host[:port][/])"
	fi

	case $(uname -s)-$(uname -m) in
	Linux-x86_64 | Linux-amd64) ASSET=jaunt-x86_64-linux-musl ;;
	*) [ -n "$FROM_FILE" ] || fail "no release binary for $(uname -s) $(uname -m); build from source: cargo build --release" ;;
	esac

	missing=
	for c in mkdir mv chmod mktemp; do
		command -v "$c" >/dev/null 2>&1 || missing="$missing $c"
	done
	sha256 /dev/null >/dev/null 2>&1 || missing="$missing sha256sum-or-shasum-or-openssl"
	[ -z "$missing" ] || fail "missing required commands:$missing (nothing was changed)"

	TMP=$(mktemp -d "${TMPDIR:-/tmp}/jaunt-install.XXXXXX") || fail "mktemp failed"
	trap 'rm -rf "$TMP"' EXIT
	trap 'exit 130' INT TERM

	if [ -n "$FROM_FILE" ]; then
		[ -f "$FROM_FILE" ] || fail "no such file: $FROM_FILE"
		cp "$FROM_FILE" "$TMP/jaunt" || fail "cannot read $FROM_FILE"
		if [ -f "$FROM_FILE.sha256" ]; then
			verify "$TMP/jaunt" "$FROM_FILE.sha256"
		else
			log "no $FROM_FILE.sha256 next to it; not checking a checksum for a local file"
		fi
	else
		fetch
		verify "$TMP/jaunt" "$TMP/jaunt.sha256"
	fi
	chmod 0755 "$TMP/jaunt"
	"$TMP/jaunt" --version >/dev/null 2>&1 || fail "the binary does not run on this machine"
	new_version=$("$TMP/jaunt" --version)

	mkdir -p "$BIN_DIR" || fail "cannot create $BIN_DIR"
	dest=$BIN_DIR/jaunt
	if [ -f "$dest" ] && [ "$(sha256 "$dest")" = "$(sha256 "$TMP/jaunt")" ]; then
		log "$dest is already $new_version"
	else
		# Same directory, then rename: a running daemon keeps its old inode.
		if ! { cp "$TMP/jaunt" "$dest.new-$$" && chmod 0755 "$dest.new-$$" && mv -f "$dest.new-$$" "$dest"; }; then
			rm -f "$dest.new-$$"
			fail "cannot write $dest"
		fi
		log "installed $new_version to $dest"
	fi

	write_config

	case :$PATH: in
	*:"$BIN_DIR":*) J=jaunt ;;
	*)
		J=$dest
		log "$BIN_DIR is not in PATH; add it, or call $dest directly"
		;;
	esac

	if [ -n "$UP" ]; then
		"$dest" up || fail "'jaunt up' failed; the binary is installed, run it again by hand"
		printf '\nnext:\n  %s link          # on a machine already in the mesh, then join with its code here\n' "$J"
		return 0
	fi
	printf '\nnothing is running yet. next:\n'
	if [ -z "$NO_SERVICE" ]; then
		printf '  %s up              # install and start the user service (survives logout and reboot)\n' "$J"
	fi
	relay_arg=
	{ [ -n "$RELAY" ] || has_relay; } || relay_arg=" --relay URL"
	printf '  %s join%s   # then type the code "jaunt link" shows on a machine in the mesh (on the first machine: jaunt relay set URL; jaunt link)\n' "$J" "$relay_arg"
}

log() { printf '==> %s\n' "$*"; }
fail() {
	printf 'error: %s\n' "$*" >&2
	exit 1
}

valid_name() {
	case $1 in
	'' | -* | *- | *[!a-z0-9-]*) return 1 ;;
	esac
	[ "${#1}" -le 32 ]
}

valid_relay() {
	case $1 in
	https://* | http://*) ;;
	*) return 1 ;;
	esac
	case $1 in
	*[!A-Za-z0-9.:/_-]*) return 1 ;;
	esac
	[ "${#1}" -le 256 ]
}

sha256() { # sha256 FILE -> hex digest on stdout
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d ' ' -f 1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | cut -d ' ' -f 1
	elif command -v openssl >/dev/null 2>&1; then
		openssl dgst -sha256 "$1" | sed 's/.*= *//'
	else
		return 1
	fi
}

verify() { # verify FILE SUMFILE
	want=$(cut -d ' ' -f 1 "$2" | head -n 1)
	case $want in
	*[!0-9a-f]* | '') fail "$2 is not a sha256 checksum file" ;;
	esac
	[ "${#want}" = 64 ] || fail "$2 is not a sha256 checksum file"
	got=$(sha256 "$1") || fail "cannot compute a sha256"
	[ "$got" = "$want" ] || fail "checksum mismatch: expected $want, got $got (nothing was installed)"
	log "sha256 ok ($got)"
}

# The release binary and its .sha256 into $TMP. Plain HTTPS first; a 404 means a private
# repository (or a missing tag), which 'gh' can reach with the owner's login.
fetch() {
	if [ -n "${JAUNT_BASE_URL:-}" ]; then
		base=${JAUNT_BASE_URL%/}
		case $base in
		https://* | file://*) ;;
		*) fail "JAUNT_BASE_URL must be https:// (or file://), not $base" ;;
		esac
	elif [ -n "$VERSION" ]; then
		base=https://github.com/$REPO/releases/download/$VERSION
	else
		base=https://github.com/$REPO/releases/latest/download
	fi
	command -v curl >/dev/null 2>&1 || fail "curl is needed to download (or use --from-file)"
	# HTTPS only, redirects included; a file:// base needs the file protocol instead.
	case $base in
	file://*) proto='=file' ;;
	*) proto='=https' ;;
	esac
	log "downloading $base/$ASSET"
	code=$(curl -sS -L --proto "$proto" --proto-redir "$proto" --retry 3 --connect-timeout 20 -w '%{http_code}' -o "$TMP/jaunt" "$base/$ASSET") || code=fail
	case $code in
	200 | 000) # 000: a file:// URL that worked
		curl -fsS -L --proto "$proto" --proto-redir "$proto" --retry 3 --connect-timeout 20 -o "$TMP/jaunt.sha256" "$base/$ASSET.sha256" ||
			fail "cannot download $base/$ASSET.sha256"
		return 0
		;;
	404) ;;
	fail) fail "download failed from $base/$ASSET (proxy? set https_proxy, or use --from-file)" ;;
	*) fail "download failed (HTTP $code) from $base/$ASSET (proxy? set https_proxy, or use --from-file)" ;;
	esac
	[ -z "${JAUNT_BASE_URL:-}" ] || fail "$base/$ASSET: not found"
	command -v gh >/dev/null 2>&1 ||
		fail "$REPO answered 404: a private repository needs the GitHub CLI ('gh auth login'), or use --from-file"
	log "404 from the public URL; trying 'gh release download' (private repository)"
	rm -f "$TMP/jaunt"
	if [ -n "$VERSION" ]; then
		gh release download "$VERSION" -R "$REPO" -p "$ASSET" -p "$ASSET.sha256" -D "$TMP/gh" ||
			fail "gh release download failed for $REPO $VERSION"
	else
		gh release download -R "$REPO" -p "$ASSET" -p "$ASSET.sha256" -D "$TMP/gh" ||
			fail "gh release download failed for $REPO (latest)"
	fi
	if ! { mv "$TMP/gh/$ASSET" "$TMP/jaunt" && mv "$TMP/gh/$ASSET.sha256" "$TMP/jaunt.sha256"; }; then
		fail "the release has no $ASSET or $ASSET.sha256"
	fi
}

config_file() {
	if [ -n "${JAUNT_HOME:-}" ]; then
		printf '%s/config/config.toml' "$JAUNT_HOME"
	else
		printf '%s/jaunt/config.toml' "${XDG_CONFIG_HOME:-$HOME/.config}"
	fi
}

has_relay() { grep -q '^relays *=' "$(config_file)" 2>/dev/null; }

# --name / --relay into config.toml. Values already there are kept (jaunt name / jaunt
# relay set change them); top-level keys go first, before any [table].
write_config() {
	[ -n "$NAME$RELAY" ] || return 0
	cfg=$(config_file)
	add=
	if [ -n "$NAME" ]; then
		if grep -q '^name *=' "$cfg" 2>/dev/null; then
			log "config already names this machine; keeping it (change it with: jaunt name $NAME)"
		else
			add="${add}name = \"$NAME\"
"
		fi
	fi
	if [ -n "$RELAY" ]; then
		if has_relay; then
			log "config already has a relay; keeping it (change it with: jaunt relay set $RELAY)"
		else
			add="${add}relays = [\"$RELAY\"]
"
		fi
	fi
	[ -n "$add" ] || return 0
	dir=$(dirname "$cfg")
	(umask 077 && mkdir -p "$dir") || fail "cannot create $dir"
	if ! {
		{
			printf '%s' "$add"
			if [ -f "$cfg" ]; then cat "$cfg"; fi
		} >"$cfg.new-$$" && chmod 0600 "$cfg.new-$$" && mv -f "$cfg.new-$$" "$cfg"
	}; then
		rm -f "$cfg.new-$$"
		fail "cannot write $cfg"
	fi
	log "wrote $(printf '%s' "$add" | tr '\n' ' ')to $cfg"
}

main "$@"
