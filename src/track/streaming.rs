//! Streaming audio clip playback.
//!
//! A [`StreamingClipBuffer`] decodes an audio file incrementally on a
//! producer thread, resamples to the engine sample rate when needed, and
//! pushes per-channel planes into one SPSC `rtrb` ring per channel. The
//! audio-thread consumer ([`StreamingClipBuffer::read_frames`]) reads frames
//! sequentially out of the rings and silence-fills on underrun.
//!
//! Seek semantics (v1): the rings act as a pure delay line. The consumer
//! tracks the next expected source frame; when a read arrives whose start
//! frame is not the sequential continuation (a transport seek, loop wrap, or
//! clip re-launch), it posts the requested absolute source frame to the
//! producer. The producer restarts its incremental decoder from the
//! beginning of the file and decodes-and-discards until the requested frame
//! (symphonia seeking is not used), then resumes pushing at the next free
//! slot in the rings without clearing them. The audio already buffered in
//! the rings is therefore played once more before the new position lands —
//! that latency is accepted for v1. Sequential playback (the common player
//! case) never posts a seek and pays no seek cost.
//!
//! Lockstep invariant: the producer always writes the exact same frame count
//! to every channel ring, so `min(slots)` across rings is the readable frame
//! count for all channels at once.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use audioadapter_buffers::direct::InterleavedSlice;
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::{Async, FixedAsync, Indexing, PolynomialDegree, Resampler};

use crate::audio_codec::{StreamingDecoder, probe_audio_file};

/// Default ring capacity multiplier (× period frames per channel ring).
pub const DEFAULT_RING_BUFFER_MULTIPLIER: usize = 8;
/// Minimum/maximum accepted ring capacity multiplier.
const MIN_RING_BUFFER_MULTIPLIER: usize = 2;
const MAX_RING_BUFFER_MULTIPLIER: usize = 32;

/// Engine-wide ring capacity multiplier, set from
/// `Action::OpenAudioDevice::ring_buffer_multiplier` when the audio device
/// is opened. The streaming clip loader reads it when sizing new rings.
static RING_BUFFER_MULTIPLIER: AtomicUsize = AtomicUsize::new(DEFAULT_RING_BUFFER_MULTIPLIER);

/// Clamp a user-provided multiplier: 0 means "use the default" (8),
/// otherwise clamp into 2..=32.
pub fn clamp_ring_buffer_multiplier(multiplier: usize) -> usize {
    match multiplier {
        0 => DEFAULT_RING_BUFFER_MULTIPLIER,
        m => m.clamp(MIN_RING_BUFFER_MULTIPLIER, MAX_RING_BUFFER_MULTIPLIER),
    }
}

/// Store the engine-wide ring capacity multiplier (already clamped).
pub fn set_ring_buffer_multiplier(multiplier: usize) {
    RING_BUFFER_MULTIPLIER.store(clamp_ring_buffer_multiplier(multiplier), Ordering::Relaxed);
}

/// Current engine-wide ring capacity multiplier.
pub fn ring_buffer_multiplier() -> usize {
    RING_BUFFER_MULTIPLIER.load(Ordering::Relaxed)
}

/// Frames decoded per producer iteration.
pub(crate) const DECODE_CHUNK_FRAMES: usize = 4096;

/// Hard cap on live streaming producer threads across the engine. Sessions
/// with very many clips would otherwise spawn one thread (and one set of
/// rings) per clip at preload time; beyond this cap new streaming-class
/// clips fall back to a whole-file buffered decode instead.
pub(crate) const MAX_STREAMING_PRODUCERS: usize = 128;

/// Number of currently live streaming producer threads.
static LIVE_PRODUCERS: AtomicUsize = AtomicUsize::new(0);

/// Try to reserve one streaming producer slot. Returns `false` when the cap
/// is reached; callers must then fall back to a buffered decode.
pub(crate) fn try_acquire_producer_slot() -> bool {
    let mut live = LIVE_PRODUCERS.load(Ordering::Relaxed);
    loop {
        if live >= MAX_STREAMING_PRODUCERS {
            return false;
        }
        match LIVE_PRODUCERS.compare_exchange_weak(
            live,
            live + 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(current) => live = current,
        }
    }
}

/// Return a streaming producer slot claimed with [`try_acquire_producer_slot`].
pub(crate) fn release_producer_slot() {
    LIVE_PRODUCERS.fetch_sub(1, Ordering::Relaxed);
}

