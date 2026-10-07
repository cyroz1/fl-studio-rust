use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use flp_rebuild::media::{SamplePathResolver, decode_audio_file};
use flp_rebuild::midi::MidiFile;
use flp_rebuild::plugins::scan_installed_plugins;
use flp_rebuild::sample_render::{AudioClipRenderOptions, render_audio_clips_to_wav};
use flp_rebuild::vst3::{Vst3HostRuntime, Vst3PatternRenderOptions};
use flp_rebuild::{
    AutomationPointEdit, FlpDocument, PatternNote, PatternNoteEdit, PlaylistClipEdit,
};

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    match args.as_slice() {
        [command, path] if command == "info" => inspect(Path::new(path)),
        [command, path] if command == "channels" => list_channels(Path::new(path)),
        [command, path] if command == "mixer" => list_mixer(Path::new(path)),
        [command, path] if command == "automation" => list_automation(Path::new(path)),
        [command, path] if command == "time-markers" => list_time_markers(Path::new(path)),
        [command, path] if command == "sample-paths" => list_sample_paths(Path::new(path)),
        [command, path] if command == "audio-info" => inspect_audio_file(Path::new(path)),
        [command, project, output] if command == "render-audio-clips" => render_audio_clips(
            Path::new(project),
            Path::new(output),
            AudioClipRenderOptions::default(),
        ),
        [command, project, output, arrangement_id] if command == "render-audio-clips" => {
            render_audio_clips(
                Path::new(project),
                Path::new(output),
                AudioClipRenderOptions {
                    arrangement_id: parse_u16(arrangement_id, "arrangement id")?,
                    ..AudioClipRenderOptions::default()
                },
            )
        }
        [command, path] if command == "plugin-states" => list_plugin_states(Path::new(path)),
        [command, path, channel_id] if command == "plugin-state-preview" => {
            preview_plugin_state(Path::new(path), parse_u16(channel_id, "channel id")?, 64)
        }
        [command, bundle, path, channel_id] if command == "vst3-state-probe" => probe_vst3_state(
            Path::new(bundle),
            Path::new(path),
            parse_u16(channel_id, "channel id")?,
        ),
        [command, project, pattern_id, channel_id, bundle, output]
            if command == "render-pattern-vst3" =>
        {
            render_pattern_vst3(
                Path::new(project),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Path::new(bundle),
                Path::new(output),
                2.0,
            )
        }
        [command, project, pattern_id, channel_id, bundle, output, tail]
            if command == "render-pattern-vst3" =>
        {
            let tail_seconds = tail
                .parse::<f64>()
                .map_err(|_| "render tail must be a number of seconds".to_owned())?;
            render_pattern_vst3(
                Path::new(project),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Path::new(bundle),
                Path::new(output),
                tail_seconds,
            )
        }
        [command, path, channel_id] if command == "channel-events" => list_channel_events(
            Path::new(path),
            parse_u16(channel_id, "channel id")?,
        ),
        [command, path] if command == "patterns" => list_patterns(Path::new(path)),
        [command, path] if command == "playlist" => list_playlist(Path::new(path), 0, 16),
        [command, path, start] if command == "playlist" => {
            list_playlist(Path::new(path), parse_usize(start, "clip start")?, 16)
        }
        [command, path, start, count] if command == "playlist" => list_playlist(
            Path::new(path),
            parse_usize(start, "clip start")?,
            parse_usize(count, "clip count")?,
        ),
        [command, path] if command == "events" => dump_events(Path::new(path), 0, 64),
        [command, path] if command == "midi-info" => inspect_midi(Path::new(path)),
        [command, path, track] if command == "midi-events" => {
            dump_midi_events(Path::new(path), parse_usize(track, "track number")?, 0, 64)
        }
        [command, path, track, start] if command == "midi-events" => dump_midi_events(
            Path::new(path),
            parse_usize(track, "track number")?,
            parse_usize(start, "event start")?,
            64,
        ),
        [command, path, track, start, count] if command == "midi-events" => dump_midi_events(
            Path::new(path),
            parse_usize(track, "track number")?,
            parse_usize(start, "event start")?,
            parse_usize(count, "event count")?,
        ),
        [command, path] if command == "scan" => scan_corpus(Path::new(path)),
        [command] if command == "plugin-scan" => scan_plugins(),
        [command, path, start] if command == "events" => {
            dump_events(Path::new(path), parse_usize(start, "event start")?, 64)
        }
        [command, path, start, count] if command == "events" => dump_events(
            Path::new(path),
            parse_usize(start, "event start")?,
            parse_usize(count, "event count")?,
        ),
        [command, path, pattern_id] if command == "notes" => list_notes(
            Path::new(path),
            parse_u16(pattern_id, "pattern id")?,
            0,
            usize::MAX,
        ),
        [command, path, pattern_id, start] if command == "notes" => list_notes(
            Path::new(path),
            parse_u16(pattern_id, "pattern id")?,
            parse_usize(start, "note start")?,
            usize::MAX,
        ),
        [command, path, pattern_id, start, count] if command == "notes" => list_notes(
            Path::new(path),
            parse_u16(pattern_id, "pattern id")?,
            parse_usize(start, "note start")?,
            parse_usize(count, "note count")?,
        ),
        [command, input, event_index, output] if command == "extract-event" => extract_event(
            Path::new(input),
            parse_usize(event_index, "event index")?,
            Path::new(output),
        ),
        [command, input, output] if command == "roundtrip" => {
            roundtrip(Path::new(input), Path::new(output))
        }
        [command, input, output, bpm] if command == "set-tempo" => {
            let milli_bpm = parse_tempo_milli_bpm(bpm)?;
            set_tempo(Path::new(input), Path::new(output), milli_bpm)
        }
        [command, input, output, channel_id, name] if command == "rename-channel" => {
            let channel_id = channel_id
                .parse::<u16>()
                .map_err(|_| "channel id must be an integer from 0 through 65535".to_owned())?;
            rename_channel(Path::new(input), Path::new(output), channel_id, name)
        }
        [command, input, output, channel_id, volume, pan] if command == "set-channel-levels" => {
            set_channel_levels(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_u32(volume, "channel volume")?,
                i32::try_from(parse_u32(pan, "channel pan")?)
                    .map_err(|_| "channel pan must be between 0 and 12800".to_owned())?,
            )
        }
        [command, input, output, channel_id, child_ids] if command == "set-layer-children" => {
            set_layer_children(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "Layer channel id")?,
                parse_channel_id_list(child_ids)?,
            )
        }
        [command, input, output, channel_id, point_index, position, value, tension]
            if command == "edit-automation-point" =>
        {
            edit_automation_point(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_usize(point_index, "automation point index")?,
                parse_f64(position, "automation point position")?,
                parse_f64(value, "automation point value")?,
                parse_f32(tension, "automation point tension")?,
            )
        }
        [command, input, output, channel_id, point_index, position, value, tension]
            if command == "insert-automation-point" =>
        {
            insert_automation_point(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_usize(point_index, "automation insertion slot")?,
                parse_f64(position, "automation point position")?,
                parse_f64(value, "automation point value")?,
                parse_f32(tension, "automation point tension")?,
            )
        }
        [command, input, output, channel_id, point_index]
            if command == "delete-automation-point" =>
        {
            delete_automation_point(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_usize(point_index, "automation point index")?,
            )
        }
        [command, input, output, pattern_id, channel_id, note_index, position, length, key, velocity]
            if command == "edit-note" =>
        {
            edit_note(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                parse_usize(note_index, "channel note index")?,
                PatternNoteEdit {
                    position: Some(parse_u32(position, "note position")?),
                    length: Some(parse_u32(length, "note length")?),
                    key: Some(parse_u16(key, "note key")?),
                    velocity: Some(parse_u8(velocity, "note velocity")?),
                    ..PatternNoteEdit::default()
                },
            )
        }
        [command, input, output, pattern_id, channel_id, position, length, key, velocity]
            if command == "add-note" =>
        {
            add_note(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                PatternNote {
                    channel_id: parse_u16(channel_id, "channel id")?,
                    position: parse_u32(position, "note position")?,
                    length: parse_u32(length, "note length")?,
                    key: parse_u16(key, "note key")?,
                    velocity: parse_u8(velocity, "note velocity")?,
                    ..PatternNote::default()
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_index] if command == "delete-note" => {
            delete_note(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                parse_usize(note_index, "channel note index")?,
            )
        }
        [command, input, output, arrangement_id, clip_index, position, length]
            if command == "edit-clip" =>
        {
            edit_clip(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
                parse_u32(position, "clip position")?,
                parse_u32(length, "clip length")?,
            )
        }
        [command, input, midi_path, output, track, pattern_id, channel_id]
            if command == "import-midi" =>
        {
            import_midi_track(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_usize(track, "MIDI track number")?,
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
            )
        }
        _ => Err(concat!(
            "usage:\n",
            "  flp-rebuild info <file.flp>\n",
            "  flp-rebuild midi-info <file.mid>\n",
            "  flp-rebuild midi-events <file.mid> <track> [start] [count]\n",
            "  flp-rebuild scan <directory>\n",
            "  flp-rebuild plugin-scan\n",
            "  flp-rebuild render-pattern-vst3 <project.flp> <pattern-id> <channel-id> <plugin.vst3> <output.wav> [tail-seconds]\n",
            "  flp-rebuild channels <file.flp>\n",
            "  flp-rebuild mixer <file.flp>\n",
            "  flp-rebuild automation <file.flp>\n",
            "  flp-rebuild time-markers <file.flp>\n",
            "  flp-rebuild sample-paths <file.flp>\n",
            "  flp-rebuild audio-info <audio-file>\n",
            "  flp-rebuild render-audio-clips <project.flp> <output.wav> [arrangement-id]\n",
            "  flp-rebuild plugin-states <file.flp>\n",
            "  flp-rebuild channel-events <file.flp> <channel-id>\n",
            "  flp-rebuild patterns <file.flp>\n",
            "  flp-rebuild playlist <file.flp> [start] [count]\n",
            "  flp-rebuild notes <file.flp> <pattern-id> [start] [count]\n",
            "  flp-rebuild events <file.flp> [start] [count]\n",
            "  flp-rebuild extract-event <file.flp> <index> <output.bin>\n",
            "  flp-rebuild roundtrip <input.flp> <output.flp>\n",
            "  flp-rebuild set-tempo <input.flp> <output.flp> <bpm>\n",
            "  flp-rebuild rename-channel <input.flp> <output.flp> <channel-id> <name>\n",
            "  flp-rebuild set-channel-levels <input.flp> <output.flp> <channel-id> <volume-0..12800> <pan-0..12800>\n",
            "  flp-rebuild set-layer-children <input.flp> <output.flp> <layer-channel-id> <child-ids-comma-separated|->\n",
            "  flp-rebuild edit-automation-point <input.flp> <output.flp> <channel-id> <point-index> <position-beats> <value> <tension>\n",
            "  flp-rebuild insert-automation-point <input.flp> <output.flp> <channel-id> <insertion-slot> <position-beats> <value> <tension>\n",
            "  flp-rebuild delete-automation-point <input.flp> <output.flp> <channel-id> <point-index>\n",
            "  flp-rebuild edit-note <input.flp> <output.flp> <pattern-id> <channel-id> <note-index> <position> <length> <key> <velocity>\n",
            "  flp-rebuild add-note <input.flp> <output.flp> <pattern-id> <channel-id> <position> <length> <key> <velocity>\n",
            "  flp-rebuild delete-note <input.flp> <output.flp> <pattern-id> <channel-id> <note-index>\n",
            "  flp-rebuild import-midi <input.flp> <input.mid> <output.flp> <track> <pattern-id> <channel-id>\n",
            "  flp-rebuild edit-clip <input.flp> <output.flp> <arrangement-id> <clip-index> <position> <length>"
        )
        .to_owned()),
    }
}

