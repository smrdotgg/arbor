#!/usr/bin/env bash
# Builds Satchel's package for the platform it runs on, into dist/:
#
#   macOS    dist/Satchel.app (ad-hoc signed) and dist/Satchel-<version>-macos-universal.zip: one
#            binary with an Apple silicon and an Intel slice (lipo), both for macOS 11 or later
#   Linux    dist/satchel-<version>-linux-<arch>.tar.gz: the binary, satchel.desktop, icons,
#            install.sh and uninstall.sh (installs into ~/.local, no root)
#   Windows  dist/satchel-<version>-windows-<arch>.zip: satchel.exe (icon and version info built
#            in by build.rs), README and LICENSE
#
#   scripts/package.sh                 # builds the `dist` profile first (stripped, LTO)
#   PROFILE=release scripts/package.sh # a quicker build
#   NO_BUILD=1 scripts/package.sh      # package what target/$PROFILE already has
#   ARCH=native scripts/package.sh     # macOS: this Mac's architecture only (no lipo)
#
# The universal build needs the other architecture's Rust target once:
#   rustup target add x86_64-apple-darwin    # on Apple silicon (aarch64-apple-darwin on Intel)
# This Mac's slice is the usual build (target/$PROFILE); the other one is built with --target.
#
# The macOS icon (assets/icon/Satchel.icns) is committed; `scripts/package.sh --icns` makes it
# again from scripts/icon.py with iconutil (macOS only).
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)
profile=${PROFILE:-dist}
arch=$(uname -m)
case "$arch" in arm64 | aarch64) arch=arm64 ;; x86_64 | amd64) arch=x86_64 ;; esac

if [ "${1:-}" = --icns ]; then
	set=$(mktemp -d)/Satchel.iconset
	python3 scripts/icon.py --mac "$set"
	iconutil -c icns "$set" -o assets/icon/Satchel.icns
	rm -rf "$(dirname "$set")"
	echo "assets/icon/Satchel.icns"
	exit 0
fi

# macOS: both slices are built for macOS 11, the Info.plist's LSMinimumSystemVersion (Big Sur
# is the first macOS on Apple silicon; Intel's default would be 10.12, which nothing tests).
other=
if [ "$(uname -s)" = Darwin ]; then
	export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}
	if [ "${ARCH:-universal}" = universal ]; then
		case "$arch" in arm64) other=x86_64-apple-darwin ;; *) other=aarch64-apple-darwin ;; esac
		if ! rustup target list --installed 2>/dev/null | grep -qx "$other"; then
			echo "package.sh: the universal build needs: rustup target add $other (or ARCH=native)" >&2
			exit 1
		fi
	fi
fi

if [ "${NO_BUILD:-}" != 1 ]; then
	cargo build --profile "$profile" --locked
	if [ -n "$other" ]; then
		cargo build --profile "$profile" --locked --target "$other"
	fi
fi

out=dist
rm -rf "$out"
mkdir -p "$out"

case "$(uname -s)" in
Darwin)
	app=$out/Satchel.app
	mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
	if [ -n "$other" ]; then
		lipo -create "target/$profile/satchel" "target/$other/$profile/satchel" -output "$app/Contents/MacOS/satchel"
		lipo "$app/Contents/MacOS/satchel" -verify_arch arm64 x86_64
		arch=universal
	else
		cp "target/$profile/satchel" "$app/Contents/MacOS/satchel"
	fi
	sed "s/@VERSION@/$version/g" packaging/macos/Info.plist >"$app/Contents/Info.plist"
	printf 'APPL????' >"$app/Contents/PkgInfo"
	cp assets/icon/Satchel.icns "$app/Contents/Resources/Satchel.icns"
	plutil -lint "$app/Contents/Info.plist"
	# Ad-hoc: no Apple Developer ID, so no notarization (see the README's Install section).
	codesign --force --sign - --timestamp=none "$app"
	codesign --verify --strict "$app"
	ditto -c -k --sequesterRsrc --keepParent "$app" "$out/Satchel-$version-macos-$arch.zip"
	;;
Linux)
	name=satchel-$version-linux-$arch
	stage=$out/$name
	mkdir -p "$stage/icons"
	cp "target/$profile/satchel" "$stage/satchel"
	cp packaging/linux/satchel.desktop packaging/linux/install.sh packaging/linux/uninstall.sh "$stage/"
	chmod 755 "$stage/satchel" "$stage/install.sh" "$stage/uninstall.sh"
	cp assets/icon/png/satchel-*.png assets/icon/satchel.svg "$stage/icons/"
	cp README.md LICENSE "$stage/"
	tar -C "$out" -czf "$out/$name.tar.gz" "$name"
	rm -rf "$stage"
	;;
MINGW* | MSYS* | CYGWIN* | Windows_NT)
	name=satchel-$version-windows-$arch
	stage=$out/$name
	mkdir -p "$stage"
	cp "target/$profile/satchel.exe" "$stage/satchel.exe"
	cp README.md LICENSE "$stage/"
	if command -v 7z >/dev/null 2>&1; then
		(cd "$out" && 7z a -tzip -bd "$name.zip" "$name" >/dev/null)
	else
		powershell -NoProfile -Command "Compress-Archive -Path '$stage' -DestinationPath '$out/$name.zip'"
	fi
	rm -rf "$stage"
	;;
*)
	echo "package.sh: no package for $(uname -s)" >&2
	exit 1
	;;
esac
ls -l "$out"
