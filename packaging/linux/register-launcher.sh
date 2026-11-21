#!/bin/sh
set -eu

if [ "$#" -ne 1 ] || [ ! -f "$1" ] || [ ! -x "$1" ]; then
    printf 'Usage: sh %s /path/to/sonde\n' "$0" >&2
    exit 1
fi

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
binary=$(realpath -- "$1")
data_dir=${XDG_DATA_HOME:-"$HOME/.local/share"}

# An Exec argument is quoted after desktop-entry escaping.
exec_path=$(printf '%s\n' "$binary" |
    sed -e 's/\\/\\\\\\\\/g' -e 's/["$`]/\\\\&/g' -e 's/%/%%/g')

install -Dm644 "$root/crates/sonde-app/assets/sonde.png" \
    "$data_dir/icons/hicolor/256x256/apps/sonde.png"
mkdir -p "$data_dir/applications"

launcher=$(mktemp "$data_dir/applications/.sonde.XXXXXXXX")
trap 'if [ -f "$launcher" ]; then unlink "$launcher"; fi' EXIT
cat >"$launcher" <<EOF
[Desktop Entry]
Type=Application
Name=Sonde
Comment=Browse, query and edit databases
Exec=env "$exec_path"
Icon=sonde
Terminal=false
Categories=Development;Database;
StartupWMClass=sonde
EOF
chmod 644 "$launcher"
mv -f -- "$launcher" "$data_dir/applications/sonde.desktop"
