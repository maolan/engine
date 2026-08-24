use nix::libc;
use nvtree::{Nvtree, Nvtvalue, nvtree_find, nvtree_unpack};
use std::{ffi::c_void, fs::File, os::fd::AsRawFd};

const AFMT_S16_LE: u64 = 0x00000010;
const AFMT_S16_BE: u64 = 0x00000020;
const AFMT_S8: u64 = 0x00000040;
const AFMT_S32_LE: u64 = 0x00001000;
const AFMT_S32_BE: u64 = 0x00002000;
const AFMT_S24_LE: u64 = 0x00010000;
const AFMT_S24_BE: u64 = 0x00020000;
const DIRECT_KEYS: [&str; 9] = [
    "formats",
    "iformats",
    "oformats",
    "pformats",
    "rformats",
    "playformats",
    "recformats",
    "pfmts",
    "rfmts",
];
const RATE_KEYS: [&str; 8] = [
    "rates",
    "rate",
    "irates",
    "orates",
    "playrates",
    "recrates",
    "playback_rates",
    "capture_rates",
];
const SAMPLE_RATE_CANDIDATES: [i32; 12] = [
    8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000,
    384_000,
];

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AudioDeviceDescriptor {
    pub id: String,
    pub label: String,
    pub supported_bits: Vec<usize>,
    pub supported_sample_rates: Vec<i32>,
    pub max_channels: usize,
    pub max_buffer_bytes: usize,
    pub supports_input: bool,
    pub supports_output: bool,
}

impl AudioDeviceDescriptor {
    fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        mut supported_bits: Vec<usize>,
        mut supported_sample_rates: Vec<i32>,
        max_channels: usize,
        max_buffer_bytes: usize,
    ) -> Self {
        supported_bits.sort_by(|a, b| b.cmp(a));
        supported_bits.dedup();
        supported_sample_rates.retain(|rate| *rate > 0);
        supported_sample_rates.sort_unstable();
        supported_sample_rates.dedup();
        Self {
            id: id.into(),
            label: label.into(),
            supported_bits,
            supported_sample_rates,
            max_channels,
            max_buffer_bytes,
            supports_input: true,
            supports_output: true,
        }
    }
}

pub fn discover_freebsd_audio_devices() -> Vec<AudioDeviceDescriptor> {
    let mut devices = discover_sndstat_dsp_devices().unwrap_or_default();
    devices.sort_by_key(|device| device.label.to_lowercase());
    devices.dedup_by(|a, b| a.id == b.id);
    devices
}

fn discover_sndstat_dsp_devices() -> Option<Vec<AudioDeviceDescriptor>> {
    let file = File::open("/dev/sndstat").ok()?;
    let fd = file.as_raw_fd();
    unsafe {
        if sndst_refresh_devs(fd).is_err() {
            return None;
        }
    }
    let mut arg = SndstIoctlNvArg {
        nbytes: 0,
        buf: std::ptr::null_mut(),
    };
    unsafe {
        if sndst_get_devs(fd, &mut arg).is_err() {
            return None;
        }
    }
    if arg.nbytes == 0 {
        return None;
    }
    let mut buf = vec![0_u8; arg.nbytes];
    arg.buf = buf.as_mut_ptr().cast::<c_void>();
    unsafe {
        if sndst_get_devs(fd, &mut arg).is_err() {
            return None;
        }
    }
    parse_sndstat_nvlist(&buf[..arg.nbytes.min(buf.len())])
}

