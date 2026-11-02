#![allow(unsafe_code)]
//! VST2 event/time ABI. MIDI list and time layout also checked against DPF
//! DistrhoPluginVST2.cpp at 4238e1c7f0351bbe488d79f0899c540543ac7583 (ISC).
//! Copyright (C) 2012-2024 Filipe Coelho. Permission to use, copy, modify and/or
//! distribute for any purpose with or without fee is granted provided this
//! notice appears in all copies. THE SOFTWARE IS PROVIDED "AS IS" WITHOUT ANY
//! WARRANTY; THE AUTHOR DISCLAIMS LIABILITY FOR ANY DAMAGES ARISING FROM USE.
use crate::vst2::AEffect;
use std::{
    cell::UnsafeCell,
    ffi::c_void,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};
use vst_audio_ring::{Event, EventBuffer, EventData, MAX_EVENTS, MAX_SYSEX_BYTES, Transport};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MidiEvent {
    pub kind: i32,
    pub size: i32,
    pub offset: i32,
    pub flags: i32,
    pub note_length: i32,
    pub note_offset: i32,
    pub data: [u8; 4],
    pub detune: i8,
    pub note_off_velocity: u8,
    pub reserved: [u8; 2],
}
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SysExEvent {
    pub kind: i32,
    pub size: i32,
    pub offset: i32,
    pub flags: i32,
    pub dump_bytes: i32,
    pub reserved1: isize,
    pub data: *mut u8,
    pub reserved2: isize,
}
impl Default for SysExEvent {
    fn default() -> Self {
        Self {
            kind: 6,
            size: size_of::<Self>() as i32,
            offset: 0,
            flags: 0,
            dump_bytes: 0,
            reserved1: 0,
            data: std::ptr::null_mut(),
            reserved2: 0,
        }
    }
}
#[repr(C)]
#[derive(Debug)]
pub struct Events<const N: usize> {
    pub count: i32,
    pub reserved: isize,
    pub events: [*const c_void; N],
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TimeInfo {
    pub sample_position: f64,
    pub sample_rate: f64,
    pub nanos: f64,
    pub ppq: f64,
    pub tempo: f64,
    pub bar: f64,
    pub cycle_start: f64,
    pub cycle_end: f64,
    pub numerator: i32,
    pub denominator: i32,
    pub smpte_offset: i32,
    pub smpte_rate: i32,
    pub samples_to_clock: i32,
    pub flags: i32,
}
thread_local! {
    static TIME: UnsafeCell<TimeInfo> = const { UnsafeCell::new(TimeInfo { sample_position:0.0, sample_rate:0.0, nanos:0.0, ppq:0.0, tempo:0.0, bar:0.0, cycle_start:0.0, cycle_end:0.0, numerator:0, denominator:0, smpte_offset:0, smpte_rate:0, samples_to_clock:0, flags:0 }) };
}
#[derive(Debug)]
pub struct CallbackEvents {
    pub output: crossbeam_queue::ArrayQueue<Event>,
    pub dropped: AtomicU32,
    pub base: AtomicU32,
    time: [AtomicU64; 10],
    sequence: AtomicU64,
}
impl Default for CallbackEvents {
    fn default() -> Self {
        Self {
            output: crossbeam_queue::ArrayQueue::new(MAX_EVENTS),
            dropped: AtomicU32::new(0),
            base: AtomicU32::new(0),
            time: std::array::from_fn(|_| AtomicU64::new(0)),
            sequence: AtomicU64::new(0),
        }
    }
}
impl CallbackEvents {
    pub fn context(&self, context: Transport, rate: u32, base: usize) {
        self.base.store(base as u32, Ordering::Relaxed);
        let flags = 0x3e00
            | if context.playing { 2 } else { 0 }
            | if context.cycling { 4 } else { 0 }
            | if context.recording { 8 } else { 0 };
        let values = [
            (context.sample_position as f64).to_bits(),
            f64::from(rate).to_bits(),
            context.ppq_position.to_bits(),
            context.tempo.to_bits(),
            context.bar_start.to_bits(),
            context.cycle_start.to_bits(),
            context.cycle_end.to_bits(),
            u64::from(context.numerator),
            u64::from(context.denominator),
            flags,
        ];
        self.sequence.fetch_add(1, Ordering::SeqCst);
        for (to, from) in self.time.iter().zip(values) {
            to.store(from, Ordering::SeqCst);
        }
        self.sequence.fetch_add(1, Ordering::SeqCst);
    }
    pub fn time_pointer(&self) -> isize {
        TIME.with(|cell| {
            let values = loop {
                let before = self.sequence.load(Ordering::SeqCst);
                if before & 1 != 0 {
                    continue;
                }
                let values: [u64; 10] =
                    std::array::from_fn(|i| self.time[i].load(Ordering::SeqCst));
                if before == self.sequence.load(Ordering::SeqCst) {
                    break values;
                }
            };
            let f = |i: usize| f64::from_bits(values[i]);
            let value = TimeInfo {
                sample_position: f(0),
                sample_rate: f(1),
                ppq: f(2),
                tempo: f(3),
                bar: f(4),
                cycle_start: f(5),
                cycle_end: f(6),
                numerator: values[7] as i32,
                denominator: values[8] as i32,
                flags: values[9] as i32,
                ..TimeInfo::default()
            };
            // SAFETY: thread-local plain value, accessed only in this callback;
            // VST time pointers are valid until the next getTime call on the thread.
            unsafe { cell.get().write(value) };
            cell.get() as isize
        })
    }
    /// # Safety
    /// `pointer` must be a live VST2 event list with its count of readable event
    /// pointers; MIDI/SysEx payloads must remain readable throughout this call.
    pub unsafe fn receive(&self, pointer: *mut c_void) -> isize {
        if pointer.is_null() {
            return 0;
        }
        let list = pointer.cast::<Events<0>>();
        if !list.is_aligned() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        let count = list
            .wrapping_byte_add(std::mem::offset_of!(Events<0>, count))
            .cast::<i32>();
        // SAFETY: VST processEvents provides a readable, aligned list header.
        let count = unsafe { count.read() };
        if !(0..=MAX_EVENTS as i32).contains(&count) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return 0;
        }
        // The inline pointer array is addressed from the plugin's own pointer,
        // never through a reference to the zero-length header type, whose extent
        // would not cover it.
        let first = list
            .wrapping_byte_add(std::mem::offset_of!(Events<0>, events))
            .cast::<*const c_void>();
        // SAFETY: the ABI list stores count inline pointers after the aligned header.
        let pointers = unsafe { std::slice::from_raw_parts(first, count as usize) };
        for pointer in pointers {
            if pointer.is_null() || !pointer.cast::<i32>().is_aligned() {
                continue;
            }
            // SAFETY: each event has the common 16-byte header, live during callback.
            let kind = unsafe { *pointer.cast::<i32>() };
            // SAFETY: the declared byte size sits second in that common header.
            let size = unsafe { pointer.cast::<i32>().wrapping_add(1).read() };
            // byteSize excludes the type and byteSize fields (aeffectx.h VstEvent),
            // and hosts often store sizeof instead: either way the plugin vouches
            // for 8 + byteSize readable bytes, which must cover every field read.
            let readable = usize::try_from(size).unwrap_or(0).saturating_add(8);
            let event = match kind {
                1 => {
                    // A short record must not cause a full MidiEvent reference or
                    // payload read before size validation (audit #11 TD-06;
                    // short_midi_header_is_dropped_and_worker_keeps_processing,
                    // midi_byte_size_excludes_the_common_header).
                    let raw = pointer.cast::<MidiEvent>();
                    if !raw.is_aligned() || readable < size_of::<MidiEvent>() {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    // SAFETY: the aligned record's declared size covers offset;
                    // the plugin must keep the declared bytes readable during the callback.
                    let offset = unsafe {
                        raw.wrapping_byte_add(std::mem::offset_of!(MidiEvent, offset))
                            .cast::<i32>()
                            .read()
                    };
                    if offset < 0 {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    // SAFETY: the validated size covers these three MIDI bytes;
                    // a byte array requires no additional alignment.
                    let data = unsafe {
                        raw.wrapping_byte_add(std::mem::offset_of!(MidiEvent, data))
                            .cast::<[u8; 3]>()
                            .read()
                    };
                    Event::midi(
                        (offset as u32).saturating_add(self.base.load(Ordering::Relaxed)),
                        0,
                        data,
                    )
                }
                6 => {
                    // Only the fields through `data` are read, and only once the
                    // plugin's declared size says they exist; no reference to the
                    // whole record is formed.
                    let raw = pointer.cast::<SysExEvent>();
                    let needed = std::mem::offset_of!(SysExEvent, data) + size_of::<*mut u8>();
                    if !raw.is_aligned() || readable < needed {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let field = |offset: usize| raw.wrapping_byte_add(offset);
                    // SAFETY: aligned SysEx record whose declared size covers `offset`.
                    let offset = unsafe {
                        field(std::mem::offset_of!(SysExEvent, offset))
                            .cast::<i32>()
                            .read()
                    };
                    // SAFETY: the declared size covers `dump_bytes`.
                    let dump_bytes = unsafe {
                        field(std::mem::offset_of!(SysExEvent, dump_bytes))
                            .cast::<i32>()
                            .read()
                    };
                    // SAFETY: the declared size covers `data`.
                    let data = unsafe {
                        field(std::mem::offset_of!(SysExEvent, data))
                            .cast::<*mut u8>()
                            .read()
                    };
                    if offset < 0
                        || dump_bytes <= 0
                        || dump_bytes as usize > MAX_SYSEX_BYTES
                        || data.is_null()
                    {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let mut bytes = [0; MAX_SYSEX_BYTES];
                    // SAFETY: callback supplies dump_bytes readable bytes; copy before returning.
                    bytes[..dump_bytes as usize].copy_from_slice(unsafe {
                        std::slice::from_raw_parts(data, dump_bytes as usize)
                    });
                    Event {
                        sample_offset: (offset as u32)
                            .saturating_add(self.base.load(Ordering::Relaxed)),
                        bus: 0,
                        data: EventData::SysEx {
                            len: dump_bytes as u16,
                            bytes,
                        },
                    }
                }
                _ => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            if self.output.push(event).is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        1
    }
    pub fn drain(&self, output: &mut EventBuffer, frames: usize) {
        for _ in 0..MAX_EVENTS {
            let Some(event) = self.output.pop() else {
                break;
            };
            if event.valid(frames as u32) {
                output.push(event);
            } else {
                output.dropped = output.dropped.saturating_add(1);
            }
        }
        output.dropped = output
            .dropped
            .saturating_add(self.dropped.swap(0, Ordering::Relaxed));
    }
}

pub(crate) trait Sample: Copy {
    const DOUBLE: bool;
    unsafe fn invoke(
        effect: *mut AEffect,
        inputs: *const *const Self,
        outputs: *mut *mut Self,
        frames: i32,
    );
}
impl Sample for f32 {
    const DOUBLE: bool = false;
    unsafe fn invoke(
        effect: *mut AEffect,
        inputs: *const *const Self,
        outputs: *mut *mut Self,
        frames: i32,
    ) {
        // SAFETY: active plugin effect, selected f32 entry validated at configuration.
        let process = unsafe { (*effect).process_replacing }.expect("configured f32 entry");
        // SAFETY: processor supplies bounded live buffers with the configured channel counts.
        unsafe { process(effect, inputs, outputs, frames) };
    }
}
impl Sample for f64 {
    const DOUBLE: bool = true;
    unsafe fn invoke(
        effect: *mut AEffect,
        inputs: *const *const Self,
        outputs: *mut *mut Self,
        frames: i32,
    ) {
        // SAFETY: active plugin effect, selected f64 entry validated at configuration.
        let process = unsafe { (*effect).process_double }.expect("configured f64 entry");
        // SAFETY: processor supplies bounded live buffers with the configured channel counts.
        unsafe { process(effect, inputs, outputs, frames) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Delivers one event record through processEvents and reports (received, dropped).
    fn deliver(record: *const c_void) -> (usize, u32) {
        let events = CallbackEvents::default();
        let mut list = Events::<1> {
            count: 1,
            reserved: 0,
            events: [record],
        };
        // SAFETY: the aligned list and its record stay live for this synchronous call.
        unsafe { events.receive((&mut list as *mut Events<1>).cast()) };
        (events.output.len(), events.dropped.load(Ordering::Relaxed))
    }

    #[test]
    fn midi_byte_size_excludes_the_common_header() {
        // aeffectx.h: byteSize excludes type and byteSize, so a full record says 24;
        // hosts that store sizeof(VstMidiEvent) say 32 (audit #11 TD-06).
        for size in [24, 32] {
            let event = MidiEvent {
                kind: 1,
                size,
                data: [0x90, 60, 100, 0],
                ..Default::default()
            };
            assert_eq!(
                deliver((&raw const event).cast()),
                (1, 0),
                "byteSize {size}"
            );
        }
        // Only the common header is readable: dropped without reading past it.
        let header = [1i32, 8, 0, 0];
        assert_eq!(deliver(header.as_ptr().cast()), (0, 1));
    }

    #[test]
    fn sysex_byte_size_excludes_the_common_header() {
        let dump = [0xf0u8, 0x7e, 0xf7];
        let event = SysExEvent {
            size: (size_of::<SysExEvent>() - 8) as i32,
            dump_bytes: dump.len() as i32,
            data: dump.as_ptr().cast_mut(),
            ..Default::default()
        };
        assert_eq!(deliver((&raw const event).cast()), (1, 0));
    }
}
