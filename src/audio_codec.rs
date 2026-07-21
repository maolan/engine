use std::io::{self, Write};
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, Frame, MediaType, Packet, RuntimeContext, SampleFormat,
    StreamInfo, TimeBase,
};

/// Export format selector for [`encode_audio_to_file`].
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

/// Decode an audio file to interleaved `f32` samples.
///
/// The format is auto-detected by Symphonia for `.wav`, `.flac`, `.mp3`,
/// `.ogg`/`.vorbis`, `.m4a`/`.aac`/`.alac`, and friends.
/// Returns `(samples, channels, sample_rate)`.
/// All decode paths emit samples in the range `[-1.0, 1.0]`.
pub fn decode_audio_to_f32_interleaved_sync(path: &Path) -> io::Result<(Vec<f32>, usize, u32)> {
    decode_with_symphonia(path)
}

/// Decode a WAV file preferentially, falling back to the general decoder.
///
/// This used to short-circuit `.wav` inputs to a dedicated path. Symphonia
/// handles WAV natively, so this now simply delegates to the unified decoder.
pub fn decode_audio_to_f32_interleaved_preferring_wav(
    path: &Path,
) -> io::Result<(Vec<f32>, usize, u32)> {
    decode_audio_to_f32_interleaved_sync(path)
}

// ---------------------------------------------------------------------------
// Symphonia decode (WAV, FLAC, MP3, Vorbis, AAC, ALAC, ...)
// ---------------------------------------------------------------------------

fn decode_with_symphonia(path: &Path) -> io::Result<(Vec<f32>, usize, u32)> {
    let file = std::fs::File::open(path)
        .map_err(|e| io::Error::other(format!("Failed to open '{}': {e}", path.display())))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let format_opts = FormatOptions::default();
    let metadata_opts = MetadataOptions::default();
    let decoder_opts = DecoderOptions::default();

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &format_opts, &metadata_opts)
        .map_err(|e| {
            io::Error::other(format!(
                "Symphonia failed to probe format for '{}': {e}",
                path.display()
            ))
        })?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .or_else(|| format.tracks().first())
        .ok_or_else(|| {
            io::Error::other(format!("No usable audio track in '{}'", path.display()))
        })?;

    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(1);
    let sample_rate = track.codec_params.sample_rate.unwrap_or(48_000);
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &decoder_opts)
        .map_err(|e| {
            io::Error::other(format!(
                "Symphonia failed to create decoder for '{}': {e}",
                path.display()
            ))
        })?;

    let mut sample_buf = None;
    let mut samples = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(e) => {
                return Err(io::Error::other(format!(
                    "Symphonia read error for '{}': {e}",
                    path.display()
                )));
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = decoder.decode(&packet).map_err(|e| {
            io::Error::other(format!(
                "Symphonia decode error for '{}': {e}",
                path.display()
            ))
        })?;

        if sample_buf.is_none() {
            let spec = *decoded.spec();
            sample_buf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
        }
        let buf = sample_buf.as_mut().unwrap();
        buf.copy_interleaved_ref(decoded);
        samples.extend_from_slice(buf.samples());
    }

    if samples.is_empty() {
        return Err(io::Error::other(format!(
            "No samples decoded from '{}'",
            path.display()
        )));
    }

    Ok((samples, channels, sample_rate))
}

// ---------------------------------------------------------------------------
// Encode entry point
// ---------------------------------------------------------------------------

/// Encode interleaved `f32` samples to a file using OxideAV.
///
/// `samples` must be interleaved (`ch0 ch1 ... chN ...`).
/// `channels` is clamped to at least 1. `sample_rate` must be non-zero.
/// Integer formats are quantised from the `[-1.0, 1.0]` float range; for
/// WAV and FLAC the requested bit depth is honoured, while MP3 always
/// uses 16-bit PCM internally.
pub fn encode_audio_to_file(
    path: &Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    format: AudioEncodeFormat,
    dither: AudioDither,
) -> io::Result<()> {
    let channels = channels.max(1);
    if sample_rate == 0 {
        return Err(io::Error::other("encode: sample_rate must be > 0"));
    }
    if channels > 8 {
        return Err(io::Error::other(format!(
            "encode: channel count {channels} exceeds the supported maximum of 8"
        )));
    }
    if !samples.len().is_multiple_of(channels) {
        return Err(io::Error::other(
            "encode: sample slice length is not a multiple of channels",
        ));
    }

    match format {
        AudioEncodeFormat::Wav(depth) => {
            encode_wav(path, samples, channels, sample_rate, depth, dither)
        }
        AudioEncodeFormat::Flac(bits) => {
            encode_flac_to_file(path, samples, channels, sample_rate, bits, dither)
        }
        AudioEncodeFormat::OggFlac(bits) => {
            encode_ogg_flac(path, samples, channels, sample_rate, bits, dither)
        }
        AudioEncodeFormat::Mp3 => encode_mp3(path, samples, channels, sample_rate, dither),
    }
}