fn parse_usize(value: &str, description: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|_| format!("{description} must be a non-negative integer"))
}

fn parse_u8(value: &str, description: &str) -> Result<u8, String> {
    value
        .parse::<u8>()
        .map_err(|_| format!("{description} must be an integer from 0 through 255"))
}

fn parse_u16(value: &str, description: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .map_err(|_| format!("{description} must be an integer from 0 through 65535"))
}

fn parse_channel_id_list(value: &str) -> Result<Vec<u16>, String> {
    if value == "-" {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|child_id| {
            if child_id.is_empty() {
                return Err("child channel IDs must be comma-separated integers".to_owned());
            }
            parse_u16(child_id, "child channel id")
        })
        .collect()
}

fn parse_u32(value: &str, description: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|_| format!("{description} must be a non-negative 32-bit integer"))
}

fn parse_f64(value: &str, description: &str) -> Result<f64, String> {
    let parsed = value
        .parse::<f64>()
        .map_err(|_| format!("{description} must be a finite number"))?;
    if !parsed.is_finite() {
        return Err(format!("{description} must be a finite number"));
    }
    Ok(parsed)
}

fn parse_f32(value: &str, description: &str) -> Result<f32, String> {
    let parsed = value
        .parse::<f32>()
        .map_err(|_| format!("{description} must be a finite number"))?;
    if !parsed.is_finite() {
        return Err(format!("{description} must be a finite number"));
    }
    Ok(parsed)
}

