use std::io::{self, Write};
use std::path::{Path, PathBuf};
use symphonia::core::codecs::CodecParameters as SymphoniaCodecParameters;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

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
/// The format is auto-detected by the OxideAV demuxers/decoders:
/// `.wav`/PCM, `.flac`, `.mp3`, and Ogg-encapsulated FLAC (`.ogg`).
/// Files OxideAV cannot probe or decode fall back to Symphonia
/// (`.ogg`/Vorbis, `.m4a`/`.aac`/`.alac`, and friends).
/// Returns `(samples, channels, sample_rate)`.
/// All decode paths emit samples in the range `[-1.0, 1.0]`.
pub fn decode_audio_to_f32_interleaved_sync(path: &Path) -> io::Result<(Vec<f32>, usize, u32)> {
    match decode_with_oxideav(path) {
        Ok(decoded) => Ok(decoded),
        Err(oxideav_err) => decode_with_symphonia(path).map_err(|symphonia_err| {
            io::Error::other(format!(
                "Failed to decode '{}' with OxideAV ({oxideav_err}) \
                 or Symphonia ({symphonia_err})",
                path.display()
            ))
        }),
    }
}

/// Decode a WAV file preferentially, falling back to the general decoder.
///
/// This used to short-circuit `.wav` inputs to a dedicated path. The
/// unified OxideAV decoder handles WAV natively, so this now simply
/// delegates to it.
pub fn decode_audio_to_f32_interleaved_preferring_wav(
    path: &Path,
) -> io::Result<(Vec<f32>, usize, u32)> {
    decode_audio_to_f32_interleaved_sync(path)
}

// ---------------------------------------------------------------------------
// Metadata probe + incremental streaming decode (Symphonia)
// ---------------------------------------------------------------------------

/// Fast metadata probe result for an audio file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFileInfo {
    pub sample_rate: u32,
    pub channels: usize,
    /// Approximate total frame count, when the container reports a duration.
    pub frames: Option<u64>,
}

/// Probe sample rate, channel count and approximate frame count without
/// decoding. Works for every format Symphonia can probe (wav, flac, mp3,
/// ogg/vorbis, aac/alac in m4a/isomp4, ...).
pub fn probe_audio_file(path: &Path) -> io::Result<AudioFileInfo> {
    let format = probe_format(path)?;
    let track = format
        .default_track(TrackType::Audio)
        .or_else(|| format.tracks().first())
        .ok_or_else(|| {
            io::Error::other(format!("No usable audio track in '{}'", path.display()))
        })?;
    let params: &AudioCodecParameters = track
        .codec_params
        .as_ref()
        .and_then(SymphoniaCodecParameters::audio)
        .ok_or_else(|| {
            io::Error::other(format!("No usable audio track in '{}'", path.display()))
        })?;
    let sample_rate = params.sample_rate.unwrap_or(0);
    if sample_rate == 0 {
        return Err(io::Error::other(format!(
            "No sample rate in '{}'",
            path.display()
        )));
    }
    let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(0);
    if channels == 0 {
        return Err(io::Error::other(format!(
            "No channel count in '{}'",
            path.display()
        )));
    }
    let frames = track
        .time_base
        .and_then(|tb| {
            track
                .duration
                .and_then(|duration| tb.calc_duration(duration))
        })
        .map(|time| (time.as_secs_f64() * f64::from(sample_rate)).round() as u64);
    Ok(AudioFileInfo {
        sample_rate,
        channels,
        frames,
    })
}

/// Metadata (tags) read from an audio file, all fields optional.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioMetadata {
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub track_number: Option<String>,
    pub date: Option<String>,
    pub genre: Option<String>,
}