/// Backwards-compatible WAV writer: 32-bit float PCM.
pub fn write_wav_f32(
    path: &Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
) -> io::Result<()> {
    encode_audio_to_file(
        path,
        samples,
        channels,
        sample_rate,
        AudioEncodeFormat::Wav(WavBitDepth::Float32),
        AudioDither::None,
    )
}

/// Backwards-compatible native FLAC writer.
pub fn write_flac(
    path: &Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    bits_per_sample: u16,
) -> io::Result<()> {
    encode_audio_to_file(
        path,
        samples,
        channels,
        sample_rate,
        AudioEncodeFormat::Flac(bits_per_sample),
        AudioDither::None,
    )
}

// ---------------------------------------------------------------------------
// WAV
// ---------------------------------------------------------------------------

fn encode_wav(
    path: &Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    depth: WavBitDepth,
    dither: AudioDither,
) -> io::Result<()> {
    let (codec_id, sample_format) = match depth {
        WavBitDepth::Int16 => ("pcm_s16le", SampleFormat::S16),
        WavBitDepth::Int24 => ("pcm_s24le", SampleFormat::S24),
        WavBitDepth::Int32 => ("pcm_s32le", SampleFormat::S32),
        WavBitDepth::Float32 => ("pcm_f32le", SampleFormat::F32),
    };
    let bytes = pack_interleaved_samples(samples, sample_format, dither)?;

    let mut ctx = RuntimeContext::new();
    oxideav_basic::register(&mut ctx);

    let stream = audio_stream_info(codec_id, channels, sample_rate, sample_format, None);
    let file = std::fs::File::create(path)?;
    let output: Box<dyn oxideav_core::WriteSeek> = Box::new(file);
    let mut mux = ctx
        .containers
        .open_muxer("wav", output, std::slice::from_ref(&stream))
        .map_err(oxideav_err_to_io)?;
    mux.write_header().map_err(oxideav_err_to_io)?;
    let packet = Packet::new(0, TimeBase::new(1, sample_rate as i64), bytes);
    mux.write_packet(&packet).map_err(oxideav_err_to_io)?;
    mux.write_trailer().map_err(oxideav_err_to_io)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// FLAC (native and Ogg)
// ---------------------------------------------------------------------------

fn encode_flac_to_file(
    path: &Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    bits_per_sample: u16,
    dither: AudioDither,
) -> io::Result<()> {
    let (packets, output_params) =
        encode_flac_packets(samples, channels, sample_rate, bits_per_sample, dither)?;

    let mut ctx = RuntimeContext::new();
    oxideav_flac::register(&mut ctx);

    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, sample_rate as i64),
        duration: None,
        start_time: Some(0),
        params: output_params,
    };
    let file = std::fs::File::create(path)?;
    let output: Box<dyn oxideav_core::WriteSeek> = Box::new(file);
    let mut mux = ctx
        .containers
        .open_muxer("flac", output, std::slice::from_ref(&stream))
        .map_err(oxideav_err_to_io)?;
    mux.write_header().map_err(oxideav_err_to_io)?;
    for pkt in &packets {
        mux.write_packet(pkt).map_err(oxideav_err_to_io)?;
    }
    mux.write_trailer().map_err(oxideav_err_to_io)?;
    Ok(())
}

/// Returns the encoded FLAC frame packets and the finalised output
/// parameters (including the STREAMINFO extradata).
fn encode_flac_packets(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    bits_per_sample: u16,
    dither: AudioDither,
) -> io::Result<(Vec<Packet>, CodecParameters)> {
    let sample_format = flac_sample_format(bits_per_sample)?;
    let bytes = pack_interleaved_samples(samples, sample_format, dither)?;

    let mut ctx = RuntimeContext::new();
    oxideav_flac::register(&mut ctx);

    let params = audio_codec_params("flac", channels, sample_rate, sample_format, None);
    let mut enc = ctx
        .codecs
        .first_encoder(&params)
        .map_err(oxideav_err_to_io)?;

    let frame = AudioFrame {
        samples: (samples.len() / channels) as u32,
        pts: Some(0),
        data: vec![bytes],
    };
    enc.send_frame(&Frame::Audio(frame))
        .map_err(oxideav_err_to_io)?;
    enc.flush().map_err(oxideav_err_to_io)?;

    let mut packets = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => packets.push(p),
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
            Err(e) => return Err(oxideav_err_to_io(e)),
        }
    }

    Ok((packets, enc.output_params().clone()))
}

