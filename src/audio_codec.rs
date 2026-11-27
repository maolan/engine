use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, Decoder, Demuxer, Frame, MediaType, Packet,
    RuntimeContext, SampleFormat, StreamInfo, TimeBase,
};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Export format selector and WAV bit-depth / dither types live in
/// `wav_stream` (always compiled) so offline-bounce message types can use
/// them without the `audio` feature; re-exported here for compatibility.
pub use crate::wav_stream::{AudioDither, AudioEncodeFormat, WavBitDepth};
use crate::wav_stream::{DitherRng, dither_offset, quantize_with_dither};

fn oxideav_err_to_io(e: oxideav_core::Error) -> io::Error {
    io::Error::other(format!("OxideAV error: {e}"))
}

/// Decode an audio file to interleaved `f32` samples.
///
/// The format is auto-detected by OxideAV. Supported inputs include WAV/PCM,
/// FLAC, MP3, Ogg FLAC, Ogg Vorbis, and AAC-LC in ADTS or MP4/M4A containers.
/// Returns `(samples, channels, sample_rate)` with samples in `[-1.0, 1.0]`.
pub fn decode_audio_to_f32_interleaved_sync(path: &Path) -> io::Result<(Vec<f32>, usize, u32)> {
    decode_with_oxideav(path)
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
// Metadata probe + incremental streaming decode
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
/// decoding audio.
pub fn probe_audio_file(path: &Path) -> io::Result<AudioFileInfo> {
    let demuxer = open_oxideav_demuxer(path)?;
    let (_, stream) = first_audio_stream(&*demuxer, path)?;
    let sample_rate = stream
        .params
        .sample_rate
        .ok_or_else(|| io::Error::other(format!("No sample rate in '{}'", path.display())))?;
    let channels = stream.params.channels.unwrap_or(0) as usize;
    if channels == 0 {
        return Err(io::Error::other(format!(
            "No channel count in '{}'",
            path.display()
        )));
    }
    let frames = stream.duration.and_then(|duration| {
        let seconds =
            duration as f64 * stream.time_base.num() as f64 / stream.time_base.den() as f64;
        (seconds.is_finite() && seconds >= 0.0)
            .then(|| (seconds * f64::from(sample_rate)).round() as u64)
    });
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

/// Read container metadata tags without decoding audio.
pub fn read_audio_metadata(path: &Path) -> io::Result<AudioMetadata> {
    let demuxer = open_oxideav_demuxer(path)?;
    let mut meta = AudioMetadata::default();
    for (key, value) in demuxer.metadata() {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let slot = match key.to_ascii_lowercase().as_str() {
            "artist" | "performer" => &mut meta.artist,
            "title" | "tracktitle" => &mut meta.title,
            "album" => &mut meta.album,
            "tracknumber" | "track" => &mut meta.track_number,
            "date" | "year" => &mut meta.date,
            "genre" => &mut meta.genre,
            _ => continue,
        };
        if slot.is_none() {
            *slot = Some(value.to_string());
        }
    }
    Ok(meta)
}

type OxideavStreamParts = (Box<dyn Demuxer>, Box<dyn Decoder>, u32, CodecParameters);

/// Incremental audio decoder: probes once, then yields decoded interleaved
/// `f32` chunks on demand. Used by the streaming clip producer thread so the
/// audio thread never decodes whole files.
pub struct StreamingDecoder {
    path: PathBuf,
    demuxer: Box<dyn Demuxer>,
    decoder: Box<dyn Decoder>,
    stream_index: u32,
    params: CodecParameters,
    channels: usize,
    sample_rate: u32,
    pending: Vec<f32>,
    flushed: bool,
}

impl StreamingDecoder {
    pub fn new(path: &Path) -> io::Result<Self> {
        let (demuxer, decoder, stream_index, params) = open_oxideav_decoder(path)?;
        let channels = params.channels.unwrap_or(1).max(1) as usize;
        let sample_rate = params.sample_rate.unwrap_or(48_000);
        Ok(Self {
            path: path.to_path_buf(),
            demuxer,
            decoder,
            stream_index,
            params,
            channels,
            sample_rate,
            pending: Vec::new(),
            flushed: false,
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Restart decoding from the beginning of the file. Used for seeks: the
    /// caller decodes and discards frames until the target position.
    pub fn reset(&mut self) -> io::Result<()> {
        let (demuxer, decoder, stream_index, params) = open_oxideav_decoder(&self.path)?;
        self.demuxer = demuxer;
        self.decoder = decoder;
        self.stream_index = stream_index;
        self.params = params;
        self.pending.clear();
        self.flushed = false;
        Ok(())
    }

    /// Decode and return up to `max_frames` interleaved frames.
    /// Returns `Ok(None)` at end of stream.
    pub fn next_chunk(&mut self, max_frames: usize) -> io::Result<Option<Vec<f32>>> {
        let target = max_frames.saturating_mul(self.channels);
        let mut out = std::mem::take(&mut self.pending);
        while out.len() < target && !self.flushed {
            match self.demuxer.next_packet() {
                Ok(packet) => {
                    if packet.stream_index != self.stream_index {
                        continue;
                    }
                    self.decoder
                        .send_packet(&packet)
                        .map_err(oxideav_err_to_io)?;
                    drain_decoder_frames(&mut *self.decoder, &self.params, &mut out)?;
                }
                Err(oxideav_core::Error::Eof) => {
                    self.decoder.flush().map_err(oxideav_err_to_io)?;
                    drain_decoder_frames(&mut *self.decoder, &self.params, &mut out)?;
                    self.flushed = true;
                }
                Err(e) => return Err(oxideav_err_to_io(e)),
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
// OxideAV decode
// ---------------------------------------------------------------------------

struct AdtsDemuxer {
    input: Box<dyn oxideav_core::ReadSeek>,
    streams: Vec<StreamInfo>,
    next: Option<Packet>,
    next_pts: i64,
}

impl AdtsDemuxer {
    fn open(mut input: Box<dyn oxideav_core::ReadSeek>) -> io::Result<Self> {
        let (packet, params) = Self::read_packet(&mut input, 0)?;
        let stream = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, params.sample_rate.unwrap_or(44_100) as i64),
            duration: None,
            start_time: Some(0),
            params,
        };
        Ok(Self {
            input,
            streams: vec![stream],
            next: Some(packet),
            next_pts: 1_024,
        })
    }

    fn read_packet(
        input: &mut dyn oxideav_core::ReadSeek,
        pts: i64,
    ) -> io::Result<(Packet, CodecParameters)> {
        let mut header = [0_u8; 7];
        input.read_exact(&mut header)?;
        let (adts, _) = oxideav_aac::adts::AdtsHeader::parse(&header)
            .map_err(|e| io::Error::other(format!("Invalid ADTS header: {e}")))?;
        let mut data = vec![0_u8; adts.aac_frame_length as usize];
        data[..7].copy_from_slice(&header);
        input.read_exact(&mut data[7..])?;
        let mut params = CodecParameters::audio(CodecId::new("aac"));
        params.media_type = MediaType::Audio;
        params.channels = Some(u16::from(adts.channel_configuration.max(1)));
        params.sample_rate = Some(adts.sample_rate());
        params.sample_format = Some(SampleFormat::S16);
        let mut packet = Packet::new(0, TimeBase::new(1, adts.sample_rate() as i64), data);
        packet.pts = Some(pts);
        packet.dts = Some(pts);
        packet.duration = Some(1_024 * i64::from(adts.number_of_raw_data_blocks_in_frame));
        packet.flags.keyframe = true;
        Ok((packet, params))
    }
}

impl Demuxer for AdtsDemuxer {
    fn format_name(&self) -> &str {
        "adts"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> oxideav_core::Result<Packet> {
        if let Some(packet) = self.next.take() {
            return Ok(packet);
        }
        match Self::read_packet(&mut *self.input, self.next_pts) {
            Ok((packet, _)) => {
                self.next_pts += packet.duration.unwrap_or(1_024);
                Ok(packet)
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(oxideav_core::Error::Eof),
            Err(e) => Err(oxideav_core::Error::other(e.to_string())),
        }
    }
}

struct RawAacDecoder {
    inner: Box<dyn Decoder>,
    config: oxideav_aac::asc::AudioSpecificConfig,
}

impl RawAacDecoder {
    fn new(inner: Box<dyn Decoder>, config: oxideav_aac::asc::AudioSpecificConfig) -> Self {
        Self { inner, config }
    }
}

impl Decoder for RawAacDecoder {
    fn codec_id(&self) -> &CodecId {
        self.inner.codec_id()
    }

    fn send_packet(&mut self, packet: &Packet) -> oxideav_core::Result<()> {
        if !(1..=4).contains(&self.config.aot) {
            return Err(oxideav_core::Error::unsupported(format!(
                "AAC object type {} cannot be carried in ADTS",
                self.config.aot
            )));
        }
        let frame_length = packet.data.len().saturating_add(7);
        let header = oxideav_aac::adts::AdtsHeader {
            mpeg_version_mpeg2: false,
            protection_absent: true,
            profile: self.config.aot - 1,
            sampling_frequency_index: self.config.sampling_frequency_index,
            channel_configuration: self.config.channel_configuration,
            aac_frame_length: u16::try_from(frame_length)
                .map_err(|_| oxideav_core::Error::invalid("AAC access unit is too large"))?,
            adts_buffer_fullness: 0x7ff,
            number_of_raw_data_blocks_in_frame: 1,
        };
        let mut framed = Vec::with_capacity(frame_length);
        framed.extend_from_slice(
            &header
                .write()
                .map_err(|e| oxideav_core::Error::other(e.to_string()))?,
        );
        framed.extend_from_slice(&packet.data);
        let mut packet = packet.clone();
        packet.data = framed;
        self.inner.send_packet(&packet)
    }

    fn receive_frame(&mut self) -> oxideav_core::Result<Frame> {
        self.inner.receive_frame()
    }

    fn flush(&mut self) -> oxideav_core::Result<()> {
        self.inner.flush()
    }

    fn reset(&mut self) -> oxideav_core::Result<()> {
        self.inner.reset()
    }

    fn output_params(&self) -> Option<CodecParameters> {
        self.inner.output_params()
    }

    fn output_params_generation(&self) -> u64 {
        self.inner.output_params_generation()
    }
}

fn oxideav_runtime() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_basic::register(&mut ctx);
    oxideav_flac::register(&mut ctx);
    oxideav_mp3::register(&mut ctx);
    oxideav_ogg::register(&mut ctx);
    oxideav_vorbis::register(&mut ctx);
    oxideav_aac::register(&mut ctx);
    oxideav_mp4::register(&mut ctx);
    ctx
}

fn open_oxideav_demuxer(path: &Path) -> io::Result<Box<dyn Demuxer>> {
    let ctx = oxideav_runtime();
    let mut input: Box<dyn oxideav_core::ReadSeek> = Box::new(
        std::fs::File::open(path)
            .map_err(|e| io::Error::other(format!("Failed to open '{}': {e}", path.display())))?,
    );
    let ext_hint = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if ext_hint.as_deref() == Some("aac") {
        return Ok(Box::new(AdtsDemuxer::open(input)?));
    }
    let container = ctx
        .containers
        .probe_input(&mut *input, ext_hint.as_deref())
        .map_err(oxideav_err_to_io)?;
    ctx.containers
        .open_demuxer(&container, input, &ctx.codecs)
        .map_err(oxideav_err_to_io)
}

fn first_audio_stream(demuxer: &dyn Demuxer, path: &Path) -> io::Result<(u32, StreamInfo)> {
    demuxer
        .streams()
        .iter()
        .find(|stream| stream.params.media_type == MediaType::Audio)
        .map(|stream| (stream.index, stream.clone()))
        .ok_or_else(|| io::Error::other(format!("No usable audio track in '{}'", path.display())))
}

fn open_oxideav_decoder(path: &Path) -> io::Result<OxideavStreamParts> {
    let ctx = oxideav_runtime();
    let mut demuxer = open_oxideav_demuxer(path)?;
    let (stream_index, stream) = first_audio_stream(&*demuxer, path)?;
    let decoder = ctx
        .codecs
        .first_decoder(&stream.params)
        .map_err(oxideav_err_to_io)?;
    let decoder = if stream.params.codec_id.as_str() == "aac" && !stream.params.extradata.is_empty()
    {
        let (config, _) = oxideav_aac::asc::AudioSpecificConfig::parse(&stream.params.extradata)
            .map_err(|e| io::Error::other(format!("Invalid AAC configuration: {e}")))?;
        Box::new(RawAacDecoder::new(decoder, config)) as Box<dyn Decoder>
    } else {
        decoder
    };
    demuxer.set_active_streams(std::slice::from_ref(&stream_index));
    Ok((demuxer, decoder, stream_index, stream.params))
}

fn decode_with_oxideav(path: &Path) -> io::Result<(Vec<f32>, usize, u32)> {
    let (mut demuxer, mut decoder, stream_index, params) = open_oxideav_decoder(path)?;
    let channels = params.channels.unwrap_or(1).max(1) as usize;
    let sample_rate = params.sample_rate.unwrap_or(48_000);
    let mut samples = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(packet) => {
                if packet.stream_index != stream_index {
                    continue;
                }
                decoder.send_packet(&packet).map_err(oxideav_err_to_io)?;
                drain_decoder_frames(&mut *decoder, &params, &mut samples)?;
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => return Err(oxideav_err_to_io(e)),
        }
    }
    decoder.flush().map_err(oxideav_err_to_io)?;
    drain_decoder_frames(&mut *decoder, &params, &mut samples)?;

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
    decoder: &mut dyn Decoder,
    stream_params: &CodecParameters,
    out: &mut Vec<f32>,
) -> io::Result<()> {
    loop {
        let output = decoder.output_params();
        let params = output.as_ref().unwrap_or(stream_params);
        let format = params.sample_format.unwrap_or(SampleFormat::S16);
        let channels = params.channels.unwrap_or(1).max(1) as usize;
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
                out.push(sample_bytes_to_f32(&plane[start..end], format)?);
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
            out.push(sample_bytes_to_f32(chunk, format)?);
        }
    }

    Ok(out)
}

fn sample_bytes_to_f32(bytes: &[u8], format: SampleFormat) -> io::Result<f32> {
    let sample = match format {
        SampleFormat::U8 | SampleFormat::U8P => (bytes[0] as f32 - 128.0) / 128.0,
        SampleFormat::S8 => bytes[0] as i8 as f32 / i8::MAX as f32,
        SampleFormat::S16 | SampleFormat::S16P => {
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
        SampleFormat::S32 | SampleFormat::S32P => {
            i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32 / i32::MAX as f32
        }
        SampleFormat::F32 | SampleFormat::F32P => {
            f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        }
        SampleFormat::F64 | SampleFormat::F64P => f64::from_le_bytes(
            bytes[..8]
                .try_into()
                .map_err(|_| io::Error::other("OxideAV frame contains a truncated F64 sample"))?,
        ) as f32,
        _ => {
            return Err(io::Error::other(format!(
                "Unsupported OxideAV sample format {format:?}"
            )));
        }
    };
    Ok(sample)
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
    write_flac_packets_to_file(path, &packets, &output_params, sample_rate)
}

/// Mux encoded FLAC packets into a native FLAC file.
fn write_flac_packets_to_file(
    path: &Path,
    packets: &[Packet],
    output_params: &CodecParameters,
    sample_rate: u32,
) -> io::Result<()> {
    let mut ctx = RuntimeContext::new();
    oxideav_flac::register(&mut ctx);

    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, sample_rate as i64),
        duration: None,
        start_time: Some(0),
        params: output_params.clone(),
    };
    let file = std::fs::File::create(path)?;
    let output: Box<dyn oxideav_core::WriteSeek> = Box::new(file);
    let mut mux = ctx
        .containers
        .open_muxer("flac", output, std::slice::from_ref(&stream))
        .map_err(oxideav_err_to_io)?;
    mux.write_header().map_err(oxideav_err_to_io)?;
    for pkt in packets {
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
    write_ogg_flac_file(path, &packets, &output_params)
}

/// Assemble encoded FLAC packets into a FLAC-in-Ogg file.
fn write_ogg_flac_file(
    path: &Path,
    packets: &[Packet],
    output_params: &CodecParameters,
) -> io::Result<()> {
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

    let streaminfo = output_params.extradata.clone();

    let mut writer = oxideav_ogg::framing::PageWriter::new(0).with_page_target(4096);
    writer.push_packet(&mapping, 0);
    writer.flush_page();
    writer.push_packet(&streaminfo, 0);
    writer.flush_page();

    for pkt in packets {
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
    let mut rng = DitherRng::new(0x1234_5678_9abc_defe);
    pack_interleaved_samples_with_rng(samples, format, &mut rng, dither)
}

fn pack_interleaved_samples_with_rng(
    samples: &[f32],
    format: SampleFormat,
    rng: &mut DitherRng,
    dither: AudioDither,
) -> io::Result<Vec<u8>> {
    let bytes_per_sample = format.bytes_per_sample();
    let mut out = Vec::with_capacity(samples.len().saturating_mul(bytes_per_sample));

    for &sample in samples {
        let s = sample.clamp(-1.0, 1.0);
        match format {
            SampleFormat::U8 => {
                let v = ((s + 1.0) * 127.5 + dither_offset(rng, dither)).round() as u8;
                out.push(v);
            }
            SampleFormat::S16 => {
                let scale = i16::MAX as f32;
                let q = quantize_with_dither(s, scale, rng, dither)
                    .round()
                    .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                out.extend_from_slice(&q.to_le_bytes());
            }
            SampleFormat::S24 => {
                let scale = 8_388_607.0;
                let q = quantize_with_dither(s, scale, rng, dither)
                    .round()
                    .clamp(-8_388_608.0, 8_388_607.0) as i32;
                let b = q.to_le_bytes();
                out.extend_from_slice(&b[..3]);
            }
            SampleFormat::S32 => {
                let scale = i32::MAX as f32;
                let q = quantize_with_dither(s, scale, rng, dither)
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

/// Per-file streaming encoder for offline stem export.
///
/// All conversion (sample format, bit depth, dither) happens per block
/// before anything is persisted: WAV writes incrementally, while FLAC,
/// Ogg-FLAC and MP3 buffer their compressed packets in memory and assemble
/// the file at [`finish`](Self::finish) — FLAC STREAMINFO (MD5, totals) and
/// MP3 frame emission only become final at encoder flush.
pub(crate) enum StreamingEncoder {
    Wav(crate::wav_stream::WavStreamWriter),
    Buffered {
        encoder: Box<dyn oxideav_core::Encoder>,
        packets: Vec<Packet>,
        format: AudioEncodeFormat,
        path: PathBuf,
        channels: usize,
        sample_rate: u32,
        sample_format: SampleFormat,
        dither: AudioDither,
        rng: DitherRng,
        frames_fed: u64,
    },
}

impl StreamingEncoder {
    pub fn create(
        path: &Path,
        format: AudioEncodeFormat,
        channels: usize,
        sample_rate: u32,
        dither: AudioDither,
    ) -> Result<Self, String> {
        match format {
            AudioEncodeFormat::Wav(depth) => crate::wav_stream::WavStreamWriter::create(
                path,
                channels,
                sample_rate,
                depth,
                dither,
            )
            .map(Self::Wav)
            .map_err(|e| e.to_string()),
            AudioEncodeFormat::Flac(bits) => {
                Self::create_codec(path, format, channels, sample_rate, dither, bits)
            }
            AudioEncodeFormat::OggFlac(bits) => {
                Self::create_codec(path, format, channels, sample_rate, dither, bits)
            }
            AudioEncodeFormat::Mp3 => {
                if channels > 2 {
                    return Err("MP3 encode: only mono and stereo are supported".to_string());
                }
                Self::create_codec(path, format, channels, sample_rate, dither, 16)
            }
        }
    }

    fn create_codec(
        path: &Path,
        format: AudioEncodeFormat,
        channels: usize,
        sample_rate: u32,
        dither: AudioDither,
        bits: u16,
    ) -> Result<Self, String> {
        let (codec_id, sample_format, bit_rate) = match format {
            AudioEncodeFormat::Flac(_) => (
                "flac",
                flac_sample_format(bits).map_err(|e| e.to_string())?,
                None,
            ),
            AudioEncodeFormat::OggFlac(_) => (
                "flac",
                flac_sample_format(bits).map_err(|e| e.to_string())?,
                None,
            ),
            AudioEncodeFormat::Mp3 => (
                "mp3",
                SampleFormat::S16,
                Some(mp3_default_bitrate(sample_rate, channels) as u64),
            ),
            AudioEncodeFormat::Wav(_) => unreachable!("WAV handled by the streaming arm"),
        };
        let mut ctx = RuntimeContext::new();
        if codec_id == "flac" {
            oxideav_flac::register(&mut ctx);
        } else {
            oxideav_mp3::register(&mut ctx);
        }
        let params = audio_codec_params(codec_id, channels, sample_rate, sample_format, bit_rate);
        let encoder = ctx
            .codecs
            .first_encoder(&params)
            .map_err(|e| e.to_string())?;
        Ok(Self::Buffered {
            encoder,
            packets: Vec::new(),
            format,
            path: path.to_path_buf(),
            channels,
            sample_rate,
            sample_format,
            dither,
            rng: DitherRng::new(0x1234_5678_9abc_defe),
            frames_fed: 0,
        })
    }

    /// Feed one block of interleaved f32 samples.
    pub fn encode_block(&mut self, samples: &[f32]) -> io::Result<()> {
        match self {
            Self::Wav(writer) => writer.write_interleaved_f32(samples),
            Self::Buffered {
                encoder,
                packets,
                channels,
                sample_format,
                dither,
                rng,
                frames_fed,
                ..
            } => {
                if samples.is_empty() {
                    return Ok(());
                }
                if !samples.len().is_multiple_of(*channels) {
                    return Err(io::Error::other(
                        "encode: sample slice length is not a multiple of channels",
                    ));
                }
                let bytes =
                    pack_interleaved_samples_with_rng(samples, *sample_format, rng, *dither)?;
                let frame = AudioFrame {
                    samples: (samples.len() / *channels) as u32,
                    pts: Some(*frames_fed as i64),
                    data: vec![bytes],
                };
                encoder
                    .send_frame(&Frame::Audio(frame))
                    .map_err(oxideav_err_to_io)?;
                *frames_fed += (samples.len() / *channels) as u64;
                Self::drain_packets(encoder, packets)
            }
        }
    }

    fn drain_packets(
        encoder: &mut Box<dyn oxideav_core::Encoder>,
        packets: &mut Vec<Packet>,
    ) -> io::Result<()> {
        loop {
            match encoder.receive_packet() {
                Ok(packet) => packets.push(packet),
                Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => return Ok(()),
                Err(e) => return Err(oxideav_err_to_io(e)),
            }
        }
    }

    /// Flush the encoder and write the complete file.
    pub fn finish(mut self) -> io::Result<()> {
        match &mut self {
            Self::Wav(writer) => writer.finalize(),
            Self::Buffered {
                encoder,
                packets,
                format,
                path,
                sample_rate,
                ..
            } => {
                encoder.flush().map_err(oxideav_err_to_io)?;
                Self::drain_packets(encoder, packets)?;
                match format {
                    AudioEncodeFormat::Flac(_) => write_flac_packets_to_file(
                        path,
                        packets,
                        encoder.output_params(),
                        *sample_rate,
                    ),
                    AudioEncodeFormat::OggFlac(_) => {
                        write_ogg_flac_file(path, packets, encoder.output_params())
                    }
                    AudioEncodeFormat::Mp3 => {
                        let mut file = std::fs::File::create(path)?;
                        for packet in packets.iter() {
                            file.write_all(&packet.data)?;
                        }
                        Ok(())
                    }
                    AudioEncodeFormat::Wav(_) => unreachable!("WAV handled by the streaming arm"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(miri, ignore = "Miri isolation does not support filesystem open")]
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
        std::fs::remove_file(&path).expect("remove temporary test file");

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
    #[cfg_attr(miri, ignore = "Miri isolation does not support filesystem open")]
    fn oxideav_decodes_flac_and_mp3() {
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

            // Exercise the OxideAV decoder directly.
            let (decoded, out_channels, out_rate) =
                decode_with_oxideav(&path).expect("oxideav decode");
            std::fs::remove_file(&path).expect("remove temporary test file");

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
    #[cfg_attr(miri, ignore = "Miri isolation does not support filesystem open")]
    fn oxideav_decodes_vorbis() {
        let sample_rate = 44_100;
        let pcm: Vec<f32> = (0..4_096)
            .map(|i| {
                0.4 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin()
            })
            .collect();
        let config = oxideav_vorbis::oggfile::StreamEncoderConfig::new(sample_rate, 1);
        let bytes =
            oxideav_vorbis::oggfile::encode_pcm_to_ogg(&[pcm], &config).expect("encode Vorbis");
        let path =
            std::env::temp_dir().join(format!("maolan_oxideav_vorbis_{}.ogg", std::process::id()));
        std::fs::write(&path, bytes).expect("write Vorbis");
        let (decoded, channels, rate) =
            decode_audio_to_f32_interleaved_sync(&path).expect("decode Vorbis");
        std::fs::remove_file(&path).expect("remove Vorbis");
        assert_eq!(channels, 1);
        assert_eq!(rate, sample_rate);
        assert!(decoded.iter().all(|sample| sample.is_finite()));
        assert!(decoded.iter().any(|sample| sample.abs() > 0.1));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri isolation does not support filesystem open")]
    fn oxideav_decodes_aac_adts() {
        let sample_rate = 44_100;
        let pcm: Vec<i16> = (0..4_096)
            .map(|i| {
                (12_000.0
                    * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin())
                    as i16
            })
            .collect();
        let mut encoder =
            oxideav_aac::encoder::StreamEncoder::new(oxideav_aac::encoder::EncoderConfig {
                sample_rate,
                channels: 1,
                bitrate: 128_000,
            })
            .expect("create AAC encoder");
        let bytes = encoder.encode_all(&pcm).expect("encode AAC");
        let path =
            std::env::temp_dir().join(format!("maolan_oxideav_aac_{}.aac", std::process::id()));
        std::fs::write(&path, bytes).expect("write AAC");
        let (decoded, channels, rate) =
            decode_audio_to_f32_interleaved_sync(&path).expect("decode AAC");
        std::fs::remove_file(&path).expect("remove AAC");
        assert_eq!(channels, 1);
        assert_eq!(rate, sample_rate);
        assert!(decoded.iter().all(|sample| sample.is_finite()));
        assert!(decoded.iter().any(|sample| sample.abs() > 0.1));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri isolation does not support filesystem open")]
    fn oxideav_decodes_aac_mp4() {
        let sample_rate = 44_100;
        let pcm = vec![0_i16; 4_096];
        let mut encoder =
            oxideav_aac::encoder::StreamEncoder::new(oxideav_aac::encoder::EncoderConfig {
                sample_rate,
                channels: 1,
                bitrate: 128_000,
            })
            .expect("create AAC encoder");
        let adts = encoder.encode_all(&pcm).expect("encode AAC");
        let mut params = audio_codec_params("aac", 1, sample_rate, SampleFormat::S16, None);
        params.extradata = vec![0x12, 0x08];
        let stream = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, sample_rate as i64),
            duration: None,
            start_time: Some(0),
            params,
        };
        let path =
            std::env::temp_dir().join(format!("maolan_oxideav_aac_{}.m4a", std::process::id()));
        let mut mux = oxideav_runtime()
            .containers
            .open_muxer(
                "mp4",
                Box::new(std::fs::File::create(&path).expect("create M4A")),
                std::slice::from_ref(&stream),
            )
            .expect("create MP4 muxer");
        mux.write_header().expect("write MP4 header");
        let mut offset = 0;
        let mut pts = 0;
        while offset < adts.len() {
            let (header, _) =
                oxideav_aac::adts::AdtsHeader::parse(&adts[offset..]).expect("parse ADTS");
            let frame_len = header.aac_frame_length as usize;
            let mut packet = Packet::new(
                0,
                stream.time_base,
                adts[offset + 7..offset + frame_len].to_vec(),
            );
            packet.pts = Some(pts);
            packet.duration = Some(1_024);
            mux.write_packet(&packet).expect("write AAC packet");
            offset += frame_len;
            pts += 1_024;
        }
        mux.write_trailer().expect("write MP4 trailer");
        let (decoded, channels, rate) =
            decode_audio_to_f32_interleaved_sync(&path).expect("decode M4A");
        std::fs::remove_file(&path).expect("remove M4A");
        assert_eq!(channels, 1);
        assert_eq!(rate, sample_rate);
        assert!(decoded.len() >= 4_096);
        assert!(decoded.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri isolation does not support filesystem open")]
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
        std::fs::remove_file(&path).expect("remove temporary test file");

        assert_eq!(info.sample_rate, sample_rate);
        assert_eq!(info.channels, channels);
        let probed_frames = info.frames.expect("wav probe should report frames");
        assert!(
            (probed_frames as i64 - frames as i64).abs() <= 1,
            "probed frames {probed_frames} != {frames}"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri isolation does not support filesystem open")]
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
        std::fs::remove_file(&path).expect("remove temporary test file");

        assert_eq!(info.sample_rate, sample_rate);
        assert_eq!(info.channels, channels);
        let probed_frames = info.frames.expect("flac probe should report frames");
        assert!(
            (probed_frames as i64 - frames as i64).abs() <= 1,
            "probed frames {probed_frames} != {frames}"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri isolation does not support filesystem open")]
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
        std::fs::remove_file(&path).expect("remove temporary test file");

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