fn parse_sndstat_nvlist(buf: &[u8]) -> Option<Vec<AudioDeviceDescriptor>> {
    let root = nvtree_unpack(buf).ok()?;
    let pair = nvtree_find(&root, "dsps")?;
    let Nvtvalue::NestedArray(dsps) = &pair.value else {
        return None;
    };
    let out = dsps
        .iter()
        .filter_map(|dsp| {
            let devnode_pair = nvtree_find(dsp, "devnode")?;
            let Nvtvalue::String(devnode) = &devnode_pair.value else {
                return None;
            };
            let devpath = if devnode.starts_with('/') {
                devnode.to_string()
            } else {
                format!("/dev/{devnode}")
            };
            if !devpath.starts_with("/dev/dsp") || devpath.contains(".ctl") {
                return None;
            }
            let label_prefix = nvtree_find(dsp, "desc")
                .and_then(|pair| match &pair.value {
                    Nvtvalue::String(s) if !s.is_empty() => Some(s.clone()),
                    _ => None,
                })
                .or_else(|| {
                    nvtree_find(dsp, "nameunit").and_then(|pair| match &pair.value {
                        Nvtvalue::String(s) if !s.is_empty() => Some(s.clone()),
                        _ => None,
                    })
                });
            let label = label_prefix
                .map(|prefix| format!("{prefix} ({devpath})"))
                .unwrap_or_else(|| devpath.clone());
            let mut supported_bits = decode_supported_bits_from_dsp(dsp);
            if supported_bits.is_empty() {
                supported_bits = probe_oss_supported_bits(&devpath);
            }
            let mut supported_sample_rates = decode_supported_sample_rates_from_dsp(dsp);
            if supported_sample_rates.is_empty() {
                supported_sample_rates = probe_oss_supported_sample_rates(&devpath);
            }
            let decoded_channels = decode_max_channels_from_dsp(dsp).unwrap_or(0);
            let info_channels = probe_oss_info(&devpath)
                .map(|info| info.max_channels())
                .unwrap_or(0);
            let probed_channels = probe_oss_max_channels(&devpath);
            let max_channels = decoded_channels.max(info_channels).max(probed_channels);
            let max_buffer_bytes = decode_max_buffer_bytes_from_dsp(dsp)
                .or_else(|| probe_oss_buffer_bytes(&devpath))
                .unwrap_or(0);
            let mut device = AudioDeviceDescriptor::new(
                devpath,
                label,
                supported_bits,
                supported_sample_rates,
                max_channels,
                max_buffer_bytes,
            );
            device.supports_output = dsp_supports_output(dsp);
            device.supports_input = dsp_supports_input(dsp);
            Some(device)
        })
        .collect::<Vec<_>>();
    (!out.is_empty()).then_some(out)
}

fn parse_usize_text(s: &str) -> Option<usize> {
    let trimmed = s.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return usize::from_str_radix(hex, 16).ok();
    }
    trimmed.parse::<usize>().ok()
}

fn usize_from_value(value: &Nvtvalue) -> Option<usize> {
    match value {
        Nvtvalue::Number(n) => usize::try_from(*n).ok(),
        Nvtvalue::String(s) => parse_usize_text(s),
        Nvtvalue::NumberArray(arr) => arr.iter().filter_map(|n| usize::try_from(*n).ok()).max(),
        Nvtvalue::StringArray(arr) => arr.iter().filter_map(|s| parse_usize_text(s)).max(),
        _ => None,
    }
}

fn max_value_from_tree(tree: &Nvtree, keys: &[&str]) -> Option<usize> {
    keys.iter()
        .filter_map(|key| nvtree_find(tree, key).and_then(|pair| usize_from_value(&pair.value)))
        .max()
}

fn max_value_from_dsp(dsp: &Nvtree, keys: &[&str]) -> Option<usize> {
    let mut max_value = max_value_from_tree(dsp, keys);
    for nested_name in ["info_play", "info_rec"] {
        if let Some(pair) = nvtree_find(dsp, nested_name)
            && let Nvtvalue::Nested(nested) = &pair.value
            && let Some(value) = max_value_from_tree(nested, keys)
        {
            max_value = Some(max_value.map_or(value, |current| current.max(value)));
        }
    }
    max_value.filter(|value| *value > 0)
}

fn decode_max_channels_from_dsp(dsp: &Nvtree) -> Option<usize> {
    max_value_from_dsp(
        dsp,
        &[
            "max_channels",
            "channels",
            "nchannels",
            "playchannels",
            "recchannels",
            "playback_channels",
            "capture_channels",
        ],
    )
}

