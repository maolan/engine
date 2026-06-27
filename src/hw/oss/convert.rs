use super::consts::*;
use crate::audio::io::AudioIO;
use std::sync::Arc;

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
use std::arch::x86_64::*;

pub(super) fn bytes_per_sample(format: u32) -> Option<usize> {
    match format {
        AFMT_S16_LE | AFMT_S16_BE => Some(2),
        AFMT_S24_LE | AFMT_S24_BE => Some(3),
        AFMT_S32_LE | AFMT_S32_BE => Some(4),
        AFMT_S8 => Some(1),
        _ => None,
    }
}

pub(super) fn supported_sample_format(format: u32) -> bool {
    matches!(
        format,
        AFMT_S16_LE | AFMT_S16_BE | AFMT_S24_LE | AFMT_S24_BE | AFMT_S32_LE | AFMT_S32_BE | AFMT_S8
    )
}

pub(super) fn convert_in_to_i32_interleaved(
    format: u32,
    channels: usize,
    frames: usize,
    src: &[u8],
    dst: &mut [i32],
) {
    if format == AFMT_S32_NE {
        let samples = frames
            .saturating_mul(channels)
            .min(dst.len())
            .min(src.len() / 4);
        let bytes = samples * 4;
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr().cast::<u8>(), bytes);
        }
        return;
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if format == AFMT_S16_LE && is_x86_feature_detected!("sse4.1") {
            let n = frames.saturating_mul(channels).min(dst.len());
            let mut i = 0;
            while i + 8 <= n {
                let bytes = _mm_loadu_si128(src.as_ptr().add(i * 2) as *const __m128i);
                let low = _mm_slli_epi32(_mm_cvtepi16_epi32(bytes), 16);
                let high = _mm_slli_epi32(_mm_cvtepi16_epi32(_mm_srli_si128(bytes, 8)), 16);
                _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, low);
                _mm_storeu_si128(dst.as_mut_ptr().add(i + 4) as *mut __m128i, high);
                i += 8;
            }
            for (j, d) in dst[i..n].iter_mut().enumerate() {
                let o = (i + j) * 2;
                *d = i16::from_le_bytes([src[o], src[o + 1]]) as i32 * 65536;
            }
            return;
        }
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if format == AFMT_S24_LE && is_x86_feature_detected!("ssse3") {
            let n = frames.saturating_mul(channels).min(dst.len());
            let mut i = 0;
            while i + 4 <= n {
                let bytes = _mm_loadu_si128(src.as_ptr().add(i * 3) as *const __m128i);
                let shuffle = _mm_set_epi8(-1, 11, 10, 9, -1, 8, 7, 6, -1, 5, 4, 3, -1, 2, 1, 0);
                let unpacked = _mm_shuffle_epi8(bytes, shuffle);
                let extended = _mm_srai_epi32(_mm_slli_epi32(unpacked, 8), 8);
                _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, extended);
                i += 4;
            }
            for (j, d) in dst[i..n].iter_mut().enumerate() {
                let o = (i + j) * 3;
                let v = ((src[o + 2] as i32) << 24)
                    | ((src[o + 1] as i32) << 16)
                    | ((src[o] as i32) << 8);
                *d = v >> 8;
            }
            return;
        }
    }

    let bps = bytes_per_sample(format).unwrap_or(4);
    let n = frames.saturating_mul(channels);
    for i in 0..n.min(dst.len()) {
        let o = i * bps;
        dst[i] = match format {
            AFMT_S16_LE => i16::from_le_bytes([src[o], src[o + 1]]) as i32 * 65536,
            AFMT_S16_BE => i16::from_be_bytes([src[o], src[o + 1]]) as i32 * 65536,
            AFMT_S24_LE => {
                let v = ((src[o + 2] as i32) << 24)
                    | ((src[o + 1] as i32) << 16)
                    | ((src[o] as i32) << 8);
                v >> 8
            }
            AFMT_S24_BE => {
                let v = ((src[o] as i32) << 24)
                    | ((src[o + 1] as i32) << 16)
                    | ((src[o + 2] as i32) << 8);
                v >> 8
            }
            AFMT_S32_LE => i32::from_le_bytes([src[o], src[o + 1], src[o + 2], src[o + 3]]),
            AFMT_S32_BE => i32::from_be_bytes([src[o], src[o + 1], src[o + 2], src[o + 3]]),
            AFMT_S8 => (src[o] as i8 as i32) << 24,
            _ => 0,
        };
    }
}

