use arc_swap::ArcSwap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Debug, Clone)]
pub struct AudioIO {
    pub connections: Arc<ArcSwap<Vec<Arc<Self>>>>,
    pub connection_count: Arc<AtomicUsize>,
    buffer_size: usize,
    pub finished: Arc<AtomicBool>,
}

impl AudioIO {
    pub fn new(size: usize) -> Self {
        Self {
            connections: Arc::new(ArcSwap::from_pointee(vec![])),
            connection_count: Arc::new(AtomicUsize::new(0)),
            buffer_size: size,
            finished: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    pub fn connections(&self) -> Arc<Vec<Arc<Self>>> {
        self.connections.load_full()
    }

    pub fn store_connections(&self, connections: Vec<Arc<Self>>) {
        let len = connections.len();
        self.connections.store(Arc::new(connections));
        self.connection_count.store(len, Ordering::Relaxed);
    }

    pub fn update_connections(&self, update: impl FnOnce(&mut Vec<Arc<Self>>)) {
        let mut connections = self.connections.load_full().as_ref().clone();
        update(&mut connections);
        self.store_connections(connections);
    }

    pub fn connect_directed(from: &Arc<Self>, to: &Arc<Self>) {
        to.update_connections(|connections| {
            if !connections.iter().any(|conn| Arc::ptr_eq(conn, from)) {
                connections.push(from.clone());
            }
        });
    }

    pub fn connect(from: &Arc<Self>, to: &Arc<Self>) {
        Self::connect_directed(from, to);
        Self::connect_directed(to, from);
    }

    pub fn disconnect(from: &Arc<Self>, to: &Arc<Self>) -> Result<(), String> {
        let mut to_conns = to.connections.load_full().as_ref().clone();
        let to_original_len = to_conns.len();
        to_conns.retain(|conn| !Arc::ptr_eq(conn, from));
        to.store_connections(to_conns.clone());

        let mut from_conns = from.connections.load_full().as_ref().clone();
        from_conns.retain(|conn| !Arc::ptr_eq(conn, to));
        from.store_connections(from_conns);

        if to_conns.len() < to_original_len {
            Ok(())
        } else {
            Err("Connection not found".to_string())
        }
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
        let connections = self.connections.load_full();
        for conn in connections.iter() {
            if !conn.finished.load(Ordering::Acquire) {
                return false;
            }
        }
        true
    }
}

impl PartialEq for AudioIO {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.connections, &other.connections)
    }
}

impl Eq for AudioIO {}

#[cfg(test)]
mod tests {
    use super::AudioIO;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

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
        assert!(source.connections().is_empty());
        assert!(dest.connections().is_empty());
    }
}