fn encode_ogg_flac(
    path: &Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    bits_per_sample: u16,
    dither: AudioDither,
) -> io::Result<()> {
    let (packets, output_params) =
        encode_flac_packets(samples, channels, sample_rate, bits_per_sample, dither)?;

    // Build the FLAC-in-Ogg mapping header packet:
    // 0x7F "FLAC" major minor header_packets_be "fLaC"
    let mut mapping = Vec::with_capacity(13);
    mapping.push(0x7F);
    mapping.extend_from_slice(b"FLAC");
    mapping.push(0x01); // mapping major version
    mapping.push(0x00); // mapping minor version
    // One header packet follows the mapping header: the STREAMINFO block.
    mapping.extend_from_slice(&1u16.to_be_bytes());
    mapping.extend_from_slice(b"fLaC");

    let streaminfo = output_params.extradata;

    let mut writer = oxideav_ogg::framing::PageWriter::new(0).with_page_target(4096);
    writer.push_packet(&mapping, 0);
    writer.flush_page();
    writer.push_packet(&streaminfo, 0);
    writer.flush_page();

    for pkt in &packets {
        let granule = pkt
            .pts
            .map(|pts| pts + pkt.duration.unwrap_or(0))
            .unwrap_or(0);
        writer.push_packet(&pkt.data, granule);
    }

    std::fs::write(path, writer.finish())?;
    Ok(())
}

fn flac_sample_format(bits_per_sample: u16) -> io::Result<SampleFormat> {
    match bits_per_sample {
        8 => Ok(SampleFormat::U8),
        16 => Ok(SampleFormat::S16),
        24 => Ok(SampleFormat::S24),
        32 => Ok(SampleFormat::S32),
        _ => Err(io::Error::other(format!(
            "FLAC bit depth {bits_per_sample} not supported (use 8, 16, 24 or 32)"
        ))),
    }
}

// ---------------------------------------------------------------------------
// MP3
// ---------------------------------------------------------------------------

fn encode_mp3(
    path: &Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    dither: AudioDither,
) -> io::Result<()> {
    if channels > 2 {
        return Err(io::Error::other(
            "MP3 encode: only mono and stereo are supported",
        ));
    }
    let bitrate = mp3_default_bitrate(sample_rate, channels);
    let bytes = pack_interleaved_samples(samples, SampleFormat::S16, dither)?;

    let mut ctx = RuntimeContext::new();
    oxideav_mp3::register(&mut ctx);

    let params = audio_codec_params(
        "mp3",
        channels,
        sample_rate,
        SampleFormat::S16,
        Some(bitrate as u64),
    );
    let mut enc = ctx
        .codecs
        .first_encoder(&params)
        .map_err(oxideav_err_to_io)?;

    let frame = AudioFrame {
        samples: (samples.len() / channels) as u32,
        pts: Some(0),
        data: vec![bytes],
    };
    enc.send_frame(&Frame::Audio(frame))
        .map_err(oxideav_err_to_io)?;
    enc.flush().map_err(oxideav_err_to_io)?;

    let mut file = std::fs::File::create(path)?;
    loop {
        match enc.receive_packet() {
            Ok(pkt) => file.write_all(&pkt.data)?,
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
            Err(e) => return Err(oxideav_err_to_io(e)),
        }
    }
    Ok(())
}