/// Read metadata tags from an audio file without decoding audio.
///
/// Container tags (e.g. Vorbis comments, ID3v2) are preferred; track-level
/// tags (e.g. MP4 metadata) are used as a fallback.
pub fn read_audio_metadata(path: &Path) -> io::Result<AudioMetadata> {
    use symphonia::core::meta::{RawValue, StandardTag};

    fn raw_string(value: &RawValue) -> Option<String> {
        match value {
            RawValue::String(s) => {
                let s = s.trim();
                (!s.is_empty()).then(|| s.to_string())
            }
            RawValue::UnsignedInt(v) => Some(v.to_string()),
            _ => None,
        }
    }

    let mut format = probe_format(path)?;
    let metadata = format.metadata();
    let Some(revision) = metadata.current() else {
        return Err(io::Error::other(format!(
            "No metadata in '{}'",
            path.display()
        )));
    };
    let mut meta = AudioMetadata::default();
    // Media-level tags first, then per-track tags as a fallback.
    let mut tags: Vec<&symphonia::core::meta::Tag> = revision.media.tags.iter().collect();
    for track_meta in &revision.per_track {
        tags.extend(track_meta.metadata.tags.iter());
    }
    for tag in tags {
        let (slot, value): (&mut Option<String>, Option<String>) = match tag.std.as_ref() {
            Some(StandardTag::Artist(s)) | Some(StandardTag::Performer(s)) => {
                (&mut meta.artist, Some(s.trim().to_string()))
            }
            Some(StandardTag::TrackTitle(s)) => (&mut meta.title, Some(s.trim().to_string())),
            Some(StandardTag::Album(s)) => (&mut meta.album, Some(s.trim().to_string())),
            Some(StandardTag::TrackNumber(n)) => (&mut meta.track_number, Some(n.to_string())),
            Some(StandardTag::RecordingDate(s))
            | Some(StandardTag::ReleaseDate(s))
            | Some(StandardTag::OriginalReleaseDate(s)) => {
                (&mut meta.date, Some(s.trim().to_string()))
            }
            Some(StandardTag::Genre(s)) => (&mut meta.genre, Some(s.trim().to_string())),
            Some(_) => continue,
            // Fall back to the raw key for tags Symphonia doesn't map.
            None => {
                let key = tag.raw.key.to_ascii_lowercase();
                let slot = match key.as_str() {
                    "artist" | "performer" => &mut meta.artist,
                    "title" | "tracktitle" => &mut meta.title,
                    "album" => &mut meta.album,
                    "tracknumber" | "track" => &mut meta.track_number,
                    "date" | "year" => &mut meta.date,
                    "genre" => &mut meta.genre,
                    _ => continue,
                };
                (slot, raw_string(&tag.raw.value))
            }
        };
        if slot.is_none()
            && let Some(value) = value
            && !value.is_empty()
        {
            *slot = Some(value);
        }
    }
    Ok(meta)
}

fn probe_format(path: &Path) -> io::Result<Box<dyn FormatReader>> {
    let file = std::fs::File::open(path)
        .map_err(|e| io::Error::other(format!("Failed to open '{}': {e}", path.display())))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| {
            io::Error::other(format!(
                "Symphonia failed to probe format for '{}': {e}",
                path.display()
            ))
        })
}

/// Parts of a probed + opened Symphonia audio stream: the format reader, the
/// audio decoder, the audio track id, channel count and sample rate.
type SymphoniaStreamParts = (
    Box<dyn FormatReader>,
    Box<dyn AudioDecoder>,
    u32,
    usize,
    u32,
);

/// Incremental audio decoder: probes once, then yields decoded interleaved
/// `f32` chunks on demand. Used by the streaming clip producer thread so the
/// audio thread never decodes whole files.
pub struct StreamingDecoder {
    path: PathBuf,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    channels: usize,
    sample_rate: u32,
    /// Pending decoded samples carried between chunk boundaries.
    pending: Vec<f32>,
}

