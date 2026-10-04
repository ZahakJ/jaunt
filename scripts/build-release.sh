#!/bin/sh
# Build the release artefact: a static x86_64 Linux binary (musl), inside a throwaway
# debian:12 container with rustup, so the host needs nothing but Docker.
#
#   scripts/build-release.sh            # -> dist/jaunt-x86_64-linux-musl (+ .sha256)
#
# Caches (rustup toolchain, cargo registry, apt packages, build dir) live under
# target/musl-build/, so a second build only recompiles jaunt. The container runs the
# build as the calling user, so nothing in the tree ends up owned by root.
# Env: TOOLCHAIN (default 1.97.1), JOBS (default 4), IMAGE (default debian:12).
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
TOOLCHAIN=${TOOLCHAIN:-1.97.1}
JOBS=${JOBS:-4}
IMAGE=${IMAGE:-debian:12}
CACHE=$ROOT/target/musl-build
TARGET=x86_64-unknown-linux-musl
OUT=$ROOT/dist
NAME=jaunt-build-musl

command -v docker >/dev/null 2>&1 || { echo "error: docker is needed" >&2; exit 1; }
if docker ps -a --format '{{.Names}}' | grep -qx "$NAME"; then
	echo "error: a container named $NAME exists (another build running?)" >&2
	exit 1
fi
mkdir -p "$CACHE/apt" "$CACHE/rustup" "$CACHE/cargo" "$CACHE/target" "$CACHE/home" "$OUT"

uid=$(id -u)
gid=$(id -g)
# Inside: apt as root (packages cached on the host), then everything else as the caller.
# shellcheck disable=SC2016 # expanded inside the container
inner='
set -eu
rm -f /etc/apt/apt.conf.d/docker-clean
apt-get -qq update
DEBIAN_FRONTEND=noninteractive apt-get -qq install -y --no-install-recommends \
	ca-certificates curl gcc libc6-dev musl-tools util-linux >/dev/null
chown -R "$BUILD_UID:$BUILD_GID" /var/cache/apt/archives
setpriv --reuid="$BUILD_UID" --regid="$BUILD_GID" --clear-groups sh -eu -c "
	export HOME=/cache/home RUSTUP_HOME=/cache/rustup CARGO_HOME=/cache/cargo
	export PATH=/cache/cargo/bin:\$PATH
	if [ ! -x /cache/cargo/bin/rustup ]; then
		curl -sSf --proto =https --tlsv1.2 https://sh.rustup.rs -o /tmp/rustup-init.sh
		sh /tmp/rustup-init.sh -y -q --no-modify-path --profile minimal --default-toolchain none
	fi
	rustup toolchain install --profile minimal $TOOLCHAIN
	rustup target add --toolchain $TOOLCHAIN $TARGET
	cd /src
	CARGO_TARGET_DIR=/cache/target cargo +$TOOLCHAIN build --locked --release -j $JOBS --target $TARGET
	rustc +$TOOLCHAIN --version
"
'
docker run --rm --name "$NAME" \
	-e BUILD_UID="$uid" -e BUILD_GID="$gid" -e TOOLCHAIN="$TOOLCHAIN" -e TARGET="$TARGET" -e JOBS="$JOBS" \
	-v "$ROOT:/src:ro" \
	-v "$CACHE/apt:/var/cache/apt/archives" \
	-v "$CACHE/rustup:/cache/rustup" -v "$CACHE/cargo:/cache/cargo" \
	-v "$CACHE/target:/cache/target" -v "$CACHE/home:/cache/home" \
	"$IMAGE" sh -c "$inner"

bin=$CACHE/target/$TARGET/release/jaunt
[ -x "$bin" ] || { echo "error: the build produced no binary" >&2; exit 1; }
cp "$bin" "$OUT/jaunt-x86_64-linux-musl"
(cd "$OUT" && sha256sum jaunt-x86_64-linux-musl >jaunt-x86_64-linux-musl.sha256)
file "$OUT/jaunt-x86_64-linux-musl" 2>/dev/null || true
ls -l "$OUT/jaunt-x86_64-linux-musl"
cat "$OUT/jaunt-x86_64-linux-musl.sha256"
