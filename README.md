# maolan-engine

[![crates.io](https://img.shields.io/crates/v/maolan-engine.svg)](https://crates.io/crates/maolan-engine)

`maolan-engine` is the Rust audio engine that powers Maolan.

It provides:

- Audio and MIDI track processing
- Timeline-oriented recording and clip playback
- Track routing and plugin graph routing
- Offline bounce and export helpers
- **Out-of-process (OOP) plugin hosting** for CLAP, VST3, and LV2 (Unix) — each plugin runs in a separate OS process for crash isolation
- Platform audio backends for Linux, macOS, FreeBSD, and Windows (WASAPI)

## Architecture

The engine uses a **two-tier process model**:

1. **Main DAW process** — hosts the UI, audio engine, and project state.
2. **Plugin host processes** — one per plugin instance, launched by the engine. Each host loads the plugin binary in-process and communicates with the DAW via shared-memory IPC (`mmap` on Unix, `CreateFileMappingW` on Windows) and lightweight cross-process events (`pipe` on Unix, named auto-reset events on Windows).

Plugins are never loaded directly into the DAW. If a plugin crashes, only its host process dies; the DAW detects the failure, bypasses the plugin, and continues playback.

## Platform support

| Feature | Linux | macOS | FreeBSD | Windows |
|---------|-------|-------|---------|---------|
| Audio backend | ALSA, JACK | CoreAudio | ALSA, JACK | WASAPI |
| CLAP hosting | ✅ OOP | ✅ OOP | ✅ OOP | ✅ OOP |
| VST3 hosting | ✅ OOP | ✅ OOP | ✅ OOP | ✅ OOP |
| LV2 hosting | ✅ OOP | ❌ | ✅ OOP | N/A |
| GUI embedding | X11 | — | X11 | HWND (`SetParent`) |

This crate is under active development alongside the main Maolan application:

- Repository: <https://github.com/maolan/maolan>

Platform integrations depend on system libraries and host/plugin compatibility.