/// Shared producer control state.
pub(crate) struct ProducerControl {
    pub(crate) stop: Arc<AtomicBool>,
    /// 0 = no seek pending, otherwise `requested_source_frame + 1`.
    pub(crate) seek_request: Arc<AtomicUsize>,
    pub(crate) eof: Arc<AtomicBool>,
}

pub struct StreamingClipBuffer {
    pub channels: usize,
    pub engine_sample_rate: usize,
    /// Total source frames reported by the container probe, if known.
    total_frames: Option<u64>,
    /// Source file path; used by `read_window` to open an independent
    /// decoder without disturbing the real-time rings.
    path: PathBuf,
    rings: Vec<Mutex<Consumer<f32>>>,
    eof: Arc<AtomicBool>,
    underruns: AtomicUsize,
    control: ProducerControl,
    /// Next source frame the consumer expects to read; `u64::MAX` until the
    /// first read. Used to detect non-sequential (seek) reads.
    expected_next: AtomicU64,
    producer: Mutex<Option<ProducerThread>>,
}

impl std::fmt::Debug for StreamingClipBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingClipBuffer")
            .field("channels", &self.channels)
            .field("engine_sample_rate", &self.engine_sample_rate)
            .field("eof", &self.is_eof())
            .field("underruns", &self.underruns())
            .finish()
    }
}

struct ProducerThread {
    handle: JoinHandle<()>,
    done: std::sync::mpsc::Receiver<()>,
}

