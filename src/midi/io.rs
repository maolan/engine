use arc_swap::ArcSwap;
use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// MIDI port.
///
/// Lock-free MIDI port shape (see `LOCKLESS.md` Phase 3):
///
/// - `sources` is published RCU-style by control-side connect/disconnect and
///   read on the RT path by `process()`.
/// - `connections` is control-side only (routing queries, disconnect, plan
///   compiler); no RT reader exists.
/// - `buffer`/`finished` are accessed under the plan's single-writer
///   invariant: every port buffer has exactly one writer per cycle, all
///   readers run in later plan nodes, the engine's pre-cycle writes are
///   serialized by cycle start, and its post-cycle reads by cycle
///   completion. That invariant is what makes the `unsafe` accessors sound;
///   it is enforced by the render plan's MIDI edges and checked by
///   `RenderPlan::verify()`.
#[derive(Debug, Default)]
#[allow(clippy::upper_case_acronyms)]
pub struct MIDIIO {
    /// Ports that feed events into this port (consumers see producers here).
    sources: ArcSwap<Vec<Arc<MIDIIO>>>,
    /// Ports that this port feeds events into (producers see consumers here).
    /// Control-side only, COW-published like `sources`.
    connections: ArcSwap<Vec<Arc<MIDIIO>>>,
    buffer: UnsafeCell<Vec<MidiEvent>>,
    finished: AtomicBool,
}

// Safety: the only non-atomic interior mutability is `buffer`, whose access
// discipline is the single-writer/plan-ordered invariant documented on the
// type and on each accessor. Concurrent threads never alias the buffer
// mutably because plan edges serialize the writer before any reader.
unsafe impl Sync for MIDIIO {}

/// Mutable access to a MIDI port's event buffer, handed out by
/// [`MIDIIO::buffer_mut`] under the single-writer invariant. Derefs to
/// `Vec<MidiEvent>`.
#[derive(Debug)]
pub struct MidiBufferMut<'a> {
    buffer: &'a mut Vec<MidiEvent>,
}

impl std::ops::Deref for MidiBufferMut<'_> {
    type Target = Vec<MidiEvent>;

    fn deref(&self) -> &Self::Target {
        self.buffer
    }
}

impl std::ops::DerefMut for MidiBufferMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.buffer
    }
}

impl MIDIIO {
    pub fn new() -> Self {
        Self::default()
    }

    /// Connect producer `from` to consumer `to`. Updates both sides so
    /// `to.sources` contains `from` and `from.connections` contains `to`.
    /// Control-side only; publishes the new `sources` list RCU-style.
    pub fn connect(from: &Arc<MIDIIO>, to: &Arc<MIDIIO>) {
        let mut conns = from.connections.load_full().as_ref().clone();
        if !conns.iter().any(|c| Arc::ptr_eq(c, to)) {
            conns.push(to.clone());
            from.connections.store(Arc::new(conns));
        }
        let mut sources = to.sources.load_full().as_ref().clone();
        if !sources.iter().any(|s| Arc::ptr_eq(s, from)) {
            sources.push(from.clone());
            to.sources.store(Arc::new(sources));
        }
    }

    /// Disconnect producer `from` from consumer `to`. Removes from both sides.
    /// Control-side only.
    pub fn disconnect(from: &Arc<MIDIIO>, to: &Arc<MIDIIO>) -> Result<(), String> {
        let mut removed = false;
        let mut conns = from.connections.load_full().as_ref().clone();
        let before = conns.len();
        conns.retain(|c| !Arc::ptr_eq(c, to));
        if conns.len() < before {
            from.connections.store(Arc::new(conns));
            removed = true;
        }
        let mut sources = to.sources.load_full().as_ref().clone();
        sources.retain(|s| !Arc::ptr_eq(s, from));
        to.sources.store(Arc::new(sources));
        if removed {
            Ok(())
        } else {
            Err("Connection not found".to_string())
        }
    }

    /// Control-side only: record `to` in this port's `connections` without
    /// touching `to.sources` (duplicate-safe). Used by folder/child
    /// reparenting, where events flow by direct buffer writes ordered by
    /// plan edges rather than by source merging.
    pub fn add_connection(&self, to: &Arc<MIDIIO>) {
        let mut conns = self.connections.load_full().as_ref().clone();
        if !conns.iter().any(|c| Arc::ptr_eq(c, to)) {
            conns.push(to.clone());
            self.connections.store(Arc::new(conns));
        }
    }

    /// Control-side snapshot of the ports this port feeds. Used by routing
    /// queries and the plan compiler; never on the RT path.
    pub fn connections(&self) -> Vec<Arc<MIDIIO>> {
        self.connections.load_full().as_ref().clone()
    }

    /// Control-side snapshot of the ports feeding this port.
    pub fn sources(&self) -> Vec<Arc<MIDIIO>> {
        self.sources.load_full().as_ref().clone()
    }

    /// Prepare this port for a new processing cycle.
    ///
    /// # Safety
    /// The caller must be this port's sole writer for the coming cycle and
    /// no reader may be active (single-writer invariant).
    pub unsafe fn setup(&self) {
        // Safety: forwarded from the caller — sole writer, no active reader.
        unsafe { &mut *self.buffer.get() }.clear();
        self.finished.store(false, Ordering::Release);
    }

