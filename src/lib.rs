mod audio;
pub mod audio_codec;
pub mod client;
pub mod connectable;
mod engine;
pub use engine::Engine;
pub mod executor;
pub mod history;
mod hw;
pub mod kind;
pub mod loudness;
pub use loudness::LoudnessValues;
pub mod message;
pub mod meter;
pub mod midi;
pub mod modulator;
mod osc;
#[cfg(unix)]
mod pitch_shift;
mod plan_builder;
pub mod plugins;
pub mod render_plan;
mod routing;
pub mod simd;
pub mod state;
mod track;
pub mod triple_buffer;
pub mod workers;
pub use workers::worker;

pub use plugins::clap_proc;
#[cfg(unix)]
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
#[cfg(unix)]
pub mod lv2 {
    pub use crate::plugins::types::Lv2PluginInfo;
}

use tokio::sync::mpsc::{Sender, channel};
use tokio::task::JoinHandle;

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

/// RAII guard that restores the previous Windows timer resolution on drop.
#[cfg(target_os = "windows")]
pub struct WindowsTimerResolutionGuard;

#[cfg(target_os = "windows")]
impl Drop for WindowsTimerResolutionGuard {
    fn drop(&mut self) {
        #[link(name = "winmm")]
        unsafe extern "system" {
            fn timeEndPeriod(period: u32) -> u32;
        }
        unsafe {
            timeEndPeriod(1);
        }
    }
}

/// Request a 1 ms Windows timer resolution so that tokio's 1 ms poll interval
/// (and other waits) actually fire near 1 ms instead of being rounded up to
/// the default ~15.6 ms quantum. Without this, the engine's dependent node
/// chain is processed too slowly on Windows and WASAPI output underruns.
#[cfg(target_os = "windows")]
pub fn enable_windows_high_resolution_timer() -> Option<WindowsTimerResolutionGuard> {
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
    }
    unsafe {
        if timeBeginPeriod(1) == 0 {
            tracing::info!("Windows timer resolution set to 1 ms");
            Some(WindowsTimerResolutionGuard)
        } else {
            tracing::warn!("Failed to set Windows timer resolution to 1 ms");
            None
        }
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
    let command_queue_capacity = num_cpus::get().saturating_mul(4).max(128);
    let (tx, rx) = channel::<message::Message>(command_queue_capacity);
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