fn mp3_default_bitrate(sample_rate: u32, channels: usize) -> u32 {
    // MPEG-1 (32/44.1/48 kHz) ladder
    if sample_rate >= 32_000 {
        if channels >= 2 { 192_000 } else { 128_000 }
    } else if sample_rate >= 16_000 {
        // MPEG-2 LSF (16/22.05/24 kHz) ladder
        if channels >= 2 { 96_000 } else { 64_000 }
    } else {
        // MPEG-2.5 (8/11.025/12 kHz) ladder
        if channels >= 2 { 48_000 } else { 32_000 }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn audio_codec_params(
    codec_id: &str,
    channels: usize,
    sample_rate: u32,
    sample_format: SampleFormat,
    bit_rate: Option<u64>,
) -> CodecParameters {
    let mut params = CodecParameters::audio(CodecId::new(codec_id));
    params.media_type = MediaType::Audio;
    params.channels = Some(channels as u16);
    params.sample_rate = Some(sample_rate);
    params.sample_format = Some(sample_format);
    if let Some(br) = bit_rate {
        params.bit_rate = Some(br);
    }
    params
}

fn audio_stream_info(
    codec_id: &str,
    channels: usize,
    sample_rate: u32,
    sample_format: SampleFormat,
    bit_rate: Option<u64>,
) -> StreamInfo {
    let params = audio_codec_params(codec_id, channels, sample_rate, sample_format, bit_rate);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, sample_rate as i64),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn pack_interleaved_samples(
    samples: &[f32],
    format: SampleFormat,
    dither: AudioDither,
) -> io::Result<Vec<u8>> {
    let bytes_per_sample = format.bytes_per_sample();
    let mut out = Vec::with_capacity(samples.len().saturating_mul(bytes_per_sample));
    let mut rng = DitherRng::new(0x1234_5678_9abc_defe);

    for &sample in samples {
        let s = sample.clamp(-1.0, 1.0);
        match format {
            SampleFormat::U8 => {
                let v = ((s + 1.0) * 127.5 + dither_offset(&mut rng, dither)).round() as u8;
                out.push(v);
            }
            SampleFormat::S16 => {
                let scale = i16::MAX as f32;
                let q = quantize_with_dither(s, scale, &mut rng, dither)
                    .round()
                    .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                out.extend_from_slice(&q.to_le_bytes());
            }
            SampleFormat::S24 => {
                let scale = 8_388_607.0;
                let q = quantize_with_dither(s, scale, &mut rng, dither)
                    .round()
                    .clamp(-8_388_608.0, 8_388_607.0) as i32;
                let b = q.to_le_bytes();
                out.extend_from_slice(&b[..3]);
            }
            SampleFormat::S32 => {
                let scale = i32::MAX as f32;
                let q = quantize_with_dither(s, scale, &mut rng, dither)
                    .round()
                    .clamp(i32::MIN as f32, i32::MAX as f32) as i32;
                out.extend_from_slice(&q.to_le_bytes());
            }
            SampleFormat::F32 => {
                out.extend_from_slice(&s.to_le_bytes());
            }
            _ => {
                return Err(io::Error::other(format!(
                    "unsupported sample format {format:?}"
                )));
            }
        }
    }
    Ok(out)
}

fn quantize_with_dither(sample: f32, scale: f32, rng: &mut DitherRng, dither: AudioDither) -> f32 {
    let d = dither_offset(rng, dither);
    (sample + d / scale).clamp(-1.0, 1.0) * scale
}

fn dither_offset(rng: &mut DitherRng, dither: AudioDither) -> f32 {
    match dither {
        AudioDither::None => 0.0,
        AudioDither::Rectangular => rng.uniform_half(),
        AudioDither::Triangular => rng.uniform_half() + rng.uniform_half(),
    }
}

fn oxideav_err_to_io(e: oxideav_core::Error) -> io::Error {
    io::Error::other(format!("OxideAV error: {e}"))
}

/// Tiny deterministic PRNG used for export dither.
struct DitherRng {
    state: u64,
}

impl DitherRng {
    fn new(seed: u64) -> Self {
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
    fn uniform_half(&mut self) -> f32 {
        let u = self.next_u64() >> 32;
        (u as f32 / 4_294_967_296.0) - 0.5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_stereo_wav_returns_interleaved_samples() {
        let path =
            std::env::temp_dir().join(format!("maolan_stereo_decode_{}.wav", std::process::id()));
        write_test_wav_f32(
            &path,
            &[
                0.10, 0.60, //
                0.20, 0.70, //
                0.30, 0.80, //
                0.40, 0.90,
            ],
            2,
            48_000,
        )
        .expect("write test wav");

        let (samples, channels, sample_rate) =
            decode_audio_to_f32_interleaved_sync(&path).expect("decode test wav");
        let _ = std::fs::remove_file(&path);

        assert_eq!(channels, 2);
        assert_eq!(sample_rate, 48_000);
        assert_eq!(samples.len(), 8);
        for (actual, expected) in samples
            .iter()
            .zip([0.10, 0.60, 0.20, 0.70, 0.30, 0.80, 0.40, 0.90])
        {
            assert!((actual - expected).abs() < 1.0e-6);
        }
    }

    fn write_test_wav_f32(
        path: &Path,
        samples: &[f32],
        channels: usize,
        sample_rate: u32,
    ) -> io::Result<()> {
        let bytes_per_sample = 4usize;
        let block_align = (channels * bytes_per_sample) as u16;
        let byte_rate = sample_rate * u32::from(block_align);
        let data_size = samples.len() * bytes_per_sample;
        let riff_size = 36 + data_size as u32;

        let mut file = std::fs::File::create(path)?;
        file.write_all(b"RIFF")?;
        file.write_all(&riff_size.to_le_bytes())?;
        file.write_all(b"WAVE")?;
        file.write_all(b"fmt ")?;
        file.write_all(&16u32.to_le_bytes())?;
        file.write_all(&3u16.to_le_bytes())?;
        file.write_all(&(channels as u16).to_le_bytes())?;
        file.write_all(&sample_rate.to_le_bytes())?;
        file.write_all(&byte_rate.to_le_bytes())?;
        file.write_all(&block_align.to_le_bytes())?;
        file.write_all(&32u16.to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&(data_size as u32).to_le_bytes())?;
        for sample in samples {
            file.write_all(&sample.to_le_bytes())?;
        }
        Ok(())
    }
}