    /// Merge events from all connected sources into this port's buffer.
    /// Source buffers are left intact so multiple consumers can read them.
    ///
    /// # Safety
    /// The single-writer invariant must hold for this port (sole writer this
    /// cycle), and every source's producer must have completed earlier in
    /// the plan (MIDI edge) or belong to a finished cycle.
    pub unsafe fn process(&self) {
        // Safety: forwarded from the caller.
        let buffer = unsafe { &mut *self.buffer.get() };
        buffer.clear();
        let sources = self.sources.load();
        for source in sources.iter() {
            // Safety: sources are read-only here; their producers completed
            // earlier in the plan (MIDI edge) or in a finished cycle.
            let src = unsafe { &*source.buffer.get() };
            buffer.extend_from_slice(src);
        }
        buffer.sort_by_key(|e| e.frame);
        self.finished.store(true, Ordering::Release);
    }

    /// Read the port's event buffer.
    ///
    /// # Safety
    /// No writer may be active: the buffer's producer must have completed
    /// (plan ordering or cycle boundary).
    pub unsafe fn buffer(&self) -> &[MidiEvent] {
        // Safety: forwarded from the caller.
        unsafe { &*self.buffer.get() }
    }

    /// Write the port's event buffer.
    ///
    /// Returns a guard instead of a bare `&mut` so the signature is not
    /// `&self -> &mut T` (`clippy::mut_from_ref`); the guard derefs to
    /// `Vec<MidiEvent>`, so call sites use it like the old field access.
    ///
    /// # Safety
    /// The caller must be this port's sole writer this cycle and no reader
    /// may be active (single-writer invariant).
    pub unsafe fn buffer_mut(&self) -> MidiBufferMut<'_> {
        // Safety: forwarded from the caller.
        MidiBufferMut {
            buffer: unsafe { &mut *self.buffer.get() },
        }
    }

    /// Returns true if this port has finished processing this cycle.
    /// A port with no sources is considered ready once it has finished
    /// producing; a port with sources is ready only when all sources have.
    pub fn ready(&self) -> bool {
        let sources = self.sources.load();
        if sources.is_empty() {
            return self.finished.load(Ordering::Acquire);
        }
        sources.iter().all(|s| s.finished.load(Ordering::Acquire))
    }

    /// Mark this port as finished without processing (used by producers
    /// such as track inputs that fill their buffer directly).
    pub fn mark_finished(&self) {
        self.finished.store(true, Ordering::Release);
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
        let from = Arc::new(MIDIIO::new());
        let to = Arc::new(MIDIIO::new());

        MIDIIO::connect(&from, &to);
        let from_connections = from.connections();
        assert_eq!(from_connections.len(), 1);
        assert!(Arc::ptr_eq(&from_connections[0], &to));
        let to_sources = to.sources();
        assert_eq!(to_sources.len(), 1);
        assert!(Arc::ptr_eq(&to_sources[0], &from));

        assert!(MIDIIO::disconnect(&from, &to).is_ok());
        assert!(from.connections().is_empty());
        assert!(to.sources().is_empty());
    }

    #[test]
    fn disconnect_returns_error_for_missing_connection() {
        let from = Arc::new(MIDIIO::new());
        let to = Arc::new(MIDIIO::new());

        let err = MIDIIO::disconnect(&from, &to).expect_err("missing connection should error");
        assert_eq!(err, "Connection not found");
    }

    #[test]
    fn disconnect_removes_all_duplicate_connections_for_same_target() {
        let from = Arc::new(MIDIIO::new());
        let to = Arc::new(MIDIIO::new());

        MIDIIO::connect(&from, &to);
        MIDIIO::connect(&from, &to);

        assert!(MIDIIO::disconnect(&from, &to).is_ok());
        assert!(from.connections().is_empty());
        assert!(to.sources().is_empty());
    }

    #[test]
    fn process_merges_and_sorts_sources() {
        let source_a = Arc::new(MIDIIO::new());
        let source_b = Arc::new(MIDIIO::new());
        let consumer = Arc::new(MIDIIO::new());

        // Safety: tests are single-threaded; the single-writer invariant
        // holds trivially.
        unsafe {
            source_a
                .buffer_mut()
                .push(MidiEvent::new(10, vec![0x90, 60, 100]));
            source_b
                .buffer_mut()
                .push(MidiEvent::new(5, vec![0x80, 60, 100]));
        }

        MIDIIO::connect(&source_a, &consumer);
        MIDIIO::connect(&source_b, &consumer);

        // Safety: as above; producers "completed" before the merge.
        unsafe { consumer.process() };

        let events = unsafe { consumer.buffer() }.to_vec();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].frame, 5);
        assert_eq!(events[1].frame, 10);
    }

    #[test]
    fn ready_requires_all_sources_finished() {
        let source = Arc::new(MIDIIO::new());
        let consumer = Arc::new(MIDIIO::new());

        MIDIIO::connect(&source, &consumer);

        assert!(!consumer.ready());

        source.mark_finished();
        assert!(consumer.ready());
    }

    #[test]
    fn no_source_port_ready_after_mark_finished() {
        let io = MIDIIO::new();
        assert!(!io.ready());
        io.mark_finished();
        assert!(io.ready());
    }

    #[test]
    fn setup_clears_buffer_and_finished() {
        let io = MIDIIO::new();
        // Safety: single-threaded test.
        unsafe {
            io.buffer_mut().push(MidiEvent::new(0, vec![0x90, 60, 100]));
            io.mark_finished();
            io.setup();
            assert!(io.buffer().is_empty());
        }
        assert!(!io.ready());
    }
}
