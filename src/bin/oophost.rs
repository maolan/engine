use maolan_engine::message::{Lv2PluginState, Lv2StatePortValue, Lv2StateProperty};
use maolan_plugin_host_protocol::events::EventPair;
use maolan_plugin_host_protocol::protocol::*;
use maolan_plugin_host_protocol::ringbuf::RingBuffer;
use maolan_plugin_host_protocol::shm::ShmMapping;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

fn print_usage() {
    eprintln!("Usage: maolan-engine-oophost <format> <plugin-spec> <shm-name> <instance-id> <d2h-fd> <h2d-fd> <sample-rate> <buffer-size> <num-inputs> <num-outputs>");
    eprintln!("  format: vst3 | lv2");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 11 {
        print_usage();
        std::process::exit(1);
    }

    let format = args[1].clone();
    let plugin_spec = args[2].clone();
    let shm_name = args[3].clone();
    let instance_id = args[4].clone();
    let d2h_fd: i32 = args[5].parse().unwrap_or(-1);
    let h2d_fd: i32 = args[6].parse().unwrap_or(-1);
    let sample_rate: f64 = args[7].parse().unwrap_or(48000.0);
    let buffer_size: usize = args[8].parse().unwrap_or(256);
    let num_inputs: usize = args[9].parse().unwrap_or(2);
    let num_outputs: usize = args[10].parse().unwrap_or(2);

    if d2h_fd < 0 || h2d_fd < 0 {
        eprintln!("Invalid event pipe file descriptors");
        std::process::exit(3);
    }

    let mapping = match ShmMapping::open_existing(&shm_name, SHM_SIZE) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Failed to attach to shared memory '{}': {}", shm_name, e);
            std::process::exit(2);
        }
    };

    let events = unsafe { EventPair::from_fds(d2h_fd, h2d_fd) };

    // Signal readiness.
    let header = unsafe { header_mut(mapping.as_ptr()) };
    header.ready.store(1, Ordering::Release);
    eprintln!("[oophost {}] Ready for {} plugin {}", instance_id, format, plugin_spec);

    match plugin_spec.as_str() {
        "__test__" => {
            let scratch = unsafe { scratch_ptr(mapping.as_ptr()) };
            unsafe {
                std::ptr::write_unaligned(scratch as *mut u32, 0xDEADBEEF);
            }
            return;
        }
        "__crash__" => {
            // Use exit(1) instead of abort() to avoid core-dump delays
            // that can cause waitpid(WNOHANG) to return 0 on some platforms
            std::process::exit(1);
        }
        "__hang__" => {
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        _ => {}
    }

    match format.as_str() {
        "vst3" => run_vst3(Vst3RunArgs {
            plugin_path: &plugin_spec,
            mapping,
            events,
            instance_id: &instance_id,
            sample_rate,
            buffer_size,
            num_inputs,
            num_outputs,
        }),
        "lv2" => run_lv2(
            &plugin_spec,
            mapping,
            events,
            &instance_id,
            sample_rate,
            buffer_size,
        ),
        _ => {
            eprintln!("Unknown format: {}", format);
            std::process::exit(4);
        }
    }
}

/// Drain parameter events from the DAW-side param ring and apply them to a VST3 processor.
fn apply_vst3_param_ring(processor: &maolan_engine::plugins::vst3::Vst3Processor, ptr: *mut u8) {
    let ring = unsafe {
        let buf = param_ring_ptr(ptr);
        let (w, r) = param_indices(ptr);
        RingBuffer::new(buf, w, r, RING_CAPACITY)
    };
    while let Some(ev) = ring.pop() {
        if let Err(e) = processor.set_parameter_value(ev.param_index, ev.value) {
            eprintln!("[oophost] VST3 set_parameter_value failed: {}", e);
        }
    }
}

/// Read transport state from shared memory and apply it to a VST3 processor.
fn apply_vst3_transport(processor: &maolan_engine::plugins::vst3::Vst3Processor, ptr: *mut u8) {
    let transport = unsafe { transport_ref(ptr) };
    let info = maolan_engine::plugins::vst3::Vst3TransportInfo {
        playhead_sample: transport.playhead_sample as i64,
        playing: transport.flags & 0x1 != 0, // bit 0 = playing
        tempo: transport.tempo,
        tsig_num: transport.numerator as i32,
        tsig_denom: transport.denominator as i32,
    };
    processor.set_transport_info(info);
}