impl StreamingDecoder {
    pub fn new(path: &Path) -> io::Result<Self> {
        let (format, decoder, track_id, channels, sample_rate) = Self::open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            format,
            decoder,
            track_id,
            channels,
            sample_rate,
            pending: Vec::new(),
        })
    }

    fn open(path: &Path) -> io::Result<SymphoniaStreamParts> {
        let format = probe_format(path)?;
        let track = format
            .default_track(TrackType::Audio)
            .or_else(|| format.tracks().first())
            .ok_or_else(|| {
                io::Error::other(format!("No usable audio track in '{}'", path.display()))
            })?;
        let codec_params: &AudioCodecParameters = track
            .codec_params
            .as_ref()
            .and_then(SymphoniaCodecParameters::audio)
            .ok_or_else(|| {
                io::Error::other(format!("No usable audio track in '{}'", path.display()))
            })?;
        let channels = codec_params
            .channels
            .as_ref()
            .map(|c| c.count())
            .unwrap_or(1);
        let sample_rate = codec_params.sample_rate.unwrap_or(48_000);
        let track_id = track.id;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(codec_params, &AudioDecoderOptions::default())
            .map_err(|e| {
                io::Error::other(format!(
                    "Symphonia failed to create decoder for '{}': {e}",
                    path.display()
                ))
            })?;
        Ok((format, decoder, track_id, channels, sample_rate))
    }

    pub fn channels(&self) -> usize {
        self.channels.max(1)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Restart decoding from the beginning of the file. Used for seeks: the
    /// caller decodes and discards frames until the target position.
    pub fn reset(&mut self) -> io::Result<()> {
        let (format, decoder, track_id, channels, sample_rate) = Self::open(&self.path)?;
        self.format = format;
        self.decoder = decoder;
        self.track_id = track_id;
        self.channels = channels;
        self.sample_rate = sample_rate;
        self.pending.clear();
        Ok(())
    }

    /// Decode and return up to `max_frames` interleaved frames.
    /// Returns `Ok(None)` at end of stream.
    pub fn next_chunk(&mut self, max_frames: usize) -> io::Result<Option<Vec<f32>>> {
        let channels = self.channels();
        let target = max_frames.saturating_mul(channels);
        let mut out = std::mem::take(&mut self.pending);
        if out.len() > target {
            self.pending = out.split_off(target);
            return Ok(Some(out));
        }
        while out.len() < target {
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => break,
                Err(SymphoniaError::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                Err(e) => {
                    return Err(io::Error::other(format!(
                        "Symphonia read error for '{}': {e}",
                        self.path.display()
                    )));
                }
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let mut packet_samples = Vec::new();
                    decoded.copy_to_vec_interleaved(&mut packet_samples);
                    out.extend_from_slice(&packet_samples);
                }
                // Skip individual broken packets; fail only on IO-level errors.
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(SymphoniaError::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                Err(e) => {
                    return Err(io::Error::other(format!(
                        "Symphonia decode error for '{}': {e}",
                        self.path.display()
                    )));
                }
            }
        }
        if out.len() > target {
            self.pending = out.split_off(target);
        }
        if out.is_empty() {
            Ok(None)
        } else {
            Ok(Some(out))
        }
    }
}

// ---------------------------------------------------------------------------
// OxideAV decode (WAV/PCM, FLAC, MP3, Ogg-FLAC)
// ---------------------------------------------------------------------------