fn parse_tempo_milli_bpm(value: &str) -> Result<u32, String> {
    let bpm = value
        .parse::<f64>()
        .map_err(|_| "tempo must be a positive number".to_owned())?;
    if !bpm.is_finite() || bpm <= 0.0 {
        return Err("tempo must be a positive finite number".to_owned());
    }
    let milli_bpm = (bpm * 1000.0).round();
    if milli_bpm > f64::from(u32::MAX) {
        return Err("tempo is too large to store".to_owned());
    }
    Ok(milli_bpm as u32)
}

fn load_document(path: &Path) -> Result<(Vec<u8>, FlpDocument), String> {
    let bytes =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let document = FlpDocument::parse(&bytes)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    Ok((bytes, document))
}

fn render_pattern_vst3(
    project_path: &Path,
    pattern_id: u16,
    channel_id: u16,
    bundle_path: &Path,
    output_path: &Path,
    tail_seconds: f64,
) -> Result<(), String> {
    let (_, document) = load_document(project_path)?;
    let pattern = document
        .patterns()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|pattern| pattern.id == pattern_id)
        .ok_or_else(|| format!("pattern id {pattern_id} was not found"))?;
    if !document
        .channels()
        .iter()
        .any(|channel| channel.id() == channel_id)
    {
        return Err(format!("channel id {channel_id} was not found"));
    }

    let class_uid = matching_project_class_uid(&document, channel_id, bundle_path);
    let mut host = Vst3HostRuntime::new(48_000.0, 512)?;
    let plugin = host.load(bundle_path, class_uid.as_deref())?;
    let summary = host.render_pattern_channel_to_wav(
        plugin.id,
        &pattern.notes,
        Vst3PatternRenderOptions {
            channel_id,
            ppq: document.header().ppq(),
            tempo_bpm: document.metadata().tempo_bpm().unwrap_or(120.0),
            tail_seconds,
        },
        output_path,
    )?;
    println!(
        "rendered pattern {} channel {} through {} to {}: {} notes, {:.2} seconds, {} Hz, {} channels",
        pattern_id,
        channel_id,
        plugin.name,
        output_path.display(),
        summary.notes_rendered,
        summary.frames as f64 / f64::from(summary.sample_rate),
        summary.sample_rate,
        summary.output_channels,
    );
    println!(
        "render uses the plug-in's initial state; FLP plug-in state, Playlist clips, Mixer routing, and effects are not applied"
    );
    Ok(())
}

fn matching_project_class_uid(
    document: &FlpDocument,
    channel_id: u16,
    bundle_path: &Path,
) -> Option<String> {
    let selected_bundle_name = bundle_path
        .file_stem()
        .or_else(|| bundle_path.file_name())?
        .to_string_lossy();
    document
        .channel_plugin_states()
        .into_iter()
        .find(|state| state.channel_id() == channel_id)
        .and_then(|state| {
            let metadata = state.vst_metadata()?;
            let path_matches = metadata.path().is_some_and(|path| {
                Path::new(path)
                    .file_stem()
                    .or_else(|| Path::new(path).file_name())
                    .is_some_and(|name| {
                        name.to_string_lossy()
                            .eq_ignore_ascii_case(&selected_bundle_name)
                    })
            });
            let name_matches = metadata
                .name()
                .is_some_and(|name| name.eq_ignore_ascii_case(&selected_bundle_name));
            (path_matches || name_matches)
                .then(|| metadata.class_uid())
                .flatten()
        })
}

