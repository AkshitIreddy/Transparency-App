# Building and developing

Transparency App is Windows only (Win32 layered windows, event hooks and
gamma ramps). You need **Rust stable with the MSVC toolchain** on
**Windows 10 or 11**.

## Run and build

```powershell
cargo run                 # debug build
cargo build --release     # target\release\TransparencyApp.exe
target\release\TransparencyApp.exe --self-test
```

The UI lives in `ui/*.slint` and is compiled into the executable by
`build.rs`, together with the icon, version info and manifest in `res/`.

## Tests

```powershell
cargo test
```

The unit tests are pure logic (no windows are created), including the gamma
lifecycle with a fake display driver: dimming, a screen turning off and coming
back under a new device name, restores owed across restarts, and leftover dims
never being taken as the original.

Debug builds have two extra developer modes:

- `TransparencyApp --ui-preview <folder>` renders every page with sample data
  and saves screenshots (nothing on the system is changed).
- `TransparencyApp --drive-test <screen name>` runs the real dimmer against
  one physical screen (for example `LG`) and checks it is dimmed, survives a
  display change without compounding and is restored exactly. The screen dims
  for a few seconds.

## Releases

Pushing a tag that starts with `v` runs the tests, builds the release exe,
smoke tests it and publishes a GitHub Release with the exe attached
(`.github/workflows/release.yml`). Keep the version in `Cargo.toml` and
`res/app.rc` in step with the tag.

## Project layout

```
src/
  main.rs      Entry point: single instance, installer mode, self-test
  app.rs       Core shared by the window, tray and hotkeys
  shell.rs     Hidden shell window: tray, hotkeys, display/lid/power events
  engine.rs    Event-driven transparency engine
  dimmer.rs    Per-screen dimming (gamma plus overlay)
  gamma.rs     Gamma ramps keyed by stable screen id, with owed restores
  config.rs    Settings model, debounced atomic saves, migrations
  win.rs       Win32 helpers and monitor enumeration
  ui.rs        Glue between Slint and the core
  updater.rs   Verified GitHub Release updates and the swap installer
  preview.rs   Debug-only UI preview and hardware drive test
ui/            Slint interface and icons
res/           Windows resources (icon, version info, manifest)
```
