use std::io::{self, Write};
use std::path::Path;

/// Decode an audio file to interleaved `f32` samples.
///
/// The format is detected from the file extension (case-insensitive):
/// `.wav`, `.flac`, or `.opus`. Returns `(samples, channels, sample_rate)`.
/// All decode paths emit samples in the range `[-1.0, 1.0]`.
pub fn decode_audio_to_f32_interleaved_sync(path: &Path) -> io::Result<(Vec<f32>, usize, u32)> {
    let ext = file_extension(path)?;
    match ext.as_str() {
        "wav" => decode_wav(path),
        "flac" => decode_flac(path),
        "opus" => decode_opus(path),
        _ => Err(io::Error::other(format!(
            "Unsupported audio extension '{ext}' for '{}'",
            path.display()
        ))),
    }
}

/// Decode a WAV file preferentially, falling back to the general decoder.
///
/// This preserves the old "try the WAV-specific path first" behavior for
/// callers that want to avoid the format-detection overhead/differences for
/// `.wav` inputs.
pub fn decode_audio_to_f32_interleaved_preferring_wav(
    path: &Path,
) -> io::Result<(Vec<f32>, usize, u32)> {
    if file_extension(path)?.eq_ignore_ascii_case("wav")
        && let Ok(ok) = decode_wav(path)
    {
        return Ok(ok);
    }
    decode_audio_to_f32_interleaved_sync(path)
}

fn file_extension(path: &Path) -> io::Result<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .ok_or_else(|| io::Error::other(format!("Missing file extension for '{}'", path.display())))
}

// ---------------------------------------------------------------------------
// WAV
// ---------------------------------------------------------------------------

