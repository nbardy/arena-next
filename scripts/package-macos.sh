#!/usr/bin/env bash
# Build a one-executable macOS app bundle without Electron, Node, Chromium,
# or a bundled webview. This script intentionally never deletes an existing
# release; opt into replacement by setting ARENA_NEXT_REPLACE=1.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="${ARENA_NEXT_VERSION:-0.1.0}"
output_dir="${ARENA_NEXT_PACKAGE_DIR:-$project_root/dist}"
app_name="HearthAI.app"
app_path="$output_dir/$app_name"
template="$project_root/packaging/macos/Info.plist"
icon="$project_root/assets/arena-next-icon.png"

# A package must be named for the executable it actually contains. The first
# release is host-native only; a universal build can be added deliberately
# later rather than mislabelling an Intel package as Apple-silicon.
case "${ARENA_NEXT_RUST_TARGET:-}" in
  "" )
    case "$(uname -m)" in
      arm64) rust_target="aarch64-apple-darwin" ; package_arch="arm64" ; expected_lipo_arch="arm64" ;;
      x86_64) rust_target="x86_64-apple-darwin" ; package_arch="x86_64" ; expected_lipo_arch="x86_64" ;;
      *)
        echo "unsupported host architecture: $(uname -m)" >&2
        echo "set ARENA_NEXT_RUST_TARGET to an explicit supported Rust target" >&2
        exit 1
        ;;
    esac
    ;;
  aarch64-apple-darwin) rust_target="aarch64-apple-darwin" ; package_arch="arm64" ; expected_lipo_arch="arm64" ;;
  x86_64-apple-darwin) rust_target="x86_64-apple-darwin" ; package_arch="x86_64" ; expected_lipo_arch="x86_64" ;;
  *)
    echo "unsupported macOS release target: ${ARENA_NEXT_RUST_TARGET}" >&2
    echo "supported targets: aarch64-apple-darwin, x86_64-apple-darwin" >&2
    exit 1
    ;;
esac

zip_path="$output_dir/HearthAI-${version}-macos-${package_arch}.zip"

require_tool() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "required tool not found: $1" >&2
    exit 1
  }
}

for tool in cargo codesign ditto iconutil lipo plutil sips; do
  require_tool "$tool"
done

mkdir -p "$output_dir"
if [[ -e "$app_path" || -e "$zip_path" ]]; then
  if [[ "${ARENA_NEXT_REPLACE:-0}" != "1" ]]; then
    echo "refusing to replace an existing package in $output_dir" >&2
    echo "set ARENA_NEXT_REPLACE=1 to move the prior package aside first" >&2
    exit 1
  fi
  timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
  [[ ! -e "$app_path" ]] || mv "$app_path" "$app_path.backup-$timestamp"
  [[ ! -e "$zip_path" ]] || mv "$zip_path" "$zip_path.backup-$timestamp"
fi

temporary_dir="$(mktemp -d "$output_dir/.arena-next-package.XXXXXX")"
cleanup() { rm -rf "$temporary_dir"; }
trap cleanup EXIT

cd "$project_root"
# The executable's Mach-O deployment target must agree with the documented
# macOS 13 ScreenCaptureKit baseline, not merely the Info.plist declaration.
export MACOSX_DEPLOYMENT_TARGET=13.0
cargo build --release -p arena-next --target "$rust_target"

bundle="$temporary_dir/$app_name"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
binary="$project_root/target/$rust_target/release/arena-next"
ditto "$binary" "$bundle/Contents/MacOS/arena-next"
sed "s/@VERSION@/$version/g" "$template" > "$bundle/Contents/Info.plist"
plutil -lint "$bundle/Contents/Info.plist" >/dev/null

iconset="$temporary_dir/HearthAI.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$icon" --out "$iconset/icon_${size}x${size}.png" >/dev/null
done
sips -z 32 32 "$icon" --out "$iconset/icon_16x16@2x.png" >/dev/null
sips -z 64 64 "$icon" --out "$iconset/icon_32x32@2x.png" >/dev/null
sips -z 256 256 "$icon" --out "$iconset/icon_128x128@2x.png" >/dev/null
sips -z 512 512 "$icon" --out "$iconset/icon_256x256@2x.png" >/dev/null
# The source artwork is 512 px. Do not bloat the distribution with a fake
# 1024 px upscaled variant: Finder has the real 512 px image and the package
# stays substantially smaller for a utility whose full UI is an overlay.
iconutil -c icns "$iconset" -o "$bundle/Contents/Resources/HearthAI.icns"

# Ad-hoc signing makes a local development bundle launchable. A release
# pipeline supplies a Developer ID identity and performs notarization.
if [[ -n "${ARENA_NEXT_SIGN_IDENTITY:-}" ]]; then
  codesign --force --options runtime --timestamp \
    --sign "$ARENA_NEXT_SIGN_IDENTITY" "$bundle"
else
  codesign --force --sign - "$bundle"
fi
codesign --verify --deep --strict --verbose=2 "$bundle"
actual_lipo_arches="$(lipo -archs "$bundle/Contents/MacOS/arena-next")"
if [[ "$actual_lipo_arches" != "$expected_lipo_arch" ]]; then
  echo "packaged executable architecture '$actual_lipo_arches' did not match '$expected_lipo_arch'" >&2
  exit 1
fi

mv "$bundle" "$app_path"
# ArenaNext has no resource forks or extended attributes that need to travel
# with the installer. Excluding them avoids an unnecessary __MACOSX payload in
# the ZIP while leaving the signed app bundle itself unchanged.
ditto -c -k --norsrc --noextattr --keepParent "$app_path" "$zip_path"
echo "created native app: $app_path"
echo "created install archive: $zip_path"