fn inspect(path: &Path) -> Result<(), String> {
    let (bytes, document) = load_document(path)?;
    let header = document.header();
    println!("file: {}", path.display());
    println!("size: {} bytes", bytes.len());
    println!("format: {}", header.format());
    println!("legacy channel count: {}", header.legacy_channel_count());
    println!("PPQ: {}", header.ppq());
    println!(
        "project version: {}",
        document.project_version().unwrap_or("unknown")
    );
    if let Some(bpm) = document.metadata().tempo_bpm() {
        println!("tempo: {bpm:.3} BPM");
    }
    if let Some((numerator, denominator)) = document.metadata().time_signature() {
        println!("time signature: {numerator}/{denominator}");
    }
    if let Some(build) = document.metadata().build_number() {
        println!("writer build: {build}");
    }
    println!("events: {}", document.events().len());
    println!("channel summaries: {}", document.channels().len());
    println!("trailing bytes: {}", document.trailing_bytes().len());

    let counts = document.opcode_counts();
    let mut present: Vec<(usize, usize)> = counts
        .iter()
        .enumerate()
        .filter_map(|(opcode, count)| (*count > 0).then_some((opcode, *count)))
        .collect();
    present.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    println!("opcode counts:");
    for (opcode, count) in present.into_iter().take(24) {
        println!("  0x{opcode:02X}: {count}");
    }
    Ok(())
}

fn inspect_midi(path: &Path) -> Result<(), String> {
    let bytes =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let midi = MidiFile::parse(&bytes)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    println!("file: {}", path.display());
    println!("size: {} bytes", bytes.len());
    println!("format: {}", midi.format());
    println!("tracks: {}", midi.tracks().len());
    if let Some(ppq) = midi.ticks_per_quarter_note() {
        println!("time division: {ppq} ticks per quarter note");
    } else {
        println!("time division: SMPTE 0x{:04X}", midi.division());
    }
    for (index, track) in midi.tracks().iter().enumerate() {
        let (note_on, note_off) = track.note_event_counts();
        let notes = track.notes();
        let open_notes = notes
            .iter()
            .filter(|note| note.end_tick().is_none())
            .count();
        let tempos = track.tempo_events();
        let valid_tempos: Vec<_> = tempos
            .iter()
            .filter(|tempo| tempo.microseconds_per_quarter() > 0)
            .collect();
        let fastest = valid_tempos
            .iter()
            .copied()
            .min_by_key(|tempo| tempo.microseconds_per_quarter());
        let slowest = valid_tempos
            .iter()
            .copied()
            .max_by_key(|tempo| tempo.microseconds_per_quarter());
        let tempo_range = match (slowest, fastest) {
            (Some(slowest), Some(fastest)) => format!(
                " range={:.3}..{:.3} BPM (slowest@{}, fastest@{})",
                slowest.bpm(),
                fastest.bpm(),
                slowest.tick(),
                fastest.tick()
            ),
            _ => String::new(),
        };
        let tempo_summary = match (tempos.first(), tempos.last()) {
            (Some(first), Some(_)) if tempos.len() == 1 => {
                format!(
                    " tempo={:.3} BPM@{}{}",
                    first.bpm(),
                    first.tick(),
                    tempo_range
                )
            }
            (Some(first), Some(last)) => format!(
                " tempo_events={} first={:.3} BPM@{} last={:.3} BPM@{}{}",
                tempos.len(),
                first.bpm(),
                first.tick(),
                last.bpm(),
                last.tick(),
                tempo_range
            ),
            _ => String::new(),
        };
        println!(
            "track {}: name={} events={} end_tick={} notes={} open_notes={} note_on={} note_off={}{}",
            index + 1,
            track.name().unwrap_or_else(|| "(unnamed)".to_owned()),
            track.events().len(),
            track.end_tick(),
            notes.len(),
            open_notes,
            note_on,
            note_off,
            tempo_summary,
        );
    }
    Ok(())
}

fn dump_midi_events(
    path: &Path,
    track_number: usize,
    start: usize,
    count: usize,
) -> Result<(), String> {
    if track_number == 0 {
        return Err("track number is one-based and must be at least 1".to_owned());
    }
    let bytes =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let midi = MidiFile::parse(&bytes)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    let Some(track) = midi.tracks().get(track_number - 1) else {
        return Err(format!(
            "track {track_number} is past the end of the {}-track file",
            midi.tracks().len()
        ));
    };
    if start >= track.events().len() {
        return Err(format!(
            "event start {start} is past the end of the {}-event track",
            track.events().len()
        ));
    }
    let end = start.saturating_add(count).min(track.events().len());
    for (index, event) in track.events()[start..end].iter().enumerate() {
        let (kind, data) = match event.kind() {
            flp_rebuild::midi::MidiEventKind::ChannelVoice { status, data } => {
                (format!("channel status=0x{status:02X}"), data.as_slice())
            }
            flp_rebuild::midi::MidiEventKind::Meta { meta_type, data } => {
                (format!("meta type=0x{meta_type:02X}"), data.as_slice())
            }
            flp_rebuild::midi::MidiEventKind::SysEx { status, data } => {
                (format!("sysex status=0x{status:02X}"), data.as_slice())
            }
            flp_rebuild::midi::MidiEventKind::System { status, data } => {
                (format!("system status=0x{status:02X}"), data.as_slice())
            }
        };
        let preview = data
            .iter()
            .take(32)
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
        let truncated = data.len() > 32;
        println!(
            "{:06} delta={} tick={} {} bytes={} [{}{}]",
            start + index,
            event.delta_ticks(),
            event.absolute_tick(),
            kind,
            data.len(),
            preview,
            if truncated { " …" } else { "" }
        );
    }
    Ok(())
}

