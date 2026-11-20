#!/bin/sh
# Build and install BubbleFairy as a real Flatpak for the current user (local test build).
# One-time: flatpak install --user flathub org.flatpak.Builder org.gnome.Sdk//51 org.gnome.Platform//51 org.freedesktop.Sdk.Extension.rust-stable//26.08
# Then run it with: flatpak run io.github.theteknocat.BubbleFairy
set -e
cd "$(dirname "$0")/.."
exec flatpak run --user org.flatpak.Builder --user --install --force-clean \
  --state-dir=target/flatpak-state target/flatpak-build flatpak/io.github.theteknocat.BubbleFairy.yml
