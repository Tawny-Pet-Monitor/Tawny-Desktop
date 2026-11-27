> [!NOTE]
> **Built with AI.** Tawny is made by its maintainer working with Claude, an AI
> model made by Anthropic. Most of the code, documentation and artwork here was
> written with Claude, under the maintainer's direction. Some people avoid
> AI-built software for ethical, political, professional or personal reasons,
> so you should know that before you install, run or contribute.

# 🦉 Tawny Desktop

Tawny, the private two-way pet monitor, for Linux and Windows. Point a webcam
at your pet (**Monitor**) or watch one from your computer (**Viewer**). It runs
on the same Rust engine as the Android app,
[tawny-core](https://github.com/Tawny-Pet-Monitor/Tawny-Core), and pairs with
it by QR code. No account, no sign-up.

Video and audio go **peer to peer over WebRTC**, encrypted end to end
(DTLS-SRTP).

**Away from home, the two devices find each other over [Nostr](https://github.com/nostr-protocol/nips).**
They leave each other sealed notes on ordinary Nostr relays, so pairing does
not depend on any one company's server. See [Nostr](#nostr) below.

- **Android app:** [Tawny-APK](https://github.com/Tawny-Pet-Monitor/Tawny-APK)
- **Self-hosting:** [Tawny Docker](https://github.com/Tawny-Pet-Monitor/Tawny-Docker)
- **The engine:** [Tawny-Core](https://github.com/Tawny-Pet-Monitor/Tawny-Core)

## Install

| Format | Where |
|---|---|
| Flatpak | Flathub (`io.github.PressF4me.Tawny`, once published), or the `.flatpak` bundle from [Releases](https://github.com/Tawny-Pet-Monitor/Tawny-Desktop/releases) |
| AppImage | [Releases](https://github.com/Tawny-Pet-Monitor/Tawny-Desktop/releases): `chmod +x Tawny_*.AppImage` and run it |
| Windows | [Releases](https://github.com/Tawny-Pet-Monitor/Tawny-Desktop/releases): `Tawny_*_x64-setup.exe`. It installs for your user only, with no admin prompt |

x86_64 only for now.

**AppImage notes**

- The AppImage needs only glibc, ALSA and fontconfig from the system, which
  every desktop Linux has.
- If the system has no FUSE, the AppImage unpacks itself to a temporary
  folder on each launch instead. It still runs, but starts more slowly.
  `sudo apt install fuse3` (or your distro's equivalent) avoids that.
- 0.2.0 has no Chromium inside, so the AppArmor prompt that 0.1.x showed on
  Ubuntu 23.10 and later is gone.

**Windows notes**

- The installer isn't code-signed yet, so SmartScreen says "Windows protected
  your PC". Choose **More info → Run anyway**.
- It replaces 0.1.x in the same place, with the same Start menu entry, and
  keeps your channels.
- If the camera doesn't come on, check **Settings → Privacy & security →
  Camera → Let desktop apps access your camera**.

## Nostr

From 0.1.13, Tawny uses Nostr to introduce the two devices when they are not
on the same network.

- **Only the introduction** goes over the relays. Video and audio go straight
  between the devices.
- **Sealed.** Every note is a gift wrap (NIP-59) encrypted with NIP-44, so a
  relay sees an envelope and nothing inside it. The notes are ephemeral and
  are not stored.
- **Each relay is tested first**, with a sealed note the device sends itself.
- **Your own relays.** "Servers (advanced)", under the channel list, replaces the
  built-in list (`relay.primal.net`, `relay.nostr.net`, `relay.snort.social`).
  **Use only my relays** pairs through yours and nothing else, after a test
  and a confirmation.
- **The Monitor has its own key**, random and kept on the Monitor. The pairing
  QR carries only the public half.
- **A backup stays.** If no relay delivers, the rendezvous service is used
  instead.

## How it's built

Since 0.2.0 Tawny Desktop is a native program, written in Rust. There is no
browser engine inside it any more.

- **The screens** are [Slint](https://slint.dev), drawn with the GPU, or on
  the CPU when there is no usable GPU driver. They follow the system's light or
  dark theme, unless you pick one, and its reduced-motion setting.
- **The engine** is [**tawny-core**](https://github.com/Tawny-Pet-Monitor/Tawny-Core),
  the same Rust library as in the Android app. It does pairing, the security
  code, Nostr and the call logic, so the apps behave the same way.
  `native/core` is a copy of it.
- **Video and sound** run on libwebrtc, Google's WebRTC library, through
  [LiveKit's Rust bindings](https://github.com/livekit/rust-sdks). It uses
  software video codecs only, whatever graphics card the computer has.

```
native/desktop/   the app
  src/            Rust: camera, sound, sessions, the saved channels, recording
  ui/             the Slint screens, fonts, sounds
  lang/           translations (gettext .po files)
  csrc/           a small C layer that gives the recorder libwebrtc's encoders
  packaging/      desktop entry, icon, Flatpak manifest, Windows installer script
  tools/          AppImage and Flatpak build scripts
native/core/      tawny-core, copied from Tawny-Core
```

Your channels and settings are one file in the app's data folder:
`~/.local/share/io.github.PressF4me.Tawny` on Linux and
`%LOCALAPPDATA%\io.github.PressF4me.Tawny` on Windows. On Linux it is
readable by you only. **Updating from 0.1.x keeps every pairing.** On its first start, 0.2.0
reads the old version's storage once, and never changes or sends it.

Only one Tawny runs at a time. A pairing link opened from your browser goes to
the running copy, and it only ever opens "Join now", so nothing joins without
your click.

The 0.1.x app (Tauri and Chromium) is in this repo's history, up to tag
`v0.1.13`.

## Build

From `native/desktop`:

| | |
|---|---|
| Run from source | `cargo run --release` |
| Checks | `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` |
| AppImage | `APPIMAGETOOL=/path/to/appimagetool tools/build-appimage.sh` → `dist/` |
| Flatpak | `tools/build-flatpak.sh` (`--install` to install it for your user) → `dist/` |
| Every screen as a picture | `cargo run --release -- --shot <folder>`: light and dark PNGs, with no display needed |

Linux needs Rust 1.95 or newer, Clang and libclang (the releases use LLVM 21
or 22), `pkg-config`, and the ALSA and fontconfig development headers. The
first build downloads LiveKit's prebuilt libwebrtc. On Windows, build with the
MSVC tools; `.github/workflows/native-windows.yml` does exactly that.

### Flatpak notes

Flathub builds offline. Crates come from
`native/desktop/packaging/flatpak/cargo-sources.json`, so run
`native/desktop/tools/update-cargo-sources.sh` and commit the result whenever `Cargo.lock`
changes. The prebuilt libwebrtc is pinned by its sha256 in the manifest.

## Releasing

- **Windows:** the *Native Windows* workflow runs on every push to `main`
  that touches `native/`. It builds, lints and tests the app, builds the
  installer, installs it silently on the runner, checks every screen there,
  uninstalls it, and keeps `Tawny-<version>-x64-setup.exe` as an artifact.
- **Linux:** the AppImage and the Flatpak bundle are built with the scripts
  above.
- All three are attached to a GitHub release by hand for now.

## License

MIT, see [`LICENSE`](LICENSE). The owlet mascot and the Tawny name are the
app's identity; please use your own if you publish a fork.
