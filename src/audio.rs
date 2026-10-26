//! Live device input and output for the desktop application.
//!
//! Shared streams use CPAL (ALSA, JACK, PipeWire, or PulseAudio on Linux;
//! WASAPI or ASIO on Windows). Windows exclusive streams are opened directly
//! with WASAPI so the requested endpoint format and exclusive-access errors are
//! visible to the application.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    FromSample, Sample, SampleFormat, Stream, StreamConfig, SupportedBufferSize,
    SupportedStreamConfig, SupportedStreamConfigRange,
};
use crossbeam_queue::ArrayQueue;
#[cfg(windows)]
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

const AUDIO_SOURCE_SILENT: u8 = 0;
const AUDIO_SOURCE_TEST_TONE: u8 = 1;
const AUDIO_SOURCE_INPUT_MONITOR: u8 = 2;
const AUDIO_SOURCE_PROJECT: u8 = 3;
const AUDIO_SOURCE_STREAM: u8 = 4;
const AUDIO_RING_CAPACITY: usize = 65_536;
const LIVE_STREAM_RING_CAPACITY: usize = 4_096;
const MAX_LIVE_STREAM_SOURCES: usize = 64;
const INPUT_MONITOR_RING_CAPACITY: usize = AUDIO_RING_CAPACITY * 2;
const INPUT_RECORDING_BUFFER_FRAMES: usize = 65_536;
const LATENCY_ESTIMATE_MIN_SAMPLES: u32 = 8;
const LATENCY_ESTIMATE_MAX_MICROSECONDS: u128 = 1_000_000;
const AUDIO_ERROR_HISTORY_LIMIT: usize = 16;
const AUDIO_COMMAND_QUEUE_CAPACITY: usize = 128;
const RETIRED_STREAM_QUEUE_CAPACITY: usize = 4;
const STREAM_PREFILL_FRAMES: usize = 512;
const STREAM_HANDOFF_IDLE: u8 = 0;
const STREAM_HANDOFF_COMMITTING: u8 = 1;
const STREAM_HANDOFF_STOPPING: u8 = 2;
const OUTPUT_GAIN_MAX: f32 = 2.0;
const OUTPUT_GAIN_SMOOTHING_SECONDS: f32 = 0.02;
const BROWSER_PREVIEW_ACTIVE_BIT: u64 = 1 << 63;
const BROWSER_PREVIEW_FRAME_MASK: u64 = !BROWSER_PREVIEW_ACTIVE_BIT;
const TEST_TONE_HZ: f32 = 440.0;
const TEST_TONE_LEVEL: f32 = 0.12;

/// Number of Mixer tracks that can be named by a channel's signed-byte assignment.
pub const MIXER_TRACK_METER_COUNT: usize = i8::MAX as usize + 1;

pub(crate) fn mixer_effect_wet_mix(mix_raw: Option<i32>) -> f32 {
    mix_raw.map_or(1.0, |raw| {
        (raw.clamp(-6_400, 6_400) as f32 + 6_400.0) / 12_800.0
    })
}

/// Split a track-major Mixer bus buffer into post-transform meter buses and, when
/// present, pre-effect insert input buses.
pub(crate) fn split_mixer_track_buses_mut(
    buses: &mut [f32],
    samples_per_track: usize,
) -> Result<(&mut [f32], Option<&mut [f32]>), String> {
    let meter_bus_len = MIXER_TRACK_METER_COUNT
        .checked_mul(samples_per_track)
        .ok_or_else(|| "Mixer bus size overflow".to_owned())?;
    if buses.len() == meter_bus_len {
        return Ok((buses, None));
    }
    let combined_bus_len = meter_bus_len
        .checked_mul(2)
        .ok_or_else(|| "Mixer bus size overflow".to_owned())?;
    if buses.len() == combined_bus_len {
        let (meter_buses, effect_input_buses) = buses.split_at_mut(meter_bus_len);
        return Ok((meter_buses, Some(effect_input_buses)));
    }
    Err("Mixer buses do not match the expected block size".to_owned())
}

#[derive(Clone)]
pub(crate) struct LiveMixerSendTargetAtomics {
    pub(crate) level_raw: Arc<AtomicI32>,
    pub(crate) enabled: Arc<AtomicI32>,
}

/// Block-peak snapshots for the supported live Playlist Mixer routes.
///
/// The render worker publishes the maximum sample for each insert between UI reads. The UI
/// consumes the combined values with [`take_peak`](Self::take_peak); MIDI scripts can consume
/// channel-specific values independently with [`take_peak_channels`](Self::take_peak_channels).
/// Both paths keep the audio thread lock-free.
pub struct MixerTrackPeakMeters {
    peaks: [AtomicU32; MIXER_TRACK_METER_COUNT],
    script_peaks_left: [AtomicU32; MIXER_TRACK_METER_COUNT],
    script_peaks_right: [AtomicU32; MIXER_TRACK_METER_COUNT],
    live_mixer_insert_fader_raw: [Arc<AtomicI32>; MIXER_TRACK_METER_COUNT],
    live_mixer_insert_pan_raw: [Arc<AtomicI32>; MIXER_TRACK_METER_COUNT],
    live_mixer_insert_stereo_separation_raw: [Arc<AtomicI32>; MIXER_TRACK_METER_COUNT],
    live_mixer_insert_signal_flags: [Arc<AtomicI32>; MIXER_TRACK_METER_COUNT],
    live_mixer_insert_enabled: [Arc<AtomicI32>; MIXER_TRACK_METER_COUNT],
    live_mixer_solo_mask: Arc<ArcSwapOption<[u64; 2]>>,
    live_mixer_send_targets: Mutex<HashMap<(usize, usize), LiveMixerSendTargetAtomics>>,
    live_mixer_effect_mix_targets: Mutex<HashMap<(usize, u16), Arc<AtomicU32>>>,
    live_mixer_effect_slot_enabled_targets: Mutex<HashMap<(usize, u16), Arc<AtomicI32>>>,
    live_mixer_insert_effects_enabled_targets: Mutex<HashMap<usize, Arc<AtomicI32>>>,
    live_master_fader_gain: Arc<AtomicU32>,
    live_master_pan_raw: Arc<AtomicI32>,
    live_master_stereo_separation_raw: Arc<AtomicI32>,
    live_master_enabled: Arc<AtomicI32>,
    live_master_signal_flags: Arc<AtomicI32>,
}

impl Default for MixerTrackPeakMeters {
    fn default() -> Self {
        Self {
            peaks: std::array::from_fn(|_| AtomicU32::new(0.0_f32.to_bits())),
            script_peaks_left: std::array::from_fn(|_| AtomicU32::new(0.0_f32.to_bits())),
            script_peaks_right: std::array::from_fn(|_| AtomicU32::new(0.0_f32.to_bits())),
            live_mixer_insert_fader_raw: std::array::from_fn(|_| {
                Arc::new(AtomicI32::new(i32::MIN))
            }),
            live_mixer_insert_pan_raw: std::array::from_fn(|_| Arc::new(AtomicI32::new(i32::MIN))),
            live_mixer_insert_stereo_separation_raw: std::array::from_fn(|_| {
                Arc::new(AtomicI32::new(i32::MIN))
            }),
            live_mixer_insert_signal_flags: std::array::from_fn(|_| {
                Arc::new(AtomicI32::new(i32::MIN))
            }),
            live_mixer_insert_enabled: std::array::from_fn(|_| Arc::new(AtomicI32::new(i32::MIN))),
            live_mixer_solo_mask: Arc::new(ArcSwapOption::empty()),
            live_mixer_send_targets: Mutex::new(HashMap::new()),
            live_mixer_effect_mix_targets: Mutex::new(HashMap::new()),
            live_mixer_effect_slot_enabled_targets: Mutex::new(HashMap::new()),
            live_mixer_insert_effects_enabled_targets: Mutex::new(HashMap::new()),
            live_master_fader_gain: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
            live_master_pan_raw: Arc::new(AtomicI32::new(i32::MIN)),
            live_master_stereo_separation_raw: Arc::new(AtomicI32::new(i32::MIN)),
            live_master_enabled: Arc::new(AtomicI32::new(i32::MIN)),
            live_master_signal_flags: Arc::new(AtomicI32::new(i32::MIN)),
        }
    }
}

impl MixerTrackPeakMeters {
    /// Consume the largest sample peak published for an insert since the previous read.
    pub fn take_peak(&self, track: usize) -> f32 {
        self.peaks
            .get(track)
            .map(|peak| f32::from_bits(peak.swap(0.0_f32.to_bits(), Ordering::AcqRel)))
            .unwrap_or(0.0)
    }

    /// Consume left and right insert peaks published since the previous script-state update.
    pub fn take_peak_channels(&self, track: usize) -> (f32, f32) {
        let take = |peaks: &[AtomicU32; MIXER_TRACK_METER_COUNT]| {
            peaks
                .get(track)
                .map(|peak| f32::from_bits(peak.swap(0.0_f32.to_bits(), Ordering::AcqRel)))
                .unwrap_or(0.0)
        };
        (
            take(&self.script_peaks_left),
            take(&self.script_peaks_right),
        )
    }

    /// Clear all pending meter peaks before starting a new live stream.
    pub fn clear(&self) {
        for peak in &self.peaks {
            peak.store(0.0_f32.to_bits(), Ordering::Release);
        }
        for peak in self
            .script_peaks_left
            .iter()
            .chain(self.script_peaks_right.iter())
        {
            peak.store(0.0_f32.to_bits(), Ordering::Release);
        }
    }

    /// Set the live Playlist stream's target Master fader gain.
    ///
    /// The render worker reads this value without taking a lock and smooths changes over 20 ms.
    pub fn set_live_master_fader_gain(&self, gain: f32) {
        let gain = if gain.is_finite() {
            gain.clamp(0.0, 1.6)
        } else {
            1.0
        };
        self.live_master_fader_gain
            .store(gain.to_bits(), Ordering::Release);
    }

    /// Set a non-Master Mixer insert's live fader target in FLP raw units.
    ///
    /// Live Playlist, Pattern, and Performance render workers read the target without taking a
    /// lock and smooth supported source fader changes over 20 ms. `None` keeps the saved route
    /// transform in effect.
    pub fn set_live_mixer_insert_fader_raw(&self, track: usize, volume_raw: Option<i32>) {
        let Some(target) = self.live_mixer_insert_fader_raw.get(track) else {
            return;
        };
        let value = volume_raw
            .filter(|value| (0..=16_000).contains(value))
            .unwrap_or(i32::MIN);
        target.store(value, Ordering::Release);
    }

    pub(crate) fn live_mixer_insert_fader_raw(&self, track: usize) -> Option<Arc<AtomicI32>> {
        self.live_mixer_insert_fader_raw.get(track).cloned()
    }

    /// Set a non-Master Mixer insert's live pan target in FLP raw units.
    ///
    /// The render worker reads the target without taking a lock and smooths supported source pan
    /// changes over 20 ms. `None` keeps the saved route transform in effect.
    pub fn set_live_mixer_insert_pan_raw(&self, track: usize, pan_raw: Option<i32>) {
        let Some(target) = self.live_mixer_insert_pan_raw.get(track) else {
            return;
        };
        let value = pan_raw
            .filter(|value| (-6_400..=6_400).contains(value))
            .unwrap_or(i32::MIN);
        target.store(value, Ordering::Release);
    }

    pub(crate) fn live_mixer_insert_pan_raw(&self, track: usize) -> Option<Arc<AtomicI32>> {
        self.live_mixer_insert_pan_raw.get(track).cloned()
    }

    /// Set a non-Master Mixer insert's live stereo-separation target in FLP raw units.
    ///
    /// The render worker reads the target without taking a lock and smooths supported source
    /// changes over 20 ms. `None` keeps the saved route transform in effect.
    pub fn set_live_mixer_insert_stereo_separation_raw(
        &self,
        track: usize,
        separation_raw: Option<i32>,
    ) {
        let Some(target) = self.live_mixer_insert_stereo_separation_raw.get(track) else {
            return;
        };
        let value = separation_raw
            .filter(|value| (-64..=64).contains(value))
            .unwrap_or(i32::MIN);
        target.store(value, Ordering::Release);
    }

    pub(crate) fn live_mixer_insert_stereo_separation_raw(
        &self,
        track: usize,
    ) -> Option<Arc<AtomicI32>> {
        self.live_mixer_insert_stereo_separation_raw
            .get(track)
            .cloned()
    }

    /// Set a non-Master Mixer insert's live polarity and stereo-swap flags.
    ///
    /// The render worker reads this packed target without taking a lock and crossfades live
    /// changes over 20 ms. `None` uses the saved flags when available.
    pub fn set_live_mixer_insert_signal_flags(&self, track: usize, flags: Option<(bool, bool)>) {
        let Some(target) = self.live_mixer_insert_signal_flags.get(track) else {
            return;
        };
        let encoded = flags.map_or(i32::MIN, |(polarity_reversed, swap_left_right)| {
            i32::from(polarity_reversed) | (i32::from(swap_left_right) << 1)
        });
        target.store(encoded, Ordering::Release);
    }

    pub(crate) fn live_mixer_insert_signal_flags(&self, track: usize) -> Option<Arc<AtomicI32>> {
        self.live_mixer_insert_signal_flags.get(track).cloned()
    }

    /// Set a non-Master Mixer insert's live enabled/mute target.
    ///
    /// The render worker reads this value without taking a lock and smooths mute changes over
    /// 20 ms for active supported sources. `None` preserves the saved insert state.
    pub fn set_live_mixer_insert_enabled(&self, track: usize, enabled: Option<bool>) {
        let Some(target) = self.live_mixer_insert_enabled.get(track) else {
            return;
        };
        target.store(
            enabled.map_or(i32::MIN, |enabled| if enabled { 1 } else { 0 }),
            Ordering::Release,
        );
    }

    pub(crate) fn live_mixer_insert_enabled(&self, track: usize) -> Option<Arc<AtomicI32>> {
        self.live_mixer_insert_enabled.get(track).cloned()
    }

    /// Set a loaded Mixer effect slot's live wet/dry target using its saved FLP Mix value.
    ///
    /// The render worker reads the normalized target without taking a lock. FL Studio's raw
    /// Mix-to-wet curve has not been validated against a native render; the current mapping is
    /// linear from the documented raw range `-6400..=6400`.
    pub fn set_live_mixer_effect_mix_raw(
        &self,
        insert_ordinal: usize,
        slot_index: u16,
        mix_raw: Option<i32>,
    ) {
        let wet_mix = mixer_effect_wet_mix(mix_raw);
        let Ok(mut targets) = self.live_mixer_effect_mix_targets.lock() else {
            return;
        };
        let target = targets
            .entry((insert_ordinal, slot_index))
            .or_insert_with(|| Arc::new(AtomicU32::new(wet_mix.to_bits())));
        target.store(wet_mix.to_bits(), Ordering::Release);
    }

    pub(crate) fn live_mixer_effect_mix_target(
        &self,
        insert_ordinal: usize,
        slot_index: u16,
        initial_wet_mix: f32,
    ) -> Option<Arc<AtomicU32>> {
        let initial_wet_mix = if initial_wet_mix.is_finite() {
            initial_wet_mix.clamp(0.0, 1.0)
        } else {
            1.0
        };
        let mut targets = self.live_mixer_effect_mix_targets.lock().ok()?;
        let target = targets
            .entry((insert_ordinal, slot_index))
            .or_insert_with(|| Arc::new(AtomicU32::new(initial_wet_mix.to_bits())))
            .clone();
        target.store(initial_wet_mix.to_bits(), Ordering::Release);
        Some(target)
    }

    /// Set a loaded Mixer effect slot's enabled target without locking the audio callback.
    pub fn set_live_mixer_effect_slot_enabled(
        &self,
        insert_ordinal: usize,
        slot_index: u16,
        enabled: bool,
    ) {
        let Ok(mut targets) = self.live_mixer_effect_slot_enabled_targets.lock() else {
            return;
        };
        let target = targets
            .entry((insert_ordinal, slot_index))
            .or_insert_with(|| Arc::new(AtomicI32::new(i32::from(enabled))));
        target.store(i32::from(enabled), Ordering::Release);
    }

    pub(crate) fn live_mixer_effect_slot_enabled_target(
        &self,
        insert_ordinal: usize,
        slot_index: u16,
        initial_enabled: bool,
    ) -> Option<Arc<AtomicI32>> {
        let mut targets = self.live_mixer_effect_slot_enabled_targets.lock().ok()?;
        let target = targets
            .entry((insert_ordinal, slot_index))
            .or_insert_with(|| Arc::new(AtomicI32::new(i32::from(initial_enabled))))
            .clone();
        target.store(i32::from(initial_enabled), Ordering::Release);
        Some(target)
    }

    /// Set a loaded Mixer insert's live effects-chain enable target.
    pub fn set_live_mixer_insert_effects_enabled(&self, insert_ordinal: usize, enabled: bool) {
        let Ok(mut targets) = self.live_mixer_insert_effects_enabled_targets.lock() else {
            return;
        };
        let target = targets
            .entry(insert_ordinal)
            .or_insert_with(|| Arc::new(AtomicI32::new(i32::from(enabled))));
        target.store(i32::from(enabled), Ordering::Release);
    }

    pub(crate) fn live_mixer_insert_effects_enabled_target(
        &self,
        insert_ordinal: usize,
        initial_enabled: bool,
    ) -> Option<Arc<AtomicI32>> {
        let mut targets = self.live_mixer_insert_effects_enabled_targets.lock().ok()?;
        let target = targets
            .entry(insert_ordinal)
            .or_insert_with(|| Arc::new(AtomicI32::new(i32::from(initial_enabled))))
            .clone();
        target.store(i32::from(initial_enabled), Ordering::Release);
        Some(target)
    }

    /// Set the live solo state for every recognized Mixer insert.
    ///
    /// Direct-to-Master sources read these targets without locking and smooth solo changes over
    /// 20 ms. Send-routed sources continue to use their saved solo state.
    pub fn set_live_mixer_soloed_tracks(&self, soloed_tracks: &[usize]) {
        let mut mask = [0_u64; 2];
        for track in soloed_tracks
            .iter()
            .copied()
            .filter(|track| *track > 0 && *track < MIXER_TRACK_METER_COUNT)
        {
            mask[track / u64::BITS as usize] |= 1_u64 << (track % u64::BITS as usize);
        }
        self.live_mixer_solo_mask.store(Some(Arc::new(mask)));
    }

    pub(crate) fn live_mixer_solo_mask(&self) -> Arc<ArcSwapOption<[u64; 2]>> {
        Arc::clone(&self.live_mixer_solo_mask)
    }

    /// Set a uniquely paired Mixer send's live raw level target.
    ///
    /// The render worker receives the atomic target during stream preparation and smooths
    /// supported route changes over 20 ms without taking a lock on the audio thread.
    pub fn set_live_mixer_send_level_raw(
        &self,
        source_track: usize,
        destination_track: usize,
        level_raw: Option<i32>,
    ) {
        let Some(targets) = self.live_mixer_send_targets(source_track, destination_track) else {
            return;
        };
        let value = level_raw
            .filter(|value| (0..=16_000).contains(value))
            .unwrap_or(i32::MIN);
        targets.level_raw.store(value, Ordering::Release);
    }

    /// Set the live enabled target for one uniquely paired Mixer send route.
    ///
    /// The render worker smooths the route state over 20 ms without locking the audio callback.
    pub fn set_live_mixer_send_enabled(
        &self,
        source_track: usize,
        destination_track: usize,
        enabled: Option<bool>,
    ) {
        let Some(targets) = self.live_mixer_send_targets(source_track, destination_track) else {
            return;
        };
        targets.enabled.store(
            enabled.map_or(i32::MIN, |enabled| if enabled { 1 } else { 0 }),
            Ordering::Release,
        );
    }

    pub(crate) fn live_mixer_send_targets(
        &self,
        source_track: usize,
        destination_track: usize,
    ) -> Option<LiveMixerSendTargetAtomics> {
        if source_track >= MIXER_TRACK_METER_COUNT || destination_track >= MIXER_TRACK_METER_COUNT {
            return None;
        }
        let mut targets = self
            .live_mixer_send_targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(
            targets
                .entry((source_track, destination_track))
                .or_insert_with(|| LiveMixerSendTargetAtomics {
                    level_raw: Arc::new(AtomicI32::new(i32::MIN)),
                    enabled: Arc::new(AtomicI32::new(i32::MIN)),
                })
                .clone(),
        )
    }

    pub(crate) fn live_master_fader_gain(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.live_master_fader_gain)
    }

    /// Set the live Playlist stream's target Master pan, or clear it when no valid control exists.
    ///
    /// The render worker reads this value without taking a lock and smooths changes over 20 ms.
    pub fn set_live_master_pan_raw(&self, pan_raw: Option<i32>) {
        let pan_raw = pan_raw
            .filter(|pan_raw| (-6_400..=6_400).contains(pan_raw))
            .unwrap_or(i32::MIN);
        self.live_master_pan_raw.store(pan_raw, Ordering::Release);
    }

    pub(crate) fn live_master_pan_raw(&self) -> Arc<AtomicI32> {
        Arc::clone(&self.live_master_pan_raw)
    }

    /// Set the live Playlist stream's target Master stereo separation.
    ///
    /// The render worker reads this value without taking a lock and smooths changes over 20 ms.
    pub fn set_live_master_stereo_separation_raw(&self, separation_raw: Option<i32>) {
        let separation_raw = separation_raw
            .filter(|separation_raw| (-64..=64).contains(separation_raw))
            .unwrap_or(i32::MIN);
        self.live_master_stereo_separation_raw
            .store(separation_raw, Ordering::Release);
    }

    pub(crate) fn live_master_stereo_separation_raw(&self) -> Arc<AtomicI32> {
        Arc::clone(&self.live_master_stereo_separation_raw)
    }

    /// Set the live Playlist stream's Master enabled state, or clear the target when unknown.
    ///
    /// The render worker reads this value without taking a lock and smooths mute changes over 20 ms.
    pub fn set_live_master_enabled(&self, enabled: Option<bool>) {
        self.live_master_enabled.store(
            enabled.map_or(i32::MIN, |enabled| if enabled { 1 } else { 0 }),
            Ordering::Release,
        );
    }

    pub(crate) fn live_master_enabled(&self) -> Arc<AtomicI32> {
        Arc::clone(&self.live_master_enabled)
    }

    /// Set the live Playlist stream's Master polarity and stereo-swap state.
    ///
    /// The render worker reads this packed flag value without taking a lock and smooths changes
    /// over 20 ms. `None` preserves the saved document state when the flag record is unavailable.
    pub fn set_live_master_signal_flags(&self, flags: Option<(bool, bool)>) {
        let encoded = flags.map_or(i32::MIN, |(polarity_reversed, swap_left_right)| {
            i32::from(polarity_reversed) | (i32::from(swap_left_right) << 1)
        });
        self.live_master_signal_flags
            .store(encoded, Ordering::Release);
    }

    pub(crate) fn live_master_signal_flags(&self) -> Arc<AtomicI32> {
        Arc::clone(&self.live_master_signal_flags)
    }

    #[cfg(test)]
    pub(crate) fn publish_peak(&self, track: usize, peak: f32) {
        self.publish_stereo_peak(track, peak, peak);
    }

    pub(crate) fn publish_stereo_peak(&self, track: usize, left: f32, right: f32) {
        let Some(combined) = self.peaks.get(track) else {
            return;
        };
        publish_positive_peak(combined, left.max(right));
        if let Some(slot) = self.script_peaks_left.get(track) {
            publish_positive_peak(slot, left);
        }
        if let Some(slot) = self.script_peaks_right.get(track) {
            publish_positive_peak(slot, right);
        }
    }
}

fn publish_positive_peak(slot: &AtomicU32, peak: f32) {
    if peak.is_finite() && peak > 0.0 {
        slot.fetch_max(peak.to_bits(), Ordering::Release);
    }
}

#[derive(Debug)]
pub(crate) struct LiveMixerSourceControl {
    route_plan: crate::MixerLiveSourceRoutePlan,
    current_gain_bits: Arc<AtomicU32>,
    current_pan_raw_bits: Arc<AtomicU32>,
    current_stereo_separation_raw_bits: Arc<AtomicU32>,
    current_polarity_mix_bits: Arc<AtomicU32>,
    current_swap_mix_bits: Arc<AtomicU32>,
    current_enabled_gain_bits: Arc<AtomicU32>,
    current_solo_gain_bits: Arc<AtomicU32>,
    current_send_gain_bits: Vec<Arc<AtomicU32>>,
    current_send_enabled_gain_bits: Vec<Arc<AtomicU32>>,
    current_send_output_solo_gain_bits: Vec<Arc<AtomicU32>>,
    current_send_meter_solo_gain_bits: Vec<Arc<AtomicU32>>,
    current_insert_enabled_gain_bits: Vec<Arc<AtomicU32>>,
    gain_started: Arc<AtomicBool>,
    pan_started: Arc<AtomicBool>,
    stereo_separation_started: Arc<AtomicBool>,
    signal_flags_started: Arc<AtomicBool>,
    enabled_started: Arc<AtomicBool>,
    solo_started: Arc<AtomicBool>,
    send_started: Vec<Arc<AtomicBool>>,
    send_enabled_started: Vec<Arc<AtomicBool>>,
    send_output_solo_started: Vec<Arc<AtomicBool>>,
    send_meter_solo_started: Vec<Arc<AtomicBool>>,
    insert_enabled_started: Vec<Arc<AtomicBool>>,
    smoothing_alpha: f32,
}