/// Write VST3 parameter changes to the echo ring.
fn write_vst3_echo_ring(
    processor: &maolan_engine::plugins::vst3::Vst3Processor,
    ptr: *mut u8,
    cache: &mut HashMap<u32, f32>,
) {
    let ring = unsafe {
        let buf = echo_ring_ptr(ptr);
        let (w, r) = echo_indices(ptr);
        RingBuffer::new(buf, w, r, RING_CAPACITY)
    };
    for param in processor.parameters() {
        let current = processor.get_parameter_value(param.id).unwrap_or(0.0);
        if cache.get(&param.id) != Some(&current) {
            let ev = ParameterEvent {
                param_index: param.id,
                value: current,
                sample_offset: 0,
                _pad: 0,
            };
            if !ring.push(ev) {
                eprintln!("[oophost] Echo ring full, dropping parameter event");
                break;
            }
            cache.insert(param.id, current);
        }
    }
}

/// Serialize VST3 state into scratch area. Returns bytes written or error.
fn serialize_vst3_state(scratch: *mut u8, state: &maolan_engine::plugins::vst3::state::Vst3PluginState) -> Result<usize, String> {
    let max_len = SCRATCH_SIZE;
    let mut offset = 0usize;

    let plugin_id_bytes = state.plugin_id.as_bytes();
    if offset + 4 > max_len { return Err("scratch overflow".to_string()); }
    unsafe { std::ptr::write_unaligned(scratch.add(offset) as *mut u32, plugin_id_bytes.len() as u32); }
    offset += 4;
    if offset + plugin_id_bytes.len() > max_len { return Err("scratch overflow".to_string()); }
    unsafe { std::ptr::copy_nonoverlapping(plugin_id_bytes.as_ptr(), scratch.add(offset), plugin_id_bytes.len()); }
    offset += plugin_id_bytes.len();

    if offset + 4 > max_len { return Err("scratch overflow".to_string()); }
    unsafe { std::ptr::write_unaligned(scratch.add(offset) as *mut u32, state.component_state.len() as u32); }
    offset += 4;
    if offset + state.component_state.len() > max_len { return Err("scratch overflow".to_string()); }
    unsafe { std::ptr::copy_nonoverlapping(state.component_state.as_ptr(), scratch.add(offset), state.component_state.len()); }
    offset += state.component_state.len();

    if offset + 4 > max_len { return Err("scratch overflow".to_string()); }
    unsafe { std::ptr::write_unaligned(scratch.add(offset) as *mut u32, state.controller_state.len() as u32); }
    offset += 4;
    if offset + state.controller_state.len() > max_len { return Err("scratch overflow".to_string()); }
    unsafe { std::ptr::copy_nonoverlapping(state.controller_state.as_ptr(), scratch.add(offset), state.controller_state.len()); }
    offset += state.controller_state.len();

    Ok(offset)
}

/// Deserialize VST3 state from scratch area.
fn deserialize_vst3_state(scratch: *const u8, size: usize) -> Result<maolan_engine::plugins::vst3::state::Vst3PluginState, String> {
    if size < 12 { return Err("scratch too small for VST3 state".to_string()); }
    let mut offset = 0usize;

    let plugin_id_len = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
    offset += 4;
    if offset + plugin_id_len > size { return Err("scratch underflow".to_string()); }
    let mut plugin_id_bytes = vec![0u8; plugin_id_len];
    unsafe { std::ptr::copy_nonoverlapping(scratch.add(offset), plugin_id_bytes.as_mut_ptr(), plugin_id_len); }
    offset += plugin_id_len;
    let plugin_id = String::from_utf8(plugin_id_bytes).map_err(|e| e.to_string())?;

    let component_state_len = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
    offset += 4;
    if offset + component_state_len > size { return Err("scratch underflow".to_string()); }
    let mut component_state = vec![0u8; component_state_len];
    unsafe { std::ptr::copy_nonoverlapping(scratch.add(offset), component_state.as_mut_ptr(), component_state_len); }
    offset += component_state_len;

    let controller_state_len = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
    offset += 4;
    if offset + controller_state_len > size { return Err("scratch underflow".to_string()); }
    let mut controller_state = vec![0u8; controller_state_len];
    unsafe { std::ptr::copy_nonoverlapping(scratch.add(offset), controller_state.as_mut_ptr(), controller_state_len); }

    Ok(maolan_engine::plugins::vst3::state::Vst3PluginState {
        plugin_id,
        component_state,
        controller_state,
    })
}