fn scan_corpus(root: &Path) -> Result<(), String> {
    let mut paths = Vec::new();
    collect_project_paths(root, &mut paths)?;
    paths.sort();

    let mut parsed = 0usize;
    let mut failures = Vec::new();
    let mut versions = BTreeMap::<(String, String), usize>::new();
    let mut formats = BTreeMap::<u16, usize>::new();
    let mut ppq_values = BTreeMap::<u16, usize>::new();

    for path in &paths {
        match fs::read(path) {
            Ok(bytes) => match FlpDocument::parse(&bytes) {
                Ok(document) => {
                    parsed += 1;
                    let extension = path
                        .extension()
                        .and_then(|value| value.to_str())
                        .unwrap_or("unknown")
                        .to_ascii_lowercase();
                    *versions
                        .entry((
                            extension,
                            document.project_version().unwrap_or("unknown").to_owned(),
                        ))
                        .or_default() += 1;
                    *formats.entry(document.header().format()).or_default() += 1;
                    *ppq_values.entry(document.header().ppq()).or_default() += 1;
                }
                Err(error) => failures.push((path.clone(), error.to_string())),
            },
            Err(error) => failures.push((path.clone(), format!("read failed: {error}"))),
        }
    }

    println!("files: {}", paths.len());
    println!("parsed: {parsed}");
    println!("failed: {}", failures.len());
    println!("project versions:");
    for ((extension, version), count) in versions {
        println!("  .{extension} {version}: {count}");
    }
    println!("header formats:");
    for (format, count) in formats {
        println!("  {format}: {count}");
    }
    println!("PPQ values:");
    for (ppq, count) in ppq_values {
        println!("  {ppq}: {count}");
    }
    if !failures.is_empty() {
        println!("parse/read failures (first 40):");
        for (path, error) in failures.iter().take(40) {
            println!("  {}: {error}", path.display());
        }
    }
    Ok(())
}

fn scan_plugins() -> Result<(), String> {
    let report = scan_installed_plugins();
    println!("plug-in candidates: {}", report.candidates.len());
    println!("search roots:");
    for root in &report.search_roots {
        println!("  {}", root.display());
    }
    for candidate in report.candidates {
        println!(
            "{} name={} path={}",
            candidate.format,
            candidate.name,
            candidate.path.display()
        );
    }
    if !report.scan_errors.is_empty() {
        println!("scan warnings: {}", report.scan_errors.len());
        for (path, error) in report.scan_errors.iter().take(20) {
            println!("  {}: {error}", path.display());
        }
    }
    Ok(())
}

fn collect_project_paths(root: &Path, paths: &mut Vec<std::path::PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(root)
        .map_err(|error| format!("could not list {}: {error}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("could not read directory entry: {error}"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
        if file_type.is_dir() {
            collect_project_paths(&path, paths)?;
        } else if file_type.is_file()
            && path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("flp") || extension.eq_ignore_ascii_case("fst")
                })
        {
            paths.push(path);
        }
    }
    Ok(())
}

fn dump_events(path: &Path, start: usize, count: usize) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    if start >= document.events().len() {
        return Err(format!(
            "event start {start} is past the end of the {}-event stream",
            document.events().len()
        ));
    }
    let end = start.saturating_add(count).min(document.events().len());
    for (index, event) in document.events()[start..end].iter().enumerate() {
        let preview = event
            .payload()
            .iter()
            .take(24)
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
        let truncated = event.payload().len() > 24;
        println!(
            "{:06} @{:08X} opcode=0x{:02X} encoding={:?} payload={} [{}{}]",
            start + index,
            event.file_offset(),
            event.opcode(),
            event.encoding(),
            event.payload().len(),
            preview,
            if truncated { " …" } else { "" }
        );
    }
    Ok(())
}

fn extract_event(input: &Path, event_index: usize, output: &Path) -> Result<(), String> {
    let (_, document) = load_document(input)?;
    let Some(event) = document.events().get(event_index) else {
        return Err(format!(
            "event index {event_index} is past the end of the {}-event stream",
            document.events().len()
        ));
    };
    fs::write(output, event.payload())
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "extracted {} payload bytes from event {} (opcode 0x{:02X}) to {}",
        event.payload().len(),
        event_index,
        event.opcode(),
        output.display()
    );
    Ok(())
}

fn list_channels(path: &Path) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let channels = document.channels();
    println!("channels: {}", channels.len());
    for channel in channels {
        println!(
            "id={} kind={} type={:?} enabled={} volume={:?} pan={:?} layer_children={:?} layer_flags={:?} plugin={} name={} sample_path={:?} events={:?}",
            channel.id(),
            channel
                .kind()
                .map_or_else(|| "unknown".to_owned(), |kind| kind.to_string()),
            channel.channel_type(),
            channel
                .enabled()
                .map_or_else(|| "unknown".to_owned(), |enabled| enabled.to_string()),
            channel.volume(),
            channel.pan(),
            channel.layer_child_ids(),
            channel.layer_flags(),
            channel.plugin_identifier().unwrap_or("unknown"),
            channel.display_name().unwrap_or("(unnamed)"),
            channel.sample_path(),
            channel.event_range()
        );
    }
    Ok(())
}

fn list_mixer(path: &Path) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let inserts = document.mixer_inserts();
    let parameters = document
        .mixer_parameter_records()
        .map_err(|error| format!("could not decode Mixer parameter records: {error}"))?;
    println!("recognized insert records: {}", inserts.len());
    for insert in inserts {
        println!(
            "ordinal={} name={} input_raw={} output_raw={} color_raw=0x{:08X} icon_raw={:?} events={:?}",
            insert.ordinal(),
            insert.name().unwrap_or("(unnamed)"),
            insert.input_raw(),
            insert.output_raw(),
            insert.color_raw(),
            insert.icon_raw(),
            insert.event_range()
        );
    }
    let count_kind = |kind| {
        parameters
            .iter()
            .filter(|record| record.kind() == kind)
            .count()
    };
    println!(
        "0xE1 records: {} (volume={}, pan={}, stereo_separation={}, route_volume={})",
        parameters.len(),
        count_kind(flp_rebuild::MixerParameterKind::Volume),
        count_kind(flp_rebuild::MixerParameterKind::Pan),
        count_kind(flp_rebuild::MixerParameterKind::StereoSeparation),
        count_kind(flp_rebuild::MixerParameterKind::RouteVolume),
    );
    Ok(())
}

