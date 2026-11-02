#![allow(unsafe_code)]
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    path::PathBuf,
};
use vst_audio_protocol::ProcessingSetup;
use vst_audio_ring::{
    Event, EventBuffer, EventData, MAX_EVENTS, MAX_SYSEX_BYTES, SampleFormat, Transport,
};
use vst_audio_worker_sys::{
    plugin::{Vst2Plugin, Vst2Processor},
    vst3::{Vst3Plugin, Vst3Processor},
};

// This observes worker-side Rust allocations, including synchronous host callbacks,
// not a dynamically loaded fixture's separate allocator or its native malloc calls.
struct CountingAllocator;
thread_local! {
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn record_allocation() {
    let _ = ALLOCATIONS.try_with(|count| {
        if let Some(value) = count.get() {
            count.set(Some(value.saturating_add(1)));
        }
    });
}

// SAFETY: every operation forwards its unchanged pointer/layout to System;
// constant-initialized, thread-local counting neither allocates nor unwinds.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        // SAFETY: GlobalAlloc's caller supplies a valid nonzero layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        // SAFETY: GlobalAlloc's caller supplies a valid nonzero layout.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_allocation();
        // SAFETY: pointer belongs to System with this layout; caller supplies a
        // nonzero new size satisfying GlobalAlloc's size/alignment requirements.
        unsafe { System.realloc(pointer, layout, size) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: pointer was allocated through System with this exact layout
        // and the caller relinquishes it exactly once.
        unsafe { System.dealloc(pointer, layout) }
    }
}

struct AllocationGuard;
impl Drop for AllocationGuard {
    fn drop(&mut self) {
        ALLOCATIONS.set(None);
    }
}

fn assert_no_alloc(process: impl FnOnce()) {
    assert!(ALLOCATIONS.get().is_none(), "nested allocation guard");
    ALLOCATIONS.set(Some(0));
    let guard = AllocationGuard;
    process();
    let count = ALLOCATIONS.get().unwrap();
    drop(guard);
    assert_eq!(count, 0, "worker processing allocated");
}

const FRAMES: usize = 256;

fn fixture(variable: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .expect("advanced native fixture required; run tools/native_tests.py")
}

fn prepare_vst2(format: SampleFormat) -> (Vst2Plugin, Vst2Processor) {
    let mut plugin = Vst2Plugin::load(&fixture("VST_AUDIO_ADVANCED_VST2")).unwrap();
    let info = plugin.info();
    assert_eq!((info.inputs, info.outputs), (0, 2));
    plugin
        .configure_precision(48000, FRAMES as u32, 0, 2, format, false)
        .unwrap();
    plugin.set_parameter(0, 0.5).unwrap();
    plugin.activate();
    let processor = plugin.processor().unwrap();
    (plugin, processor)
}

fn prepare_vst3(format: SampleFormat) -> (Vst3Plugin, Vst3Processor) {
    let mut plugin = Vst3Plugin::load(&fixture("VST_AUDIO_ADVANCED_VST3"), None).unwrap();
    let info = plugin.info();
    assert_eq!((info.inputs, info.outputs), (0, 2));
    plugin
        .configure_audio(&ProcessingSetup {
            sample_rate: 48000,
            quantum: FRAMES as u32,
            sample_format: format,
            offline: false,
            inputs: info
                .input_buses
                .iter()
                .map(|bus| bus.configuration(true))
                .collect(),
            outputs: info
                .output_buses
                .iter()
                .map(|bus| bus.configuration(true))
                .collect(),
        })
        .unwrap();
    plugin.activate().unwrap();
    let processor = plugin.take_processor().unwrap();
    // Fill the actual control-to-audio queue before guarded DSP drains it.
    // set_parameter itself is a control API and intentionally allocates metadata.
    for index in 0..256 {
        plugin
            .set_parameter(0, if index == 255 { 0.5 } else { 0.25 })
            .unwrap();
    }
    assert_eq!(
        plugin.set_parameter(0, 0.75).unwrap_err().0,
        "parameter queue full"
    );
    (plugin, processor)
}

fn mixed_events() -> [Event; 6] {
    let mut bytes = [0; MAX_SYSEX_BYTES];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = (index % 128) as u8;
    }
    bytes[0] = 0xf0;
    bytes[MAX_SYSEX_BYTES - 1] = 0xf7;
    let sysex = EventData::SysEx {
        len: MAX_SYSEX_BYTES as u16,
        bytes,
    };
    [
        Event::midi(0, 0, [0x90, 60, 127]),
        Event {
            sample_offset: 17,
            bus: 0,
            data: sysex,
        },
        Event {
            sample_offset: 64,
            bus: 0,
            data: EventData::Parameter { id: 0, value: 0.25 },
        },
        Event {
            sample_offset: 128,
            bus: 0,
            data: EventData::Transport(Transport {
                sample_position: 2000,
                tempo: 137.0,
                ..Default::default()
            }),
        },
        Event {
            sample_offset: 143,
            bus: 0,
            data: sysex,
        },
        Event::midi(192, 0, [0x80, 60, 0]),
    ]
}

