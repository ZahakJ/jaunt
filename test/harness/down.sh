#!/usr/bin/env bash
# Remove exactly what up.sh created, by name. Leaves the image (pass --image to remove it too).
set -uo pipefail
. "$(dirname "$0")/lib.sh"
for c in $CONTAINERS; do
  if docker inspect "$c" >/dev/null 2>&1; then
    [ "$(docker inspect -f '{{.State.Paused}}' "$c")" = true ] && docker unpause "$c" >/dev/null
    docker rm -f "$c" >/dev/null && log "removed container $c"
  fi
done
for n in $NETWORKS; do
  docker network inspect "$n" >/dev/null 2>&1 && docker network rm "$n" >/dev/null && log "removed network $n"
done
if [ "${1:-}" = --image ]; then docker image rm "$IMAGE" >/dev/null && log "removed image $IMAGE"; fi
rm -rf "$HARNESS_DIR/.state"
exit 0