struct Vst3RunArgs<'a> {
    plugin_path: &'a str,
    mapping: ShmMapping,
    events: EventPair,
    instance_id: &'a str,
    sample_rate: f64,
    buffer_size: usize,
    num_inputs: usize,
    num_outputs: usize,
}

fn run_vst3(args: Vst3RunArgs) {
    let Vst3RunArgs {
        plugin_path,
        mapping,
        events,
        instance_id,
        sample_rate,
        buffer_size,
        num_inputs,
        num_outputs,
    } = args;
    let processor = match maolan_engine::plugins::vst3::Vst3Processor::new_with_sample_rate(
        sample_rate,
        buffer_size,
        plugin_path,
        num_inputs,
        num_outputs,
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[oophost {}] Failed to load VST3 plugin '{}': {}", instance_id, plugin_path, e);
            return;
        }
    };

    processor.setup_audio_ports();

    let header = unsafe { header_ref(mapping.as_ptr()) };
    let ptr = mapping.as_ptr();
    let mut vst3_param_cache = HashMap::new();

    loop {
        if header.shutdown_request.load(Ordering::Acquire) != 0 {
            eprintln!("[oophost {}] Shutdown requested", instance_id);
            break;
        }

        // Check for state request before waiting for audio signal.
        let req = header.request_type.load(Ordering::Acquire);
        if req != 0 {
            let scratch = unsafe { scratch_ptr(ptr) };
            let result = match req {
                1 => {
                    // Save state
                    match processor.snapshot_state() {
                        Ok(state) => {
                            match serialize_vst3_state(scratch, &state) {
                                Ok(size) => {
                                    header.scratch_size.store(size as u32, Ordering::Release);
                                    Ok(())
                                }
                                Err(e) => Err(e),
                            }
                        }
                        Err(e) => Err(e),
                    }
                }
                2 => {
                    // Restore state
                    let size = header.scratch_size.load(Ordering::Acquire) as usize;
                    match deserialize_vst3_state(scratch, size) {
                        Ok(state) => processor.restore_state(&state),
                        Err(e) => Err(e),
                    }
                }
                3 => {
                    // GUI show
                    processor.gui_show()
                }
                4 => {
                    // GUI hide
                    processor.gui_hide();
                    Ok(())
                }
                _ => Err(format!("Unknown request type: {}", req)),
            };
            header.request_status.store(if result.is_ok() { 1 } else { 2 }, Ordering::Release);
            let _ = events.signal_daw();
            header.request_type.store(0, Ordering::Release);
            continue;
        }

        match events.wait_daw(Duration::from_millis(100)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => {
                eprintln!("[oophost {}] Event error: {}", instance_id, e);
                break;
            }
        }

        let block_size = header.block_size.load(Ordering::Acquire) as usize;
        let num_in = header.num_input_channels.load(Ordering::Acquire) as usize;
        let num_out = header.num_output_channels.load(Ordering::Acquire) as usize;

        if block_size == 0 || block_size > MAX_BLOCK_SIZE {
            eprintln!("[oophost {}] Invalid block size {}, skipping", instance_id, block_size);
            let _ = events.signal_daw();
            continue;
        }

        // Apply parameter changes from DAW before processing.
        apply_vst3_param_ring(&processor, ptr);

        // Apply transport state before processing.
        apply_vst3_transport(&processor, ptr);

        // Copy SHM input (bus 0) to processor input buffers.
        let inputs = processor.audio_inputs();
        for (ch, input) in inputs.iter().enumerate().take(num_in) {
            let src = unsafe { audio_channel_ptr(ptr, ch, 0) };
            let dst = input.buffer.lock();
            let len = block_size.min(dst.len());
            unsafe {
                std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), len);
            }
            *input.finished.lock() = true;
        }

        // Process.
        processor.process_with_audio_io(block_size);

        // Echo parameter changes back to DAW.
        write_vst3_echo_ring(&processor, ptr, &mut vst3_param_cache);

        // Copy processor output buffers to SHM output (bus 1).
        let outputs = processor.audio_outputs();
        for (ch, output) in outputs.iter().enumerate().take(num_out) {
            let src = output.buffer.lock();
            let dst = unsafe { audio_channel_ptr(ptr, ch, 1) };
            let len = block_size.min(src.len());
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst, len);
            }
        }

        if let Err(e) = events.signal_daw() {
            eprintln!("[oophost {}] Failed to signal DAW: {}", instance_id, e);
            break;
        }
    }

    eprintln!("[oophost {}] VST3 host exiting", instance_id);
}

