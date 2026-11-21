# Contributing

## Reporting a problem

Open an [issue](https://github.com/noKikoushi/app-volume-lock/issues/new/choose) with:

- your App Volume Lock version (Settings → App Volume Lock) and Windows version
- the app whose volume isn't held, and its program name (Task Manager → Details)
- what you expected and what happened

If an app isn't caught, the program name matters most: some apps play sound through a helper
program with a different name.

## Building

Needs [Rust](https://rustup.rs) (stable) on Windows.

```
cargo build            # debug build
cargo build --release  # target\release\AppVolumeLock.exe
```

Debug builds run from where they are; release builds install themselves on first run.

## Where things are

| File | What's in it |
|---|---|
| `src/audio.rs` | The worker thread: finds audio sessions, applies Lock / Max / Mute, listens to Windows' notifications |
| `src/ui.rs` | Tray icon and the flyout (layout, drawing, input), Settings page |
| `src/gfx.rs` | Direct2D helpers, the padlock and other icons, the tray icon |
| `src/settings.rs` | `settings.json` |
| `src/startup.rs` | Installing, uninstalling, Start with Windows |
| `src/theme.rs` | Windows accent colour |

## Pull requests

- Keep the app small and single-purpose. Open an issue first for bigger features.
- No network code, no data collection, nothing that touches other programs beyond the audio API.
- Run `cargo build --release` without warnings before sending.
- By contributing you agree that your code is released under the [GPL-3.0](LICENSE).
