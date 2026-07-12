use super::{clip::AudioClip, io::AudioIO};
use arc_swap::ArcSwap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug)]
pub struct AudioTrack {
    clips: ArcSwap<Vec<Arc<AudioClip>>>,
    pub ins: Vec<Arc<AudioIO>>,
    pub outs: Vec<Arc<AudioIO>>,
    finished: AtomicBool,
    processing: AtomicBool,
    buffer_size: usize,
}

impl Clone for AudioTrack {
    fn clone(&self) -> Self {
        Self {
            clips: ArcSwap::from(self.clips()),
            ins: self.ins.clone(),
            outs: self.outs.clone(),
            finished: AtomicBool::new(self.finished()),
            processing: AtomicBool::new(self.processing()),
            buffer_size: self.buffer_size,
        }
    }
}

impl AudioTrack {
    pub fn new(ins_count: usize, outs_count: usize, buffer_size: usize) -> Self {
        let mut ret = Self {
            clips: ArcSwap::from_pointee(Vec::new()),
            ins: Vec::with_capacity(ins_count),
            outs: Vec::with_capacity(outs_count),
            finished: AtomicBool::new(false),
            processing: AtomicBool::new(false),
            buffer_size,
        };
        for _ in 0..ins_count {
            ret.ins.push(Arc::new(AudioIO::new(buffer_size)));
        }
        for _ in 0..outs_count {
            ret.outs.push(Arc::new(AudioIO::new(buffer_size)));
        }
        ret
    }

    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    pub fn clips(&self) -> Arc<Vec<Arc<AudioClip>>> {
        self.clips.load_full()
    }

    pub fn set_clips(&self, clips: Vec<AudioClip>) {
        self.clips
            .store(Arc::new(clips.into_iter().map(Arc::new).collect()));
    }

    pub fn push_clip(&self, clip: AudioClip) {
        let mut clips = self.clips();
        Arc::make_mut(&mut clips).push(Arc::new(clip));
        self.clips.store(clips);
    }

    pub fn remove_clip(&self, index: usize) -> Option<Arc<AudioClip>> {
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

    pub fn update_clip<R>(&self, index: usize, f: impl FnOnce(&mut AudioClip) -> R) -> Option<R> {
        let mut clips = self.clips();
        let clip = Arc::make_mut(&mut clips).get_mut(index)?;
        let ret = f(Arc::make_mut(clip));
        self.clips.store(clips);
        Some(ret)
    }

    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    pub fn set_finished(&self, finished: bool) {
        self.finished.store(finished, Ordering::Release);
    }

    pub fn processing(&self) -> bool {
        self.processing.load(Ordering::Acquire)
    }

    pub fn set_processing(&self, processing: bool) {
        self.processing.store(processing, Ordering::Release);
    }

    pub fn connect_in(&self, index: usize, from: Arc<AudioIO>) -> Result<(), String> {
        if let Some(audio_in) = self.ins.get(index) {
            AudioIO::connect(&from, audio_in);
            Ok(())
        } else {
            Err(format!("Audio input index {} too high", index))
        }
    }

    pub fn connect_out(&self, index: usize, to: Arc<AudioIO>) -> Result<(), String> {
        if let Some(audio_out) = self.outs.get(index) {
            AudioIO::connect(audio_out, &to);
            Ok(())
        } else {
            Err(format!("Audio output index {} too high", index))
        }
    }

    pub fn disconnect_in(&self, index: usize, from: &Arc<AudioIO>) -> Result<(), String> {
        if let Some(audio_in) = self.ins.get(index) {
            AudioIO::disconnect(from, audio_in)
        } else {
            Err(format!("Audio input index {} too high", index))
        }
    }

    pub fn disconnect_out(&self, index: usize, to: &Arc<AudioIO>) -> Result<(), String> {
        if let Some(audio_out) = self.outs.get(index) {
            AudioIO::disconnect(audio_out, to)
        } else {
            Err(format!("Audio output index {} too high", index))
        }
    }

    pub fn process(&mut self) {
        for audio_in in &self.ins {
            audio_in.process();
        }
        for (audio_in, audio_out) in self.ins.iter().zip(self.outs.iter()) {
            let in_samples = audio_in.buffer.lock();
            let mut out_samples = audio_out.buffer.lock();

            out_samples.copy_from_slice(&in_samples);
            audio_out.finished.store(true, Ordering::Release);
        }
        self.set_finished(true);
        self.set_processing(false);
    }

    pub fn setup(&mut self) {
        self.set_finished(false);
        self.set_processing(false);
        for input in &self.ins {
            input.setup();
        }
        for output in &self.outs {
            output.setup();
        }
    }

    pub fn ready(&self) -> bool {
        for input in &self.ins {
            if !input.ready() {
                return false;
            }
        }
        true
    }

    pub fn add_input(&mut self, buffer_size: usize) -> Arc<AudioIO> {
        let io = Arc::new(AudioIO::new(buffer_size));
        self.ins.push(io.clone());
        io
    }

    pub fn add_output(&mut self, buffer_size: usize) -> Arc<AudioIO> {
        let io = Arc::new(AudioIO::new(buffer_size));
        self.outs.push(io.clone());
        io
    }
}
