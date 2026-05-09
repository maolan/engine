//! Portable SIMD helper routines for buffer math.
//!
//! Uses runtime CPU feature detection to dispatch:
//! - AVX (`f32x8`) on x86_64/x86 when available
//! - SSE (`wide::f32x4`) as fallback on x86_64/x86
//! - Scalar loops on all other architectures

#![allow(unsafe_op_in_unsafe_fn)]

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
mod x86 {
    pub use std::arch::x86_64::*;
    pub use wide::f32x4;
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
use x86::*;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// dst[i] += src[i]
pub fn add_inplace(dst: &mut [f32], src: &[f32]) {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx") {
            add_inplace_avx(dst, src);
            return;
        }
        if is_x86_feature_detected!("sse") {
            add_inplace_sse(dst, src);
            return;
        }
    }
    add_inplace_scalar(dst, src);
}

/// dst[i] *= gain
pub fn mul_inplace(dst: &mut [f32], gain: f32) {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx") {
            mul_inplace_avx(dst, gain);
            return;
        }
        if is_x86_feature_detected!("sse") {
            mul_inplace_sse(dst, gain);
            return;
        }
    }
    mul_inplace_scalar(dst, gain);
}

/// dst[i] += src[i] * gain
pub fn add_scaled_inplace(dst: &mut [f32], src: &[f32], gain: f32) {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx") {
            add_scaled_inplace_avx(dst, src, gain);
            return;
        }
        if is_x86_feature_detected!("sse") {
            add_scaled_inplace_sse(dst, src, gain);
            return;
        }
    }
    add_scaled_inplace_scalar(dst, src, gain);
}

/// dst[i] = src[i] * gain
pub fn copy_scaled_inplace(dst: &mut [f32], src: &[f32], gain: f32) {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx") {
            copy_scaled_inplace_avx(dst, src, gain);
            return;
        }
        if is_x86_feature_detected!("sse") {
            copy_scaled_inplace_sse(dst, src, gain);
            return;
        }
    }
    copy_scaled_inplace_scalar(dst, src, gain);
}

/// dst[i] += sanitize_finite(src[i])
pub fn add_sanitized_inplace(dst: &mut [f32], src: &[f32]) {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx") {
            add_sanitized_inplace_avx(dst, src);
            return;
        }
        if is_x86_feature_detected!("sse") {
            add_sanitized_inplace_sse(dst, src);
            return;
        }
    }
    add_sanitized_inplace_scalar(dst, src);
}

/// dst[i] = sanitize_finite(src[i])
pub fn copy_sanitized_inplace(dst: &mut [f32], src: &[f32]) {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx") {
            copy_sanitized_inplace_avx(dst, src);
            return;
        }
        if is_x86_feature_detected!("sse") {
            copy_sanitized_inplace_sse(dst, src);
            return;
        }
    }
    copy_sanitized_inplace_scalar(dst, src);
}

/// Replace NaN / ±Inf with 0.0 in place.
pub fn sanitize_finite_inplace(buf: &mut [f32]) {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx") {
            sanitize_finite_inplace_avx(buf);
            return;
        }
        if is_x86_feature_detected!("sse") {
            sanitize_finite_inplace_sse(buf);
            return;
        }
    }
    sanitize_finite_inplace_scalar(buf);
}

/// Horizontal max of abs(buf[i]).
pub fn peak_abs(buf: &[f32]) -> f32 {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx") {
            return peak_abs_avx(buf);
        }
        if is_x86_feature_detected!("sse") {
            return peak_abs_sse(buf);
        }
    }
    peak_abs_scalar(buf)
}

/// Clamp every element to [min, max].
pub fn clamp_inplace(buf: &mut [f32], min: f32, max: f32) {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx") {
            clamp_inplace_avx(buf, min, max);
            return;
        }
        if is_x86_feature_detected!("sse") {
            clamp_inplace_sse(buf, min, max);
            return;
        }
    }
    clamp_inplace_scalar(buf, min, max);
}

// ---------------------------------------------------------------------------
// Scalar fallbacks
// ---------------------------------------------------------------------------

fn add_inplace_scalar(dst: &mut [f32], src: &[f32]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d += *s;
    }
}

fn mul_inplace_scalar(dst: &mut [f32], gain: f32) {
    for d in dst.iter_mut() {
        *d *= gain;
    }
}

fn add_scaled_inplace_scalar(dst: &mut [f32], src: &[f32], gain: f32) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d += *s * gain;
    }
}

fn copy_scaled_inplace_scalar(dst: &mut [f32], src: &[f32], gain: f32) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d = *s * gain;
    }
}

fn add_sanitized_inplace_scalar(dst: &mut [f32], src: &[f32]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d += if s.is_finite() { *s } else { 0.0 };
    }
}

fn copy_sanitized_inplace_scalar(dst: &mut [f32], src: &[f32]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d = if s.is_finite() { *s } else { 0.0 };
    }
}

fn sanitize_finite_inplace_scalar(buf: &mut [f32]) {
    for s in buf.iter_mut() {
        if !s.is_finite() {
            *s = 0.0;
        }
    }
}

fn peak_abs_scalar(buf: &[f32]) -> f32 {
    buf.iter().fold(0.0f32, |acc, s| acc.max(s.abs()))
}

fn clamp_inplace_scalar(buf: &mut [f32], min: f32, max: f32) {
    for s in buf.iter_mut() {
        *s = s.clamp(min, max);
    }
}

