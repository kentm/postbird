#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
bin_dir="${HOME}/.local/bin"

cd "$project_dir"
if ! command -v rsvg-convert >/dev/null; then
  echo "Installing Postbird's icons requires rsvg-convert (the librsvg package)." >&2
  exit 1
fi
cargo build --release --locked

# GTK's symbolic SVG loader does not preserve Lucide's stroked outlines.
# Rasterize with librsvg, then let GTK recolor the monochrome alpha masks.
# Multiple sizes keep the icons sharp at different display scales.
icon_build_dir="$project_dir/target/postbird-icons"
icon_sizes=(16 22 24 32 48 64 96 128)
for size in "${icon_sizes[@]}"; do
  mkdir -p "$icon_build_dir/${size}x${size}"
  for icon in src/icons/*.svg; do
    name="$(basename "$icon" .svg)"
    rsvg-convert --width "$size" --height "$size" "$icon" \
      --output "$icon_build_dir/${size}x${size}/postbird-${name}-symbolic.symbolic.png"
  done
done

install -Dm755 target/release/postbird "$bin_dir/postbird"
install -Dm644 data/io.github.postbird.Mail.desktop "$data_dir/applications/io.github.postbird.Mail.desktop"
install -Dm644 data/io.github.postbird.Mail.svg "$data_dir/icons/hicolor/scalable/apps/io.github.postbird.Mail.svg"
install -Dm644 data/io.github.postbird.Mail.metainfo.xml "$data_dir/metainfo/io.github.postbird.Mail.metainfo.xml"
for icon in src/icons/*.svg; do
  name="$(basename "$icon" .svg)"
  for size in "${icon_sizes[@]}"; do
    install -Dm644 "$icon_build_dir/${size}x${size}/postbird-${name}-symbolic.symbolic.png" \
      "$data_dir/icons/hicolor/${size}x${size}/actions/postbird-${name}-symbolic.symbolic.png"
  done
  # Remove the old SVG so GTK cannot prefer it over the corrected PNGs.
  rm -f "$data_dir/icons/hicolor/scalable/actions/postbird-${name}-symbolic.svg"
done
install -Dm644 src/icons/LICENSE "$data_dir/doc/postbird/LICENSE.lucide"

command -v update-desktop-database >/dev/null && update-desktop-database "$data_dir/applications" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -f -t "$data_dir/icons/hicolor" || true

echo "Postbird installed. Launch it from your application menu or run: $bin_dir/postbird"
