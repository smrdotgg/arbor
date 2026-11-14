#!/bin/sh
# Removes what install.sh installed. If Satchel is the default for folders, it first puts the old
# default back (what "Restore the System Default for Folders…" does), with the installed Satchel,
# or the one beside this script when that's gone; with neither, it puts back the D-Bus
# activation file and folder default the backup Satchel saved recorded, itself (a damaged backup
# stops it before anything changes, and stays). Your settings and Jump history
# (~/.config/satchel) stay.
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
if [ -x "$bin/satchel" ]; then
	restorer=$bin/satchel
elif [ -x "$here/satchel" ]; then
	restorer=$here/satchel
fi

if [ -n "$restorer" ]; then
	"$restorer" --restore-folder-default || {
		echo "Couldn't restore the folder default; nothing was removed." >&2
		exit 1
	}
else
	# No Satchel to ask. What Satchel's Restore does, by hand, from the backup Satchel saved: its
	# activation file goes back to what was there (or goes, if nothing was), and the folder
	# default it wrote goes back to what the backup recorded. A backup that's damaged stops it
	# here, before anything changes; the backup goes only once everything is back.
	service=$user_data/dbus-1/services/org.freedesktop.FileManager1.service
	mimeapps=$config/mimeapps.list
	backup=$config/satchel/folder-handler-backup.txt
	header='# What "Make Satchel the Default for Folders" replaced. "Restore the System Default for Folders" puts it back. Keep this file.'
	mime_key='mimeapps.list inode/directory'
	service_key='dbus-1/services/org.freedesktop.FileManager1.service'
	# The value the backup records under a name, decoded from hex ("-" is "there was none":
	# then it prints nothing and fails). The bytes come out of printf's octal escapes, so a
	# NUL is put back too (not every awk can print one).
	saved() {
		hex=$(LC_ALL=C awk -F '\t' -v k="$1" '$2 == k && $1 != "-" { print tolower($1); found = 1 } END { exit !found }' "$backup") || return 1
		esc=$(printf '%s' "$hex" | LC_ALL=C awk '{
			d = "0123456789abcdef"
			for (i = 1; i < length($0); i += 2)
				printf "\\%03o", (index(d, substr($0, i, 1)) - 1) * 16 + index(d, substr($0, i + 1, 1)) - 1
		}')
		# shellcheck disable=SC2059 # the escapes are the format, on purpose
		printf "$esc"
	}
	has_backup=
	if [ -e "$backup" ]; then
		damaged=
		[ -f "$backup" ] && head -n 1 "$backup" | grep -qxF "$header" || damaged=1
		for k in "$mime_key" "$service_key"; do
			awk -F '\t' -v k="$k" '$2 == k { found = 1 } END { exit !found }' "$backup" 2>/dev/null || damaged=1
		done
		# Every record whole, as Satchel writes them: "<hex or ->\t<name>", the hex of whole
		# bytes (none for an empty value), and no name twice. The folder default must be one
		# line of text (UTF-8, no control characters), as Satchel's Restore requires: a line
		# break in it would add lines of its own to mimeapps.list.
		LC_ALL=C awk -F '\t' -v mime="$mime_key" '
			# Well-formed UTF-8 as Unicode defines it: no overlong forms (C0, C1, E0 80-9F,
			# F0 80-8F), no surrogates (ED A0-BF), nothing past U+10FFFF (F4 90-BF, F5-FF), and
			# no C1 controls (C2 80-9F). lo and hi bound the next continuation byte.
			function not_text(h,   d, i, b, need, lo, hi) {
				d = "0123456789abcdef"; h = tolower(h); need = 0
				for (i = 1; i < length(h); i += 2) {
					b = (index(d, substr(h, i, 1)) - 1) * 16 + index(d, substr(h, i + 1, 1)) - 1
					if (need > 0) { if (b < lo || b > hi) return 1; need--; lo = 128; hi = 191; continue }
					if (b < 32 || b == 127) return 1
					if (b < 128) continue
					lo = 128; hi = 191
					if (b >= 194 && b < 224) need = 1
					else if (b >= 224 && b < 240) need = 2
					else if (b >= 240 && b < 245) need = 3
					else return 1
					if (b == 194) lo = 160
					else if (b == 224) lo = 160
					else if (b == 237) hi = 159
					else if (b == 240) lo = 144
					else if (b == 244) hi = 143
				}
				return need > 0
			}
			/^#/ || /^[ \t]*$/ { next }
			NF != 2 || $2 == "" { bad = 1 }
			$1 != "-" && ($1 !~ /^[0-9a-fA-F]*$/ || length($1) % 2) { bad = 1 }
			$2 == mime && $1 != "-" && not_text($1) { bad = 1 }
			seen[$2]++ { bad = 1 }
			END { exit bad }' "$backup" 2>/dev/null || damaged=1
		if [ -n "$damaged" ]; then
			echo "The backup of your folder default ($backup) is damaged, so nothing was changed. Put the folder default back by hand (or reinstall Satchel and run Restore the System Default for Folders), then run this again." >&2
			exit 1
		fi
		has_backup=1
	fi
	if [ -f "$service" ] && head -n 1 "$service" | grep -q '^# Written by Satchel'; then
		if [ -n "$has_backup" ] && saved "$service_key" >"$service.satchel-uninstall"; then
			mv -f "$service.satchel-uninstall" "$service"
			echo "Put back the D-Bus activation file that was there before Satchel ($service)."
		else
			rm -f "$service.satchel-uninstall" "$service"
			echo "Removed Satchel's D-Bus activation file ($service)."
		fi
		if command -v dbus-send >/dev/null 2>&1; then
			dbus-send --session --type=method_call --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.ReloadConfig 2>/dev/null || true
		fi
	elif [ -f "$service" ]; then
		echo "Left $service alone: it isn't Satchel's (it changed since Satchel wrote it)."
	fi
	if [ -f "$mimeapps" ] && grep -q '^inode/directory=satchel\.desktop' "$mimeapps"; then
		# What was there before, if anything was (an empty value is a value, put back as it was).
		before= had=
		if [ -n "$has_backup" ] && before=$(saved "$mime_key"); then
			had=1
		fi
		HAD=$had BEFORE=$before awk '
			/^[ \t]*\[/ { inside = ($0 ~ /^[ \t]*\[Default Applications\][ \t]*$/) }
			inside && /^inode\/directory=satchel\.desktop/ {
				if (ENVIRON["HAD"] != "") print "inode/directory=" ENVIRON["BEFORE"]
				next
			}
			{ print }
		' "$mimeapps" >"$mimeapps.satchel-uninstall"
		mv -f "$mimeapps.satchel-uninstall" "$mimeapps"
		if [ -n "$had" ]; then
			echo "Folders open in $before again (put back in $mimeapps)."
		else
			echo "Removed Satchel as the folder default from $mimeapps; the desktop's own file manager opens folders again."
		fi
	fi
	# Everything is back (a failure above stops the script with the backup kept).
	rm -f "$backup"
fi

rm -f "$bin/satchel" "$data/applications/satchel.desktop"
for n in 16 24 32 48 64 128 256 512; do
	rm -f "$data/icons/hicolor/${n}x${n}/apps/satchel.png"
done
rm -f "$data/icons/hicolor/scalable/apps/satchel.svg"
if command -v update-desktop-database >/dev/null 2>&1; then
	update-desktop-database -q "$data/applications" || true
fi
echo "Removed Satchel. Your settings are still in $config/satchel."