pub(super) fn convert_in_to_i32_connected(
    format: u32,
    frames: usize,
    src: &[u8],
    dst: &mut [i32],
    channels: &[Arc<AudioIO>],
) {
    if channels.iter().all(crate::hw::ports::has_audio_connections) {
        convert_in_to_i32_interleaved(format, channels.len(), frames, src, dst);
        return;
    }
    let bps = bytes_per_sample(format).unwrap_or(4);
    let channel_count = channels.len();
    for (ch, port) in channels.iter().enumerate() {
        if !crate::hw::ports::has_audio_connections(port) {
            continue;
        }
        for frame in 0..frames {
            let i = frame * channel_count + ch;
            if i >= dst.len() {
                continue;
            }
            let o = i * bps;
            dst[i] = match format {
                AFMT_S16_LE => i16::from_le_bytes([src[o], src[o + 1]]) as i32 * 65536,
                AFMT_S16_BE => i16::from_be_bytes([src[o], src[o + 1]]) as i32 * 65536,
                AFMT_S24_LE => {
                    let v = ((src[o + 2] as i32) << 24)
                        | ((src[o + 1] as i32) << 16)
                        | ((src[o] as i32) << 8);
                    v >> 8
                }
                AFMT_S24_BE => {
                    let v = ((src[o] as i32) << 24)
                        | ((src[o + 1] as i32) << 16)
                        | ((src[o + 2] as i32) << 8);
                    v >> 8
                }
                AFMT_S32_LE => i32::from_le_bytes([src[o], src[o + 1], src[o + 2], src[o + 3]]),
                AFMT_S32_BE => i32::from_be_bytes([src[o], src[o + 1], src[o + 2], src[o + 3]]),
                AFMT_S8 => (src[o] as i8 as i32) << 24,
                _ => 0,
            };
        }
    }
}

pub(super) fn convert_out_from_i32_interleaved(
    format: u32,
    channels: usize,
    frames: usize,
    src: &mut [i32],
    dst: &mut [u8],
) {
    if format == AFMT_S32_NE {
        let samples = frames
            .saturating_mul(channels)
            .min(src.len())
            .min(dst.len() / 4);
        let bytes = samples * 4;
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr().cast::<u8>(), dst.as_mut_ptr(), bytes);
        }
        return;
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if format == AFMT_S16_LE && is_x86_feature_detected!("sse2") {
            let n = frames.saturating_mul(channels).min(src.len());
            let mut i = 0;
            while i + 8 <= n {
                let low32 = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
                let high32 = _mm_loadu_si128(src.as_ptr().add(i + 4) as *const __m128i);
                let low16 = _mm_srai_epi32(low32, 16);
                let high16 = _mm_srai_epi32(high32, 16);
                let packed = _mm_packs_epi32(low16, high16);
                _mm_storeu_si128(dst.as_mut_ptr().add(i * 2) as *mut __m128i, packed);
                i += 8;
            }
            for (j, s) in src[i..n].iter().enumerate() {
                let o = (i + j) * 2;
                let v = (*s >> 16) as i16;
                dst[o..o + 2].copy_from_slice(&v.to_le_bytes());
            }
            return;
        }
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if format == AFMT_S24_LE && is_x86_feature_detected!("ssse3") {
            let n = frames.saturating_mul(channels).min(src.len());
            let mut i = 0;
            while i + 4 <= n {
                let s = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
                let shifted = _mm_srai_epi32(s, 8);
                let shuffle = _mm_set_epi8(-1, -1, -1, -1, 14, 13, 12, 10, 9, 8, 6, 5, 4, 2, 1, 0);
                let packed = _mm_shuffle_epi8(shifted, shuffle);
                _mm_storeu_si128(dst.as_mut_ptr().add(i * 3) as *mut __m128i, packed);
                i += 4;
            }
            for (j, s) in src[i..n].iter().enumerate() {
                let o = (i + j) * 3;
                let v = s >> 8;
                dst[o] = v as u8;
                dst[o + 1] = (v >> 8) as u8;
                dst[o + 2] = (v >> 16) as u8;
            }
            return;
        }
    }

    let bps = bytes_per_sample(format).unwrap_or(4);
    let n = frames.saturating_mul(channels);
    for (i, _item) in src.iter().enumerate().take(n.min(src.len())) {
        let o = i * bps;
        let s = src[i];
        match format {
            AFMT_S16_LE => {
                let v = (s >> 16) as i16;
                dst[o..o + 2].copy_from_slice(&v.to_le_bytes());
            }
            AFMT_S16_BE => {
                let v = (s >> 16) as i16;
                dst[o..o + 2].copy_from_slice(&v.to_be_bytes());
            }
            AFMT_S24_LE => {
                let v = s >> 8;
                dst[o] = v as u8;
                dst[o + 1] = (v >> 8) as u8;
                dst[o + 2] = (v >> 16) as u8;
            }
            AFMT_S24_BE => {
                let v = s >> 8;
                dst[o] = (v >> 16) as u8;
                dst[o + 1] = (v >> 8) as u8;
                dst[o + 2] = v as u8;
            }
            AFMT_S32_LE => {
                dst[o..o + 4].copy_from_slice(&s.to_le_bytes());
            }
            AFMT_S32_BE => {
                dst[o..o + 4].copy_from_slice(&s.to_be_bytes());
            }
            AFMT_S8 => {
                dst[o] = (s >> 24) as i8 as u8;
            }
            _ => {}
        }
    }
}
