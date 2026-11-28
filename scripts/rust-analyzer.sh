#!/bin/sh
# rust-analyzer from the same Flatpak SDK as dev.sh, for editors (the host has no GTK dev packages).
BF_TOOL=rust-analyzer exec "$(dirname "$0")/dev.sh" "$@"