fn decode_wav(path: &Path) -> io::Result<(Vec<f32>, usize, u32)> {
    let mut reader = hound::WavReader::open(path)
        .map_err(|e| io::Error::other(format!("Failed to open WAV '{}': {e}", path.display())))?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    let sample_rate = spec.sample_rate;
    let bits_per_sample = spec.bits_per_sample;

    let mut samples = Vec::new();
    match spec.sample_format {
        hound::SampleFormat::Float => {
            for s in reader.samples::<f32>() {
                let v = s.map_err(|e| {
                    io::Error::other(format!("WAV decode error for '{}': {e}", path.display()))
                })?;
                samples.push(v.clamp(-1.0, 1.0));
            }
        }
        hound::SampleFormat::Int => {
            for s in reader.samples::<i32>() {
                let v = s.map_err(|e| {
                    io::Error::other(format!("WAV decode error for '{}': {e}", path.display()))
                })?;
                samples.push(int_sample_to_f32(v, bits_per_sample));
            }
        }
    }

    if samples.is_empty() {
        return Err(io::Error::other(format!(
            "WAV '{}' contains no samples",
            path.display()
        )));
    }
    Ok((samples, channels, sample_rate))
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

// ---------------------------------------------------------------------------
// FLAC
// ---------------------------------------------------------------------------

fn decode_flac(path: &Path) -> io::Result<(Vec<f32>, usize, u32)> {
    let bytes = std::fs::read(path)?;
    let decoded = libflac_rs::decode(&bytes)
        .ok_or_else(|| io::Error::other(format!("Failed to decode FLAC '{}'", path.display())))?;

    let channels = decoded.channels as usize;
    let sample_rate = decoded.sample_rate;
    let bits_per_sample = decoded.bits_per_sample as u16;

    let mut samples = Vec::with_capacity(decoded.interleaved.len());
    for v in decoded.interleaved {
        samples.push(int_sample_to_f32(v, bits_per_sample));
    }

    if samples.is_empty() {
        return Err(io::Error::other(format!(
            "FLAC '{}' contains no samples",
            path.display()
        )));
    }
    Ok((samples, channels, sample_rate))
}

pub fn write_flac(
    path: &Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    bits_per_sample: u16,
) -> io::Result<()> {
    if channels == 0 {
        return Err(io::Error::other("FLAC write: channels must be > 0"));
    }
    if !samples.len().is_multiple_of(channels) {
        return Err(io::Error::other(
            "FLAC write: sample slice length is not a multiple of channels",
        ));
    }
    if !matches!(bits_per_sample, 16 | 24 | 32) {
        return Err(io::Error::other(format!(
            "FLAC write: unsupported bits_per_sample {bits_per_sample} (use 16, 24, or 32)"
        )));
    }

    let pcm: Vec<i32> = samples
        .iter()
        .map(|&s| f32_sample_to_int(s, bits_per_sample))
        .collect();

    let config =
        libflac_rs::EncoderConfig::new(channels as u32, bits_per_sample as u32, sample_rate);
    let encoder = libflac_rs::Encoder::new(config);
    let encoded = encoder.encode(&pcm);

    std::fs::write(path, encoded)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Opus (Ogg Opus container)
// ---------------------------------------------------------------------------

fn decode_opus(path: &Path) -> io::Result<(Vec<f32>, usize, u32)> {
    use ogg::PacketReader;

    let file = std::fs::File::open(path)?;
    let mut reader = PacketReader::new(file);

    // First packet: OpusHead
    let head_packet = reader
        .read_packet()
        .map_err(|e| io::Error::other(format!("Ogg read error for '{}': {e}", path.display())))?
        .ok_or_else(|| {
            io::Error::other(format!(
                "Opus file '{}' contains no packets",
                path.display()
            ))
        })?;

    if !head_packet.data.starts_with(b"OpusHead") {
        return Err(io::Error::other(format!(
            "Opus file '{}' missing OpusHead header",
            path.display()
        )));
    }
    let channels = parse_opus_head_channels(&head_packet.data)?;

    // Second packet: OpusTags (skip)
    let _tags_packet = reader
        .read_packet()
        .map_err(|e| io::Error::other(format!("Ogg read error for '{}': {e}", path.display())))?;

    // Remaining packets are audio.
    let sample_rate = 48_000;
    let mut decoder = opus_rs::OpusDecoder::new(sample_rate, channels).map_err(|e| {
        io::Error::other(format!(
            "Opus decoder init failed for '{}': {e}",
            path.display()
        ))
    })?;

    let mut output = Vec::new();
    loop {
        let packet = match reader.read_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => {
                return Err(io::Error::other(format!(
                    "Ogg read error for '{}': {e}",
                    path.display()
                )));
            }
        };

        let frame_size = opus_packet_frame_size(&packet.data, sample_rate).ok_or_else(|| {
            io::Error::other(format!(
                "Invalid Opus packet framing in '{}'",
                path.display()
            ))
        })?;

        let mut frame = vec![0.0f32; frame_size * channels];
        decoder
            .decode(&packet.data, frame_size, &mut frame)
            .map_err(|e| {
                io::Error::other(format!("Opus decode error for '{}': {e}", path.display()))
            })?;
        output.extend_from_slice(&frame);
    }

    if output.is_empty() {
        return Err(io::Error::other(format!(
            "Opus file '{}' contains no audio",
            path.display()
        )));
    }
    Ok((output, channels, sample_rate as u32))
}

pub fn write_opus(
    path: &Path,
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    bitrate_bps: i32,
) -> io::Result<()> {
    use ogg::{PacketWriteEndInfo, PacketWriter};

    if channels == 0 || channels > 2 {
        return Err(io::Error::other(
            "Opus write: only mono and stereo are supported",
        ));
    }
    if !samples.len().is_multiple_of(channels) {
        return Err(io::Error::other(
            "Opus write: sample slice length is not a multiple of channels",
        ));
    }
    if !matches!(sample_rate, 8_000 | 12_000 | 16_000 | 24_000 | 48_000) {
        return Err(io::Error::other(format!(
            "Opus write: unsupported sample rate {sample_rate} (use 8000, 12000, 16000, 24000, or 48000)"
        )));
    }

    let total_samples = samples.len() / channels;
    // 20 ms frames are the standard Opus frame size and are valid for all
    // supported sample rates / applications.
    let frame_size = (sample_rate as usize / 50).max(1);

    let mut encoder =
        opus_rs::OpusEncoder::new(sample_rate as i32, channels, opus_rs::Application::Audio)
            .map_err(|e| io::Error::other(format!("Opus encoder init failed: {e}")))?;
    encoder.bitrate_bps = bitrate_bps;

    let file = std::fs::File::create(path)?;
    let mut writer = PacketWriter::new(file);
    let serial = ogg_serial_from_path(path);

    // OpusHead must be the sole packet on the first page.
    writer.write_packet(
        opus_head(channels as u8, sample_rate),
        serial,
        PacketWriteEndInfo::EndPage,
        0,
    )?;

    // OpusTags on its own page.
    writer.write_packet(opus_tags(), serial, PacketWriteEndInfo::EndPage, 0)?;

    let mut packet_buf = vec![0u8; 4000];
    let mut encoded_granule: u64 = 0;

    for frame_idx in 0..frame_count(total_samples, frame_size) {
        let start = frame_idx * frame_size;
        let end = ((start + frame_size).min(total_samples)).max(start);
        let actual_len = end - start;

        // Opus requires a full frame; pad the tail with silence.
        let mut frame_input = vec![0.0f32; frame_size * channels];
        frame_input[..actual_len * channels]
            .copy_from_slice(&samples[start * channels..end * channels]);

        let encoded_len = encoder
            .encode(&frame_input, frame_size, &mut packet_buf)
            .map_err(|e| io::Error::other(format!("Opus encode failed: {e}")))?;

        encoded_granule = encoded_granule.saturating_add(frame_size as u64);
        // For the final frame use the real sample count as the granule position
        // so compliant decoders can trim the padding.
        let is_last = frame_idx == frame_count(total_samples, frame_size) - 1;
        let granule = if is_last {
            total_samples.min(encoded_granule as usize) as u64
        } else {
            encoded_granule
        };

        let end_info = if is_last {
            PacketWriteEndInfo::EndStream
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        writer.write_packet(
            packet_buf[..encoded_len].to_vec(),
            serial,
            end_info,
            granule,
        )?;
    }

    Ok(())
}

fn parse_opus_head_channels(head: &[u8]) -> io::Result<usize> {
    if head.len() < 10 || head[8] != 1 {
        return Err(io::Error::other(
            "OpusHead header is too short or has unsupported version",
        ));
    }
    let channels = head[9] as usize;
    if channels == 0 {
        return Err(io::Error::other("OpusHead reports zero channels"));
    }
    Ok(channels)
}

fn opus_head(channels: u8, sample_rate: u32) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(channels);
    head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    head.extend_from_slice(&sample_rate.to_le_bytes()); // input sample rate
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family (mono/stereo)
    head
}

fn opus_tags() -> Vec<u8> {
    let vendor = b"maolan-engine";
    let mut tags = Vec::with_capacity(8 + 4 + vendor.len() + 4);
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes()); // user comment count
    tags
}

