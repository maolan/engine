//! MIDI Polyphonic Expression (MPE) v1.1 state and message parsing.
//!
//! MPE divides MIDI channels into Zones. Each Zone has one Manager Channel
//! (global controllers) and one or more Member Channels (per-note expression).
//! This module tracks zone configuration from MPE Configuration Messages
//! (MCM, RPN #6) and Pitch Bend Sensitivity (RPN #0).

use std::ops::RangeInclusive;

/// A single MPE Zone.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MpeZone {
    /// The Manager Channel for this zone (0-based MIDI channel number).
    pub manager_channel: u8,
    /// Inclusive range of Member Channels (0-based MIDI channel numbers).
    pub member_channels: RangeInclusive<u8>,
    /// Pitch bend sensitivity in semitones for the Manager Channel.
    pub manager_pitch_bend_semitones: u8,
    /// Pitch bend sensitivity in semitones for Member Channels.
    pub member_pitch_bend_semitones: u8,
}

impl MpeZone {
    /// Default per MPE v1.1: 48 semitones on members, 2 on manager.
    pub const DEFAULT_MEMBER_PITCH_BEND_SEMITONES: u8 = 48;
    pub const DEFAULT_MANAGER_PITCH_BEND_SEMITONES: u8 = 2;

    /// Create a zone from a manager channel and a member-channel count.
    /// Returns `None` if the count is zero or the resulting range is invalid.
    pub fn new(manager_channel: u8, member_count: u8) -> Option<Self> {
        let manager_channel = manager_channel.min(15);
        let member_count = member_count.min(15);
        if member_count == 0 {
            return None;
        }
        let member_channels = if manager_channel == 0 {
            // Lower Zone: members start at channel 2 (index 1) and ascend.
            let end = 1_u8.saturating_add(member_count).min(15);
            1..=end
        } else if manager_channel == 15 {
            // Upper Zone: members start at channel 15 (index 14) and descend.
            let start = 15_u8.saturating_sub(member_count).max(1);
            start..=14
        } else {
            // Only channels 1 and 16 can be Manager Channels.
            return None;
        };
        Some(Self {
            manager_channel,
            member_channels,
            manager_pitch_bend_semitones: Self::DEFAULT_MANAGER_PITCH_BEND_SEMITONES,
            member_pitch_bend_semitones: Self::DEFAULT_MEMBER_PITCH_BEND_SEMITONES,
        })
    }

    /// Returns true if `channel` is the Manager Channel of this zone.
    pub fn is_manager(&self, channel: u8) -> bool {
        self.manager_channel == channel.min(15)
    }

    /// Returns true if `channel` is a Member Channel of this zone.
    pub fn is_member(&self, channel: u8) -> bool {
        self.member_channels.contains(&channel.min(15))
    }

    /// Returns true if `channel` belongs to this zone in any role.
    pub fn contains(&self, channel: u8) -> bool {
        let channel = channel.min(15);
        self.is_manager(channel) || self.is_member(channel)
    }
}

/// MPE state for one MIDI input stream (typically one track).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct MpeState {
    lower: Option<MpeZone>,
    upper: Option<MpeZone>,
    #[serde(skip)]
    pending_rpn_msb: [Option<u8>; 16],
    #[serde(skip)]
    pending_rpn_lsb: [Option<u8>; 16],
}

impl MpeState {
    /// Create a fresh, inactive MPE state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the Lower Zone if configured.
    pub fn lower(&self) -> Option<&MpeZone> {
        self.lower.as_ref()
    }

    /// Returns the Upper Zone if configured.
    pub fn upper(&self) -> Option<&MpeZone> {
        self.upper.as_ref()
    }

    /// Returns the zone (lower or upper) that `channel` belongs to, if any.
    pub fn zone_for_channel(&self, channel: u8) -> Option<&MpeZone> {
        let channel = channel.min(15);
        if let Some(ref lower) = self.lower
            && lower.contains(channel)
        {
            return Some(lower);
        }
        if let Some(ref upper) = self.upper
            && upper.contains(channel)
        {
            return Some(upper);
        }
        None
    }

    /// Returns true if `channel` is a Manager Channel in any configured zone.
    pub fn is_manager_channel(&self, channel: u8) -> bool {
        let channel = channel.min(15);
        self.lower.as_ref().is_some_and(|z| z.is_manager(channel))
            || self.upper.as_ref().is_some_and(|z| z.is_manager(channel))
    }