fn decode_max_buffer_bytes_from_dsp(dsp: &Nvtree) -> Option<usize> {
    max_value_from_dsp(
        dsp,
        &[
            "bytes",
            "bufsz",
            "buffer_size",
            "buffersize",
            "playbufsz",
            "recbufsz",
            "playback_buffer_size",
            "capture_buffer_size",
        ],
    )
}

fn bool_from_value(value: &Nvtvalue) -> Option<bool> {
    match value {
        Nvtvalue::Bool(b) => Some(*b),
        Nvtvalue::Number(n) => Some(*n != 0),
        Nvtvalue::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn dsp_supports_output(dsp: &Nvtree) -> bool {
    if let Some(pair) = nvtree_find(dsp, "play") {
        return bool_from_value(&pair.value).unwrap_or(false);
    }
    nvtree_find(dsp, "pchan")
        .and_then(|pair| usize_from_value(&pair.value))
        .map(|n| n > 0)
        .unwrap_or(false)
}

fn dsp_supports_input(dsp: &Nvtree) -> bool {
    if let Some(pair) = nvtree_find(dsp, "rec") {
        return bool_from_value(&pair.value).unwrap_or(false);
    }
    nvtree_find(dsp, "rchan")
        .and_then(|pair| usize_from_value(&pair.value))
        .map(|n| n > 0)
        .unwrap_or(false)
}

fn decode_supported_bits_from_dsp(dsp: &Nvtree) -> Vec<usize> {
    fn parse_number_text(s: &str) -> Option<u64> {
        let trimmed = s.trim();
        if let Some(hex) = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
        {
            return u64::from_str_radix(hex, 16).ok();
        }
        trimmed.parse::<u64>().ok()
    }
    fn format_mask_from_value(value: &Nvtvalue) -> Option<u64> {
        match value {
            Nvtvalue::Number(n) => Some(*n),
            Nvtvalue::String(s) => parse_number_text(s),
            Nvtvalue::NumberArray(arr) => Some(arr.iter().copied().fold(0_u64, |acc, n| acc | n)),
            Nvtvalue::StringArray(arr) => Some(
                arr.iter()
                    .filter_map(|s| parse_number_text(s))
                    .fold(0_u64, |acc, n| acc | n),
            ),
            _ => None,
        }
    }
    fn format_mask_from_tree(tree: &Nvtree) -> Option<u64> {
        for key in DIRECT_KEYS {
            if let Some(pair) = nvtree_find(tree, key)
                && let Some(mask) = format_mask_from_value(&pair.value)
            {
                return Some(mask);
            }
        }
        None
    }
    let mut mask = format_mask_from_tree(dsp).unwrap_or(0);
    for nested_name in ["info_play", "info_rec"] {
        if let Some(pair) = nvtree_find(dsp, nested_name)
            && let Nvtvalue::Nested(nested) = &pair.value
        {
            mask |= format_mask_from_tree(nested).unwrap_or(0);
        }
    }
    bits_from_format_mask(mask)
}

fn bits_from_format_mask(mask: u64) -> Vec<usize> {
    let mut bits = Vec::with_capacity(4);
    if (mask & (AFMT_S32_LE | AFMT_S32_BE)) != 0 {
        bits.push(32);
    }
    if (mask & (AFMT_S24_LE | AFMT_S24_BE)) != 0 {
        bits.push(24);
    }
    if (mask & (AFMT_S16_LE | AFMT_S16_BE)) != 0 {
        bits.push(16);
    }
    if (mask & AFMT_S8) != 0 {
        bits.push(8);
    }
    bits
}

fn decode_supported_sample_rates_from_dsp(dsp: &Nvtree) -> Vec<i32> {
    use std::collections::BTreeSet;
    let mut rates = BTreeSet::new();
    collect_rates_from_tree(dsp, &mut rates);
    for nested_name in ["info_play", "info_rec"] {
        if let Some(pair) = nvtree_find(dsp, nested_name)
            && let Nvtvalue::Nested(nested) = &pair.value
        {
            collect_rates_from_tree(nested, &mut rates);
        }
    }
    rates.into_iter().collect()
}

fn collect_rates_from_tree(tree: &Nvtree, rates: &mut std::collections::BTreeSet<i32>) {
    for key in RATE_KEYS {
        if let Some(pair) = nvtree_find(tree, key) {
            collect_rates_from_value(&pair.value, &mut *rates);
        }
    }
}

fn collect_rates_from_value(value: &Nvtvalue, rates: &mut std::collections::BTreeSet<i32>) {
    match value {
        Nvtvalue::Number(n) => {
            if let Ok(rate) = i32::try_from(*n)
                && rate > 0
            {
                rates.insert(rate);
            }
        }
        Nvtvalue::String(s) => {
            if let Ok(rate) = s.trim().parse::<i32>()
                && rate > 0
            {
                rates.insert(rate);
            }
        }
        Nvtvalue::NumberArray(arr) => {
            rates.extend(
                arr.iter()
                    .filter_map(|n| i32::try_from(*n).ok())
                    .filter(|rate| *rate > 0),
            );
        }
        Nvtvalue::StringArray(arr) => {
            rates.extend(
                arr.iter()
                    .filter_map(|s| s.trim().parse::<i32>().ok())
                    .filter(|rate| *rate > 0),
            );
        }
        _ => {}
    }
}

fn probe_oss_supported_bits(devpath: &str) -> Vec<usize> {
    let Ok(file) = std::fs::OpenOptions::new().read(true).open(devpath) else {
        return Vec::new();
    };
    let fd = file.as_raw_fd();
    let mut formats = 0_i32;
    if unsafe { oss_get_formats(fd, &mut formats).is_ok() } {
        bits_from_format_mask(formats as u64)
    } else {
        Vec::new()
    }
}

fn probe_oss_supported_sample_rates(devpath: &str) -> Vec<i32> {
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(devpath)
    else {
        return Vec::new();
    };
    let fd = file.as_raw_fd();
    let mut supported = std::collections::BTreeSet::new();
    for candidate in SAMPLE_RATE_CANDIDATES {
        let mut rate = candidate;
        if unsafe { oss_set_speed(fd, &mut rate).is_ok() } && rate > 0 {
            supported.insert(rate);
        }
    }
    supported.into_iter().collect()
}

fn probe_oss_info(devpath: &str) -> Option<AudioInfoProbe> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(devpath)
        .or_else(|_| std::fs::OpenOptions::new().read(true).open(devpath))
        .ok()?;
    let fd = file.as_raw_fd();
    let mut info = AudioInfoProbe::new();
    unsafe { oss_get_info(fd, &mut info).ok()? };
    Some(info)
}

fn probe_oss_max_channels(devpath: &str) -> usize {
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(devpath)
    else {
        return 0;
    };
    let fd = file.as_raw_fd();
    let mut best = 0_usize;
    for candidate in [64_i32, 32, 24, 16, 12, 10, 8, 6, 4, 2, 1] {
        let mut channels = candidate;
        if unsafe { oss_set_channels(fd, &mut channels) }.is_ok()
            && let Ok(channels) = usize::try_from(channels.max(0))
        {
            best = best.max(channels);
            if best >= candidate as usize {
                break;
            }
        }
    }
    best
}

fn probe_oss_buffer_bytes(devpath: &str) -> Option<usize> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(devpath)
        .or_else(|_| std::fs::OpenOptions::new().read(true).open(devpath))
        .ok()?;
    let fd = file.as_raw_fd();
    let mut request = ((0xffff_u32 << 16) | 16) as i32;
    let _ = unsafe { oss_set_fragment(fd, &mut request) };
    let mut output = BufferInfoProbe::new();
    let mut input = BufferInfoProbe::new();
    let output_bytes =
        unsafe { oss_output_buffer_info(fd, &mut output).ok() }.and_then(|_| output.bytes_total());
    let input_bytes =
        unsafe { oss_input_buffer_info(fd, &mut input).ok() }.and_then(|_| input.bytes_total());
    output_bytes.into_iter().chain(input_bytes).max()
}

