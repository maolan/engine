//! Export format selectors and an incremental WAV writer.
//!
//! [`WavStreamWriter`] writes a RIFF header with placeholder sizes up front,
//! streams samples block by block (quantizing to the requested bit depth
//! with optional dither), and patches the RIFF/fact/data sizes on
//! [`finalize`](WavStreamWriter::finalize). This bounds memory to one block
//! per file when many files are written in parallel, e.g. the multi-stem
//! offline bounce.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

/// Export format selector for one output file of an offline encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEncodeFormat {
    /// Microsoft RIFF/WAVE, integer or float PCM.
    Wav(WavBitDepth),
    /// Native FLAC (`*.flac`). The `u16` is the desired bit depth
    /// (16, 24 or 32).
    Flac(u16),
    /// Ogg-encapsulated FLAC (`*.ogg`). The `u16` is the desired bit
    /// depth (16, 24 or 32).
    OggFlac(u16),
    /// MPEG-1/2/2.5 Layer III (`*.mp3`). Uses a sensible CBR bitrate
    /// chosen from the standard Layer III ladder based on sample rate
    /// and channel count.
    Mp3,
}

/// WAV PCM bit-depth / sample-format choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavBitDepth {
    Int16,
    Int24,
    Int32,
    Float32,
}

/// Dither mode applied when quantising floating-point samples to an
/// integer target format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioDither {
    #[default]
    None,
    Rectangular,
    Triangular,
}

/// Tiny deterministic PRNG used for export dither.
pub(crate) struct DitherRng {
    state: u64,
}

impl DitherRng {
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        // xorshift64*
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        self.state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform random value in [-0.5, 0.5).
    pub(crate) fn uniform_half(&mut self) -> f32 {
        let u = self.next_u64() >> 32;
        (u as f32 / 4_294_967_296.0) - 0.5
    }
}

/// Dither offset in quantization steps for one sample.
pub(crate) fn dither_offset(rng: &mut DitherRng, dither: AudioDither) -> f32 {
    match dither {
        AudioDither::None => 0.0,
        AudioDither::Rectangular => rng.uniform_half(),
        AudioDither::Triangular => rng.uniform_half() + rng.uniform_half(),
    }
}

/// Quantize one sample to an integer scale with dither, returning the
/// scaled value ready for `round()`.
pub(crate) fn quantize_with_dither(
    sample: f32,
    scale: f32,
    rng: &mut DitherRng,
    dither: AudioDither,
) -> f32 {
    let d = dither_offset(rng, dither);
    (sample + d / scale).clamp(-1.0, 1.0) * scale
}

pub struct WavStreamWriter {
    file: BufWriter<File>,
    depth: WavBitDepth,
    dither: AudioDither,
    channels: u16,
    sample_rate: u32,
    block_align: u16,
    data_bytes: u64,
    scratch: Vec<u8>,
    rng: DitherRng,
}

impl WavStreamWriter {
    pub fn create(
        path: &Path,
        channels: usize,
        sample_rate: u32,
        depth: WavBitDepth,
        dither: AudioDither,
    ) -> io::Result<Self> {
        let channels = channels.clamp(1, u16::MAX as usize) as u16;
        let sample_rate = sample_rate.max(1);
        let file = BufWriter::new(File::create(path)?);
        let mut writer = Self {
            file,
            depth,
            dither,
            channels,
            sample_rate,
            block_align: channels * depth.bytes_per_sample() as u16,
            data_bytes: 0,
            scratch: Vec::new(),
            rng: DitherRng::new(0x1234_5678_9abc_defe),
        };
        // Placeholder header; `finalize` rewrites it with the real sizes.
        let header = writer.header(0);
        writer.file.write_all(&header)?;
        Ok(writer)
    }

    /// Append interleaved f32 samples, quantized to this writer's bit depth.
    pub fn write_interleaved_f32(&mut self, samples: &[f32]) -> io::Result<()> {
        self.scratch.clear();
        self.scratch.reserve(
            samples
                .len()
                .saturating_mul(usize::from(self.depth.bytes_per_sample())),
        );
        for &sample in samples {
            let s = sample.clamp(-1.0, 1.0);
            match self.depth {
                WavBitDepth::Int16 => {
                    let scale = i16::MAX as f32;
                    let q = quantize_with_dither(s, scale, &mut self.rng, self.dither)
                        .round()
                        .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                    self.scratch.extend_from_slice(&q.to_le_bytes());
                }
                WavBitDepth::Int24 => {
                    let scale = 8_388_607.0;
                    let q = quantize_with_dither(s, scale, &mut self.rng, self.dither)
                        .round()
                        .clamp(-8_388_608.0, 8_388_607.0) as i32;
                    self.scratch.extend_from_slice(&q.to_le_bytes()[..3]);
                }
                WavBitDepth::Int32 => {
                    let scale = i32::MAX as f32;
                    let q = quantize_with_dither(s, scale, &mut self.rng, self.dither)
                        .round()
                        .clamp(i32::MIN as f32, i32::MAX as f32) as i32;
                    self.scratch.extend_from_slice(&q.to_le_bytes());
                }
                WavBitDepth::Float32 => {
                    self.scratch.extend_from_slice(&s.to_le_bytes());
                }
            }
        }
        self.file.write_all(&self.scratch)?;
        self.data_bytes += self.scratch.len() as u64;
        Ok(())
    }

