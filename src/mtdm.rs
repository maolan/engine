//! Multi-tone delay measurement (MTDM), the measurement algorithm behind
//! Fons Adriaensen's `jack_delay` / `jack_iodelay`: a continuous multitone
//! signal is played through the path under test, the returned signal is
//! demodulated against each tone's sine and cosine, and the per-tone phase
//! offsets resolve into a single delay.
//!
//! The latency is content-inferred: it comes purely from the phase of the
//! returned signal, never from driver position counters. Demodulation phases
//! are pure functions of the absolute sample index, so skipped or corrupted
//! regions only cost convergence time and can never corrupt the phase
//! relationship.
//!
//! Resolution scheme: the strong reference tone gives the fractional delay;
//! each of the twelve quiet helper tones contributes one binary digit of the
//! integer delay, giving an unambiguous range of `16 * 2^12 = 65536` frames
//! (~1.36 s at 48 kHz). A two-stage low-pass (200 Hz) keeps the estimate
//! converging under noise.

use std::f32::consts::PI as PI_F32;
use std::f64::consts::PI as PI_F64;

pub const NUM_TONES: usize = 13;
/// Low-pass update cadence, in samples. The resolved delay is expressed in
/// multiples of this many frames.
pub const DEMOD_INTERVAL: usize = 16;
/// Unambiguous delay range in frames.
pub const MAX_DELAY_FRAMES: f64 = DEMOD_INTERVAL as f64 * 4096.0;

/// Per-tone phase increments in 1/65536ths of a turn per sample
/// (`jack_delay`'s table). The nominal Hz values assume a 65536 Hz rate; at
/// 48 kHz the reference tone lands on exactly 3000 Hz. Only the increment
/// ratios matter to the resolver, not the absolute frequencies.
pub const TONE_STEP: [u32; NUM_TONES] = [
    4096, 2048, 3072, 2560, 2304, 2176, 1088, 1312, 1552, 1800, 3332, 3586, 3841,
];

/// Amplitude of the reference tone in the synthesized signal.
pub const REFERENCE_AMPLITUDE: f32 = 0.20;
/// Amplitude of each helper tone in the synthesized signal.
pub const HELPER_AMPLITUDE: f32 = 0.01;

/// Why [`Mtdm::resolve`] could not produce a delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    /// The reference tone's energy is below threshold: no signal (yet), a
    /// disconnected loopback, or a badly attenuated path.
    BelowThreshold,
    /// One of the helper tones is inconsistent with the reference: the
    /// integrated window straddles a delay change or corruption. Collect
    /// more (clean) data and resolve again.
    Inconsistent,
}

/// Status of a resolved MTDM measurement report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoDelayStatus {
    /// The reference tone's energy is below threshold: no looped signal.
    BelowThreshold,
    /// Signal present but the helper tones disagree (garbage window); keep
    /// collecting.
    Collecting,
    /// A delay was resolved.
    Resolved,
}

/// One MTDM measurement report, consumed by the standalone `maolan-iodelay`
/// binary and the in-engine `iodelay` session component alike.
#[derive(Debug, Clone, Copy)]
pub struct IoDelayReport {
    pub status: IoDelayStatus,
    pub delay_frames: f64,
    pub error: f64,
    pub inverted: bool,
    /// True on the last report of a run.
    pub final_report: bool,
}

/// Resolve the demodulator state into a report, mirroring `jack_iodelay`'s
/// invert-on-marginal handling. Shared by the standalone runner and the
/// in-engine measurement node.
pub fn make_report(mtdm: &mut Mtdm, final_report: bool) -> IoDelayReport {
    let mut status = match mtdm.resolve() {
        Ok(_) if mtdm.error() <= 0.3 => IoDelayStatus::Resolved,
        Err(ResolveError::BelowThreshold) => IoDelayStatus::BelowThreshold,
        _ => {
            mtdm.toggle_invert();
            match mtdm.resolve() {
                Ok(_) if mtdm.error() <= 0.3 => IoDelayStatus::Resolved,
                Err(ResolveError::BelowThreshold) => IoDelayStatus::BelowThreshold,
                _ => IoDelayStatus::Collecting,
            }
        }
    };
    if status == IoDelayStatus::Resolved && mtdm.delay_frames() >= MAX_DELAY_FRAMES * 0.9 {
        status = IoDelayStatus::Collecting;
    }
    IoDelayReport {
        status,
        delay_frames: if status == IoDelayStatus::Resolved {
            mtdm.delay_frames()
        } else {
            0.0
        },
        error: mtdm.error(),
        inverted: mtdm.inverted(),
        final_report,
    }
}

