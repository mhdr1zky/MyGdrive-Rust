#!/bin/sh
set -eu
# Flatpak resets XDG_DATA_HOME before launching; use its private short alias
# after entry so UUID profile service sockets fit Linux sun_path.
export XDG_DATA_HOME=/var/data
case "${0##*/}" in
    mygdrive-sync) exec /app/libexec/mygdrive/mygdrive-sync "$@" ;;
    *) exec /app/libexec/mygdrive/mygdrive-desktop "$@" ;;
esac