#[repr(C)]
struct AudioInfoProbe {
    dev: libc::c_int,
    name: [libc::c_char; 64],
    busy: libc::c_int,
    pid: libc::c_int,
    caps: libc::c_int,
    iformats: libc::c_int,
    oformats: libc::c_int,
    magic: libc::c_int,
    cmd: [libc::c_char; 64],
    card_number: libc::c_int,
    port_number: libc::c_int,
    mixer_dev: libc::c_int,
    legacy_device: libc::c_int,
    enabled: libc::c_int,
    flags: libc::c_int,
    min_rate: libc::c_int,
    max_rate: libc::c_int,
    min_channels: libc::c_int,
    max_channels: libc::c_int,
    binding: libc::c_int,
    rate_source: libc::c_int,
    handle: [libc::c_char; 32],
    nrates: libc::c_uint,
    rates: [libc::c_uint; 20],
    song_name: [libc::c_char; 64],
    label: [libc::c_char; 16],
    latency: libc::c_int,
    devnode: [libc::c_char; 32],
    next_play_engine: libc::c_int,
    next_rec_engine: libc::c_int,
    filler: [libc::c_int; 184],
}

impl AudioInfoProbe {
    fn new() -> Self {
        Self {
            dev: 0,
            name: [0; 64],
            busy: 0,
            pid: 0,
            caps: 0,
            iformats: 0,
            oformats: 0,
            magic: 0,
            cmd: [0; 64],
            card_number: 0,
            port_number: 0,
            mixer_dev: 0,
            legacy_device: 0,
            enabled: 0,
            flags: 0,
            min_rate: 0,
            max_rate: 0,
            min_channels: 0,
            max_channels: 0,
            binding: 0,
            rate_source: 0,
            handle: [0; 32],
            nrates: 0,
            rates: [0; 20],
            song_name: [0; 64],
            label: [0; 16],
            latency: 0,
            devnode: [0; 32],
            next_play_engine: 0,
            next_rec_engine: 0,
            filler: [0; 184],
        }
    }