// Each precision/format has its own test so a missing or failing fixture cannot
// hide the other branch. All assertions of DSP samples and event counts execute
// inside the guard, without formatting/allocation unless an assertion fails.
macro_rules! processing_test {
    ($name:ident, $prepare:ident, $format:ident, $sample:ty, $process:ident, $tolerance:expr) => {
        #[test]
        #[ignore = "requires advanced native fixtures built by tools/native_tests.py"]
        fn $name() {
            let (_plugin, mut processor) = $prepare(SampleFormat::$format);
            let mut left = [0.0 as $sample; FRAMES];
            let mut right = [0.0 as $sample; FRAMES];
            let mut output = EventBuffer::default();
            let input = mixed_events();
            let expected_events = [input[0], input[1], input[4], input[5]];
            let context = Transport {
                sample_position: 1000,
                ..Default::default()
            };
            assert_no_alloc(|| {
                assert!(processor.$process(
                    &mut [&mut left, &mut right],
                    FRAMES,
                    &context,
                    &input,
                    &mut output,
                ));
                assert_eq!(output.dropped, 0);
                assert_eq!(output.as_slice().len(), 4);
                assert_eq!(output.as_slice(), expected_events);
                for frame in 0..FRAMES {
                    let expected_left = if frame < 64 {
                        0.0617283945061725
                    } else if frame < 192 {
                        0.03086419725308625
                    } else {
                        0.0
                    };
                    let expected_right = if frame < 128 {
                        (1000 + frame) as f64 * 0.000001
                    } else if frame < 192 {
                        (2000 + frame - 128) as f64 * 0.000001
                    } else {
                        0.0
                    };
                    assert!((f64::from(left[frame]) - expected_left).abs() < $tolerance);
                    assert!((f64::from(right[frame]) - expected_right).abs() < $tolerance);
                }
            });

            // Restore the note gate in its own block, leaving all 256 event
            // slots free for distinct-offset automation points in the next block.
            let note = [Event::midi(0, 0, [0x90, 60, 127])];
            assert_no_alloc(|| {
                assert!(processor.$process(
                    &mut [&mut left, &mut right],
                    FRAMES,
                    &context,
                    &note,
                    &mut output,
                ));
                assert_eq!(output.dropped, 0);
                assert_eq!(output.as_slice(), note);
                assert!(
                    left.iter()
                        .all(|value| (f64::from(*value) - 0.03086419725308625).abs() < $tolerance)
                );
            });
            let parameters: [Event; MAX_EVENTS] = std::array::from_fn(|index| Event {
                sample_offset: index as u32,
                bus: 0,
                data: EventData::Parameter {
                    id: 0,
                    value: if index % 2 == 0 { 0.25 } else { 0.75 },
                },
            });
            // VST3 fills one 256-point SDK queue; VST2 splits into 256 calls.
            // Repeat to verify reuse/reset rather than only a fresh buffer.
            for _ in 0..2 {
                assert_no_alloc(|| {
                    assert!(processor.$process(
                        &mut [&mut left, &mut right],
                        FRAMES,
                        &context,
                        &parameters,
                        &mut output,
                    ));
                    assert_eq!(output.dropped, 0);
                    assert_eq!(output.as_slice().len(), 0);
                    for frame in 0..FRAMES {
                        let expected_left = if frame % 2 == 0 {
                            0.03086419725308625
                        } else {
                            0.09259259175925875
                        };
                        assert!((f64::from(left[frame]) - expected_left).abs() < $tolerance);
                        assert!(
                            (f64::from(right[frame]) - (1000 + frame) as f64 * 0.000001).abs()
                                < $tolerance
                        );
                    }
                });
            }
        }
    };
}

processing_test!(
    vst2_f32_prepared_processing_does_not_allocate,
    prepare_vst2,
    F32,
    f32,
    process_block,
    1e-8
);
processing_test!(
    vst2_f64_prepared_processing_does_not_allocate,
    prepare_vst2,
    F64,
    f64,
    process_block_f64,
    1e-14
);
processing_test!(
    vst3_f32_prepared_processing_does_not_allocate,
    prepare_vst3,
    F32,
    f32,
    process_block,
    1e-8
);
processing_test!(
    vst3_f64_prepared_processing_does_not_allocate,
    prepare_vst3,
    F64,
    f64,
    process_block_f64,
    1e-14
);
