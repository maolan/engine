mod audio;
mod audio_codec;
pub mod client;
mod engine;
pub use engine::Engine;
pub mod history;
mod hw;
pub mod kind;
pub mod message;
mod midi;
pub mod mutex;
mod osc;
pub mod plugins;
mod routing;
#[cfg(unix)]
mod rubberband;
pub mod simd;
pub mod state;
mod track;
pub mod workers;
pub use workers::worker;

pub use plugins::clap_proc;
#[cfg(all(unix, not(target_os = "macos")))]
pub use plugins::lv2_proc;
pub use plugins::vst3_proc;

// Re-export plugin info/state types for backward compatibility with the DAW
// and internal engine code.
pub mod clap {
    pub use crate::plugins::types::is_supported_clap_binary;
    pub use crate::plugins::types::{
        ClapMidiOutputEvent, ClapParameterInfo, ClapPluginInfo, ClapPluginState,
    };
}
pub mod vst3 {
    pub use crate::plugins::types::{Vst3PluginInfo, Vst3PluginState};
    pub mod interfaces {
        pub use crate::plugins::types::Vst3GuiInfo;
    }
    pub mod port {
        pub use crate::plugins::types::ParameterInfo;
    }
    pub mod state {
        pub use crate::plugins::types::Vst3PluginState;
    }
}
#[cfg(all(unix, not(target_os = "macos")))]
pub mod lv2 {
    pub use crate::plugins::types::Lv2PluginInfo;
}

use tokio::sync::mpsc::{Sender, channel};
use tokio::task::JoinHandle;

#[cfg(target_os = "macos")]
pub fn discover_coreaudio_devices() -> Vec<String> {
    hw::coreaudio::device::list_devices()
        .into_iter()
        .map(|d| d.name)
        .collect()
}

pub fn init() -> (Sender<message::Message>, JoinHandle<()>) {
    let (tx, rx) = channel::<message::Message>(32);
    let mut engine = engine::Engine::new(rx, tx.clone());
    let handle = tokio::spawn(async move {
        engine.init().await;
        engine.work().await;
    });
    (tx.clone(), handle)
}