    fn max_channels(&self) -> usize {
        usize::try_from(self.max_channels.max(0)).unwrap_or(0)
    }
}

#[repr(C)]
struct BufferInfoProbe {
    fragments: libc::c_int,
    fragstotal: libc::c_int,
    fragsize: libc::c_int,
    bytes: libc::c_int,
}

impl BufferInfoProbe {
    fn new() -> Self {
        Self {
            fragments: 0,
            fragstotal: 0,
            fragsize: 0,
            bytes: 0,
        }
    }

    fn bytes_total(&self) -> Option<usize> {
        if self.bytes > 0 {
            return usize::try_from(self.bytes).ok();
        }
        if self.fragstotal > 0 && self.fragsize > 0 {
            let total = i64::from(self.fragstotal) * i64::from(self.fragsize);
            return usize::try_from(total).ok();
        }
        None
    }
}

#[repr(C)]
struct SndstIoctlNvArg {
    nbytes: usize,
    buf: *mut c_void,
}

nix::ioctl_none!(sndst_refresh_devs, b'D', 100);
nix::ioctl_readwrite!(sndst_get_devs, b'D', 101, SndstIoctlNvArg);
nix::ioctl_read!(oss_get_formats, b'P', 11, i32);
nix::ioctl_read!(oss_output_buffer_info, b'P', 12, BufferInfoProbe);
nix::ioctl_read!(oss_input_buffer_info, b'P', 13, BufferInfoProbe);
nix::ioctl_readwrite!(oss_set_fragment, b'P', 10, i32);
nix::ioctl_readwrite!(oss_set_speed, b'P', 2, i32);
nix::ioctl_readwrite!(oss_set_channels, b'P', 6, i32);
nix::ioctl_readwrite!(oss_get_info, b'X', 12, AudioInfoProbe);