fn list_automation(path: &Path) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let channels = document
        .automation_channels()
        .map_err(|error| format!("could not decode automation points: {error}"))?;
    println!("automation channels: {}", channels.len());
    for channel in channels {
        println!(
            "channel={} name={} points={} event={:?}",
            channel.channel_id(),
            channel.display_name().unwrap_or("(unnamed)"),
            channel.points().len(),
            channel.data_event_index()
        );
        for (index, point) in channel.points().iter().enumerate() {
            println!(
                "  point={} position_beats={:.9} value={:.9} tension={:.7} tail={:02X?}",
                index,
                point.position_beats(),
                point.value(),
                point.tension(),
                point.trailing_bytes()
            );
        }
    }
    Ok(())
}

fn list_sample_paths(project_path: &Path) -> Result<(), String> {
    let (_, document) = load_document(project_path)?;
    let resolver = SamplePathResolver::new(project_path);
    let mut sample_count = 0usize;
    for channel in document.channels() {
        let Some(sample_path) = channel.sample_path() else {
            continue;
        };
        sample_count += 1;
        match resolver.resolve(sample_path) {
            Ok(resolved) => println!(
                "channel={} name={} source={:?} resolved={}",
                channel.id(),
                channel.display_name().unwrap_or("(unnamed)"),
                sample_path,
                resolved.display()
            ),
            Err(error) => println!(
                "channel={} name={} source={:?} unresolved={error}",
                channel.id(),
                channel.display_name().unwrap_or("(unnamed)"),
                sample_path
            ),
        }
    }
    println!("sample references: {sample_count}");
    Ok(())
}

fn inspect_audio_file(path: &Path) -> Result<(), String> {
    let audio = decode_audio_file(path)?;
    println!("file: {}", path.display());
    println!("sample rate: {} Hz", audio.sample_rate);
    println!("channels: {}", audio.channels.len());
    println!("frames: {}", audio.frame_count());
    println!("duration: {:.3} seconds", audio.duration_seconds());
    Ok(())
}

fn render_audio_clips(
    project_path: &Path,
    output_path: &Path,
    options: AudioClipRenderOptions,
) -> Result<(), String> {
    let (_, document) = load_document(project_path)?;
    let summary = render_audio_clips_to_wav(&document, project_path, options, output_path)?;
    println!(
        "rendered {} Playlist audio clips from {} source files to {} ({} Hz stereo, {} frames; skipped {} scaled clips)",
        summary.clips_rendered,
        summary.source_files,
        output_path.display(),
        summary.sample_rate,
        summary.frames,
        summary.clips_skipped_unsupported_scale,
    );
    Ok(())
}

fn list_channel_events(path: &Path, channel_id: u16) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let channel = document
        .channels()
        .into_iter()
        .find(|channel| channel.id() == channel_id)
        .ok_or_else(|| format!("channel id {channel_id} was not found"))?;
    println!(
        "channel id={} plugin={} name={} events={:?}",
        channel.id(),
        channel.plugin_identifier().unwrap_or("unknown"),
        channel.display_name().unwrap_or("(unnamed)"),
        channel.event_range()
    );
    for index in channel.event_range() {
        let event = &document.events()[index];
        let preview = event
            .payload()
            .iter()
            .take(32)
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "{:06} opcode=0x{:02X} encoding={:?} payload={} [{}{}]",
            index,
            event.opcode(),
            event.encoding(),
            event.payload().len(),
            preview,
            if event.payload().len() > 32 {
                " …"
            } else {
                ""
            }
        );
    }
    Ok(())
}

fn list_plugin_states(path: &Path) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let states = document.channel_plugin_states();
    println!("channel plug-in data records: {}", states.len());
    for state in states {
        let vst = state.vst_metadata();
        println!(
            "channel={} plugin={} channel_name={} vst_name={} vendor={} class_uid={} path={} data_event={} data_bytes={} nested_state_bytes={} wrapper_bytes={}",
            state.channel_id(),
            state.plugin_identifier().unwrap_or("unknown"),
            state.display_name().unwrap_or("(unnamed)"),
            vst.and_then(|metadata| metadata.name())
                .unwrap_or("unknown"),
            vst.and_then(|metadata| metadata.vendor())
                .unwrap_or("unknown"),
            vst.and_then(|metadata| metadata.class_uid())
                .unwrap_or_else(|| "unknown".to_owned()),
            vst.and_then(|metadata| metadata.path())
                .unwrap_or("unknown"),
            state.data_event_index(),
            state.data_payload().len(),
            vst.and_then(|metadata| metadata.state_bytes())
                .map_or_else(|| "unknown".to_owned(), |bytes| bytes.to_string()),
            state
                .wrapper_payload()
                .map_or_else(|| "none".to_owned(), |bytes| bytes.len().to_string()),
        );
    }
    Ok(())
}

fn preview_plugin_state(path: &Path, channel_id: u16, prefix_len: usize) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let state = document
        .channel_plugin_states()
        .into_iter()
        .find(|state| state.channel_id() == channel_id)
        .ok_or_else(|| format!("channel {channel_id} has no recognized 0xD5 state event"))?;
    let prefix = |bytes: &[u8]| {
        bytes
            .iter()
            .take(prefix_len)
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    println!(
        "channel={} wrapper_bytes={} wrapper_prefix=[{}] data_bytes={} data_prefix=[{}]",
        state.channel_id(),
        state.wrapper_payload().map_or(0, <[u8]>::len),
        state.wrapper_payload().map_or_else(String::new, prefix),
        state.data_payload().len(),
        prefix(state.data_payload()),
    );
    Ok(())
}