fn ogg_serial_from_path(path: &Path) -> u32 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    path.as_os_str().hash(&mut hasher);
    hasher.finish() as u32
}

/// Compute the total number of fixed-size frames needed for `total_samples`,
/// padding the final frame if necessary.
fn frame_count(total_samples: usize, frame_size: usize) -> usize {
    if total_samples == 0 {
        return 0;
    }
    total_samples.div_ceil(frame_size)
}

/// Determine the total number of samples encoded by an Opus packet.
/// The TOC byte describes the mode and the per-sub-frame duration; this
/// helper multiplies by the number of sub-frames to obtain the value that
/// the opus-rs decoder expects as `frame_size`.
fn opus_packet_frame_size(packet: &[u8], sample_rate: i32) -> Option<usize> {
    if packet.is_empty() {
        return None;
    }
    let toc = packet[0];

    let sub_frame_duration_ms = if toc & 0x80 != 0 {
        // CELT-only
        match (toc >> 3) & 0x03 {
            0 => 2,
            1 => 5,
            2 => 10,
            _ => 20,
        }
    } else if toc & 0x60 == 0x60 {
        // Hybrid
        if ((toc >> 3) & 0x01) == 0 { 10 } else { 20 }
    } else {
        // SILK-only
        match (toc >> 3) & 0x03 {
            0 => 10,
            1 => 20,
            2 => 40,
            _ => 60,
        }
    };

    let sub_frame_size = (sample_rate as i64 * sub_frame_duration_ms as i64 / 1000).max(1) as usize;
    let code = toc & 0x03;
    let frame_count = match code {
        0 => 1,
        1 | 2 => 2,
        3 => {
            if packet.len() < 2 {
                return None;
            }
            let n = (packet[1] & 0x3F) as usize;
            if n == 0 {
                return None;
            }
            n
        }
        _ => return None,
    };

    Some(sub_frame_size * frame_count)
}

// ---------------------------------------------------------------------------
// Sample format conversions
// ---------------------------------------------------------------------------

fn int_sample_to_f32(v: i32, bits_per_sample: u16) -> f32 {
    let divisor = match bits_per_sample {
        8 => 128.0,
        16 => 32_768.0,
        24 => 8_388_608.0,
        32 => 2_147_483_648.0,
        _ => 2.0f32.powi(bits_per_sample as i32 - 1),
    };
    (v as f32 / divisor).clamp(-1.0, 1.0)
}

fn f32_sample_to_int(v: f32, bits_per_sample: u16) -> i32 {
    let max_positive = match bits_per_sample {
        16 => 32_767i32,
        24 => 8_388_607i32,
        32 => 2_147_483_647i32,
        _ => ((1u64 << (bits_per_sample - 1)) - 1) as i32,
    };
    (v.clamp(-1.0, 1.0) * max_positive as f32).round() as i32
}
