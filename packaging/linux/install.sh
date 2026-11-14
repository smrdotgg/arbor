#!/bin/sh
# Installs Satchel for this user only, no root needed:
#   ~/.local/bin/satchel
#   ~/.local/share/applications/satchel.desktop   (so the desktop lists it, and it can be the folder default)
#   ~/.local/share/icons/hicolor/*/apps/satchel.png and scalable/apps/satchel.svg
# It doesn't make Satchel the default for folders: that's "Make Satchel the Default for Folders…"
# in Satchel's command palette (Ctrl+K), which "Restore the System Default for Folders…" undoes.
#
#   ./install.sh                 # into ~/.local (or $XDG_DATA_HOME for the desktop file and icons)
#   PREFIX=/some/dir ./install.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
if [ -n "${PREFIX:-}" ]; then
	bin=$PREFIX/bin
	data=$PREFIX/share
else
	bin=$HOME/.local/bin
	data=${XDG_DATA_HOME:-$HOME/.local/share}
fi

mkdir -p "$bin" "$data/applications"
cp "$here/satchel" "$bin/satchel.new"
chmod 755 "$bin/satchel.new"
mv -f "$bin/satchel.new" "$bin/satchel"

# The desktop file runs this copy, wherever PATH points. The path is written as data, never
# into a sed expression (where `&` or `|` would mean something), with the Desktop Entry
# escaping: in Exec, quoted when it has a reserved character ("…", with " ` $ \ escaped), % as
# %%, and every backslash doubled again for the string itself; in TryExec, backslashes doubled.
exe=$bin/satchel
case $exe in
*'
'*)
	echo "Can't install into a path with a line break in it: $bin" >&2
	exit 1
	;;
esac
exec_arg=$(printf '%s' "$exe" | sed -e 's/%/%%/g')
case $exe in
*[[:space:]\"\'\\\>\<~\|\&\;\$\*\?\#\(\)\`]*)
	exec_arg=\"$(printf '%s' "$exec_arg" | sed -e 's/[\\"`$]/\\&/g')\"
	;;
esac
exec_arg=$(printf '%s' "$exec_arg" | sed -e 's/\\/\\\\/g')
try_arg=$(printf '%s' "$exe" | sed -e 's/\\/\\\\/g')
while IFS= read -r line || [ -n "$line" ]; do
	case $line in
	Exec=*) printf '%s\n' "Exec=$exec_arg %F" ;;
	TryExec=*) printf '%s\n' "TryExec=$try_arg" ;;
	*) printf '%s\n' "$line" ;;
	esac
done <"$here/satchel.desktop" >"$data/applications/satchel.desktop"
chmod 644 "$data/applications/satchel.desktop"

for n in 16 24 32 48 64 128 256 512; do
	mkdir -p "$data/icons/hicolor/${n}x${n}/apps"
	cp "$here/icons/satchel-$n.png" "$data/icons/hicolor/${n}x${n}/apps/satchel.png"
done
mkdir -p "$data/icons/hicolor/scalable/apps"
cp "$here/icons/satchel.svg" "$data/icons/hicolor/scalable/apps/satchel.svg"

if command -v update-desktop-database >/dev/null 2>&1; then
	update-desktop-database -q "$data/applications" || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
	gtk-update-icon-cache -q -t -f "$data/icons/hicolor" 2>/dev/null || true
fi

echo "Installed Satchel: $bin/satchel"
case ":$PATH:" in
*":$bin:"*) ;;
*) echo "Note: $bin isn't on your PATH, so type its full path to run it from a terminal." ;;
esac
echo "To open folders in Satchel from other apps, run \"Make Satchel the Default for Folders…\" from its command palette (Ctrl+K)."