fn probe_vst3_state(bundle: &Path, project: &Path, channel_id: u16) -> Result<(), String> {
    let (_, document) = load_document(project)?;
    let state = document
        .channel_plugin_states()
        .into_iter()
        .find(|state| state.channel_id() == channel_id)
        .ok_or_else(|| format!("channel {channel_id} has no recognized 0xD5 state event"))?;
    let class_uid = state
        .vst_metadata()
        .and_then(|metadata| metadata.class_uid());
    let mut host = Vst3HostRuntime::new(44_100.0, 512)?;
    let info = host.load(bundle, class_uid.as_deref())?;
    println!(
        "loaded {} by {} class={} path={}",
        info.name,
        info.vendor,
        info.uid,
        info.path.display()
    );
    match host.restore_state(info.id, state.data_payload()) {
        Ok(()) => {
            let snapshot = host.save_state(info.id)?;
            println!(
                "state restore accepted: FLP event bytes={} nested state bytes={} host snapshot bytes={}",
                state.data_payload().len(),
                state
                    .vst_metadata()
                    .and_then(|metadata| metadata.state_bytes())
                    .map_or_else(|| "unknown".to_owned(), |bytes| bytes.to_string()),
                snapshot.len()
            );
        }
        Err(error) => println!(
            "state restore rejected: FLP event bytes={} nested state bytes={} error={error}",
            state.data_payload().len(),
            state
                .vst_metadata()
                .and_then(|metadata| metadata.state_bytes())
                .map_or_else(|| "unknown".to_owned(), |bytes| bytes.to_string())
        ),
    }
    Ok(())
}

fn list_patterns(path: &Path) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let patterns = document.patterns().map_err(|error| error.to_string())?;
    println!("patterns: {}", patterns.len());
    for pattern in patterns {
        let mut channels = BTreeMap::<u16, usize>::new();
        for note in &pattern.notes {
            *channels.entry(note.channel_id).or_default() += 1;
        }
        let channel_summary = channels
            .iter()
            .map(|(channel_id, count)| format!("{channel_id}:{count}"))
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "id={} name={} length_ticks={} notes={} channels=[{}]",
            pattern.id,
            pattern.name.as_deref().unwrap_or("(unnamed)"),
            pattern
                .length_ticks
                .map_or_else(|| "default".to_owned(), |length| length.to_string()),
            pattern.notes.len(),
            channel_summary,
        );
    }
    Ok(())
}

fn list_playlist(path: &Path, start: usize, count: usize) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let playlist_tracks = document.playlist_tracks();
    let channels = document.channels();
    let arrangements = document.arrangements().map_err(|error| error.to_string())?;
    println!("arrangements: {}", arrangements.len());
    for arrangement in arrangements {
        println!(
            "arrangement id={} name={} clips={} showing={}:{}",
            arrangement.id,
            arrangement.name.as_deref().unwrap_or("(unnamed)"),
            arrangement.clips.len(),
            start.min(arrangement.clips.len()),
            start.saturating_add(count).min(arrangement.clips.len()),
        );
        let end = start.saturating_add(count).min(arrangement.clips.len());
        for (index, clip) in arrangement
            .clips
            .iter()
            .enumerate()
            .take(end)
            .skip(start.min(arrangement.clips.len()))
        {
            let target = clip.target();
            let target_name = match target {
                flp_rebuild::PlaylistClipTarget::Channel { id } => channels
                    .iter()
                    .find(|channel| channel.id() == id)
                    .and_then(|channel| channel.display_name())
                    .unwrap_or("(channel not found)"),
                flp_rebuild::PlaylistClipTarget::Pattern { .. } => "(pattern)",
            };
            println!(
                "  clip={} position={} length={} track={} track_name={:?} raw_track={} target={:?} target_name={:?} group={} flags=0x{:04X} offsets={:.6}..{:.6} scale={:?} clip_id={} record_size={}",
                index,
                clip.position_ticks,
                clip.length_ticks,
                clip.track_index
                    .map_or_else(|| "unknown".to_owned(), |track| track.to_string()),
                clip.playlist_track_id()
                    .and_then(|id| playlist_tracks.iter().find(|track| track.id == id))
                    .and_then(|track| track.name.as_deref())
                    .unwrap_or("(unnamed)"),
                clip.raw_track_index,
                target,
                target_name,
                clip.group,
                clip.item_flags,
                clip.start_offset,
                clip.end_offset,
                clip.scale,
                clip.clip_id
                    .map_or_else(|| "none".to_owned(), |id| id.to_string()),
                clip.record_size,
            );
        }
    }
    Ok(())
}

fn list_time_markers(path: &Path) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let markers = document.time_markers().map_err(|error| error.to_string())?;
    println!("time markers: {}", markers.len());
    for (index, (arrangement_id, marker)) in markers.iter().enumerate() {
        println!(
            "arrangement={} marker={} ticks={} raw=0x{:08X} signature={} meter={}/{} name={:?}",
            arrangement_id,
            index,
            marker.position_ticks(),
            marker.raw_position(),
            marker.is_signature(),
            marker
                .numerator()
                .map_or_else(|| "?".to_owned(), |value| value.to_string()),
            marker
                .denominator()
                .map_or_else(|| "?".to_owned(), |value| value.to_string()),
            marker.name().unwrap_or(""),
        );
    }
    Ok(())
}