/// Drain parameter events from the DAW-side param ring and apply them to an LV2 processor.
fn apply_lv2_param_ring(processor: &mut maolan_engine::plugins::lv2::Lv2Processor, ptr: *mut u8) {
    let ring = unsafe {
        let buf = param_ring_ptr(ptr);
        let (w, r) = param_indices(ptr);
        RingBuffer::new(buf, w, r, RING_CAPACITY)
    };
    while let Some(ev) = ring.pop() {
        if let Err(e) = processor.set_control_value(ev.param_index, ev.value) {
            eprintln!("[oophost] LV2 set_control_value failed: {}", e);
        }
    }
}

/// Read transport state from shared memory and build LV2 transport info.
fn read_lv2_transport(ptr: *mut u8) -> maolan_engine::plugins::lv2::Lv2TransportInfo {
    let transport = unsafe { transport_ref(ptr) };
    maolan_engine::plugins::lv2::Lv2TransportInfo {
        transport_sample: transport.playhead_sample as usize,
        playing: transport.flags & 0x1 != 0,
        bpm: transport.tempo,
        tsig_num: transport.numerator,
        tsig_denom: transport.denominator,
    }
}

/// Write LV2 control-port changes to the echo ring.
fn write_lv2_echo_ring(
    processor: &maolan_engine::plugins::lv2::Lv2Processor,
    ptr: *mut u8,
    cache: &mut HashMap<u32, f32>,
) {
    let ring = unsafe {
        let buf = echo_ring_ptr(ptr);
        let (w, r) = echo_indices(ptr);
        RingBuffer::new(buf, w, r, RING_CAPACITY)
    };
    for port in processor.control_ports_with_values() {
        let current = port.value;
        if cache.get(&port.index) != Some(&current) {
            let ev = ParameterEvent {
                param_index: port.index,
                value: current,
                sample_offset: 0,
                _pad: 0,
            };
            if !ring.push(ev) {
                eprintln!("[oophost] Echo ring full, dropping parameter event");
                break;
            }
            cache.insert(port.index, current);
        }
    }
}

/// Serialize LV2 state into scratch area. Returns bytes written or error.
fn serialize_lv2_state(scratch: *mut u8, state: &Lv2PluginState) -> Result<usize, String> {
    let max_len = SCRATCH_SIZE;
    let mut offset = 0usize;

    // Port values
    if offset + 4 > max_len { return Err("scratch overflow".to_string()); }
    unsafe { std::ptr::write_unaligned(scratch.add(offset) as *mut u32, state.port_values.len() as u32); }
    offset += 4;
    for v in &state.port_values {
        if offset + 8 > max_len { return Err("scratch overflow".to_string()); }
        unsafe { std::ptr::write_unaligned(scratch.add(offset) as *mut u32, v.index); }
        offset += 4;
        unsafe { std::ptr::write_unaligned(scratch.add(offset) as *mut u32, v.value.to_bits()); }
        offset += 4;
    }

    // Properties
    if offset + 4 > max_len { return Err("scratch overflow".to_string()); }
    unsafe { std::ptr::write_unaligned(scratch.add(offset) as *mut u32, state.properties.len() as u32); }
    offset += 4;
    for prop in &state.properties {
        let key_bytes = prop.key_uri.as_bytes();
        if offset + 4 > max_len { return Err("scratch overflow".to_string()); }
        unsafe { std::ptr::write_unaligned(scratch.add(offset) as *mut u32, key_bytes.len() as u32); }
        offset += 4;
        if offset + key_bytes.len() > max_len { return Err("scratch overflow".to_string()); }
        unsafe { std::ptr::copy_nonoverlapping(key_bytes.as_ptr(), scratch.add(offset), key_bytes.len()); }
        offset += key_bytes.len();

        let type_bytes = prop.type_uri.as_bytes();
        if offset + 4 > max_len { return Err("scratch overflow".to_string()); }
        unsafe { std::ptr::write_unaligned(scratch.add(offset) as *mut u32, type_bytes.len() as u32); }
        offset += 4;
        if offset + type_bytes.len() > max_len { return Err("scratch overflow".to_string()); }
        unsafe { std::ptr::copy_nonoverlapping(type_bytes.as_ptr(), scratch.add(offset), type_bytes.len()); }
        offset += type_bytes.len();

        if offset + 4 > max_len { return Err("scratch overflow".to_string()); }
        unsafe { std::ptr::write_unaligned(scratch.add(offset) as *mut u32, prop.flags); }
        offset += 4;
        if offset + 4 > max_len { return Err("scratch overflow".to_string()); }
        unsafe { std::ptr::write_unaligned(scratch.add(offset) as *mut u32, prop.value.len() as u32); }
        offset += 4;
        if offset + prop.value.len() > max_len { return Err("scratch overflow".to_string()); }
        unsafe { std::ptr::copy_nonoverlapping(prop.value.as_ptr(), scratch.add(offset), prop.value.len()); }
        offset += prop.value.len();
    }

    Ok(offset)
}

