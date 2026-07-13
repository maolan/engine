use crate::track::Track;
use std::{
    cell::UnsafeCell,
    collections::HashMap,
    fmt,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::Arc,
};

pub type TrackHandle = Arc<Track>;
pub type StateSlot = arc_swap::ArcSwap<StateSnapshot>;

pub struct State {
    inner: UnsafeCell<StateData>,
}

#[derive(Default, Debug)]
pub struct StateData {
    pub tracks: HashMap<String, TrackHandle>,
}

pub struct StateGuard<'a> {
    ptr: *mut StateData,
    _marker: PhantomData<&'a mut StateData>,
}

unsafe impl Send for StateGuard<'_> {}

// SAFETY: `State` preserves the legacy engine invariant that mutation is
// externally serialized by the control/runtime path. `lock` returns a guard to
// make interior mutation explicit without retagging a shared reference as
// unique, which Miri rightfully rejects.
unsafe impl Sync for State {}

impl Default for State {
    fn default() -> Self {
        Self {
            inner: UnsafeCell::new(StateData::default()),
        }
    }
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("State")
            .field("tracks", &self.tracks)
            .finish()
    }
}

impl Deref for State {
    type Target = StateData;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.inner.get() }
    }
}

impl DerefMut for State {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.get_mut()
    }
}

impl StateData {
    pub fn snapshot(&self) -> StateSnapshot {
        StateSnapshot {
            tracks: self.tracks.clone(),
        }
    }
}

impl Deref for StateGuard<'_> {
    type Target = StateData;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.ptr }
    }
}

impl DerefMut for StateGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.ptr }
    }
}

#[derive(Clone, Default, Debug)]
pub struct StateSnapshot {
    pub tracks: HashMap<String, TrackHandle>,
}

impl State {
    pub fn lock(&self) -> StateGuard<'_> {
        StateGuard {
            ptr: self.inner.get(),
            _marker: PhantomData,
        }
    }

    pub fn snapshot(&self) -> StateSnapshot {
        StateData::snapshot(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_default_creates_empty() {
        let state = State::default();
        assert!(state.lock().tracks.is_empty());
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
        let state2 = State::default();
        assert_eq!(state1.lock().tracks.len(), state2.lock().tracks.len());
    }

    #[test]
    fn state_snapshot_clones_track_handles() {
        let state = State::default();
        let track = Arc::new(Track::new("track".to_string(), 1, 1, 0, 0, 64, 48_000.0));
        state
            .lock()
            .tracks
            .insert("track".to_string(), track.clone());

        let snapshot = state.snapshot();

        assert!(Arc::ptr_eq(snapshot.tracks.get("track").unwrap(), &track));
    }
}
