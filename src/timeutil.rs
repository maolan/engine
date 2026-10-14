//! Helpers for portable `libc::timespec` arithmetic.
//!
//! `timespec` fields are `c_long`-sized: `i32` on 32-bit targets and `i64`
//! on 64-bit ones. Widening a field for arithmetic must be gated on pointer
//! width — a plain `as i64` cast is flagged as unnecessary on 64-bit hosts
//! and removed by `cargo clippy --fix`, which silently re-breaks 32-bit
//! builds.

#[cfg(target_pointer_width = "32")]
pub(crate) fn field_to_i64(v: i32) -> i64 {
    v as i64
}

#[cfg(target_pointer_width = "64")]
pub(crate) fn field_to_i64(v: i64) -> i64 {
    v
}