fn decode_with_oxideav(path: &Path) -> io::Result<(Vec<f32>, usize, u32)> {
    let mut ctx = RuntimeContext::new();
    oxideav_basic::register(&mut ctx);
    oxideav_flac::register(&mut ctx);
    oxideav_mp3::register(&mut ctx);
    oxideav_ogg::register(&mut ctx);

    let mut input: Box<dyn oxideav_core::ReadSeek> = Box::new(
        std::fs::File::open(path)
            .map_err(|e| io::Error::other(format!("Failed to open '{}': {e}", path.display())))?,
    );
    let ext_hint = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let container = ctx
        .containers
        .probe_input(&mut *input, ext_hint.as_deref())
        .map_err(oxideav_err_to_io)?;
    let mut demuxer = ctx
        .containers
        .open_demuxer(&container, input, &ctx.codecs)
        .map_err(oxideav_err_to_io)?;

    let (stream_index, params) = demuxer
        .streams()
        .iter()
        .enumerate()
        .map(|(i, s)| (i as u32, &s.params))
        .find(|(_, p)| p.media_type == MediaType::Audio)
        .ok_or_else(|| {
            io::Error::other(format!("No usable audio track in '{}'", path.display()))
        })?;
    let params = params.clone();

    let channels = params.channels.unwrap_or(1) as usize;
    let sample_rate = params.sample_rate.unwrap_or(48_000);
    // The MP3 demuxer leaves `sample_format` unset; its decoder always
    // emits packed S16. Every other OxideAV decoder declares its format.
    let format = params.sample_format.unwrap_or(SampleFormat::S16);
    let mut decoder = ctx
        .codecs
        .first_decoder(&params)
        .map_err(oxideav_err_to_io)?;
    demuxer.set_active_streams(std::slice::from_ref(&stream_index));

    let mut samples = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(packet) => {
                if packet.stream_index != stream_index {
                    continue;
                }
                decoder.send_packet(&packet).map_err(oxideav_err_to_io)?;
                drain_decoder_frames(&mut *decoder, format, channels, &mut samples)?;
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => return Err(oxideav_err_to_io(e)),
        }
    }
    decoder.flush().map_err(oxideav_err_to_io)?;
    drain_decoder_frames(&mut *decoder, format, channels, &mut samples)?;

    if samples.is_empty() {
        return Err(io::Error::other(format!(
            "No samples decoded from '{}'",
            path.display()
        )));
    }

    Ok((samples, channels, sample_rate))
}

/// Pull every decoded frame currently pending on `decoder` and append it
/// to `out` as interleaved `f32`.
fn drain_decoder_frames(
    decoder: &mut dyn oxideav_core::Decoder,
    format: SampleFormat,
    channels: usize,
    out: &mut Vec<f32>,
) -> io::Result<()> {
    loop {
        match decoder.receive_frame() {
            Ok(Frame::Audio(frame)) => {
                out.extend(unpack_audio_frame(&frame, format, channels)?);
            }
            Ok(_) => {}
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => return Ok(()),
            Err(e) => return Err(oxideav_err_to_io(e)),
        }
    }
}

/// Convert one `AudioFrame` to interleaved `f32` samples in `[-1.0, 1.0]`.
/// Packed formats carry a single interleaved plane; planar formats carry
/// one plane per channel.
fn unpack_audio_frame(
    frame: &AudioFrame,
    format: SampleFormat,
    channels: usize,
) -> io::Result<Vec<f32>> {
    let bytes_per_sample = format.bytes_per_sample();
    if channels == 0 || frame.samples == 0 {
        return Ok(Vec::new());
    }
    let total = frame
        .samples
        .checked_mul(channels as u32)
        .ok_or_else(|| io::Error::other("OxideAV frame sample count overflow"))?
        as usize;
    let mut out = Vec::with_capacity(total);

    // Layout is inferred from the actual planes: one plane holding
    // `total` samples means packed/interleaved; one plane per channel
    // (as the MP3 decoder emits, despite its packed S16 tag) means
    // planar.
    let planar = frame.data.len() >= channels && frame.data.len() > 1;
    if planar {
        if frame.data.len() < channels {
            return Err(io::Error::other(
                "OxideAV planar frame has fewer planes than channels",
            ));
        }
        for i in 0..frame.samples as usize {
            for plane in frame.data.iter().take(channels) {
                let start = i * bytes_per_sample;
                let end = start + bytes_per_sample;
                if end > plane.len() {
                    return Err(io::Error::other("OxideAV frame plane is truncated"));
                }
                out.push(sample_bytes_to_f32(&plane[start..end], format));
            }
        }
    } else {
        let plane = frame
            .data
            .first()
            .ok_or_else(|| io::Error::other("OxideAV packed frame has no plane"))?;
        if plane.len() < total * bytes_per_sample {
            return Err(io::Error::other(format!(
                "OxideAV frame plane is truncated: {} bytes for {total} samples \
                 at {bytes_per_sample} B/sample ({} declared per channel)",
                plane.len(),
                frame.samples
            )));
        }
        for chunk in plane[..total * bytes_per_sample].chunks_exact(bytes_per_sample) {
            out.push(sample_bytes_to_f32(chunk, format));
        }
    }

    Ok(out)
}