    /// Returns true if `channel` is a Member Channel in any configured zone.
    pub fn is_member_channel(&self, channel: u8) -> bool {
        let channel = channel.min(15);
        self.lower.as_ref().is_some_and(|z| z.is_member(channel))
            || self.upper.as_ref().is_some_and(|z| z.is_member(channel))
    }

    /// Returns true if any MPE zone is active.
    pub fn is_active(&self) -> bool {
        self.lower.is_some() || self.upper.is_some()
    }

    /// Returns the active zone to use for voice allocation. When both zones
    /// are configured the Lower Zone is preferred because it is the most
    /// common setup; callers that need both zones can inspect [`lower`] and
    /// [`upper`] directly.
    pub fn active_zone(&self) -> Option<&MpeZone> {
        self.lower.as_ref().or(self.upper.as_ref())
    }

    /// Configure a zone from an MPE Configuration Message.
    ///
    /// `manager_channel` is the 0-based channel on which the MCM was received
    /// (must be 0 for Lower Zone or 15 for Upper Zone). `member_count` is the
    /// value sent with CC#6 (0..=15). A count of zero deactivates the zone.
    ///
    /// If the new zone overlaps an existing zone, the new zone steals the
    /// channels and the old zone is deactivated if it is left with no members,
    /// per MPE v1.1 Appendix B.
    pub fn configure_zone(&mut self, manager_channel: u8, member_count: u8) {
        let manager_channel = manager_channel.min(15);
        if manager_channel != 0 && manager_channel != 15 {
            return;
        }

        if member_count == 0 {
            if manager_channel == 0 {
                self.lower = None;
            } else {
                self.upper = None;
            }
            return;
        }

        let new_zone = match MpeZone::new(manager_channel, member_count) {
            Some(z) => z,
            None => return,
        };

        // If the new zone steals channels from the other zone, shrink the
        // other zone to remove the stolen channels and deactivate it if no
        // members remain.
        let other = if manager_channel == 0 {
            &mut self.upper
        } else {
            &mut self.lower
        };
        if let Some(other_zone) = other.as_mut() {
            let remaining_members: Vec<u8> = other_zone
                .member_channels
                .clone()
                .filter(|ch| !new_zone.member_channels.contains(ch))
                .collect();
            if remaining_members.is_empty() {
                *other = None;
            } else {
                let start = *remaining_members.first().unwrap();
                let end = *remaining_members.last().unwrap();
                other_zone.member_channels = start..=end;
            }
        }

        if manager_channel == 0 {
            self.lower = Some(new_zone);
        } else {
            self.upper = Some(new_zone);
        }
    }

    /// Set Pitch Bend Sensitivity from an RPN #0 message.
    ///
    /// `channel` is the 0-based channel on which the RPN was received.
    /// `semitones` is the MSB value (CC#6). The cent value (CC#38) is ignored
    /// for now; MPE recommends integer semitones.
    pub fn set_pitch_bend_sensitivity(&mut self, channel: u8, semitones: u8) {
        let channel = channel.min(15);
        let semitones = semitones.min(96);

        if let Some(ref mut lower) = self.lower {
            if lower.is_manager(channel) {
                lower.manager_pitch_bend_semitones = semitones;
            } else if lower.is_member(channel) {
                lower.member_pitch_bend_semitones = semitones;
            }
        }
        if let Some(ref mut upper) = self.upper {
            if upper.is_manager(channel) {
                upper.manager_pitch_bend_semitones = semitones;
            } else if upper.is_member(channel) {
                upper.member_pitch_bend_semitones = semitones;
            }
        }
    }

