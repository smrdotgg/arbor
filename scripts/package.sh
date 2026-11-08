#!/usr/bin/env bash
# Builds Arbor's package for the platform it runs on, into dist/:
#
#   macOS    dist/Arbor.app (ad-hoc signed) and dist/Arbor-<version>-macos-<arch>.zip
#   Linux    dist/arbor-<version>-linux-<arch>.tar.gz: the binary, arbor.desktop, icons,
#            install.sh and uninstall.sh (installs into ~/.local, no root)
#   Windows  dist/arbor-<version>-windows-<arch>.zip: arbor.exe (icon and version info built
#            in by build.rs), README and LICENSE
#
#   scripts/package.sh                 # builds the `dist` profile first (stripped, LTO)
#   PROFILE=release scripts/package.sh # a quicker build
#   NO_BUILD=1 scripts/package.sh      # package what target/$PROFILE already has
#
# The macOS icon (assets/icon/Arbor.icns) is committed; `scripts/package.sh --icns` makes it
# again from scripts/icon.py with iconutil (macOS only).
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)
profile=${PROFILE:-dist}
arch=$(uname -m)
case "$arch" in arm64 | aarch64) arch=arm64 ;; x86_64 | amd64) arch=x86_64 ;; esac

if [ "${1:-}" = --icns ]; then
	set=$(mktemp -d)/Arbor.iconset
	python3 scripts/icon.py --mac "$set"
	iconutil -c icns "$set" -o assets/icon/Arbor.icns
	rm -rf "$(dirname "$set")"
	echo "assets/icon/Arbor.icns"
	exit 0
fi

if [ "${NO_BUILD:-}" != 1 ]; then
	cargo build --profile "$profile" --locked
fi

out=dist
rm -rf "$out"
mkdir -p "$out"

case "$(uname -s)" in
Darwin)
	app=$out/Arbor.app
	mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
	cp "target/$profile/arbor" "$app/Contents/MacOS/arbor"
	sed "s/@VERSION@/$version/g" packaging/macos/Info.plist >"$app/Contents/Info.plist"
	printf 'APPL????' >"$app/Contents/PkgInfo"
	cp assets/icon/Arbor.icns "$app/Contents/Resources/Arbor.icns"
	plutil -lint "$app/Contents/Info.plist"
	# Ad-hoc: no Apple Developer ID, so no notarization (see the README's Install section).
	codesign --force --sign - --timestamp=none "$app"
	codesign --verify --strict "$app"
	ditto -c -k --sequesterRsrc --keepParent "$app" "$out/Arbor-$version-macos-$arch.zip"
	;;
Linux)
	name=arbor-$version-linux-$arch
	stage=$out/$name
	mkdir -p "$stage/icons"
	cp "target/$profile/arbor" "$stage/arbor"
	cp packaging/linux/arbor.desktop packaging/linux/install.sh packaging/linux/uninstall.sh "$stage/"
	chmod 755 "$stage/arbor" "$stage/install.sh" "$stage/uninstall.sh"
	cp assets/icon/png/arbor-*.png assets/icon/arbor.svg "$stage/icons/"
	cp README.md LICENSE "$stage/"
	tar -C "$out" -czf "$out/$name.tar.gz" "$name"
	rm -rf "$stage"
	;;
MINGW* | MSYS* | CYGWIN* | Windows_NT)
	name=arbor-$version-windows-$arch
	stage=$out/$name
	mkdir -p "$stage"
	cp "target/$profile/arbor.exe" "$stage/arbor.exe"
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
