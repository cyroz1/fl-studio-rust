#![allow(unsafe_code, non_snake_case)]
//! Preallocated VST3 event lists and sample-accurate parameter queues.
use std::{
    cell::{Cell, RefCell, UnsafeCell},
    rc::Rc,
};
use vst_audio_ring::{
    Event as AudioEvent, EventBuffer, EventData, MAX_EVENTS, MAX_SYSEX_BYTES, Transport,
};
use vst3::{Class, ComPtr, ComWrapper, Steinberg::Vst::*, Steinberg::*};

struct Points {
    id: Cell<u32>,
    count: Cell<usize>,
    values: [Cell<(i32, f64)>; MAX_EVENTS],
    budget: Rc<Cell<usize>>,
    overflow: Rc<Cell<bool>>,
}
impl Points {
    fn push(&self, offset: i32, value: f64) -> Option<i32> {
        let count = self.count.get();
        if offset < 0
            || !value.is_finite()
            || !(0.0..=1.0).contains(&value)
            || (count != 0 && self.values[count - 1].get().0 > offset)
        {
            self.overflow.set(true);
            return None;
        }
        if count != 0 && self.values[count - 1].get().0 == offset {
            self.values[count - 1].set((offset, value));
            return Some(count as i32 - 1);
        }
        if count == MAX_EVENTS || self.budget.get() == MAX_EVENTS {
            self.overflow.set(true);
            return None;
        }
        self.values[count].set((offset, value));
        self.count.set(count + 1);
        self.budget.set(self.budget.get() + 1);
        Some(count as i32)
    }
}
impl Class for Points {
    type Interfaces = (IParamValueQueue,);
}
impl IParamValueQueueTrait for Points {
    unsafe fn getParameterId(&self) -> u32 {
        self.id.get()
    }
    unsafe fn getPointCount(&self) -> i32 {
        self.count.get() as i32
    }
    unsafe fn getPoint(&self, index: i32, offset: *mut i32, value: *mut f64) -> tresult {
        if index < 0 || index as usize >= self.count.get() || offset.is_null() || value.is_null() {
            return kInvalidArgument;
        }
        let point = self.values[index as usize].get();
        // SAFETY: the SDK supplies writable point outputs for the validated index.
        unsafe { *offset = point.0 };
        // SAFETY: the normalized value output is live for this call.
        unsafe { *value = point.1 };
        kResultOk
    }
    unsafe fn addPoint(&self, offset: i32, value: f64, index: *mut i32) -> tresult {
        let Some(result) = self.push(offset, value) else {
            return kResultFalse;
        };
        if !index.is_null() {
            // SAFETY: optional SDK writable index output.
            unsafe { *index = result };
        }
        kResultOk
    }
}
pub(crate) struct Changes {
    points: Vec<ComWrapper<Points>>,
    pointers: Vec<ComPtr<IParamValueQueue>>,
    count: Cell<usize>,
    budget: Rc<Cell<usize>>,
    overflow: Rc<Cell<bool>>,
}
impl Changes {
    pub fn new() -> ComWrapper<Self> {
        let budget = Rc::new(Cell::new(0));
        let overflow = Rc::new(Cell::new(false));
        let points: Vec<_> = (0..MAX_EVENTS)
            .map(|_| {
                ComWrapper::new(Points {
                    id: Cell::new(0),
                    count: Cell::new(0),
                    values: std::array::from_fn(|_| Cell::new((0, 0.0))),
                    budget: budget.clone(),
                    overflow: overflow.clone(),
                })
            })
            .collect();
        let pointers = points
            .iter()
            .map(|p| p.to_com_ptr().expect("parameter queue"))
            .collect();
        ComWrapper::new(Self {
            points,
            pointers,
            count: Cell::new(0),
            budget,
            overflow,
        })
    }
    pub fn clear(&self) {
        self.count.set(0);
        self.budget.set(0);
        self.overflow.set(false);
    }
    fn queue(&self, id: u32) -> Option<usize> {
        let count = self.count.get();
        if let Some(index) = self.points[..count].iter().position(|p| p.id.get() == id) {
            return Some(index);
        }
        if count == MAX_EVENTS {
            self.overflow.set(true);
            return None;
        }
        self.points[count].id.set(id);
        self.points[count].count.set(0);
        self.count.set(count + 1);
        Some(count)
    }
    pub fn add(&self, id: u32, offset: i32, value: f64) -> bool {
        self.queue(id)
            .is_some_and(|index| self.points[index].push(offset, value).is_some())
    }
    pub fn drain(&self, edits: &crate::vst3_support::Edits) {
        self.clear();
        for _ in 0..MAX_EVENTS {
            let Some((id, value)) = edits.pop() else {
                break;
            };
            self.add(id, 0, value);
        }
    }
    pub fn collect(&self, base: u32, frames: u32, output: &mut EventBuffer) {
        for point in &self.points[..self.count.get()] {
            for value in &point.values[..point.count.get()] {
                let (offset, value) = value.get();
                let event = AudioEvent {
                    sample_offset: base.saturating_add(offset as u32),
                    bus: 0,
                    data: EventData::Parameter {
                        id: point.id.get(),
                        value,
                    },
                };
                if offset >= 0 && event.valid(frames) {
                    output.push(event);
                } else {
                    output.dropped = output.dropped.saturating_add(1);
                }
            }
        }
        if self.overflow.get() {
            output.dropped = output.dropped.saturating_add(1);
        }
    }
}
impl Class for Changes {
    type Interfaces = (IParameterChanges,);
}
impl IParameterChangesTrait for Changes {
    unsafe fn getParameterCount(&self) -> i32 {
        self.count.get() as i32
    }
    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        if index < 0 || index as usize >= self.count.get() {
            std::ptr::null_mut()
        } else {
            self.pointers[index as usize].as_ptr()
        }
    }
    unsafe fn addParameterData(&self, id: *const u32, index: *mut i32) -> *mut IParamValueQueue {
        if id.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: SDK passes a readable parameter ID.
        let id = unsafe { *id };
        let Some(found) = self.queue(id) else {
            return std::ptr::null_mut();
        };
        if !index.is_null() {
            // SAFETY: optional writable SDK queue-index output.
            unsafe { *index = found as i32 };
        }
        self.pointers[found].as_ptr()
    }
}

