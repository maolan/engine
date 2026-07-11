use crate::audio::io::AudioIO;
use crate::midi::io::MIDIIO;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A set of audio input/output ports.
pub trait AudioPorts {
    fn audio_inputs(&self) -> Vec<Arc<AudioIO>>;
    fn audio_outputs(&self) -> Vec<Arc<AudioIO>>;
}

/// A set of MIDI input/output ports.
pub trait MidiPorts {
    fn midi_inputs(&self) -> Vec<Arc<MIDIIO>>;
    fn midi_outputs(&self) -> Vec<Arc<MIDIIO>>;
}

/// Anything that can participate in the engine's connection graph:
/// tracks, plugins, folder children, etc.
pub trait Connectable: AudioPorts + MidiPorts {}
impl<T: AudioPorts + MidiPorts> Connectable for T {}

/// Identifies a connectable object inside a track's routing graph.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum ConnectableRef {
    TrackInput,
    TrackOutput,
    ChildTrack(String),
    ClapPlugin(usize),
    Vst3Plugin(usize),
    #[cfg(all(unix, not(target_os = "macos")))]
    Lv2Plugin(usize),
}

/// A connection between two `Connectable` objects inside a track.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectableConnection {
    pub from: ConnectableRef,
    pub from_port: usize,
    pub to: ConnectableRef,
    pub to_port: usize,
    pub kind: crate::kind::Kind,
}

fn audio_output_port(connectable: &dyn AudioPorts, port: usize) -> Result<Arc<AudioIO>, String> {
    connectable
        .audio_outputs()
        .get(port)
        .cloned()
        .ok_or_else(|| format!("Audio output port {port} not found"))
}

fn audio_input_port(connectable: &dyn AudioPorts, port: usize) -> Result<Arc<AudioIO>, String> {
    connectable
        .audio_inputs()
        .get(port)
        .cloned()
        .ok_or_else(|| format!("Audio input port {port} not found"))
}

fn midi_output_port(connectable: &dyn MidiPorts, port: usize) -> Result<Arc<MIDIIO>, String> {
    connectable
        .midi_outputs()
        .get(port)
        .cloned()
        .ok_or_else(|| format!("MIDI output port {port} not found"))
}

fn midi_input_port(connectable: &dyn MidiPorts, port: usize) -> Result<Arc<MIDIIO>, String> {
    connectable
        .midi_inputs()
        .get(port)
        .cloned()
        .ok_or_else(|| format!("MIDI input port {port} not found"))
}

/// Connect an audio output port to an audio input port.
pub fn connect_audio(
    source: &dyn AudioPorts,
    source_port: usize,
    target: &dyn AudioPorts,
    target_port: usize,
) -> Result<(), String> {
    let from = audio_output_port(source, source_port)?;
    let to = audio_input_port(target, target_port)?;
    AudioIO::connect(&from, &to);
    Ok(())
}

/// Disconnect an audio output port from an audio input port.
pub fn disconnect_audio(
    source: &dyn AudioPorts,
    source_port: usize,
    target: &dyn AudioPorts,
    target_port: usize,
) -> Result<(), String> {
    let from = audio_output_port(source, source_port)?;
    let to = audio_input_port(target, target_port)?;
    AudioIO::disconnect(&from, &to)
}

/// Connect a MIDI output port to a MIDI input port.
pub fn connect_midi(
    source: &dyn MidiPorts,
    source_port: usize,
    target: &dyn MidiPorts,
    target_port: usize,
) -> Result<(), String> {
    let from = midi_output_port(source, source_port)?;
    let to = midi_input_port(target, target_port)?;
    MIDIIO::connect(&from, &to);
    Ok(())
}

/// Disconnect a MIDI output port from a MIDI input port.
pub fn disconnect_midi(
    source: &dyn MidiPorts,
    source_port: usize,
    target: &dyn MidiPorts,
    target_port: usize,
) -> Result<(), String> {
    let from = midi_output_port(source, source_port)?;
    let to = midi_input_port(target, target_port)?;
    MIDIIO::disconnect(&from, &to)
}

#[cfg(test)]
mod tests {
    use super::{
        AudioPorts, ConnectableConnection, ConnectableRef, MidiPorts, connect_audio, connect_midi,
        disconnect_audio, disconnect_midi,
    };
    use crate::audio::io::AudioIO;
    use crate::midi::io::MIDIIO;
    use std::sync::Arc;

    struct TestNode {
        audio_ins: Vec<Arc<AudioIO>>,
        audio_outs: Vec<Arc<AudioIO>>,
        midi_ins: Vec<Arc<MIDIIO>>,
        midi_outs: Vec<Arc<MIDIIO>>,
    }