/// Deserialize LV2 state from scratch area.
fn deserialize_lv2_state(scratch: *const u8, size: usize) -> Result<Lv2PluginState, String> {
    if size < 8 { return Err("scratch too small for LV2 state".to_string()); }
    let mut offset = 0usize;

    let port_count = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
    offset += 4;
    let mut port_values = Vec::with_capacity(port_count);
    for _ in 0..port_count {
        if offset + 8 > size { return Err("scratch underflow".to_string()); }
        let index = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) };
        offset += 4;
        let bits = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) };
        offset += 4;
        port_values.push(Lv2StatePortValue { index, value: f32::from_bits(bits) });
    }

    let prop_count = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
    offset += 4;
    let mut properties = Vec::with_capacity(prop_count);
    for _ in 0..prop_count {
        let key_len = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
        offset += 4;
        if offset + key_len > size { return Err("scratch underflow".to_string()); }
        let mut key_bytes = vec![0u8; key_len];
        unsafe { std::ptr::copy_nonoverlapping(scratch.add(offset), key_bytes.as_mut_ptr(), key_len); }
        offset += key_len;
        let key_uri = String::from_utf8(key_bytes).map_err(|e| e.to_string())?;

        let type_len = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
        offset += 4;
        if offset + type_len > size { return Err("scratch underflow".to_string()); }
        let mut type_bytes = vec![0u8; type_len];
        unsafe { std::ptr::copy_nonoverlapping(scratch.add(offset), type_bytes.as_mut_ptr(), type_len); }
        offset += type_len;
        let type_uri = String::from_utf8(type_bytes).map_err(|e| e.to_string())?;

        let flags = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) };
        offset += 4;
        let value_len = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
        offset += 4;
        if offset + value_len > size { return Err("scratch underflow".to_string()); }
        let mut value = vec![0u8; value_len];
        unsafe { std::ptr::copy_nonoverlapping(scratch.add(offset), value.as_mut_ptr(), value_len); }
        offset += value_len;

        properties.push(Lv2StateProperty { key_uri, type_uri, flags, value });
    }

    Ok(Lv2PluginState { port_values, properties })
}

