# BitChord

Native YouTube Music desktop client written in Rust, with egui/eframe for the
interface and rodio/Symphonia for audio playback. Supports Windows, macOS and Linux.

Playback includes offline downloads, synced lyrics, themes, Spotify playlist
transfer, Last.fm, Discord Rich Presence and shared listening through the Jam relay.
Hi-res playback uses the addon sources configured by the listener.

## Build and run

Requires Rust 1.95 or newer and the platform's native development libraries.
On macOS, install the Xcode command-line tools. The
[CI workflow](.github/workflows/check.yml) lists the required Linux development packages.

```sh
cargo run --locked
cargo test --locked
cargo build --release --locked
```

The executable is `target/release/bitchord` (`bitchord.exe` on Windows).
Cargo embeds the required fonts, icons, theme data and YouTube solver scripts;
there is no need to copy `assets/` beside the executable.

Last.fm credentials can be supplied through `LASTFM_API_KEY` and `LASTFM_SECRET`
at build time or runtime. Without them, the app shows Last.fm as unconfigured.

## Build a macOS DMG

Run on macOS with the Xcode command-line tools installed:

```sh
./scripts/build-dmg.sh                 # This Mac's architecture
./scripts/build-dmg.sh universal       # Apple Silicon and Intel
```

For a cross build, install the Rust targets first:

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
```

The script builds a locked release and creates
`dist/bitchord-setup-<version>-macos-<arch>.dmg`, containing `bitchord.app` and an
Applications shortcut. `arm64` and `x86_64` can also be selected explicitly.
`BITCHORD_DMG_DIR` overrides the output directory. Version and minimum macOS
version are read from Cargo and the linked executable.
Publish per-architecture DMGs for the built-in updater; the universal DMG is
intended for manual installation.

Local builds use an ad-hoc signature. For direct distribution, configure a
Developer ID Application identity and an existing `notarytool` Keychain profile:

```sh
BITCHORD_SIGN_IDENTITY='Developer ID Application: NAME (TEAMID)' \
BITCHORD_NOTARY_PROFILE='your-notary-profile' \
./scripts/build-dmg.sh
```

That enables hardened runtime, timestamps, DMG signing, notarization and ticket
stapling. An ad-hoc signature does not provide Gatekeeper distribution approval.
The script verifies the completed DMG before replacing an earlier output.
See Apple's [Mac distribution packaging guide](https://developer.apple.com/documentation/xcode/packaging-mac-software-for-distribution).

## Project layout

```text
Cargo.toml / Cargo.lock  Desktop package and locked dependencies
build.rs                Windows resources and macOS native bridge compilation
src/main.rs             Desktop application entry point
src/                    Application modules and UI
src/native/macos/       Objective-C bridges used by the Rust application
scripts/                macOS DMG build script and bundle metadata
assets/                 Embedded application resources and font licenses
server/                 Standalone Rust Jam relay and smoke client
vendor/block/           Compatibility patch required by souvlaki on macOS
.github/workflows/      Build and test checks
```

The Jam relay is an independent Cargo project. See its [README](server/README.md)
for development and deployment instructions.

## Notes

BitChord is an independent, unofficial client, unaffiliated with YouTube, Google
or Apple. Playback availability depends on the upstream services and configured
sources. Sign-in and verification flows use a system webview.

Existing release downloads are available on
[GitHub Releases](https://github.com/pearleseed/BitChord-PC-Continued/releases).
Build directly with Cargo, or use the macOS DMG script above for drag-to-Applications
installation.

## License

[GPL-3.0](LICENSE). Bundled fonts retain their license files in `assets/fonts/`.
The interface, playback and integrations use egui, rodio, Symphonia and the other
libraries declared in `Cargo.toml`.
