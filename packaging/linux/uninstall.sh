#!/bin/sh
# Removes what install.sh installed. If Arbor is the default for folders, it first puts the old
# default back (what "Restore the System Default for Folders…" does), with the installed Arbor,
# or the one beside this script when that's gone; with neither, it puts back the D-Bus
# activation file and folder default the backup Arbor saved recorded, itself (a damaged backup
# stops it before anything changes, and stays). Your settings and Jump history
# (~/.config/arbor) stay.
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
	# No Arbor to ask. What Arbor's Restore does, by hand, from the backup Arbor saved: its
	# activation file goes back to what was there (or goes, if nothing was), and the folder
	# default it wrote goes back to what the backup recorded. A backup that's damaged stops it
	# here, before anything changes; the backup goes only once everything is back.
	service=$user_data/dbus-1/services/org.freedesktop.FileManager1.service
	mimeapps=$config/mimeapps.list
	backup=$config/arbor/folder-handler-backup.txt
	header='# What "Make Arbor the Default for Folders" replaced. "Restore the System Default for Folders" puts it back. Keep this file.'
	mime_key='mimeapps.list inode/directory'
	service_key='dbus-1/services/org.freedesktop.FileManager1.service'
	# The value the backup records under a name, decoded from hex ("-" is "there was none":
	# then it prints nothing and fails).
	saved() {
		LC_ALL=C awk -F '\t' -v k="$1" '$2 == k && $1 != "-" {
			d = "0123456789abcdef"; s = ""; h = tolower($1)
			for (i = 1; i < length(h); i += 2)
				s = s sprintf("%c", (index(d, substr(h, i, 1)) - 1) * 16 + index(d, substr(h, i + 1, 1)) - 1)
			printf "%s", s; found = 1
		} END { exit !found }' "$backup"
	}
	has_backup=
	if [ -e "$backup" ]; then
		damaged=
		[ -f "$backup" ] && head -n 1 "$backup" | grep -qxF "$header" || damaged=1
		for k in "$mime_key" "$service_key"; do
			awk -F '\t' -v k="$k" '$2 == k { found = 1 } END { exit !found }' "$backup" 2>/dev/null || damaged=1
		done
		# Every record whole, as Arbor writes them: "<hex or ->\t<name>", the hex of whole
		# bytes, and no name twice.
		LC_ALL=C awk -F '\t' '
			/^#/ || /^[ \t]*$/ { next }
			NF != 2 || $2 == "" { bad = 1 }
			$1 != "-" && ($1 !~ /^[0-9a-fA-F]+$/ || length($1) % 2) { bad = 1 }
			seen[$2]++ { bad = 1 }
			END { exit bad }' "$backup" 2>/dev/null || damaged=1
		if [ -n "$damaged" ]; then
			echo "The backup of your folder default ($backup) is damaged, so nothing was changed. Put the folder default back by hand (or reinstall Arbor and run Restore the System Default for Folders), then run this again." >&2
			exit 1
		fi
		has_backup=1
	fi
	if [ -f "$service" ] && head -n 1 "$service" | grep -q '^# Written by Arbor'; then
		if [ -n "$has_backup" ] && saved "$service_key" >"$service.arbor-uninstall"; then
			mv -f "$service.arbor-uninstall" "$service"
			echo "Put back the D-Bus activation file that was there before Arbor ($service)."
		else
			rm -f "$service.arbor-uninstall" "$service"
			echo "Removed Arbor's D-Bus activation file ($service)."
		fi
		if command -v dbus-send >/dev/null 2>&1; then
			dbus-send --session --type=method_call --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.ReloadConfig 2>/dev/null || true
		fi
	elif [ -f "$service" ]; then
		echo "Left $service alone: it isn't Arbor's (it changed since Arbor wrote it)."
	fi
	if [ -f "$mimeapps" ] && grep -q '^inode/directory=arbor\.desktop' "$mimeapps"; then
		before=
		if [ -n "$has_backup" ]; then
			before=$(saved "$mime_key" || true)
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
	# Everything is back (a failure above stops the script with the backup kept).
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
