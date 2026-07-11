#[cfg(target_os = "macos")]
use crate::clap::ClapMidiOutputEvent;

use std::sync::Arc;

pub struct ClapInstance {
    pub id: usize,
    pub processor: crate::clap_proc::SharedClapProcessor,
}

impl std::fmt::Debug for ClapInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClapInstance")
            .field("id", &self.id)
            .field("processor", &"<SharedClapProcessor>")
            .finish()
    }
}

impl ClapInstance {
    pub(crate) fn new(id: usize, processor: crate::clap_proc::SharedClapProcessor) -> Self {
        Self { id, processor }
    }
}

pub struct Vst3Instance {
    pub id: usize,
    pub processor: crate::vst3_proc::SharedVst3Processor,
}

impl std::fmt::Debug for Vst3Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst3Instance")
            .field("id", &self.id)
            .field("processor", &"<SharedVst3Processor>")
            .finish()
    }
}

impl Vst3Instance {
    pub(crate) fn new(id: usize, processor: crate::vst3_proc::SharedVst3Processor) -> Self {
        Self { id, processor }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub struct Lv2Instance {
    pub id: usize,
    pub processor: crate::lv2_proc::SharedLv2Processor,
}

#[cfg(all(unix, not(target_os = "macos")))]
impl std::fmt::Debug for Lv2Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lv2Instance")
            .field("id", &self.id)
            .field("processor", &"<SharedLv2Processor>")
            .finish()
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl Lv2Instance {
    pub(crate) fn new(id: usize, processor: crate::lv2_proc::SharedLv2Processor) -> Self {
        Self { id, processor }
    }
}

impl crate::connectable::AudioPorts for ClapInstance {
    fn audio_inputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.processor.lock().audio_inputs().to_vec()
    }

    fn audio_outputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.processor.lock().audio_outputs().to_vec()
    }
}

impl crate::connectable::MidiPorts for ClapInstance {
    fn midi_inputs(&self) -> Vec<Arc<crate::midi::io::MIDIIO>> {
        self.processor.lock().midi_input_ports().to_vec()
    }

    fn midi_outputs(&self) -> Vec<Arc<crate::midi::io::MIDIIO>> {
        self.processor.lock().midi_output_ports().to_vec()
    }
}

impl crate::connectable::AudioPorts for Vst3Instance {
    fn audio_inputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.processor.lock().audio_inputs().to_vec()
    }

    fn audio_outputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.processor.lock().audio_outputs().to_vec()
    }
}

impl crate::connectable::MidiPorts for Vst3Instance {
    fn midi_inputs(&self) -> Vec<Arc<crate::midi::io::MIDIIO>> {
        self.processor.lock().midi_input_ports().to_vec()
    }

    fn midi_outputs(&self) -> Vec<Arc<crate::midi::io::MIDIIO>> {
        self.processor.lock().midi_output_ports().to_vec()
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl crate::connectable::AudioPorts for Lv2Instance {
    fn audio_inputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.processor.lock().audio_inputs().to_vec()
    }

    fn audio_outputs(&self) -> Vec<Arc<crate::audio::io::AudioIO>> {
        self.processor.lock().audio_outputs().to_vec()
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl crate::connectable::MidiPorts for Lv2Instance {
    fn midi_inputs(&self) -> Vec<Arc<crate::midi::io::MIDIIO>> {
        self.processor.lock().midi_input_ports().to_vec()
    }

    fn midi_outputs(&self) -> Vec<Arc<crate::midi::io::MIDIIO>> {
        self.processor.lock().midi_output_ports().to_vec()
    }
}
