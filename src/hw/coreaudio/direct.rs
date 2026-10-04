//! Native float HAL IOProc path. One device callback owns capture and playback;
//! no audio queue or AudioUnit conversion sits between the device and the arena.
use super::*;
use crate::inline_render::InlineRender;
use crate::render_plan::PlanSlot;
use std::time::Instant;

const INPUT: u32 = u32::from_be_bytes(*b"inpt");
const OUTPUT: u32 = u32::from_be_bytes(*b"outp");
const VIRTUAL_FORMAT: u32 = u32::from_be_bytes(*b"sfmt");
const STREAM_LATENCY: u32 = u32::from_be_bytes(*b"ltnc");
type IoProc = unsafe extern "C" fn(
    u32,
    *const c_void,
    *const AudioBufferList,
    *const c_void,
    *mut AudioBufferList,
    *const c_void,
    *mut c_void,
) -> i32;

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioDeviceCreateIOProcID(
        device: u32,
        callback: IoProc,
        context: *mut c_void,
        id: *mut Option<IoProc>,
    ) -> i32;
    fn AudioDeviceDestroyIOProcID(device: u32, id: IoProc) -> i32;
    fn AudioDeviceStart(device: u32, id: IoProc) -> i32;
    fn AudioDeviceStop(device: u32, id: IoProc) -> i32;
}

struct Cycle {
    requested: bool,
    failed: bool,
    playing: bool,
    gain: f32,
    balance: f32,
    plan: Option<Arc<PlanSlot>>,
    inline: Option<Arc<InlineRender>>,
}

struct Context {
    cycle: Mutex<Cycle>,
    completed: Condvar,
    stop: Arc<AtomicBool>,
    xruns: AtomicUsize,
    frames: usize,
    inputs: usize,
    outputs: usize,
}

/// The IOProc holds an Arc reference until it is stopped and destroyed. Only the
/// callback accesses HAL buffers; the worker waits for completion before the
/// dispatcher may reuse the arena. The callback never waits for the cycle lock.
pub struct DirectDriver {
    device: u32,
    proc: Option<IoProc>,
    context: Arc<Context>,
    pub(super) stop: Arc<AtomicBool>,
    rate: i32,
    ins: Vec<Arc<AudioIO>>,
    outs: Vec<Arc<AudioIO>>,
    input_latency: usize,
    output_latency: usize,
}

fn formats(device: u32, input: bool, rate: f64) -> Result<usize, String> {
    let mut channels = 0;
    for stream in device_stream_ids_for_scope(device, if input { INPUT } else { OUTPUT }) {
        let direction: u32 = get_property_data(
            stream,
            K_AUDIO_STREAM_PROPERTY_DIRECTION,
            K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            "stream direction",
        )?;
        if direction != u32::from(input) {
            continue;
        }
        let f: AudioStreamBasicDescription = get_property_data(
            stream,
            VIRTUAL_FORMAT,
            K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            "stream virtual format",
        )?;
        // IOProc data uses the virtual format, not the hardware physical format.
        // Accept only native packed float32, without a converter or resampler.
        if f.m_format_id != K_AUDIO_FORMAT_LINEAR_PCM
            || f.m_bits_per_channel != 32
            || f.m_format_flags & (K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED)
                != K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED
            || f.m_format_flags & 2 != 0
            || !f.m_sample_rate.is_finite()
            || (f.m_sample_rate - rate).abs() > 0.01
            || f.m_channels_per_frame == 0
            || f.m_frames_per_packet != 1
            || f.m_bytes_per_frame
                != if f.m_format_flags & 32 != 0 {
                    4
                } else {
                    f.m_channels_per_frame * 4
                }
        {
            return Err(
                "Device does not expose native float32 at the requested sample rate".into(),
            );
        }
        channels += f.m_channels_per_frame as usize;
    }
    Ok(channels)
}

fn direction_latency(device: u32, input: bool) -> usize {
    let scope = if input { INPUT } else { OUTPUT };
    let property = |selector| {
        get_property_data::<u32>(device, selector, scope, "device latency").unwrap_or(0) as usize
    };
    let stream_latency = device_stream_ids(device)
        .into_iter()
        .filter_map(|stream| {
            let direction = get_property_data::<u32>(
                stream,
                K_AUDIO_STREAM_PROPERTY_DIRECTION,
                K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
                "stream direction",
            )
            .ok()?;
            (direction == u32::from(input)).then(|| {
                get_property_data::<u32>(
                    stream,
                    STREAM_LATENCY,
                    K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
                    "stream latency",
                )
                .unwrap_or(0) as usize
            })
        })
        .max()
        .unwrap_or(0);
    property(K_AUDIO_DEVICE_PROPERTY_LATENCY)
        + property(K_AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET)
        + stream_latency
}