    /// Feed one raw MIDI event into the MPE state machine.
    ///
    /// Returns the channel that was affected if the event was an MCM or RPN
    /// message that changed state. This is mostly useful for tests and logging.
    pub fn feed(&mut self, data: &[u8]) -> Option<u8> {
        if data.len() < 3 {
            return None;
        }
        let status = data[0];
        let channel = status & 0x0F;
        let msg_type = status & 0xF0;

        // Controller messages are needed for RPN/MCM parsing.
        if msg_type != 0xB0 {
            return None;
        }

        let cc = data[1];
        let value = data[2];

        match cc {
            101 => {
                // RPN MSB. Any new RPN selection aborts the previous sequence.
                self.pending_rpn_msb[channel as usize] = Some(value);
                self.pending_rpn_lsb[channel as usize] = None;
            }
            100 => {
                if self.pending_rpn_msb[channel as usize].is_some() {
                    self.pending_rpn_lsb[channel as usize] = Some(value);
                }
            }
            6 => {
                if let (Some(msb), Some(lsb)) = (
                    self.pending_rpn_msb[channel as usize],
                    self.pending_rpn_lsb[channel as usize],
                ) {
                    if msb == 0x00 && lsb == 0x00 {
                        // RPN #0: Pitch Bend Sensitivity.
                        self.set_pitch_bend_sensitivity(channel, value);
                        self.clear_rpn_state(channel);
                        return Some(channel);
                    } else if msb == 0x00 && lsb == 0x06 {
                        // RPN #6: MPE Configuration Message.
                        self.configure_zone(channel, value);
                        self.clear_rpn_state(channel);
                        return Some(channel);
                    }
                }
            }
            38 => {
                // Data Entry LSB — ignored for now.
            }
            _ => {
                // Any other CC on the channel aborts the pending RPN sequence.
                self.clear_rpn_state(channel);
            }
        }

        None
    }

    fn clear_rpn_state(&mut self, channel: u8) {
        let idx = channel.min(15) as usize;
        self.pending_rpn_msb[idx] = None;
        self.pending_rpn_lsb[idx] = None;
    }
}

/// Voice allocator that spreads note-ons across the Member Channels of an
/// active MPE zone.
///
/// The allocator is intended for playback-time conversion: a piano roll or
/// incoming MIDI stream that is not yet channelised is rewritten so that each
/// note lives on its own Member Channel, allowing per-note expression.
#[derive(Debug, Clone)]
pub struct MpeVoiceAllocator {
    /// Active MPE zone used for allocation.
    zone: MpeZone,
    /// Active note voices indexed by pitch. Because the same pitch can be
    /// triggered while still sounding, each pitch keeps a stack of allocated
    /// channels (LIFO).
    active: std::collections::HashMap<u8, Vec<u8>>,
    /// Running count of active notes on each Member Channel so we can place
    /// new notes on the least-loaded channel.
    channel_load: std::collections::HashMap<u8, usize>,
}

impl MpeVoiceAllocator {
    /// Create an allocator from an active zone.
    pub fn new(zone: MpeZone) -> Self {
        let mut channel_load = std::collections::HashMap::new();
        for ch in zone.member_channels.clone() {
            channel_load.insert(ch, 0);
        }
        Self {
            zone,
            active: std::collections::HashMap::new(),
            channel_load,
        }
    }

