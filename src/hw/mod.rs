#[cfg(target_os = "linux")]
pub mod alsa;
pub mod common;
pub mod config;
pub mod convert_policy;
#[cfg(target_os = "macos")]
pub mod coreaudio;
pub mod error_fmt;
#[cfg(unix)]
pub mod jack;
pub mod latency;
#[cfg(all(unix, not(target_os = "macos")))]
pub mod midi_hub;
pub mod options;
#[cfg(target_os = "freebsd")]
pub mod oss;
pub mod ports;
#[cfg(target_os = "openbsd")]
pub mod sndio;
pub mod traits;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub use coreaudio::midi as coremidi;
#[cfg(target_os = "freebsd")]
pub use oss::freebsd;
#[cfg(target_os = "windows")]
pub use windows::midi as windows_midi;
#[cfg(target_os = "windows")]
pub use windows::{asio, wasapi};
