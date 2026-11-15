#!/bin/sh
# Run cargo inside the GNOME 51 Flatpak SDK (GTK4 + Rust), so the host needs no GTK dev packages.
# Usage: scripts/dev.sh build | run | test | clippy ...
# One-time setup:
#   flatpak install --user flathub org.gnome.Sdk//51 org.gnome.Platform//51 org.freedesktop.Sdk.Extension.rust-stable//26.08
set -e
cd "$(dirname "$0")/.."
# Same data folders the real Flatpak will use, so the app's key and settings carry over.
app="$HOME/.var/app/io.github.theteknocat.BubbleFairy"
mkdir -p "$app/data" "$app/config" "$app/cache"
exec flatpak run --user --share=network --socket=wayland --socket=fallback-x11 --socket=session-bus --device=dri \
  --filesystem="$PWD" --filesystem="$app" --env=XDG_DATA_HOME="$app/data" --env=XDG_CONFIG_HOME="$app/config" --env=XDG_CACHE_HOME="$app/cache" --filesystem="$HOME/.cargo" --env=CARGO_HOME="$HOME/.cargo" \
  --env=CARGO_TARGET_DIR="$PWD/target/sdk" \
  --command=sh org.gnome.Sdk//51 -c '. /usr/lib/sdk/rust-stable/enable.sh && cd "$0" && cargo "$@"' "$PWD" "$@"
