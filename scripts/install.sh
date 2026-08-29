#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}"
bin_dir="${HOME}/.local/bin"

cd "$project_dir"
cargo build --release --locked
install -Dm755 target/release/postbird "$bin_dir/postbird"
install -Dm644 data/io.github.postbird.Mail.desktop "$data_dir/applications/io.github.postbird.Mail.desktop"
install -Dm644 data/io.github.postbird.Mail.svg "$data_dir/icons/hicolor/scalable/apps/io.github.postbird.Mail.svg"
install -Dm644 data/io.github.postbird.Mail.metainfo.xml "$data_dir/metainfo/io.github.postbird.Mail.metainfo.xml"

command -v update-desktop-database >/dev/null && update-desktop-database "$data_dir/applications" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -f -t "$data_dir/icons/hicolor" || true

echo "Postbird installed. Launch it from your application menu or run: $bin_dir/postbird"
