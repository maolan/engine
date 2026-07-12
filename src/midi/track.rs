use super::{clip::MIDIClip, io::MIDIIO};
use arc_swap::ArcSwap;
use std::sync::Arc;

#[derive(Debug)]
pub struct MIDITrack {
    clips: ArcSwap<Vec<Arc<MIDIClip>>>,
    pub ins: Vec<Arc<MIDIIO>>,
    pub outs: Vec<Arc<MIDIIO>>,
}

impl MIDITrack {
    pub fn new(ins: usize, outs: usize) -> Self {
        let mut ret = Self {
            clips: ArcSwap::from_pointee(Vec::new()),
            ins: vec![],
            outs: vec![],
        };
        for _ in 0..ins {
            ret.ins.push(Arc::new(MIDIIO::new()));
        }
        for _ in 0..outs {
            ret.outs.push(Arc::new(MIDIIO::new()));
        }

        ret
    }

    pub fn clips(&self) -> Arc<Vec<Arc<MIDIClip>>> {
        self.clips.load_full()
    }

    pub fn set_clips(&self, clips: Vec<MIDIClip>) {
        self.clips
            .store(Arc::new(clips.into_iter().map(Arc::new).collect()));
    }

    pub fn push_clip(&self, clip: MIDIClip) {
        let mut clips = self.clips();
        Arc::make_mut(&mut clips).push(Arc::new(clip));
        self.clips.store(clips);
    }

    pub fn remove_clip(&self, index: usize) -> Option<Arc<MIDIClip>> {
        let mut clips = self.clips();
        let removed = if index < clips.len() {
            Some(Arc::make_mut(&mut clips).remove(index))
        } else {
            None
        };
        if removed.is_some() {
            self.clips.store(clips);
        }
        removed
    }

    pub fn update_clip<R>(&self, index: usize, f: impl FnOnce(&mut MIDIClip) -> R) -> Option<R> {
        let mut clips = self.clips();
        let clip = Arc::make_mut(&mut clips).get_mut(index)?;
        let ret = f(Arc::make_mut(clip));
        self.clips.store(clips);
        Some(ret)
    }

    pub fn connect_in(&mut self, index: usize, to: Arc<MIDIIO>) -> Result<(), String> {
        if index >= self.ins.len() {
            return Err(format!(
                "Index {} is too high, as there are only {} ins",
                index,
                self.ins.len()
            ));
        }
        let myin = self.ins[index].clone();
        MIDIIO::connect(&myin, &to);
        Ok(())
    }

    pub fn connect_out(&mut self, index: usize, to: Arc<MIDIIO>) -> Result<(), String> {
        if index >= self.outs.len() {
            return Err(format!(
                "Index {} is too high, as there are only {} outs",
                index,
                self.outs.len()
            ));
        }
        let out = self.outs[index].clone();
        MIDIIO::connect(&out, &to);
        Ok(())
    }

    pub fn disconnect_in(&mut self, index: usize, to: &Arc<MIDIIO>) -> Result<(), String> {
        if index >= self.ins.len() {
            return Err(format!(
                "Index {} is too high, as there are only {} ins",
                index,
                self.ins.len()
            ));
        }
        let myin = self.ins[index].clone();
        MIDIIO::disconnect(&myin, to)
    }

    pub fn disconnect_out(&mut self, index: usize, to: &Arc<MIDIIO>) -> Result<(), String> {
        if index >= self.outs.len() {
            return Err(format!(
                "Index {} is too high, as there are only {} outs",
                index,
                self.outs.len()
            ));
        }
        let out = self.outs[index].clone();
        MIDIIO::disconnect(&out, to)
    }

    pub fn process(&self) {}
}
