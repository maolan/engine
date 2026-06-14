use crate::midi::io::MidiEvent;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct ClapParameterInfo {
    pub id: u32,
    pub name: String,
    pub module: String,
    pub min_value: f64,
    pub max_value: f64,
    pub default_value: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClapPluginState {
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClapMidiOutputEvent {
    pub port: usize,
    pub event: MidiEvent,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ClapTransportInfo {
    pub transport_sample: usize,
    pub playing: bool,
    pub loop_enabled: bool,
    pub loop_range_samples: Option<(usize, usize)>,
    pub bpm: f64,
    pub tsig_num: u16,
    pub tsig_denom: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClapGuiInfo {
    pub api: String,
    pub supports_embedded: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct ClapParamUpdate {
    pub param_id: u32,
    pub value: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClapPluginInfo {
    pub name: String,
    pub path: String,
    pub capabilities: Option<ClapPluginCapabilities>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClapPluginCapabilities {
    pub has_gui: bool,
    pub gui_apis: Vec<String>,
    pub supports_embedded: bool,
    pub supports_floating: bool,
    pub has_params: bool,
    pub has_state: bool,
    pub audio_inputs: usize,
    pub audio_outputs: usize,
    pub midi_inputs: usize,
    pub midi_outputs: usize,
}

pub fn is_supported_clap_binary(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("clap"))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vst3PluginInfo {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub path: String,
    pub category: String,
    pub version: String,
    pub audio_inputs: usize,
    pub audio_outputs: usize,
    pub has_midi_input: bool,
    pub has_midi_output: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParameterInfo {
    pub id: u32,
    pub title: String,
    pub short_title: String,
    pub units: String,
    pub step_count: i32,
    pub default_value: f64,
    pub flags: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Vst3PluginState {
    pub plugin_id: String,
    pub component_state: Vec<u8>,
    pub controller_state: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vst3GuiInfo {
    pub has_gui: bool,
    pub size: Option<(i32, i32)>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Lv2PluginInfo {
    pub uri: String,
    pub name: String,
    pub class_label: String,
    pub bundle_uri: String,
    pub required_features: Vec<String>,
    pub audio_inputs: usize,
    pub audio_outputs: usize,
    pub midi_inputs: usize,
    pub midi_outputs: usize,
}
