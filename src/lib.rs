mod audio;
mod audio_codec;
pub mod client;
pub mod connectable;
mod engine;
pub use engine::Engine;
pub mod executor;
pub mod history;
mod hw;
pub mod kind;
pub mod message;
pub mod meter;
mod midi;
pub mod modulator;
mod osc;
mod plan_builder;
pub mod plugins;
pub mod render_plan;
mod routing;
#[cfg(unix)]
mod rubberband;
pub mod simd;
pub mod state;
mod track;
pub mod triple_buffer;
pub mod workers;
pub use workers::worker;

pub use plugins::clap_proc;
#[cfg(all(unix, not(target_os = "macos")))]
pub use plugins::lv2_proc;
pub use plugins::vst3_proc;

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

pub fn enable_flush_denormals_to_zero() {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        let mut mxcsr: u32 = 0;
        std::arch::asm!("stmxcsr [{}]", in(reg) &mut mxcsr);
        mxcsr |= 0x8040;
        std::arch::asm!("ldmxcsr [{}]", in(reg) &mxcsr);
    }

    #[cfg(target_arch = "aarch64")]
    unsafe {
        let mut fpcr: u64;
        std::arch::asm!("mrs {0}, fpcr", out(reg) fpcr);
        fpcr |= 1 << 24;
        std::arch::asm!("msr fpcr, {0}", in(reg) fpcr);
    }
}

pub type EngineInit = (
    Sender<message::Message>,
    JoinHandle<()>,
    triple_buffer::TripleBufferConsumer<meter::MeterSnapshot>,
    triple_buffer::TripleBufferConsumer<meter::TransportSnapshot>,
    triple_buffer::TripleBufferConsumer<meter::SessionRuntimeSnapshot>,
);

pub fn init() -> EngineInit {
    let (tx, rx) = channel::<message::Message>(32);
    let (meter_producer, meter_consumer) =
        triple_buffer::triple_buffer(meter::MeterSnapshot::default());
    let (transport_producer, transport_consumer) =
        triple_buffer::triple_buffer(meter::TransportSnapshot::default());
    let (session_runtime_producer, session_runtime_consumer) =
        triple_buffer::triple_buffer(meter::SessionRuntimeSnapshot::default());
    let mut engine = engine::Engine::new_with_snapshots(
        rx,
        tx.clone(),
        meter_producer,
        transport_producer,
        session_runtime_producer,
    );
    let handle = tokio::spawn(async move {
        engine.init().await;
        engine.work().await;
    });
    (
        tx.clone(),
        handle,
        meter_consumer,
        transport_consumer,
        session_runtime_consumer,
    )
}