    /// Patch the header sizes and flush. The file is complete afterwards.
    pub fn finalize(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let header = self.header(self.data_bytes);
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        self.file.flush()?;
        Ok(())
    }

    /// RIFF + WAVE + fmt (+ fact for float) + data chunks, with sizes
    /// filled in from `data_bytes`.
    fn header(&self, data_bytes: u64) -> Vec<u8> {
        let is_float = matches!(self.depth, WavBitDepth::Float32);
        let bits = self.depth.bits_per_sample();
        let byte_rate = self.sample_rate * u32::from(self.block_align);
        let frames = (data_bytes / u64::from(self.block_align)) as u32;
        // PCM: 12 + 24 + 8 = 44 bytes; float adds a 12-byte fact chunk.
        let mut header = Vec::with_capacity(if is_float { 56 } else { 44 });
        let riff_size = if is_float { 48_u32 } else { 36_u32 }.wrapping_add(data_bytes as u32);
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&riff_size.to_le_bytes());
        header.extend_from_slice(b"WAVE");
        header.extend_from_slice(b"fmt ");
        header.extend_from_slice(&16_u32.to_le_bytes());
        header.extend_from_slice(&(if is_float { 3_u16 } else { 1_u16 }).to_le_bytes());
        header.extend_from_slice(&self.channels.to_le_bytes());
        header.extend_from_slice(&self.sample_rate.to_le_bytes());
        header.extend_from_slice(&byte_rate.to_le_bytes());
        header.extend_from_slice(&self.block_align.to_le_bytes());
        header.extend_from_slice(&bits.to_le_bytes());
        if is_float {
            header.extend_from_slice(b"fact");
            header.extend_from_slice(&4_u32.to_le_bytes());
            header.extend_from_slice(&frames.to_le_bytes());
        }
        header.extend_from_slice(b"data");
        header.extend_from_slice(&(data_bytes as u32).to_le_bytes());
        header
    }
}

impl WavBitDepth {
    fn bytes_per_sample(self) -> u8 {
        match self {
            Self::Int16 => 2,
            Self::Int24 => 3,
            Self::Int32 | Self::Float32 => 4,
        }
    }

    fn bits_per_sample(self) -> u16 {
        u16::from(self.bytes_per_sample()) * 8
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "audio")]
    fn round_trip(depth: super::WavBitDepth, dither: super::AudioDither) {
        use super::WavStreamWriter;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("maolan_wav_stream_{nanos}.wav"));
        let samples: Vec<f32> = (0..258).map(|i| (i as f32 - 129.0) * 0.001).collect();
        let mut writer =
            WavStreamWriter::create(&path, 2, 48_000, depth, dither).expect("create writer");
        // Write across several oddly sized blocks to exercise chunking.
        for block in samples.chunks(37) {
            writer.write_interleaved_f32(block).expect("write block");
        }
        writer.finalize().expect("finalize");
        let (decoded, channels, rate) =
            crate::audio_codec::decode_audio_to_f32_interleaved_sync(&path)
                .expect("decode streamed wav");
        std::fs::remove_file(&path).expect("remove streamed wav");
        assert_eq!(channels, 2);
        assert_eq!(rate, 48_000);
        assert_eq!(decoded.len(), samples.len());
        let quantum = 2_f32.powi(1 - i32::from(super::WavBitDepth::bits_per_sample(depth)));
        for (a, b) in decoded.iter().zip(samples.iter()) {
            // Rounding plus up to two LSB of dither.
            assert!(
                (a - b).abs() <= quantum * 3.0,
                "sample {a} deviates from {b} by more than rounding plus dither"
            );
        }
    }

    #[cfg(feature = "audio")]
    #[test]
    fn stream_writer_round_trips_float_wav() {
        use super::{AudioDither, WavBitDepth};
        round_trip(WavBitDepth::Float32, AudioDither::None);
    }

    #[cfg(feature = "audio")]
    #[test]
    fn stream_writer_round_trips_int16_wav() {
        use super::{AudioDither, WavBitDepth};
        round_trip(WavBitDepth::Int16, AudioDither::Triangular);
    }

    #[cfg(feature = "audio")]
    #[test]
    fn stream_writer_round_trips_int24_wav() {
        use super::{AudioDither, WavBitDepth};
        round_trip(WavBitDepth::Int24, AudioDither::Rectangular);
    }
}
