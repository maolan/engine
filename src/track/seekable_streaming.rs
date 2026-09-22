//! Seekable streaming playback for engine-rate PCM/WAV clips.
//!
//! A [`SeekableStreamingClipBuffer`] serves audio from a WAV file that is
//! already at the engine sample rate. Unlike [`super::streaming::StreamingClipBuffer`]
//! (which decodes-and-discards to seek), the producer thread owns the
//! `File` and can jump to any frame with a single O(1) seek, so transport
//! seeks, loop wraps, and clip re-launches land immediately instead of
//! paying a decode-and-discard penalty.
//!
//! Structure mirrors the compressed streaming module: the audio-thread
//! consumer ([`SeekableStreamingClipBuffer::read_frames`]) only pops
//! per-channel SPSC `rtrb` rings and silence-fills on underrun; the producer
//! thread does all file I/O and sample conversion. Non-sequential reads post
//! an absolute frame to the producer through the same lock-free atomic
//! seek-request slot used by the compressed path.
//!
//! Random access for offline features (pitch correction, reversed playback)
//! goes through [`SeekableStreamingClipBuffer::read_window`], which opens a
//! short-lived file handle on the calling thread and returns exactly `len`
//! frames per channel, silence-padded past EOF. It never touches the rings,
//! so it cannot disturb the real-time read position.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rtrb::{Consumer, Producer, RingBuffer};

use super::streaming::{DECODE_CHUNK_FRAMES, ProducerControl, try_acquire_producer_slot};
use crate::audio_codec::probe_audio_file;

/// On-disk layout of a PCM (integer or float) RIFF/WAVE stream.
#[derive(Clone)]
struct WavPcmLayout {
    /// Absolute file offset of the first sample byte of the `data` chunk.
    data_offset: u64,
    /// Byte length of the `data` chunk payload.
    data_len: u64,
    channels: usize,
    bits_per_sample: u16,
    /// WAVE format tag: 1 = integer PCM, 3 = IEEE float.
    format_tag: u16,
}

impl WavPcmLayout {
    fn bytes_per_sample(&self) -> usize {
        (self.bits_per_sample as usize / 8).max(1)
    }

    fn block_align(&self) -> usize {
        self.bytes_per_sample() * self.channels.max(1)
    }

    fn total_frames(&self) -> usize {
        self.data_len as usize / self.block_align().max(1)
    }

    /// Convert one interleaved raw sample window (exactly `frames *
    /// channels` samples worth of bytes) to interleaved `f32`.
    fn convert_interleaved(&self, bytes: &[u8], frames: usize, out: &mut Vec<f32>) {
        let bps = self.bytes_per_sample();
        let total = frames.saturating_mul(self.channels.max(1));
        out.clear();
        out.reserve(total);
        for chunk in bytes[..total.saturating_mul(bps).min(bytes.len())].chunks_exact(bps) {
            out.push(match (self.format_tag, self.bits_per_sample) {
                (3, 32) => f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                (_, 16) => {
                    f32::from(i16::from_le_bytes([chunk[0], chunk[1]])) / f32::from(i16::MAX)
                }
                (_, 24) => {
                    let v = i32::from_le_bytes([
                        chunk[0],
                        chunk[1],
                        chunk[2],
                        if chunk[2] & 0x80 != 0 { 0xFF } else { 0 },
                    ]);
                    v as f32 / 8_388_607.0
                }
                (_, 32) => {
                    i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f32
                        / i32::MAX as f32
                }
                (_, 8) => (chunk[0] as f32 - 128.0) / 128.0,
                _ => 0.0,
            });
        }
    }
}

