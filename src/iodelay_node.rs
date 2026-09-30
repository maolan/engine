//! In-engine MTDM generator and independently routed measurement inputs.

use crate::audio::io::AudioIO;
use crate::mtdm::{self, IoDelayReport, Mtdm};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const REPORT_INTERVAL_DIV: usize = 4;

struct DemodState {
    mtdm: Mtdm,
    next_report_frame: u64,
    scratch: Vec<f32>,
}

/// One isolated return input and demodulator.
pub struct IoDelayMeasurementRt {
    pub id: u64,
    pub in_port: Arc<AudioIO>,
    demod: Mutex<DemodState>,
    gain: f32,
    report_slot: Mutex<Option<IoDelayReport>>,
}

impl IoDelayMeasurementRt {
    fn new(id: u64, gain: f32, buffer_size: usize, sample_rate: usize, start_frame: u64) -> Self {
        let mut mtdm = Mtdm::new(sample_rate.max(1));
        mtdm.set_stream_position(start_frame);
        Self {
            id,
            in_port: Arc::new(AudioIO::new(buffer_size)),
            demod: Mutex::new(DemodState {
                mtdm,
                next_report_frame: start_frame,
                scratch: Vec::with_capacity(buffer_size),
            }),
            gain,
            report_slot: Mutex::new(None),
        }
    }

    pub fn process(&self, input: &[f32]) {
        let mut demod = self.demod.lock().expect("iodelay demod mutex poisoned");
        let DemodState {
            mtdm,
            next_report_frame,
            scratch,
        } = &mut *demod;
        scratch.clear();
        scratch.extend(input.iter().map(|&sample| sample * self.gain));
        mtdm.process(scratch);
        let position = mtdm.stream_position();
        if position >= *next_report_frame {
            let report = mtdm::make_report(mtdm, false);
            let interval = (mtdm.rate() / REPORT_INTERVAL_DIV).max(1) as u64;
            *next_report_frame = position + interval;
            *self
                .report_slot
                .lock()
                .expect("iodelay report mutex poisoned") = Some(report);
        }
    }

    /// Resolve the current demodulator state for an explicit calibration request.
    pub(crate) fn calibration_report(&self) -> IoDelayReport {
        let mut demod = self.demod.lock().expect("iodelay demod mutex poisoned");
        mtdm::make_report(&mut demod.mtdm, false)
    }

    pub fn take_report(&self) -> Option<IoDelayReport> {
        self.report_slot
            .lock()
            .expect("iodelay report mutex poisoned")
            .take()
    }
}

/// Shared tone generator and registry of independently measured return paths.
pub struct IoDelayRt {
    pub out_port: Arc<AudioIO>,
    /// Legacy endpoint alias for measurement 0.
    pub in_port: Arc<AudioIO>,
    buffer_size: usize,
    sample_rate: usize,
    gain: f32,
    generator_frame: AtomicU64,
    measurements: Mutex<BTreeMap<u64, Arc<IoDelayMeasurementRt>>>,
}

impl IoDelayRt {
    pub fn new(gain: f32, buffer_size: usize, sample_rate: usize) -> Self {
        // Keep the original `iodelay` endpoint as measurement 0 for sessions
        // saved before measurements received individual ids.
        let measurement_zero = Arc::new(IoDelayMeasurementRt::new(
            0,
            gain,
            buffer_size,
            sample_rate.max(1),
            0,
        ));
        let mut measurements = BTreeMap::new();
        measurements.insert(0, measurement_zero.clone());
        Self {
            out_port: Arc::new(AudioIO::new(buffer_size)),
            in_port: measurement_zero.in_port.clone(),
            buffer_size,
            sample_rate: sample_rate.max(1),
            gain,
            generator_frame: AtomicU64::new(0),
            measurements: Mutex::new(measurements),
        }
    }

    pub fn add_measurement(&self, id: u64, gain: f32) -> Arc<IoDelayMeasurementRt> {
        let mut measurements = self
            .measurements
            .lock()
            .expect("iodelay measurement registry poisoned");
        measurements
            .entry(id)
            .or_insert_with(|| {
                Arc::new(IoDelayMeasurementRt::new(
                    id,
                    gain,
                    self.buffer_size,
                    self.sample_rate,
                    self.generator_frame.load(Ordering::Acquire),
                ))
            })
            .clone()
    }

    pub fn measurement(&self, id: u64) -> Option<Arc<IoDelayMeasurementRt>> {
        self.measurements
            .lock()
            .expect("iodelay measurement registry poisoned")
            .get(&id)
            .cloned()
    }

