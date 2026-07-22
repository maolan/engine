const DEFAULT_BLOCK_SIZE: usize = 4096;
const IDENTITY_PITCH_EPSILON: f64 = 1.0e-6;

#[derive(Debug)]
pub struct LivePitchShifter {
    sample_rate: u32,
    channels: usize,
    block_size: usize,
    next_input_frame: usize,
    next_output_frame: usize,
    formant_preserved: bool,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
    interleaved: Vec<f32>,
}

impl LivePitchShifter {
    pub fn new(
        sample_rate: usize,
        channels: usize,
        formant_preserved: bool,
    ) -> Result<Self, String> {
        let sample_rate = u32::try_from(sample_rate.max(1))
            .map_err(|_| format!("Sample rate {sample_rate} is too large"))?;
        let channels = channels.max(1);
        let block_size = DEFAULT_BLOCK_SIZE;
        Ok(Self {
            sample_rate,
            channels,
            block_size,
            next_input_frame: 0,
            next_output_frame: 0,
            formant_preserved,
            input: vec![vec![0.0; block_size]; channels],
            output: vec![vec![0.0; block_size]; channels],
            interleaved: vec![0.0; block_size * channels.min(2)],
        })
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn reset(&mut self, output_frame: usize) {
        self.next_input_frame = output_frame;
        self.next_output_frame = output_frame;
        for channel in &mut self.input {
            channel.fill(0.0);
        }
        for channel in &mut self.output {
            channel.fill(0.0);
        }
    }

    pub fn set_formant_preserved(&mut self, formant_preserved: bool) {
        self.formant_preserved = formant_preserved;
    }

    pub fn render<F>(
        &mut self,
        request_start_frame: usize,
        frames: usize,
        mut fill_input: F,
    ) -> Vec<Vec<f32>>
    where
        F: FnMut(usize, &mut [Vec<f32>]) -> f64,
    {
        if self.next_output_frame != request_start_frame {
            self.reset(request_start_frame);
        }

        let mut rendered = vec![vec![0.0; frames]; self.channels];
        let mut written = 0usize;
        while written < frames {
            for channel in &mut self.input {
                channel.fill(0.0);
            }
            let pitch_scale = fill_input(self.next_input_frame, &mut self.input).max(0.01);
            self.pitch_shift_block(pitch_scale);

            let copy_len = self.block_size.min(frames - written);
            for (dst, src) in rendered.iter_mut().zip(&self.output) {
                dst[written..written + copy_len].copy_from_slice(&src[..copy_len]);
            }
            self.next_input_frame = self.next_input_frame.saturating_add(self.block_size);
            self.next_output_frame = self.next_output_frame.saturating_add(copy_len);
            written += copy_len;
        }

        rendered
    }

    fn pitch_shift_block(&mut self, pitch_scale: f64) {
        if (pitch_scale - 1.0).abs() <= IDENTITY_PITCH_EPSILON {
            for (dst, src) in self.output.iter_mut().zip(&self.input) {
                dst.copy_from_slice(src);
            }
            return;
        }

        if self.channels <= 2 {
            self.pitch_shift_interleaved_block(pitch_scale);
        } else {
            self.pitch_shift_planar_channels(pitch_scale);
        }
    }

    fn pitch_shift_interleaved_block(&mut self, pitch_scale: f64) {
        let channels = self.channels;
        self.interleaved.resize(self.block_size * channels, 0.0);
        for frame in 0..self.block_size {
            for channel in 0..channels {
                self.interleaved[frame * channels + channel] = self.input[channel][frame];
            }
        }

        let params = self.params(channels);
        let expected_len = self.block_size * channels;
        let corrected = timestretch::pitch_shift(&self.interleaved, &params, pitch_scale)
            .unwrap_or_else(|error| {
                tracing::warn!("timestretch pitch shift failed: {error}");
                self.interleaved.clone()
            });

        for channel in &mut self.output {
            channel.fill(0.0);
        }
        for frame in 0..self.block_size {
            for channel in 0..channels {
                let sample_index = frame * channels + channel;
                if sample_index < corrected.len().min(expected_len) {
                    self.output[channel][frame] = corrected[sample_index];
                }
            }
        }
    }

    fn pitch_shift_planar_channels(&mut self, pitch_scale: f64) {
        let params = self.params(1);
        for channel in 0..self.channels {
            let corrected = timestretch::pitch_shift(&self.input[channel], &params, pitch_scale)
                .unwrap_or_else(|error| {
                    tracing::warn!("timestretch pitch shift failed: {error}");
                    self.input[channel].clone()
                });
            self.output[channel].fill(0.0);
            let copy_len = self.block_size.min(corrected.len());
            self.output[channel][..copy_len].copy_from_slice(&corrected[..copy_len]);
        }
    }

    fn params(&self, channels: usize) -> timestretch::StretchParams {
        let mut params = timestretch::StretchParams::new(1.0)
            .with_sample_rate(self.sample_rate)
            .with_channels(channels as u32);
        if !self.formant_preserved {
            params.envelope_preservation = false;
            params.envelope_strength = 0.0;
        }
        params
    }
}