/// Parse the RIFF/WAVE chunk layout and verify the stream is uncompressed
/// PCM (integer or float). Returns an error for any other codec so the
/// caller can fall back to the general streaming decoder.
fn parse_wav_pcm_layout(path: &Path) -> io::Result<WavPcmLayout> {
    let mut file = File::open(path)?;
    let mut riff = [0u8; 12];
    file.read_exact(&mut riff)?;
    if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        return Err(io::Error::other(format!(
            "'{}' is not a RIFF/WAVE file",
            path.display()
        )));
    }
    let mut format_tag = None;
    let mut channels = None;
    let mut bits_per_sample = None;
    let mut data_offset = None;
    let mut data_len = None;
    loop {
        let mut header = [0u8; 8];
        match file.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        let size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if &header[0..4] == b"fmt " {
            let mut fmt = vec![0u8; size as usize];
            file.read_exact(&mut fmt)?;
            if fmt.len() < 16 {
                return Err(io::Error::other("WAV fmt chunk too short"));
            }
            format_tag = Some(u16::from_le_bytes([fmt[0], fmt[1]]));
            channels = Some(u16::from_le_bytes([fmt[2], fmt[3]]));
            bits_per_sample = Some(u16::from_le_bytes([fmt[14], fmt[15]]));
            // WAVE_FORMAT_EXTENSIBLE (0xFFFE) carries the real format tag in
            // the first two bytes of the SubFormat GUID (at offset 24).
            if format_tag == Some(0xFFFE) && fmt.len() >= 26 {
                format_tag = Some(u16::from_le_bytes([fmt[24], fmt[25]]));
            }
            // Chunks are word-aligned; the payload size is rounded up to
            // even and was fully consumed by `read_exact` above.
            if size & 1 == 1 {
                file.seek(SeekFrom::Current(1))?;
            }
        } else if &header[0..4] == b"data" {
            data_offset = Some(file.stream_position()?);
            data_len = Some(size as u64);
            break;
        } else {
            // Unknown chunk: skip its payload (word-aligned).
            let payload_end = file
                .stream_position()?
                .saturating_add(size as u64)
                .saturating_add(u64::from(size & 1));
            file.seek(SeekFrom::Start(payload_end))?;
        }
        if format_tag.is_some() && data_offset.is_some() {
            break;
        }
    }
    let format_tag = format_tag.ok_or_else(|| io::Error::other("WAV file has no fmt chunk"))?;
    let channels = channels.ok_or_else(|| io::Error::other("WAV file has no fmt chunk"))?;
    let bits_per_sample =
        bits_per_sample.ok_or_else(|| io::Error::other("WAV file has no fmt chunk"))?;
    if !matches!(format_tag, 1 | 3) {
        return Err(io::Error::other(format!(
            "WAV codec {format_tag} is not uncompressed PCM"
        )));
    }
    let data_offset = data_offset.ok_or_else(|| io::Error::other("WAV file has no data chunk"))?;
    let data_len = data_len.unwrap_or(0);
    Ok(WavPcmLayout {
        data_offset,
        data_len,
        channels: channels.max(1) as usize,
        bits_per_sample,
        format_tag,
    })
}

/// Read `buf.len()` bytes from `file` at absolute `position` without
/// changing the shared file cursor semantics the producer relies on.
#[cfg(unix)]
fn read_exact_at(file: &File, position: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, position)
}

#[cfg(not(unix))]
fn read_exact_at(file: &File, position: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut file = file;
    let mut clone = file.try_clone()?;
    clone.seek(SeekFrom::Start(position))?;
    clone.read_exact(buf)
}

/// Convert `frames` interleaved raw bytes into per-channel planes.
fn deinterleave(data: &[f32], frames: usize, channels: usize, rings: &mut [Producer<f32>]) {
    for (channel, ring) in rings.iter_mut().enumerate() {
        for frame in 0..frames {
            // Lockstep: the same frame count is pushed to every ring.
            let _ = ring.push(data[frame * channels + channel]);
        }
    }
}

pub struct SeekableStreamingClipBuffer {
    pub channels: usize,
    pub engine_sample_rate: usize,
    total_frames: usize,
    path: PathBuf,
    layout: WavPcmLayout,
    rings: Vec<Mutex<Consumer<f32>>>,
    eof: Arc<AtomicBool>,
    underruns: AtomicUsize,
    control: ProducerControl,
    /// Next source frame the consumer expects to read; `u64::MAX` until the
    /// first read. Used to detect non-sequential (seek) reads.
    expected_next: AtomicU64,
    producer: Mutex<Option<ProducerThread>>,
}

impl std::fmt::Debug for SeekableStreamingClipBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeekableStreamingClipBuffer")
            .field("channels", &self.channels)
            .field("engine_sample_rate", &self.engine_sample_rate)
            .field("total_frames", &self.total_frames)
            .field("eof", &self.is_eof())
            .field("underruns", &self.underruns())
            .finish()
    }
}

