# Packaging

## Linux (implemented)

```sh
scripts/package-linux.sh            # release build (thin LTO, stripped)
PROFILE=debug scripts/package-linux.sh   # fast smoke package
```

Produces `target/package/nova-<version>-linux-<arch>.tar.gz` containing:

- `nova` — the binary (headed by default; `nova headless` runs the engine alone)
- `nova.desktop` — XDG desktop entry
- `nova.png` — 1024×1024 app icon (the nova mark from the original app;
  vector source `nova.svg`)
- `install.sh` — installs into `~/.local/{bin,share/applications,share/icons}`

The release profile in the root `Cargo.toml` sets `lto = "thin"` and
`strip = "symbols"` for distribution builds.

## macOS

```sh
scripts/package-macos.sh    # → target/package/nova-<version>-macos-<arch>.dmg
```

Builds the release binary, assembles `Nova.app` (Info.plist + icns), ad-hoc
signs it (set `CODESIGN_IDENTITY` for a real Developer ID), and wraps it in a
dmg. CI runs this on tags (`.github/workflows/release.yml`). The manual steps
it automates, for reference (run on a macOS host — gpui needs Metal; no
cross-build from Linux):

1. Build the universal (or per-arch) binary:
   ```sh
   cargo build --release -p nova --target aarch64-apple-darwin
   cargo build --release -p nova --target x86_64-apple-darwin
   lipo -create -output nova \
     target/aarch64-apple-darwin/release/nova \
     target/x86_64-apple-darwin/release/nova
   ```
2. Assemble the bundle:
   ```sh
   mkdir -p Nova.app/Contents/{MacOS,Resources}
   cp nova Nova.app/Contents/MacOS/nova
   sed "s/__VERSION__/$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')/" \
     dist/macos/Info.plist > Nova.app/Contents/Info.plist
   ```
3. Icon: generate `nova.icns` from `dist/nova.png` (`iconutil`) and place it at
   `Nova.app/Contents/Resources/nova.icns`:
   ```sh
   mkdir nova.iconset && sips -z 256 256 dist/nova.png --out nova.iconset/icon_256x256.png
   iconutil -c icns nova.iconset -o Nova.app/Contents/Resources/nova.icns
   ```
4. Sign + notarize (required for distribution):
   ```sh
   codesign --deep --force --options runtime --sign "Developer ID Application: …" Nova.app
   xcrun notarytool submit Nova.zip --keychain-profile … --wait
   xcrun stapler staple Nova.app
   ```
5. Ship as a `.dmg` (`hdiutil create -volname Nova -srcfolder Nova.app -ov -format UDZO Nova.dmg`).
