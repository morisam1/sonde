#!/bin/sh
# Install Sonde from this release folder for the current user: the program
# in ~/.local/bin, and a launcher with its icon in the desktop's app menu.
# Nothing needs root. Run it again after a manual update; Sonde's own
# updater replaces ~/.local/bin/sonde in place.
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
bin_dir=${XDG_BIN_HOME:-"$HOME/.local/bin"}
data_dir=${XDG_DATA_HOME:-"$HOME/.local/share"}

install -Dm755 "$here/sonde" "$bin_dir/sonde"
install -Dm644 "$here/sonde.png" "$data_dir/icons/hicolor/256x256/apps/sonde.png"
mkdir -p "$data_dir/applications"
cat >"$data_dir/applications/sonde.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Sonde
Comment=Browse, query and edit databases
Exec="$bin_dir/sonde"
Icon=sonde
Terminal=false
Categories=Development;Database;
StartupWMClass=sonde
DESKTOP

echo "Installed Sonde to $bin_dir/sonde."
case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) echo "Add $bin_dir to your PATH to start it from a terminal." ;;
esac