fn run_lv2(
    plugin_uri: &str,
    mapping: ShmMapping,
    events: EventPair,
    instance_id: &str,
    sample_rate: f64,
    buffer_size: usize,
) {
    let mut processor = match maolan_engine::plugins::lv2::Lv2Processor::new(
        sample_rate,
        buffer_size,
        plugin_uri,
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[oophost {}] Failed to load LV2 plugin '{}': {}", instance_id, plugin_uri, e);
            return;
        }
    };

    let header = unsafe { header_ref(mapping.as_ptr()) };
    let ptr = mapping.as_ptr();
    let mut lv2_param_cache = HashMap::new();

    loop {
        if header.shutdown_request.load(Ordering::Acquire) != 0 {
            eprintln!("[oophost {}] Shutdown requested", instance_id);
            break;
        }

        // Check for state request before waiting for audio signal.
        let req = header.request_type.load(Ordering::Acquire);
        if req != 0 {
            let scratch = unsafe { scratch_ptr(ptr) };
            let result = match req {
                1 => {
                    // Save state
                    let state = processor.snapshot_state();
                    match serialize_lv2_state(scratch, &state) {
                        Ok(size) => {
                            header.scratch_size.store(size as u32, Ordering::Release);
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                }
                2 => {
                    // Restore state
                    let size = header.scratch_size.load(Ordering::Acquire) as usize;
                    match deserialize_lv2_state(scratch, size) {
                        Ok(state) => processor.restore_state(&state),
                        Err(e) => Err(e),
                    }
                }
                _ => Err(format!("Unknown request type: {}", req)),
            };
            header.request_status.store(if result.is_ok() { 1 } else { 2 }, Ordering::Release);
            let _ = events.signal_daw();
            header.request_type.store(0, Ordering::Release);
            continue;
        }

        match events.wait_daw(Duration::from_millis(100)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => {
                eprintln!("[oophost {}] Event error: {}", instance_id, e);
                break;
            }
        }

        let block_size = header.block_size.load(Ordering::Acquire) as usize;
        let num_in = header.num_input_channels.load(Ordering::Acquire) as usize;
        let num_out = header.num_output_channels.load(Ordering::Acquire) as usize;

        if block_size == 0 || block_size > MAX_BLOCK_SIZE {
            eprintln!("[oophost {}] Invalid block size {}, skipping", instance_id, block_size);
            let _ = events.signal_daw();
            continue;
        }

        // Apply parameter changes from DAW before processing.
        apply_lv2_param_ring(&mut processor, ptr);

        // Read transport state before processing.
        let transport = read_lv2_transport(ptr);

        // Copy SHM input (bus 0) to processor input buffers.
        let inputs = processor.audio_inputs();
        for (ch, input) in inputs.iter().enumerate().take(num_in) {
            let src = unsafe { audio_channel_ptr(ptr, ch, 0) };
            let dst = input.buffer.lock();
            let len = block_size.min(dst.len());
            unsafe {
                std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), len);
            }
            *input.finished.lock() = true;
        }

        // Process.
        let _midi_out = processor.process_with_audio_io(
            block_size,
            &[],
            transport,
        );

        // Echo parameter changes back to DAW.
        write_lv2_echo_ring(&processor, ptr, &mut lv2_param_cache);

        // Copy processor output buffers to SHM output (bus 1).
        let outputs = processor.audio_outputs();
        for (ch, output) in outputs.iter().enumerate().take(num_out) {
            let src = output.buffer.lock();
            let dst = unsafe { audio_channel_ptr(ptr, ch, 1) };
            let len = block_size.min(src.len());
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst, len);
            }
        }

        if let Err(e) = events.signal_daw() {
            eprintln!("[oophost {}] Failed to signal DAW: {}", instance_id, e);
            break;
        }
    }

    eprintln!("[oophost {}] LV2 host exiting", instance_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lv2_state_serialization_roundtrip() {
        let state = Lv2PluginState {
            port_values: vec![
                Lv2StatePortValue { index: 0, value: 0.5 },
                Lv2StatePortValue { index: 1, value: 1.0 },
            ],
            properties: vec![
                Lv2StateProperty {
                    key_uri: "http://example.com/key".to_string(),
                    type_uri: "http://example.com/type".to_string(),
                    flags: 0,
                    value: vec![1, 2, 3],
                },
            ],
        };
        let mut scratch = vec![0u8; SCRATCH_SIZE];
        let size = serialize_lv2_state(scratch.as_mut_ptr(), &state).expect("serialize should succeed");
        assert!(size > 0);
        assert!(size < SCRATCH_SIZE);

        let decoded = deserialize_lv2_state(scratch.as_ptr(), size).expect("deserialize should succeed");
        assert_eq!(decoded.port_values.len(), state.port_values.len());
        assert_eq!(decoded.port_values[0].index, state.port_values[0].index);
        assert_eq!(decoded.port_values[0].value, state.port_values[0].value);
        assert_eq!(decoded.properties.len(), state.properties.len());
        assert_eq!(decoded.properties[0].key_uri, state.properties[0].key_uri);
        assert_eq!(decoded.properties[0].type_uri, state.properties[0].type_uri);
        assert_eq!(decoded.properties[0].flags, state.properties[0].flags);
        assert_eq!(decoded.properties[0].value, state.properties[0].value);
    }
}