// SAFETY: callers supply HAL-owned variable-length AudioBufferLists, valid for
// the current callback. No slice or sample pointer escapes the callback.
unsafe fn buffers<'a>(list: *const AudioBufferList) -> &'a [AudioBuffer] {
    if list.is_null() {
        return &[];
    }
    unsafe {
        std::slice::from_raw_parts(
            ptr::addr_of!((*list).m_buffers).cast(),
            (*list).m_number_buffers as usize,
        )
    }
}

fn valid_buffers(buffers: &[AudioBuffer], frames: usize, channels: usize) -> bool {
    buffers
        .iter()
        .map(|b| b.m_number_channels as usize)
        .sum::<usize>()
        == channels
        && buffers.iter().all(|b| {
            !b.m_data.is_null()
                && b.m_number_channels > 0
                && b.m_data_byte_size as usize
                    == frames * b.m_number_channels as usize * size_of::<f32>()
        })
}

fn channel(buffers: &[AudioBuffer], mut ch: usize) -> Option<(*mut f32, usize, usize)> {
    for b in buffers {
        let stride = b.m_number_channels as usize;
        if ch < stride {
            return Some((b.m_data.cast(), stride, ch));
        }
        ch -= stride;
    }
    None
}

unsafe extern "C" fn callback(
    _device: u32,
    _now: *const c_void,
    input: *const AudioBufferList,
    _input_time: *const c_void,
    output: *mut AudioBufferList,
    _output_time: *const c_void,
    context: *mut c_void,
) -> i32 {
    // SAFETY: HAL retains this registered context through Stop/DestroyIOProcID.
    let context = unsafe { &*context.cast::<Context>() };
    let outputs = unsafe { buffers(output) };
    for b in outputs {
        if !b.m_data.is_null() {
            // Silence even on stop, starvation, or a changed device format.
            unsafe {
                ptr::write_bytes(b.m_data.cast::<u8>(), 0, b.m_data_byte_size as usize);
            }
        }
    }
    if context.stop.load(Ordering::Acquire) {
        return 0;
    }
    let Ok(mut cycle) = context.cycle.try_lock() else {
        return 0;
    };
    if !cycle.requested {
        if cycle.playing {
            context.xruns.fetch_add(1, Ordering::Relaxed);
        }
        return 0;
    }
    let inputs = unsafe { buffers(input) };
    if !valid_buffers(outputs, context.frames, context.outputs)
        || (context.inputs > 0 && !valid_buffers(inputs, context.frames, context.inputs))
    {
        context.xruns.fetch_add(1, Ordering::Relaxed);
        cycle.failed = true;
        cycle.requested = false;
        context.completed.notify_one();
        return 0;
    }
    let plan = cycle
        .inline
        .as_ref()
        .map(|ctx| ctx.cycle_plan())
        .or_else(|| cycle.plan.as_ref().map(|slot| slot.load_full()));
    if let Some(plan) = plan {
        for &(ch, buffer) in &plan.hw_in_map {
            // SAFETY: the worker has handed this cycle exclusively to the IOProc;
            // no arena reader is dispatched until capture is filled.
            let arena = unsafe { &mut *plan.buffer_ptr(buffer) };
            let available = context.frames.min(arena.len());
            for (frame, sample) in arena[..available].iter_mut().enumerate() {
                *sample = if context.inputs > 0 {
                    channel(inputs, ch)
                        .map(|(base, stride, offset)| unsafe { *base.add(frame * stride + offset) })
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
            }
        }
        let mut stale = false;
        if let Some(inline) = &cycle.inline {
            inline.render_cycle(context.frames as u32, None);
            stale = inline.take_stale_silence();
        }
        if cycle.playing && !stale {
            crate::hw::ports::write_interleaved_from_arena(
                &plan,
                context.frames,
                cycle.gain,
                cycle.balance,
                |ch, frame, sample| {
                    if let Some((base, stride, offset)) = channel(outputs, ch) {
                        // SAFETY: valid_buffers checked bounds and float layout above.
                        unsafe {
                            *base.add(frame * stride + offset) = sample;
                        }
                    }
                },
            );
        }
    }
    cycle.requested = false;
    context.completed.notify_one();
    0
}

impl DirectDriver {
    pub fn open(
        device: &str,
        input: Option<&str>,
        rate: i32,
        options: HwOptions,
    ) -> Result<Self, String> {
        let device = resolve_device_id(
            device,
            K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE,
            "default output",
        )?;
        if let Some(input) = input {
            let input = resolve_device_id(
                input,
                K_AUDIO_HARDWARE_PROPERTY_DEFAULT_INPUT_DEVICE,
                "default input",
            )?;
            if input != device {
                return Err(
                    "Direct duplex requires one device (or a CoreAudio aggregate device)".into(),
                );
            }
        }
        let rate = rate.max(1);
        let current = device_nominal_sample_rate(device).ok_or("Cannot read device sample rate")?;
        if (current - f64::from(rate)).abs() > 0.01
            && !set_device_nominal_sample_rate(device, f64::from(rate))
        {
            return Err("Device rejected native sample rate".into());
        }
        let outputs = formats(device, false, f64::from(rate))?;
        let inputs = if input.is_some() {
            formats(device, true, f64::from(rate))?
        } else {
            0
        };
        if outputs == 0 {
            return Err("No native output streams".into());
        }
        let range =
            device_buffer_frame_size_range(device).ok_or("Cannot read device buffer range")?;
        if !range.m_minimum.is_finite()
            || !range.m_maximum.is_finite()
            || range.m_minimum < 1.0
            || range.m_maximum.floor() < range.m_minimum.ceil()
        {
            return Err("Invalid device buffer range".into());
        }
        let requested = (options.period_frames.max(1) as f64)
            .clamp(range.m_minimum.ceil(), range.m_maximum.floor()) as u32;
        if !set_device_buffer_frame_size(device, requested) {
            return Err("Device rejected buffer size".into());
        }
        let frames = actual_buffer_frame_size(device)
            .filter(|frames| *frames > 0)
            .ok_or("Cannot read actual device buffer size")? as usize;
        let stop = Arc::new(AtomicBool::new(false));
        let context = Arc::new(Context {
            cycle: Mutex::new(Cycle {
                requested: false,
                failed: false,
                playing: false,
                gain: 1.0,
                balance: 0.0,
                plan: None,
                inline: None,
            }),
            completed: Condvar::new(),
            stop: stop.clone(),
            xruns: AtomicUsize::new(0),
            frames,
            inputs,
            outputs,
        });
        let mut driver = Self {
            device,
            proc: None,
            context,
            stop,
            rate,
            ins: (0..inputs)
                .map(|_| Arc::new(AudioIO::new(frames)))
                .collect(),
            outs: (0..outputs)
                .map(|_| Arc::new(AudioIO::new(frames)))
                .collect(),
            input_latency: if inputs > 0 {
                direction_latency(device, true) + options.input_latency_frames
            } else {
                0
            },
            output_latency: direction_latency(device, false) + options.output_latency_frames,
        };
        // The registration owns a reference independently of the driver. If
        // HAL cannot retire it, retain it to keep late callbacks safe.
        let retained = Arc::into_raw(driver.context.clone());
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                device,
                callback,
                retained.cast_mut().cast(),
                &mut driver.proc,
            )
        };
        if status != 0 || driver.proc.is_none() {
            if driver.proc.is_none() {
                unsafe {
                    drop(Arc::from_raw(retained));
                }
            }
            return Err(os_error("AudioDeviceCreateIOProcID", status));
        }
        let proc = driver.proc.ok_or("HAL returned no IOProc")?;
        let status = unsafe { AudioDeviceStart(device, proc) };
        if status != 0 {
            return Err(os_error("AudioDeviceStart", status));
        }
        debug!(
            device,
            frames, rate, inputs, outputs, "Direct CoreAudio HAL backend opened"
        );
        Ok(driver)
    }

    pub fn input_channels(&self) -> usize {
        self.ins.len()
    }
    pub fn output_channels(&self) -> usize {
        self.outs.len()
    }
    pub fn sample_rate(&self) -> i32 {
        self.rate
    }
    pub fn cycle_samples(&self) -> usize {
        self.context.frames
    }
    pub fn sample_bits(&self) -> i32 {
        32
    }
    pub fn frame_size_bytes(&self) -> usize {
        self.outs.len() * 4
    }
    pub fn input_port(&self, idx: usize) -> Option<Arc<AudioIO>> {
        self.ins.get(idx).cloned()
    }
    pub fn output_port(&self, idx: usize) -> Option<Arc<AudioIO>> {
        self.outs.get(idx).cloned()
    }
    pub fn set_output_gain_balance(&mut self, gain: f32, balance: f32) {
        let mut cycle = self.context.cycle.lock().unwrap_or_else(|e| e.into_inner());
        cycle.gain = gain.max(0.0);
        cycle.balance = balance.clamp(-1.0, 1.0);
    }
    pub fn set_plan_slot(&mut self, slot: Arc<PlanSlot>) {
        self.context
            .cycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .plan = Some(slot);
    }
    pub fn set_inline_render(&mut self, ctx: Option<Arc<InlineRender>>) {
        self.context
            .cycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .inline = ctx;
    }
    pub fn output_meter_db(&self, gain: f32, balance: f32) -> Vec<f32> {
        common::output_meter_db(self.outs.len(), gain, balance)
    }
    pub fn output_meter_linear(&self, gain: f32, balance: f32) -> Vec<f32> {
        let cycle = self.context.cycle.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = &cycle.plan {
            common::output_meter_linear_from_plan(&slot.load(), gain, balance)
        } else {
            common::output_meter_linear(self.outs.len(), gain, balance)
        }
    }
    pub fn run_cycle(&mut self) -> Result<(), String> {
        let mut cycle = self.context.cycle.lock().unwrap_or_else(|e| e.into_inner());
        if self.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        cycle.requested = true;
        let deadline = Instant::now() + Duration::from_millis(500);
        while cycle.requested {
            let (guard, _) = self
                .context
                .completed
                .wait_timeout(cycle, Duration::from_millis(10))
                .unwrap_or_else(|e| e.into_inner());
            cycle = guard;
            // Holding the mutex guarantees no callback is still using the arena.
            if self.stop.load(Ordering::Acquire) {
                cycle.requested = false;
                return Ok(());
            }
            if cycle.requested && Instant::now() >= deadline {
                cycle.requested = false;
                return Err("Timed out waiting for CoreAudio device cycle".into());
            }
        }
        if cycle.failed {
            return Err("CoreAudio device format/buffer size changed; reopen the device".into());
        }
        Ok(())
    }
    pub fn run_assist_step(&mut self) -> Result<bool, String> {
        Ok(false)
    }
    pub fn set_playing(&mut self, playing: bool) {
        self.context
            .cycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .playing = playing;
    }
    pub fn xrun_count(&self) -> u64 {
        self.context.xruns.load(Ordering::Relaxed) as u64
    }
    pub fn latency_ranges(&self) -> ((usize, usize), (usize, usize)) {
        // One hardware cycle plus device, stream, and safety latency. No software
        // queue periods are added on this path. Capture and playback each cover
        // a full buffer window conservatively.
        let input = if self.ins.is_empty() {
            0
        } else {
            self.input_latency + self.context.frames
        };
        let pipeline = usize::from(
            self.context
                .cycle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .inline
                .is_none(),
        ) * self.context.frames;
        let output = self.output_latency + self.context.frames + pipeline;
        ((input, input), (output, output))
    }
    pub fn close_fds(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(proc) = self.proc.take() {
            // SAFETY: release the registered reference only after HAL retires
            // the callback. On failure retain it to prevent use-after-free.
            let stopped = unsafe { AudioDeviceStop(self.device, proc) };
            let destroyed = unsafe { AudioDeviceDestroyIOProcID(self.device, proc) };
            if stopped == 0 && destroyed == 0 {
                unsafe {
                    drop(Arc::from_raw(Arc::as_ptr(&self.context)));
                }
            } else {
                warn!(
                    stopped,
                    destroyed, "HAL teardown failed; retaining callback context"
                );
            }
        }
    }
}