impl Clone for LiveMixerSourceControl {
    fn clone(&self) -> Self {
        Self {
            route_plan: self.route_plan.clone(),
            current_gain_bits: Arc::new(AtomicU32::new(
                self.current_gain_bits.load(Ordering::Relaxed),
            )),
            current_pan_raw_bits: Arc::new(AtomicU32::new(
                self.current_pan_raw_bits.load(Ordering::Relaxed),
            )),
            current_stereo_separation_raw_bits: Arc::new(AtomicU32::new(
                self.current_stereo_separation_raw_bits
                    .load(Ordering::Relaxed),
            )),
            current_polarity_mix_bits: Arc::new(AtomicU32::new(
                self.current_polarity_mix_bits.load(Ordering::Relaxed),
            )),
            current_swap_mix_bits: Arc::new(AtomicU32::new(
                self.current_swap_mix_bits.load(Ordering::Relaxed),
            )),
            current_enabled_gain_bits: Arc::new(AtomicU32::new(
                self.current_enabled_gain_bits.load(Ordering::Relaxed),
            )),
            current_solo_gain_bits: Arc::new(AtomicU32::new(
                self.current_solo_gain_bits.load(Ordering::Relaxed),
            )),
            current_send_gain_bits: self
                .current_send_gain_bits
                .iter()
                .map(|gain| Arc::new(AtomicU32::new(gain.load(Ordering::Relaxed))))
                .collect(),
            current_send_enabled_gain_bits: self
                .current_send_enabled_gain_bits
                .iter()
                .map(|gain| Arc::new(AtomicU32::new(gain.load(Ordering::Relaxed))))
                .collect(),
            current_send_output_solo_gain_bits: self
                .current_send_output_solo_gain_bits
                .iter()
                .map(|gain| Arc::new(AtomicU32::new(gain.load(Ordering::Relaxed))))
                .collect(),
            current_send_meter_solo_gain_bits: self
                .current_send_meter_solo_gain_bits
                .iter()
                .map(|gain| Arc::new(AtomicU32::new(gain.load(Ordering::Relaxed))))
                .collect(),
            current_insert_enabled_gain_bits: self
                .current_insert_enabled_gain_bits
                .iter()
                .map(|gain| Arc::new(AtomicU32::new(gain.load(Ordering::Relaxed))))
                .collect(),
            gain_started: Arc::new(AtomicBool::new(self.gain_started.load(Ordering::Relaxed))),
            pan_started: Arc::new(AtomicBool::new(self.pan_started.load(Ordering::Relaxed))),
            stereo_separation_started: Arc::new(AtomicBool::new(
                self.stereo_separation_started.load(Ordering::Relaxed),
            )),
            signal_flags_started: Arc::new(AtomicBool::new(
                self.signal_flags_started.load(Ordering::Relaxed),
            )),
            enabled_started: Arc::new(AtomicBool::new(
                self.enabled_started.load(Ordering::Relaxed),
            )),
            solo_started: Arc::new(AtomicBool::new(self.solo_started.load(Ordering::Relaxed))),
            send_started: self
                .send_started
                .iter()
                .map(|started| Arc::new(AtomicBool::new(started.load(Ordering::Relaxed))))
                .collect(),
            send_enabled_started: self
                .send_enabled_started
                .iter()
                .map(|started| Arc::new(AtomicBool::new(started.load(Ordering::Relaxed))))
                .collect(),
            send_output_solo_started: self
                .send_output_solo_started
                .iter()
                .map(|started| Arc::new(AtomicBool::new(started.load(Ordering::Relaxed))))
                .collect(),
            send_meter_solo_started: self
                .send_meter_solo_started
                .iter()
                .map(|started| Arc::new(AtomicBool::new(started.load(Ordering::Relaxed))))
                .collect(),
            insert_enabled_started: self
                .insert_enabled_started
                .iter()
                .map(|started| Arc::new(AtomicBool::new(started.load(Ordering::Relaxed))))
                .collect(),
            smoothing_alpha: self.smoothing_alpha,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LiveMixerSourceTargets {
    fader_raw: Arc<AtomicI32>,
    pan_raw: Arc<AtomicI32>,
    stereo_separation_raw: Arc<AtomicI32>,
    signal_flags: Arc<AtomicI32>,
    enabled: Arc<AtomicI32>,
    solo_mask: Option<Arc<ArcSwapOption<[u64; 2]>>>,
    send_levels: Vec<Arc<AtomicI32>>,
    send_enabled: Vec<Arc<AtomicI32>>,
    route_insert_enabled: Vec<Arc<AtomicI32>>,
}

#[derive(Clone, Copy)]
pub(crate) struct LiveMixerSourceFrameControls {
    gain: f32,
    pan_raw: Option<f32>,
    stereo_separation_raw: Option<f32>,
    signal_flags_mix: Option<(f32, f32)>,
    enabled_gain: Option<f32>,
    solo_gain: Option<f32>,
    pan_law_raw: Option<u8>,
}

impl LiveMixerSourceControl {
    pub(crate) fn new(route_plan: crate::MixerLiveSourceRoutePlan, sample_rate: u32) -> Self {
        let current_send_gain_bits = route_plan
            .saved_live_send_gains
            .iter()
            .map(|gain| Arc::new(AtomicU32::new(gain.to_bits())))
            .collect();
        let send_started = route_plan
            .saved_live_send_gains
            .iter()
            .map(|_| Arc::new(AtomicBool::new(false)))
            .collect();
        let current_send_enabled_gain_bits = route_plan
            .saved_live_send_enabled
            .iter()
            .map(|enabled| {
                Arc::new(AtomicU32::new(
                    if *enabled { 1.0_f32 } else { 0.0 }.to_bits(),
                ))
            })
            .collect();
        let send_enabled_started = route_plan
            .saved_live_send_enabled
            .iter()
            .map(|_| Arc::new(AtomicBool::new(false)))
            .collect();
        let current_send_output_solo_gain_bits = route_plan
            .saved_live_send_output_solo_gains
            .iter()
            .map(|gain| Arc::new(AtomicU32::new(gain.to_bits())))
            .collect();
        let send_output_solo_started = route_plan
            .saved_live_send_output_solo_gains
            .iter()
            .map(|_| Arc::new(AtomicBool::new(false)))
            .collect();
        let current_send_meter_solo_gain_bits = route_plan
            .saved_live_send_meter_solo_gains
            .iter()
            .map(|gain| Arc::new(AtomicU32::new(gain.to_bits())))
            .collect();
        let send_meter_solo_started = route_plan
            .saved_live_send_meter_solo_gains
            .iter()
            .map(|_| Arc::new(AtomicBool::new(false)))
            .collect();
        let current_insert_enabled_gain_bits = route_plan
            .saved_live_insert_enabled
            .iter()
            .map(|enabled| {
                Arc::new(AtomicU32::new(
                    if *enabled { 1.0_f32 } else { 0.0 }.to_bits(),
                ))
            })
            .collect();
        let insert_enabled_started = route_plan
            .saved_live_insert_enabled
            .iter()
            .map(|_| Arc::new(AtomicBool::new(false)))
            .collect();
        let initial_solo_gain: f32 = route_plan.live_solo.map_or(1.0, |solo| {
            if !solo.any_soloed || solo.soloed {
                1.0
            } else {
                0.0
            }
        });
        Self {
            current_gain_bits: Arc::new(AtomicU32::new(route_plan.saved_fader_gain.to_bits())),
            current_pan_raw_bits: Arc::new(AtomicU32::new(
                (route_plan.saved_pan_raw.unwrap_or_default() as f32).to_bits(),
            )),
            current_stereo_separation_raw_bits: Arc::new(AtomicU32::new(
                (route_plan.saved_stereo_separation_raw.unwrap_or_default() as f32).to_bits(),
            )),
            current_polarity_mix_bits: Arc::new(AtomicU32::new(
                if route_plan
                    .saved_signal_flags
                    .is_some_and(|(polarity_reversed, _)| polarity_reversed)
                {
                    1.0_f32.to_bits()
                } else {
                    0.0_f32.to_bits()
                },
            )),
            current_swap_mix_bits: Arc::new(AtomicU32::new(
                if route_plan
                    .saved_signal_flags
                    .is_some_and(|(_, swap_left_right)| swap_left_right)
                {
                    1.0_f32.to_bits()
                } else {
                    0.0_f32.to_bits()
                },
            )),
            current_enabled_gain_bits: Arc::new(AtomicU32::new(
                if route_plan.saved_enabled == Some(false) {
                    0.0_f32.to_bits()
                } else {
                    1.0_f32.to_bits()
                },
            )),
            current_solo_gain_bits: Arc::new(AtomicU32::new(initial_solo_gain.to_bits())),
            current_send_gain_bits,
            current_send_enabled_gain_bits,
            current_send_output_solo_gain_bits,
            current_send_meter_solo_gain_bits,
            current_insert_enabled_gain_bits,
            gain_started: Arc::new(AtomicBool::new(false)),
            pan_started: Arc::new(AtomicBool::new(false)),
            stereo_separation_started: Arc::new(AtomicBool::new(false)),
            signal_flags_started: Arc::new(AtomicBool::new(false)),
            enabled_started: Arc::new(AtomicBool::new(false)),
            solo_started: Arc::new(AtomicBool::new(false)),
            send_started,
            send_enabled_started,
            send_output_solo_started,
            send_meter_solo_started,
            insert_enabled_started,
            smoothing_alpha: output_gain_smoothing_alpha(sample_rate as f32),
            route_plan,
        }
    }

    pub(crate) fn targets(&self, meters: &MixerTrackPeakMeters) -> Option<LiveMixerSourceTargets> {
        let send_targets = self
            .route_plan
            .live_send_target_keys
            .iter()
            .map(|key| meters.live_mixer_send_targets(key.source_track, key.destination_track))
            .collect::<Option<Vec<_>>>()?;
        let send_levels = send_targets
            .iter()
            .map(|targets| Arc::clone(&targets.level_raw))
            .collect();
        let send_enabled = send_targets
            .into_iter()
            .map(|targets| targets.enabled)
            .collect();
        let route_insert_enabled = self
            .route_plan
            .live_insert_enabled_tracks
            .iter()
            .map(|track| meters.live_mixer_insert_enabled(*track))
            .collect::<Option<Vec<_>>>()?;
        let solo_mask = if self.route_plan.live_solo.is_some()
            || self.route_plan.live_send_output_paths.is_some()
        {
            Some(meters.live_mixer_solo_mask())
        } else {
            None
        };
        Some(LiveMixerSourceTargets {
            fader_raw: meters.live_mixer_insert_fader_raw(self.route_plan.track)?,
            pan_raw: meters.live_mixer_insert_pan_raw(self.route_plan.track)?,
            stereo_separation_raw: meters
                .live_mixer_insert_stereo_separation_raw(self.route_plan.track)?,
            signal_flags: meters.live_mixer_insert_signal_flags(self.route_plan.track)?,
            enabled: meters.live_mixer_insert_enabled(self.route_plan.track)?,
            solo_mask,
            send_levels,
            send_enabled,
            route_insert_enabled,
        })
    }

    pub(crate) fn insert_meter_transforms(&self) -> &[(usize, crate::MixerSourceRouteTransform)] {
        &self.route_plan.insert_meter_transforms
    }

    pub(crate) fn insert_effect_input_transforms(
        &self,
    ) -> &[(usize, crate::MixerSourceRouteTransform)] {
        &self.route_plan.insert_effect_input_transforms
    }

    pub(crate) fn next_frame_controls(
        &self,
        targets: &LiveMixerSourceTargets,
    ) -> LiveMixerSourceFrameControls {
        let fader_target = targets.fader_raw.load(Ordering::Acquire);
        let target_gain = if (0..=16_000).contains(&fader_target) {
            fader_target as f32 / 10_000.0
        } else {
            self.route_plan.saved_fader_gain
        };
        let current_gain = if self.gain_started.swap(true, Ordering::Relaxed) {
            f32::from_bits(self.current_gain_bits.load(Ordering::Relaxed))
        } else {
            target_gain
        };
        let gain = smooth_output_gain(current_gain, target_gain, self.smoothing_alpha);
        self.current_gain_bits
            .store(gain.to_bits(), Ordering::Relaxed);

        let pan_target = targets.pan_raw.load(Ordering::Acquire);
        let target_pan_raw = if (-6_400..=6_400).contains(&pan_target) {
            Some(pan_target as f32)
        } else {
            self.route_plan.saved_pan_raw.map(|pan_raw| pan_raw as f32)
        };
        let pan_raw = target_pan_raw.map(|target_pan_raw| {
            let current_pan_raw = if self.pan_started.swap(true, Ordering::Relaxed) {
                f32::from_bits(self.current_pan_raw_bits.load(Ordering::Relaxed))
            } else {
                target_pan_raw
            };
            let next_pan_raw =
                current_pan_raw + (target_pan_raw - current_pan_raw) * self.smoothing_alpha;
            self.current_pan_raw_bits
                .store(next_pan_raw.to_bits(), Ordering::Relaxed);
            next_pan_raw
        });

        let separation_target = targets.stereo_separation_raw.load(Ordering::Acquire);
        let target_separation_raw = if (-64..=64).contains(&separation_target) {
            Some(separation_target as f32)
        } else {
            self.route_plan
                .saved_stereo_separation_raw
                .map(|raw| raw as f32)
        };
        let stereo_separation_raw = target_separation_raw.map(|target_separation_raw| {
            let current_separation_raw =
                if self.stereo_separation_started.swap(true, Ordering::Relaxed) {
                    f32::from_bits(
                        self.current_stereo_separation_raw_bits
                            .load(Ordering::Relaxed),
                    )
                } else {
                    target_separation_raw
                };
            let next_separation_raw = current_separation_raw
                + (target_separation_raw - current_separation_raw) * self.smoothing_alpha;
            self.current_stereo_separation_raw_bits
                .store(next_separation_raw.to_bits(), Ordering::Relaxed);
            next_separation_raw
        });

        let signal_flags_mix = self.route_plan.saved_signal_flags.map(|saved_flags| {
            let flags_target = targets.signal_flags.load(Ordering::Acquire);
            let target_polarity_mix = if flags_target == i32::MIN {
                if saved_flags.0 { 1.0 } else { 0.0 }
            } else {
                if flags_target & 1 != 0 { 1.0 } else { 0.0 }
            };
            let target_swap_mix = if flags_target == i32::MIN {
                if saved_flags.1 { 1.0 } else { 0.0 }
            } else {
                if flags_target & 2 != 0 { 1.0 } else { 0.0 }
            };
            let (current_polarity_mix, current_swap_mix) =
                if self.signal_flags_started.swap(true, Ordering::Relaxed) {
                    (
                        f32::from_bits(self.current_polarity_mix_bits.load(Ordering::Relaxed)),
                        f32::from_bits(self.current_swap_mix_bits.load(Ordering::Relaxed)),
                    )
                } else {
                    (target_polarity_mix, target_swap_mix)
                };
            let polarity_mix = current_polarity_mix
                + (target_polarity_mix - current_polarity_mix) * self.smoothing_alpha;
            let swap_mix =
                current_swap_mix + (target_swap_mix - current_swap_mix) * self.smoothing_alpha;
            self.current_polarity_mix_bits
                .store(polarity_mix.to_bits(), Ordering::Relaxed);
            self.current_swap_mix_bits
                .store(swap_mix.to_bits(), Ordering::Relaxed);
            (polarity_mix, swap_mix)
        });

        let enabled_gain = self.route_plan.saved_enabled.map(|saved_enabled| {
            let enabled_target = targets.enabled.load(Ordering::Acquire);
            let target_gain = match enabled_target {
                0 => 0.0,
                1 => 1.0,
                _ if saved_enabled => 1.0,
                _ => 0.0,
            };
            let current_gain = if self.enabled_started.swap(true, Ordering::Relaxed) {
                f32::from_bits(self.current_enabled_gain_bits.load(Ordering::Relaxed))
            } else {
                target_gain
            };
            let next_gain =
                crate::audio::smooth_output_gain(current_gain, target_gain, self.smoothing_alpha);
            self.current_enabled_gain_bits
                .store(next_gain.to_bits(), Ordering::Relaxed);
            next_gain
        });

        let live_solo_mask = targets
            .solo_mask
            .as_ref()
            .and_then(|target| target.load_full());
        let solo_gain = self.route_plan.live_solo.map(|saved_solo| {
            let (any_soloed, soloed) = live_solo_mask.as_ref().map_or(
                (saved_solo.any_soloed, saved_solo.soloed),
                |mask| {
                    let track_mask = 1_u64 << (self.route_plan.track % u64::BITS as usize);
                    let track_word = self.route_plan.track / u64::BITS as usize;
                    (
                        mask.iter().any(|word| *word != 0),
                        mask.get(track_word)
                            .is_some_and(|word| word & track_mask != 0),
                    )
                },
            );
            let target_gain = if !any_soloed || soloed { 1.0 } else { 0.0 };
            let current_gain = if self.solo_started.swap(true, Ordering::Relaxed) {
                f32::from_bits(self.current_solo_gain_bits.load(Ordering::Relaxed))
            } else {
                target_gain
            };
            let next_gain = smooth_output_gain(current_gain, target_gain, self.smoothing_alpha);
            self.current_solo_gain_bits
                .store(next_gain.to_bits(), Ordering::Relaxed);
            next_gain
        });

        for (((path_tracks, current_gain_bits), started), saved_gain) in self
            .route_plan
            .live_send_output_solo_tracks
            .iter()
            .zip(&self.current_send_output_solo_gain_bits)
            .zip(&self.send_output_solo_started)
            .zip(&self.route_plan.saved_live_send_output_solo_gains)
        {
            let target_gain =
                live_solo_path_gain(live_solo_mask.as_deref(), path_tracks, *saved_gain);
            let current_gain = if started.swap(true, Ordering::Relaxed) {
                f32::from_bits(current_gain_bits.load(Ordering::Relaxed))
            } else {
                target_gain
            };
            let next_gain = smooth_output_gain(current_gain, target_gain, self.smoothing_alpha);
            current_gain_bits.store(next_gain.to_bits(), Ordering::Relaxed);
        }

        for (((path_tracks, current_gain_bits), started), saved_gain) in self
            .route_plan
            .live_send_meter_solo_tracks
            .iter()
            .zip(&self.current_send_meter_solo_gain_bits)
            .zip(&self.send_meter_solo_started)
            .zip(&self.route_plan.saved_live_send_meter_solo_gains)
        {
            let target_gain =
                live_solo_path_gain(live_solo_mask.as_deref(), path_tracks, *saved_gain);
            let current_gain = if started.swap(true, Ordering::Relaxed) {
                f32::from_bits(current_gain_bits.load(Ordering::Relaxed))
            } else {
                target_gain
            };
            let next_gain = smooth_output_gain(current_gain, target_gain, self.smoothing_alpha);
            current_gain_bits.store(next_gain.to_bits(), Ordering::Relaxed);
        }

        for (((target, current_gain_bits), started), saved_gain) in targets
            .send_levels
            .iter()
            .zip(&self.current_send_gain_bits)
            .zip(&self.send_started)
            .zip(&self.route_plan.saved_live_send_gains)
        {
            let level_raw = target.load(Ordering::Acquire);
            let target_gain = if (0..=16_000).contains(&level_raw) {
                level_raw as f32 / 12_800.0
            } else {
                *saved_gain
            };
            let current_gain = if started.swap(true, Ordering::Relaxed) {
                f32::from_bits(current_gain_bits.load(Ordering::Relaxed))
            } else {
                target_gain
            };
            let next_gain =
                crate::audio::smooth_output_gain(current_gain, target_gain, self.smoothing_alpha);
            current_gain_bits.store(next_gain.to_bits(), Ordering::Relaxed);
        }

        for (((target, current_enabled_gain_bits), started), saved_enabled) in targets
            .send_enabled
            .iter()
            .zip(&self.current_send_enabled_gain_bits)
            .zip(&self.send_enabled_started)
            .zip(&self.route_plan.saved_live_send_enabled)
        {
            let enabled_target = target.load(Ordering::Acquire);
            let target_gain = match enabled_target {
                0 => 0.0,
                1 => 1.0,
                _ if *saved_enabled => 1.0,
                _ => 0.0,
            };
            let current_gain = if started.swap(true, Ordering::Relaxed) {
                f32::from_bits(current_enabled_gain_bits.load(Ordering::Relaxed))
            } else {
                target_gain
            };
            let next_gain =
                crate::audio::smooth_output_gain(current_gain, target_gain, self.smoothing_alpha);
            current_enabled_gain_bits.store(next_gain.to_bits(), Ordering::Relaxed);
        }

        for (((target, current_enabled_gain_bits), started), saved_enabled) in targets
            .route_insert_enabled
            .iter()
            .zip(&self.current_insert_enabled_gain_bits)
            .zip(&self.insert_enabled_started)
            .zip(&self.route_plan.saved_live_insert_enabled)
        {
            let enabled_target = target.load(Ordering::Acquire);
            let target_gain = match enabled_target {
                0 => 0.0,
                1 => 1.0,
                _ if *saved_enabled => 1.0,
                _ => 0.0,
            };
            let current_gain = if started.swap(true, Ordering::Relaxed) {
                f32::from_bits(current_enabled_gain_bits.load(Ordering::Relaxed))
            } else {
                target_gain
            };
            let next_gain = smooth_output_gain(current_gain, target_gain, self.smoothing_alpha);
            current_enabled_gain_bits.store(next_gain.to_bits(), Ordering::Relaxed);
        }

        LiveMixerSourceFrameControls {
            gain,
            pan_raw,
            stereo_separation_raw,
            signal_flags_mix,
            enabled_gain,
            solo_gain,
            pan_law_raw: self.route_plan.pan_law_raw,
        }
    }

    pub(crate) fn apply_output_frame(
        &self,
        controls: LiveMixerSourceFrameControls,
        frame: &mut [f32; 2],
    ) {
        self.route_plan.source_transform.apply_frame(frame);
        if let Some(enabled_gain) = controls.enabled_gain {
            frame[0] *= enabled_gain;
            frame[1] *= enabled_gain;
        }
        if let Some(solo_gain) = controls.solo_gain {
            frame[0] *= solo_gain;
            frame[1] *= solo_gain;
        }
        if let Some((polarity_mix, swap_mix)) = controls.signal_flags_mix {
            crate::apply_mixer_signal_flags_f32(frame, polarity_mix, swap_mix);
        }
        if let Some(separation_raw) = controls.stereo_separation_raw {
            crate::apply_mixer_stereo_separation_raw_f32(frame, separation_raw);
        }
        if let Some(pan_raw) = controls.pan_raw {
            crate::apply_mixer_pan_raw_f32(frame, pan_raw, controls.pan_law_raw);
        }
        frame[0] *= controls.gain;
        frame[1] *= controls.gain;

        if let Some(paths) = &self.route_plan.live_send_output_paths {
            let source_frame = *frame;
            let mut routed_frame = [0.0; 2];
            for (path_index, path) in paths.iter().enumerate() {
                let solo_gain = self
                    .current_send_output_solo_gain_bits
                    .get(path_index)
                    .map_or(1.0, |gain| f32::from_bits(gain.load(Ordering::Relaxed)));
                let gain =
                    self.send_path_gain(&path.send_gains, &path.insert_enabled_gains, solo_gain);
                if gain == 0.0 {
                    continue;
                }
                let mut branch_frame = source_frame;
                path.transform.apply_frame(&mut branch_frame);
                routed_frame[0] += branch_frame[0] * gain;
                routed_frame[1] += branch_frame[1] * gain;
            }
            *frame = routed_frame;
        } else {
            self.route_plan.output_transform.apply_frame(frame);
        }
    }

    pub(crate) fn apply_meter_frame(
        &self,
        controls: LiveMixerSourceFrameControls,
        track: usize,
        route_transform: crate::MixerSourceRouteTransform,
        frame: &mut [f32; 2],
    ) {
        self.route_plan.source_transform.apply_frame(frame);
        if let Some(enabled_gain) = controls.enabled_gain {
            frame[0] *= enabled_gain;
            frame[1] *= enabled_gain;
        }
        if let Some(solo_gain) = controls.solo_gain {
            frame[0] *= solo_gain;
            frame[1] *= solo_gain;
        }
        if let Some((polarity_mix, swap_mix)) = controls.signal_flags_mix {
            crate::apply_mixer_signal_flags_f32(frame, polarity_mix, swap_mix);
        }
        if let Some(separation_raw) = controls.stereo_separation_raw {
            crate::apply_mixer_stereo_separation_raw_f32(frame, separation_raw);
        }
        if let Some(pan_raw) = controls.pan_raw {
            crate::apply_mixer_pan_raw_f32(frame, pan_raw, controls.pan_law_raw);
        }
        frame[0] *= controls.gain;
        frame[1] *= controls.gain;

        if self.route_plan.live_send_output_paths.is_some() {
            let source_frame = *frame;
            let mut meter_frame = [0.0; 2];
            let mut found_path = false;
            for (path_index, path) in self
                .route_plan
                .live_send_meter_paths
                .iter()
                .enumerate()
                .filter(|(_, path)| path.track == track)
            {
                found_path = true;
                let solo_gain = self
                    .current_send_meter_solo_gain_bits
                    .get(path_index)
                    .map_or(1.0, |gain| f32::from_bits(gain.load(Ordering::Relaxed)));
                let gain =
                    self.send_path_gain(&path.send_gains, &path.insert_enabled_gains, solo_gain);
                if gain == 0.0 {
                    continue;
                }
                let mut branch_frame = source_frame;
                path.transform.apply_frame(&mut branch_frame);
                meter_frame[0] += branch_frame[0] * gain;
                meter_frame[1] += branch_frame[1] * gain;
            }
            if found_path {
                *frame = meter_frame;
            } else {
                route_transform.apply_frame(frame);
            }
        } else {
            route_transform.apply_frame(frame);
        }
    }

    pub(crate) fn apply_effect_input_frame(
        &self,
        controls: LiveMixerSourceFrameControls,
        track: usize,
        route_transform: crate::MixerSourceRouteTransform,
        frame: &mut [f32; 2],
    ) {
        self.route_plan.source_transform.apply_frame(frame);
        if let Some(enabled_gain) = controls.enabled_gain {
            frame[0] *= enabled_gain;
            frame[1] *= enabled_gain;
        }
        if let Some(solo_gain) = controls.solo_gain {
            frame[0] *= solo_gain;
            frame[1] *= solo_gain;
        }
        if let Some((polarity_mix, swap_mix)) = controls.signal_flags_mix {
            crate::apply_mixer_signal_flags_f32(frame, polarity_mix, swap_mix);
        }
        if let Some(separation_raw) = controls.stereo_separation_raw {
            crate::apply_mixer_stereo_separation_raw_f32(frame, separation_raw);
        }
        if let Some(pan_raw) = controls.pan_raw {
            crate::apply_mixer_pan_raw_f32(frame, pan_raw, controls.pan_law_raw);
        }
        frame[0] *= controls.gain;
        frame[1] *= controls.gain;

        if self.route_plan.live_send_output_paths.is_some() {
            let source_frame = *frame;
            let mut input_frame = [0.0; 2];
            let mut found_path = false;
            for (path_index, path) in self
                .route_plan
                .live_send_effect_input_paths
                .iter()
                .enumerate()
                .filter(|(_, path)| path.track == track)
            {
                found_path = true;
                let solo_gain = self
                    .current_send_meter_solo_gain_bits
                    .get(path_index)
                    .map_or(1.0, |gain| f32::from_bits(gain.load(Ordering::Relaxed)));
                let gain =
                    self.send_path_gain(&path.send_gains, &path.insert_enabled_gains, solo_gain);
                if gain == 0.0 {
                    continue;
                }
                let mut branch_frame = source_frame;
                path.transform.apply_frame(&mut branch_frame);
                input_frame[0] += branch_frame[0] * gain;
                input_frame[1] += branch_frame[1] * gain;
            }
            if found_path {
                *frame = input_frame;
            } else {
                route_transform.apply_frame(frame);
            }
        } else {
            route_transform.apply_frame(frame);
        }
    }

    fn send_path_gain(
        &self,
        send_gains: &[crate::MixerLiveSendGain],
        insert_enabled_gains: &[crate::MixerLiveInsertEnabledGain],
        solo_gain: f32,
    ) -> f32 {
        let send_gain = send_gains.iter().fold(1.0, |gain, route| {
            let send_gain = route.target_index.map_or(route.saved_gain, |index| {
                self.current_send_gain_bits
                    .get(index)
                    .map_or(route.saved_gain, |target| {
                        f32::from_bits(target.load(Ordering::Relaxed))
                    })
            });
            let enabled_gain = route.enabled_target_index.map_or(
                if route.saved_enabled { 1.0 } else { 0.0 },
                |index| {
                    self.current_send_enabled_gain_bits
                        .get(index)
                        .map_or(if route.saved_enabled { 1.0 } else { 0.0 }, |target| {
                            f32::from_bits(target.load(Ordering::Relaxed))
                        })
                },
            );
            gain * send_gain * enabled_gain
        });
        insert_enabled_gains.iter().fold(send_gain, |gain, insert| {
            let enabled_gain =
                insert
                    .target_index
                    .map_or(if insert.saved_enabled { 1.0 } else { 0.0 }, |index| {
                        self.current_insert_enabled_gain_bits
                            .get(index)
                            .map_or(if insert.saved_enabled { 1.0 } else { 0.0 }, |target| {
                                f32::from_bits(target.load(Ordering::Relaxed))
                            })
                    });
            gain * enabled_gain
        }) * solo_gain
    }
}

fn live_solo_path_gain(mask: Option<&[u64; 2]>, path_tracks: &[usize], saved_gain: f32) -> f32 {
    let Some(mask) = mask else {
        return saved_gain;
    };
    let any_soloed = mask.iter().any(|word| *word != 0);
    if !any_soloed
        || path_tracks.iter().any(|track| {
            let track_word = track / u64::BITS as usize;
            let track_mask = 1_u64 << (track % u64::BITS as usize);
            mask.get(track_word)
                .is_some_and(|word| word & track_mask != 0)
        })
    {
        1.0
    } else {
        0.0
    }
}

/// A bounded stereo ring buffer used to align source timing in the Mixer.
#[derive(Default)]
pub(crate) struct StereoFrameDelay {
    frames: Vec<[f32; 2]>,
    cursor: usize,
}

impl StereoFrameDelay {
    pub(crate) fn new(delay_frames: u64) -> Result<Self, String> {
        let capacity = usize::try_from(delay_frames)
            .map_err(|_| "PDC delay is too large for this platform".to_owned())?;
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(capacity)
            .map_err(|error| format!("could not allocate PDC delay line: {error}"))?;
        frames.resize(capacity, [0.0; 2]);
        Ok(Self { frames, cursor: 0 })
    }

    pub(crate) fn delay_frames(&self) -> u64 {
        self.frames.len() as u64
    }

    pub(crate) fn process_interleaved(&mut self, samples: &mut [f32]) -> Result<(), String> {
        if !samples.len().is_multiple_of(2) {
            return Err("PDC delay buffer must contain stereo frames".to_owned());
        }
        if self.frames.is_empty() {
            return Ok(());
        }
        for frame in samples.as_chunks_mut::<2>().0 {
            self.process_frame(frame);
        }
        Ok(())
    }

    pub(crate) fn process_frame(&mut self, frame: &mut [f32; 2]) {
        if self.frames.is_empty() {
            return;
        }
        let delayed = self.frames[self.cursor];
        self.frames[self.cursor] = *frame;
        frame.copy_from_slice(&delayed);
        self.cursor += 1;
        if self.cursor == self.frames.len() {
            self.cursor = 0;
        }
    }
}

/// Enable flush-to-zero for floating-point work on the current audio/render thread.
///
/// x86-64 uses MXCSR.FTZ and MXCSR.DAZ; AArch64 uses FPCR.FZ. The floating-point
/// control state belongs to the calling thread and is left enabled for its lifetime.
#[inline]
pub(crate) fn enable_denormal_protection() {
    #[cfg(target_arch = "x86_64")]
    {
        const FTZ_AND_DAZ: u32 = (1 << 15) | (1 << 6);
        write_mxcsr(read_mxcsr() | FTZ_AND_DAZ);
    }

    #[cfg(target_arch = "aarch64")]
    unsafe {
        let control: u64;
        std::arch::asm!(
            "mrs {control}, fpcr",
            control = out(reg) control,
            options(nostack, preserves_flags)
        );
        std::arch::asm!(
            "msr fpcr, {control}",
            control = in(reg) (control | (1 << 24)),
            options(nostack, preserves_flags)
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn read_mxcsr() -> u32 {
    let mut value = 0_u32;
    unsafe {
        std::arch::asm!(
            "stmxcsr [{address}]",
            address = in(reg) &mut value,
            options(nostack, preserves_flags)
        );
    }
    value
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn write_mxcsr(value: u32) {
    unsafe {
        std::arch::asm!(
            "ldmxcsr [{address}]",
            address = in(reg) &value,
            options(nostack, preserves_flags)
        );
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AudioAccess {
    #[default]
    Shared,
    Exclusive,
}

impl AudioAccess {
    pub fn label(self) -> &'static str {
        match self {
            Self::Shared => "Shared",
            Self::Exclusive => "Exclusive",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
    /// Stream formats reported by CPAL for this device and direction.
    pub config_ranges: Vec<AudioDeviceConfigRange>,
    /// Sample rates accepted by the Windows WASAPI exclusive format probe.
    pub exclusive_sample_rates: Vec<u32>,
    /// WASAPI default device period in 100-nanosecond units.
    pub exclusive_default_period_hns: Option<u64>,
    /// WASAPI minimum device period in 100-nanosecond units.
    pub exclusive_minimum_period_hns: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioDeviceConfigRange {
    pub minimum_sample_rate: u32,
    pub maximum_sample_rate: u32,
    pub channels: u16,
    /// `None` means the backend does not report a fixed-buffer range.
    pub minimum_buffer_frames: Option<u32>,
    pub maximum_buffer_frames: Option<u32>,
}

impl AudioDeviceConfigRange {
    fn supports_sample_rate(self, sample_rate: u32) -> bool {
        self.channels > 0
            && self.minimum_sample_rate <= sample_rate
            && sample_rate <= self.maximum_sample_rate
    }

    fn supports_buffer_frames(self, buffer_frames: u32) -> bool {
        match (self.minimum_buffer_frames, self.maximum_buffer_frames) {
            (Some(minimum), Some(maximum)) => minimum <= buffer_frames && buffer_frames <= maximum,
            (None, None) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioDeviceCatalog {
    pub inputs: Vec<AudioDeviceInfo>,
    pub outputs: Vec<AudioDeviceInfo>,
    pub default_input_id: Option<String>,
    pub default_output_id: Option<String>,
    pub default_exclusive_input_id: Option<String>,
    pub default_exclusive_output_id: Option<String>,
    pub default_sample_rate: Option<u32>,
    pub error: Option<String>,
}

const COMMON_AUDIO_SAMPLE_RATES: &[u32] = &[
    8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 64_000, 88_200, 96_000,
    176_400, 192_000, 352_800, 384_000,
];
const COMMON_AUDIO_BUFFER_FRAMES: &[u32] = &[32, 64, 128, 256, 512, 1_024, 2_048, 4_096, 8_192];

impl AudioDeviceCatalog {
    /// Number of stereo input pairs available at the selected sample rate.
    /// Mono devices expose one selectable pair and duplicate their input to stereo.
    pub fn input_channel_pair_count(&self, settings: &AudioSettings) -> u16 {
        let default_id = if settings.access == AudioAccess::Exclusive {
            self.default_exclusive_input_id.as_ref()
        } else {
            self.default_input_id.as_ref()
        };
        let device = settings
            .input_device_id
            .as_ref()
            .or(default_id)
            .and_then(|id| self.inputs.iter().find(|device| &device.id == id));
        let Some(device) = device else {
            return 1;
        };
        let supported_channels = device
            .config_ranges
            .iter()
            .filter(|range| {
                range.minimum_sample_rate <= settings.sample_rate
                    && settings.sample_rate <= range.maximum_sample_rate
            })
            .map(|range| range.channels)
            .max()
            .or_else(|| {
                device
                    .config_ranges
                    .iter()
                    .map(|range| range.channels)
                    .max()
            })
            .unwrap_or(1);
        (supported_channels / 2).max(1)
    }

    /// Returns common sample rates supported by every selected endpoint.
    /// Shared-mode ranges contribute their endpoints; exclusive mode uses probed rates.
    pub fn supported_sample_rates(&self, settings: &AudioSettings) -> Vec<u32> {
        let Ok(devices) = self.selected_device_infos(settings) else {
            return Vec::new();
        };
        let mut candidates = COMMON_AUDIO_SAMPLE_RATES.to_vec();
        candidates.push(settings.sample_rate);
        candidates.extend(self.default_sample_rate);
        for device in &devices {
            if settings.access == AudioAccess::Exclusive {
                candidates.extend(device.exclusive_sample_rates.iter().copied());
            } else {
                for config in &device.config_ranges {
                    candidates.push(config.minimum_sample_rate);
                    candidates.push(config.maximum_sample_rate);
                }
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        candidates.retain(|rate| (8_000..=384_000).contains(rate));
        candidates.retain(|rate| {
            devices.is_empty()
                || devices.iter().all(|device| {
                    if settings.access == AudioAccess::Exclusive {
                        device.exclusive_sample_rates.contains(rate)
                    } else {
                        device
                            .config_ranges
                            .iter()
                            .any(|config| config.supports_sample_rate(*rate))
                    }
                })
        });
        candidates
    }

    /// Returns buffer-size choices supported by every selected endpoint at `sample_rate`.
    /// WASAPI exclusive choices include endpoint periods and exclude sizes below minimum periods.
    pub fn supported_buffer_sizes(&self, settings: &AudioSettings, sample_rate: u32) -> Vec<u32> {
        let Ok(devices) = self.selected_device_infos(settings) else {
            return Vec::new();
        };
        let mut candidates = COMMON_AUDIO_BUFFER_FRAMES.to_vec();
        candidates.push(settings.buffer_frames);
        if settings.access == AudioAccess::Exclusive {
            for device in &devices {
                candidates.extend(
                    device
                        .exclusive_minimum_period_hns
                        .and_then(|period| minimum_period_frames(period, sample_rate)),
                );
                candidates.extend(
                    device
                        .exclusive_default_period_hns
                        .and_then(|period| nearest_period_frames(period, sample_rate)),
                );
            }
            candidates.sort_unstable();
            candidates.dedup();
            candidates.retain(|frames| (32..=8_192).contains(frames));
            candidates.retain(|frames| {
                devices.iter().all(|device| {
                    device
                        .exclusive_minimum_period_hns
                        .and_then(|period| minimum_period_frames(period, sample_rate))
                        .is_none_or(|minimum| *frames >= minimum)
                })
            });
            return candidates;
        }
        for device in &devices {
            for config in &device.config_ranges {
                if config.supports_sample_rate(sample_rate) {
                    candidates.extend(config.minimum_buffer_frames);
                    candidates.extend(config.maximum_buffer_frames);
                }
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        candidates.retain(|frames| (32..=8_192).contains(frames));
        candidates.retain(|frames| {
            devices.is_empty()
                || devices.iter().all(|device| {
                    let configs = device
                        .config_ranges
                        .iter()
                        .filter(|config| config.supports_sample_rate(sample_rate))
                        .collect::<Vec<_>>();
                    let preferred_channels = configs
                        .iter()
                        .map(|config| (config.channels == 2, config.channels))
                        .max();
                    preferred_channels.is_some_and(|preferred| {
                        configs.iter().any(|config| {
                            (config.channels == 2, config.channels) == preferred
                                && config.supports_buffer_frames(*frames)
                        })
                    })
                })
        });
        candidates
    }

    /// Labels device-period buffer choices so Audio Settings can identify them.
    pub fn buffer_size_option_label(
        &self,
        settings: &AudioSettings,
        sample_rate: u32,
        buffer_frames: u32,
    ) -> String {
        let mut labels = Vec::new();
        if settings.access == AudioAccess::Exclusive
            && let Ok(devices) = self.selected_device_infos(settings)
        {
            if devices.iter().any(|device| {
                device
                    .exclusive_minimum_period_hns
                    .and_then(|period| minimum_period_frames(period, sample_rate))
                    == Some(buffer_frames)
            }) {
                labels.push("WASAPI minimum period");
            }
            if devices.iter().any(|device| {
                device
                    .exclusive_default_period_hns
                    .and_then(|period| nearest_period_frames(period, sample_rate))
                    == Some(buffer_frames)
            }) {
                labels.push("WASAPI default period");
            }
        }
        if labels.is_empty() {
            format!("{buffer_frames} frames")
        } else {
            format!("{buffer_frames} frames · {}", labels.join(" · "))
        }
    }

    fn selected_device_infos(&self, settings: &AudioSettings) -> Result<Vec<&AudioDeviceInfo>, ()> {
        let (default_input_id, default_output_id) = match settings.access {
            AudioAccess::Shared => (
                self.default_input_id.as_deref(),
                self.default_output_id.as_deref(),
            ),
            AudioAccess::Exclusive => (
                self.default_exclusive_input_id.as_deref(),
                self.default_exclusive_output_id.as_deref(),
            ),
        };
        let mut devices = Vec::new();
        for (enabled, selected_id, default_id, candidates) in [
            (
                settings.enable_input,
                settings.input_device_id.as_deref(),
                default_input_id,
                self.inputs.as_slice(),
            ),
            (
                settings.enable_output,
                settings.output_device_id.as_deref(),
                default_output_id,
                self.outputs.as_slice(),
            ),
        ] {
            if !enabled {
                continue;
            }
            let Some(id) = selected_id.or(default_id) else {
                continue;
            };
            let Some(device) = candidates.iter().find(|device| device.id == id) else {
                return Err(());
            };
            devices.push(device);
        }
        Ok(devices)
    }
}

fn nearest_period_frames(period_hns: u64, sample_rate: u32) -> Option<u32> {
    period_frames(period_hns, sample_rate, false)
}

fn minimum_period_frames(period_hns: u64, sample_rate: u32) -> Option<u32> {
    period_frames(period_hns, sample_rate, true)
}

fn period_frames(period_hns: u64, sample_rate: u32, round_up: bool) -> Option<u32> {
    if sample_rate == 0 {
        return None;
    }
    let numerator = period_hns.checked_mul(u64::from(sample_rate))?;
    const HUNDRED_NS_PER_SECOND: u64 = 10_000_000;
    let frames = if round_up {
        numerator / HUNDRED_NS_PER_SECOND
            + u64::from(!numerator.is_multiple_of(HUNDRED_NS_PER_SECOND))
    } else {
        numerator.checked_add(HUNDRED_NS_PER_SECOND / 2)? / HUNDRED_NS_PER_SECOND
    };
    u32::try_from(frames.max(1)).ok()
}

fn nonzero_callback_frames(frames: u32) -> Option<u32> {
    (frames > 0).then_some(frames)
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioSettings {
    pub enable_input: bool,
    pub enable_output: bool,
    pub input_device_id: Option<String>,
    /// Zero-based stereo pair index. Mono inputs duplicate channel zero.
    pub input_channel_pair: u16,
    pub output_device_id: Option<String>,
    pub output_gain: f32,
    pub sample_rate: u32,
    pub buffer_frames: u32,
    pub access: AudioAccess,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            enable_input: true,
            enable_output: true,
            input_device_id: None,
            input_channel_pair: 0,
            output_device_id: None,
            output_gain: 1.0,
            sample_rate: 48_000,
            buffer_frames: 256,
            access: AudioAccess::Shared,
        }
    }
}

struct PlaybackState {
    commands: AudioCommandQueue,
    samples: ArcSwapOption<Vec<f32>>,
    streaming: ArcSwapOption<StreamingPlayback>,
    pending_streaming: ArcSwapOption<StreamingPlayback>,
    retired_streaming: ArrayQueue<Arc<StreamingPlayback>>,
    stream_handoff_gate: AtomicU8,
    cursor_frames: AtomicU64,
    playback_speed: AtomicU32,
    playback_speed_phase: AtomicU32,
    active: AtomicBool,
    browser_preview_samples: ArcSwapOption<Vec<f32>>,
    browser_preview_state: AtomicU64,
    browser_preview_looping: AtomicBool,
    browser_preview_gain: AtomicU32,
    output_gain: AtomicU32,
    output_peak: AtomicU32,
    output_peak_left: AtomicU32,
    output_peak_right: AtomicU32,
    output_callback_frames: AtomicU32,
    output_latency_estimate: AudioLatencyEstimate,
    input_monitor_ring_flush_requested: AtomicBool,
    input_monitor_active: Arc<AtomicBool>,
}

struct AudioCommandQueue {
    transport: ArrayQueue<TransportCommand>,
    parameters: ArrayQueue<ParameterCommand>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum TransportCommand {
    SetOutputSource(u8),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ParameterCommand {
    SetBrowserPreviewGain(u32),
}

struct StreamingProducerState {
    active_count: usize,
}

struct StreamingPlayback {
    rings: Vec<AudioRingBuffer>,
    producer_finished: Vec<AtomicBool>,
    producer_underrun_frames: Vec<AtomicU64>,
    producer_state: Mutex<StreamingProducerState>,
    project_source_index: Option<usize>,
    project_source_finished: AtomicBool,
    active: AtomicBool,
    paused: AtomicBool,
    finished: AtomicBool,
    cancelled: AtomicBool,
    handoff_target_frame: AtomicU64,
    handoff_committed: AtomicBool,
    underrun_frames: AtomicU64,
    consumed_frames: AtomicU64,
}

impl StreamingPlayback {
    fn available_stereo_frames(&self) -> usize {
        self.rings
            .iter()
            .map(AudioRingBuffer::available_stereo_frames)
            .sum()
    }

    fn is_ready(&self) -> bool {
        self.rings
            .iter()
            .zip(&self.producer_finished)
            .all(|(ring, finished)| {
                finished.load(Ordering::Acquire)
                    || ring.available_stereo_frames() >= STREAM_PREFILL_FRAMES
            })
    }

    fn project_source_active(&self) -> bool {
        if self.cancelled.load(Ordering::Acquire) || self.paused.load(Ordering::Acquire) {
            return false;
        }
        self.project_source_index.is_some_and(|index| {
            !self.project_source_finished.load(Ordering::Acquire)
                || self.rings[index].available_stereo_frames() > 0
        })
    }

    fn is_ready_at_frame(&self, frame: u64) -> bool {
        let Some(index) = self.project_source_index else {
            return false;
        };
        if self.cancelled.load(Ordering::Acquire) {
            return false;
        }
        let start_frame = self.consumed_frames.load(Ordering::Acquire);
        let frames_to_skip = frame.saturating_sub(start_frame);
        let Ok(frames_to_skip) = usize::try_from(frames_to_skip) else {
            return false;
        };
        if self.project_source_finished.load(Ordering::Acquire) {
            return true;
        }
        let available_frames = self.rings[index].available_stereo_frames();
        frames_to_skip
            .checked_add(STREAM_PREFILL_FRAMES)
            .is_some_and(|required| available_frames >= required)
    }
}

fn create_streaming_playback(
    initial_position_frames: u64,
    ring_capacity_samples: usize,
    source_count: usize,
    project_source: bool,
) -> (Arc<StreamingPlayback>, Vec<StreamingAudioWriter>) {
    assert!((1..=MAX_LIVE_STREAM_SOURCES).contains(&source_count));
    let playback = Arc::new(StreamingPlayback {
        rings: (0..MAX_LIVE_STREAM_SOURCES)
            .map(|index| {
                let capacity = if index < source_count {
                    ring_capacity_samples
                } else {
                    ring_capacity_samples.min(LIVE_STREAM_RING_CAPACITY)
                };
                AudioRingBuffer::new(capacity)
            })
            .collect(),
        producer_finished: (0..MAX_LIVE_STREAM_SOURCES)
            .map(|index| AtomicBool::new(index >= source_count))
            .collect(),
        producer_underrun_frames: (0..MAX_LIVE_STREAM_SOURCES)
            .map(|_| AtomicU64::new(0))
            .collect(),
        producer_state: Mutex::new(StreamingProducerState {
            active_count: source_count,
        }),
        project_source_index: project_source.then_some(0),
        project_source_finished: AtomicBool::new(!project_source),
        active: AtomicBool::new(false),
        paused: AtomicBool::new(false),
        finished: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        handoff_target_frame: AtomicU64::new(u64::MAX),
        handoff_committed: AtomicBool::new(false),
        underrun_frames: AtomicU64::new(0),
        consumed_frames: AtomicU64::new(initial_position_frames),
    });
    let writers = (0..source_count)
        .map(|ring_index| StreamingAudioWriter {
            playback: Arc::clone(&playback),
            ring_index,
            is_project_source: project_source && ring_index == 0,
        })
        .collect();
    (playback, writers)
}

#[derive(Default)]
struct AudioErrorState {
    latest: Mutex<Option<String>>,
    recent: Mutex<VecDeque<String>>,
    count: AtomicU64,
}

impl AudioErrorState {
    fn report(&self, message: String) {
        self.count.fetch_add(1, Ordering::Relaxed);
        *self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message.clone());

        let mut recent = self
            .recent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if recent.len() == AUDIO_ERROR_HISTORY_LIMIT {
            recent.pop_front();
        }
        recent.push_back(message);
    }

    fn take_latest(&self) -> Option<String> {
        self.latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn recent(&self) -> Vec<String> {
        self.recent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }
}

/// The producer side of audio streamed into an active device callback.
///
/// Writes may wait for the callback to consume ring-buffer space. This handle is intended for
/// worker threads; the device callback never waits for the producer.
pub struct StreamingAudioWriter {
    playback: Arc<StreamingPlayback>,
    ring_index: usize,
    is_project_source: bool,
}

/// A newly rendered project stream waiting to replace the current stream at an audio callback
/// boundary. The host may keep the current stream audible while this stream prepares its buffer.
#[derive(Clone)]
pub struct StagedStreamingPlayback {
    playback: Arc<PlaybackState>,
    stream: Arc<StreamingPlayback>,
}

impl StagedStreamingPlayback {
    /// Whether the staged producer has enough audio to begin a handoff at `frame`.
    pub fn is_ready_at_frame(&self, frame: u64) -> bool {
        self.stream.is_ready_at_frame(frame)
    }

    /// Whether the audio callback has switched to this stream.
    pub fn is_committed(&self) -> bool {
        self.stream.handoff_committed.load(Ordering::Acquire)
    }
}

impl StreamingAudioWriter {
    /// Write interleaved stereo samples, waiting outside the audio callback when the ring fills.
    pub fn write_stereo_samples(&self, samples: &[f32]) -> Result<(), String> {
        if !samples.len().is_multiple_of(2) {
            return Err("streamed audio must contain stereo sample frames".to_owned());
        }
        let (frames, _) = samples.as_chunks::<2>();
        for &[left, right] in frames {
            while !self.playback.rings[self.ring_index].push_stereo_frame(left, right) {
                if self.playback.cancelled.load(Ordering::Acquire) {
                    return Err("streamed audio playback was stopped".to_owned());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            if self.playback.cancelled.load(Ordering::Acquire) {
                return Err("streamed audio playback was stopped".to_owned());
            }
        }
        Ok(())
    }

    /// Mark the stream complete. Buffered frames continue playing before the device goes idle.
    pub fn finish(&self) {
        let producer_finished = &self.playback.producer_finished[self.ring_index];
        let mut state = self
            .playback
            .producer_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !producer_finished.swap(true, Ordering::AcqRel) {
            if self.is_project_source {
                self.playback
                    .project_source_finished
                    .store(true, Ordering::Release);
            }
            state.active_count = state.active_count.saturating_sub(1);
            if state.active_count == 0 {
                self.playback.finished.store(true, Ordering::Release);
            }
        }
    }

    /// Whether Stop or a replacement playback source cancelled this stream.
    pub fn is_cancelled(&self) -> bool {
        self.playback.cancelled.load(Ordering::Acquire)
    }

    /// Number of stereo frames the device requested after the stream began but before data was
    /// available. This is useful for diagnosing producer underruns.
    pub fn underrun_frames(&self) -> u64 {
        self.playback.producer_underrun_frames[self.ring_index].load(Ordering::Acquire)
    }
}

impl Drop for StreamingAudioWriter {
    fn drop(&mut self) {
        self.finish();
    }
}

struct InputRecordingState {
    ring: AudioRingBuffer,
    active: AtomicBool,
    callbacks_in_flight: AtomicU64,
    sample_rate: u32,
    overflow_frames: AtomicU64,
}

struct AudioLatencyEstimate {
    smoothed_microseconds: AtomicU32,
    sample_count: AtomicU32,
}

impl Default for AudioLatencyEstimate {
    fn default() -> Self {
        Self {
            smoothed_microseconds: AtomicU32::new(0),
            sample_count: AtomicU32::new(0),
        }
    }
}

impl AudioLatencyEstimate {
    fn observe(&self, latency: Option<Duration>) {
        let Some(latency) =
            latency.filter(|latency| latency.as_micros() <= LATENCY_ESTIMATE_MAX_MICROSECONDS)
        else {
            return;
        };
        let observed = latency.as_micros() as u32;
        let count = self.sample_count.load(Ordering::Relaxed);
        let filtered = if count == 0 {
            observed
        } else {
            self.smoothed_microseconds
                .load(Ordering::Relaxed)
                .saturating_mul(7)
                .saturating_add(observed)
                / 8
        };
        self.smoothed_microseconds
            .store(filtered, Ordering::Relaxed);
        let _ = self
            .sample_count
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                Some(count.saturating_add(1))
            });
    }

    fn observe_cpal_input_callback(&self, info: &cpal::InputCallbackInfo) {
        let timestamp = info.timestamp();
        if timestamp.callback.as_nanos() == 0 || timestamp.capture.as_nanos() == 0 {
            return;
        }
        self.observe(timestamp.callback.checked_duration_since(timestamp.capture));
    }

    fn observe_cpal_output_callback(&self, info: &cpal::OutputCallbackInfo) {
        let timestamp = info.timestamp();
        if timestamp.callback.as_nanos() == 0 || timestamp.playback.as_nanos() == 0 {
            return;
        }
        self.observe(
            timestamp
                .playback
                .checked_duration_since(timestamp.callback),
        );
    }

    fn estimated_milliseconds(&self) -> Option<f32> {
        (self.sample_count.load(Ordering::Acquire) >= LATENCY_ESTIMATE_MIN_SAMPLES)
            .then(|| self.smoothed_microseconds.load(Ordering::Relaxed) as f32 / 1_000.0)
    }

    fn wait_for_estimate(&self, timeout: Duration) -> Option<f32> {
        let started = std::time::Instant::now();
        loop {
            if let Some(latency_ms) = self.estimated_milliseconds() {
                return Some(latency_ms);
            }
            if started.elapsed() >= timeout {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl InputRecordingState {
    fn new(sample_rate: u32, capacity_frames: usize) -> Self {
        Self {
            ring: AudioRingBuffer::new(capacity_frames.saturating_mul(2)),
            active: AtomicBool::new(true),
            callbacks_in_flight: AtomicU64::new(0),
            sample_rate,
            overflow_frames: AtomicU64::new(0),
        }
    }

    fn begin_callback(&self) -> Option<InputRecordingCallback<'_>> {
        self.callbacks_in_flight.fetch_add(1, Ordering::SeqCst);
        if !self.active.load(Ordering::SeqCst) {
            self.callbacks_in_flight.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(InputRecordingCallback { recording: self })
    }

    fn push_frame(&self, left: f32, right: f32) {
        if !self.ring.push_stereo_frame(left, right) {
            self.overflow_frames.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn stop(&self) {
        self.active.store(false, Ordering::SeqCst);
    }
}

struct InputRecordingCallback<'a> {
    recording: &'a InputRecordingState,
}

impl InputRecordingCallback<'_> {
    fn push_frame(&self, left: f32, right: f32) {
        self.recording.push_frame(left, right);
    }
}

impl Drop for InputRecordingCallback<'_> {
    fn drop(&mut self) {
        self.recording
            .callbacks_in_flight
            .fetch_sub(1, Ordering::SeqCst);
    }
}

/// Reader for nonblocking stereo frames captured from the active input device.
///
/// Audio callbacks write to a bounded lock-free queue. A worker thread should drain
/// frames with [`Self::read_frames`] and monitor [`Self::overflow_frames`] while
/// recording.
#[derive(Clone)]
pub struct AudioInputRecording {
    recording: Arc<InputRecordingState>,
}

/// Worker-side sink for Mixer-processed input monitor frames.
pub struct AudioInputMonitorOutput {
    ring: Arc<AudioRingBuffer>,
    recording: Arc<InputRecordingState>,
    processed_recording: AudioInputMonitorRecorder,
}

/// Recording tap for the signal after the selected Mixer route and effects.
#[derive(Clone)]
pub struct AudioInputMonitorRecorder {
    sample_rate: u32,
    recording: Arc<ArcSwapOption<InputRecordingState>>,
}

impl AudioInputMonitorRecorder {
    /// Start a bounded recording tap for the Mixer-processed monitor signal.
    pub fn start_recording(&self) -> Result<AudioInputRecording, String> {
        if self
            .recording
            .load()
            .as_deref()
            .is_some_and(|recording| recording.active.load(Ordering::Acquire))
        {
            return Err("Mixer output recording is already active".to_owned());
        }
        let recording = Arc::new(InputRecordingState::new(
            self.sample_rate,
            INPUT_RECORDING_BUFFER_FRAMES,
        ));
        self.recording.store(Some(Arc::clone(&recording)));
        Ok(AudioInputRecording { recording })
    }

    pub fn stop_recording(&self) {
        if let Some(recording) = self.recording.load().as_deref() {
            recording.stop();
        }
        self.recording.store(None);
    }
}

impl AudioInputMonitorOutput {
    pub fn processed_recording(&self) -> AudioInputMonitorRecorder {
        self.processed_recording.clone()
    }

    pub fn is_stopped(&self) -> bool {
        !self.recording.active.load(Ordering::Acquire)
    }

    /// Queue interleaved stereo frames for the device output callback.
    pub fn write_stereo_samples(&self, samples: &[f32]) -> Result<(), String> {
        if !samples.len().is_multiple_of(2) {
            return Err("input monitor audio must contain stereo sample frames".to_owned());
        }
        let recording = self.processed_recording.recording.load();
        let recording_callback = recording
            .as_deref()
            .and_then(InputRecordingState::begin_callback);
        for &[left, right] in samples.as_chunks::<2>().0 {
            loop {
                if !self.recording.active.load(Ordering::Acquire) {
                    return Err("input monitoring was stopped".to_owned());
                }
                if self.ring.push_stereo_frame(left, right) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            if let Some(recording) = recording_callback.as_ref() {
                recording.push_frame(left, right);
            }
        }
        Ok(())
    }
}

impl Drop for AudioInputMonitorOutput {
    fn drop(&mut self) {
        self.processed_recording.stop_recording();
    }
}

impl AudioInputRecording {
    #[cfg(test)]
    pub(crate) fn from_test_frames(sample_rate: u32, frames: &[[f32; 2]]) -> Self {
        let recording = Arc::new(InputRecordingState::new(sample_rate, frames.len().max(1)));
        let callback = recording
            .begin_callback()
            .expect("a test recording starts active");
        for &[left, right] in frames {
            callback.push_frame(left, right);
        }
        drop(callback);
        recording.stop();
        Self { recording }
    }

    pub fn sample_rate(&self) -> u32 {
        self.recording.sample_rate
    }

    pub fn stop(&self) {
        self.recording.stop();
    }

    /// Copy as many queued stereo frames as fit in `frames`, returning the number read.
    pub fn read_frames(&self, frames: &mut [[f32; 2]]) -> usize {
        let mut count = 0;
        for frame in frames {
            let Some(captured) = self.recording.ring.pop_stereo_frame() else {
                break;
            };
            *frame = captured;
            count += 1;
        }
        count
    }

    /// Whether capture has stopped and the queue has been drained.
    pub fn is_finished(&self) -> bool {
        !self.recording.active.load(Ordering::SeqCst)
            && self.recording.callbacks_in_flight.load(Ordering::SeqCst) == 0
            && self.recording.ring.available_stereo_frames() == 0
    }

    /// Number of input frames that could not be queued because the consumer fell behind.
    pub fn overflow_frames(&self) -> u64 {
        self.recording.overflow_frames.load(Ordering::Acquire)
    }
}

impl PlaybackState {
    fn new() -> Self {
        Self {
            commands: AudioCommandQueue {
                transport: ArrayQueue::new(AUDIO_COMMAND_QUEUE_CAPACITY),
                parameters: ArrayQueue::new(AUDIO_COMMAND_QUEUE_CAPACITY),
            },
            samples: ArcSwapOption::empty(),
            streaming: ArcSwapOption::empty(),
            pending_streaming: ArcSwapOption::empty(),
            retired_streaming: ArrayQueue::new(RETIRED_STREAM_QUEUE_CAPACITY),
            stream_handoff_gate: AtomicU8::new(STREAM_HANDOFF_IDLE),
            cursor_frames: AtomicU64::new(0),
            playback_speed: AtomicU32::new(1.0_f32.to_bits()),
            playback_speed_phase: AtomicU32::new(0.0_f32.to_bits()),
            active: AtomicBool::new(false),
            browser_preview_samples: ArcSwapOption::empty(),
            browser_preview_state: AtomicU64::new(0),
            browser_preview_looping: AtomicBool::new(false),
            browser_preview_gain: AtomicU32::new(1.0_f32.to_bits()),
            output_gain: AtomicU32::new(1.0_f32.to_bits()),
            output_peak: AtomicU32::new(0.0_f32.to_bits()),
            output_peak_left: AtomicU32::new(0.0_f32.to_bits()),
            output_peak_right: AtomicU32::new(0.0_f32.to_bits()),
            output_callback_frames: AtomicU32::new(0),
            output_latency_estimate: AudioLatencyEstimate::default(),
            input_monitor_ring_flush_requested: AtomicBool::new(false),
            input_monitor_active: Arc::new(AtomicBool::new(false)),
        }
    }

    fn enqueue_transport_command(&self, command: TransportCommand) {
        // This lane carries only complete transport state, so replacing old
        // commands preserves the newest requested source.
        let _ = self.commands.transport.force_push(command);
    }

    fn enqueue_parameter_command(&self, command: ParameterCommand) {
        // Parameter updates use their own lane so a burst of one kind cannot
        // discard the latest value from the other kind.
        let _ = self.commands.parameters.force_push(command);
    }

    fn apply_pending_commands(&self, source: &AtomicU8) {
        for _ in 0..AUDIO_COMMAND_QUEUE_CAPACITY {
            let Some(TransportCommand::SetOutputSource(next_source)) =
                self.commands.transport.pop()
            else {
                break;
            };
            source.store(next_source, Ordering::Release);
        }
        for _ in 0..AUDIO_COMMAND_QUEUE_CAPACITY {
            let Some(ParameterCommand::SetBrowserPreviewGain(gain_bits)) =
                self.commands.parameters.pop()
            else {
                break;
            };
            self.browser_preview_gain
                .store(gain_bits, Ordering::Release);
        }
    }

    fn apply_pending_streaming_handoff(&self) {
        if self
            .stream_handoff_gate
            .compare_exchange(
                STREAM_HANDOFF_IDLE,
                STREAM_HANDOFF_COMMITTING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return;
        }
        self.try_apply_pending_streaming_handoff();
        self.stream_handoff_gate
            .store(STREAM_HANDOFF_IDLE, Ordering::Release);
    }

    fn try_apply_pending_streaming_handoff(&self) {
        // The UI thread reaps retired streams. Keeping this queue empty before a handoff means the
        // callback can transfer ownership of the old Arc without dropping its ring buffers.
        if !self.retired_streaming.is_empty() {
            return;
        }
        let current_guard = self.streaming.load();
        let Some(current) = current_guard.as_deref() else {
            return;
        };
        let next_guard = self.pending_streaming.load();
        let Some(next) = next_guard.as_deref() else {
            return;
        };
        let target_frame = next.handoff_target_frame.load(Ordering::Acquire);
        if target_frame == u64::MAX {
            return;
        }
        let current_frame = current.consumed_frames.load(Ordering::Acquire);
        if current_frame < target_frame || !next.is_ready_at_frame(current_frame) {
            return;
        }
        let Some(source_index) = next.project_source_index else {
            return;
        };
        let next_start_frame = next.consumed_frames.load(Ordering::Acquire);
        let frames_to_skip = current_frame.saturating_sub(next_start_frame);
        let Ok(frames_to_skip) = usize::try_from(frames_to_skip) else {
            return;
        };
        let frames_to_discard = if next.project_source_finished.load(Ordering::Acquire) {
            frames_to_skip.min(next.rings[source_index].available_stereo_frames())
        } else {
            frames_to_skip
        };
        let skipped = next.rings[source_index].advance_stereo_frames(frames_to_discard);
        if skipped != frames_to_discard {
            return;
        }
        next.consumed_frames.store(current_frame, Ordering::Release);
        let paused = current.paused.load(Ordering::Acquire);
        next.paused.store(paused, Ordering::Release);
        next.active.store(!paused, Ordering::Release);

        let Some(next_owned) = self.pending_streaming.swap(None) else {
            return;
        };
        let previous = self.streaming.swap(Some(next_owned));
        if let Some(previous) = previous {
            // Only the audio callback publishes retired streams; UI calls drain the queue.
            if let Err(previous) = self.retired_streaming.push(previous) {
                // This is unreachable while the queue-empty guard above holds, but retaining the
                // stream in the pending slot is safer than dropping it on the realtime thread.
                if let Some(current) = self.streaming.swap(None) {
                    self.pending_streaming.store(Some(current));
                }
                self.streaming.store(Some(previous));
                return;
            }
        }
        next.handoff_committed.store(true, Ordering::Release);
    }

    fn start(&self, samples: Vec<f32>) {
        self.cancel_stream();
        self.samples.store(Some(Arc::new(samples)));
        self.cursor_frames.store(0, Ordering::Release);
        self.playback_speed_phase
            .store(0.0_f32.to_bits(), Ordering::Release);
        self.active.store(true, Ordering::Release);
    }

    fn start_browser_preview(&self, samples: Vec<f32>) {
        self.stop_browser_preview();
        self.browser_preview_samples.store(Some(Arc::new(samples)));
        self.browser_preview_state
            .store(BROWSER_PREVIEW_ACTIVE_BIT, Ordering::Release);
    }

    fn stop_browser_preview(&self) {
        self.browser_preview_state.store(0, Ordering::Release);
        self.browser_preview_samples.store(None);
    }

    fn browser_preview_active(&self) -> bool {
        self.browser_preview_state.load(Ordering::Acquire) & BROWSER_PREVIEW_ACTIVE_BIT != 0
    }

    fn browser_preview_frame_count(&self) -> u64 {
        self.browser_preview_samples
            .load()
            .as_ref()
            .map_or(0, |samples| (samples.len() / 2) as u64)
    }

    fn browser_preview_position_frames(&self) -> u64 {
        (self.browser_preview_state.load(Ordering::Acquire) & BROWSER_PREVIEW_FRAME_MASK)
            .min(self.browser_preview_frame_count())
    }

    fn seek_browser_preview(&self, frame: u64) {
        let frame_count = self.browser_preview_frame_count();
        if frame_count == 0 {
            return;
        }
        self.browser_preview_state.store(
            BROWSER_PREVIEW_ACTIVE_BIT | frame.min(frame_count - 1),
            Ordering::Release,
        );
    }

    fn set_browser_preview_looping(&self, looping: bool) {
        self.browser_preview_looping
            .store(looping, Ordering::Release);
    }

    fn next_browser_preview_frame(&self, samples: Option<&[f32]>) -> [f32; 2] {
        if !self.browser_preview_active() {
            return [0.0, 0.0];
        }
        let Some(samples) = samples else {
            return [0.0, 0.0];
        };
        let frame_count = (samples.len() / 2) as u64;
        if frame_count == 0 {
            return [0.0, 0.0];
        }
        let looping = self.browser_preview_looping.load(Ordering::Acquire);
        let mut state = self.browser_preview_state.load(Ordering::Relaxed);
        let frame = loop {
            if state & BROWSER_PREVIEW_ACTIVE_BIT == 0 {
                return [0.0, 0.0];
            }
            let cursor = state & BROWSER_PREVIEW_FRAME_MASK;
            let (frame, next_state) = if cursor < frame_count {
                (cursor, BROWSER_PREVIEW_ACTIVE_BIT | (cursor + 1))
            } else if looping {
                let frame = cursor % frame_count;
                (frame, BROWSER_PREVIEW_ACTIVE_BIT | (frame + 1))
            } else {
                (0, cursor)
            };
            match self.browser_preview_state.compare_exchange_weak(
                state,
                next_state,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) if cursor >= frame_count && !looping => return [0.0, 0.0],
                Ok(_) => break frame,
                Err(observed) => state = observed,
            }
        };
        let Some(offset) = usize::try_from(frame)
            .ok()
            .and_then(|frame| frame.checked_mul(2))
        else {
            return [0.0, 0.0];
        };
        let Some(frame_samples) = samples.get(offset..offset.saturating_add(2)) else {
            return [0.0, 0.0];
        };
        let gain = f32::from_bits(self.browser_preview_gain.load(Ordering::Relaxed));
        [frame_samples[0] * gain, frame_samples[1] * gain]
    }

    fn start_streaming(&self, initial_position_frames: u64) -> StreamingAudioWriter {
        self.start_streaming_sources_with_capacity_and_project_source(
            initial_position_frames,
            AUDIO_RING_CAPACITY,
            1,
            true,
        )
        .pop()
        .expect("one streaming writer was requested")
    }

    fn start_streaming_with_capacity(
        &self,
        initial_position_frames: u64,
        ring_capacity_samples: usize,
    ) -> StreamingAudioWriter {
        self.start_streaming_sources_with_capacity(
            initial_position_frames,
            ring_capacity_samples,
            1,
        )
        .pop()
        .expect("one streaming writer was requested")
    }

    fn start_streaming_sources_with_capacity(
        &self,
        initial_position_frames: u64,
        ring_capacity_samples: usize,
        source_count: usize,
    ) -> Vec<StreamingAudioWriter> {
        self.start_streaming_sources_with_capacity_and_project_source(
            initial_position_frames,
            ring_capacity_samples,
            source_count,
            false,
        )
    }

    fn start_streaming_sources_with_capacity_and_project_source(
        &self,
        initial_position_frames: u64,
        ring_capacity_samples: usize,
        source_count: usize,
        project_source: bool,
    ) -> Vec<StreamingAudioWriter> {
        assert!((1..=MAX_LIVE_STREAM_SOURCES).contains(&source_count));
        self.stop();
        let (playback, writers) = create_streaming_playback(
            initial_position_frames,
            ring_capacity_samples,
            source_count,
            project_source,
        );
        self.streaming.store(Some(Arc::clone(&playback)));
        writers
    }

    fn add_streaming_sources(
        &self,
        source_count: usize,
    ) -> Result<Vec<StreamingAudioWriter>, String> {
        if !(1..=MAX_LIVE_STREAM_SOURCES).contains(&source_count) {
            return Err(format!(
                "live audio mixing supports between 1 and {MAX_LIVE_STREAM_SOURCES} sources"
            ));
        }
        let playback = self
            .streaming
            .load_full()
            .ok_or_else(|| "Start project audio playback before live audition".to_owned())?;
        let mut state = playback
            .producer_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let project_audio_queued = playback
            .project_source_index
            .is_some_and(|index| playback.rings[index].available_stereo_frames() > 0);
        if playback.cancelled.load(Ordering::Acquire)
            || playback.paused.load(Ordering::Acquire)
            || (playback.finished.load(Ordering::Acquire) && !project_audio_queued)
        {
            return Err("Project audio playback is not available for live audition".to_owned());
        }
        let available_rings = playback
            .rings
            .iter()
            .enumerate()
            .filter(|(index, ring)| {
                playback.producer_finished[*index].load(Ordering::Acquire)
                    && ring.available_stereo_frames() == 0
            })
            .map(|(index, _)| index)
            .take(source_count)
            .collect::<Vec<_>>();
        if available_rings.len() != source_count {
            return Err("The live audio source limit was reached".to_owned());
        }
        state.active_count += source_count;
        playback.finished.store(false, Ordering::Release);
        for &index in &available_rings {
            playback.producer_underrun_frames[index].store(0, Ordering::Release);
            playback.producer_finished[index].store(false, Ordering::Release);
        }
        drop(state);
        Ok(available_rings
            .into_iter()
            .map(|ring_index| StreamingAudioWriter {
                playback: Arc::clone(&playback),
                ring_index,
                is_project_source: false,
            })
            .collect())
    }

    fn pause(&self) {
        self.active.store(false, Ordering::Release);
        if let Some(streaming) = self.streaming.load().as_deref() {
            streaming.active.store(false, Ordering::Release);
            streaming.paused.store(true, Ordering::Release);
        }
    }

    fn resume(&self) -> Result<(), String> {
        if let Some(streaming) = self.streaming.load().as_deref() {
            let stream_exhausted = streaming.project_source_index.map_or_else(
                || {
                    streaming.finished.load(Ordering::Acquire)
                        && streaming.available_stereo_frames() == 0
                },
                |index| {
                    streaming.project_source_finished.load(Ordering::Acquire)
                        && streaming.rings[index].available_stereo_frames() == 0
                },
            );
            if streaming.cancelled.load(Ordering::Acquire) || stream_exhausted {
                return Err("No streamed project audio remains to resume".to_owned());
            }
            streaming.paused.store(false, Ordering::Release);
            streaming.active.store(true, Ordering::Release);
            return Ok(());
        }
        let frame_count = self
            .samples
            .load()
            .as_deref()
            .map(|samples| samples.len() / 2)
            .unwrap_or(0);
        if frame_count == 0 {
            return Err("No rendered project audio is loaded".into());
        }
        if self.cursor_frames.load(Ordering::Acquire) >= frame_count as u64 {
            self.cursor_frames.store(0, Ordering::Release);
        }
        self.active.store(true, Ordering::Release);
        Ok(())
    }

    fn stop(&self) {
        self.active.store(false, Ordering::Release);
        self.cursor_frames.store(0, Ordering::Release);
        self.playback_speed_phase
            .store(0.0_f32.to_bits(), Ordering::Release);
        self.samples.store(None);
        self.cancel_stream();
    }

    fn cancel_stream(&self) {
        while self
            .stream_handoff_gate
            .compare_exchange(
                STREAM_HANDOFF_IDLE,
                STREAM_HANDOFF_STOPPING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            std::thread::yield_now();
        }
        if let Some(streaming) = self.streaming.swap(None) {
            streaming.cancelled.store(true, Ordering::Release);
            streaming.active.store(false, Ordering::Release);
        }
        if let Some(streaming) = self.pending_streaming.swap(None) {
            streaming.cancelled.store(true, Ordering::Release);
            streaming.active.store(false, Ordering::Release);
        }
        self.stream_handoff_gate
            .store(STREAM_HANDOFF_IDLE, Ordering::Release);
    }

    #[cfg(test)]
    fn is_active(&self) -> bool {
        if self.active.load(Ordering::Acquire) {
            return true;
        }
        self.streaming.load().as_deref().is_some_and(|streaming| {
            !streaming.cancelled.load(Ordering::Acquire)
                && !streaming.paused.load(Ordering::Acquire)
                && (streaming.active.load(Ordering::Acquire)
                    || !streaming.finished.load(Ordering::Acquire)
                    || streaming.available_stereo_frames() > 0)
        })
    }

    fn is_project_active(&self) -> bool {
        if self.active.load(Ordering::Acquire) {
            return true;
        }
        let current = self.streaming.load();
        if let Some(current) = current.as_deref() {
            if current.project_source_active() {
                return true;
            }
            if current.paused.load(Ordering::Acquire) || current.cancelled.load(Ordering::Acquire) {
                return false;
            }
        }
        self.pending_streaming
            .load()
            .as_deref()
            .is_some_and(|pending| {
                !pending.cancelled.load(Ordering::Acquire)
                    && pending.handoff_target_frame.load(Ordering::Acquire) != u64::MAX
            })
    }

    fn is_streaming(&self) -> bool {
        self.streaming.load().is_some()
    }

    fn position_frames(&self) -> u64 {
        self.streaming.load().as_deref().map_or_else(
            || self.cursor_frames.load(Ordering::Acquire),
            |streaming| streaming.consumed_frames.load(Ordering::Acquire),
        )
    }
}

/// Enumerate the default system host's active input and output endpoints.
pub fn enumerate_devices() -> AudioDeviceCatalog {
    let host = cpal::default_host();
    let default_input_id = host
        .default_input_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let default_output = host.default_output_device();
    let default_sample_rate = default_output
        .as_ref()
        .and_then(|device| device.default_output_config().ok())
        .map(|config| config.sample_rate());
    let default_output_id = default_output
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let mut catalog = AudioDeviceCatalog {
        default_input_id,
        default_output_id,
        default_sample_rate,
        ..AudioDeviceCatalog::default()
    };

    let mut errors = Vec::new();
    for host_id in cpal::available_hosts() {
        let host = match cpal::host_from_id(host_id) {
            Ok(host) => host,
            Err(error) => {
                errors.push(format!("{host_id}: {error}"));
                continue;
            }
        };
        match host.devices() {
            Ok(devices) => {
                for device in devices {
                    let Ok(id) = device.id() else {
                        continue;
                    };
                    let name = device
                        .description()
                        .map(|description| description.to_string())
                        .unwrap_or_else(|_| device.to_string());
                    let info = AudioDeviceInfo {
                        id: id.to_string(),
                        name: format!("{} · {name}", host_id.to_string().to_ascii_uppercase()),
                        config_ranges: Vec::new(),
                        exclusive_sample_rates: Vec::new(),
                        exclusive_default_period_hns: None,
                        exclusive_minimum_period_hns: None,
                    };
                    if device.supports_input() {
                        let config_ranges = match device.supported_input_configs() {
                            Ok(configs) => audio_device_config_ranges(configs),
                            Err(error) => {
                                errors.push(format!("{} input formats: {error}", info.name));
                                Vec::new()
                            }
                        };
                        catalog.inputs.push(AudioDeviceInfo {
                            config_ranges,
                            ..info.clone()
                        });
                    }
                    if device.supports_output() {
                        let config_ranges = match device.supported_output_configs() {
                            Ok(configs) => audio_device_config_ranges(configs),
                            Err(error) => {
                                errors.push(format!("{} output formats: {error}", info.name));
                                Vec::new()
                            }
                        };
                        catalog.outputs.push(AudioDeviceInfo {
                            config_ranges,
                            ..info
                        });
                    }
                }
            }
            Err(error) => errors.push(format!("{host_id}: {error}")),
        }
    }
    #[cfg(windows)]
    enumerate_wasapi_exclusive_capabilities(&mut catalog, &mut errors);
    if !errors.is_empty() {
        catalog.error = Some(errors.join("; "));
    }

    catalog
        .inputs
        .sort_by(|left, right| left.name.cmp(&right.name));
    catalog
        .outputs
        .sort_by(|left, right| left.name.cmp(&right.name));
    catalog
}

#[cfg(windows)]
fn enumerate_wasapi_exclusive_capabilities(
    catalog: &mut AudioDeviceCatalog,
    errors: &mut Vec<String>,
) {
    if !catalog
        .inputs
        .iter()
        .chain(&catalog.outputs)
        .any(|device| device.id.starts_with("wasapi:"))
    {
        return;
    }
    let result = std::thread::spawn(enumerate_wasapi_exclusive_capabilities_on_mta).join();
    let snapshot = match result {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(error)) => {
            errors.push(error);
            return;
        }
        Err(_) => {
            errors.push("WASAPI exclusive capability discovery worker panicked".to_owned());
            return;
        }
    };
    errors.extend(snapshot.errors);
    catalog.default_exclusive_input_id = snapshot.default_input_id;
    catalog.default_exclusive_output_id = snapshot.default_output_id;
    for capability in snapshot.devices {
        let devices = match capability.direction {
            wasapi::Direction::Capture => &mut catalog.inputs,
            wasapi::Direction::Render => &mut catalog.outputs,
        };
        if let Some(device) = devices.iter_mut().find(|device| device.id == capability.id) {
            device.exclusive_sample_rates = capability.sample_rates;
            device.exclusive_default_period_hns = capability.default_period_hns;
            device.exclusive_minimum_period_hns = capability.minimum_period_hns;
        }
    }
}

#[cfg(windows)]
struct WasapiExclusiveDeviceCapabilities {
    id: String,
    direction: wasapi::Direction,
    sample_rates: Vec<u32>,
    default_period_hns: Option<u64>,
    minimum_period_hns: Option<u64>,
}

#[cfg(windows)]
struct WasapiExclusiveCapabilitySnapshot {
    devices: Vec<WasapiExclusiveDeviceCapabilities>,
    errors: Vec<String>,
    default_input_id: Option<String>,
    default_output_id: Option<String>,
}

#[cfg(windows)]
fn enumerate_wasapi_exclusive_capabilities_on_mta()
-> Result<WasapiExclusiveCapabilitySnapshot, String> {
    wasapi::initialize_mta()
        .ok()
        .map_err(|error| format!("Could not initialize Windows audio COM: {error}"))?;
    let _com = WasapiComGuard;
    let enumerator = wasapi::DeviceEnumerator::new().map_err(|error| error.to_string())?;
    let mut devices = Vec::new();
    let mut errors = Vec::new();
    let default_input_id = enumerator
        .get_default_device(&wasapi::Direction::Capture)
        .ok()
        .and_then(|device| device.get_id().ok())
        .map(|id| format!("wasapi:{id}"));
    let default_output_id = enumerator
        .get_default_device(&wasapi::Direction::Render)
        .ok()
        .and_then(|device| device.get_id().ok())
        .map(|id| format!("wasapi:{id}"));
    for direction in [wasapi::Direction::Capture, wasapi::Direction::Render] {
        let collection = match enumerator.get_device_collection(&direction) {
            Ok(collection) => collection,
            Err(error) => {
                errors.push(format!("WASAPI {direction:?} device list: {error}"));
                continue;
            }
        };
        let count = match collection.get_nbr_devices() {
            Ok(count) => count,
            Err(error) => {
                errors.push(format!("WASAPI {direction:?} device count: {error}"));
                continue;
            }
        };
        for index in 0..count {
            let device = match collection.get_device_at_index(index) {
                Ok(device) => device,
                Err(error) => {
                    errors.push(format!("WASAPI {direction:?} device {index}: {error}"));
                    continue;
                }
            };
            let id = match device.get_id() {
                Ok(id) => format!("wasapi:{id}"),
                Err(error) => {
                    errors.push(format!("WASAPI {direction:?} device {index} ID: {error}"));
                    continue;
                }
            };
            let sample_rates = match wasapi_exclusive_sample_rates(&device) {
                Ok(rates) => rates,
                Err(error) => {
                    errors.push(format!("{id} exclusive sample rates: {error}"));
                    Vec::new()
                }
            };
            let (default_period_hns, minimum_period_hns) = match wasapi_device_periods(&device) {
                Ok((default, minimum)) => (Some(default), Some(minimum)),
                Err(error) => {
                    errors.push(format!("{id} exclusive device periods: {error}"));
                    (None, None)
                }
            };
            devices.push(WasapiExclusiveDeviceCapabilities {
                id,
                direction,
                sample_rates,
                default_period_hns,
                minimum_period_hns,
            });
        }
    }
    Ok(WasapiExclusiveCapabilitySnapshot {
        devices,
        errors,
        default_input_id,
        default_output_id,
    })
}

#[cfg(windows)]
fn wasapi_device_periods(device: &wasapi::Device) -> Result<(u64, u64), String> {
    let client = device
        .get_iaudioclient()
        .map_err(|error| format!("could not create an audio client: {error}"))?;
    let (default_period_hns, minimum_period_hns) = client
        .get_device_period()
        .map_err(|error| format!("could not read endpoint periods: {error}"))?;
    let default_period_hns = u64::try_from(default_period_hns)
        .ok()
        .filter(|period| *period > 0)
        .ok_or_else(|| "WASAPI returned an invalid default period".to_owned())?;
    let minimum_period_hns = u64::try_from(minimum_period_hns)
        .ok()
        .filter(|period| *period > 0)
        .ok_or_else(|| "WASAPI returned an invalid minimum period".to_owned())?;
    Ok((default_period_hns, minimum_period_hns))
}

#[cfg(windows)]
fn wasapi_exclusive_sample_rates(device: &wasapi::Device) -> Result<Vec<u32>, String> {
    let shared_format = device
        .get_device_format()
        .map_err(|error| format!("could not read shared device format: {error}"))?;
    let channels = usize::from(shared_format.get_nchannels().max(1));
    let mut probe = wasapi::CapabilityProbe::new(device)
        .map_err(|error| format!("could not create format probe: {error}"))?;
    // Probe the actual endpoint format checker instead of trusting potentially stale driver ranges.
    probe.set_data_ranges(Vec::new());
    let mut candidates = COMMON_AUDIO_SAMPLE_RATES.to_vec();
    candidates.push(shared_format.get_samplespersec());
    candidates.sort_unstable();
    candidates.dedup();
    candidates.retain(|rate| (8_000..=384_000).contains(rate));
    Ok(candidates
        .into_iter()
        .filter(|rate| {
            probe
                .supported_formats(*rate as usize, channels)
                .iter()
                .any(wasapi_format_is_supported_by_stream_builder)
        })
        .collect())
}

#[cfg(windows)]
fn wasapi_format_is_supported_by_stream_builder(format: &wasapi::WaveFormat) -> bool {
    let bits = format.get_bitspersample();
    let valid_bits = format.get_validbitspersample();
    matches!(
        (format.get_subformat(), bits, valid_bits),
        (Ok(wasapi::SampleType::Float), 32, 32)
            | (Ok(wasapi::SampleType::Int), 16, 16)
            | (Ok(wasapi::SampleType::Int), 24, 24)
            | (Ok(wasapi::SampleType::Int), 32, 32)
    )
}

fn audio_device_config_ranges(
    configs: impl Iterator<Item = SupportedStreamConfigRange>,
) -> Vec<AudioDeviceConfigRange> {
    let mut ranges = configs
        .map(|config| {
            let (minimum_buffer_frames, maximum_buffer_frames) = match config.buffer_size() {
                SupportedBufferSize::Range { min, max } => (Some(*min), Some(*max)),
                SupportedBufferSize::Unknown => (None, None),
            };
            AudioDeviceConfigRange {
                minimum_sample_rate: config.min_sample_rate(),
                maximum_sample_rate: config.max_sample_rate(),
                channels: config.channels(),
                minimum_buffer_frames,
                maximum_buffer_frames,
            }
        })
        .collect::<Vec<_>>();
    ranges.sort_unstable_by_key(|range| {
        (
            range.minimum_sample_rate,
            range.maximum_sample_rate,
            range.channels,
            range.minimum_buffer_frames,
            range.maximum_buffer_frames,
        )
    });
    ranges.dedup();
    ranges
}

/// Active device streams. Input is captured from the selected channel pair into
/// a lock-free stereo ring buffer and exposed as a peak meter; output can play
/// project audio, a test tone, and monitored input together.
pub struct AudioEngine {
    source: Arc<AtomicU8>,
    requested_source: AtomicU8,
    playback: Arc<PlaybackState>,
    sample_rate: u32,
    input_peak: Arc<AtomicU32>,
    input_callback_frames: Arc<AtomicU32>,
    input_recording: Arc<ArcSwapOption<InputRecordingState>>,
    input_monitor_recording: Arc<ArcSwapOption<InputRecordingState>>,
    input_monitor_ring: Arc<AudioRingBuffer>,
    input_channel_pair: Arc<AtomicU16>,
    input_channel_count: u16,
    input_latency_estimate: Arc<AudioLatencyEstimate>,
    error: Arc<AudioErrorState>,
    input_stream: Option<Stream>,
    output_stream: Option<Stream>,
    input_active: bool,
    output_active: bool,
    #[cfg(windows)]
    exclusive_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    #[cfg(windows)]
    exclusive_threads: Vec<std::thread::JoinHandle<()>>,
}

/// A lock-free view of the current Playlist playback frame for realtime schedulers.
#[derive(Clone)]
pub struct ProjectPlaybackClock {
    playback: Arc<PlaybackState>,
}

impl ProjectPlaybackClock {
    pub fn position_frames(&self) -> u64 {
        self.playback.position_frames()
    }

    pub fn is_active(&self) -> bool {
        self.playback.is_project_active()
    }
}

impl AudioEngine {
    pub fn start(settings: &AudioSettings) -> Result<Self, String> {
        validate_settings(settings)?;
        match settings.access {
            AudioAccess::Shared => Self::start_shared(settings),
            AudioAccess::Exclusive => Self::start_exclusive(settings),
        }
    }

    pub fn set_test_tone(&self, enabled: bool) {
        if enabled {
            self.playback.pause();
        }
        self.request_output_source(if enabled {
            AUDIO_SOURCE_TEST_TONE
        } else {
            AUDIO_SOURCE_SILENT
        });
    }

    pub fn set_input_monitor(&self, enabled: bool) -> Result<(), String> {
        if enabled && (!self.input_active || !self.output_active) {
            return Err(
                "Select and start both an input and an output device to monitor input".into(),
            );
        }
        if enabled {
            if self
                .input_monitor_recording
                .load()
                .as_deref()
                .is_some_and(|recording| !recording.active.load(Ordering::Acquire))
            {
                self.input_monitor_recording.store(None);
            }
        } else if let Some(recording) = self.input_monitor_recording.load().as_deref() {
            recording.stop();
        }
        self.playback
            .input_monitor_active
            .store(enabled, Ordering::Release);
        self.playback
            .input_monitor_ring_flush_requested
            .store(true, Ordering::Release);
        Ok(())
    }

    /// Start outputting interleaved stereo project audio at the device sample rate.
    pub fn set_project_playback(&self, samples: Vec<f32>) -> Result<(), String> {
        if !self.output_active {
            return Err("Start an output device before playing the project".into());
        }
        if samples.is_empty() || !samples.len().is_multiple_of(2) {
            return Err("Rendered project audio must contain stereo sample frames".into());
        }
        self.playback.start(samples);
        self.request_output_source(AUDIO_SOURCE_PROJECT);
        Ok(())
    }

    /// Play a browser sample preview alongside any active project audio.
    pub fn set_browser_preview(&self, samples: Vec<f32>) -> Result<(), String> {
        if !self.output_active {
            return Err("Start an output device before previewing a sample".into());
        }
        if samples.is_empty() || !samples.len().is_multiple_of(2) {
            return Err("Sample preview audio must contain stereo frames".into());
        }
        self.playback.start_browser_preview(samples);
        Ok(())
    }

    /// Stop the independent Browser preview voice without stopping project playback.
    pub fn stop_browser_preview(&self) {
        self.playback.stop_browser_preview();
    }

    pub fn browser_preview_active(&self) -> bool {
        self.playback.browser_preview_active()
    }

    /// Number of stereo frames in the currently loaded Browser preview.
    pub fn browser_preview_frame_count(&self) -> u64 {
        self.playback.browser_preview_frame_count()
    }

    /// Next frame position in the currently loaded Browser preview.
    pub fn browser_preview_position_frames(&self) -> u64 {
        self.playback.browser_preview_position_frames()
    }

    /// Seek to a frame in the loaded Browser preview and resume it there.
    pub fn seek_browser_preview(&self, frame: u64) {
        self.playback.seek_browser_preview(frame);
    }

    /// Repeat the loaded Browser preview region while it plays.
    pub fn set_browser_preview_looping(&self, looping: bool) {
        self.playback.set_browser_preview_looping(looping);
    }

    pub fn set_browser_preview_gain(&self, gain: f32) {
        let gain = if gain.is_finite() {
            gain.clamp(0.0, 1.0)
        } else {
            1.0
        };
        if self.output_active {
            self.playback
                .enqueue_parameter_command(ParameterCommand::SetBrowserPreviewGain(gain.to_bits()));
        } else {
            self.playback
                .browser_preview_gain
                .store(gain.to_bits(), Ordering::Release);
        }
    }

    /// Set a live master output gain for every device playback source.
    ///
    /// The audio callback smooths changes to prevent clicks.
    pub fn set_output_gain(&self, gain: f32) {
        let gain = if gain.is_finite() {
            gain.clamp(0.0, OUTPUT_GAIN_MAX)
        } else {
            1.0
        };
        self.playback
            .output_gain
            .store(gain.to_bits(), Ordering::Release);
    }

    /// Set the transport playback rate, changing both pitch and song duration.
    pub fn set_project_playback_speed(&self, speed_multiplier: f32) -> Result<(), String> {
        if !valid_project_playback_speed(speed_multiplier) {
            return Err("Project playback speed must be between 0.25 and 4.0".to_owned());
        }
        self.playback
            .playback_speed
            .store(speed_multiplier.to_bits(), Ordering::Release);
        Ok(())
    }

    /// Begin consuming interleaved stereo samples from a bounded producer ring.
    ///
    /// VST3 and other block processors can render on a worker thread while the shared or
    /// WASAPI-exclusive device callback pulls frames without allocating or locking.
    pub fn begin_streaming_playback(&self) -> Result<StreamingAudioWriter, String> {
        self.begin_streaming_playback_from_frame(0)
    }

    /// Begin streaming playback with the device position initialized to a song-frame offset.
    ///
    /// The producer must omit audio before this frame from its stream. The offset keeps the
    /// transport display and MIDI clock aligned with the requested song position.
    pub fn begin_streaming_playback_from_frame(
        &self,
        start_frame: u64,
    ) -> Result<StreamingAudioWriter, String> {
        if !self.output_active {
            return Err("Start an output device before streaming audio".into());
        }
        let writer = self.playback.start_streaming(start_frame);
        self.request_output_source(AUDIO_SOURCE_STREAM);
        Ok(writer)
    }

    /// Prepare a replacement project stream without disturbing the currently audible stream.
    ///
    /// The caller should stop and join the old render producer before staging a replacement that
    /// uses the same plug-in instances. Once the replacement has buffered enough audio, request a
    /// handoff; the output callback switches streams at a buffer boundary and skips staged audio
    /// that corresponds to frames already consumed by the old stream.
    pub fn begin_staged_streaming_playback_from_frame(
        &self,
        start_frame: u64,
    ) -> Result<(StreamingAudioWriter, StagedStreamingPlayback), String> {
        if !self.output_active {
            return Err("Start an output device before staging streamed audio".into());
        }
        let current_guard = self.playback.streaming.load();
        let current = current_guard.as_ref().ok_or_else(|| {
            "An active project stream is required for a staged handoff".to_owned()
        })?;
        if current.project_source_index.is_none()
            || current.cancelled.load(Ordering::Acquire)
            || self.playback.pending_streaming.load().is_some()
        {
            return Err("The current project stream cannot accept a staged handoff".to_owned());
        }
        let (stream, mut writers) =
            create_streaming_playback(start_frame, AUDIO_RING_CAPACITY, 1, true);
        let writer = writers
            .pop()
            .ok_or_else(|| "Could not allocate a staged project stream".to_owned())?;
        let staged = StagedStreamingPlayback {
            playback: Arc::clone(&self.playback),
            stream,
        };
        Ok((writer, staged))
    }

    /// Let the output callback replace its current Playlist stream with a staged stream.
    pub fn request_staged_streaming_handoff(
        &self,
        staged: &StagedStreamingPlayback,
    ) -> Result<(), String> {
        if !Arc::ptr_eq(&self.playback, &staged.playback) {
            return Err("The staged stream belongs to a different audio engine".to_owned());
        }
        if staged.stream.cancelled.load(Ordering::Acquire) {
            return Err("The staged stream was cancelled".to_owned());
        }
        if staged.stream.handoff_committed.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.playback.pending_streaming.load().is_some() {
            return Err("Another staged stream handoff is already pending".to_owned());
        }
        staged.stream.handoff_target_frame.store(
            staged.stream.consumed_frames.load(Ordering::Acquire),
            Ordering::Release,
        );
        self.playback
            .pending_streaming
            .store(Some(Arc::clone(&staged.stream)));
        self.request_output_source(AUDIO_SOURCE_STREAM);
        Ok(())
    }

    /// Cancel a staged stream that has not yet replaced the current project stream.
    pub fn cancel_staged_streaming_handoff(
        &self,
        staged: &StagedStreamingPlayback,
    ) -> Result<(), String> {
        if !Arc::ptr_eq(&self.playback, &staged.playback) {
            return Err("The staged stream belongs to a different audio engine".to_owned());
        }
        if staged.is_committed() {
            return Err("The staged stream has already been committed".to_owned());
        }
        if let Some(pending) = self.playback.pending_streaming.load_full()
            && Arc::ptr_eq(&pending, &staged.stream)
        {
            self.playback.pending_streaming.store(None);
        }
        staged.stream.cancelled.store(true, Ordering::Release);
        staged.stream.active.store(false, Ordering::Release);
        Ok(())
    }

    /// Release retired stream buffers on the control thread after the audio callback hands off.
    pub fn reap_retired_streaming_playbacks(&self) {
        while self.playback.retired_streaming.pop().is_some() {}
    }

    /// Begin a low-latency stream for interactive instrument audition.
    ///
    /// The smaller ring limits the delay between a producer's note event and audible output.
    pub fn begin_live_streaming_playback(&self) -> Result<StreamingAudioWriter, String> {
        self.begin_live_streaming_playback_from_frame(0)
    }

    /// Begin a low-latency stream with the device position initialized to a song-frame offset.
    pub fn begin_live_streaming_playback_from_frame(
        &self,
        start_frame: u64,
    ) -> Result<StreamingAudioWriter, String> {
        if !self.output_active {
            return Err("Start an output device before streaming audio".into());
        }
        let writer = self
            .playback
            .start_streaming_with_capacity(start_frame, LIVE_STREAM_RING_CAPACITY);
        self.request_output_source(AUDIO_SOURCE_STREAM);
        Ok(writer)
    }

    /// Begin one low-latency audio playback stream with independent producers mixed together.
    ///
    /// Each returned writer contributes one stereo source to the same device stream. All writers
    /// share the playback clock and must be finished independently before the stream drains.
    pub fn begin_live_mixed_streaming_playback(
        &self,
        source_count: usize,
    ) -> Result<Vec<StreamingAudioWriter>, String> {
        if !self.output_active {
            return Err("Start an output device before streaming audio".into());
        }
        if !(1..=MAX_LIVE_STREAM_SOURCES).contains(&source_count) {
            return Err(format!(
                "live audio mixing supports between 1 and {MAX_LIVE_STREAM_SOURCES} sources"
            ));
        }
        let writers = self.playback.start_streaming_sources_with_capacity(
            0,
            LIVE_STREAM_RING_CAPACITY,
            source_count,
        );
        self.request_output_source(AUDIO_SOURCE_STREAM);
        Ok(writers)
    }

    /// Add live instrument audio sources to the currently playing stream without replacing it.
    pub fn begin_live_mixed_streaming_overlay(
        &self,
        source_count: usize,
    ) -> Result<Vec<StreamingAudioWriter>, String> {
        if !self.output_active {
            return Err("Start an output device before streaming audio".into());
        }
        let writers = self.playback.add_streaming_sources(source_count)?;
        self.request_output_source(AUDIO_SOURCE_STREAM);
        Ok(writers)
    }

    /// Add one live instrument source to the currently playing stream without replacing it.
    pub fn begin_live_streaming_overlay(&self) -> Result<StreamingAudioWriter, String> {
        self.begin_live_mixed_streaming_overlay(1)?
            .pop()
            .ok_or_else(|| "No live audio source was allocated".to_owned())
    }

    pub fn pause_project_playback(&self) {
        self.playback.pause();
        self.stop_playback_source();
    }

    pub fn resume_project_playback(&self) -> Result<(), String> {
        if !self.output_active {
            return Err("Start an output device before resuming project playback".into());
        }
        self.playback.resume()?;
        self.request_output_source(if self.playback.is_streaming() {
            AUDIO_SOURCE_STREAM
        } else {
            AUDIO_SOURCE_PROJECT
        });
        Ok(())
    }

    pub fn stop_project_playback(&self) {
        self.playback.stop();
        self.stop_playback_source();
    }

    pub fn project_playback_active(&self) -> bool {
        self.playback.is_project_active()
    }

    /// Number of device frames that arrived before streamed audio was ready.
    pub fn streaming_underrun_frames(&self) -> u64 {
        self.playback
            .streaming
            .load()
            .as_deref()
            .map(|streaming| streaming.underrun_frames.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// Current song position in device frames, including a streaming seek offset when present.
    pub fn project_playback_position_frames(&self) -> u64 {
        self.playback.position_frames()
    }

    /// Clone a lock-free playback clock for a realtime MIDI or metering worker.
    pub fn project_playback_clock(&self) -> ProjectPlaybackClock {
        ProjectPlaybackClock {
            playback: Arc::clone(&self.playback),
        }
    }

    pub fn input_peak(&self) -> f32 {
        f32::from_bits(self.input_peak.load(Ordering::Acquire)).clamp(0.0, 1.0)
    }

    pub fn input_level_db(&self) -> f32 {
        let peak = self.input_peak().max(0.000_001);
        20.0 * peak.log10()
    }

    /// Peak level of the latest output-device buffer, normalized to 0.0–1.0.
    pub fn output_peak(&self) -> f32 {
        f32::from_bits(self.playback.output_peak.load(Ordering::Acquire)).clamp(0.0, 1.0)
    }

    /// Left and right peaks from the most recent output-device buffer before device clipping.
    pub fn output_peak_channels(&self) -> (f32, f32) {
        let peak = |value: &AtomicU32| {
            let peak = f32::from_bits(value.load(Ordering::Acquire));
            if peak.is_finite() { peak.max(0.0) } else { 0.0 }
        };
        (
            peak(&self.playback.output_peak_left),
            peak(&self.playback.output_peak_right),
        )
    }

    /// Frame count from the most recent output-device callback, if one has arrived.
    pub fn last_output_callback_frames(&self) -> Option<u32> {
        nonzero_callback_frames(self.playback.output_callback_frames.load(Ordering::Acquire))
    }

    /// Frame count from the most recent input-device callback, if one has arrived.
    pub fn last_input_callback_frames(&self) -> Option<u32> {
        nonzero_callback_frames(self.input_callback_frames.load(Ordering::Acquire))
    }

    pub fn input_active(&self) -> bool {
        self.input_active
    }

    /// Stereo input pair currently selected from the active input device.
    pub fn input_channel_pair(&self) -> u16 {
        self.input_channel_pair.load(Ordering::Acquire)
    }

    /// Number of valid stereo input pairs exposed by this engine's active input stream.
    pub fn input_channel_pair_count(&self) -> u16 {
        if self.input_channel_count == 0 {
            0
        } else if self.input_channel_count == 1 {
            1
        } else {
            self.input_channel_count / 2
        }
    }

    /// Select a stereo input pair without restarting the device stream.
    pub fn set_input_channel_pair(&self, input_channel_pair: u16) -> Result<(), String> {
        if !self.input_active {
            return Err("Start an input device before selecting its channels".into());
        }
        selected_stereo_input_channels(usize::from(self.input_channel_count), input_channel_pair)?;
        self.input_channel_pair
            .store(input_channel_pair, Ordering::Release);
        Ok(())
    }

    /// Start capturing stereo frames from the active input device into a bounded queue.
    ///
    /// The returned reader is intended for a worker thread. It must be drained regularly;
    /// frames are dropped and counted if the queue fills. Call [`Self::stop_input_recording`]
    /// to stop capture, then drain the reader until [`AudioInputRecording::is_finished`].
    pub fn start_input_recording(&self) -> Result<AudioInputRecording, String> {
        if !self.input_active {
            return Err("Start an input device before recording audio".into());
        }
        if self
            .input_recording
            .load()
            .as_deref()
            .is_some_and(|recording| recording.active.load(Ordering::Acquire))
        {
            return Err("Audio input recording is already active".into());
        }
        let recording = Arc::new(InputRecordingState::new(
            self.sample_rate,
            INPUT_RECORDING_BUFFER_FRAMES,
        ));
        self.input_recording.store(Some(Arc::clone(&recording)));
        Ok(AudioInputRecording { recording })
    }

    /// Start a separate capture queue for processing input through the Mixer before monitoring.
    ///
    /// The caller should then enable [`Self::set_input_monitor`] and drain the returned reader on
    /// a worker thread, writing processed output through the paired sink.
    pub fn start_input_monitor_processing(
        &self,
    ) -> Result<(AudioInputRecording, AudioInputMonitorOutput), String> {
        if !self.input_active || !self.output_active {
            return Err(
                "Select and start both an input and an output device to monitor input".into(),
            );
        }
        if self
            .input_monitor_recording
            .load()
            .as_deref()
            .is_some_and(|recording| recording.active.load(Ordering::Acquire))
        {
            return Err("Mixer input monitoring is already active".into());
        }
        let recording = Arc::new(InputRecordingState::new(
            self.sample_rate,
            INPUT_RECORDING_BUFFER_FRAMES,
        ));
        self.input_monitor_recording
            .store(Some(Arc::clone(&recording)));
        Ok((
            AudioInputRecording {
                recording: Arc::clone(&recording),
            },
            AudioInputMonitorOutput {
                ring: Arc::clone(&self.input_monitor_ring),
                recording,
                processed_recording: AudioInputMonitorRecorder {
                    sample_rate: self.sample_rate,
                    recording: Arc::new(ArcSwapOption::empty()),
                },
            },
        ))
    }

    /// Stop the active input recording without discarding its queued frames.
    pub fn stop_input_recording(&self) {
        if let Some(recording) = self.input_recording.load().as_deref() {
            recording.stop();
        }
        self.input_recording.store(None);
    }

    /// Number of device stream errors reported since this engine started.
    pub fn stream_error_count(&self) -> u64 {
        self.error.count.load(Ordering::Relaxed)
    }

    /// Recent device stream error messages, oldest first.
    pub fn stream_error_history(&self) -> Vec<String> {
        self.error.recent()
    }

    pub fn output_active(&self) -> bool {
        self.output_active
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Estimated time between capture and delivery to the input processing callback.
    pub fn estimated_input_latency_ms(&self) -> Option<f32> {
        self.input_latency_estimate.estimated_milliseconds()
    }

    /// Estimated time from the output render block to predicted device playback.
    pub fn estimated_output_latency_ms(&self) -> Option<f32> {
        self.playback
            .output_latency_estimate
            .estimated_milliseconds()
    }

    /// Wait briefly for the active input stream to provide timing samples.
    pub fn wait_for_input_latency_estimate(&self, timeout: Duration) -> Option<f32> {
        self.input_latency_estimate.wait_for_estimate(timeout)
    }

    pub fn take_error(&self) -> Option<String> {
        self.error.take_latest()
    }

    fn stop_playback_source(&self) {
        if matches!(
            self.requested_source.load(Ordering::Acquire),
            AUDIO_SOURCE_PROJECT | AUDIO_SOURCE_STREAM
        ) {
            self.request_output_source(AUDIO_SOURCE_SILENT);
        }
    }

    fn request_output_source(&self, source: u8) {
        self.requested_source.store(source, Ordering::Release);
        if self.output_active {
            self.playback
                .enqueue_transport_command(TransportCommand::SetOutputSource(source));
        } else {
            self.source.store(source, Ordering::Release);
        }
    }

    fn start_shared(settings: &AudioSettings) -> Result<Self, String> {
        let host = cpal::default_host();
        let ring = Arc::new(AudioRingBuffer::new(INPUT_MONITOR_RING_CAPACITY));
        let source = Arc::new(AtomicU8::new(AUDIO_SOURCE_SILENT));
        let playback = Arc::new(PlaybackState::new());
        playback
            .output_gain
            .store(settings.output_gain.to_bits(), Ordering::Release);
        let input_peak = Arc::new(AtomicU32::new(0.0_f32.to_bits()));
        let input_callback_frames = Arc::new(AtomicU32::new(0));
        let input_recording = Arc::new(ArcSwapOption::empty());
        let input_monitor_recording = Arc::new(ArcSwapOption::empty());
        let input_latency_estimate = Arc::new(AudioLatencyEstimate::default());
        let input_channel_pair = Arc::new(AtomicU16::new(settings.input_channel_pair));
        let error = Arc::new(AudioErrorState::default());

        let output_device = if settings.enable_output {
            resolve_device(&host, settings.output_device_id.as_deref(), false)?
        } else {
            None
        };
        let input_device = if settings.enable_input {
            resolve_device(&host, settings.input_device_id.as_deref(), true)?
        } else {
            None
        };
        let mut output_stream = None;
        let mut input_stream = None;
        let mut input_channel_count = 0;

        if let Some(device) = output_device {
            let stream = build_output_stream(
                &device,
                settings,
                Arc::clone(&source),
                Arc::clone(&playback),
                Arc::clone(&ring),
                Arc::clone(&error),
            )?;
            stream
                .play()
                .map_err(|error| format!("Could not start output stream: {error}"))?;
            output_stream = Some(stream);
        }

        if let Some(device) = input_device {
            let (stream, channel_count) = build_input_stream(
                &device,
                settings,
                InputStreamState {
                    ring: Arc::clone(&ring),
                    peak: Arc::clone(&input_peak),
                    source: Arc::clone(&source),
                    recording: Arc::clone(&input_recording),
                    monitor_recording: Arc::clone(&input_monitor_recording),
                    monitor_active: Arc::clone(&playback.input_monitor_active),
                    input_latency_estimate: Arc::clone(&input_latency_estimate),
                    error: Arc::clone(&error),
                    callback_frames: Arc::clone(&input_callback_frames),
                    input_channel_pair: Arc::clone(&input_channel_pair),
                },
            )?;
            stream
                .play()
                .map_err(|error| format!("Could not start input stream: {error}"))?;
            input_stream = Some(stream);
            input_channel_count = channel_count;
        }

        if output_stream.is_none() && input_stream.is_none() {
            return Err("No active input or output device is available".into());
        }

        Ok(Self {
            source,
            requested_source: AtomicU8::new(AUDIO_SOURCE_SILENT),
            playback,
            sample_rate: settings.sample_rate,
            input_peak,
            input_callback_frames,
            input_recording,
            input_monitor_recording,
            input_monitor_ring: Arc::clone(&ring),
            input_channel_pair,
            input_channel_count,
            input_latency_estimate,
            error,
            input_active: input_stream.is_some(),
            output_active: output_stream.is_some(),
            input_stream,
            output_stream,
            #[cfg(windows)]
            exclusive_stop: None,
            #[cfg(windows)]
            exclusive_threads: Vec::new(),
        })
    }

    #[cfg(windows)]
    fn start_exclusive(settings: &AudioSettings) -> Result<Self, String> {
        use std::sync::mpsc;
        use std::thread;

        let ring = Arc::new(AudioRingBuffer::new(INPUT_MONITOR_RING_CAPACITY));
        let source = Arc::new(AtomicU8::new(AUDIO_SOURCE_SILENT));
        let playback = Arc::new(PlaybackState::new());
        playback
            .output_gain
            .store(settings.output_gain.to_bits(), Ordering::Release);
        let input_peak = Arc::new(AtomicU32::new(0.0_f32.to_bits()));
        let input_callback_frames = Arc::new(AtomicU32::new(0));
        let input_recording = Arc::new(ArcSwapOption::empty());
        let input_monitor_recording = Arc::new(ArcSwapOption::empty());
        let input_latency_estimate = Arc::new(AudioLatencyEstimate::default());
        let input_channel_pair = Arc::new(AtomicU16::new(settings.input_channel_pair));
        let input_channel_count = Arc::new(AtomicU16::new(0));
        let error = Arc::new(AudioErrorState::default());
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads: Vec<thread::JoinHandle<()>> = Vec::new();
        let mut receivers = Vec::new();

        if settings.enable_output
            && (settings.output_device_id.is_some()
                || cpal::default_host().default_output_device().is_some())
        {
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let worker_settings = settings.clone();
            let worker_source = Arc::clone(&source);
            let worker_playback = Arc::clone(&playback);
            let worker_ring = Arc::clone(&ring);
            let worker_peak = Arc::clone(&input_peak);
            let worker_input_callback_frames = Arc::clone(&input_callback_frames);
            let worker_recording = Arc::clone(&input_recording);
            let worker_monitor_recording = Arc::clone(&input_monitor_recording);
            let worker_input_latency_estimate = Arc::clone(&input_latency_estimate);
            let worker_input_channel_pair = Arc::clone(&input_channel_pair);
            let worker_input_channel_count = Arc::clone(&input_channel_count);
            let worker_error = Arc::clone(&error);
            let worker_stop = Arc::clone(&stop);
            let output_thread = thread::Builder::new()
                .name("wasapi-exclusive-render".into())
                .spawn(move || {
                    wasapi_exclusive_output_worker(
                        worker_settings.output_device_id.as_deref(),
                        &worker_settings,
                        WasapiWorkerState {
                            source: worker_source,
                            playback: worker_playback,
                            ring: worker_ring,
                            input_peak: worker_peak,
                            input_callback_frames: worker_input_callback_frames,
                            input_recording: worker_recording,
                            input_monitor_recording: worker_monitor_recording,
                            input_latency_estimate: worker_input_latency_estimate,
                            input_channel_pair: worker_input_channel_pair,
                            input_channel_count: worker_input_channel_count,
                            error: worker_error,
                            stop: worker_stop,
                        },
                        ready_tx,
                    );
                });
            match output_thread {
                Ok(thread) => threads.push(thread),
                Err(error) => {
                    stop.store(true, Ordering::Release);
                    for thread in threads.drain(..) {
                        let _ = thread.join();
                    }
                    return Err(format!("Could not start WASAPI output worker: {error}"));
                }
            }
            receivers.push(("output", ready_rx));
        }

        if settings.enable_input
            && (settings.input_device_id.is_some()
                || cpal::default_host().default_input_device().is_some())
        {
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let worker_settings = settings.clone();
            let worker_ring = Arc::clone(&ring);
            let worker_peak = Arc::clone(&input_peak);
            let worker_source = Arc::clone(&source);
            let worker_playback = Arc::clone(&playback);
            let worker_input_callback_frames = Arc::clone(&input_callback_frames);
            let worker_recording = Arc::clone(&input_recording);
            let worker_monitor_recording = Arc::clone(&input_monitor_recording);
            let worker_input_latency_estimate = Arc::clone(&input_latency_estimate);
            let worker_input_channel_pair = Arc::clone(&input_channel_pair);
            let worker_input_channel_count = Arc::clone(&input_channel_count);
            let worker_error = Arc::clone(&error);
            let worker_stop = Arc::clone(&stop);
            let input_thread = thread::Builder::new()
                .name("wasapi-exclusive-capture".into())
                .spawn(move || {
                    wasapi_exclusive_input_worker(
                        worker_settings.input_device_id.as_deref(),
                        &worker_settings,
                        WasapiWorkerState {
                            source: worker_source,
                            playback: worker_playback,
                            ring: worker_ring,
                            input_peak: worker_peak,
                            input_callback_frames: worker_input_callback_frames,
                            input_recording: worker_recording,
                            input_monitor_recording: worker_monitor_recording,
                            input_latency_estimate: worker_input_latency_estimate,
                            input_channel_pair: worker_input_channel_pair,
                            input_channel_count: worker_input_channel_count,
                            error: worker_error,
                            stop: worker_stop,
                        },
                        ready_tx,
                    );
                });
            match input_thread {
                Ok(thread) => threads.push(thread),
                Err(error) => {
                    stop.store(true, Ordering::Release);
                    for thread in threads.drain(..) {
                        let _ = thread.join();
                    }
                    return Err(format!("Could not start WASAPI input worker: {error}"));
                }
            }
            receivers.push(("input", ready_rx));
        }

        if receivers.is_empty() {
            return Err("No active input or output device is available".into());
        }

        for (direction, receiver) in receivers {
            match receiver.recv_timeout(Duration::from_secs(10)) {
                Ok(Ok(())) => {}
                Ok(Err(message)) => {
                    stop.store(true, Ordering::Release);
                    for thread in threads {
                        let _ = thread.join();
                    }
                    return Err(format!("WASAPI exclusive {direction} failed: {message}"));
                }
                Err(error_kind) => {
                    stop.store(true, Ordering::Release);
                    for thread in threads {
                        let _ = thread.join();
                    }
                    return Err(format!(
                        "WASAPI exclusive {direction} did not start: {error_kind}"
                    ));
                }
            }
        }

        let output_active = settings.enable_output
            && (settings.output_device_id.is_some()
                || cpal::default_host().default_output_device().is_some());
        let input_active = settings.enable_input
            && (settings.input_device_id.is_some()
                || cpal::default_host().default_input_device().is_some());
        let input_channel_count = input_channel_count.load(Ordering::Acquire);
        Ok(Self {
            source,
            requested_source: AtomicU8::new(AUDIO_SOURCE_SILENT),
            playback,
            sample_rate: settings.sample_rate,
            input_peak,
            input_callback_frames,
            input_recording,
            input_monitor_recording,
            input_monitor_ring: Arc::clone(&ring),
            input_channel_pair,
            input_channel_count,
            input_latency_estimate,
            error,
            input_active,
            output_active,
            input_stream: None,
            output_stream: None,
            exclusive_stop: Some(stop),
            exclusive_threads: threads,
        })
    }

    #[cfg(not(windows))]
    fn start_exclusive(_settings: &AudioSettings) -> Result<Self, String> {
        Err("Exclusive device access is available through WASAPI on Windows".into())
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.playback.stop();
        self.stop_input_recording();
        if let Some(recording) = self.input_monitor_recording.load().as_deref() {
            recording.stop();
        }
        #[cfg(windows)]
        if let Some(stop) = &self.exclusive_stop {
            stop.store(true, Ordering::Release);
            for thread in self.exclusive_threads.drain(..) {
                let _ = thread.join();
            }
        }
        // Explicit field reads keep the streams alive for the lifetime of the engine.
        let _ = (&self.input_stream, &self.output_stream);
    }
}

fn validate_settings(settings: &AudioSettings) -> Result<(), String> {
    if !settings.enable_input && !settings.enable_output {
        return Err("Enable input, output, or both before starting audio".into());
    }
    if !(8_000..=384_000).contains(&settings.sample_rate) {
        return Err("Sample rate must be between 8,000 and 384,000 Hz".into());
    }
    if !(32..=8_192).contains(&settings.buffer_frames) {
        return Err("Audio buffer must be between 32 and 8,192 frames".into());
    }
    if !settings.output_gain.is_finite() || !(0.0..=OUTPUT_GAIN_MAX).contains(&settings.output_gain)
    {
        return Err("Output gain must be between 0 and 2".into());
    }
    if settings.access == AudioAccess::Exclusive
        && [
            settings.input_device_id.as_deref(),
            settings.output_device_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|id| {
            id.split_once(':')
                .is_some_and(|(host, _)| !host.eq_ignore_ascii_case("wasapi"))
        })
    {
        return Err("Exclusive device access is available through WASAPI devices only; switch to Shared access for this device".into());
    }
    Ok(())
}

fn resolve_device(
    host: &cpal::Host,
    selected_id: Option<&str>,
    input: bool,
) -> Result<Option<cpal::Device>, String> {
    match selected_id {
        Some(selected_id) => {
            let id = selected_id
                .parse::<cpal::DeviceId>()
                .map_err(|error| format!("Invalid audio device id: {error}"))?;
            let selected_host = cpal::host_from_id(id.host())
                .map_err(|error| format!("Could not open audio host for {selected_id}: {error}"))?;
            selected_host.device_by_id(&id).map(Some).ok_or_else(|| {
                format!("Selected audio device is no longer available: {selected_id}")
            })
        }
        None if input => Ok(host.default_input_device()),
        None => Ok(host.default_output_device()),
    }
}

fn select_config(
    device: &cpal::Device,
    input: bool,
    sample_rate: u32,
    requested_buffer_frames: u32,
) -> Result<SupportedStreamConfig, String> {
    let configs = if input {
        device
            .supported_input_configs()
            .map_err(|error| format!("Could not query input formats: {error}"))?
            .collect::<Vec<_>>()
    } else {
        device
            .supported_output_configs()
            .map_err(|error| format!("Could not query output formats: {error}"))?
            .collect::<Vec<_>>()
    };
    configs
        .into_iter()
        .filter(|config| {
            config.min_sample_rate() <= sample_rate && sample_rate <= config.max_sample_rate()
        })
        .max_by_key(|config| {
            let channels = u32::from(config.channels());
            let format_score = match config.sample_format() {
                SampleFormat::F32 => 8,
                SampleFormat::I16 => 7,
                SampleFormat::F64 => 6,
                SampleFormat::I24 => 5,
                SampleFormat::I32 => 4,
                SampleFormat::U16 => 3,
                _ => 0,
            };
            let (buffer_score, buffer_distance) =
                buffer_config_preference(config.buffer_size(), requested_buffer_frames);
            (
                channels == 2,
                channels,
                buffer_score,
                std::cmp::Reverse(buffer_distance),
                format_score,
            )
        })
        .map(|config| config.with_sample_rate(sample_rate))
        .ok_or_else(|| {
            format!(
                "The selected {} device does not support {sample_rate} Hz",
                if input { "input" } else { "output" }
            )
        })
}

fn buffer_config_preference(buffer_size: &SupportedBufferSize, requested: u32) -> (u8, u32) {
    match buffer_size {
        SupportedBufferSize::Range { min, max } if (*min..=*max).contains(&requested) => (3, 0),
        SupportedBufferSize::Range { min, max } => {
            let distance = if requested < *min {
                min.saturating_sub(requested)
            } else {
                requested.saturating_sub(*max)
            };
            (1, distance)
        }
        SupportedBufferSize::Unknown => (2, 0),
    }
}

fn negotiated_buffer_frames(buffer_size: &SupportedBufferSize, requested: u32) -> u32 {
    match buffer_size {
        SupportedBufferSize::Range { min, max } => requested.clamp(*min, *max),
        SupportedBufferSize::Unknown => requested,
    }
}

fn build_output_stream(
    device: &cpal::Device,
    settings: &AudioSettings,
    source: Arc<AtomicU8>,
    playback: Arc<PlaybackState>,
    ring: Arc<AudioRingBuffer>,
    error: Arc<AudioErrorState>,
) -> Result<Stream, String> {
    let supported = select_config(device, false, settings.sample_rate, settings.buffer_frames)?;
    let format = supported.sample_format();
    let mut config = supported.config();
    config.buffer_size = cpal::BufferSize::Fixed(negotiated_buffer_frames(
        supported.buffer_size(),
        settings.buffer_frames,
    ));
    match build_typed_output(
        device,
        config,
        format,
        Arc::clone(&source),
        Arc::clone(&playback),
        Arc::clone(&ring),
        Arc::clone(&error),
    ) {
        Ok(stream) => Ok(stream),
        Err(first_error) => {
            let mut default_config = supported.config();
            default_config.buffer_size = cpal::BufferSize::Default;
            build_typed_output(
                device,
                default_config,
                format,
                source,
                playback,
                ring,
                error,
            )
            .map_err(|error| format!("{error} (requested buffer was also rejected: {first_error})"))
        }
    }
}

fn build_typed_output(
    device: &cpal::Device,
    config: StreamConfig,
    format: SampleFormat,
    source: Arc<AtomicU8>,
    playback: Arc<PlaybackState>,
    ring: Arc<AudioRingBuffer>,
    error: Arc<AudioErrorState>,
) -> Result<Stream, String> {
    let initial_gain = f32::from_bits(playback.output_gain.load(Ordering::Relaxed));
    macro_rules! build {
        ($sample:ty) => {
            build_tone_stream::<$sample>(
                device,
                config,
                source,
                playback,
                ring,
                error,
                initial_gain,
            )
        };
    }
    match format {
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I24 => build!(cpal::I24),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I64 => build!(i64),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U24 => build!(cpal::U24),
        SampleFormat::U32 => build!(u32),
        SampleFormat::U64 => build!(u64),
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        unsupported => Err(format!(
            "Unsupported audio output sample format: {unsupported}"
        )),
    }
}

fn build_tone_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    source: Arc<AtomicU8>,
    playback: Arc<PlaybackState>,
    ring: Arc<AudioRingBuffer>,
    error: Arc<AudioErrorState>,
    mut output_gain: f32,
) -> Result<Stream, String>
where
    T: cpal::SizedSample + FromSample<f32>,
{
    let sample_rate = config.sample_rate as f32;
    let gain_alpha = output_gain_smoothing_alpha(sample_rate);
    let channels = usize::from(config.channels).max(1);
    let mut phase = 0.0_f32;
    let playback_for_callback = Arc::clone(&playback);
    let stream = device
        .build_output_stream::<T, _, _>(
            config,
            move |output, info| {
                enable_denormal_protection();
                playback_for_callback
                    .output_latency_estimate
                    .observe_cpal_output_callback(info);
                playback_for_callback.apply_pending_commands(&source);
                if playback_for_callback
                    .input_monitor_ring_flush_requested
                    .swap(false, Ordering::AcqRel)
                {
                    ring.clear();
                }
                playback_for_callback.apply_pending_streaming_handoff();
                let playback_samples = playback_for_callback.samples.load();
                let streaming = playback_for_callback.streaming.load();
                let browser_preview_samples = playback_for_callback.browser_preview_samples.load();
                let target_gain =
                    f32::from_bits(playback_for_callback.output_gain.load(Ordering::Relaxed));
                let mut output_peak = 0.0_f32;
                let mut output_peak_left = 0.0_f32;
                let mut output_peak_right = 0.0_f32;
                for frame in output.chunks_mut(channels) {
                    output_gain = smooth_output_gain(output_gain, target_gain, gain_alpha);
                    let [left, right] = next_output_frame_with_browser_preview(AudioFrameInputs {
                        source: &source,
                        ring: &ring,
                        project_samples: playback_samples.as_deref().map(Vec::as_slice),
                        streaming: streaming.as_deref(),
                        playback: &playback_for_callback,
                        browser_preview_samples: browser_preview_samples
                            .as_deref()
                            .map(Vec::as_slice),
                        sample_rate,
                        phase: &mut phase,
                    });
                    for (channel, destination) in frame.iter_mut().enumerate() {
                        let raw_sample = match channel {
                            0 => left,
                            1 => right,
                            _ => 0.0,
                        } * output_gain;
                        let sample = sanitize_output_sample(raw_sample);
                        output_peak = output_peak.max(sample.abs());
                        let meter_peak = if raw_sample.is_finite() {
                            raw_sample.abs()
                        } else {
                            0.0
                        };
                        match channel {
                            0 => output_peak_left = output_peak_left.max(meter_peak),
                            1 => output_peak_right = output_peak_right.max(meter_peak),
                            _ => {}
                        }
                        *destination = T::from_sample(sample);
                    }
                }
                playback_for_callback
                    .output_peak
                    .store(output_peak.to_bits(), Ordering::Release);
                playback_for_callback
                    .output_peak_left
                    .store(output_peak_left.to_bits(), Ordering::Release);
                playback_for_callback
                    .output_peak_right
                    .store(output_peak_right.to_bits(), Ordering::Release);
                playback_for_callback.output_callback_frames.store(
                    u32::try_from(output.len() / channels).unwrap_or(u32::MAX),
                    Ordering::Release,
                );
            },
            move |stream_error| set_error(&error, stream_error.to_string()),
            Some(Duration::from_secs(5)),
        )
        .map_err(|error| format!("Could not build output stream: {error}"))?;
    Ok(stream)
}

#[derive(Clone)]
struct InputStreamState {
    ring: Arc<AudioRingBuffer>,
    peak: Arc<AtomicU32>,
    callback_frames: Arc<AtomicU32>,
    source: Arc<AtomicU8>,
    recording: Arc<ArcSwapOption<InputRecordingState>>,
    monitor_recording: Arc<ArcSwapOption<InputRecordingState>>,
    monitor_active: Arc<AtomicBool>,
    input_latency_estimate: Arc<AudioLatencyEstimate>,
    error: Arc<AudioErrorState>,
    input_channel_pair: Arc<AtomicU16>,
}

fn build_input_stream(
    device: &cpal::Device,
    settings: &AudioSettings,
    state: InputStreamState,
) -> Result<(Stream, u16), String> {
    let supported = select_config(device, true, settings.sample_rate, settings.buffer_frames)?;
    let format = supported.sample_format();
    let mut config = supported.config();
    let channel_count = config.channels;
    config.buffer_size = cpal::BufferSize::Fixed(negotiated_buffer_frames(
        supported.buffer_size(),
        settings.buffer_frames,
    ));
    match build_typed_input(device, config, format, state.clone()) {
        Ok(stream) => Ok((stream, channel_count)),
        Err(first_error) => {
            let mut default_config = supported.config();
            default_config.buffer_size = cpal::BufferSize::Default;
            build_typed_input(device, default_config, format, state)
                .map(|stream| (stream, channel_count))
                .map_err(|error| {
                    format!("{error} (requested buffer was also rejected: {first_error})")
                })
        }
    }
}

fn build_typed_input(
    device: &cpal::Device,
    config: StreamConfig,
    format: SampleFormat,
    state: InputStreamState,
) -> Result<Stream, String> {
    macro_rules! build {
        ($sample:ty) => {
            build_capture_stream::<$sample>(device, config, state)
        };
    }
    match format {
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I24 => build!(cpal::I24),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I64 => build!(i64),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U24 => build!(cpal::U24),
        SampleFormat::U32 => build!(u32),
        SampleFormat::U64 => build!(u64),
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        unsupported => Err(format!(
            "Unsupported audio input sample format: {unsupported}"
        )),
    }
}

fn build_capture_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    state: InputStreamState,
) -> Result<Stream, String>
where
    T: cpal::SizedSample + Copy,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels).max(1);
    let InputStreamState {
        ring,
        peak,
        callback_frames,
        source,
        recording: input_recording,
        monitor_recording: input_monitor_recording,
        monitor_active,
        input_latency_estimate,
        error,
        input_channel_pair,
    } = state;
    let (fallback_left_channel, fallback_right_channel) = selected_stereo_input_channels(
        usize::from(config.channels),
        input_channel_pair.load(Ordering::Acquire),
    )?;
    let stream = device
        .build_input_stream::<T, _, _>(
            config,
            move |input, info| {
                enable_denormal_protection();
                input_latency_estimate.observe_cpal_input_callback(info);
                let mut max_peak = 0.0_f32;
                callback_frames.store(
                    u32::try_from(input.len() / channels).unwrap_or(u32::MAX),
                    Ordering::Release,
                );
                let recording = input_recording.load();
                let recording_callback = recording
                    .as_deref()
                    .and_then(InputRecordingState::begin_callback);
                let monitor_recording = input_monitor_recording.load();
                let monitor_queue_active = monitor_recording.is_some();
                let monitor_callback = monitor_recording
                    .as_deref()
                    .and_then(InputRecordingState::begin_callback);
                let (left_channel, right_channel) = selected_stereo_input_channels(
                    channels,
                    input_channel_pair.load(Ordering::Acquire),
                )
                .unwrap_or((fallback_left_channel, fallback_right_channel));
                for frame in input.chunks(channels) {
                    if frame.is_empty() {
                        continue;
                    }
                    let left = frame
                        .get(left_channel)
                        .copied()
                        .map(f32::from_sample)
                        .unwrap_or_default()
                        .clamp(-1.0, 1.0);
                    let right = frame
                        .get(right_channel)
                        .copied()
                        .map(f32::from_sample)
                        .unwrap_or(left)
                        .clamp(-1.0, 1.0);
                    max_peak = max_peak.max(left.abs()).max(right.abs());
                    let stereo = [left, right];
                    if (source.load(Ordering::Acquire) == AUDIO_SOURCE_INPUT_MONITOR
                        || monitor_active.load(Ordering::Acquire))
                        && !monitor_queue_active
                    {
                        ring.push_stereo_frame(stereo[0], stereo[1]);
                    }
                    if let Some(recording) = recording_callback.as_ref() {
                        recording.push_frame(stereo[0], stereo[1]);
                    }
                    if let Some(recording) = monitor_callback.as_ref() {
                        recording.push_frame(stereo[0], stereo[1]);
                    }
                }
                peak.store(max_peak.to_bits(), Ordering::Release);
            },
            move |stream_error| set_error(&error, stream_error.to_string()),
            Some(Duration::from_secs(5)),
        )
        .map_err(|error| format!("Could not build input stream: {error}"))?;
    Ok(stream)
}

fn selected_stereo_input_channels(
    channel_count: usize,
    input_channel_pair: u16,
) -> Result<(usize, usize), String> {
    if channel_count == 1 && input_channel_pair == 0 {
        return Ok((0, 0));
    }
    let left_channel = usize::from(input_channel_pair)
        .checked_mul(2)
        .ok_or_else(|| "Selected input channel pair is out of range".to_owned())?;
    let right_channel = left_channel
        .checked_add(1)
        .ok_or_else(|| "Selected input channel pair is out of range".to_owned())?;
    if right_channel >= channel_count {
        return Err(format!(
            "Selected input channel pair {} / {} is unavailable on a {channel_count}-channel device",
            left_channel + 1,
            right_channel + 1
        ));
    }
    Ok((left_channel, right_channel))
}

fn next_output_frame(
    source: &AtomicU8,
    ring: &AudioRingBuffer,
    project_samples: Option<&[f32]>,
    streaming: Option<&StreamingPlayback>,
    playback: &PlaybackState,
    sample_rate: f32,
    phase: &mut f32,
) -> [f32; 2] {
    let output_source = source.load(Ordering::Acquire);
    let mut output = (|| match output_source {
        AUDIO_SOURCE_TEST_TONE => {
            let (value, next_phase) = tone_sample(*phase, sample_rate);
            *phase = next_phase;
            [value, value]
        }
        AUDIO_SOURCE_INPUT_MONITOR => [0.0, 0.0],
        AUDIO_SOURCE_PROJECT => {
            if !playback.active.load(Ordering::Acquire) {
                return [0.0, 0.0];
            }
            let frame = playback.cursor_frames.load(Ordering::Relaxed);
            let Some(frame_offset) = usize::try_from(frame)
                .ok()
                .and_then(|frame| frame.checked_mul(2))
            else {
                playback.active.store(false, Ordering::Release);
                return [0.0, 0.0];
            };
            let Some(samples) = project_samples else {
                playback.active.store(false, Ordering::Release);
                return [0.0, 0.0];
            };
            let frame_count = (samples.len() / 2) as u64;
            if frame >= frame_count {
                playback.active.store(false, Ordering::Release);
                return [0.0, 0.0];
            }
            let Some(current) = samples.get(frame_offset..frame_offset.saturating_add(2)) else {
                playback.active.store(false, Ordering::Release);
                return [0.0, 0.0];
            };
            let next_offset = usize::try_from((frame + 1).min(frame_count - 1))
                .ok()
                .and_then(|frame| frame.checked_mul(2))
                .unwrap_or(frame_offset);
            let next = samples
                .get(next_offset..next_offset.saturating_add(2))
                .unwrap_or(current);
            let phase = f32::from_bits(playback.playback_speed_phase.load(Ordering::Relaxed));
            let sample = [
                current[0] + (next[0] - current[0]) * phase,
                current[1] + (next[1] - current[1]) * phase,
            ];
            let speed = f32::from_bits(playback.playback_speed.load(Ordering::Acquire));
            let total_phase = phase + speed;
            let advance_frames = total_phase.floor() as u64;
            playback.playback_speed_phase.store(
                (total_phase - advance_frames as f32).to_bits(),
                Ordering::Relaxed,
            );
            playback.cursor_frames.store(
                frame.saturating_add(advance_frames).min(frame_count),
                Ordering::Relaxed,
            );
            sample
        }
        AUDIO_SOURCE_STREAM => {
            let Some(streaming) = streaming else {
                return [0.0, 0.0];
            };
            if streaming.cancelled.load(Ordering::Acquire)
                || streaming.paused.load(Ordering::Acquire)
            {
                return [0.0, 0.0];
            }
            if !streaming.active.load(Ordering::Acquire) {
                if !streaming.is_ready() {
                    return [0.0, 0.0];
                }
                streaming.active.store(true, Ordering::Release);
            }
            let project_source_advances = streaming.project_source_index.is_none_or(|index| {
                !streaming.project_source_finished.load(Ordering::Acquire)
                    || streaming.rings[index].available_stereo_frames() > 0
            });
            let phase = f32::from_bits(playback.playback_speed_phase.load(Ordering::Relaxed));
            let speed = f32::from_bits(playback.playback_speed.load(Ordering::Acquire));
            let total_phase = phase + speed;
            let requested_advance = total_phase.floor() as usize;
            let next_phase = total_phase - requested_advance as f32;
            let mut mixed_frame = [0.0, 0.0];
            let mut source_underrun = false;
            let mut any_source_frame = false;
            let mut max_available_frames = 0;
            let mut project_available_frames = 0;
            for (index, ring) in streaming.rings.iter().enumerate() {
                let available_frames = ring.available_stereo_frames();
                max_available_frames = max_available_frames.max(available_frames);
                if Some(index) == streaming.project_source_index {
                    project_available_frames = available_frames;
                }
                let frame = ring.peek_stereo_frame(0);
                if let Some(frame) = frame {
                    any_source_frame = true;
                    let next = ring.peek_stereo_frame(1).unwrap_or(frame);
                    mixed_frame[0] += frame[0] + (next[0] - frame[0]) * phase;
                    mixed_frame[1] += frame[1] + (next[1] - frame[1]) * phase;
                }
                let producer_finished = streaming.producer_finished[index].load(Ordering::Acquire);
                let needs_lookahead = phase > 0.0 && available_frames < 2;
                if !producer_finished
                    && (frame.is_none() || available_frames < requested_advance || needs_lookahead)
                {
                    streaming.producer_underrun_frames[index].fetch_add(1, Ordering::Relaxed);
                    source_underrun = true;
                }
            }
            if streaming.finished.load(Ordering::Acquire)
                && streaming.available_stereo_frames() == 0
                && !any_source_frame
            {
                streaming.active.store(false, Ordering::Release);
                [0.0, 0.0]
            } else {
                for ring in &streaming.rings {
                    ring.advance_stereo_frames(requested_advance);
                }
                playback
                    .playback_speed_phase
                    .store(next_phase.to_bits(), Ordering::Relaxed);
                if source_underrun {
                    streaming.underrun_frames.fetch_add(1, Ordering::Relaxed);
                }
                if project_source_advances {
                    let position_advance = if streaming
                        .project_source_index
                        .is_some_and(|_| streaming.project_source_finished.load(Ordering::Acquire))
                    {
                        requested_advance.min(project_available_frames)
                    } else if streaming.project_source_index.is_none()
                        && streaming.finished.load(Ordering::Acquire)
                    {
                        requested_advance.min(max_available_frames)
                    } else {
                        requested_advance
                    };
                    streaming
                        .consumed_frames
                        .fetch_add(position_advance as u64, Ordering::Relaxed);
                }
                mixed_frame
            }
        }
        _ => [0.0, 0.0],
    })();
    if playback.input_monitor_active.load(Ordering::Acquire)
        || output_source == AUDIO_SOURCE_INPUT_MONITOR
    {
        let monitored = ring.pop_stereo_frame().unwrap_or([0.0, 0.0]);
        output[0] += monitored[0];
        output[1] += monitored[1];
    }
    output
}

fn valid_project_playback_speed(speed_multiplier: f32) -> bool {
    speed_multiplier.is_finite() && (0.25..=4.0).contains(&speed_multiplier)
}

struct AudioFrameInputs<'a> {
    source: &'a AtomicU8,
    ring: &'a AudioRingBuffer,
    project_samples: Option<&'a [f32]>,
    streaming: Option<&'a StreamingPlayback>,
    playback: &'a PlaybackState,
    browser_preview_samples: Option<&'a [f32]>,
    sample_rate: f32,
    phase: &'a mut f32,
}

fn next_output_frame_with_browser_preview(inputs: AudioFrameInputs<'_>) -> [f32; 2] {
    let AudioFrameInputs {
        source,
        ring,
        project_samples,
        streaming,
        playback,
        browser_preview_samples,
        sample_rate,
        phase,
    } = inputs;
    let project = next_output_frame(
        source,
        ring,
        project_samples,
        streaming,
        playback,
        sample_rate,
        phase,
    );
    let preview = playback.next_browser_preview_frame(browser_preview_samples);
    [
        (project[0] + preview[0]).clamp(-1.0, 1.0),
        (project[1] + preview[1]).clamp(-1.0, 1.0),
    ]
}

pub(crate) fn output_gain_smoothing_alpha(sample_rate: f32) -> f32 {
    if !sample_rate.is_finite() || sample_rate <= 0.0 {
        return 1.0;
    }
    1.0 - (-1.0 / (sample_rate * OUTPUT_GAIN_SMOOTHING_SECONDS)).exp()
}

pub(crate) fn smooth_output_gain(current: f32, target: f32, alpha: f32) -> f32 {
    current + (target - current) * alpha.clamp(0.0, 1.0)
}

fn sanitize_output_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn tone_sample(phase: f32, sample_rate: f32) -> (f32, f32) {
    let value = (phase * std::f32::consts::TAU).sin() * TEST_TONE_LEVEL;
    let next_phase = (phase + TEST_TONE_HZ / sample_rate.max(1.0)).fract();
    (value, next_phase)
}

fn set_error(error: &AudioErrorState, message: String) {
    error.report(message);
}

struct AudioRingBuffer {
    samples: Box<[AtomicU32]>,
    write_index: AtomicU64,
    read_index: AtomicU64,
}

impl AudioRingBuffer {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(2);
        let samples = (0..capacity)
            .map(|_| AtomicU32::new(0.0_f32.to_bits()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            samples,
            write_index: AtomicU64::new(0),
            read_index: AtomicU64::new(0),
        }
    }

    fn push_stereo_frame(&self, left: f32, right: f32) -> bool {
        let write = self.write_index.load(Ordering::Relaxed);
        let read = self.read_index.load(Ordering::Acquire);
        if write.wrapping_sub(read).saturating_add(2) > self.samples.len() as u64 {
            return false;
        }
        let capacity = self.samples.len();
        self.samples[write as usize % capacity].store(left.to_bits(), Ordering::Relaxed);
        self.samples[(write as usize + 1) % capacity].store(right.to_bits(), Ordering::Relaxed);
        self.write_index
            .store(write.wrapping_add(2), Ordering::Release);
        true
    }

    fn pop_stereo_frame(&self) -> Option<[f32; 2]> {
        let read = self.read_index.load(Ordering::Relaxed);
        let write = self.write_index.load(Ordering::Acquire);
        if write.wrapping_sub(read) < 2 {
            return None;
        }
        let capacity = self.samples.len();
        let frame = [
            f32::from_bits(self.samples[read as usize % capacity].load(Ordering::Relaxed)),
            f32::from_bits(self.samples[(read as usize + 1) % capacity].load(Ordering::Relaxed)),
        ];
        self.read_index
            .store(read.wrapping_add(2), Ordering::Release);
        Some(frame)
    }

    fn clear(&self) {
        let write = self.write_index.load(Ordering::Acquire);
        self.read_index.store(write, Ordering::Release);
    }

    fn peek_stereo_frame(&self, frame_offset: usize) -> Option<[f32; 2]> {
        let read = self.read_index.load(Ordering::Relaxed);
        let write = self.write_index.load(Ordering::Acquire);
        let frame_offset = u64::try_from(frame_offset).ok()?;
        let required_samples = frame_offset.checked_add(1)?.checked_mul(2)?;
        if write.wrapping_sub(read) < required_samples {
            return None;
        }
        let sample_offset = read.wrapping_add(frame_offset.wrapping_mul(2)) as usize;
        let capacity = self.samples.len();
        Some([
            f32::from_bits(self.samples[sample_offset % capacity].load(Ordering::Relaxed)),
            f32::from_bits(self.samples[(sample_offset + 1) % capacity].load(Ordering::Relaxed)),
        ])
    }

    fn advance_stereo_frames(&self, frames: usize) -> usize {
        let read = self.read_index.load(Ordering::Relaxed);
        let write = self.write_index.load(Ordering::Acquire);
        let available_frames = (write.wrapping_sub(read) / 2).min(usize::MAX as u64) as usize;
        let consumed_frames = frames.min(available_frames);
        self.read_index.store(
            read.wrapping_add((consumed_frames as u64).wrapping_mul(2)),
            Ordering::Release,
        );
        consumed_frames
    }

    fn available_stereo_frames(&self) -> usize {
        let write = self.write_index.load(Ordering::Acquire);
        let read = self.read_index.load(Ordering::Acquire);
        (write.wrapping_sub(read) / 2).min(usize::MAX as u64) as usize
    }
}

#[cfg(windows)]
fn wasapi_device_id(cpal_id: &str) -> Result<&str, String> {
    cpal_id
        .strip_prefix("wasapi:")
        .ok_or_else(|| format!("Selected endpoint is not a WASAPI device: {cpal_id}"))
}

#[cfg(windows)]
fn set_wasapi_error(error: &AudioErrorState, message: String) {
    set_error(error, message);
}

#[cfg(windows)]
fn query_performance_frequency() -> Option<i64> {
    let mut frequency = 0_i64;
    unsafe { QueryPerformanceFrequency(&mut frequency).ok()? };
    (frequency > 0).then_some(frequency)
}

#[cfg(windows)]
fn query_performance_time_100ns(frequency: Option<i64>) -> Option<u64> {
    let mut counter = 0_i64;
    unsafe { QueryPerformanceCounter(&mut counter).ok()? };
    let counter = u128::try_from(counter).ok()?;
    let frequency = u128::try_from(frequency?).ok()?;
    let time_100ns = counter.checked_mul(10_000_000)?.checked_div(frequency)?;
    u64::try_from(time_100ns).ok()
}

#[cfg(windows)]
fn wasapi_render_block_latency(
    callback_time_100ns: u64,
    device_position: u64,
    device_position_time_100ns: u64,
    clock_frequency: u64,
    submitted_frames: u64,
    sample_rate: u32,
) -> Option<Duration> {
    let clock_frequency = u128::from(clock_frequency);
    let sample_rate = u128::from(sample_rate);
    if clock_frequency == 0 || sample_rate == 0 {
        return None;
    }

    // `submitted_frames` marks the first frame of the next block. The clock
    // reports the currently presented stream position and its QPC timestamp.
    let next_frame_position = u128::from(submitted_frames)
        .checked_mul(clock_frequency)?
        .checked_div(sample_rate)?;
    let frames_until_playback = next_frame_position.checked_sub(u128::from(device_position))?;
    let playback_offset_100ns = frames_until_playback
        .checked_mul(10_000_000)?
        .checked_div(clock_frequency)?;
    let playback_time_100ns =
        u128::from(device_position_time_100ns).checked_add(playback_offset_100ns)?;
    let latency_100ns = playback_time_100ns.checked_sub(u128::from(callback_time_100ns))?;
    let latency_nanoseconds = latency_100ns.checked_mul(100)?;
    Some(Duration::from_nanos(
        u64::try_from(latency_nanoseconds).ok()?,
    ))
}

#[cfg(windows)]
struct WasapiWorkerState {
    source: Arc<AtomicU8>,
    playback: Arc<PlaybackState>,
    ring: Arc<AudioRingBuffer>,
    input_peak: Arc<AtomicU32>,
    input_callback_frames: Arc<AtomicU32>,
    input_recording: Arc<ArcSwapOption<InputRecordingState>>,
    input_monitor_recording: Arc<ArcSwapOption<InputRecordingState>>,
    input_latency_estimate: Arc<AudioLatencyEstimate>,
    input_channel_pair: Arc<AtomicU16>,
    input_channel_count: Arc<AtomicU16>,
    error: Arc<AudioErrorState>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(windows)]
fn wasapi_exclusive_output_worker(
    selected_id: Option<&str>,
    settings: &AudioSettings,
    state: WasapiWorkerState,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    enable_denormal_protection();
    let WasapiWorkerState {
        source,
        playback,
        ring,
        error,
        stop,
        ..
    } = state;
    let mut started = false;
    let result = (|| -> Result<(), String> {
        wasapi::initialize_mta()
            .ok()
            .map_err(|error| format!("Could not initialize Windows audio COM: {error}"))?;
        let _com = WasapiComGuard;
        let enumerator = wasapi::DeviceEnumerator::new().map_err(|error| error.to_string())?;
        let device = if let Some(id) = selected_id {
            enumerator
                .get_device(wasapi_device_id(id)?)
                .map_err(|error| error.to_string())?
        } else {
            enumerator
                .get_default_device(&wasapi::Direction::Render)
                .map_err(|error| error.to_string())?
        };
        let mut client = device
            .get_iaudioclient()
            .map_err(|error| error.to_string())?;
        let format = select_exclusive_format(&mut client, &device, settings.sample_rate)?;
        let format_spec = WasapiSampleSpec::from_format(&format)?;
        let period = exclusive_period(&client, &format, settings.buffer_frames)?;
        client
            .initialize_client(
                &format,
                &wasapi::Direction::Render,
                &wasapi::StreamMode::EventsExclusive { period_hns: period },
            )
            .map_err(|error| format_exclusive_error("output", error.to_string()))?;
        let event = client
            .set_get_eventhandle()
            .map_err(|error| error.to_string())?;
        let render = client
            .get_audiorenderclient()
            .map_err(|error| error.to_string())?;
        let output_clock = client.get_audioclock().ok();
        let output_clock_frequency = output_clock
            .as_ref()
            .and_then(|clock| clock.get_frequency().ok());
        let qpc_frequency = query_performance_frequency();
        let mut render_state = WasapiRenderState {
            phase: 0.0,
            output_gain: settings.output_gain,
        };
        let initial_frames = client
            .get_available_space_in_frames()
            .map_err(|error| error.to_string())? as usize;
        let mut submitted_frames = u64::try_from(initial_frames).unwrap_or(u64::MAX);
        // Reuse this storage for each later event instead of allocating a
        // fresh byte buffer on every wake-up.
        let mut output = Vec::with_capacity(initial_frames.saturating_mul(format_spec.frame_bytes));
        render_wasapi_buffer(
            initial_frames,
            &format_spec,
            &source,
            &playback,
            &ring,
            &mut render_state,
            &mut output,
        )?;
        render
            .write_to_device(initial_frames, &output, None)
            .map_err(|error| error.to_string())?;
        client
            .start_stream()
            .map_err(|error| format_exclusive_error("output", error.to_string()))?;
        started = true;
        let _ = ready.send(Ok(()));

        while !stop.load(Ordering::Acquire) {
            match event.wait_for_event(100) {
                Ok(()) => {
                    let callback_time_100ns = query_performance_time_100ns(qpc_frequency);
                    let frames = client
                        .get_available_space_in_frames()
                        .map_err(|error| error.to_string())?
                        as usize;
                    render_wasapi_buffer(
                        frames,
                        &format_spec,
                        &source,
                        &playback,
                        &ring,
                        &mut render_state,
                        &mut output,
                    )?;
                    render
                        .write_to_device(frames, &output, None)
                        .map_err(|error| error.to_string())?;
                    if frames > 0
                        && let (Some(callback_time_100ns), Some(clock), Some(clock_frequency)) = (
                            callback_time_100ns,
                            output_clock.as_ref(),
                            output_clock_frequency,
                        )
                        && let Ok((device_position, device_position_time_100ns)) =
                            clock.get_position()
                        && let Some(latency) = wasapi_render_block_latency(
                            callback_time_100ns,
                            device_position,
                            device_position_time_100ns,
                            clock_frequency,
                            submitted_frames,
                            format_spec.sample_rate,
                        )
                    {
                        playback.output_latency_estimate.observe(Some(latency));
                    }
                    submitted_frames =
                        submitted_frames.saturating_add(u64::try_from(frames).unwrap_or(u64::MAX));
                }
                Err(wasapi::WasapiError::EventTimeout) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        client.stop_stream().map_err(|error| error.to_string())?;
        Ok(())
    })();
    if let Err(message) = result {
        set_wasapi_error(
            &error,
            format!("WASAPI exclusive output stopped: {message}"),
        );
        if !started {
            let _ = ready.send(Err(message));
        }
    }
}

#[cfg(windows)]
fn wasapi_exclusive_input_worker(
    selected_id: Option<&str>,
    settings: &AudioSettings,
    state: WasapiWorkerState,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    enable_denormal_protection();
    let WasapiWorkerState {
        ring,
        playback,
        input_peak: peak,
        input_callback_frames,
        input_recording,
        input_monitor_recording,
        input_latency_estimate,
        input_channel_pair,
        input_channel_count,
        source,
        error,
        stop,
        ..
    } = state;
    let mut started = false;
    let result = (|| -> Result<(), String> {
        wasapi::initialize_mta()
            .ok()
            .map_err(|error| format!("Could not initialize Windows audio COM: {error}"))?;
        let _com = WasapiComGuard;
        let enumerator = wasapi::DeviceEnumerator::new().map_err(|error| error.to_string())?;
        let device = if let Some(id) = selected_id {
            enumerator
                .get_device(wasapi_device_id(id)?)
                .map_err(|error| error.to_string())?
        } else {
            enumerator
                .get_default_device(&wasapi::Direction::Capture)
                .map_err(|error| error.to_string())?
        };
        let mut client = device
            .get_iaudioclient()
            .map_err(|error| error.to_string())?;
        let format = select_exclusive_format(&mut client, &device, settings.sample_rate)?;
        let format_spec = WasapiSampleSpec::from_format(&format)?;
        input_channel_count.store(
            u16::try_from(format_spec.channels)
                .map_err(|_| "WASAPI input channel count is out of range".to_owned())?,
            Ordering::Release,
        );
        let (fallback_left_channel, fallback_right_channel) = selected_stereo_input_channels(
            format_spec.channels,
            input_channel_pair.load(Ordering::Acquire),
        )?;
        let period = exclusive_period(&client, &format, settings.buffer_frames)?;
        client
            .initialize_client(
                &format,
                &wasapi::Direction::Capture,
                &wasapi::StreamMode::EventsExclusive { period_hns: period },
            )
            .map_err(|error| format_exclusive_error("input", error.to_string()))?;
        let event = client
            .set_get_eventhandle()
            .map_err(|error| error.to_string())?;
        let buffer_frames = client
            .get_buffer_size()
            .map_err(|error| error.to_string())? as usize;
        let capture = client
            .get_audiocaptureclient()
            .map_err(|error| error.to_string())?;
        let mut bytes = vec![0_u8; buffer_frames.saturating_mul(format_spec.frame_bytes)];
        client
            .start_stream()
            .map_err(|error| format_exclusive_error("input", error.to_string()))?;
        let qpc_frequency = query_performance_frequency();
        started = true;
        let _ = ready.send(Ok(()));

        while !stop.load(Ordering::Acquire) {
            match event.wait_for_event(100) {
                Ok(()) => {
                    let capture_event_time = query_performance_time_100ns(qpc_frequency);
                    let (frames, buffer_info) = capture
                        .read_from_device(&mut bytes)
                        .map_err(|error| error.to_string())?;
                    if frames > 0
                        && !buffer_info.flags.timestamp_error
                        && let Some(latency_100ns) = capture_event_time
                            .filter(|capture_time| *capture_time >= buffer_info.timestamp)
                            .map(|capture_time| capture_time - buffer_info.timestamp)
                    {
                        input_latency_estimate.observe(Some(Duration::from_nanos(
                            latency_100ns.saturating_mul(100),
                        )));
                    }
                    input_callback_frames.store(frames, Ordering::Release);
                    let active_bytes = (frames as usize).saturating_mul(format_spec.frame_bytes);
                    let mut max_peak = 0.0_f32;
                    let recording = input_recording.load();
                    let recording_callback = recording
                        .as_deref()
                        .and_then(InputRecordingState::begin_callback);
                    let monitor_recording = input_monitor_recording.load();
                    let monitor_queue_active = monitor_recording.is_some();
                    let monitor_callback = monitor_recording
                        .as_deref()
                        .and_then(InputRecordingState::begin_callback);
                    let (left_channel, right_channel) = selected_stereo_input_channels(
                        format_spec.channels,
                        input_channel_pair.load(Ordering::Acquire),
                    )
                    .unwrap_or((fallback_left_channel, fallback_right_channel));
                    for frame in bytes[..active_bytes].chunks_exact(format_spec.frame_bytes) {
                        let mut stereo = [0.0_f32; 2];
                        for (channel, sample_bytes) in
                            frame.chunks_exact(format_spec.sample_bytes).enumerate()
                        {
                            let sample = format_spec.decode(sample_bytes)?;
                            if channel == left_channel {
                                stereo[0] = sample;
                            }
                            if channel == right_channel {
                                stereo[1] = sample;
                            }
                        }
                        max_peak = max_peak.max(stereo[0].abs()).max(stereo[1].abs());
                        if (source.load(Ordering::Acquire) == AUDIO_SOURCE_INPUT_MONITOR
                            || playback.input_monitor_active.load(Ordering::Acquire))
                            && !monitor_queue_active
                        {
                            ring.push_stereo_frame(stereo[0], stereo[1]);
                        }
                        if let Some(recording) = recording_callback.as_ref() {
                            recording.push_frame(stereo[0], stereo[1]);
                        }
                        if let Some(recording) = monitor_callback.as_ref() {
                            recording.push_frame(stereo[0], stereo[1]);
                        }
                    }
                    peak.store(max_peak.clamp(0.0, 1.0).to_bits(), Ordering::Release);
                }
                Err(wasapi::WasapiError::EventTimeout) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        client.stop_stream().map_err(|error| error.to_string())?;
        Ok(())
    })();
    if let Err(message) = result {
        set_wasapi_error(&error, format!("WASAPI exclusive input stopped: {message}"));
        if !started {
            let _ = ready.send(Err(message));
        }
    }
}

#[cfg(windows)]
struct WasapiComGuard;

#[cfg(windows)]
impl Drop for WasapiComGuard {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

#[cfg(windows)]
fn select_exclusive_format(
    client: &mut wasapi::AudioClient,
    device: &wasapi::Device,
    sample_rate: u32,
) -> Result<wasapi::WaveFormat, String> {
    let device_format = device
        .get_device_format()
        .map_err(|error| format!("Could not read the device format: {error}"))?;
    let channels = usize::from(device_format.get_nchannels().max(1));
    let candidates = [
        (32, 32, wasapi::SampleType::Float),
        (32, 32, wasapi::SampleType::Int),
        (24, 24, wasapi::SampleType::Int),
        (16, 16, wasapi::SampleType::Int),
    ];
    for (storage_bits, valid_bits, sample_type) in candidates {
        let requested = wasapi::WaveFormat::new(
            storage_bits,
            valid_bits,
            &sample_type,
            sample_rate as usize,
            channels,
            None,
        );
        if let Ok(format) = client.is_supported_exclusive_with_quirks(&requested) {
            return Ok(format);
        }
    }
    Err(format!(
        "The device does not support an exclusive 32-bit float, 32/24/16-bit PCM stream at {sample_rate} Hz"
    ))
}

#[cfg(windows)]
fn exclusive_period(
    client: &wasapi::AudioClient,
    format: &wasapi::WaveFormat,
    buffer_frames: u32,
) -> Result<i64, String> {
    let requested_hns =
        (i64::from(buffer_frames) * 10_000_000) / i64::from(format.get_samplespersec().max(1));
    client
        .calculate_aligned_period_near(requested_hns, None, format)
        .map_err(|error| format!("Could not choose an exclusive buffer period: {error}"))
}

#[cfg(windows)]
fn format_exclusive_error(direction: &str, message: String) -> String {
    let lower = message.to_lowercase();
    if lower.contains("device in use") || lower.contains("already in use") {
        format!("The selected {direction} device is already in use by another application")
    } else if lower.contains("unsupported format") || lower.contains("unsupportedformat") {
        format!("The selected {direction} device does not support the requested exclusive format")
    } else if lower.contains("exclusive mode is not allowed") {
        format!("Windows does not allow exclusive access to the selected {direction} device")
    } else {
        message
    }
}

#[cfg(windows)]
struct WasapiSampleSpec {
    sample_type: wasapi::SampleType,
    storage_bits: usize,
    valid_bits: usize,
    channels: usize,
    sample_bytes: usize,
    frame_bytes: usize,
    sample_rate: u32,
}

#[cfg(windows)]
impl WasapiSampleSpec {
    fn from_format(format: &wasapi::WaveFormat) -> Result<Self, String> {
        let storage_bits = usize::from(format.get_bitspersample());
        let valid_bits = usize::from(format.get_validbitspersample());
        let channels = usize::from(format.get_nchannels());
        if storage_bits == 0
            || storage_bits % 8 != 0
            || valid_bits == 0
            || valid_bits > storage_bits
            || channels == 0
        {
            return Err("WASAPI returned an unsupported sample layout".into());
        }
        let sample_type = format
            .get_subformat()
            .map_err(|error| format!("Unsupported WASAPI sample type: {error}"))?;
        let sample_bytes = storage_bits / 8;
        let frame_bytes = usize::try_from(format.get_blockalign())
            .map_err(|_| "Invalid WASAPI frame size".to_owned())?;
        if frame_bytes != sample_bytes.saturating_mul(channels) {
            return Err("WASAPI sample layout has an unexpected frame size".into());
        }
        Ok(Self {
            sample_type,
            storage_bits,
            valid_bits,
            channels,
            sample_bytes,
            frame_bytes,
            sample_rate: format.get_samplespersec(),
        })
    }

    fn encode(&self, sample: f32, output: &mut [u8]) -> Result<(), String> {
        if output.len() != self.sample_bytes {
            return Err("Internal WASAPI sample buffer size mismatch".into());
        }
        let sample = sample.clamp(-1.0, 1.0);
        match self.sample_type {
            wasapi::SampleType::Float => match self.storage_bits {
                32 => output.copy_from_slice(&sample.to_le_bytes()),
                64 => output.copy_from_slice(&(f64::from(sample)).to_le_bytes()),
                bits => return Err(format!("Unsupported WASAPI float depth: {bits}")),
            },
            wasapi::SampleType::Int if self.storage_bits == 8 => {
                output[0] = ((sample + 1.0) * 127.5).round().clamp(0.0, 255.0) as u8;
            }
            wasapi::SampleType::Int => {
                let max_positive = (1_i128 << (self.valid_bits - 1)) - 1;
                let value = (f64::from(sample) * max_positive as f64).round() as i128;
                let aligned = value << (self.storage_bits - self.valid_bits);
                let raw = aligned.to_le_bytes();
                output.copy_from_slice(&raw[..self.sample_bytes]);
            }
        }
        Ok(())
    }

    fn decode(&self, input: &[u8]) -> Result<f32, String> {
        if input.len() != self.sample_bytes {
            return Err("Internal WASAPI input buffer size mismatch".into());
        }
        let value = match self.sample_type {
            wasapi::SampleType::Float => match self.storage_bits {
                32 => f32::from_le_bytes(input.try_into().expect("validated 32-bit sample")),
                64 => f64::from_le_bytes(input.try_into().expect("validated 64-bit sample")) as f32,
                bits => return Err(format!("Unsupported WASAPI float depth: {bits}")),
            },
            wasapi::SampleType::Int if self.storage_bits == 8 => {
                (i16::from(input[0]) - 128) as f32 / 128.0
            }
            wasapi::SampleType::Int => {
                let raw = input.iter().enumerate().fold(0_u128, |acc, (index, byte)| {
                    acc | (u128::from(*byte) << (index * 8))
                });
                let sign = 1_u128 << (self.storage_bits - 1);
                let signed = if raw & sign != 0 {
                    raw as i128 - (1_i128 << self.storage_bits)
                } else {
                    raw as i128
                };
                let aligned = signed >> (self.storage_bits - self.valid_bits);
                let denominator = (1_u128 << (self.valid_bits - 1)) as f64;
                (aligned as f64 / denominator) as f32
            }
        };
        Ok(if value.is_finite() {
            value.clamp(-1.0, 1.0)
        } else {
            0.0
        })
    }
}

#[cfg(windows)]
struct WasapiRenderState {
    phase: f32,
    output_gain: f32,
}

#[cfg(windows)]
fn render_wasapi_buffer(
    frames: usize,
    format: &WasapiSampleSpec,
    source: &AtomicU8,
    playback: &PlaybackState,
    ring: &AudioRingBuffer,
    render_state: &mut WasapiRenderState,
    output: &mut Vec<u8>,
) -> Result<(), String> {
    playback
        .output_callback_frames
        .store(u32::try_from(frames).unwrap_or(u32::MAX), Ordering::Release);
    playback.apply_pending_commands(source);
    if playback
        .input_monitor_ring_flush_requested
        .swap(false, Ordering::AcqRel)
    {
        ring.clear();
    }
    playback.apply_pending_streaming_handoff();
    output.resize(frames.saturating_mul(format.frame_bytes), 0);
    let sample_rate = format.get_sample_rate();
    let gain_alpha = output_gain_smoothing_alpha(sample_rate);
    let target_gain = f32::from_bits(playback.output_gain.load(Ordering::Relaxed));
    let playback_samples = playback.samples.load();
    let streaming = playback.streaming.load();
    let browser_preview_samples = playback.browser_preview_samples.load();
    let mut output_peak = 0.0_f32;
    let mut output_peak_left = 0.0_f32;
    let mut output_peak_right = 0.0_f32;
    for frame in output.chunks_exact_mut(format.frame_bytes) {
        render_state.output_gain =
            smooth_output_gain(render_state.output_gain, target_gain, gain_alpha);
        let stereo = next_output_frame_with_browser_preview(AudioFrameInputs {
            source,
            ring,
            project_samples: playback_samples.as_deref().map(Vec::as_slice),
            streaming: streaming.as_deref(),
            playback,
            browser_preview_samples: browser_preview_samples.as_deref().map(Vec::as_slice),
            sample_rate,
            phase: &mut render_state.phase,
        });
        for (channel_index, channel) in frame.chunks_exact_mut(format.sample_bytes).enumerate() {
            let raw_sample = match channel_index {
                0 => stereo[0],
                1 => stereo[1],
                _ => 0.0,
            } * render_state.output_gain;
            let sample = sanitize_output_sample(raw_sample);
            output_peak = output_peak.max(sample.abs());
            let meter_peak = if raw_sample.is_finite() {
                raw_sample.abs()
            } else {
                0.0
            };
            match channel_index {
                0 => output_peak_left = output_peak_left.max(meter_peak),
                1 => output_peak_right = output_peak_right.max(meter_peak),
                _ => {}
            }
            format.encode(sample, channel)?;
        }
    }
    playback
        .output_peak
        .store(output_peak.to_bits(), Ordering::Release);
    playback
        .output_peak_left
        .store(output_peak_left.to_bits(), Ordering::Release);
    playback
        .output_peak_right
        .store(output_peak_right.to_bits(), Ordering::Release);
    Ok(())
}

#[cfg(windows)]
impl WasapiSampleSpec {
    fn get_sample_rate(&self) -> f32 {
        self.sample_rate.max(1) as f32
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, Ordering};

    use cpal::SupportedBufferSize;

    use super::{
        AUDIO_COMMAND_QUEUE_CAPACITY, AUDIO_SOURCE_INPUT_MONITOR, AUDIO_SOURCE_PROJECT,
        AUDIO_SOURCE_SILENT, AUDIO_SOURCE_STREAM, AUDIO_SOURCE_TEST_TONE, AudioAccess,
        AudioDeviceCatalog, AudioDeviceConfigRange, AudioDeviceInfo, AudioInputRecording,
        AudioRingBuffer, AudioSettings, InputRecordingState, MixerTrackPeakMeters,
        ParameterCommand, PlaybackState, ProjectPlaybackClock, STREAM_PREFILL_FRAMES,
        StagedStreamingPlayback, StereoFrameDelay, TransportCommand, audio_device_config_ranges,
        buffer_config_preference, create_streaming_playback, enable_denormal_protection,
        mixer_effect_wet_mix, negotiated_buffer_frames, next_output_frame,
        output_gain_smoothing_alpha, sanitize_output_sample, smooth_output_gain, tone_sample,
        valid_project_playback_speed, validate_settings,
    };

    #[test]
    fn mixer_meter_ui_and_script_reads_keep_independent_stereo_peaks() {
        let meters = MixerTrackPeakMeters::default();

        meters.publish_stereo_peak(3, 0.25, 0.75);

        assert_eq!(meters.take_peak(3), 0.75);
        assert_eq!(meters.take_peak_channels(3), (0.25, 0.75));
        assert_eq!(meters.take_peak(3), 0.0);
        assert_eq!(meters.take_peak_channels(3), (0.0, 0.0));
    }

    #[test]
    fn live_mixer_effect_mix_targets_track_each_slot_independently() {
        let meters = MixerTrackPeakMeters::default();
        let first = meters
            .live_mixer_effect_mix_target(2, 3, mixer_effect_wet_mix(Some(-6_400)))
            .expect("first effect should receive a live target");
        let second = meters
            .live_mixer_effect_mix_target(2, 4, mixer_effect_wet_mix(Some(6_400)))
            .expect("second effect should receive a live target");
        assert_eq!(f32::from_bits(first.load(Ordering::Acquire)), 0.0);
        assert_eq!(f32::from_bits(second.load(Ordering::Acquire)), 1.0);

        meters.set_live_mixer_effect_mix_raw(2, 3, Some(0));
        assert_eq!(f32::from_bits(first.load(Ordering::Acquire)), 0.5);
        assert_eq!(f32::from_bits(second.load(Ordering::Acquire)), 1.0);
    }

    #[test]
    fn live_mixer_effect_enabled_targets_track_each_slot_independently() {
        let meters = MixerTrackPeakMeters::default();
        let first = meters
            .live_mixer_effect_slot_enabled_target(2, 3, true)
            .expect("first effect should receive a live enabled target");
        let second = meters
            .live_mixer_effect_slot_enabled_target(2, 4, false)
            .expect("second effect should receive a live enabled target");
        assert_eq!(first.load(Ordering::Acquire), 1);
        assert_eq!(second.load(Ordering::Acquire), 0);

        meters.set_live_mixer_effect_slot_enabled(2, 3, false);
        assert_eq!(first.load(Ordering::Acquire), 0);
        assert_eq!(second.load(Ordering::Acquire), 0);
    }

    #[test]
    fn live_mixer_insert_effects_enabled_targets_track_each_insert_independently() {
        let meters = MixerTrackPeakMeters::default();
        let first = meters
            .live_mixer_insert_effects_enabled_target(2, true)
            .expect("first insert should receive a live effects-enabled target");
        let second = meters
            .live_mixer_insert_effects_enabled_target(3, false)
            .expect("second insert should receive a live effects-enabled target");
        assert_eq!(first.load(Ordering::Acquire), 1);
        assert_eq!(second.load(Ordering::Acquire), 0);

        meters.set_live_mixer_insert_effects_enabled(2, false);
        assert_eq!(first.load(Ordering::Acquire), 0);
        assert_eq!(second.load(Ordering::Acquire), 0);
    }

    #[test]
    fn device_catalog_limits_sample_rates_and_buffers_to_common_capabilities() {
        let input_range = AudioDeviceConfigRange {
            minimum_sample_rate: 44_100,
            maximum_sample_rate: 96_000,
            channels: 2,
            minimum_buffer_frames: Some(128),
            maximum_buffer_frames: Some(512),
        };
        let output_range = AudioDeviceConfigRange {
            minimum_sample_rate: 32_000,
            maximum_sample_rate: 48_000,
            channels: 2,
            minimum_buffer_frames: Some(64),
            maximum_buffer_frames: Some(1_024),
        };
        let catalog = AudioDeviceCatalog {
            inputs: vec![AudioDeviceInfo {
                id: "input".to_owned(),
                name: "Input".to_owned(),
                exclusive_sample_rates: Vec::new(),
                exclusive_default_period_hns: None,
                exclusive_minimum_period_hns: None,
                config_ranges: vec![
                    input_range,
                    AudioDeviceConfigRange {
                        channels: 1,
                        minimum_buffer_frames: Some(32),
                        maximum_buffer_frames: Some(8_192),
                        ..input_range
                    },
                ],
            }],
            outputs: vec![AudioDeviceInfo {
                id: "output".to_owned(),
                name: "Output".to_owned(),
                exclusive_sample_rates: Vec::new(),
                exclusive_default_period_hns: None,
                exclusive_minimum_period_hns: None,
                config_ranges: vec![output_range],
            }],
            default_input_id: Some("input".to_owned()),
            default_output_id: Some("output".to_owned()),
            default_exclusive_input_id: None,
            default_exclusive_output_id: None,
            default_sample_rate: Some(48_000),
            error: None,
        };
        let settings = AudioSettings {
            sample_rate: 192_000,
            buffer_frames: 2_048,
            ..AudioSettings::default()
        };

        assert_eq!(catalog.supported_sample_rates(&settings), [44_100, 48_000]);
        assert_eq!(
            catalog.supported_buffer_sizes(&settings, 48_000),
            [128, 256, 512]
        );
        assert!(catalog.supported_buffer_sizes(&settings, 96_000).is_empty());
    }

    #[test]
    fn selected_device_change_reports_unknown_and_missing_capabilities_distinctly() {
        let catalog = AudioDeviceCatalog {
            outputs: vec![AudioDeviceInfo {
                id: "output".to_owned(),
                name: "Output".to_owned(),
                exclusive_sample_rates: Vec::new(),
                exclusive_default_period_hns: None,
                exclusive_minimum_period_hns: None,
                config_ranges: vec![AudioDeviceConfigRange {
                    minimum_sample_rate: 48_000,
                    maximum_sample_rate: 48_000,
                    channels: 2,
                    minimum_buffer_frames: None,
                    maximum_buffer_frames: None,
                }],
            }],
            default_output_id: Some("output".to_owned()),
            ..AudioDeviceCatalog::default()
        };
        let settings = AudioSettings {
            enable_input: false,
            ..AudioSettings::default()
        };
        assert_eq!(catalog.supported_sample_rates(&settings), [48_000]);
        assert_eq!(
            catalog.supported_buffer_sizes(&settings, 48_000),
            [32, 64, 128, 256, 512, 1_024, 2_048, 4_096, 8_192]
        );

        let missing_device_settings = AudioSettings {
            output_device_id: Some("removed-output".to_owned()),
            ..settings
        };
        assert!(
            catalog
                .supported_sample_rates(&missing_device_settings)
                .is_empty()
        );
    }

    #[test]
    fn custom_shared_sample_rates_and_buffer_sizes_use_reported_device_ranges() {
        let catalog = AudioDeviceCatalog {
            outputs: vec![AudioDeviceInfo {
                id: "output".to_owned(),
                name: "Output".to_owned(),
                config_ranges: vec![AudioDeviceConfigRange {
                    minimum_sample_rate: 32_000,
                    maximum_sample_rate: 48_000,
                    channels: 2,
                    minimum_buffer_frames: Some(64),
                    maximum_buffer_frames: Some(1_024),
                }],
                exclusive_sample_rates: Vec::new(),
                exclusive_default_period_hns: None,
                exclusive_minimum_period_hns: None,
            }],
            default_output_id: Some("output".to_owned()),
            ..AudioDeviceCatalog::default()
        };
        let settings = AudioSettings {
            enable_input: false,
            sample_rate: 40_000,
            buffer_frames: 300,
            ..AudioSettings::default()
        };

        assert!(catalog.supported_sample_rates(&settings).contains(&40_000));
        assert!(
            catalog
                .supported_buffer_sizes(&settings, 40_000)
                .contains(&300)
        );

        let outside_rate = AudioSettings {
            sample_rate: 49_000,
            ..settings.clone()
        };
        assert!(
            !catalog
                .supported_sample_rates(&outside_rate)
                .contains(&49_000)
        );

        let outside_buffer = AudioSettings {
            buffer_frames: 1_025,
            ..settings
        };
        assert!(
            !catalog
                .supported_buffer_sizes(&outside_buffer, 40_000)
                .contains(&1_025)
        );
    }

    #[test]
    fn exclusive_mode_sample_rates_are_intersected_for_selected_wasapi_devices() {
        let catalog = AudioDeviceCatalog {
            inputs: vec![AudioDeviceInfo {
                id: "wasapi:capture".to_owned(),
                name: "Capture".to_owned(),
                config_ranges: Vec::new(),
                exclusive_sample_rates: vec![12_000, 24_000, 44_100, 48_000, 96_000],
                exclusive_default_period_hns: None,
                exclusive_minimum_period_hns: None,
            }],
            outputs: vec![AudioDeviceInfo {
                id: "wasapi:render".to_owned(),
                name: "Render".to_owned(),
                config_ranges: Vec::new(),
                exclusive_sample_rates: vec![12_000, 24_000, 48_000, 96_000, 192_000],
                exclusive_default_period_hns: None,
                exclusive_minimum_period_hns: None,
            }],
            default_input_id: Some("wasapi:capture".to_owned()),
            default_output_id: Some("wasapi:render".to_owned()),
            default_exclusive_input_id: Some("wasapi:capture".to_owned()),
            default_exclusive_output_id: Some("wasapi:render".to_owned()),
            default_sample_rate: Some(48_000),
            error: None,
        };
        let settings = AudioSettings {
            access: AudioAccess::Exclusive,
            sample_rate: 192_000,
            ..AudioSettings::default()
        };
        assert_eq!(
            catalog.supported_sample_rates(&settings),
            [12_000, 24_000, 48_000, 96_000]
        );

        let output_only = AudioSettings {
            enable_input: false,
            ..settings
        };
        assert_eq!(
            catalog.supported_sample_rates(&output_only),
            [12_000, 24_000, 48_000, 96_000, 192_000]
        );
    }

    #[test]
    fn exclusive_buffer_sizes_include_device_periods_and_respect_the_largest_minimum() {
        let catalog = AudioDeviceCatalog {
            inputs: vec![AudioDeviceInfo {
                id: "wasapi:capture".to_owned(),
                name: "Capture".to_owned(),
                config_ranges: Vec::new(),
                exclusive_sample_rates: vec![48_000, 96_000],
                exclusive_default_period_hns: Some(100_000),
                exclusive_minimum_period_hns: Some(25_000),
            }],
            outputs: vec![AudioDeviceInfo {
                id: "wasapi:render".to_owned(),
                name: "Render".to_owned(),
                config_ranges: Vec::new(),
                exclusive_sample_rates: vec![48_000, 96_000],
                exclusive_default_period_hns: Some(50_000),
                exclusive_minimum_period_hns: Some(10_000),
            }],
            default_exclusive_input_id: Some("wasapi:capture".to_owned()),
            default_exclusive_output_id: Some("wasapi:render".to_owned()),
            ..AudioDeviceCatalog::default()
        };
        let settings = AudioSettings {
            access: AudioAccess::Exclusive,
            sample_rate: 48_000,
            buffer_frames: 96,
            ..AudioSettings::default()
        };

        let sizes = catalog.supported_buffer_sizes(&settings, settings.sample_rate);
        assert!(sizes.contains(&120));
        assert!(sizes.contains(&240));
        assert!(sizes.contains(&480));
        assert!(sizes.contains(&128));
        assert!(!sizes.contains(&96));
        assert!(!sizes.contains(&64));
        assert_eq!(
            catalog.buffer_size_option_label(&settings, 48_000, 120),
            "120 frames · WASAPI minimum period"
        );
        assert_eq!(
            catalog.buffer_size_option_label(&settings, 48_000, 480),
            "480 frames · WASAPI default period"
        );

        let doubled_rate = catalog.supported_buffer_sizes(&settings, 96_000);
        assert!(doubled_rate.contains(&240));
        assert!(doubled_rate.contains(&480));
        assert!(doubled_rate.contains(&960));
        assert!(!doubled_rate.contains(&96));
    }

    #[test]
    fn stream_buffer_negotiation_prefers_supported_and_clamps_known_ranges() {
        let range = SupportedBufferSize::Range { min: 128, max: 512 };
        assert_eq!(buffer_config_preference(&range, 256), (3, 0));
        assert_eq!(buffer_config_preference(&range, 64), (1, 64));
        assert_eq!(negotiated_buffer_frames(&range, 64), 128);
        assert_eq!(negotiated_buffer_frames(&range, 1_024), 512);
        assert_eq!(
            negotiated_buffer_frames(&SupportedBufferSize::Unknown, 256),
            256
        );
    }

    #[test]
    fn cpal_device_ranges_preserve_sample_rate_channel_and_buffer_limits() {
        let ranges = audio_device_config_ranges(
            [cpal::SupportedStreamConfigRange::new(
                2,
                44_100,
                96_000,
                SupportedBufferSize::Range {
                    min: 64,
                    max: 1_024,
                },
                cpal::SampleFormat::F32,
            )]
            .into_iter(),
        );
        assert_eq!(
            ranges,
            [AudioDeviceConfigRange {
                minimum_sample_rate: 44_100,
                maximum_sample_rate: 96_000,
                channels: 2,
                minimum_buffer_frames: Some(64),
                maximum_buffer_frames: Some(1_024),
            }]
        );
    }

    #[test]
    fn stereo_frame_delay_preserves_channels_across_block_boundaries() {
        let mut delay = StereoFrameDelay::new(2).expect("small delay line should allocate");
        let mut first = [1.0, -1.0, 2.0, -2.0];
        delay
            .process_interleaved(&mut first)
            .expect("stereo frames should process");
        assert_eq!(first, [0.0; 4]);

        let mut second = [3.0, -3.0, 4.0, -4.0, 5.0, -5.0];
        delay
            .process_interleaved(&mut second)
            .expect("stereo frames should process");
        assert_eq!(second, [1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
    }

    #[test]
    fn zero_frame_delay_and_invalid_buffers_are_handled() {
        let mut delay = StereoFrameDelay::new(0).expect("zero delay needs no allocation");
        let mut samples = [0.25, -0.5];
        delay
            .process_interleaved(&mut samples)
            .expect("zero delay should pass samples through");
        assert_eq!(samples, [0.25, -0.5]);
        assert!(delay.process_interleaved(&mut [0.0]).is_err());
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn denormal_protection_enables_the_architecture_flush_mode() {
        #[cfg(target_arch = "x86_64")]
        {
            use super::{read_mxcsr, write_mxcsr};
            const FTZ_AND_DAZ: u32 = (1 << 15) | (1 << 6);
            let previous = read_mxcsr();
            enable_denormal_protection();
            let enabled = read_mxcsr();
            write_mxcsr(previous);
            assert_eq!(enabled & FTZ_AND_DAZ, FTZ_AND_DAZ);
        }

        #[cfg(target_arch = "aarch64")]
        unsafe {
            let previous: u64;
            std::arch::asm!(
                "mrs {control}, fpcr",
                control = out(reg) previous,
                options(nostack, preserves_flags)
            );
            enable_denormal_protection();
            let enabled: u64;
            std::arch::asm!(
                "mrs {control}, fpcr",
                control = out(reg) enabled,
                options(nostack, preserves_flags)
            );
            std::arch::asm!(
                "msr fpcr, {control}",
                control = in(reg) previous,
                options(nostack, preserves_flags)
            );
            assert_ne!(enabled & (1 << 24), 0);
        }
    }

    #[test]
    fn output_samples_are_finite_and_clamped_for_the_master_meter() {
        assert_eq!(sanitize_output_sample(0.75), 0.75);
        assert_eq!(sanitize_output_sample(-1.5), -1.0);
        assert_eq!(sanitize_output_sample(1.5), 1.0);
        assert_eq!(sanitize_output_sample(f32::NAN), 0.0);
    }

    #[test]
    fn output_gain_changes_are_smoothed_over_multiple_audio_frames() {
        let alpha = output_gain_smoothing_alpha(48_000.0);
        let first_frame = smooth_output_gain(1.0, 0.0, alpha);
        assert!(first_frame < 1.0 && first_frame > 0.99);

        let after_block = (0..256).fold(1.0, |gain, _| smooth_output_gain(gain, 0.0, alpha));
        assert!(after_block < first_frame && after_block > 0.75);

        let settled = (0..10_000).fold(1.0, |gain, _| smooth_output_gain(gain, 0.0, alpha));
        assert!(settled < 0.001);
    }

    #[test]
    fn shared_is_the_default_access_mode_and_device_settings_are_sane() {
        let settings = AudioSettings::default();
        assert_eq!(settings.access, AudioAccess::Shared);
        assert_eq!(settings.output_gain, 1.0);
        assert_eq!(settings.sample_rate, 48_000);
        assert_eq!(settings.buffer_frames, 256);
    }

    #[test]
    fn playback_clock_reads_position_and_active_state_from_audio_transport() {
        let playback = Arc::new(PlaybackState::new());
        let clock = ProjectPlaybackClock {
            playback: Arc::clone(&playback),
        };
        assert!(!clock.is_active());
        assert_eq!(clock.position_frames(), 0);

        playback.start(Vec::new());
        playback.cursor_frames.store(480, Ordering::Release);
        assert!(clock.is_active());
        assert_eq!(clock.position_frames(), 480);

        playback.pause();
        assert!(!clock.is_active());
    }

    #[test]
    fn exclusive_audio_rejects_devices_from_non_wasapi_hosts() {
        let settings = AudioSettings {
            access: AudioAccess::Exclusive,
            output_device_id: Some("asio:studio-interface".into()),
            ..AudioSettings::default()
        };
        assert_eq!(
            validate_settings(&settings).unwrap_err(),
            "Exclusive device access is available through WASAPI devices only; switch to Shared access for this device"
        );

        let settings = AudioSettings {
            access: AudioAccess::Exclusive,
            output_device_id: Some("wasapi:system-endpoint".into()),
            ..AudioSettings::default()
        };
        assert!(validate_settings(&settings).is_ok());
    }

    #[test]
    fn audio_settings_reject_unreasonable_rates_and_buffers() {
        let settings = AudioSettings {
            sample_rate: 0,
            ..AudioSettings::default()
        };
        assert!(validate_settings(&settings).is_err());
        let settings = AudioSettings {
            buffer_frames: 1,
            ..AudioSettings::default()
        };
        assert!(validate_settings(&settings).is_err());
        let settings = AudioSettings {
            output_gain: 2.1,
            ..AudioSettings::default()
        };
        assert!(validate_settings(&settings).is_err());
    }

    #[test]
    fn ui_audio_commands_are_applied_by_the_callback_and_keep_latest_values() {
        let playback = PlaybackState::new();
        let source = AtomicU8::new(AUDIO_SOURCE_SILENT);
        let initial_gain = playback.browser_preview_gain.load(Ordering::Acquire);

        playback
            .enqueue_transport_command(TransportCommand::SetOutputSource(AUDIO_SOURCE_TEST_TONE));
        playback
            .enqueue_parameter_command(ParameterCommand::SetBrowserPreviewGain(0.25_f32.to_bits()));
        playback.enqueue_transport_command(TransportCommand::SetOutputSource(
            AUDIO_SOURCE_INPUT_MONITOR,
        ));
        playback
            .enqueue_parameter_command(ParameterCommand::SetBrowserPreviewGain(0.75_f32.to_bits()));

        assert_eq!(source.load(Ordering::Acquire), AUDIO_SOURCE_SILENT);
        assert_eq!(
            playback.browser_preview_gain.load(Ordering::Acquire),
            initial_gain
        );

        playback.apply_pending_commands(&source);

        assert_eq!(source.load(Ordering::Acquire), AUDIO_SOURCE_INPUT_MONITOR);
        assert_eq!(
            f32::from_bits(playback.browser_preview_gain.load(Ordering::Acquire)),
            0.75
        );
    }

    #[test]
    fn browser_preview_seeking_and_looping_follow_loaded_frame_bounds() {
        let playback = PlaybackState::new();
        let next_frame = |playback: &PlaybackState| {
            let samples = playback.browser_preview_samples.load();
            playback.next_browser_preview_frame(samples.as_deref().map(Vec::as_slice))
        };

        playback.start_browser_preview(vec![0.1, 0.1, 0.2, 0.2]);
        assert_eq!(playback.browser_preview_frame_count(), 2);
        assert_eq!(next_frame(&playback), [0.1, 0.1]);
        assert_eq!(playback.browser_preview_position_frames(), 1);

        playback.seek_browser_preview(1);
        assert_eq!(next_frame(&playback), [0.2, 0.2]);
        assert_eq!(playback.browser_preview_position_frames(), 2);
        assert_eq!(next_frame(&playback), [0.0, 0.0]);
        assert!(!playback.browser_preview_active());

        playback.set_browser_preview_looping(true);
        playback.seek_browser_preview(0);
        assert_eq!(next_frame(&playback), [0.1, 0.1]);
        assert_eq!(next_frame(&playback), [0.2, 0.2]);
        assert_eq!(next_frame(&playback), [0.1, 0.1]);
        assert_eq!(playback.browser_preview_position_frames(), 1);

        playback.seek_browser_preview(u64::MAX);
        assert_eq!(next_frame(&playback), [0.2, 0.2]);
        playback.stop_browser_preview();
        assert_eq!(playback.browser_preview_frame_count(), 0);
        assert_eq!(playback.browser_preview_position_frames(), 0);
    }

    #[test]
    fn full_audio_command_queue_retains_the_newest_transport_state() {
        let playback = PlaybackState::new();
        let source = AtomicU8::new(AUDIO_SOURCE_SILENT);
        let command_count = AUDIO_COMMAND_QUEUE_CAPACITY + 17;

        for index in 0..command_count {
            let source = if index.is_multiple_of(2) {
                AUDIO_SOURCE_TEST_TONE
            } else {
                AUDIO_SOURCE_INPUT_MONITOR
            };
            playback.enqueue_transport_command(TransportCommand::SetOutputSource(source));
        }
        playback.apply_pending_commands(&source);

        assert_eq!(
            source.load(Ordering::Acquire),
            if (command_count - 1).is_multiple_of(2) {
                AUDIO_SOURCE_TEST_TONE
            } else {
                AUDIO_SOURCE_INPUT_MONITOR
            }
        );
    }

    #[test]
    fn input_monitor_outputs_captured_stereo_channels() {
        let ring = AudioRingBuffer::new(4);
        assert!(ring.push_stereo_frame(0.25, -0.5));
        let source = AtomicU8::new(AUDIO_SOURCE_INPUT_MONITOR);
        let playback = PlaybackState::new();
        let mut phase = 0.0;
        assert_eq!(
            next_output_frame(&source, &ring, None, None, &playback, 48_000.0, &mut phase,),
            [0.25, -0.5]
        );
        assert_eq!(
            next_output_frame(&source, &ring, None, None, &playback, 48_000.0, &mut phase,),
            [0.0, 0.0]
        );
    }

    #[test]
    fn input_recording_preserves_stereo_frames_and_reports_overflow() {
        let recording = std::sync::Arc::new(InputRecordingState::new(44_100, 1));
        let reader = AudioInputRecording {
            recording: std::sync::Arc::clone(&recording),
        };

        let callback = recording.begin_callback().unwrap();
        callback.push_frame(0.25, -0.5);
        callback.push_frame(0.75, -1.0);
        assert_eq!(reader.sample_rate(), 44_100);
        assert_eq!(reader.overflow_frames(), 1);
        assert!(!reader.is_finished());

        recording.stop();
        assert!(recording.begin_callback().is_none());
        assert!(!reader.is_finished());
        drop(callback);
        let mut frames = [[0.0; 2]; 2];
        assert_eq!(reader.read_frames(&mut frames), 1);
        assert_eq!(frames[0], [0.25, -0.5]);
        assert!(reader.is_finished());
    }

    #[test]
    fn test_tone_uses_a_stable_amplitude_and_advancing_phase() {
        let (first, phase) = tone_sample(0.0, 48_000.0);
        let (second, next_phase) = tone_sample(phase, 48_000.0);
        assert_eq!(first, 0.0);
        assert!(second > 0.0);
        assert!(next_phase > phase);
        assert!(second <= 0.12);
    }

    #[test]
    fn project_output_preserves_stereo_order_and_stops_at_the_end() {
        let playback = PlaybackState::new();
        playback.start(vec![0.25_f32, -0.5, 0.75, -1.0]);
        let source = AtomicU8::new(AUDIO_SOURCE_PROJECT);
        let ring = AudioRingBuffer::new(2);
        let mut phase = 0.0;
        let samples = playback.samples.load();

        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                samples.as_deref().map(Vec::as_slice),
                None,
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.25, -0.5]
        );
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                samples.as_deref().map(Vec::as_slice),
                None,
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.75, -1.0]
        );
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                samples.as_deref().map(Vec::as_slice),
                None,
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.0, 0.0]
        );
        assert!(!playback.active.load(Ordering::Acquire));
        playback.resume().expect("loaded playback should resume");
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                samples.as_deref().map(Vec::as_slice),
                None,
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.25, -0.5]
        );
        playback.stop();
        assert!(playback.resume().is_err());
    }

    #[test]
    fn project_output_resamples_at_fractional_playback_speeds() {
        assert!(valid_project_playback_speed(0.25));
        assert!(valid_project_playback_speed(1.0));
        assert!(valid_project_playback_speed(4.0));
        assert!(!valid_project_playback_speed(0.249));
        assert!(!valid_project_playback_speed(4.001));
        assert!(!valid_project_playback_speed(f32::NAN));
        assert!(!valid_project_playback_speed(f32::INFINITY));

        let playback = PlaybackState::new();
        playback.start(vec![0.0, 0.0, 1.0, 1.0, 2.0, 2.0]);
        playback
            .playback_speed
            .store(0.5_f32.to_bits(), Ordering::Release);
        let source = AtomicU8::new(AUDIO_SOURCE_PROJECT);
        let ring = AudioRingBuffer::new(2);
        let mut phase = 0.0;
        let samples = playback.samples.load();
        let mut output = Vec::new();
        for _ in 0..6 {
            output.push(next_output_frame(
                &source,
                &ring,
                samples.as_deref().map(Vec::as_slice),
                None,
                &playback,
                48_000.0,
                &mut phase,
            ));
        }
        assert_eq!(
            output,
            [
                [0.0, 0.0],
                [0.5, 0.5],
                [1.0, 1.0],
                [1.5, 1.5],
                [2.0, 2.0],
                [2.0, 2.0],
            ]
        );
        assert_eq!(playback.position_frames(), 3);
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                samples.as_deref().map(Vec::as_slice),
                None,
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.0, 0.0]
        );
    }

    #[test]
    fn stereo_ring_keeps_frames_atomic_and_in_order() {
        let ring = AudioRingBuffer::new(4);
        assert!(ring.push_stereo_frame(0.25, -0.5));
        assert!(ring.push_stereo_frame(0.75, -1.0));
        assert!(!ring.push_stereo_frame(0.0, 0.0));
        assert_eq!(ring.pop_stereo_frame(), Some([0.25, -0.5]));
        assert_eq!(ring.pop_stereo_frame(), Some([0.75, -1.0]));
        assert_eq!(ring.pop_stereo_frame(), None);
    }

    #[test]
    fn streamed_output_waits_for_prefill_and_drains_before_stopping() {
        let playback = PlaybackState::new();
        let writer = playback.start_streaming(0);
        let source = AtomicU8::new(AUDIO_SOURCE_STREAM);
        let ring = AudioRingBuffer::new(2);
        let mut phase = 0.0;
        let stream = playback.streaming.load();

        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.0, 0.0]
        );
        assert_eq!(writer.underrun_frames(), 0);
        writer
            .write_stereo_samples(&[0.25, -0.5, 0.75, -1.0])
            .unwrap();
        writer.finish();

        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.25, -0.5]
        );
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.75, -1.0]
        );
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.0, 0.0]
        );
        assert!(!playback.is_active());
    }

    #[test]
    fn staged_stream_handoff_skips_already_consumed_frames() {
        let playback = Arc::new(PlaybackState::new());
        let old_writer = playback.start_streaming(1_000);
        old_writer
            .write_stereo_samples(&[0.25, -0.25].repeat(STREAM_PREFILL_FRAMES))
            .unwrap();
        let old_stream = playback.streaming.load_full().expect("old stream");
        old_stream.consumed_frames.store(1_024, Ordering::Release);

        let (next_stream, mut writers) = create_streaming_playback(1_000, 4_096, 1, true);
        let next_writer = writers.pop().expect("staged project writer");
        let mut next_samples = Vec::new();
        for frame in 0..STREAM_PREFILL_FRAMES + 32 {
            next_samples.extend([frame as f32, -(frame as f32)]);
        }
        next_writer.write_stereo_samples(&next_samples).unwrap();
        next_writer.finish();
        let staged = StagedStreamingPlayback {
            playback: Arc::clone(&playback),
            stream: Arc::clone(&next_stream),
        };
        next_stream
            .handoff_target_frame
            .store(1_000, Ordering::Release);
        playback
            .pending_streaming
            .store(Some(Arc::clone(&next_stream)));

        playback.apply_pending_streaming_handoff();

        assert!(staged.is_committed());
        assert_eq!(playback.position_frames(), 1_024);
        assert_eq!(
            next_stream.rings[0].peek_stereo_frame(0),
            Some([24.0, -24.0])
        );
        assert_eq!(playback.retired_streaming.len(), 1);
    }

    #[test]
    fn staged_stream_handoff_waits_for_target_and_prefill() {
        let playback = Arc::new(PlaybackState::new());
        let old_writer = playback.start_streaming(0);
        let old_stream = playback.streaming.load_full().expect("old stream");
        old_stream.consumed_frames.store(10, Ordering::Release);
        old_writer.finish();
        let (next_stream, mut writers) = create_streaming_playback(0, 4_096, 1, true);
        let next_writer = writers.pop().expect("staged project writer");
        next_writer
            .write_stereo_samples(&[0.5, -0.5].repeat(STREAM_PREFILL_FRAMES - 1))
            .unwrap();
        let staged = StagedStreamingPlayback {
            playback: Arc::clone(&playback),
            stream: Arc::clone(&next_stream),
        };
        next_stream
            .handoff_target_frame
            .store(11, Ordering::Release);
        playback
            .pending_streaming
            .store(Some(Arc::clone(&next_stream)));
        assert!(playback.is_project_active());

        playback.apply_pending_streaming_handoff();
        assert!(!staged.is_committed());

        old_stream.consumed_frames.store(11, Ordering::Release);
        next_writer
            .write_stereo_samples(&[0.5, -0.5].repeat(12))
            .unwrap();
        playback.apply_pending_streaming_handoff();

        assert!(staged.is_committed());
        assert_eq!(playback.position_frames(), 11);
        drop(old_writer);
    }

    #[test]
    fn stopping_cancels_a_pending_stream_handoff() {
        let playback = Arc::new(PlaybackState::new());
        let _old_writer = playback.start_streaming(0);
        let (next_stream, _writers) = create_streaming_playback(0, 4_096, 1, true);
        let staged = StagedStreamingPlayback {
            playback: Arc::clone(&playback),
            stream: Arc::clone(&next_stream),
        };
        next_stream.handoff_target_frame.store(0, Ordering::Release);
        playback
            .pending_streaming
            .store(Some(Arc::clone(&next_stream)));

        playback.stop();
        playback.apply_pending_streaming_handoff();

        assert!(next_stream.cancelled.load(Ordering::Acquire));
        assert!(playback.streaming.load().is_none());
        assert!(playback.pending_streaming.load().is_none());
        assert!(!staged.is_committed());
    }

    #[test]
    fn streamed_output_resamples_and_clamps_transport_at_end_of_source() {
        let playback = PlaybackState::new();
        let writer = playback.start_streaming(20);
        playback
            .playback_speed
            .store(2.0_f32.to_bits(), Ordering::Release);
        writer
            .write_stereo_samples(&[0.0, 0.0, 1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 4.0, 4.0])
            .unwrap();
        writer.finish();
        let source = AtomicU8::new(AUDIO_SOURCE_STREAM);
        let ring = AudioRingBuffer::new(2);
        let mut phase = 0.0;
        let stream = playback.streaming.load();

        for expected in [[0.0, 0.0], [2.0, 2.0], [4.0, 4.0]] {
            assert_eq!(
                next_output_frame(
                    &source,
                    &ring,
                    None,
                    stream.as_deref(),
                    &playback,
                    48_000.0,
                    &mut phase,
                ),
                expected
            );
        }
        assert_eq!(playback.position_frames(), 25);
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.0, 0.0]
        );
        assert!(!playback.is_active());
    }

    #[test]
    fn live_stream_sources_mix_on_one_clock_and_drain_independently() {
        let playback = PlaybackState::new();
        let mut writers = playback.start_streaming_sources_with_capacity(0, 8, 2);
        let second_writer = writers.pop().expect("second live writer");
        let first_writer = writers.pop().expect("first live writer");
        first_writer
            .write_stereo_samples(&[0.25, -0.5, 0.75, -1.0])
            .unwrap();
        second_writer
            .write_stereo_samples(&[0.5, 0.25, -0.25, 0.5])
            .unwrap();
        first_writer.finish();
        second_writer.finish();

        let source = AtomicU8::new(AUDIO_SOURCE_STREAM);
        let ring = AudioRingBuffer::new(2);
        let mut phase = 0.0;
        let stream = playback.streaming.load();
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.75, -0.25]
        );
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.5, -0.5]
        );
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.0, 0.0]
        );
        assert!(!playback.is_active());
    }

    #[test]
    fn live_stream_overlay_mixes_without_replacing_project_audio() {
        let playback = PlaybackState::new();
        let base_writer = playback.start_streaming(0);
        let base_samples = [0.25, -0.5].repeat(STREAM_PREFILL_FRAMES);
        base_writer.write_stereo_samples(&base_samples).unwrap();

        let source = AtomicU8::new(AUDIO_SOURCE_STREAM);
        let ring = AudioRingBuffer::new(2);
        let mut phase = 0.0;
        let stream = playback.streaming.load();
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.25, -0.5]
        );