fn sample_bytes_to_f32(bytes: &[u8], format: SampleFormat) -> f32 {
    match format {
        SampleFormat::U8 => (bytes[0] as f32 - 128.0) / 128.0,
        SampleFormat::S16 => {
            f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) / f32::from(i16::MAX)
        }
        SampleFormat::S24 => {
            let v = i32::from_le_bytes([
                bytes[0],
                bytes[1],
                bytes[2],
                if bytes[2] & 0x80 != 0 { 0xFF } else { 0 },
            ]);
            v as f32 / 8_388_607.0
        }
        SampleFormat::S32 => {
            i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32 / i32::MAX as f32
        }
        SampleFormat::F32 => f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        _ => 0.0,
    }
}

// ---------------------------------------------------------------------------
// Symphonia decode fallback (Ogg Vorbis, AAC, ALAC, MP4/M4A, ...)
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
    let decoder_opts = AudioDecoderOptions::default();

    let mut format: Box<dyn FormatReader> = symphonia::default::get_probe()
        .probe(&hint, mss, format_opts, metadata_opts)
        .map_err(|e| {
            io::Error::other(format!(
                "Symphonia failed to probe format for '{}': {e}",
                path.display()
            ))
        })?;

    let track = format
        .default_track(TrackType::Audio)
        .or_else(|| format.tracks().first())
        .ok_or_else(|| {
            io::Error::other(format!("No usable audio track in '{}'", path.display()))
        })?;

    let codec_params: &AudioCodecParameters = track
        .codec_params
        .as_ref()
        .and_then(SymphoniaCodecParameters::audio)
        .ok_or_else(|| {
            io::Error::other(format!("No usable audio track in '{}'", path.display()))
        })?;

    let channels = codec_params
        .channels
        .as_ref()
        .map(|c| c.count())
        .unwrap_or(1);
    let sample_rate = codec_params.sample_rate.unwrap_or(48_000);
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(codec_params, &decoder_opts)
        .map_err(|e| {
            io::Error::other(format!(
                "Symphonia failed to create decoder for '{}': {e}",
                path.display()
            ))
        })?;

    let mut samples = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
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

        if packet.track_id != track_id {
            continue;
        }

        let decoded = decoder.decode(&packet).map_err(|e| {
            io::Error::other(format!(
                "Symphonia decode error for '{}': {e}",
                path.display()
            ))
        })?;

        let mut packet_samples = Vec::new();
        decoded.copy_to_vec_interleaved(&mut packet_samples);
        samples.extend_from_slice(&packet_samples);
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

    #[test]
    fn oxideav_fallback_decodes_flac_and_mp3() {
        let sample_rate = 44_100u32;
        let channels = 2usize;
        let frames = sample_rate as usize / 10; // 100 ms
        let source: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let s = 0.5
                    * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin();
                [s, s]
            })
            .collect();

        for (ext, format) in [
            ("flac", AudioEncodeFormat::Flac(16)),
            ("mp3", AudioEncodeFormat::Mp3),
        ] {
            let path = std::env::temp_dir().join(format!(
                "maolan_oxideav_fallback_{}_{}.{}",
                ext,
                std::process::id(),
                ext
            ));
            encode_audio_to_file(
                &path,
                &source,
                channels,
                sample_rate,
                format,
                AudioDither::None,
            )
            .expect("encode test file");

            // Bypass Symphonia to exercise the OxideAV fallback directly.
            let (decoded, out_channels, out_rate) =
                decode_with_oxideav(&path).expect("oxideav decode");
            let _ = std::fs::remove_file(&path);

            assert_eq!(out_channels, channels);
            assert_eq!(out_rate, sample_rate);
            assert!(!decoded.is_empty());
            assert!(decoded.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
            // MP3 adds codec delay/padding; just require audible signal.
            let peak = decoded.iter().fold(0.0f32, |a, &b| a.max(b.abs()));
            assert!(peak > 0.2, "{ext}: decoded signal too quiet (peak {peak})");
        }
    }

    #[test]
    fn probe_audio_file_reports_wav_metadata() {
        let sample_rate = 48_000u32;
        let channels = 2usize;
        let frames = 1_000usize;
        let source: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let s = 0.5
                    * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin();
                [s, s]
            })
            .collect();
        let path =
            std::env::temp_dir().join(format!("maolan_probe_wav_{}.wav", std::process::id()));
        write_wav_f32(&path, &source, channels, sample_rate).expect("write test wav");

        let info = probe_audio_file(&path).expect("probe wav");
        let _ = std::fs::remove_file(&path);

        assert_eq!(info.sample_rate, sample_rate);
        assert_eq!(info.channels, channels);
        let probed_frames = info.frames.expect("wav probe should report frames");
        assert!(
            (probed_frames as i64 - frames as i64).abs() <= 1,
            "probed frames {probed_frames} != {frames}"
        );
    }

    #[test]
    fn probe_audio_file_reports_flac_metadata() {
        let sample_rate = 44_100u32;
        let channels = 1usize;
        let frames = 44_100usize;
        let source: Vec<f32> = (0..frames)
            .map(|i| {
                0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin()
            })
            .collect();
        let path =
            std::env::temp_dir().join(format!("maolan_probe_flac_{}.flac", std::process::id()));
        encode_audio_to_file(
            &path,
            &source,
            channels,
            sample_rate,
            AudioEncodeFormat::Flac(16),
            AudioDither::None,
        )
        .expect("encode test flac");

        let info = probe_audio_file(&path).expect("probe flac");
        let _ = std::fs::remove_file(&path);

        assert_eq!(info.sample_rate, sample_rate);
        assert_eq!(info.channels, channels);
        let probed_frames = info.frames.expect("flac probe should report frames");
        assert!(
            (probed_frames as i64 - frames as i64).abs() <= 1,
            "probed frames {probed_frames} != {frames}"
        );
    }

    #[test]
    fn streaming_decoder_yields_incremental_flac_chunks() {
        let sample_rate = 44_100u32;
        let channels = 2usize;
        let frames = 20_000usize;
        let source: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let s = 0.4
                    * (2.0 * std::f32::consts::PI * 330.0 * i as f32 / sample_rate as f32).sin();
                [s, -s]
            })
            .collect();
        let path = std::env::temp_dir().join(format!(
            "maolan_streaming_decoder_{}.flac",
            std::process::id()
        ));
        encode_audio_to_file(
            &path,
            &source,
            channels,
            sample_rate,
            AudioEncodeFormat::Flac(16),
            AudioDither::None,
        )
        .expect("encode test flac");

        let mut decoder = StreamingDecoder::new(&path).expect("open streaming decoder");
        assert_eq!(decoder.channels(), channels);
        assert_eq!(decoder.sample_rate(), sample_rate);

        let mut total_frames = 0usize;
        let mut chunks = 0usize;
        loop {
            match decoder.next_chunk(4096) {
                Ok(Some(chunk)) => {
                    chunks += 1;
                    assert!(chunk.len() % channels == 0);
                    total_frames += chunk.len() / channels;
                    assert!(chunk.iter().all(|s| s.is_finite()));
                }
                Ok(None) => break,
                Err(e) => panic!("streaming decode failed: {e}"),
            }
        }
        let _ = std::fs::remove_file(&path);

        assert!(chunks > 1, "expected multiple chunks, got {chunks}");
        assert!(
            (total_frames as i64 - frames as i64).abs() <= 8_192,
            "decoded frames {total_frames} far from {frames}"
        );
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