impl Drop for DirectDriver {
    fn drop(&mut self) {
        self.close_fds();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_plan::{DelayLine, Op, RenderPlan};
    use std::cell::UnsafeCell;
    use std::collections::HashMap;

    fn context(inputs: usize) -> Context {
        Context {
            cycle: Mutex::new(Cycle {
                requested: true,
                failed: false,
                playing: true,
                gain: 1.0,
                balance: 0.0,
                plan: None,
                inline: None,
            }),
            completed: Condvar::new(),
            stop: Arc::new(AtomicBool::new(false)),
            xruns: AtomicUsize::new(0),
            frames: 8,
            inputs,
            outputs: 2,
        }
    }

    fn buffer(data: &mut [f32], channels: u32) -> AudioBuffer {
        AudioBuffer {
            m_number_channels: channels,
            m_data_byte_size: std::mem::size_of_val(data) as u32,
            m_data: data.as_mut_ptr().cast(),
        }
    }

    fn invoke(context: &Context, input: *const AudioBufferList, output: *mut AudioBufferList) {
        // SAFETY: lists own live sample storage for this synchronous test call.
        unsafe {
            callback(
                0,
                ptr::null(),
                input,
                ptr::null(),
                output,
                ptr::null(),
                (context as *const Context).cast_mut().cast(),
            );
        }
    }

    #[test]
    fn contended_callback_returns_silence_without_waiting() {
        let context = context(0);
        let guard = context.cycle.lock().unwrap();
        let mut samples = [7.0; 16];
        let mut output = AudioBufferList {
            m_number_buffers: 1,
            m_buffers: [buffer(&mut samples, 2)],
        };
        invoke(&context, ptr::null(), &mut output);
        assert_eq!(samples, [0.0; 16]);
        assert!(guard.requested);
    }

    #[test]
    fn changed_buffer_size_fails_cycle_and_silences_only_supplied_memory() {
        let context = context(0);
        let mut samples = [7.0; 16];
        let mut output = AudioBufferList {
            m_number_buffers: 1,
            m_buffers: [buffer(&mut samples[..8], 2)],
        };
        invoke(&context, ptr::null(), &mut output);
        assert_eq!(&samples[..8], &[0.0; 8]);
        assert_eq!(&samples[8..], &[7.0; 8]);
        let cycle = context.cycle.lock().unwrap();
        assert!(!cycle.requested);
        assert!(cycle.failed);
        assert_eq!(context.xruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn capture_is_rendered_into_planar_output_in_the_same_callback() {
        let mut collector = basedrop::Collector::new();
        {
            let plan = RenderPlan {
                buffer_size: 8,
                buffers: (0..2).map(|_| UnsafeCell::new(vec![0.0; 8])).collect(),
                buffer_latencies: (0..2).map(|_| AtomicUsize::new(0)).collect(),
                nodes: vec![
                    Op::HwInput {
                        channel: 0,
                        output: 0,
                    },
                    Op::Sum {
                        inputs: vec![0],
                        delays: vec![UnsafeCell::new(DelayLine::new())],
                        output: 1,
                    },
                ],
                indegree: vec![0, 1],
                dependents: vec![vec![1], vec![]],
                sources: vec![0],
                hw_in_map: vec![(0, 0)],
                hw_out_map: vec![(1, 0)],
                port_map: HashMap::new(),
                midi_edges: vec![],
                forced: vec![],
            };
            let slot = Arc::new(PlanSlot::from_pointee(basedrop::Owned::new(
                &collector.handle(),
                plan,
            )));
            let inline = InlineRender::new(slot);
            inline.configure_parallel(false, 48_000);
            inline.request_render(true);
            let context = context(1);
            context.cycle.lock().unwrap().inline = Some(inline);
            let mut capture = [0.25; 8];
            let input = AudioBufferList {
                m_number_buffers: 1,
                m_buffers: [buffer(&mut capture, 1)],
            };
            let mut left = [7.0; 8];
            let mut right = [7.0; 8];
            #[repr(C)]
            struct StereoList {
                count: u32,
                buffers: [AudioBuffer; 2],
            }
            let mut output = StereoList {
                count: 2,
                buffers: [buffer(&mut left, 1), buffer(&mut right, 1)],
            };
            invoke(&context, &input, (&mut output as *mut StereoList).cast());
            assert_eq!(left, capture);
            assert_eq!(right, [0.0; 8]);
            assert!(!context.cycle.lock().unwrap().requested);
            // A missed dispatcher cycle emits silence, never the prior block.
            invoke(&context, &input, (&mut output as *mut StereoList).cast());
            assert_eq!(left, [0.0; 8]);
            assert_eq!(context.xruns.load(Ordering::Relaxed), 1);
        }
        collector.collect();
        assert!(collector.try_cleanup().is_ok());
    }
}