        base_writer.finish();
        let overlay = playback.add_streaming_sources(1).unwrap().remove(0);
        overlay
            .write_stereo_samples(&[0.5, -0.25, 0.75, 0.25])
            .unwrap();
        overlay.finish();
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.75, -0.75]
        );
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [1.0, -0.25]
        );

        base_writer.finish();
        for _ in 0..STREAM_PREFILL_FRAMES - 3 {
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            );
        }
        assert!(!playback.is_project_active());
        assert!(playback.is_active());
        assert_eq!(playback.position_frames(), STREAM_PREFILL_FRAMES as u64);

        overlay.finish();
        assert!(playback.add_streaming_sources(1).is_err());
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.0, 0.0]
        );
        assert!(!playback.is_active());
    }

    #[test]
    fn streamed_output_pause_preserves_queued_frames_for_resume() {
        let playback = PlaybackState::new();
        let writer = playback.start_streaming(0);
        writer.write_stereo_samples(&[0.25, -0.5]).unwrap();
        writer.finish();
        let stream = playback.streaming.load();
        let source = AtomicU8::new(AUDIO_SOURCE_STREAM);
        let ring = AudioRingBuffer::new(2);
        let mut phase = 0.0;

        playback.pause();
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.0, 0.0]
        );
        assert_eq!(
            stream.as_deref().unwrap().rings[0].available_stereo_frames(),
            1
        );

        playback.resume().unwrap();
        assert_eq!(
            next_output_frame(
                &source,
                &ring,
                None,
                stream.as_deref(),
                &playback,
                48_000.0,
                &mut phase,
            ),
            [0.25, -0.5]
        );
    }
}