#[derive(Debug, Clone, Default)]
struct Tone {
    /// Quadrature accumulators.
    xa: f32,
    ya: f32,
    /// Two-stage low-pass state.
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

/// MTDM demodulator. Feed captured (looped-back) samples through
/// [`Mtdm::process`], then call [`Mtdm::resolve`].
#[derive(Debug, Clone)]
pub struct Mtdm {
    rate: usize,
    wlp: f32,
    cnt: usize,
    inv: bool,
    del: f64,
    err: f64,
    /// Absolute index the next processed sample will occupy in its stream.
    next_frame: u64,
    tones: [Tone; NUM_TONES],
}

/// Tone phase at absolute stream `frame`, in 1/65536ths of a turn. Both the
/// synthesizer and the demodulator call this with their own stream's index,
/// keeping them locked without shared mutable state.
pub fn tone_phase(step: u32, frame: u64) -> u32 {
    (frame.wrapping_mul(step as u64)) as u32
}

/// Multitone sample at absolute stream `frame`, ready to write into a
/// playback stream or file.
pub fn synthesize_frame(frame: u64) -> f32 {
    let mut vop = 0.0f32;
    for (i, &step) in TONE_STEP.iter().enumerate() {
        let a = 2.0 * PI_F32 * ((tone_phase(step, frame) & 65535) as f32) / 65536.0;
        vop += if i == 0 {
            REFERENCE_AMPLITUDE
        } else {
            HELPER_AMPLITUDE
        } * -a.sin();
    }
    vop
}

/// A `frames`-long multitone signal starting at stream index 0.
pub fn synthesize(frames: usize) -> Vec<f32> {
    (0..frames as u64).map(synthesize_frame).collect()
}

impl Mtdm {
    pub fn new(rate: usize) -> Self {
        Self {
            rate,
            wlp: 200.0 / rate.max(1) as f32,
            cnt: 0,
            inv: false,
            del: 0.0,
            err: 0.0,
            next_frame: 0,
            tones: std::array::from_fn(|_| Tone::default()),
        }
    }

    /// Sample rate of the stream being demodulated.
    pub fn rate(&self) -> usize {
        self.rate
    }

    /// Absolute stream index the next processed sample will occupy.
    pub fn stream_position(&self) -> u64 {
        self.next_frame
    }

    /// Set the absolute phase origin before processing begins. Used when a
    /// measurement node is added while the shared generator is already live.
    pub fn set_stream_position(&mut self, frame: u64) {
        self.next_frame = frame;
    }

    /// Last resolved delay in frames (valid after [`Mtdm::resolve`] returned
    /// `Ok`).
    pub fn delay_frames(&self) -> f64 {
        self.del
    }

    /// Worst helper-tone residual of the last resolve, in turns (0..0.5).
    /// Values above ~0.2 indicate a marginal measurement.
    pub fn error(&self) -> f64 {
        self.err
    }

    /// Whether the polarity-inversion flag is set (the path inverts the
    /// signal).
    pub fn inverted(&self) -> bool {
        self.inv
    }

    /// Flip the polarity-inversion flag; resolve again afterwards.
    pub fn toggle_invert(&mut self) {
        self.inv = !self.inv;
    }

    /// Drop partially integrated data, e.g. after a scan that may have
    /// raced the feeder. The index-derived phase references survive this;
    /// only the averaged energy is discarded.
    pub fn reset_filters(&mut self) {
        self.cnt = 0;
        for tone in &mut self.tones {
            tone.xa = 0.0;
            tone.ya = 0.0;
            tone.x1 = 0.0;
            tone.y1 = 0.0;
            tone.x2 = 0.0;
            tone.y2 = 0.0;
        }
    }

    /// Demodulate a chunk of captured samples. The samples' absolute stream
    /// positions are `next_frame .. next_frame + len` (tracked internally,
    /// starting at 0); chunk boundaries do not affect the result.
    pub fn process(&mut self, samples: &[f32]) {
        for (offset, &vip) in samples.iter().enumerate() {
            let frame = self.next_frame + offset as u64;
            for (i, &step) in TONE_STEP.iter().enumerate() {
                let a = 2.0 * PI_F32 * ((tone_phase(step, frame) & 65535) as f32) / 65536.0;
                let c = a.cos();
                let s = -a.sin();
                let tone = &mut self.tones[i];
                tone.xa += s * vip;
                tone.ya += c * vip;
            }
            self.cnt += 1;
            if self.cnt == DEMOD_INTERVAL {
                for tone in &mut self.tones {
                    tone.x1 += self.wlp * (tone.xa - tone.x1 + 1e-20);
                    tone.y1 += self.wlp * (tone.ya - tone.y1 + 1e-20);
                    tone.x2 += self.wlp * (tone.x1 - tone.x2 + 1e-20);
                    tone.y2 += self.wlp * (tone.y1 - tone.y2 + 1e-20);
                    tone.xa = 0.0;
                    tone.ya = 0.0;
                }
                self.cnt = 0;
            }
        }
        self.next_frame += samples.len() as u64;
    }

