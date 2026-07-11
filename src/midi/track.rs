use super::{clip::MIDIClip, io::MIDIIO};
use std::sync::Arc;

#[derive(Debug)]
pub struct MIDITrack {
    pub clips: Vec<MIDIClip>,
    pub ins: Vec<Arc<MIDIIO>>,
    pub outs: Vec<Arc<MIDIIO>>,
}

impl MIDITrack {
    pub fn new(ins: usize, outs: usize) -> Self {
        let mut ret = Self {
            clips: vec![],
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