fn list_notes(path: &Path, pattern_id: u16, start: usize, count: usize) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let patterns = document.patterns().map_err(|error| error.to_string())?;
    let Some(pattern) = patterns.iter().find(|pattern| pattern.id == pattern_id) else {
        return Err(format!("pattern id {pattern_id} was not found"));
    };
    if start >= pattern.notes.len() {
        return Err(format!(
            "note start {start} is past the end of the {}-note pattern",
            pattern.notes.len()
        ));
    }
    let end = start.saturating_add(count).min(pattern.notes.len());
    let mut channel_note_indices = BTreeMap::<u16, usize>::new();
    for (index, note) in pattern.notes.iter().enumerate().take(end) {
        let channel_note_index = channel_note_indices.entry(note.channel_id).or_default();
        if index >= start {
            println!(
                "{:06} channel={} channel_note_index={} position={} length={} key={} velocity={} pan={} release={} flags=0x{:04X} group={} fine_pitch={} midi_channel={} mod_x={} mod_y={}",
                index,
                note.channel_id,
                channel_note_index,
                note.position,
                note.length,
                note.key,
                note.velocity,
                note.pan,
                note.release,
                note.flags,
                note.group,
                note.fine_pitch,
                note.midi_channel,
                note.mod_x,
                note.mod_y,
            );
        }
        *channel_note_index += 1;
    }
    Ok(())
}

fn roundtrip(input: &Path, output: &Path) -> Result<(), String> {
    let (original, document) = load_document(input)?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    if bytes != original {
        return Err(format!(
            "lossless round-trip changed bytes in {}",
            input.display()
        ));
    }
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!("wrote byte-identical FLP copy to {}", output.display());
    Ok(())
}

fn set_tempo(input: &Path, output: &Path, milli_bpm: u32) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_tempo_milli_bpm(milli_bpm)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "wrote project with tempo {:.3} BPM to {}",
        f64::from(milli_bpm) / 1000.0,
        output.display()
    );
    Ok(())
}

fn rename_channel(input: &Path, output: &Path, channel_id: u16, name: &str) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_channel_name(channel_id, name)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "renamed channel {channel_id} to {name:?} in {}",
        output.display()
    );
    Ok(())
}

fn set_channel_levels(
    input: &Path,
    output: &Path,
    channel_id: u16,
    volume: u32,
    pan: i32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_channel_levels(channel_id, volume, pan)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set channel {channel_id} volume to {volume} and pan to {pan} in {}",
        output.display()
    );
    Ok(())
}

fn set_layer_children(
    input: &Path,
    output: &Path,
    layer_channel_id: u16,
    child_channel_ids: Vec<u16>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_layer_child_ids(layer_channel_id, &child_channel_ids)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Layer channel {layer_channel_id} children to {child_channel_ids:?} in {}",
        output.display()
    );
    Ok(())
}

fn edit_automation_point(
    input: &Path,
    output: &Path,
    channel_id: u16,
    point_index: usize,
    position_beats: f64,
    value: f64,
    tension: f32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_automation_point(
            channel_id,
            point_index,
            AutomationPointEdit {
                position_beats: Some(position_beats),
                value: Some(value),
                tension: Some(tension),
            },
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "edited automation channel {channel_id} point {point_index} in {}",
        output.display()
    );
    Ok(())
}

fn insert_automation_point(
    input: &Path,
    output: &Path,
    channel_id: u16,
    point_index: usize,
    position_beats: f64,
    value: f64,
    tension: f32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .insert_automation_point(channel_id, point_index, position_beats, value, tension)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "inserted automation point at slot {point_index} in channel {channel_id} in {}",
        output.display()
    );
    Ok(())
}

fn delete_automation_point(
    input: &Path,
    output: &Path,
    channel_id: u16,
    point_index: usize,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .delete_automation_point(channel_id, point_index)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "deleted automation point {point_index} from channel {channel_id} in {}",
        output.display()
    );
    Ok(())
}

fn edit_note(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_index: usize,
    edit: PatternNoteEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_pattern_note(pattern_id, channel_id, note_index, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "edited note {note_index} in pattern {pattern_id}, channel {channel_id} to {}",
        output.display()
    );
    Ok(())
}

fn add_note(input: &Path, output: &Path, pattern_id: u16, note: PatternNote) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .add_pattern_note(pattern_id, note.clone())
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "added note to pattern {pattern_id}, channel {} at {} ticks to {}",
        note.channel_id,
        note.position,
        output.display()
    );
    Ok(())
}

fn delete_note(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_index: usize,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .delete_pattern_note(pattern_id, channel_id, note_index)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "deleted note {note_index} from pattern {pattern_id}, channel {channel_id} to {}",
        output.display()
    );
    Ok(())
}

fn import_midi_track(
    input: &Path,
    midi_path: &Path,
    output: &Path,
    track_index: usize,
    pattern_id: u16,
    channel_id: u16,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let midi_bytes = fs::read(midi_path)
        .map_err(|error| format!("could not read {}: {error}", midi_path.display()))?;
    let midi = MidiFile::parse(&midi_bytes)
        .map_err(|error| format!("could not parse {}: {error}", midi_path.display()))?;
    let imported = document
        .import_midi_track(&midi, track_index, pattern_id, channel_id)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "imported {imported} MIDI notes from track {track_index} into pattern {pattern_id}, channel {channel_id} in {}",
        output.display()
    );
    Ok(())
}

fn edit_clip(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_index: usize,
    position: u32,
    length: u32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_playlist_clip(
            arrangement_id,
            clip_index,
            PlaylistClipEdit {
                position_ticks: Some(position),
                length_ticks: Some(length),
                ..PlaylistClipEdit::default()
            },
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "wrote arrangement {arrangement_id} clip {clip_index} with position {position} and length {length} to {}",
        output.display()
    );
    Ok(())
}