struct ProducerThread {
    handle: JoinHandle<()>,
    done: std::sync::mpsc::Receiver<()>,
}

impl SeekableStreamingClipBuffer {
    /// Start a seekable streaming producer for an engine-rate PCM/WAV
    /// `path`. Each channel ring holds `multiplier * period_frames` floats.
    pub fn start(
        path: &Path,
        engine_rate: usize,
        period_frames: usize,
        multiplier: usize,
    ) -> io::Result<Self> {
        let layout = parse_wav_pcm_layout(path)?;
        let info = probe_audio_file(path)?;
        let channels = layout.channels.max(1);
        if info.sample_rate as usize != engine_rate {
            return Err(io::Error::other(format!(
                "'{}' is at {} Hz, not the engine rate {engine_rate} Hz",
                path.display(),
                info.sample_rate
            )));
        }
        // Guard against unbounded producer threads in sessions with very
        // many clips (see `preload_audio_clip_cache`); beyond the cap the
        // caller falls back to a whole-file buffered decode.
        if !try_acquire_producer_slot() {
            return Err(io::Error::other(format!(
                "Streaming producer cap reached; not starting seekable stream for '{}'",
                path.display()
            )));
        }
        let capacity = multiplier.max(1).saturating_mul(period_frames.max(1));
        let mut rings = Vec::with_capacity(channels);
        let mut producers = Vec::with_capacity(channels);
        for _ in 0..channels {
            let (producer, consumer) = RingBuffer::new(capacity.max(1));
            producers.push(producer);
            rings.push(Mutex::new(consumer));
        }
        let control = ProducerControl {
            stop: Arc::new(AtomicBool::new(false)),
            seek_request: Arc::new(AtomicUsize::new(0)),
            eof: Arc::new(AtomicBool::new(false)),
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let thread_control = ProducerControl {
            stop: control.stop.clone(),
            seek_request: control.seek_request.clone(),
            eof: control.eof.clone(),
        };
        let path_buf = path.to_path_buf();
        let total_frames = layout.total_frames();
        let handle = std::thread::Builder::new()
            .name(format!("maolan-seekstream-{}", path.display()))
            .spawn({
                let layout = layout.clone();
                move || {
                    producer_main(path_buf, layout, producers, thread_control, done_tx);
                }
            })
            .map_err(|e| {
                super::streaming::release_producer_slot();
                io::Error::other(format!("Failed to spawn seekable streaming producer: {e}"))
            })?;
        Ok(Self {
            channels,
            engine_sample_rate: engine_rate,
            total_frames,
            path: path.to_path_buf(),
            layout,
            rings,
            eof: control.eof.clone(),
            underruns: AtomicUsize::new(0),
            control,
            expected_next: AtomicU64::new(u64::MAX),
            producer: Mutex::new(Some(ProducerThread {
                handle,
                done: done_rx,
            })),
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn is_eof(&self) -> bool {
        self.eof.load(Ordering::Acquire)
    }

    pub fn underruns(&self) -> usize {
        self.underruns.load(Ordering::Relaxed)
    }

    /// Total decoded frames in the source file, if known.
    pub fn total_frames(&self) -> Option<u64> {
        Some(self.total_frames as u64)
    }

    /// Read up to `len` frames starting at source frame `from_frame` into
    /// `out` (one `Vec<f32>` per channel, each at least `len` long).
    ///
    /// A non-sequential `from_frame` posts an O(1) file seek to the
    /// producer. Frames that are not available (producer behind, or end of
    /// file) are silence-filled and counted as underruns.
    ///
    /// Returns the number of frames actually read from the rings (the rest
    /// of `out[..len]` is silence).
    pub fn read_frames(&self, from_frame: usize, len: usize, out: &mut [Vec<f32>]) -> usize {
        let expected = self.expected_next.load(Ordering::Relaxed);
        if expected != u64::MAX && expected != from_frame as u64 {
            self.control
                .seek_request
                .store(from_frame.saturating_add(1), Ordering::Release);
        }
        self.expected_next
            .store(from_frame as u64 + len as u64, Ordering::Relaxed);

        if len == 0 {
            return 0;
        }
        let avail = self
            .rings
            .iter()
            .filter_map(|ring| ring.lock().ok().map(|consumer| consumer.slots()))
            .min()
            .unwrap_or(0);
        let frames = avail.min(len);
        for (channel, ring) in self.rings.iter().enumerate() {
            let Some(dst) = out.get_mut(channel) else {
                break;
            };
            let Ok(mut consumer) = ring.lock() else { break };
            let mut read = 0;
            while read < frames {
                match consumer.pop() {
                    Ok(sample) => {
                        dst[read] = sample;
                        read += 1;
                    }
                    Err(_) => break,
                }
            }
            for sample in dst.iter_mut().take(len).skip(read) {
                *sample = 0.0;
            }
        }
        if frames < len {
            let underruns = self.underruns.fetch_add(1, Ordering::Relaxed) + 1;
            if underruns % 100 == 1 {
                tracing::warn!(
                    "Seekable streaming clip underrun #{underruns} (read {frames}/{len} frames, eof={})",
                    self.is_eof()
                );
            }
        }
        frames
    }

    /// Random access read for offline features (pitch correction, reversed
    /// playback): returns exactly `len` frames per channel starting at
    /// source frame `start`, silence-padded past EOF.
    ///
    /// This opens a short-lived file handle on the calling thread and never
    /// touches the SPSC rings, so the real-time read position is undisturbed.
    pub fn read_window(&self, start: usize, len: usize) -> io::Result<Vec<Vec<f32>>> {
        let channels = self.channels.max(1);
        let mut window = vec![vec![0.0_f32; len]; channels];
        if len == 0 {
            return Ok(window);
        }
        let available = self.total_frames.saturating_sub(start);
        let real = len.min(available);
        if real == 0 {
            return Ok(window);
        }
        let block_align = self.layout.block_align();
        let position = self
            .layout
            .data_offset
            .saturating_add(start as u64 * block_align as u64);
        let mut bytes = vec![0u8; real.saturating_mul(block_align)];
        let file = File::open(self.layout_path_hint())?;
        read_exact_at(&file, position, &mut bytes)?;
        let mut interleaved = Vec::new();
        self.layout
            .convert_interleaved(&bytes, real, &mut interleaved);
        for (channel, plane) in window.iter_mut().enumerate() {
            for (i, sample) in plane.iter_mut().enumerate().take(real) {
                *sample = interleaved[i * channels + channel];
            }
        }
        Ok(window)
    }

    /// Path kept only so [`Self::read_window`] can open its own handle; the
    /// producer thread holds the primary one.
    fn layout_path_hint(&self) -> PathBuf {
        self.path.clone()
    }
}

impl Drop for SeekableStreamingClipBuffer {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Release);
        if let Ok(mut guard) = self.producer.lock()
            && let Some(producer) = guard.take()
            && !producer.handle.is_finished()
        {
            let _ = producer.done.recv_timeout(Duration::from_millis(200));
        }
    }
}

fn producer_main(
    path: PathBuf,
    layout: WavPcmLayout,
    mut rings: Vec<Producer<f32>>,
    control: ProducerControl,
    done: std::sync::mpsc::Sender<()>,
) {
    let result = run_producer(&path, &layout, &mut rings, &control);
    if let Err(e) = result {
        tracing::warn!(
            "Seekable streaming producer for '{}' stopped with error: {e}",
            path.display()
        );
    }
    control.eof.store(true, Ordering::Release);
    super::streaming::release_producer_slot();
    let _ = done.send(());
}

fn run_producer(
    path: &Path,
    layout: &WavPcmLayout,
    rings: &mut [Producer<f32>],
    control: &ProducerControl,
) -> io::Result<()> {
    let file = File::open(path)?;
    let channels = layout.channels.max(1);
    let block_align = layout.block_align().max(1);
    let total_bytes = layout.data_len as usize;
    // Byte position inside the data chunk that the next read consumes.
    let mut position = 0u64;
    let mut pending: Option<(Vec<f32>, usize)> = None;

    loop {
        if control.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let seek = control.seek_request.load(Ordering::Acquire);
        if seek != 0 {
            control.seek_request.store(0, Ordering::Release);
            // O(1) seek: the next read starts at the requested frame.
            position = (seek as u64 - 1).saturating_mul(block_align as u64);
            pending = None;
        }

        if pending.is_none() {
            if position as usize >= total_bytes {
                return Ok(());
            }
            let want_bytes = DECODE_CHUNK_FRAMES.saturating_mul(block_align);
            let remaining = total_bytes.saturating_sub(position as usize);
            let read_bytes = want_bytes.min(remaining);
            let mut bytes = vec![0u8; read_bytes];
            read_exact_at(
                &file,
                layout.data_offset.saturating_add(position),
                &mut bytes,
            )?;
            position = position.saturating_add(read_bytes as u64);
            let frames = read_bytes / block_align;
            if frames == 0 {
                return Ok(());
            }
            let mut interleaved = Vec::new();
            layout.convert_interleaved(&bytes, frames, &mut interleaved);
            pending = Some((interleaved, frames));
        }

        let Some((data, frames)) = pending.take() else {
            continue;
        };
        let min_free = rings.iter().map(Producer::slots).min().unwrap_or(0);
        let push_frames = frames.min(min_free);
        if push_frames > 0 {
            deinterleave(&data, push_frames, channels, rings);
        }
        if push_frames != frames {
            let rest: Vec<f32> = data[push_frames * channels..].to_vec();
            pending = Some((rest, frames - push_frames));
            if control.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_codec::write_wav_f32;

    fn tone_wav(name: &str, sample_rate: u32, seconds: f64, freq: f32) -> (PathBuf, Vec<f32>) {
        let frames = (sample_rate as f64 * seconds) as usize;
        let mono: Vec<f32> = (0..frames)
            .map(|i| {
                0.5 * (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin()
            })
            .collect();
        let path = std::env::temp_dir().join(format!("{name}_{}.wav", std::process::id()));
        write_wav_f32(&path, &mono, 1, sample_rate).expect("write tone wav");
        (path, mono)
    }

    fn wait_for_frames(
        buffer: &SeekableStreamingClipBuffer,
        frames: usize,
        timeout: Duration,
    ) -> usize {
        let started = std::time::Instant::now();
        loop {
            let avail = buffer
                .rings
                .iter()
                .filter_map(|ring| ring.lock().ok().map(|consumer| consumer.slots()))
                .min()
                .unwrap_or(0);
            if avail >= frames || started.elapsed() > timeout {
                return avail;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn seekable_stream_exact_first_samples_and_audible_through_track() {
        use crate::track::Track;

        let (path, source) = tone_wav("maolan_seekstream_track", 48_000, 1.0, 440.0);
        let mut track = Track::new("t".to_string(), 2, 2, 0, 0, 256, 48_000.0);
        track.set_input_monitor(vec![false; 2]);
        track.set_disk_monitor(vec![true; 2]);
        let mut clip = crate::audio::clip::AudioClip::new(
            path.to_string_lossy().into_owned(),
            0,
            48_000 * 240,
        );
        clip.id = "clip-1".to_string();
        clip.fade_enabled = false;
        track.audio.push_clip(clip);

        let mut first_block: Option<Vec<f32>> = None;
        let mut peak = 0.0f32;
        for cycle in 0..188 {
            track.rt.transport_sample = cycle * 256;
            track.process();
            let out = track.last_audio_outputs();
            for lane in out {
                for &s in lane {
                    peak = peak.max(s.abs());
                }
            }
            if cycle == 1 && !out.is_empty() {
                first_block = Some(out[0].to_vec());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = std::fs::remove_file(&path);

        assert!(
            peak > 0.01,
            "seekable streaming clip must be audible through Track (peak {peak})"
        );
        let first_block = first_block.expect("second render block");
        // First audible frames must equal the source samples exactly
        // (float WAV round-trips bit-exactly through f32).
        for (i, (&actual, &expected)) in first_block.iter().zip(source.iter()).enumerate() {
            assert!(
                (actual - expected).abs() < 1.0e-6,
                "sample {i}: got {actual}, expected {expected}"
            );
        }
    }

    #[test]
    fn seekable_stream_mid_buffer_seek_is_exact() {
        let (path, source) = tone_wav("maolan_seekstream_seek", 48_000, 0.5, 440.0);
        let buffer = SeekableStreamingClipBuffer::start(&path, 48_000, 128, 4)
            .expect("start seekable buffer");
        assert!(wait_for_frames(&buffer, 512, Duration::from_secs(5)) >= 512);

        let mut out = vec![vec![0.0_f32; 256]; 1];
        // Sequential read first.
        buffer.read_frames(0, 256, &mut out);
        assert_eq!(out[0][0], source[0]);

        // Seek mid-file: drain the rings so the next pops observe only
        // post-seek producer data, then post the seek request.
        for ring in &buffer.rings {
            let mut consumer = ring.lock().expect("ring lock");
            while consumer.pop().is_ok() {}
        }
        let seek_target = 12_000usize;
        buffer.read_frames(seek_target, 256, &mut out);

        // The seek read under-runs (rings drained, producer still seeking);
        // the following sequential read receives exactly the frames the
        // producer pushed from `seek_target` onward.
        assert!(wait_for_frames(&buffer, 256, Duration::from_secs(5)) >= 256);
        buffer.read_frames(seek_target + 256, 256, &mut out);
        let _ = std::fs::remove_file(&path);

        for (i, (&actual, &expected)) in out[0]
            .iter()
            .zip(&source[seek_target..seek_target + 256])
            .enumerate()
        {
            assert!(
                (actual - expected).abs() < 1.0e-6,
                "post-seek sample {i}: got {actual}, expected {expected}"
            );
        }
    }

    #[test]
    fn seekable_stream_underrun_silence_fills_and_counts() {
        let (path, _source) = tone_wav("maolan_seekstream_underrun", 48_000, 0.2, 440.0);
        let buffer =
            SeekableStreamingClipBuffer::start(&path, 48_000, 8, 2).expect("start seekable buffer");
        buffer.control.stop.store(true, Ordering::Release);
        std::thread::sleep(Duration::from_millis(100));

        let mut out = vec![vec![7.0_f32; 512]; 1];
        let avail = buffer
            .rings
            .iter()
            .filter_map(|ring| ring.lock().ok().map(|consumer| consumer.slots()))
            .min()
            .unwrap_or(0);
        let read = buffer.read_frames(0, 512, &mut out);
        let _ = std::fs::remove_file(&path);

        assert!(read <= avail);
        assert_eq!(
            out[0][read..].iter().filter(|&&s| s == 0.0).count(),
            512 - read
        );
        assert!(
            buffer.underruns() > 0 || read < 512,
            "expected an underrun to be recorded"
        );
    }

    #[test]
    fn seekable_read_window_at_start_middle_and_eof() {
        let (path, source) = tone_wav("maolan_seekstream_window", 48_000, 0.2, 440.0);
        let buffer = SeekableStreamingClipBuffer::start(&path, 48_000, 64, 4)
            .expect("start seekable buffer");
        let total = source.len();

        let start = buffer.read_window(0, 256).expect("window at start");
        for (i, (&actual, &expected)) in start[0].iter().zip(&source[..256]).enumerate() {
            assert!(
                (actual - expected).abs() < 1.0e-6,
                "start window sample {i}: got {actual}, expected {expected}"
            );
        }

        let middle_at = total - 1_000;
        let middle = buffer
            .read_window(middle_at, 512)
            .expect("window at middle");
        for (i, (&actual, &expected)) in middle[0]
            .iter()
            .zip(&source[middle_at..middle_at + 512])
            .enumerate()
        {
            assert!(
                (actual - expected).abs() < 1.0e-6,
                "middle window sample {i}: got {actual}, expected {expected}"
            );
        }

        // Past EOF: real frames then silence padding.
        let eof_window = buffer.read_window(total - 128, 512).expect("window at eof");
        for (i, (&actual, &expected)) in eof_window[0][..128]
            .iter()
            .zip(&source[total - 128..])
            .enumerate()
        {
            assert!(
                (actual - expected).abs() < 1.0e-6,
                "eof window sample {i}: got {actual}, expected {expected}"
            );
        }
        assert!(
            eof_window[0][128..].iter().all(|&s| s == 0.0),
            "expected silence past EOF"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn seekable_rejects_foreign_rate_and_non_pcm() {
        let (path, _source) = tone_wav("maolan_seekstream_rate", 44_100, 0.1, 440.0);
        assert!(
            SeekableStreamingClipBuffer::start(&path, 48_000, 64, 4).is_err(),
            "foreign-rate wav must be rejected by the seekable buffer"
        );
        let _ = std::fs::remove_file(&path);
    }
}
