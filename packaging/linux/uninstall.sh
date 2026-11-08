#!/bin/sh
# Removes what install.sh installed. If Arbor is the default for folders, it first puts the old
# default back (what "Restore the System Default for Folders…" does). Your settings and Jump
# history (~/.config/arbor) stay.
#
#   ./uninstall.sh
#   PREFIX=/some/dir ./uninstall.sh
set -eu
if [ -n "${PREFIX:-}" ]; then
	bin=$PREFIX/bin
	data=$PREFIX/share
else
	bin=$HOME/.local/bin
	data=${XDG_DATA_HOME:-$HOME/.local/share}
fi

if [ -x "$bin/arbor" ]; then
	"$bin/arbor" --restore-folder-default || {
		echo "Couldn't restore the folder default; nothing was removed." >&2
		exit 1
	}
fi

rm -f "$bin/arbor" "$data/applications/arbor.desktop"
for n in 16 24 32 48 64 128 256 512; do
	rm -f "$data/icons/hicolor/${n}x${n}/apps/arbor.png"
done
rm -f "$data/icons/hicolor/scalable/apps/arbor.svg"
if command -v update-desktop-database >/dev/null 2>&1; then
	update-desktop-database -q "$data/applications" || true
fi
echo "Removed Arbor. Your settings are still in ${XDG_CONFIG_HOME:-$HOME/.config}/arbor."