    impl TestNode {
        fn new(
            audio_in_count: usize,
            audio_out_count: usize,
            midi_in_count: usize,
            midi_out_count: usize,
            buffer_size: usize,
        ) -> Self {
            Self {
                audio_ins: (0..audio_in_count)
                    .map(|_| Arc::new(AudioIO::new(buffer_size)))
                    .collect(),
                audio_outs: (0..audio_out_count)
                    .map(|_| Arc::new(AudioIO::new(buffer_size)))
                    .collect(),
                midi_ins: (0..midi_in_count)
                    .map(|_| Arc::new(MIDIIO::new()))
                    .collect(),
                midi_outs: (0..midi_out_count)
                    .map(|_| Arc::new(MIDIIO::new()))
                    .collect(),
            }
        }
    }

    impl AudioPorts for TestNode {
        fn audio_inputs(&self) -> Vec<Arc<AudioIO>> {
            self.audio_ins.clone()
        }
        fn audio_outputs(&self) -> Vec<Arc<AudioIO>> {
            self.audio_outs.clone()
        }
    }

    impl MidiPorts for TestNode {
        fn midi_inputs(&self) -> Vec<Arc<MIDIIO>> {
            self.midi_ins.clone()
        }
        fn midi_outputs(&self) -> Vec<Arc<MIDIIO>> {
            self.midi_outs.clone()
        }
    }

    #[test]
    fn connect_audio_links_output_to_input() {
        let source = TestNode::new(0, 1, 0, 0, 4);
        let target = TestNode::new(1, 0, 0, 0, 4);

        connect_audio(&source, 0, &target, 0).unwrap();

        assert!(
            target.audio_ins[0]
                .connections
                .lock()
                .iter()
                .any(|c| Arc::ptr_eq(c, &source.audio_outs[0]))
        );
    }

    #[test]
    fn disconnect_audio_removes_link() {
        let source = TestNode::new(0, 1, 0, 0, 4);
        let target = TestNode::new(1, 0, 0, 0, 4);
        connect_audio(&source, 0, &target, 0).unwrap();

        disconnect_audio(&source, 0, &target, 0).unwrap();

        assert!(target.audio_ins[0].connections.lock().is_empty());
        assert!(source.audio_outs[0].connections.lock().is_empty());
    }

    #[test]
    fn disconnect_audio_errors_when_missing() {
        let source = TestNode::new(0, 1, 0, 0, 4);
        let target = TestNode::new(1, 0, 0, 0, 4);

        let err = disconnect_audio(&source, 0, &target, 0).unwrap_err();
        assert_eq!(err, "Connection not found");
    }

    #[test]
    fn connect_midi_links_output_to_input() {
        let source = TestNode::new(0, 0, 0, 1, 4);
        let target = TestNode::new(0, 0, 1, 0, 4);

        connect_midi(&source, 0, &target, 0).unwrap();

        assert!(
            target.midi_ins[0]
                .sources()
                .iter()
                .any(|s| Arc::ptr_eq(s, &source.midi_outs[0]))
        );
    }

    #[test]
    fn disconnect_midi_removes_link() {
        let source = TestNode::new(0, 0, 0, 1, 4);
        let target = TestNode::new(0, 0, 1, 0, 4);
        connect_midi(&source, 0, &target, 0).unwrap();

        disconnect_midi(&source, 0, &target, 0).unwrap();

        assert!(target.midi_ins[0].sources().is_empty());
        assert!(source.midi_outs[0].connections().is_empty());
    }

    #[test]
    fn connect_invalid_audio_port_errors() {
        let source = TestNode::new(0, 0, 0, 0, 4);
        let target = TestNode::new(1, 0, 0, 0, 4);

        assert!(connect_audio(&source, 0, &target, 0).is_err());
        assert!(connect_audio(&target, 0, &source, 0).is_err());
    }

    #[test]
    fn connectable_ref_serde_round_trip() {
        let refs = vec![
            ConnectableRef::TrackInput,
            ConnectableRef::TrackOutput,
            ConnectableRef::ChildTrack("Child".to_string()),
            ConnectableRef::ClapPlugin(3),
            ConnectableRef::Vst3Plugin(7),
        ];
        for r in refs {
            let serialized = serde_json::to_string(&r).unwrap();
            let deserialized: ConnectableRef = serde_json::from_str(&serialized).unwrap();
            assert_eq!(r, deserialized);
        }
    }

    #[test]
    fn connectable_connection_serde_round_trip() {
        let conn = ConnectableConnection {
            from: ConnectableRef::ChildTrack("Child".to_string()),
            from_port: 0,
            to: ConnectableRef::ClapPlugin(2),
            to_port: 1,
            kind: crate::kind::Kind::Audio,
        };
        let serialized = serde_json::to_string(&conn).unwrap();
        let deserialized: ConnectableConnection = serde_json::from_str(&serialized).unwrap();
        assert_eq!(conn, deserialized);
    }
}
