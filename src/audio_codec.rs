use std::io::{self, Write};
use std::path::Path;

pub fn decode_audio_to_f32_interleaved_sync(path: &Path) -> io::Result<(Vec<f32>, usize, u32)> {
    match decode_audio_to_f32_interleaved_ffmpeg(path) {
        Ok(ok) => Ok(ok),
        Err(ffmpeg_err) => {
            if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
            {
                decode_wav_fallback(path).map_err(|wav_err| {
                    io::Error::other(format!(
                        "Audio decode failed for '{}': ffmpeg={ffmpeg_err}; wav_fallback={wav_err}",
                        path.display()
                    ))
                })
            } else {
                Err(ffmpeg_err)
            }
        }
    }
}

pub fn decode_audio_to_f32_interleaved_preferring_wav(
    path: &Path,
) -> io::Result<(Vec<f32>, usize, u32)> {
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
    {
        match decode_wav_fallback(path) {
            Ok(ok) => return Ok(ok),
            Err(_wav_err) => {
                // Fall through to FFmpeg path below.
            }
        }
    }
    decode_audio_to_f32_interleaved_sync(path)
}

fn decode_audio_to_f32_interleaved_ffmpeg(path: &Path) -> io::Result<(Vec<f32>, usize, u32)> {
    use ffmpeg_next::{format::sample::Type as SampleType, media::Type};

    ffmpeg_init().map_err(|e| io::Error::other(format!("FFmpeg init failed: {e}")))?;
    let mut ictx = ffmpeg_next::format::input(path)
        .map_err(|e| io::Error::other(format!("Failed to open '{}': {e}", path.display())))?;
    let stream = ictx
        .streams()
        .best(Type::Audio)
        .ok_or_else(|| io::Error::other(format!("No audio stream in '{}'", path.display())))?;
    let stream_index = stream.index();
    let mut decoder = ffmpeg_next::codec::Context::from_parameters(stream.parameters())
        .map_err(|e| io::Error::other(format!("Decoder init failed: {e}")))?
        .decoder()
        .audio()
        .map_err(|e| io::Error::other(format!("Audio decoder init failed: {e}")))?;
    let sample_rate = decoder.rate() as u32;
    let channels = decoder.channels().max(1) as usize;
    let mut samples = Vec::<f32>::new();
    let mut raw_frame = ffmpeg_next::frame::Audio::empty();
    let append_frame = |frame: &ffmpeg_next::frame::Audio,
                        channels: usize,
                        out: &mut Vec<f32>|
     -> io::Result<()> {
        let frame_samples = frame.samples();
        match frame.format() {
            ffmpeg_next::format::Sample::F32(SampleType::Packed) => {
                let plane = frame.plane::<f32>(0);
                out.extend_from_slice(plane);
            }
            ffmpeg_next::format::Sample::F32(SampleType::Planar) => {
                let start = out.len();
                out.resize(start + frame_samples * channels, 0.0);
                for ch in 0..channels {
                    let plane = frame.plane::<f32>(ch);
                    for i in 0..frame_samples {
                        out[start + i * channels + ch] = plane[i];
                    }
                }
            }
            ffmpeg_next::format::Sample::I16(SampleType::Packed) => {
                let plane = frame.plane::<i16>(0);
                out.extend(plane.iter().map(|&v| v as f32 / 32768.0));
            }
            ffmpeg_next::format::Sample::I16(SampleType::Planar) => {
                let start = out.len();
                out.resize(start + frame_samples * channels, 0.0);
                for ch in 0..channels {
                    let plane = frame.plane::<i16>(ch);
                    for i in 0..frame_samples {
                        out[start + i * channels + ch] = plane[i] as f32 / 32768.0;
                    }
                }
            }
            ffmpeg_next::format::Sample::I32(SampleType::Packed) => {
                let plane = frame.plane::<i32>(0);
                out.extend(plane.iter().map(|&v| v as f32 / 2_147_483_648.0));
            }
            ffmpeg_next::format::Sample::I32(SampleType::Planar) => {
                let start = out.len();
                out.resize(start + frame_samples * channels, 0.0);
                for ch in 0..channels {
                    let plane = frame.plane::<i32>(ch);
                    for i in 0..frame_samples {
                        out[start + i * channels + ch] = plane[i] as f32 / 2_147_483_648.0;
                    }
                }
            }
            other => {
                return Err(io::Error::other(format!(
                    "Unsupported decoded sample format: {other:?}"
                )));
            }
        }
        Ok(())
    };
    for (stream, packet) in ictx.packets() {
        if stream.index() != stream_index {
            continue;
        }
        decoder
            .send_packet(&packet)
            .map_err(|e| io::Error::other(format!("Failed to send packet: {e}")))?;
        while decoder.receive_frame(&mut raw_frame).is_ok() {
            append_frame(&raw_frame, channels, &mut samples)?;
        }
    }
    let _ = decoder.send_eof();
    while decoder.receive_frame(&mut raw_frame).is_ok() {
        append_frame(&raw_frame, channels, &mut samples)?;
    }
    if samples.is_empty() {
        return Err(io::Error::other(format!(
            "Audio file '{}' contains no samples",
            path.display()
        )));
    }
    Ok((samples, channels, sample_rate))
}

