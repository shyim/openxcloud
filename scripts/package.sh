#!/usr/bin/env bash
# Build native packages; run on the OS being packaged.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --locked --release -p xcloud-app --all-features
# Resolve custom CARGO_TARGET_DIR as well as the workspace default.
target_dir=$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
binary="$target_dir/release/openxcloud"
mkdir -p dist
case "$(uname -s)" in
  Darwin)
    cargo bundle --release -p xcloud-app --format osx --binary-path "$binary"
    app="$target_dir/release/bundle/osx/OpenXCloud.app"
    dylibbundler -b -x "$app/Contents/MacOS/openxcloud" \
      -d "$app/Contents/Frameworks" -p '@executable_path/../Frameworks/' -od -ns
    mkdir -p "$app/Contents/Resources"
    cp LICENSE THIRD_PARTY_NOTICES.md "$app/Contents/Resources/"
    # Reject any remaining build-machine dependency before signing the bundle.
    python3 scripts/check-macos-libs.py "$app"
    find "$app/Contents/Frameworks" -type f -name '*.dylib' -exec codesign --force --sign - {} \;
    codesign --force --sign - "$app"
    codesign --verify --deep --strict "$app"
    ditto -c -k --sequesterRsrc --keepParent "$app" "dist/OpenXCloud-macos-$(uname -m).zip"
    ;;
  Linux)
    cargo bundle --release -p xcloud-app --format deb --binary-path "$binary"
    # cargo-bundle doesn't discover shared-library dependencies. Derive them from
    # the binary and the build distro, then add them to the generated control file.
    stage=$(mktemp -d)
    trap 'rm -rf "$stage"' EXIT
    mkdir -p "$stage/debian"
    printf 'Source: openxcloud\n\nPackage: openxcloud\nArchitecture: any\nDescription: Xbox Cloud Gaming client\n' > "$stage/debian/control"
    deps=$(cd "$stage" && dpkg-shlibdeps -O -e "$binary" | sed -n 's/^shlibs:Depends=//p')
    test -n "$deps"
    version=$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "xcloud-app"))')
    arch=$(dpkg --print-architecture)
    deb="$target_dir/release/bundle/deb/openxcloud_${version}_${arch}.deb"
    dpkg-deb -R "$deb" "$stage/package"
    python3 - "$stage/package/DEBIAN/control" "$deps" <<'PY'
from pathlib import Path
import sys
p = Path(sys.argv[1])
lines = [line for line in p.read_text().splitlines() if not line.startswith('Depends:')]
# Vulkan is loaded dynamically, so dpkg-shlibdeps cannot discover it.
lines.append('Depends: ' + sys.argv[2] + ', libvulkan1')
p.write_text('\n'.join(lines) + '\n')
PY
    mkdir -p "$stage/package/usr/share/doc/openxcloud"
    cp LICENSE THIRD_PARTY_NOTICES.md "$stage/package/usr/share/doc/openxcloud/"
    dpkg-deb --root-owner-group --build "$stage/package" "dist/OpenXCloud-linux-${arch}.deb"
    ;;
  *) echo 'Desktop packaging currently supports macOS and Debian/Ubuntu Linux.' >&2; exit 1 ;;
esac