impl StreamingClipBuffer {
    /// Start a streaming producer for `path`, resampled to `engine_rate`.
    /// Each channel ring holds `multiplier * period_frames` floats.
    pub fn start(
        path: &Path,
        engine_rate: usize,
        period_frames: usize,
        multiplier: usize,
    ) -> io::Result<Self> {
        let info = probe_audio_file(path)?;
        let channels = info.channels.max(1);
        // Guard against unbounded producer threads in sessions with very
        // many clips (see `preload_audio_clip_cache`); beyond the cap the
        // caller falls back to a whole-file buffered decode.
        if !try_acquire_producer_slot() {
            return Err(io::Error::other(format!(
                "Streaming producer cap reached; not starting stream for '{}'",
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
        let source_rate = info.sample_rate as usize;
        let handle = std::thread::Builder::new()
            .name(format!("maolan-stream-{}", path.display()))
            .spawn(move || {
                producer_main(
                    path_buf,
                    engine_rate,
                    source_rate,
                    channels,
                    producers,
                    thread_control,
                    done_tx,
                );
            })
            .map_err(|e| {
                release_producer_slot();
                io::Error::other(format!("Failed to spawn streaming producer: {e}"))
            })?;
        Ok(Self {
            channels,
            engine_sample_rate: engine_rate,
            total_frames: info.frames,
            path: path.to_path_buf(),
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

    /// Total decoded frames in the source file, if the container probe
    /// reported a duration.
    pub fn total_frames(&self) -> Option<u64> {
        self.total_frames
    }

    /// Random access read for offline features (pitch correction, reversed
    /// playback): returns exactly `len` frames per channel starting at
    /// source frame `start`, silence-padded past EOF.
    ///
    /// This runs on the calling thread with an independent incremental
    /// decoder (decode-and-discard up to `start`, then read `len` frames)
    /// and never touches the SPSC rings, so the real-time read position and
    /// the producer are undisturbed.
    pub fn read_window(&self, start: usize, len: usize) -> io::Result<Vec<Vec<f32>>> {
        let channels = self.channels.max(1);
        let mut window = vec![vec![0.0_f32; len]; channels];
        if len == 0 {
            return Ok(window);
        }
        let mut decoder = StreamingDecoder::new(&self.path)?;
        let mut position = 0usize;
        let mut filled = 0usize;
        while filled < len {
            let Some(chunk) = decoder.next_chunk(DECODE_CHUNK_FRAMES)? else {
                break;
            };
            let frames = chunk.len() / channels;
            if position + frames <= start {
                // Whole chunk lands before the window; discard it.
                position += frames;
                continue;
            }
            let chunk_from = start.saturating_sub(position);
            let take = (frames - chunk_from).min(len - filled);
            for (channel, plane) in window.iter_mut().enumerate() {
                for i in 0..take {
                    plane[filled + i] = chunk[(chunk_from + i) * channels + channel];
                }
            }
            filled += take;
            position += frames;
        }
        Ok(window)
    }

    /// Read up to `len` frames starting at source frame `from_frame` into
    /// `out` (one `Vec<f32>` per channel, each at least `len` long).
    ///
    /// The range is compared against the sequential continuation; a mismatch
    /// posts a seek request to the producer (see module docs). Frames that
    /// are not available (producer behind, or end of file) are silence-filled
    /// and counted as underruns.
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
                    "Streaming clip underrun #{underruns} (read {frames}/{len} frames, eof={})",
                    self.is_eof()
                );
            }
        }
        frames
    }
}

impl Drop for StreamingClipBuffer {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Release);
        // Wait briefly for the producer to observe the stop flag and exit,
        // but never hang shutdown if it is blocked inside a decode call.
        if let Ok(mut guard) = self.producer.lock()
            && let Some(producer) = guard.take()
            && !producer.handle.is_finished()
        {
            let _ = producer.done.recv_timeout(Duration::from_millis(200));
        }
        // If the producer is still alive here, `handle` is dropped and
        // the thread is detached; it observes the stop flag as soon as
        // the current decode call returns and exits on its own.
    }
}

fn make_resampler(
    engine_rate: usize,
    source_rate: usize,
    channels: usize,
) -> io::Result<Async<f32>> {
    let ratio = engine_rate as f64 / source_rate.max(1) as f64;
    Async::<f32>::new_poly(
        ratio,
        1.5,
        PolynomialDegree::Cubic,
        DECODE_CHUNK_FRAMES,
        channels,
        FixedAsync::Input,
    )
    .map_err(|e| io::Error::other(format!("Failed to create resampler: {e}")))
}

fn producer_main(
    path: PathBuf,
    engine_rate: usize,
    source_rate: usize,
    channels: usize,
    mut rings: Vec<Producer<f32>>,
    control: ProducerControl,
    done: std::sync::mpsc::Sender<()>,
) {
    let result = run_producer(
        &path,
        engine_rate,
        source_rate,
        channels,
        &mut rings,
        &control,
    );
    if let Err(e) = result {
        tracing::warn!(
            "Streaming producer for '{}' stopped with error: {e}",
            path.display()
        );
    }
    control.eof.store(true, Ordering::Release);
    release_producer_slot();
    let _ = done.send(());
}

fn run_producer(
    path: &Path,
    engine_rate: usize,
    source_rate: usize,
    channels: usize,
    rings: &mut [Producer<f32>],
    control: &ProducerControl,
) -> io::Result<()> {
    let mut decoder = StreamingDecoder::new(path)?;
    let needs_resample = source_rate != engine_rate && source_rate > 0;
    let mut resampler = if needs_resample {
        Some(make_resampler(engine_rate, source_rate, channels)?)
    } else {
        None
    };
    let mut out_capacity = resampler
        .as_ref()
        .map(|r| r.output_frames_max())
        .unwrap_or(0);
    let mut out_buffer = vec![0.0_f32; out_capacity.saturating_mul(channels)];
    let mut resampler_input = vec![0.0_f32; DECODE_CHUNK_FRAMES.saturating_mul(channels)];
    // Interleaved chunk waiting for enough free ring space.
    let mut pending: Option<(Vec<f32>, usize)> = None;
    // Frames already discarded while satisfying the current seek request.
    let mut discard_remaining = 0usize;
    let mut at_eof = false;

    loop {
        if control.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let seek = control.seek_request.load(Ordering::Acquire);
        if seek != 0 {
            control.seek_request.store(0, Ordering::Release);
            let target_engine_frame = seek - 1;
            // Convert the engine-rate target to a source-rate frame count;
            // the resampler startup delay is accepted latency (see module docs).
            discard_remaining = if needs_resample {
                target_engine_frame.saturating_mul(source_rate) / engine_rate.max(1)
            } else {
                target_engine_frame
            };
            pending = None;
            decoder.reset()?;
            if needs_resample {
                resampler = Some(make_resampler(engine_rate, source_rate, channels)?);
                out_capacity = resampler
                    .as_ref()
                    .map(|r| r.output_frames_max())
                    .unwrap_or(0);
                out_buffer = vec![0.0_f32; out_capacity.saturating_mul(channels)];
            }
            at_eof = false;
            control.eof.store(false, Ordering::Release);
        }
        if at_eof {
            // Keep the producer alive so a loop can seek this clip again.
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }

        if pending.is_none() {
            let Some(chunk) = decoder.next_chunk(DECODE_CHUNK_FRAMES)? else {
                // Flush the resampler delay tail as a final partial chunk.
                if resampler.is_some() {
                    let silence = vec![0.0_f32; DECODE_CHUNK_FRAMES.saturating_mul(channels)];
                    push_chunk(
                        silence,
                        DECODE_CHUNK_FRAMES,
                        channels,
                        &mut resampler,
                        &mut resampler_input,
                        &mut out_buffer,
                        &mut pending,
                    );
                    if pending.is_some() {
                        continue;
                    }
                }
                at_eof = true;
                control.eof.store(true, Ordering::Release);
                continue;
            };
            let frames = chunk.len() / channels;
            if discard_remaining > 0 {
                let skip = frames.min(discard_remaining);
                discard_remaining -= skip;
                if skip == frames {
                    continue;
                }
                let kept = frames - skip;
                let mut kept_chunk = Vec::with_capacity(kept * channels);
                for frame in skip..frames {
                    kept_chunk.extend_from_slice(&chunk[frame * channels..(frame + 1) * channels]);
                }
                push_chunk(
                    kept_chunk,
                    kept,
                    channels,
                    &mut resampler,
                    &mut resampler_input,
                    &mut out_buffer,
                    &mut pending,
                );
            } else {
                push_chunk(
                    chunk,
                    frames,
                    channels,
                    &mut resampler,
                    &mut resampler_input,
                    &mut out_buffer,
                    &mut pending,
                );
            }
        }

        let Some((data, frames)) = pending.take() else {
            continue;
        };
        let min_free = rings.iter().map(Producer::slots).min().unwrap_or(0);
        let push_frames = frames.min(min_free);
        if push_frames > 0 {
            for (channel, ring) in rings.iter_mut().enumerate() {
                for frame in 0..push_frames {
                    // Lockstep: the same frame count is pushed to every ring.
                    let _ = ring.push(data[frame * channels + channel]);
                }
            }
        }
        if push_frames == frames {
            // Chunk fully pushed; decode the next one on the next iteration.
        } else {
            // Keep the unpushed tail; the consumer drains the rings while we
            // wait, so sleep briefly before retrying. Decoded data is held in
            // `pending` so it is never re-decoded or lost.
            let rest: Vec<f32> = data[push_frames * channels..].to_vec();
            pending = Some((rest, frames - push_frames));
            if control.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_chunk(
    chunk: Vec<f32>,
    frames: usize,
    channels: usize,
    resampler: &mut Option<Async<f32>>,
    resampler_input: &mut [f32],
    out_buffer: &mut Vec<f32>,
    pending: &mut Option<(Vec<f32>, usize)>,
) {
    if let Some(resampler) = resampler {
        let full_input = frames >= resampler.input_frames_next();
        resampler_input[..chunk.len()].copy_from_slice(&chunk);
        let input = match InterleavedSlice::new(resampler_input, channels, frames) {
            Ok(input) => input,
            Err(e) => {
                tracing::warn!("Streaming resampler input adapter failed: {e}");
                return;
            }
        };
        let capacity = out_buffer.len() / channels.max(1);
        let mut output =
            match InterleavedSlice::new_mut(out_buffer.as_mut_slice(), channels, capacity) {
                Ok(output) => output,
                Err(e) => {
                    tracing::warn!("Streaming resampler output adapter failed: {e}");
                    return;
                }
            };
        // A short final chunk (or the silent EOF flush) is signalled via
        // `partial_len`; the resampler treats the rest of the input as
        // silence and still produces a full output chunk.
        let indexing = if full_input {
            None
        } else {
            Some(Indexing::new().partial_len(frames))
        };
        match resampler.process_into_buffer(&input, &mut output, indexing.as_ref()) {
            Ok((_, written)) => {
                *pending = Some((out_buffer[..written * channels].to_vec(), written));
            }
            Err(e) => tracing::warn!("Streaming resample failed: {e}"),
        }
    } else {
        *pending = Some((chunk, frames));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_codec::{AudioDither, AudioEncodeFormat, encode_audio_to_file, write_wav_f32};

    fn tone_flac(name: &str, sample_rate: u32, seconds: f64, freq: f32) -> PathBuf {
        let frames = (sample_rate as f64 * seconds) as usize;
        let mono: Vec<f32> = (0..frames)
            .map(|i| {
                0.5 * (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin()
            })
            .collect();
        let wav_path = std::env::temp_dir().join(format!("{name}_{}.wav", std::process::id()));
        write_wav_f32(&wav_path, &mono, 1, sample_rate).expect("write tone wav");
        let (decoded, channels, _) =
            crate::audio_codec::decode_audio_to_f32_interleaved_sync(&wav_path)
                .expect("decode tone wav");
        assert_eq!(channels, 1);
        let _ = std::fs::remove_file(&wav_path);
        let flac_path = std::env::temp_dir().join(format!("{name}_{}.flac", std::process::id()));
        encode_audio_to_file(
            &flac_path,
            &decoded,
            1,
            sample_rate,
            AudioEncodeFormat::Flac(16),
            AudioDither::None,
        )
        .expect("encode tone flac");
        flac_path
    }

    fn wait_for_frames(buffer: &StreamingClipBuffer, frames: usize, timeout: Duration) -> usize {
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

    /// Render a streaming clip through a real `Track` (the same code path
    /// the engine runtime uses) and return the peak of the track outputs
    /// over the first `cycles` render blocks, paced so the producer thread
    /// can keep the rings fed.
    fn render_track_peak(path: &Path, cycles: usize) -> f32 {
        use crate::audio::clip::AudioClip;
        use crate::track::Track;

        let mut track = Track::new("t".to_string(), 2, 2, 0, 0, 256, 48_000.0);
        track.set_input_monitor(vec![false; 2]);
        track.set_disk_monitor(vec![true; 2]);
        let mut clip = AudioClip::new(path.to_string_lossy().into_owned(), 0, 48_000 * 240);
        clip.id = "clip-1".to_string();
        clip.fade_enabled = false;
        track.audio.push_clip(clip);
        let mut peak = 0.0f32;
        for cycle in 0..cycles {
            track.rt.transport_sample = cycle * 256;
            track.process();
            for out in track.last_audio_outputs() {
                for &s in out {
                    peak = peak.max(s.abs());
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        peak
    }

    fn stereo_tone_flac(name: &str, sample_rate: u32, seconds: f64, freq: f32) -> PathBuf {
        let frames = (sample_rate as f64 * seconds) as usize;
        let stereo: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let s =
                    0.5 * (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin();
                [s, s]
            })
            .collect();
        let flac_path = std::env::temp_dir().join(format!("{name}_{}.flac", std::process::id()));
        encode_audio_to_file(
            &flac_path,
            &stereo,
            2,
            sample_rate,
            AudioEncodeFormat::Flac(16),
            AudioDither::None,
        )
        .expect("encode stereo tone flac");
        flac_path
    }

    #[test]
    fn streaming_clip_audible_through_track_render() {
        // Regression test: a 44.1 kHz stereo FLAC (foreign rate → Streaming
        // strategy) must produce audible-level track output. The original bug
        // report was complete silence for exactly this class of file.
        let path = stereo_tone_flac("maolan_stream_track_render", 44_100, 1.0, 440.0);
        let peak = render_track_peak(&path, 188); // ~1s of engine-rate frames
        let _ = std::fs::remove_file(&path);
        assert!(
            peak > 0.01,
            "streaming clip rendered through Track must be audible (peak {peak})"
        );
    }

    #[test]
    fn streaming_real_flac_file_audible() {
        let path = PathBuf::from(
            "/home/meka/Files/Music/Guitar/Audioslave - Discography (2002-2006) [FLAC]/[2002] Audioslave/01. Cochise.flac",
        );
        if !path.exists() {
            eprintln!("SKIP: real flac not present");
            return;
        }
        let peak = render_track_peak(&path, 188);
        assert!(
            peak > 0.01,
            "real-world streaming flac must be audible (peak {peak})"
        );
    }

    #[test]
    fn ring_buffer_multiplier_clamps() {
        assert_eq!(clamp_ring_buffer_multiplier(0), 8);
        assert_eq!(clamp_ring_buffer_multiplier(1), 2);
        assert_eq!(clamp_ring_buffer_multiplier(2), 2);
        assert_eq!(clamp_ring_buffer_multiplier(8), 8);
        assert_eq!(clamp_ring_buffer_multiplier(32), 32);
        assert_eq!(clamp_ring_buffer_multiplier(64), 32);
    }

    #[test]
    fn streaming_flac_resamples_and_preserves_tone() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let source_rate = 44_100u32;
        let engine_rate = 48_000usize;
        let path = tone_flac("maolan_stream_tone", source_rate, 1.0, 440.0);
        let buffer =
            StreamingClipBuffer::start(&path, engine_rate, 128, 4).expect("start streaming buffer");
        assert_eq!(buffer.channels(), 1);

        let mut collected = Vec::new();
        let mut out = vec![vec![0.0_f32; 1024]; 1];
        let mut from = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while collected.len() < engine_rate && std::time::Instant::now() < deadline {
            let avail = wait_for_frames(&buffer, 1, Duration::from_millis(200));
            if avail == 0 {
                if buffer.is_eof() {
                    break;
                }
                continue;
            }
            let n = avail.min(out[0].len());
            buffer.read_frames(from, n, &mut out);
            collected.extend_from_slice(&out[0][..n]);
            from += n;
        }
        let _ = std::fs::remove_file(&path);

        let expected = engine_rate;
        assert!(
            collected.len() >= expected - 50,
            "collected {} frames, expected ~{expected}",
            collected.len()
        );
        assert!(
            collected.iter().all(|s| s.is_finite()),
            "non-finite samples in stream"
        );
        let peak = collected.iter().fold(0.0_f32, |a, &b| a.max(b.abs()));
        assert!(peak > 0.2, "sine peak did not survive (peak {peak})");

        // Compare against a buffered + resampled reference: windowed RMS of
        // the collected stream must track the reference envelope (the tone is
        // constant-amplitude, so every window should carry similar energy).
        let window = 4_800usize;
        let rms: Vec<f32> = collected
            .chunks(window)
            .map(|chunk| (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32).sqrt())
            .collect();
        let mean_rms = rms.iter().sum::<f32>() / rms.len() as f32;
        assert!(mean_rms > 0.2, "mean window RMS too low: {mean_rms}");
        let min_rms = rms.iter().fold(f32::INFINITY, |a, &b| a.min(b));
        // Allow fade-in/out edges and resampler tails to dip, but the body of
        // the clip must stay close to the constant reference envelope.
        let body = &rms[rms.len().min(2)..rms.len().saturating_sub(2)];
        let body_min = body.iter().fold(f32::INFINITY, |a, &b| a.min(b));
        assert!(
            body_min > 0.6 * mean_rms,
            "window RMS envelope diverged: body_min={body_min} mean={mean_rms} min={min_rms}"
        );
    }

    #[test]
    fn streaming_underrun_silence_fills_and_counts() {
        let path = tone_flac("maolan_stream_underrun", 48_000, 0.2, 440.0);
        let buffer =
            StreamingClipBuffer::start(&path, 48_000, 8, 2).expect("start streaming buffer");
        buffer.control.stop.store(true, Ordering::Release);
        // Wait for the producer to wind down so the rings stop filling.
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
    fn streaming_read_window_at_middle_and_eof() {
        let path = tone_flac("maolan_stream_window", 48_000, 0.2, 440.0);
        let info = probe_audio_file(&path).expect("probe");
        let total = info.frames.expect("flac probe frames") as usize;
        let (_decoded, channels, _) =
            crate::audio_codec::decode_audio_to_f32_interleaved_sync(&path).expect("decode");
        let buffer =
            StreamingClipBuffer::start(&path, 48_000, 64, 4).expect("start streaming buffer");

        let middle_at = total / 2;
        let middle = buffer.read_window(middle_at, 512).expect("middle window");
        assert_eq!(middle.len(), channels);
        assert!(
            middle.iter().flatten().all(|s| s.is_finite()),
            "non-finite samples in window"
        );
        let peak = middle.iter().flatten().fold(0.0f32, |a, &b| a.max(b.abs()));
        assert!(
            peak > 0.1,
            "middle window must contain signal (peak {peak})"
        );

        let eof_window = buffer
            .read_window(total.saturating_sub(128), 512)
            .expect("eof window");
        assert!(
            eof_window
                .iter()
                .flatten()
                .skip(128 * channels)
                .all(|&s| s == 0.0),
            "expected silence past EOF"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn streaming_seek_restarts_decode() {
        let path = tone_flac("maolan_stream_seek", 48_000, 0.5, 440.0);
        let buffer =
            StreamingClipBuffer::start(&path, 48_000, 64, 4).expect("start streaming buffer");

        let mut out = vec![vec![0.0_f32; 256]; 1];
        buffer.read_frames(0, 256, &mut out);
        assert!(wait_for_frames(&buffer, 1, Duration::from_secs(5)) > 0);

        // A non-sequential read posts a seek; the producer restarts decoding
        // from the file start and discards up to the target frame, after
        // which the rings fill again.
        let seek_target = 4_000usize;
        buffer.read_frames(seek_target, 256, &mut out);
        assert!(
            wait_for_frames(&buffer, 1, Duration::from_secs(10)) > 0,
            "producer did not resume after seek"
        );
        let _ = std::fs::remove_file(&path);
    }
}
