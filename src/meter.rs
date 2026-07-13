use crate::message::SessionSlotState;

#[derive(Clone, Debug, Default)]
pub struct MeterSnapshot {
    pub hw_out_db: Vec<f32>,
    pub track_meters: Vec<(String, Vec<f32>)>,
}

#[derive(Clone, Debug)]
pub struct TransportSnapshot {
    pub sample: usize,
    pub tempo_bpm: f64,
    pub playing: bool,
    pub transport_running: bool,
    pub tsig_num: u16,
    pub tsig_denom: u16,
}

impl Default for TransportSnapshot {
    fn default() -> Self {
        Self {
            sample: 0,
            tempo_bpm: 120.0,
            playing: false,
            transport_running: false,
            tsig_num: 4,
            tsig_denom: 4,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionRuntimeSlotSnapshot {
    pub track_name: String,
    pub scene_index: usize,
    pub state: SessionSlotState,
    pub play_position_samples: usize,
    pub elapsed_samples: usize,
}

#[derive(Clone, Debug, Default)]
pub struct SessionRuntimeSnapshot {
    pub slots: Vec<SessionRuntimeSlotSnapshot>,
}