    /// Process one raw MIDI event. Returns zero or more raw MIDI events.
    ///
    /// * Note-on events on non-member channels are allocated to a Member
    ///   Channel and rewritten.
    /// * Note-off events look up the previously allocated channel for the
    ///   pitch and are rewritten to the same channel.
    /// * All other events pass through unchanged. This includes events that
    ///   are already on Member Channels (e.g. per-note expression generated by
    ///   the editor) and Manager Channel global controllers.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        if data.is_empty() {
            return Vec::new();
        }
        let status = data[0];
        let msg_type = status & 0xF0;
        let channel = status & 0x0F;
        let pitch = data.get(1).copied().unwrap_or(0);

        match msg_type {
            0x90 => {
                // Note-on. Velocity zero is treated as note-off by many
                // devices, but we still allocate for consistent handling.
                if data.len() < 2 {
                    return vec![data.to_vec()];
                }
                let allocated = if self.zone.is_member(channel) {
                    // Already on a member channel: track it but do not move it.
                    channel
                } else {
                    self.allocate(pitch)
                };
                self.active.entry(pitch).or_default().push(allocated);
                *self.channel_load.entry(allocated).or_insert(0) += 1;
                // Per MPE v1.1, reset per-note controllers on the member channel
                // before the note-on so the new note does not inherit stale
                // expression from a previous note on the same channel.
                vec![
                    vec![0xE0 | allocated, 0x00, 0x40], // pitch bend center
                    vec![0xD0 | allocated, 0x00],       // channel pressure zero
                    vec![0xB0 | allocated, 74, 0x00],   // CC74 (timbre) zero
                    Self::with_channel(data, allocated),
                ]
            }
            0x80 => {
                // Note-off.
                if data.len() < 2 {
                    return vec![data.to_vec()];
                }
                let allocated = self.release(pitch);
                let out_channel = allocated.unwrap_or(channel);
                vec![Self::with_channel(data, out_channel)]
            }
            _ => {
                // Pass through unchanged.
                vec![data.to_vec()]
            }
        }
    }

    /// Process a slice of events in order.
    pub fn feed_many(&mut self, data: &[Vec<u8>]) -> Vec<Vec<u8>> {
        data.iter().flat_map(|d| self.feed(d)).collect()
    }

    fn allocate(&mut self, _pitch: u8) -> u8 {
        // Pick the member channel with the smallest load. Ties are resolved by
        // channel number, which keeps allocation deterministic.
        self.zone
            .member_channels
            .clone()
            .min_by_key(|ch| self.channel_load.get(ch).copied().unwrap_or(0))
            .unwrap_or(*self.zone.member_channels.start())
    }

    fn release(&mut self, pitch: u8) -> Option<u8> {
        let stack = self.active.get_mut(&pitch)?;
        let ch = stack.pop();
        if stack.is_empty() {
            self.active.remove(&pitch);
        }
        if let Some(ch) = ch
            && let Some(load) = self.channel_load.get_mut(&ch)
        {
            *load = load.saturating_sub(1);
        }
        ch
    }

    fn with_channel(data: &[u8], channel: u8) -> Vec<u8> {
        let mut out = data.to_vec();
        out[0] = (data[0] & 0xF0) | (channel & 0x0F);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cc(channel: u8, cc: u8, value: u8) -> Vec<u8> {
        vec![0xB0 | (channel & 0x0F), cc, value]
    }

    #[test]
    fn lower_zone_with_15_members() {
        let mut state = MpeState::new();
        // MCM on channel 1 (index 0) with 15 member channels.
        state.feed(&cc(0, 101, 0));
        state.feed(&cc(0, 100, 6));
        state.feed(&cc(0, 6, 15));

        let lower = state.lower().unwrap();
        assert_eq!(lower.manager_channel, 0);
        assert_eq!(lower.member_channels, 1..=15);
        assert_eq!(lower.member_pitch_bend_semitones, 48);
        assert_eq!(lower.manager_pitch_bend_semitones, 2);
        assert!(state.upper().is_none());
    }

    #[test]
    fn upper_zone_with_7_members() {
        let mut state = MpeState::new();
        // MCM on channel 16 (index 15) with 7 member channels.
        state.feed(&cc(15, 101, 0));
        state.feed(&cc(15, 100, 6));
        state.feed(&cc(15, 6, 7));

        let upper = state.upper().unwrap();
        assert_eq!(upper.manager_channel, 15);
        assert_eq!(upper.member_channels, 8..=14);
        assert!(state.lower().is_none());
    }

    #[test]
    fn zero_member_count_deactivates_zone() {
        let mut state = MpeState::new();
        state.feed(&cc(0, 101, 0));
        state.feed(&cc(0, 100, 6));
        state.feed(&cc(0, 6, 5));
        assert!(state.lower().is_some());

        state.feed(&cc(0, 101, 0));
        state.feed(&cc(0, 100, 6));
        state.feed(&cc(0, 6, 0));
        assert!(state.lower().is_none());
    }

    #[test]
    fn overlapping_zone_steals_channels() {
        let mut state = MpeState::new();
        // Lower zone uses channels 1..=8 (member count 7, channels 2-8).
        state.feed(&cc(0, 101, 0));
        state.feed(&cc(0, 100, 6));
        state.feed(&cc(0, 6, 7));
        assert!(state.lower().is_some());
        assert!(state.upper().is_none());

        // Upper zone uses channels 6..=15 (member count 10).
        // This overlaps with lower zone members 2..=8; lower zone keeps the
        // non-overlapping members 2..=5.
        state.feed(&cc(15, 101, 0));
        state.feed(&cc(15, 100, 6));
        state.feed(&cc(15, 6, 10));
        let lower = state.lower().unwrap();
        assert_eq!(lower.member_channels, 1..=4);
        let upper = state.upper().unwrap();
        assert_eq!(upper.member_channels, 5..=14);
    }

    #[test]
    fn rpn_zero_sets_pitch_bend_sensitivity() {
        let mut state = MpeState::new();
        state.feed(&cc(0, 101, 0));
        state.feed(&cc(0, 100, 6));
        state.feed(&cc(0, 6, 7));

        // Manager channel pitch bend sensitivity to 5 semitones.
        state.feed(&cc(0, 101, 0));
        state.feed(&cc(0, 100, 0));
        state.feed(&cc(0, 6, 5));
        assert_eq!(state.lower().unwrap().manager_pitch_bend_semitones, 5);

        // Member channel pitch bend sensitivity to 24 semitones.
        state.feed(&cc(2, 101, 0));
        state.feed(&cc(2, 100, 0));
        state.feed(&cc(2, 6, 24));
        assert_eq!(state.lower().unwrap().member_pitch_bend_semitones, 24);
    }

    #[test]
    fn invalid_manager_channels_are_ignored() {
        let mut state = MpeState::new();
        for ch in 1..=14 {
            state.feed(&cc(ch, 101, 0));
            state.feed(&cc(ch, 100, 6));
            state.feed(&cc(ch, 6, 5));
        }
        assert!(!state.is_active());
    }

    fn note_on(channel: u8, pitch: u8, velocity: u8) -> Vec<u8> {
        vec![0x90 | (channel & 0x0F), pitch, velocity]
    }

    fn note_off(channel: u8, pitch: u8, velocity: u8) -> Vec<u8> {
        vec![0x80 | (channel & 0x0F), pitch, velocity]
    }

    fn note_on_event(events: &[Vec<u8>]) -> Option<&Vec<u8>> {
        events
            .iter()
            .find(|e| matches!(e.first().copied().unwrap_or(0) & 0xF0, 0x90))
    }

    #[test]
    fn allocator_spreads_notes_across_member_channels() {
        let zone = MpeZone::new(0, 3).unwrap();
        let mut alloc = MpeVoiceAllocator::new(zone);

        let out = alloc.feed_many(&[
            note_on(0, 60, 100),
            note_on(0, 64, 100),
            note_on(0, 67, 100),
        ]);

        assert_eq!(note_on_event(&out[0..4]).unwrap()[0] & 0x0F, 1);
        assert_eq!(note_on_event(&out[4..8]).unwrap()[0] & 0x0F, 2);
        assert_eq!(note_on_event(&out[8..12]).unwrap()[0] & 0x0F, 3);
    }

    #[test]
    fn allocator_reuses_released_channel() {
        let zone = MpeZone::new(0, 2).unwrap();
        let mut alloc = MpeVoiceAllocator::new(zone);

        let _ = alloc.feed(&note_on(0, 60, 100)); // channel 1
        let _ = alloc.feed(&note_off(0, 60, 100)); // release channel 1
        let out = alloc.feed(&note_on(0, 64, 100));

        assert_eq!(note_on_event(&out).unwrap()[0] & 0x0F, 1);
    }

    #[test]
    fn allocator_matches_note_off_to_note_on_channel() {
        let zone = MpeZone::new(0, 3).unwrap();
        let mut alloc = MpeVoiceAllocator::new(zone);

        let _ = alloc.feed(&note_on(0, 60, 100));
        let _ = alloc.feed(&note_on(0, 64, 100));
        let off = alloc.feed(&note_off(0, 60, 100));

        assert_eq!(off[0][0] & 0x0F, 1);
    }

    #[test]
    fn allocator_resets_per_note_controllers_before_note_on() {
        let zone = MpeZone::new(0, 3).unwrap();
        let mut alloc = MpeVoiceAllocator::new(zone);

        let out = alloc.feed(&note_on(0, 60, 100));

        assert_eq!(out.len(), 4);
        assert_eq!(out[0], vec![0xE1, 0x00, 0x40]); // pitch bend center
        assert_eq!(out[1], vec![0xD1, 0x00]); // pressure zero
        assert_eq!(out[2], vec![0xB1, 74, 0x00]); // CC74 zero
        assert_eq!(out[3], vec![0x91, 60, 100]); // note-on
    }

    #[test]
    fn allocator_keeps_member_channel_events() {
        let zone = MpeZone::new(0, 3).unwrap();
        let mut alloc = MpeVoiceAllocator::new(zone);

        let pb = vec![0xE0 | 2, 0x00, 0x40];
        let out = alloc.feed(&pb);
        assert_eq!(out[0][0] & 0x0F, 2);
    }

    #[test]
    fn allocator_leaves_manager_controllers_unchanged() {
        let zone = MpeZone::new(0, 3).unwrap();
        let mut alloc = MpeVoiceAllocator::new(zone);

        let cc_msg = vec![0xB0, 74, 64];
        let out = alloc.feed(&cc_msg);
        assert_eq!(out[0][0] & 0x0F, 0);
    }
}
