use crate::mutex::UnsafeMutex;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MidiEvent {
    pub frame: u32,
    pub data: Vec<u8>,
}

impl MidiEvent {
    pub fn new(frame: u32, data: Vec<u8>) -> Self {
        Self { frame, data }
    }
}

#[derive(Clone, Debug, Default)]
#[allow(clippy::upper_case_acronyms)]
pub struct MIDIIO {
    /// Ports that feed events into this port (consumers see producers here).
    pub sources: Vec<Arc<UnsafeMutex<Box<Self>>>>,
    /// Ports that this port feeds events into (producers see consumers here).
    pub connections: Vec<Arc<UnsafeMutex<Box<Self>>>>,
    pub buffer: Vec<MidiEvent>,
    finished: bool,
}

impl MIDIIO {
    pub fn new() -> Self {
        Self::default()
    }

    /// Connect producer `from` to consumer `to`. Updates both sides so
    /// `to.sources` contains `from` and `from.connections` contains `to`.
    pub fn connect(from: &Arc<UnsafeMutex<Box<Self>>>, to: &Arc<UnsafeMutex<Box<Self>>>) {
        {
            let from_lock = from.lock();
            if !from_lock.connections.iter().any(|c| Arc::ptr_eq(c, to)) {
                from_lock.connections.push(to.clone());
            }
        }
        {
            let to_lock = to.lock();
            if !to_lock.sources.iter().any(|s| Arc::ptr_eq(s, from)) {
                to_lock.sources.push(from.clone());
            }
        }
    }

    /// Disconnect producer `from` from consumer `to`. Removes from both sides.
    pub fn disconnect(
        from: &Arc<UnsafeMutex<Box<Self>>>,
        to: &Arc<UnsafeMutex<Box<Self>>>,
    ) -> Result<(), String> {
        let mut removed = false;
        {
            let from_lock = from.lock();
            let before = from_lock.connections.len();
            from_lock.connections.retain(|c| !Arc::ptr_eq(c, to));
            if from_lock.connections.len() < before {
                removed = true;
            }
        }
        {
            let to_lock = to.lock();
            to_lock.sources.retain(|s| !Arc::ptr_eq(s, from));
        }
        if removed {
            Ok(())
        } else {
            Err("Connection not found".to_string())
        }
    }

    /// Prepare this port for a new processing cycle.
    pub fn setup(&mut self) {
        self.buffer.clear();
        self.finished = false;
    }

    /// Merge events from all connected sources into this port's buffer.
    /// Source buffers are left intact so multiple consumers can read them.
    pub fn process(&mut self) {
        self.buffer.clear();
        for source in &self.sources {
            let source_lock = source.lock();
            self.buffer.extend_from_slice(&source_lock.buffer);
        }
        self.buffer.sort_by_key(|e| e.frame);
        self.finished = true;
    }

    /// Returns true if this port has finished processing this cycle.
    /// A port with no sources is considered ready once it has finished
    /// producing; a port with sources is ready only when all sources have.
    pub fn ready(&self) -> bool {
        if self.sources.is_empty() {
            return self.finished;
        }
        self.sources.iter().all(|s| s.lock().finished)
    }

    /// Mark this port as finished without processing (used by producers
    /// such as track inputs that fill their buffer directly).
    pub fn mark_finished(&mut self) {
        self.finished = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_event_new_sets_fields() {
        let event = MidiEvent::new(42, vec![0x90, 60, 100]);

        assert_eq!(event.frame, 42);
        assert_eq!(event.data, vec![0x90, 60, 100]);
    }

    #[test]
    fn connect_and_disconnect_manage_both_sides() {
        let from = Arc::new(UnsafeMutex::new(Box::new(MIDIIO::new())));
        let to = Arc::new(UnsafeMutex::new(Box::new(MIDIIO::new())));

        MIDIIO::connect(&from, &to);
        assert_eq!(from.lock().connections.len(), 1);
        assert!(Arc::ptr_eq(&from.lock().connections[0], &to));
        assert_eq!(to.lock().sources.len(), 1);
        assert!(Arc::ptr_eq(&to.lock().sources[0], &from));

        assert!(MIDIIO::disconnect(&from, &to).is_ok());
        assert!(from.lock().connections.is_empty());
        assert!(to.lock().sources.is_empty());
    }

    #[test]
    fn disconnect_returns_error_for_missing_connection() {
        let from = Arc::new(UnsafeMutex::new(Box::new(MIDIIO::new())));
        let to = Arc::new(UnsafeMutex::new(Box::new(MIDIIO::new())));

        let err = MIDIIO::disconnect(&from, &to).expect_err("missing connection should error");
        assert_eq!(err, "Connection not found");
    }

    #[test]
    fn disconnect_removes_all_duplicate_connections_for_same_target() {
        let from = Arc::new(UnsafeMutex::new(Box::new(MIDIIO::new())));
        let to = Arc::new(UnsafeMutex::new(Box::new(MIDIIO::new())));

        MIDIIO::connect(&from, &to);
        MIDIIO::connect(&from, &to);

        assert!(MIDIIO::disconnect(&from, &to).is_ok());
        assert!(from.lock().connections.is_empty());
        assert!(to.lock().sources.is_empty());
    }

    #[test]
    fn process_merges_and_sorts_sources() {
        let source_a = Arc::new(UnsafeMutex::new(Box::new(MIDIIO::new())));
        let source_b = Arc::new(UnsafeMutex::new(Box::new(MIDIIO::new())));
        let consumer = Arc::new(UnsafeMutex::new(Box::new(MIDIIO::new())));

        source_a
            .lock()
            .buffer
            .push(MidiEvent::new(10, vec![0x90, 60, 100]));
        source_b
            .lock()
            .buffer
            .push(MidiEvent::new(5, vec![0x80, 60, 100]));

        MIDIIO::connect(&source_a, &consumer);
        MIDIIO::connect(&source_b, &consumer);

        consumer.lock().process();

        let events = consumer.lock().buffer.clone();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].frame, 5);
        assert_eq!(events[1].frame, 10);
    }

    #[test]
    fn ready_requires_all_sources_finished() {
        let source = Arc::new(UnsafeMutex::new(Box::new(MIDIIO::new())));
        let consumer = Arc::new(UnsafeMutex::new(Box::new(MIDIIO::new())));

        MIDIIO::connect(&source, &consumer);

        assert!(!consumer.lock().ready());

        source.lock().mark_finished();
        assert!(consumer.lock().ready());
    }

    #[test]
    fn no_source_port_ready_after_mark_finished() {
        let mut io = MIDIIO::new();
        assert!(!io.ready());
        io.mark_finished();
        assert!(io.ready());
    }
}
