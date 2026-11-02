#![forbid(unsafe_code)]
use std::path::Path;
use vst_audio_worker_sys::plugin::Vst2Plugin;

#[test]
#[ignore = "requires the public native fixture built by tools/native_tests.py"]
fn processor_is_unique_and_freezes_configuration() {
    let path = std::env::var_os("VST_AUDIO_TEST_VST2").expect("fixture required");
    let mut plugin = Vst2Plugin::load(Path::new(&path)).unwrap();
    assert!(plugin.processor().is_err());
    plugin.configure(48000, 1024, 2, 6).unwrap();
    let processor = plugin.processor().unwrap();
    assert!(plugin.processor().is_err());
    // An old processor must never send 1024 frames after a 64-frame reconfiguration.
    assert!(plugin.configure(48000, 64, 2, 6).is_err());
    drop(processor);
}

#[test]
#[ignore = "native fixtures required"]
fn io_change_never_calls_old_buffers() {
    let path = std::env::var_os("VST_AUDIO_TEST_VST2").unwrap();
    let mut plugin = Vst2Plugin::load(Path::new(&path)).unwrap();
    plugin.configure(44100, 128, 2, 6).unwrap();
    plugin.set_state(&[1, 7]).unwrap();
    plugin.activate();
    let mut processor = plugin.processor().unwrap();
    let mut storage = [[0.0; 128]; 6];
    assert!(
        processor.process(
            &mut storage
                .iter_mut()
                .map(|p| p.as_mut_slice())
                .collect::<Vec<_>>(),
            128
        )
    );
    let blob = plugin.get_state().unwrap();
    assert_eq!(u32::from_le_bytes(blob[10..14].try_into().unwrap()), 44100);
    assert!(plugin.set_state(&[1, 9]).is_err());
    for _ in 0..20 {
        assert!(
            !processor.process(
                &mut storage
                    .iter_mut()
                    .map(|p| p.as_mut_slice())
                    .collect::<Vec<_>>(),
                128
            )
        );
    }
    assert!(plugin.take_events().fault.is_some());
    let blob = plugin.get_state().unwrap();
    assert_eq!(u32::from_le_bytes(blob[34..38].try_into().unwrap()), 0);
    println!("resume time=44100, post-change process calls=0 across 20 attempts");
}

#[test]
#[ignore = "native fixtures required"]
fn audio_io_callback_blocks_following_calls_before_control_poll() {
    let path = std::env::var_os("VST_AUDIO_TEST_VST2").unwrap();
    let mut plugin = Vst2Plugin::load(Path::new(&path)).unwrap();
    plugin.set_state(&[1, 17]).unwrap();
    plugin.configure(48000, 128, 2, 6).unwrap();
    plugin.activate();
    let mut processor = plugin.processor().unwrap();
    let mut storage = [[0.0; 128]; 6];
    let mut planes: Vec<_> = storage.iter_mut().map(|p| p.as_mut_slice()).collect();
    assert!(processor.process(&mut planes, 128));
    for _ in 0..20 {
        assert!(!processor.process(&mut planes, 128));
    }
    assert!(plugin.take_events().fault.is_some());
    assert!(!processor.process(&mut planes, 128));
    let blob = plugin.get_state().unwrap();
    assert_eq!(u32::from_le_bytes(blob[34..38].try_into().unwrap()), 0);
    println!("audio ioChanged: post-change DSP calls=0 before and after control poll");
}