    pub fn measurements(&self) -> Vec<Arc<IoDelayMeasurementRt>> {
        self.measurements
            .lock()
            .expect("iodelay measurement registry poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// A cancelled parallel cycle may have executed the generator but not
    /// all demodulators (or vice versa). Restart them together after all
    /// workers have retired, so an xrun cannot leave a permanent phase offset
    /// masquerading as device latency. Never call while node jobs are live.
    pub(crate) fn restart_after_cancelled_cycle(&self) {
        self.generator_frame.store(0, Ordering::Release);
        for measurement in self.measurements() {
            let mut demod = measurement.demod.lock().unwrap_or_else(|e| e.into_inner());
            demod.mtdm = Mtdm::new(self.sample_rate);
            demod.next_report_frame = 0;
            *measurement
                .report_slot
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    pub fn process_generator(&self, output: &mut [f32]) {
        let base = self
            .generator_frame
            .fetch_add(output.len() as u64, Ordering::AcqRel);
        for (i, out) in output.iter_mut().enumerate() {
            *out = mtdm::synthesize_frame(base + i as u64);
        }
    }

    /// Compatibility helper for existing unit tests and the standalone node
    /// path: run the legacy measurement 0 and generator together.
    pub fn process(&self, input: &[f32], output: &mut [f32]) {
        self.process_generator(output);
        if let Some(measurement) = self.measurement(0) {
            measurement.process(input);
        }
    }

    pub fn take_report(&self) -> Option<IoDelayReport> {
        self.measurement(0)
            .and_then(|measurement| measurement.take_report())
    }
}

impl fmt::Debug for IoDelayRt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoDelayRt")
            .field("gain", &self.gain)
            .field(
                "measurement_count",
                &self
                    .measurements
                    .lock()
                    .map(|m| m.len())
                    .unwrap_or_default(),
            )
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for IoDelayMeasurementRt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoDelayMeasurementRt")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mtdm::IoDelayStatus;
    const PERIOD: usize = 512;
    const RATE: usize = 48_000;

    #[test]
    fn calibration_uses_live_state_and_rejects_reset_measurement() {
        let node = IoDelayRt::new(1.0, PERIOD, RATE);
        run_loop(&node, 1000, 2);
        let measurement = node.measurement(0).unwrap();
        let _ = measurement.take_report();
        let report = measurement.calibration_report();
        assert_eq!(report.status, IoDelayStatus::Resolved);
        assert!((report.delay_frames - (2 * PERIOD) as f64).abs() < 1.0);
        node.restart_after_cancelled_cycle();
        assert_ne!(
            measurement.calibration_report().status,
            IoDelayStatus::Resolved
        );
    }

    fn run_loop(node: &IoDelayRt, cycles: usize, delay_cycles: usize) {
        let mut history: Vec<Vec<f32>> = Vec::new();
        let mut input = vec![0.0f32; PERIOD];
        let mut output = vec![0.0f32; PERIOD];
        for _ in 0..cycles {
            node.process(&input, &mut output);
            history.push(output.clone());
            input = if history.len() >= delay_cycles {
                history[history.len() - delay_cycles].clone()
            } else {
                vec![0.0; PERIOD]
            };
        }
    }

    #[test]
    fn cancelled_generator_only_cycle_does_not_bias_subsequent_measurements() {
        let node = IoDelayRt::new(1.0, PERIOD, RATE);
        node.add_measurement(1, 1.0);
        let mut output = vec![0.0; PERIOD];
        node.process_generator(&mut output);
        // The deadline cancelled both return nodes after generation.
        node.restart_after_cancelled_cycle();
        let extra = node.measurement(1).unwrap();
        let mut history = Vec::new();
        let mut input = vec![0.0; PERIOD];
        for _ in 0..400 {
            node.process(&input, &mut output);
            extra.process(&input);
            history.push(output.clone());
            if history.len() >= 3 {
                input = history[history.len() - 3].clone();
            }
        }
        for report in [node.take_report().unwrap(), extra.take_report().unwrap()] {
            assert_eq!(report.status, IoDelayStatus::Resolved);
            assert!((report.delay_frames - 3.0 * PERIOD as f64).abs() < 1.0);
        }
    }

    #[test]
    fn looped_tone_resolves_to_the_loop_delay() {
        let node = IoDelayRt::new(1.0, PERIOD, RATE);
        run_loop(&node, 400, 3);
        let report = node.take_report().expect("a report must have accumulated");
        assert_eq!(report.status, IoDelayStatus::Resolved, "{report:?}");
        let expected = 3.0 * PERIOD as f64;
        assert!((report.delay_frames - expected).abs() < 1.0);
    }

    #[test]
    fn unlooped_input_stays_below_threshold() {
        let node = IoDelayRt::new(1.0, PERIOD, RATE);
        run_loop(&node, 100, usize::MAX);
        assert_eq!(
            node.take_report().expect("report").status,
            IoDelayStatus::BelowThreshold
        );
    }

    #[test]
    fn report_slot_drains_once() {
        let node = IoDelayRt::new(1.0, PERIOD, RATE);
        run_loop(&node, 400, 3);
        assert!(node.take_report().is_some());
        assert!(node.take_report().is_none());
    }
}