// ---------------------------------------------------------------------------
// SSE paths (wide::f32x4)
// ---------------------------------------------------------------------------

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn add_inplace_sse(dst: &mut [f32], src: &[f32]) {
    let len = dst.len().min(src.len());
    let dst_head = &mut dst[..len];
    let src_head = &src[..len];
    let n = dst_head.len() / 4;
    for i in 0..n {
        let d_chunk = &mut dst_head[i * 4..(i + 1) * 4];
        let s_chunk = &src_head[i * 4..(i + 1) * 4];
        let d: f32x4 = [d_chunk[0], d_chunk[1], d_chunk[2], d_chunk[3]].into();
        let s: f32x4 = [s_chunk[0], s_chunk[1], s_chunk[2], s_chunk[3]].into();
        let r = d + s;
        d_chunk.copy_from_slice(&r.to_array());
    }
    for (d, s) in dst_head[n * 4..].iter_mut().zip(src_head[n * 4..].iter()) {
        *d += *s;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn mul_inplace_sse(dst: &mut [f32], gain: f32) {
    let n = dst.len() / 4;
    let g: f32x4 = [gain; 4].into();
    for i in 0..n {
        let d_chunk = &mut dst[i * 4..(i + 1) * 4];
        let d: f32x4 = [d_chunk[0], d_chunk[1], d_chunk[2], d_chunk[3]].into();
        let r = d * g;
        d_chunk.copy_from_slice(&r.to_array());
    }
    for d in &mut dst[n * 4..] {
        *d *= gain;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn add_scaled_inplace_sse(dst: &mut [f32], src: &[f32], gain: f32) {
    let len = dst.len().min(src.len());
    let dst_head = &mut dst[..len];
    let src_head = &src[..len];
    let n = dst_head.len() / 4;
    let g: f32x4 = [gain; 4].into();
    for i in 0..n {
        let d_chunk = &mut dst_head[i * 4..(i + 1) * 4];
        let s_chunk = &src_head[i * 4..(i + 1) * 4];
        let d: f32x4 = [d_chunk[0], d_chunk[1], d_chunk[2], d_chunk[3]].into();
        let s: f32x4 = [s_chunk[0], s_chunk[1], s_chunk[2], s_chunk[3]].into();
        let r = d + s * g;
        d_chunk.copy_from_slice(&r.to_array());
    }
    for (d, s) in dst_head[n * 4..].iter_mut().zip(src_head[n * 4..].iter()) {
        *d += *s * gain;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn copy_scaled_inplace_sse(dst: &mut [f32], src: &[f32], gain: f32) {
    let len = dst.len().min(src.len());
    let dst_head = &mut dst[..len];
    let src_head = &src[..len];
    let n = dst_head.len() / 4;
    let g: f32x4 = [gain; 4].into();
    for i in 0..n {
        let d_chunk = &mut dst_head[i * 4..(i + 1) * 4];
        let s_chunk = &src_head[i * 4..(i + 1) * 4];
        let s: f32x4 = [s_chunk[0], s_chunk[1], s_chunk[2], s_chunk[3]].into();
        let r = s * g;
        d_chunk.copy_from_slice(&r.to_array());
    }
    for (d, s) in dst_head[n * 4..].iter_mut().zip(src_head[n * 4..].iter()) {
        *d = *s * gain;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn add_sanitized_inplace_sse(dst: &mut [f32], src: &[f32]) {
    let len = dst.len().min(src.len());
    let dst_head = &mut dst[..len];
    let src_head = &src[..len];
    let n = dst_head.len() / 4;
    let zero: f32x4 = [0.0; 4].into();
    for i in 0..n {
        let d_chunk = &mut dst_head[i * 4..(i + 1) * 4];
        let s_chunk = &src_head[i * 4..(i + 1) * 4];
        let d: f32x4 = [d_chunk[0], d_chunk[1], d_chunk[2], d_chunk[3]].into();
        let s: f32x4 = [s_chunk[0], s_chunk[1], s_chunk[2], s_chunk[3]].into();
        let mask = s.is_finite();
        let sanitized = mask.blend(s, zero);
        let r = d + sanitized;
        d_chunk.copy_from_slice(&r.to_array());
    }
    for (d, s) in dst_head[n * 4..].iter_mut().zip(src_head[n * 4..].iter()) {
        *d += if s.is_finite() { *s } else { 0.0 };
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn copy_sanitized_inplace_sse(dst: &mut [f32], src: &[f32]) {
    let len = dst.len().min(src.len());
    let dst_head = &mut dst[..len];
    let src_head = &src[..len];
    let n = dst_head.len() / 4;
    let zero: f32x4 = [0.0; 4].into();
    for i in 0..n {
        let d_chunk = &mut dst_head[i * 4..(i + 1) * 4];
        let s_chunk = &src_head[i * 4..(i + 1) * 4];
        let s: f32x4 = [s_chunk[0], s_chunk[1], s_chunk[2], s_chunk[3]].into();
        let mask = s.is_finite();
        let r = mask.blend(s, zero);
        d_chunk.copy_from_slice(&r.to_array());
    }
    for (d, s) in dst_head[n * 4..].iter_mut().zip(src_head[n * 4..].iter()) {
        *d = if s.is_finite() { *s } else { 0.0 };
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn sanitize_finite_inplace_sse(buf: &mut [f32]) {
    let n = buf.len() / 4;
    let zero: f32x4 = [0.0; 4].into();
    for i in 0..n {
        let chunk = &mut buf[i * 4..(i + 1) * 4];
        let v: f32x4 = [chunk[0], chunk[1], chunk[2], chunk[3]].into();
        let mask = v.is_finite();
        let r = mask.blend(v, zero);
        chunk.copy_from_slice(&r.to_array());
    }
    for s in &mut buf[n * 4..] {
        if !s.is_finite() {
            *s = 0.0;
        }
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn peak_abs_sse(buf: &[f32]) -> f32 {
    let n = buf.len() / 4;
    let mut peak: f32x4 = [0.0; 4].into();
    for i in 0..n {
        let chunk = &buf[i * 4..(i + 1) * 4];
        let v: f32x4 = [chunk[0], chunk[1], chunk[2], chunk[3]].into();
        peak = peak.max(v.abs());
    }
    let mut max_scalar = peak.to_array().into_iter().fold(0.0f32, |a, b| a.max(b));
    for s in &buf[n * 4..] {
        max_scalar = max_scalar.max(s.abs());
    }
    max_scalar
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn clamp_inplace_sse(buf: &mut [f32], min: f32, max: f32) {
    let n = buf.len() / 4;
    let vmin: f32x4 = [min; 4].into();
    let vmax: f32x4 = [max; 4].into();
    for i in 0..n {
        let chunk = &mut buf[i * 4..(i + 1) * 4];
        let v: f32x4 = [chunk[0], chunk[1], chunk[2], chunk[3]].into();
        let r = v.clamp(vmin, vmax);
        chunk.copy_from_slice(&r.to_array());
    }
    for s in &mut buf[n * 4..] {
        *s = s.clamp(min, max);
    }
}

// ---------------------------------------------------------------------------
// AVX paths (std::arch::x86_64::__m256)
// ---------------------------------------------------------------------------

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx")]
unsafe fn add_inplace_avx(dst: &mut [f32], src: &[f32]) {
    let len = dst.len().min(src.len());
    let dst_head = &mut dst[..len];
    let src_head = &src[..len];
    let mut i = 0;
    while i + 8 <= dst_head.len() {
        let d = _mm256_loadu_ps(dst_head.as_ptr().add(i));
        let s = _mm256_loadu_ps(src_head.as_ptr().add(i));
        let r = _mm256_add_ps(d, s);
        _mm256_storeu_ps(dst_head.as_mut_ptr().add(i), r);
        i += 8;
    }
    for (d, s) in dst_head[i..].iter_mut().zip(src_head[i..].iter()) {
        *d += *s;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx")]
unsafe fn mul_inplace_avx(dst: &mut [f32], gain: f32) {
    let g = _mm256_set1_ps(gain);
    let mut i = 0;
    while i + 8 <= dst.len() {
        let d = _mm256_loadu_ps(dst.as_ptr().add(i));
        let r = _mm256_mul_ps(d, g);
        _mm256_storeu_ps(dst.as_mut_ptr().add(i), r);
        i += 8;
    }
    for d in &mut dst[i..] {
        *d *= gain;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx")]
unsafe fn add_scaled_inplace_avx(dst: &mut [f32], src: &[f32], gain: f32) {
    let len = dst.len().min(src.len());
    let dst_head = &mut dst[..len];
    let src_head = &src[..len];
    let g = _mm256_set1_ps(gain);
    let mut i = 0;
    while i + 8 <= dst_head.len() {
        let d = _mm256_loadu_ps(dst_head.as_ptr().add(i));
        let s = _mm256_loadu_ps(src_head.as_ptr().add(i));
        let r = _mm256_add_ps(d, _mm256_mul_ps(s, g));
        _mm256_storeu_ps(dst_head.as_mut_ptr().add(i), r);
        i += 8;
    }
    for (d, s) in dst_head[i..].iter_mut().zip(src_head[i..].iter()) {
        *d += *s * gain;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx")]
unsafe fn copy_scaled_inplace_avx(dst: &mut [f32], src: &[f32], gain: f32) {
    let len = dst.len().min(src.len());
    let dst_head = &mut dst[..len];
    let src_head = &src[..len];
    let g = _mm256_set1_ps(gain);
    let mut i = 0;
    while i + 8 <= dst_head.len() {
        let s = _mm256_loadu_ps(src_head.as_ptr().add(i));
        let r = _mm256_mul_ps(s, g);
        _mm256_storeu_ps(dst_head.as_mut_ptr().add(i), r);
        i += 8;
    }
    for (d, s) in dst_head[i..].iter_mut().zip(src_head[i..].iter()) {
        *d = *s * gain;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx")]
unsafe fn add_sanitized_inplace_avx(dst: &mut [f32], src: &[f32]) {
    let len = dst.len().min(src.len());
    let dst_head = &mut dst[..len];
    let src_head = &src[..len];
    let zero = _mm256_setzero_ps();
    let max_val = _mm256_set1_ps(f32::MAX);
    let mut i = 0;
    while i + 8 <= dst_head.len() {
        let d = _mm256_loadu_ps(dst_head.as_ptr().add(i));
        let s = _mm256_loadu_ps(src_head.as_ptr().add(i));
        let abs_s = _mm256_andnot_ps(_mm256_set1_ps(-0.0), s);
        let mask = _mm256_cmp_ps(abs_s, max_val, _CMP_LE_OQ);
        let sanitized = _mm256_blendv_ps(zero, s, mask);
        let r = _mm256_add_ps(d, sanitized);
        _mm256_storeu_ps(dst_head.as_mut_ptr().add(i), r);
        i += 8;
    }
    for (d, s) in dst_head[i..].iter_mut().zip(src_head[i..].iter()) {
        *d += if s.is_finite() { *s } else { 0.0 };
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx")]
unsafe fn copy_sanitized_inplace_avx(dst: &mut [f32], src: &[f32]) {
    let len = dst.len().min(src.len());
    let dst_head = &mut dst[..len];
    let src_head = &src[..len];
    let zero = _mm256_setzero_ps();
    let max_val = _mm256_set1_ps(f32::MAX);
    let mut i = 0;
    while i + 8 <= dst_head.len() {
        let s = _mm256_loadu_ps(src_head.as_ptr().add(i));
        let abs_s = _mm256_andnot_ps(_mm256_set1_ps(-0.0), s);
        let mask = _mm256_cmp_ps(abs_s, max_val, _CMP_LE_OQ);
        let r = _mm256_blendv_ps(zero, s, mask);
        _mm256_storeu_ps(dst_head.as_mut_ptr().add(i), r);
        i += 8;
    }
    for (d, s) in dst_head[i..].iter_mut().zip(src_head[i..].iter()) {
        *d = if s.is_finite() { *s } else { 0.0 };
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx")]
unsafe fn sanitize_finite_inplace_avx(buf: &mut [f32]) {
    let zero = _mm256_setzero_ps();
    let max_val = _mm256_set1_ps(f32::MAX);
    let mut i = 0;
    while i + 8 <= buf.len() {
        let v = _mm256_loadu_ps(buf.as_ptr().add(i));
        let abs_v = _mm256_andnot_ps(_mm256_set1_ps(-0.0), v);
        let mask = _mm256_cmp_ps(abs_v, max_val, _CMP_LE_OQ);
        let r = _mm256_blendv_ps(zero, v, mask);
        _mm256_storeu_ps(buf.as_mut_ptr().add(i), r);
        i += 8;
    }
    for s in &mut buf[i..] {
        if !s.is_finite() {
            *s = 0.0;
        }
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx")]
unsafe fn peak_abs_avx(buf: &[f32]) -> f32 {
    let mut peak = _mm256_setzero_ps();
    let mut i = 0;
    while i + 8 <= buf.len() {
        let v = _mm256_loadu_ps(buf.as_ptr().add(i));
        let abs_v = _mm256_andnot_ps(_mm256_set1_ps(-0.0), v);
        peak = _mm256_max_ps(peak, abs_v);
        i += 8;
    }
    let mut arr = [0.0f32; 8];
    _mm256_storeu_ps(arr.as_mut_ptr(), peak);
    let mut max_scalar = arr.iter().fold(0.0f32, |a, &b| a.max(b));
    for s in &buf[i..] {
        max_scalar = max_scalar.max(s.abs());
    }
    max_scalar
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx")]
unsafe fn clamp_inplace_avx(buf: &mut [f32], min: f32, max: f32) {
    let vmin = _mm256_set1_ps(min);
    let vmax = _mm256_set1_ps(max);
    let mut i = 0;
    while i + 8 <= buf.len() {
        let v = _mm256_loadu_ps(buf.as_ptr().add(i));
        let r = _mm256_max_ps(vmin, _mm256_min_ps(vmax, v));
        _mm256_storeu_ps(buf.as_mut_ptr().add(i), r);
        i += 8;
    }
    for s in &mut buf[i..] {
        *s = s.clamp(min, max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_inplace_basic() {
        let mut a = [1.0f32, 2.0, 3.0, 4.0, 5.0];
        let b = [10.0f32, 20.0, 30.0, 40.0, 50.0];
        add_inplace(&mut a, &b);
        assert_eq!(a, [11.0, 22.0, 33.0, 44.0, 55.0]);
    }

    #[test]
    fn mul_inplace_basic() {
        let mut a = [1.0f32, 2.0, 3.0, 4.0, 5.0];
        mul_inplace(&mut a, 2.0);
        assert_eq!(a, [2.0, 4.0, 6.0, 8.0, 10.0]);
    }

    #[test]
    fn add_scaled_inplace_basic() {
        let mut a = [1.0f32, 2.0, 3.0, 4.0, 5.0];
        let b = [10.0f32, 20.0, 30.0, 40.0, 50.0];
        add_scaled_inplace(&mut a, &b, 0.5);
        assert_eq!(a, [6.0, 12.0, 18.0, 24.0, 30.0]);
    }

    #[test]
    fn copy_scaled_inplace_basic() {
        let mut a = [0.0f32; 5];
        let b = [10.0f32, 20.0, 30.0, 40.0, 50.0];
        copy_scaled_inplace(&mut a, &b, 0.5);
        assert_eq!(a, [5.0, 10.0, 15.0, 20.0, 25.0]);
    }

    #[test]
    fn add_sanitized_inplace_basic() {
        let mut a = [1.0f32, 2.0, 3.0, 4.0];
        let b = [0.5f32, f32::NAN, f32::INFINITY, 1.0];
        add_sanitized_inplace(&mut a, &b);
        assert!(a[0].is_finite() && a[0] == 1.5);
        assert!(a[1].is_finite() && a[1] == 2.0);
        assert!(a[2].is_finite() && a[2] == 3.0);
        assert!(a[3].is_finite() && a[3] == 5.0);
    }

    #[test]
    fn copy_sanitized_inplace_basic() {
        let mut a = [0.0f32; 4];
        let b = [0.5f32, f32::NAN, f32::INFINITY, 1.0];
        copy_sanitized_inplace(&mut a, &b);
        assert!(a[0].is_finite() && a[0] == 0.5);
        assert!(a[1].is_finite() && a[1] == 0.0);
        assert!(a[2].is_finite() && a[2] == 0.0);
        assert!(a[3].is_finite() && a[3] == 1.0);
    }

    #[test]
    fn sanitize_finite_inplace_basic() {
        let mut a = [1.0f32, f32::NAN, f32::INFINITY, 4.0, f32::NEG_INFINITY];
        sanitize_finite_inplace(&mut a);
        assert!(a[0].is_finite() && a[0] == 1.0);
        assert_eq!(a[1], 0.0);
        assert_eq!(a[2], 0.0);
        assert!(a[3].is_finite() && a[3] == 4.0);
        assert_eq!(a[4], 0.0);
    }

    #[test]
    fn peak_abs_basic() {
        let a = [1.0f32, -3.0, 2.0, 0.5];
        assert_eq!(peak_abs(&a), 3.0);
    }

    #[test]
    fn clamp_inplace_basic() {
        let mut a = [-2.0f32, -0.5, 0.0, 0.5, 2.0];
        clamp_inplace(&mut a, -1.0, 1.0);
        assert_eq!(a, [-1.0, -0.5, 0.0, 0.5, 1.0]);
    }
}

// ---------------------------------------------------------------------------
// Integer -> f32 conversion
// ---------------------------------------------------------------------------

/// Convert i32 samples to f32 and scale by `gain`.
/// `dst` must be at least as long as `src`.
pub fn convert_i32_to_f32(src: &[i32], dst: &mut [f32], gain: f32) {
    let n = src.len().min(dst.len());
    if n == 0 {
        return;
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx") {
            convert_i32_to_f32_avx(&src[..n], &mut dst[..n], gain);
            return;
        }
        if is_x86_feature_detected!("sse") {
            convert_i32_to_f32_sse(&src[..n], &mut dst[..n], gain);
            return;
        }
    }
    convert_i32_to_f32_scalar(&src[..n], &mut dst[..n], gain);
}

fn convert_i32_to_f32_scalar(src: &[i32], dst: &mut [f32], gain: f32) {
    for (s, d) in src.iter().zip(dst.iter_mut()) {
        *d = *s as f32 * gain;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn convert_i32_to_f32_sse(src: &[i32], dst: &mut [f32], gain: f32) {
    let g = _mm_set1_ps(gain);
    let mut i = 0;
    while i + 4 <= src.len() {
        let s = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
        let f = _mm_cvtepi32_ps(s);
        let r = _mm_mul_ps(f, g);
        _mm_storeu_ps(dst.as_mut_ptr().add(i), r);
        i += 4;
    }
    for (s, d) in src[i..].iter().zip(dst[i..].iter_mut()) {
        *d = *s as f32 * gain;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx")]
unsafe fn convert_i32_to_f32_avx(src: &[i32], dst: &mut [f32], gain: f32) {
    let g = _mm256_set1_ps(gain);
    let mut i = 0;
    while i + 8 <= src.len() {
        let s = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
        let f = _mm256_cvtepi32_ps(s);
        let r = _mm256_mul_ps(f, g);
        _mm256_storeu_ps(dst.as_mut_ptr().add(i), r);
        i += 8;
    }
    for (s, d) in src[i..].iter().zip(dst[i..].iter_mut()) {
        *d = *s as f32 * gain;
    }
}

/// Convert i16 samples to f32 and scale by `gain`.
/// `dst` must be at least as long as `src`.
pub fn convert_i16_to_f32(src: &[i16], dst: &mut [f32], gain: f32) {
    let n = src.len().min(dst.len());
    if n == 0 {
        return;
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx2") {
            convert_i16_to_f32_avx2(&src[..n], &mut dst[..n], gain);
            return;
        }
        if is_x86_feature_detected!("sse4.1") {
            convert_i16_to_f32_sse41(&src[..n], &mut dst[..n], gain);
            return;
        }
    }
    convert_i16_to_f32_scalar(&src[..n], &mut dst[..n], gain);
}

fn convert_i16_to_f32_scalar(src: &[i16], dst: &mut [f32], gain: f32) {
    for (s, d) in src.iter().zip(dst.iter_mut()) {
        *d = *s as f32 * gain;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn convert_i16_to_f32_sse41(src: &[i16], dst: &mut [f32], gain: f32) {
    let g = _mm_set1_ps(gain);
    let mut i = 0;
    while i + 8 <= src.len() {
        let bytes = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
        let low = _mm_cvtepi16_epi32(bytes);
        let high = _mm_cvtepi16_epi32(_mm_srli_si128(bytes, 8));
        let low_f = _mm_mul_ps(_mm_cvtepi32_ps(low), g);
        let high_f = _mm_mul_ps(_mm_cvtepi32_ps(high), g);
        _mm_storeu_ps(dst.as_mut_ptr().add(i), low_f);
        _mm_storeu_ps(dst.as_mut_ptr().add(i + 4), high_f);
        i += 8;
    }
    for (s, d) in src[i..].iter().zip(dst[i..].iter_mut()) {
        *d = *s as f32 * gain;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx2")]
unsafe fn convert_i16_to_f32_avx2(src: &[i16], dst: &mut [f32], gain: f32) {
    let g = _mm256_set1_ps(gain);
    let mut i = 0;
    while i + 16 <= src.len() {
        let bytes = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
        let low = _mm256_cvtepi16_epi32(bytes);
        let high = _mm256_cvtepi16_epi32(_mm_srli_si128(bytes, 8));
        let low_f = _mm256_mul_ps(_mm256_cvtepi32_ps(low), g);
        let high_f = _mm256_mul_ps(_mm256_cvtepi32_ps(high), g);
        _mm256_storeu_ps(dst.as_mut_ptr().add(i), low_f);
        _mm256_storeu_ps(dst.as_mut_ptr().add(i + 8), high_f);
        i += 16;
    }
    if i + 8 <= src.len() {
        let bytes = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
        let low = _mm_cvtepi16_epi32(bytes);
        let high = _mm_cvtepi16_epi32(_mm_srli_si128(bytes, 8));
        let low_f = _mm_mul_ps(_mm_cvtepi32_ps(low), _mm_set1_ps(gain));
        let high_f = _mm_mul_ps(_mm_cvtepi32_ps(high), _mm_set1_ps(gain));
        _mm_storeu_ps(dst.as_mut_ptr().add(i), low_f);
        _mm_storeu_ps(dst.as_mut_ptr().add(i + 4), high_f);
        i += 8;
    }
    for (s, d) in src[i..].iter().zip(dst[i..].iter_mut()) {
        *d = *s as f32 * gain;
    }
}

/// Convert i8 samples to f32 and scale by `gain`.
/// `dst` must be at least as long as `src`.
pub fn convert_i8_to_f32(src: &[i8], dst: &mut [f32], gain: f32) {
    let n = src.len().min(dst.len());
    if n == 0 {
        return;
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx2") {
            convert_i8_to_f32_avx2(&src[..n], &mut dst[..n], gain);
            return;
        }
        if is_x86_feature_detected!("sse4.1") {
            convert_i8_to_f32_sse41(&src[..n], &mut dst[..n], gain);
            return;
        }
    }
    convert_i8_to_f32_scalar(&src[..n], &mut dst[..n], gain);
}

fn convert_i8_to_f32_scalar(src: &[i8], dst: &mut [f32], gain: f32) {
    for (s, d) in src.iter().zip(dst.iter_mut()) {
        *d = *s as f32 * gain;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn convert_i8_to_f32_sse41(src: &[i8], dst: &mut [f32], gain: f32) {
    let g = _mm_set1_ps(gain);
    let mut i = 0;
    while i + 4 <= src.len() {
        let bytes = _mm_cvtsi32_si128(*(src.as_ptr().add(i) as *const i32));
        let i32s = _mm_cvtepi8_epi32(bytes);
        let f32s = _mm_mul_ps(_mm_cvtepi32_ps(i32s), g);
        _mm_storeu_ps(dst.as_mut_ptr().add(i), f32s);
        i += 4;
    }
    for (s, d) in src[i..].iter().zip(dst[i..].iter_mut()) {
        *d = *s as f32 * gain;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx2")]
unsafe fn convert_i8_to_f32_avx2(src: &[i8], dst: &mut [f32], gain: f32) {
    let g = _mm256_set1_ps(gain);
    let mut i = 0;
    while i + 8 <= src.len() {
        let bytes = _mm_cvtsi64_si128(*(src.as_ptr().add(i) as *const i64));
        let i32s = _mm256_cvtepi8_epi32(bytes);
        let f32s = _mm256_mul_ps(_mm256_cvtepi32_ps(i32s), g);
        _mm256_storeu_ps(dst.as_mut_ptr().add(i), f32s);
        i += 8;
    }
    if i + 4 <= src.len() {
        let bytes = _mm_cvtsi32_si128(*(src.as_ptr().add(i) as *const i32));
        let i32s = _mm_cvtepi8_epi32(bytes);
        let f32s = _mm_mul_ps(_mm_cvtepi32_ps(i32s), _mm_set1_ps(gain));
        _mm_storeu_ps(dst.as_mut_ptr().add(i), f32s);
        i += 4;
    }
    for (s, d) in src[i..].iter().zip(dst[i..].iter_mut()) {
        *d = *s as f32 * gain;
    }
}

/// Convert i32 samples with lower 24 bits valid to f32 and scale by `gain`.
/// Sign-extends the lower 24 bits of each i32 before conversion.
/// `dst` must be at least as long as `src`.
pub fn convert_i24_to_f32(src: &[i32], dst: &mut [f32], gain: f32) {
    let n = src.len().min(dst.len());
    if n == 0 {
        return;
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx2") {
            convert_i24_to_f32_avx2(&src[..n], &mut dst[..n], gain);
            return;
        }
        if is_x86_feature_detected!("sse4.1") {
            convert_i24_to_f32_sse41(&src[..n], &mut dst[..n], gain);
            return;
        }
    }
    convert_i24_to_f32_scalar(&src[..n], &mut dst[..n], gain);
}

fn convert_i24_to_f32_scalar(src: &[i32], dst: &mut [f32], gain: f32) {
    for (s, d) in src.iter().zip(dst.iter_mut()) {
        let mut v = *s & 0x00FF_FFFF;
        if (v & 0x0080_0000) != 0 {
            v |= 0xFF00_0000u32 as i32;
        }
        *d = v as f32 * gain;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn convert_i24_to_f32_sse41(src: &[i32], dst: &mut [f32], gain: f32) {
    let g = _mm_set1_ps(gain);
    let mut i = 0;
    while i + 4 <= src.len() {
        let s = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
        let extended = _mm_srai_epi32(_mm_slli_epi32(s, 8), 8);
        let f = _mm_cvtepi32_ps(extended);
        let r = _mm_mul_ps(f, g);
        _mm_storeu_ps(dst.as_mut_ptr().add(i), r);
        i += 4;
    }
    for (s, d) in src[i..].iter().zip(dst[i..].iter_mut()) {
        let mut v = *s & 0x00FF_FFFF;
        if (v & 0x0080_0000) != 0 {
            v |= 0xFF00_0000u32 as i32;
        }
        *d = v as f32 * gain;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx2")]
unsafe fn convert_i24_to_f32_avx2(src: &[i32], dst: &mut [f32], gain: f32) {
    let g = _mm256_set1_ps(gain);
    let mut i = 0;
    while i + 8 <= src.len() {
        let s = _mm256_loadu_si256(src.as_ptr().add(i) as *const __m256i);
        let extended = _mm256_srai_epi32(_mm256_slli_epi32(s, 8), 8);
        let f = _mm256_cvtepi32_ps(extended);
        let r = _mm256_mul_ps(f, g);
        _mm256_storeu_ps(dst.as_mut_ptr().add(i), r);
        i += 8;
    }
    if i + 4 <= src.len() {
        let s = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
        let extended = _mm_srai_epi32(_mm_slli_epi32(s, 8), 8);
        let f = _mm_cvtepi32_ps(extended);
        let r = _mm_mul_ps(f, _mm_set1_ps(gain));
        _mm_storeu_ps(dst.as_mut_ptr().add(i), r);
        i += 4;
    }
    for (s, d) in src[i..].iter().zip(dst[i..].iter_mut()) {
        let mut v = *s & 0x00FF_FFFF;
        if (v & 0x0080_0000) != 0 {
            v |= 0xFF00_0000u32 as i32;
        }
        *d = v as f32 * gain;
    }
}

/// Convert f32 samples to i32 and scale by `gain`, masking to lower 24 bits.
/// Uses truncation toward zero (matching Rust `as i32`).
/// `dst` must be at least as long as `src`.
pub fn convert_f32_to_i24(src: &[f32], dst: &mut [i32], gain: f32) {
    let n = src.len().min(dst.len());
    if n == 0 {
        return;
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx") {
            convert_f32_to_i24_avx(&src[..n], &mut dst[..n], gain);
            return;
        }
        if is_x86_feature_detected!("sse2") {
            convert_f32_to_i24_sse2(&src[..n], &mut dst[..n], gain);
            return;
        }
    }
    convert_f32_to_i24_scalar(&src[..n], &mut dst[..n], gain);
}

fn convert_f32_to_i24_scalar(src: &[f32], dst: &mut [i32], gain: f32) {
    for (s, d) in src.iter().zip(dst.iter_mut()) {
        *d = (*s * gain) as i32 & 0x00FF_FFFF;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn convert_f32_to_i24_sse2(src: &[f32], dst: &mut [i32], gain: f32) {
    let g = _mm_set1_ps(gain);
    let mask = _mm_set1_epi32(0x00FF_FFFF);
    let mut i = 0;
    while i + 4 <= src.len() {
        let s = _mm_loadu_ps(src.as_ptr().add(i));
        let v = _mm_cvttps_epi32(_mm_mul_ps(s, g));
        let m = _mm_and_si128(v, mask);
        _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, m);
        i += 4;
    }
    for (s, d) in src[i..].iter().zip(dst[i..].iter_mut()) {
        *d = (*s * gain) as i32 & 0x00FF_FFFF;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx")]
unsafe fn convert_f32_to_i24_avx(src: &[f32], dst: &mut [i32], gain: f32) {
    let g = _mm256_set1_ps(gain);
    let mask = _mm256_castsi256_ps(_mm256_set1_epi32(0x00FF_FFFF));
    let mut i = 0;
    while i + 8 <= src.len() {
        let s = _mm256_loadu_ps(src.as_ptr().add(i));
        let v = _mm256_cvttps_epi32(_mm256_mul_ps(s, g));
        let m = _mm256_castps_si256(_mm256_and_ps(_mm256_castsi256_ps(v), mask));
        _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, m);
        i += 8;
    }
    if i + 4 <= src.len() {
        let g_sse = _mm_set1_ps(gain);
        let mask_sse = _mm_set1_epi32(0x00FF_FFFF);
        let s = _mm_loadu_ps(src.as_ptr().add(i));
        let v = _mm_cvttps_epi32(_mm_mul_ps(s, g_sse));
        let m = _mm_and_si128(v, mask_sse);
        _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, m);
        i += 4;
    }
    for (s, d) in src[i..].iter().zip(dst[i..].iter_mut()) {
        *d = (*s * gain) as i32 & 0x00FF_FFFF;
    }
}

/// Convert f32 samples to i32 and scale by `gain`.
/// Uses truncation toward zero (matching Rust `as i32`).
/// `dst` must be at least as long as `src`.
pub fn convert_f32_to_i32(src: &[f32], dst: &mut [i32], gain: f32) {
    let n = src.len().min(dst.len());
    if n == 0 {
        return;
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("avx") {
            convert_f32_to_i32_avx(&src[..n], &mut dst[..n], gain);
            return;
        }
        if is_x86_feature_detected!("sse2") {
            convert_f32_to_i32_sse2(&src[..n], &mut dst[..n], gain);
            return;
        }
    }
    convert_f32_to_i32_scalar(&src[..n], &mut dst[..n], gain);
}

fn convert_f32_to_i32_scalar(src: &[f32], dst: &mut [i32], gain: f32) {
    for (s, d) in src.iter().zip(dst.iter_mut()) {
        *d = (*s * gain) as i32;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn convert_f32_to_i32_sse2(src: &[f32], dst: &mut [i32], gain: f32) {
    let g = _mm_set1_ps(gain);
    let mut i = 0;
    while i + 4 <= src.len() {
        let s = _mm_loadu_ps(src.as_ptr().add(i));
        let v = _mm_cvttps_epi32(_mm_mul_ps(s, g));
        _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, v);
        i += 4;
    }
    for (s, d) in src[i..].iter().zip(dst[i..].iter_mut()) {
        *d = (*s * gain) as i32;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[target_feature(enable = "avx")]
unsafe fn convert_f32_to_i32_avx(src: &[f32], dst: &mut [i32], gain: f32) {
    let g = _mm256_set1_ps(gain);
    let mut i = 0;
    while i + 8 <= src.len() {
        let s = _mm256_loadu_ps(src.as_ptr().add(i));
        let v = _mm256_cvttps_epi32(_mm256_mul_ps(s, g));
        _mm256_storeu_si256(dst.as_mut_ptr().add(i) as *mut __m256i, v);
        i += 8;
    }
    if i + 4 <= src.len() {
        let s = _mm_loadu_ps(src.as_ptr().add(i));
        let v = _mm_cvttps_epi32(_mm_mul_ps(s, _mm_set1_ps(gain)));
        _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, v);
        i += 4;
    }
    for (s, d) in src[i..].iter().zip(dst[i..].iter_mut()) {
        *d = (*s * gain) as i32;
    }
}

/// Convert f32 samples to i16 and scale by `gain`.
/// Uses truncation toward zero (matching Rust `as i16`).
/// `dst` must be at least as long as `src`.
pub fn convert_f32_to_i16(src: &[f32], dst: &mut [i16], gain: f32) {
    let n = src.len().min(dst.len());
    if n == 0 {
        return;
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("sse2") {
            convert_f32_to_i16_sse2(&src[..n], &mut dst[..n], gain);
            return;
        }
    }
    convert_f32_to_i16_scalar(&src[..n], &mut dst[..n], gain);
}

fn convert_f32_to_i16_scalar(src: &[f32], dst: &mut [i16], gain: f32) {
    for (s, d) in src.iter().zip(dst.iter_mut()) {
        *d = (*s * gain) as i16;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn convert_f32_to_i16_sse2(src: &[f32], dst: &mut [i16], gain: f32) {
    let g = _mm_set1_ps(gain);
    let mut i = 0;
    while i + 8 <= src.len() {
        let s0 = _mm_loadu_ps(src.as_ptr().add(i));
        let s1 = _mm_loadu_ps(src.as_ptr().add(i + 4));
        let v0 = _mm_cvttps_epi32(_mm_mul_ps(s0, g));
        let v1 = _mm_cvttps_epi32(_mm_mul_ps(s1, g));
        let packed = _mm_packs_epi32(v0, v1);
        _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, packed);
        i += 8;
    }
    if i + 4 <= src.len() {
        let s = _mm_loadu_ps(src.as_ptr().add(i));
        let v = _mm_cvttps_epi32(_mm_mul_ps(s, g));
        let packed = _mm_packs_epi32(v, _mm_setzero_si128());
        _mm_storel_epi64(dst.as_mut_ptr().add(i) as *mut __m128i, packed);
        i += 4;
    }
    for (s, d) in src[i..].iter().zip(dst[i..].iter_mut()) {
        *d = (*s * gain) as i16;
    }
}

/// Convert f32 samples to i8 and scale by `gain`.
/// Uses truncation toward zero (matching Rust `as i8`).
/// `dst` must be at least as long as `src`.
pub fn convert_f32_to_i8(src: &[f32], dst: &mut [i8], gain: f32) {
    let n = src.len().min(dst.len());
    if n == 0 {
        return;
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    unsafe {
        if is_x86_feature_detected!("sse2") {
            convert_f32_to_i8_sse2(&src[..n], &mut dst[..n], gain);
            return;
        }
    }
    convert_f32_to_i8_scalar(&src[..n], &mut dst[..n], gain);
}

fn convert_f32_to_i8_scalar(src: &[f32], dst: &mut [i8], gain: f32) {
    for (s, d) in src.iter().zip(dst.iter_mut()) {
        *d = (*s * gain) as i8;
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
unsafe fn convert_f32_to_i8_sse2(src: &[f32], dst: &mut [i8], gain: f32) {
    let g = _mm_set1_ps(gain);
    let mut i = 0;
    while i + 16 <= src.len() {
        let s0 = _mm_loadu_ps(src.as_ptr().add(i));
        let s1 = _mm_loadu_ps(src.as_ptr().add(i + 4));
        let s2 = _mm_loadu_ps(src.as_ptr().add(i + 8));
        let s3 = _mm_loadu_ps(src.as_ptr().add(i + 12));
        let v0 = _mm_cvttps_epi32(_mm_mul_ps(s0, g));
        let v1 = _mm_cvttps_epi32(_mm_mul_ps(s1, g));
        let v2 = _mm_cvttps_epi32(_mm_mul_ps(s2, g));
        let v3 = _mm_cvttps_epi32(_mm_mul_ps(s3, g));
        let p0 = _mm_packs_epi32(v0, v1);
        let p1 = _mm_packs_epi32(v2, v3);
        let packed = _mm_packs_epi16(p0, p1);
        _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, packed);
        i += 16;
    }
    for (s, d) in src[i..].iter().zip(dst[i..].iter_mut()) {
        *d = (*s * gain) as i8;
    }
}

// ---------------------------------------------------------------------------
// Fade ramps (sin/cos based)
// ---------------------------------------------------------------------------

/// Apply a sine-based fade-in gain ramp in place: `gain = sin(t * π/2)`.
/// `t` for sample `i` is `(start_t + i as f32 * dt).clamp(0.0, 1.0)`.
pub fn apply_fade_in_inplace(buf: &mut [f32], start_t: f32, dt: f32) {
    use wide::f32x4;
    let pi_2 = f32x4::splat(std::f32::consts::FRAC_PI_2);
    let dt_vec = f32x4::from([0.0, dt, 2.0 * dt, 3.0 * dt]);
    let mut i = 0;
    while i + 4 <= buf.len() {
        let base_t = f32x4::splat(start_t + i as f32 * dt);
        let t = (base_t + dt_vec).clamp(f32x4::splat(0.0), f32x4::splat(1.0));
        let gain = (t * pi_2).sin();
        let chunk = &mut buf[i..i + 4];
        let s: f32x4 = [chunk[0], chunk[1], chunk[2], chunk[3]].into();
        let r = s * gain;
        chunk.copy_from_slice(&r.to_array());
        i += 4;
    }
    for (j, v) in buf[i..].iter_mut().enumerate() {
        let t = (((i + j) as f32 * dt) + start_t).clamp(0.0, 1.0);
        *v *= (t * std::f32::consts::FRAC_PI_2).sin();
    }
}

/// Apply a cosine-based fade-out gain ramp in place: `gain = cos(t * π/2)`.
/// `t` for sample `i` is `(start_t + i as f32 * dt).clamp(0.0, 1.0)`.
pub fn apply_fade_out_inplace(buf: &mut [f32], start_t: f32, dt: f32) {
    use wide::f32x4;
    let pi_2 = f32x4::splat(std::f32::consts::FRAC_PI_2);
    let dt_vec = f32x4::from([0.0, dt, 2.0 * dt, 3.0 * dt]);
    let mut i = 0;
    while i + 4 <= buf.len() {
        let base_t = f32x4::splat(start_t + i as f32 * dt);
        let t = (base_t + dt_vec).clamp(f32x4::splat(0.0), f32x4::splat(1.0));
        let gain = (t * pi_2).cos();
        let chunk = &mut buf[i..i + 4];
        let s: f32x4 = [chunk[0], chunk[1], chunk[2], chunk[3]].into();
        let r = s * gain;
        chunk.copy_from_slice(&r.to_array());
        i += 4;
    }
    for (j, v) in buf[i..].iter_mut().enumerate() {
        let t = (((i + j) as f32 * dt) + start_t).clamp(0.0, 1.0);
        *v *= (t * std::f32::consts::FRAC_PI_2).cos();
    }
}