pub(crate) struct EventList {
    native: UnsafeCell<Box<[Event; MAX_EVENTS]>>,
    bytes: UnsafeCell<Box<[[u8; MAX_SYSEX_BYTES]; MAX_EVENTS]>>,
    count: Cell<usize>,
    output: RefCell<EventBuffer>,
    frames: Cell<u32>,
    base: Cell<u32>,
    buses: Cell<u16>,
}
impl EventList {
    pub fn new() -> ComWrapper<Self> {
        // SAFETY: SDK Event is a plain integer/float/pointer union; zero is a valid initial representation.
        let empty: Event = unsafe { std::mem::zeroed() };
        ComWrapper::new(Self {
            native: UnsafeCell::new(Box::new([empty; MAX_EVENTS])),
            bytes: UnsafeCell::new(Box::new([[0; MAX_SYSEX_BYTES]; MAX_EVENTS])),
            count: Cell::new(0),
            output: RefCell::new(EventBuffer::default()),
            frames: Cell::new(0),
            base: Cell::new(0),
            buses: Cell::new(0),
        })
    }
    pub fn reset(&self, frames: u32, base: u32, buses: u16) {
        self.count.set(0);
        self.output.borrow_mut().clear();
        self.frames.set(frames);
        self.base.set(base);
        self.buses.set(buses);
    }
    #[allow(clippy::too_many_arguments)]
    pub fn fill(
        &self,
        input: &[AudioEvent],
        start: usize,
        end: usize,
        context: Transport,
        rate: u32,
        changes: &Changes,
        mappings: &[(u32, u32, u32)],
        parameter_ids: &[u32],
        rejected: &mut u32,
    ) {
        self.count.set(0);
        // SAFETY: unique audio handle fills these buffers before any foreign process call.
        let native = unsafe { &mut *self.native.get() };
        // SAFETY: exclusive preparation of owned SysEx scratch; pointers remain valid through process.
        let bytes = unsafe { &mut *self.bytes.get() };
        for event in input
            .iter()
            .filter(|e| (start..end).contains(&(e.sample_offset as usize)))
        {
            if matches!(event.data, EventData::Transport(_)) {
                continue;
            }
            if let EventData::Parameter { id, value } = event.data {
                if parameter_ids.binary_search(&id).is_err()
                    || !changes.add(id, event.sample_offset as i32 - start as i32, value)
                {
                    *rejected = rejected.saturating_add(1);
                }
                continue;
            }
            if event.bus >= self.buses.get() {
                *rejected = rejected.saturating_add(1);
                continue;
            }
            let i = self.count.get();
            if i == MAX_EVENTS {
                *rejected = rejected.saturating_add(1);
                break;
            }
            let mut value = Event {
                busIndex: i32::from(event.bus),
                sampleOffset: event.sample_offset as i32 - start as i32,
                ppqPosition: context
                    .advanced(event.sample_offset as usize - start, rate)
                    .ppq_position,
                flags: 0,
                r#type: 0,
                __field0: Event__type0 {
                    noteOn: NoteOnEvent {
                        channel: 0,
                        pitch: 0,
                        tuning: 0.0,
                        velocity: 0.0,
                        length: 0,
                        noteId: -1,
                    },
                },
            };
            match &event.data {
                EventData::Midi([status, a, b]) => {
                    let channel = i16::from(status & 15);
                    match status & 0xf0 {
                        0x80 | 0x90 if status & 0xf0 == 0x80 || *b == 0 => {
                            value.r#type = 1;
                            value.__field0.noteOff = NoteOffEvent {
                                channel,
                                pitch: i16::from(*a),
                                velocity: f32::from(*b) / 127.0,
                                noteId: -1,
                                tuning: 0.0,
                            };
                        }
                        0x90 => {
                            value.r#type = 0;
                            value.__field0.noteOn = NoteOnEvent {
                                channel,
                                pitch: i16::from(*a),
                                velocity: f32::from(*b) / 127.0,
                                noteId: -1,
                                tuning: 0.0,
                                length: 0,
                            };
                        }
                        0xa0 => {
                            value.r#type = 3;
                            value.__field0.polyPressure = PolyPressureEvent {
                                channel,
                                pitch: i16::from(*a),
                                pressure: f32::from(*b) / 127.0,
                                noteId: -1,
                            };
                        }
                        kind => {
                            let (cc, normalized) = match kind {
                                0xb0 => (u32::from(*a), f64::from(*b) / 127.0),
                                0xc0 => (130, f64::from(*a) / 127.0),
                                0xd0 => (128, f64::from(*a) / 127.0),
                                0xe0 => (
                                    129,
                                    f64::from(u16::from(*a) | (u16::from(*b) << 7)) / 16383.0,
                                ),
                                _ => {
                                    *rejected = rejected.saturating_add(1);
                                    continue;
                                }
                            };
                            let key = (u32::from(event.bus) << 16) | ((channel as u32) << 8) | cc;
                            if let Ok(index) = mappings.binary_search_by_key(&key, |p| p.0) {
                                let normalized = if kind == 0xc0 {
                                    let steps = mappings[index].2;
                                    if steps == 0 || u32::from(*a) > steps {
                                        *rejected = rejected.saturating_add(1);
                                        continue;
                                    }
                                    f64::from(*a) / f64::from(steps)
                                } else {
                                    normalized
                                };
                                if !changes.add(mappings[index].1, value.sampleOffset, normalized) {
                                    *rejected = rejected.saturating_add(1);
                                }
                            } else {
                                *rejected = rejected.saturating_add(1);
                            }
                            continue;
                        }
                    }
                }
                EventData::SysEx { len, bytes: source } => {
                    bytes[i].copy_from_slice(source);
                    value.r#type = 2;
                    value.__field0.data = DataEvent {
                        size: u32::from(*len),
                        r#type: 0,
                        bytes: bytes[i].as_ptr(),
                    };
                }
                _ => continue,
            }
            native[i] = value;
            self.count.set(i + 1);
        }
    }
    fn reject(&self, result: tresult) -> tresult {
        let mut output = self.output.borrow_mut();
        output.dropped = output.dropped.saturating_add(1);
        result
    }
    pub fn collect(&self, output: &mut EventBuffer) {
        for event in self.output.borrow().as_slice() {
            output.push(*event);
        }
        output.dropped = output.dropped.saturating_add(self.output.borrow().dropped);
    }
}
impl Class for EventList {
    type Interfaces = (IEventList,);
}
impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> i32 {
        self.count.get() as i32
    }
    unsafe fn getEvent(&self, index: i32, event: *mut Event) -> tresult {
        if index < 0 || index as usize >= self.count.get() || event.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: initialized event in the retained audio-thread input list.
        let value = unsafe { (&*self.native.get())[index as usize] };
        // SAFETY: SDK gives one writable Event output.
        unsafe { *event = value };
        kResultOk
    }
    unsafe fn addEvent(&self, event: *mut Event) -> tresult {
        if event.is_null() {
            return self.reject(kInvalidArgument);
        }
        // SAFETY: SDK supplies a live event for synchronous copying.
        let event = unsafe { &*event };
        if event.sampleOffset < 0
            || event.busIndex < 0
            || event.busIndex >= i32::from(self.buses.get())
        {
            return self.reject(kInvalidArgument);
        }
        let mut result = AudioEvent {
            bus: event.busIndex as u16,
            sample_offset: self.base.get().saturating_add(event.sampleOffset as u32),
            ..Default::default()
        };
        let data = match event.r#type {
            0 => {
                // SAFETY: event tag selects the NoteOn union member.
                let n = unsafe { event.__field0.noteOn };
                if !(0..16).contains(&n.channel)
                    || !(0..128).contains(&n.pitch)
                    || !n.velocity.is_finite()
                {
                    return self.reject(kInvalidArgument);
                }
                EventData::Midi([
                    0x90 | n.channel as u8,
                    n.pitch as u8,
                    (n.velocity.clamp(0.0, 1.0) * 127.0).round() as u8,
                ])
            }
            1 => {
                // SAFETY: event tag selects NoteOff.
                let n = unsafe { event.__field0.noteOff };
                if !(0..16).contains(&n.channel)
                    || !(0..128).contains(&n.pitch)
                    || !n.velocity.is_finite()
                {
                    return self.reject(kInvalidArgument);
                }
                EventData::Midi([
                    0x80 | n.channel as u8,
                    n.pitch as u8,
                    (n.velocity.clamp(0.0, 1.0) * 127.0).round() as u8,
                ])
            }
            3 => {
                // SAFETY: event tag selects PolyPressure.
                let n = unsafe { event.__field0.polyPressure };
                if !(0..16).contains(&n.channel)
                    || !(0..128).contains(&n.pitch)
                    || !n.pressure.is_finite()
                {
                    return self.reject(kInvalidArgument);
                }
                EventData::Midi([
                    0xa0 | n.channel as u8,
                    n.pitch as u8,
                    (n.pressure.clamp(0.0, 1.0) * 127.0).round() as u8,
                ])
            }
            2 => {
                // SAFETY: event tag selects DataEvent.
                let n = unsafe { event.__field0.data };
                if n.r#type != 0
                    || n.size == 0
                    || n.size as usize > MAX_SYSEX_BYTES
                    || n.bytes.is_null()
                {
                    return self.reject(kInvalidArgument);
                }
                let mut bytes = [0; MAX_SYSEX_BYTES];
                // SAFETY: SDK data event supplies size readable bytes; copy before callback return.
                bytes[..n.size as usize].copy_from_slice(unsafe {
                    std::slice::from_raw_parts(n.bytes, n.size as usize)
                });
                EventData::SysEx {
                    len: n.size as u16,
                    bytes,
                }
            }
            65535 => {
                // SAFETY: legacy MIDI output tag selects its union member.
                let n = unsafe { event.__field0.midiCCOut };
                if !(0..16).contains(&n.channel) {
                    return self.reject(kInvalidArgument);
                }
                EventData::Midi(match n.controlNumber {
                    0..=127 => [0xb0 | n.channel as u8, n.controlNumber, n.value as u8],
                    128 => [0xd0 | n.channel as u8, n.value as u8, 0],
                    129 => [0xe0 | n.channel as u8, n.value as u8, n.value2 as u8],
                    130 => [0xc0 | n.channel as u8, n.value as u8, 0],
                    131 => [0xa0 | n.channel as u8, n.value as u8, n.value2 as u8],
                    _ => return self.reject(kInvalidArgument),
                })
            }
            _ => {
                return self.reject(kNotImplemented);
            }
        };
        result.data = data;
        if !result.valid(self.frames.get()) {
            return self.reject(kInvalidArgument);
        }
        if self.output.borrow_mut().push(result) {
            kResultOk
        } else {
            kResultFalse
        }
    }
}

