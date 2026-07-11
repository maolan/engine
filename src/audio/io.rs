use crate::mutex::UnsafeMutex;
use crate::simd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Debug, Clone)]
pub struct AudioIO {
    pub connections: Arc<UnsafeMutex<Vec<Arc<Self>>>>,
    pub connection_count: Arc<AtomicUsize>,
    pub buffer: Arc<UnsafeMutex<Vec<f32>>>,
    pub finished: Arc<AtomicBool>,
}

impl AudioIO {
    pub fn new(size: usize) -> Self {
        Self {
            connections: Arc::new(UnsafeMutex::new(vec![])),
            connection_count: Arc::new(AtomicUsize::new(0)),
            buffer: Arc::new(UnsafeMutex::new(vec![0.0; size])),
            finished: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn connect(from: &Arc<Self>, to: &Arc<Self>) {
        let to_len = {
            let conns = to.connections.lock();
            if !conns.iter().any(|conn| Arc::ptr_eq(conn, from)) {
                conns.push(from.clone());
            }
            conns.len()
        };
        to.connection_count.store(to_len, Ordering::Relaxed);

        let from_len = {
            let conns = from.connections.lock();
            if !conns.iter().any(|conn| Arc::ptr_eq(conn, to)) {
                conns.push(to.clone());
            }
            conns.len()
        };
        from.connection_count.store(from_len, Ordering::Relaxed);
    }

    pub fn disconnect(from: &Arc<Self>, to: &Arc<Self>) -> Result<(), String> {
        let to_conns = to.connections.lock();
        let to_original_len = to_conns.len();
        to_conns.retain(|conn| !Arc::ptr_eq(conn, from));
        to.connection_count.store(to_conns.len(), Ordering::Relaxed);

        let from_conns = from.connections.lock();
        from_conns.retain(|conn| !Arc::ptr_eq(conn, to));
        from.connection_count
            .store(from_conns.len(), Ordering::Relaxed);

        if to_conns.len() < to_original_len {
            Ok(())
        } else {
            Err("Connection not found".to_string())
        }
    }

    pub fn process(&self) {
        let local_buf = self.buffer.lock();
        let connections = self.connections.lock();

        match connections.len() {
            0 => {
                local_buf.fill(0.0);
            }
            1 => {
                let source_buf = connections[0].buffer.lock();
                simd::copy_sanitized_inplace(local_buf, source_buf);
                if source_buf.len() < local_buf.len() {
                    local_buf[source_buf.len()..].fill(0.0);
                }
            }
            _ => {
                let mut sources = connections.iter();
                if let Some(first_source) = sources.next() {
                    let source_buf = first_source.buffer.lock();
                    simd::copy_sanitized_inplace(local_buf, source_buf);
                    if source_buf.len() < local_buf.len() {
                        local_buf[source_buf.len()..].fill(0.0);
                    }
                } else {
                    local_buf.fill(0.0);
                }
                for source in sources {
                    let source_buf = source.buffer.lock();
                    simd::add_sanitized_inplace(local_buf, source_buf);
                }
            }
        }
        self.finished.store(true, Ordering::Release);
    }

    pub fn setup(&self) {
        self.finished.store(false, Ordering::Release);
    }

    pub fn ready(&self) -> bool {
        if self.finished.load(Ordering::Acquire) {
            return true;
        }
        if self.connection_count.load(Ordering::Relaxed) == 0 {
            return true;
        }
        for conn in self.connections.lock() {
            if !conn.finished.load(Ordering::Acquire) {
                return false;
            }
        }
        true
    }
}

impl PartialEq for AudioIO {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.buffer, &other.buffer)
    }
}

impl Eq for AudioIO {}

#[cfg(test)]
mod tests {
    use super::AudioIO;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    #[test]
    fn process_with_no_connections_clears_buffer() {
        let io = AudioIO::new(3);
        io.buffer.lock().copy_from_slice(&[1.0, -2.0, 3.0]);

        io.process();

        assert_eq!(io.buffer.lock().as_slice(), &[0.0, 0.0, 0.0]);
        assert!(io.finished.load(Ordering::Relaxed));
    }

    #[test]
    fn process_with_one_connection_copies_source() {
        let source = Arc::new(AudioIO::new(3));
        source.buffer.lock().copy_from_slice(&[0.1, 0.2, 0.3]);
        source.finished.store(true, Ordering::Relaxed);
        let dest = Arc::new(AudioIO::new(3));
        AudioIO::connect(&source, &dest);

        dest.process();

        assert_eq!(dest.buffer.lock().as_slice(), &[0.1, 0.2, 0.3]);
    }

    #[test]
    fn process_with_mismatched_buffer_sizes_copies_overlap_only() {
        let source = Arc::new(AudioIO::new(2));
        source.buffer.lock().copy_from_slice(&[0.5, -0.25]);
        source.finished.store(true, Ordering::Relaxed);
        let dest = Arc::new(AudioIO::new(4));
        dest.buffer.lock().copy_from_slice(&[9.0, 9.0, 9.0, 9.0]);
        AudioIO::connect(&source, &dest);

        dest.process();

        assert_eq!(dest.buffer.lock().as_slice(), &[0.5, -0.25, 0.0, 0.0]);
    }

    #[test]
    fn process_with_multiple_connections_sums_sources() {
        let a = Arc::new(AudioIO::new(3));
        let b = Arc::new(AudioIO::new(3));
        a.buffer.lock().copy_from_slice(&[0.25, 0.5, 0.75]);
        b.buffer.lock().copy_from_slice(&[0.75, 0.5, 0.25]);
        a.finished.store(true, Ordering::Relaxed);
        b.finished.store(true, Ordering::Relaxed);
        let dest = Arc::new(AudioIO::new(3));
        AudioIO::connect(&a, &dest);
        AudioIO::connect(&b, &dest);

        dest.process();

        assert_eq!(dest.buffer.lock().as_slice(), &[1.0, 1.0, 1.0]);
    }

    #[test]
    fn process_sanitizes_non_finite_samples() {
        let a = Arc::new(AudioIO::new(3));
        let b = Arc::new(AudioIO::new(3));
        a.buffer
            .lock()
            .copy_from_slice(&[0.25, f32::NAN, f32::INFINITY]);
        b.buffer
            .lock()
            .copy_from_slice(&[0.75, f32::NEG_INFINITY, 0.25]);
        a.finished.store(true, Ordering::Relaxed);
        b.finished.store(true, Ordering::Relaxed);
        let dest = Arc::new(AudioIO::new(3));
        AudioIO::connect(&a, &dest);
        AudioIO::connect(&b, &dest);

        dest.process();

        assert_eq!(dest.buffer.lock().as_slice(), &[1.0, 0.0, 0.25]);
    }

    #[test]
    fn ready_requires_all_connected_sources_to_finish() {
        let source = Arc::new(AudioIO::new(1));
        let dest = Arc::new(AudioIO::new(1));
        AudioIO::connect(&source, &dest);

        assert!(!dest.ready());
        source.finished.store(true, Ordering::Relaxed);
        assert!(dest.ready());
    }

    #[test]
    fn disconnect_removes_connection_from_both_sides() {
        let source = Arc::new(AudioIO::new(1));
        let dest = Arc::new(AudioIO::new(1));
        AudioIO::connect(&source, &dest);

        AudioIO::disconnect(&source, &dest).expect("disconnect");

        assert_eq!(
            source
                .connection_count
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            dest.connection_count
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(source.connections.lock().is_empty());
        assert!(dest.connections.lock().is_empty());
    }
}