fn decode_wav_fallback(path: &Path) -> io::Result<(Vec<f32>, usize, u32)> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(io::Error::other("Not a RIFF/WAVE file"));
    }
    let mut pos = 12usize;
    let mut channels = 0usize;
    let mut sample_rate = 0u32;
    let mut bits_per_sample = 0u16;
    let mut audio_format = 0u16;
    let mut data_offset = 0usize;
    let mut data_len = 0usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let chunk_start = pos + 8;
        let chunk_end = chunk_start.saturating_add(len);
        if chunk_end > bytes.len() {
            break;
        }
        if id == b"fmt " && len >= 16 {
            audio_format = u16::from_le_bytes([bytes[chunk_start], bytes[chunk_start + 1]]);
            channels =
                u16::from_le_bytes([bytes[chunk_start + 2], bytes[chunk_start + 3]]) as usize;
            sample_rate = u32::from_le_bytes([
                bytes[chunk_start + 4],
                bytes[chunk_start + 5],
                bytes[chunk_start + 6],
                bytes[chunk_start + 7],
            ]);
            bits_per_sample =
                u16::from_le_bytes([bytes[chunk_start + 14], bytes[chunk_start + 15]]);
        } else if id == b"data" {
            data_offset = chunk_start;
            data_len = len;
        }
        pos = chunk_end + (len & 1);
    }
    if channels == 0 || sample_rate == 0 || data_len == 0 {
        return Err(io::Error::other("Missing WAV fmt/data chunks"));
    }
    let frame_bytes = channels
        .checked_mul((bits_per_sample as usize).saturating_div(8))
        .ok_or_else(|| io::Error::other("Invalid WAV frame size"))?;
    if frame_bytes == 0 || data_offset + data_len > bytes.len() {
        return Err(io::Error::other("Invalid WAV data"));
    }
    let mut out = Vec::<f32>::new();
    let data = &bytes[data_offset..data_offset + data_len];
    match (audio_format, bits_per_sample) {
        (3, 32) => {
            out.reserve(data.len() / 4);
            for b in data.chunks_exact(4) {
                out.push(f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            }
        }
        (1, 16) => {
            out.reserve(data.len() / 2);
            for b in data.chunks_exact(2) {
                out.push(i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0);
            }
        }
        (1, 24) => {
            out.reserve(data.len() / 3);
            for b in data.chunks_exact(3) {
                let v = ((b[2] as i32) << 24 | (b[1] as i32) << 16 | (b[0] as i32) << 8) >> 8;
                out.push(v as f32 / 8_388_608.0);
            }
        }
        (1, 32) => {
            out.reserve(data.len() / 4);
            for b in data.chunks_exact(4) {
                out.push(i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0);
            }
        }
        _ => {
            return Err(io::Error::other(format!(
                "Unsupported WAV format: audio_format={audio_format} bits={bits_per_sample}"
            )));
        }
    }
    if out.is_empty() {
        return Err(io::Error::other("WAV contains no samples"));
    }
    Ok((out, channels, sample_rate))
}

pub fn write_wav_f32(
    path: &Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
) -> io::Result<()> {
    let bytes_per_sample = 4usize;
    let block_align = (channels.max(1) * bytes_per_sample) as u16;
    let byte_rate = sample_rate * u32::from(block_align);
    let data_size = samples
        .len()
        .checked_mul(bytes_per_sample)
        .ok_or_else(|| io::Error::other("WAV data too large"))? as u32;
    let riff_size = 36u32
        .checked_add(data_size)
        .ok_or_else(|| io::Error::other("WAV file too large"))?;

    let mut file = std::fs::File::create(path)?;
    file.write_all(b"RIFF")?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.write_all(b"WAVE")?;
    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&3u16.to_le_bytes())?;
    file.write_all(&(channels.max(1) as u16).to_le_bytes())?;
    file.write_all(&sample_rate.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&32u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_size.to_le_bytes())?;
    for &sample in samples {
        file.write_all(&sample.clamp(-1.0, 1.0).to_le_bytes())?;
    }
    Ok(())
}

fn ffmpeg_init() -> Result<(), ffmpeg_next::Error> {
    static RESULT: std::sync::OnceLock<Result<(), ffmpeg_next::Error>> = std::sync::OnceLock::new();
    *RESULT.get_or_init(ffmpeg_next::init)
}
