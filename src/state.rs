use crate::track::Track;
use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::Arc,
};

pub type TrackHandle = Arc<Track>;
pub type StateSlot = arc_swap::ArcSwap<StateSnapshot>;

#[derive(Default, Debug)]
pub struct State {
    pub tracks: HashMap<String, TrackHandle>,
}

pub struct StateGuard {
    ptr: *mut State,
}

unsafe impl Send for StateGuard {}

impl Deref for StateGuard {
    type Target = State;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.ptr }
    }
}

impl DerefMut for StateGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.ptr }
    }
}

#[derive(Clone, Default, Debug)]
pub struct StateSnapshot {
    pub tracks: HashMap<String, TrackHandle>,
}

impl State {
    pub fn lock(&self) -> StateGuard {
        StateGuard {
            ptr: std::ptr::from_ref(self).cast_mut(),
        }
    }

    pub fn snapshot(&self) -> StateSnapshot {
        StateSnapshot {
            tracks: self.tracks.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_default_creates_empty() {
        let state = State::default();
        assert!(state.tracks.is_empty());
    }

    #[test]
    fn state_debug_format() {
        let state = State::default();
        let debug_str = format!("{:?}", state);
        assert!(debug_str.contains("State"));
        assert!(debug_str.contains("tracks"));
    }

    #[test]
    fn state_new_is_default() {
        let state1 = State::default();
        let state2 = State {
            tracks: HashMap::new(),
        };
        assert_eq!(state1.tracks.len(), state2.tracks.len());
    }

    #[test]
    fn state_snapshot_clones_track_handles() {
        let mut state = State::default();
        let track = Arc::new(Track::new("track".to_string(), 1, 1, 0, 0, 64, 48_000.0));
        state.tracks.insert("track".to_string(), track.clone());

        let snapshot = state.snapshot();

        assert!(Arc::ptr_eq(snapshot.tracks.get("track").unwrap(), &track));
    }
}
