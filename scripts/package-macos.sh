#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

: "${DEVELOPER_ID_APPLICATION:?Set DEVELOPER_ID_APPLICATION to the certificate name or SHA-1 fingerprint}"
: "${NOTARY_PROFILE:?Set NOTARY_PROFILE to a notarytool Keychain profile}"

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
build_number=${BUILD_NUMBER:-1}
dist="$root/dist"
app="$dist/ShellHopper.app"
dmg_root="$dist/dmg"
dmg="$dist/ShellHopper-$version.dmg"

for target in aarch64-apple-darwin x86_64-apple-darwin; do
    rustup target list --installed | grep -qx "$target" || {
        echo "Missing Rust target: $target" >&2
        echo "Install it with: rustup target add $target" >&2
        exit 1
    }
    cargo build --release --target "$target"
done

rm -rf -- "$app" "$dmg_root"
rm -f -- "$dmg"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$dmg_root"

lipo -create \
    "target/aarch64-apple-darwin/release/shellhopper" \
    "target/x86_64-apple-darwin/release/shellhopper" \
    -output "$app/Contents/MacOS/shellhopper"
install -m 644 assets/ShellHopper.icns "$app/Contents/Resources/ShellHopper.icns"

plist="$app/Contents/Info.plist"
/usr/libexec/PlistBuddy \
    -c 'Add :CFBundleDisplayName string ShellHopper' \
    -c 'Add :CFBundleExecutable string shellhopper' \
    -c 'Add :CFBundleIconFile string ShellHopper.icns' \
    -c 'Add :CFBundleIdentifier string com.nicholascheek.shellhopper' \
    -c 'Add :CFBundleName string ShellHopper' \
    -c 'Add :CFBundlePackageType string APPL' \
    -c "Add :CFBundleShortVersionString string $version" \
    -c "Add :CFBundleVersion string $build_number" \
    -c 'Add :LSApplicationCategoryType string public.app-category.utilities' \
    -c 'Add :LSMinimumSystemVersion string 13.0' \
    -c 'Add :NSHighResolutionCapable bool true' \
    "$plist"

codesign --force --options runtime --timestamp \
    --sign "$DEVELOPER_ID_APPLICATION" "$app"
codesign --verify --deep --strict --verbose=2 "$app"

ditto "$app" "$dmg_root/ShellHopper.app"
ln -s /Applications "$dmg_root/Applications"
hdiutil create -volname ShellHopper -srcfolder "$dmg_root" -ov -format UDZO "$dmg"
codesign --force --timestamp --sign "$DEVELOPER_ID_APPLICATION" "$dmg"

xcrun notarytool submit "$dmg" --keychain-profile "$NOTARY_PROFILE" --wait
xcrun stapler staple "$dmg"
xcrun stapler validate "$dmg"
spctl --assess --type open --context context:primary-signature --verbose=4 "$dmg"

echo "Created $dmg"