pub(crate) fn process_context(context: Transport, rate: u32) -> ProcessContext {
    // SAFETY: SDK context consists of primitive fields with valid zero representations.
    let mut result: ProcessContext = unsafe { std::mem::zeroed() };
    result.state = 512
        | 1024
        | 2048
        | 4096
        | 8192
        | 131072
        | if context.playing { 2 } else { 0 }
        | if context.recording { 8 } else { 0 }
        | if context.cycling { 4 } else { 0 };
    result.sampleRate = f64::from(rate);
    result.projectTimeSamples = context.sample_position;
    result.continousTimeSamples = context.continuous_position;
    result.projectTimeMusic = context.ppq_position;
    result.tempo = context.tempo;
    result.barPositionMusic = context.bar_start;
    result.cycleStartMusic = context.cycle_start;
    result.cycleEndMusic = context.cycle_end;
    result.timeSigNumerator = i32::from(context.numerator);
    result.timeSigDenominator = i32::from(context.denominator);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_poly_pressure_and_invalid_output_have_explicit_results() {
        let list = EventList::new();
        list.reset(128, 0, 1);
        // SAFETY: SDK Event contains plain numbers/pointers with a valid zero representation.
        let mut event: Event = unsafe { std::mem::zeroed() };
        event.r#type = 65535;
        event.sampleOffset = 17;
        event.__field0.midiCCOut = LegacyMIDICCOutEvent {
            controlNumber: 131,
            channel: 2,
            value: 60,
            value2: 95,
        };
        // SAFETY: retained audio-thread list and a live writable SDK event.
        assert_eq!(unsafe { list.addEvent(&mut event) }, kResultOk);
        event.__field0.midiCCOut = LegacyMIDICCOutEvent {
            controlNumber: 131,
            channel: 17,
            value: 60,
            value2: 95,
        };
        // SAFETY: live SDK event; invalid MIDI channel must be rejected without dereferencing foreign data.
        assert_eq!(unsafe { list.addEvent(&mut event) }, kInvalidArgument);
        event.r#type = 99;
        // SAFETY: live SDK event with an unsupported tag, carrying no borrowed payload.
        assert_eq!(unsafe { list.addEvent(&mut event) }, kNotImplemented);
        let mut output = EventBuffer::default();
        list.collect(&mut output);
        assert_eq!(output.as_slice(), [AudioEvent::midi(17, 0, [0xa2, 60, 95])]);
        assert_eq!(output.dropped, 2);
    }
}
