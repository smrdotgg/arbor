#!/bin/sh
# Removes what install.sh installed. If Arbor is the default for folders, it first puts the old
# default back (what "Restore the System Default for Folders…" does), with the installed Arbor,
# or the one beside this script when that's gone; with neither, it takes back Arbor's D-Bus
# activation file and folder default itself, from the backup Arbor saved. Your settings and
# Jump history (~/.config/arbor) stay.
#
#   ./uninstall.sh
#   PREFIX=/some/dir ./uninstall.sh
set -eu
here=$(cd "$(dirname "$0")" && pwd)
if [ -n "${PREFIX:-}" ]; then
	bin=$PREFIX/bin
	data=$PREFIX/share
else
	bin=$HOME/.local/bin
	data=${XDG_DATA_HOME:-$HOME/.local/share}
fi
config=${XDG_CONFIG_HOME:-$HOME/.config}
user_data=${XDG_DATA_HOME:-$HOME/.local/share}

restorer=
if [ -x "$bin/arbor" ]; then
	restorer=$bin/arbor
elif [ -x "$here/arbor" ]; then
	restorer=$here/arbor
fi

if [ -n "$restorer" ]; then
	"$restorer" --restore-folder-default || {
		echo "Couldn't restore the folder default; nothing was removed." >&2
		exit 1
	}
else
	# No Arbor to ask. What Arbor's Restore does, by hand: its activation file goes (only if
	# it's Arbor's), and the folder default it wrote goes back to what the backup recorded.
	service=$user_data/dbus-1/services/org.freedesktop.FileManager1.service
	if [ -f "$service" ] && head -n 1 "$service" | grep -q '^# Written by Arbor'; then
		rm -f "$service"
		echo "Removed Arbor's D-Bus activation file ($service)."
		if command -v dbus-send >/dev/null 2>&1; then
			dbus-send --session --type=method_call --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.ReloadConfig 2>/dev/null || true
		fi
	fi
	mimeapps=$config/mimeapps.list
	backup=$config/arbor/folder-handler-backup.txt
	if [ -f "$mimeapps" ] && grep -q '^inode/directory=arbor\.desktop' "$mimeapps"; then
		before=
		if [ -f "$backup" ]; then
			# The value as the backup records it: hex, a tab, the entry's name ("-" for none).
			before=$(awk -F '\t' '$2 == "mimeapps.list inode/directory" && $1 != "-" {
				d = "0123456789abcdef"; s = ""
				for (i = 1; i < length($1); i += 2)
					s = s sprintf("%c", (index(d, substr($1, i, 1)) - 1) * 16 + index(d, substr($1, i + 1, 1)) - 1)
				print s
			}' "$backup")
		fi
		BEFORE=$before awk '
			/^[ \t]*\[/ { inside = ($0 ~ /^[ \t]*\[Default Applications\][ \t]*$/) }
			inside && /^inode\/directory=arbor\.desktop/ {
				if (ENVIRON["BEFORE"] != "") print "inode/directory=" ENVIRON["BEFORE"]
				next
			}
			{ print }
		' "$mimeapps" >"$mimeapps.arbor-uninstall"
		mv -f "$mimeapps.arbor-uninstall" "$mimeapps"
		if [ -n "$before" ]; then
			echo "Folders open in $before again (put back in $mimeapps)."
		else
			echo "Removed Arbor as the folder default from $mimeapps; the desktop's own file manager opens folders again."
		fi
	fi
	rm -f "$backup"
fi

rm -f "$bin/arbor" "$data/applications/arbor.desktop"
for n in 16 24 32 48 64 128 256 512; do
	rm -f "$data/icons/hicolor/${n}x${n}/apps/arbor.png"
done
rm -f "$data/icons/hicolor/scalable/apps/arbor.svg"
if command -v update-desktop-database >/dev/null 2>&1; then
	update-desktop-database -q "$data/applications" || true
fi
echo "Removed Arbor. Your settings are still in $config/arbor."
