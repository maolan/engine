# maolan-engine

[![crates.io](https://img.shields.io/crates/v/maolan-engine.svg)](https://crates.io/crates/maolan-engine)

`maolan-engine` is the Rust audio engine that powers Maolan.

It provides:

- Real-time Audio and MIDI track processing
- Timeline-oriented recording and clip playback
- Track routing and plugin graph routing
- Offline bounce and export helpers
- **Out-of-process (OOP) plugin hosting** for CLAP, VST3, and LV2 (Unix) — each plugin runs in a separate OS process for crash isolation
- Platform audio backends for Linux, FreeBSD, macOS (CoreAudio), and Windows (WASAPI)

## Architecture

The engine uses a **two-tier process model**:

1. **Main DAW process** — hosts the UI, audio engine, and project state.
2. **Plugin host processes** — one per plugin instance, launched by the engine. Each host loads the plugin binary in-process and communicates with the DAW via shared-memory IPC (`mmap` on Unix, `CreateFileMappingW` on Windows) and lightweight cross-process events (`pipe` on Unix, named auto-reset events on Windows).

Plugins are never loaded directly into the DAW. If a plugin crashes, only its host process dies; the DAW detects the failure, bypasses the plugin, and continues playback.

## Cargo features

All capabilities beyond core real-time track processing are opt-in cargo
features; the crate builds with `default = []`:

| Feature | Enables | Dependencies |
|---------|---------|--------------|
| `audio` | Audio file decode/encode (import, export, recording takes, freeze/bounce WAV) | `symphonia`, `oxideav-*` |
| `midi` | Standard MIDI File import/export (`.mid` clips and MIDI takes) | `midly` |
| `plugins` | Out-of-process CLAP/VST3/LV2/AU plugin hosting, plugin graphs, plugin messages | `maolan-plugin-protocol` |
| `mixosc` | OSC control-surface automation | `mixosc` |

Without `audio`, engine-rate PCM/WAV clips still play via the codec-free
seekable streaming path, but compressed formats and file export are
unavailable. Without `midi`, MIDI tracks still process internal events, but
`.mid` file I/O is disabled. Without `plugins`, the plugin API is absent
entirely.

## Platform support

| Feature | Linux | FreeBSD | macOS | Windows |
|---------|-------|---------|-------|---------|
| Audio backend | ALSA, JACK | OSS, JACK | CoreAudio | WASAPI |
| CLAP hosting | ✅ OOP | ✅ OOP | ✅ OOP | ✅ OOP |
| VST3 hosting | ✅ OOP | ✅ OOP | ✅ OOP | ✅ OOP |
| LV2 hosting | ✅ OOP | ✅ OOP | ✅ OOP | N/A |
| GUI embedding | X11 | — | — | HWND (`SetParent`) |