    /// Resolve the per-tone phases into a delay in frames, `jack_delay`'s
    /// `mtdm_resolve()`: the reference tone gives the fractional delay, each
    /// helper tone one bit of the integer delay.
    pub fn resolve(&mut self) -> Result<f64, ResolveError> {
        let t0 = &self.tones[0];
        if (t0.x2 as f64).hypot(t0.y2 as f64) < 0.001 {
            return Err(ResolveError::BelowThreshold);
        }
        let mut d = (t0.y2 as f64).atan2(t0.x2 as f64) / (2.0 * PI_F64);
        if self.inv {
            d += 0.5;
        }
        if d > 0.5 {
            d -= 1.0;
        }
        let f0 = TONE_STEP[0] as f64;
        let mut m: u32 = 1;
        self.err = 0.0;
        for (i, tone) in self.tones.iter().enumerate().skip(1) {
            let mut p = (tone.y2 as f64).atan2(tone.x2 as f64) / (2.0 * PI_F64)
                - d * TONE_STEP[i] as f64 / f0;
            if self.inv {
                p += 0.5;
            }
            p -= p.floor();
            p *= 2.0;
            let k = (p + 0.5).floor() as i64;
            let e = (p - k as f64).abs();
            if e > self.err {
                self.err = e;
            }
            if e > 0.4 {
                return Err(ResolveError::Inconsistent);
            }
            d += (m * (k as u32 & 1)) as f64;
            m *= 2;
        }
        self.del = DEMOD_INTERVAL as f64 * d;
        Ok(self.del)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: usize = 48_000;

    /// Synthesize `frames` of multitone, delayed by `delay` frames, as the
    /// looped-back capture would look in the take stream.
    fn delayed_signal(frames: usize, delay: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; frames];
        for (i, sample) in out[delay..].iter_mut().enumerate() {
            *sample = synthesize_frame(i as u64);
        }
        out
    }

    fn measure(delay: usize, frames: usize) -> Mtdm {
        let mut mtdm = Mtdm::new(RATE);
        mtdm.process(&delayed_signal(frames, delay));
        mtdm
    }

    #[test]
    fn recovers_known_delay_exactly() {
        for &delay in &[16usize, 128, 512, 1000, 1234, 4096] {
            let mut mtdm = measure(delay, RATE);
            let resolved = mtdm.resolve().unwrap_or_else(|e| {
                panic!("delay {delay} must resolve, got {e:?}");
            });
            assert!(
                (resolved - delay as f64).abs() < 0.1,
                "delay {delay}: resolved {resolved}"
            );
            assert!(mtdm.error() < 0.2, "delay {delay}: err {}", mtdm.error());
        }
    }

    #[test]
    fn chunk_boundaries_do_not_change_the_result() {
        let signal = delayed_signal(RATE, 1000);
        let mut chunked = Mtdm::new(RATE);
        for chunk in signal.chunks(997) {
            chunked.process(chunk);
        }
        let mut whole = Mtdm::new(RATE);
        whole.process(&signal);
        assert_eq!(chunked.resolve(), whole.resolve());
    }

    #[test]
    fn inverted_loopback_recovers_after_toggle() {
        let mut signal = delayed_signal(RATE, 1000);
        for sample in &mut signal {
            *sample = -*sample;
        }
        let mut mtdm = Mtdm::new(RATE);
        mtdm.process(&signal);
        let first = mtdm.resolve();
        if first.is_err() || mtdm.error() > 0.3 {
            mtdm.toggle_invert();
            let resolved = mtdm
                .resolve()
                .unwrap_or_else(|e| panic!("inverted resolve failed: {e:?}"));
            assert!((resolved - 1000.0).abs() < 0.1, "resolved {resolved}");
            assert!(mtdm.inverted());
        } else {
            panic!("inverted signal should not resolve cleanly: {first:?}");
        }
    }

    #[test]
    fn silence_is_below_threshold() {
        let mut mtdm = Mtdm::new(RATE);
        mtdm.process(&vec![0.0f32; RATE / 10]);
        assert_eq!(mtdm.resolve(), Err(ResolveError::BelowThreshold));
    }

    #[test]
    fn uncorrelated_signal_does_not_resolve() {
        // Deterministic pseudo-noise: correlates with no tone.
        let noise: Vec<f32> = (0..RATE / 2)
            .map(|i| {
                let x = (i as u64).wrapping_mul(6364136223846793005) >> 33;
                (x as f32 / u32::MAX as f32) * 0.6 - 0.3
            })
            .collect();
        let mut mtdm = Mtdm::new(RATE);
        mtdm.process(&noise);
        assert!(mtdm.resolve().is_err());
    }

    #[test]
    fn synthesize_is_deterministic_and_bounded() {
        let a = synthesize(4096);
        let b = synthesize(4096);
        assert_eq!(a, b);
        let peak = a.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(peak <= REFERENCE_AMPLITUDE + 12.0 * HELPER_AMPLITUDE);
        assert!(peak > REFERENCE_AMPLITUDE);
    }

    #[test]
    fn unambiguous_range_covers_a_second() {
        const { assert!(MAX_DELAY_FRAMES > 48_000.0) };
        // Just inside the range must resolve; the resolver would alias past
        // it, so document the bound with the largest exact case instead.
        let mut mtdm = measure(65_000, RATE * 3);
        let resolved = mtdm.resolve().expect("65000 within range");
        assert!((resolved - 65_000.0).abs() < 0.1, "resolved {resolved}");
    }
}
