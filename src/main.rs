use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use flp_rebuild::media::{SamplePathResolver, decode_audio_file};
use flp_rebuild::midi::{MidiChannelMapping, MidiFile};
use flp_rebuild::plugins::scan_installed_plugins;
use flp_rebuild::sample_render::{
    AudioClipRenderOptions, SamplerPatternRenderOptions, render_audio_clips_to_wav,
    render_sampler_pattern_to_wav,
};
use flp_rebuild::vst3::{Vst3HostRuntime, Vst3PatternRenderOptions};
use flp_rebuild::{
    ArpeggioDirection, ArpeggioOptions, AutomationPointEdit, FlpDocument, FstPreset,
    PatternControllerEdit, PatternNote, PatternNoteEdit, PlaylistClipEdit, PlaylistTrackEdit,
    ProjectInfoEdit, ProjectSettingsEdit, RandomizerOptions, TimeMarkerEdit,
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
        [command, path] if command == "preset-info" => inspect_preset(Path::new(path)),
        [command, path] if command == "project-info" => show_project_info(Path::new(path)),
        [command, path] if command == "project-settings" => {
            show_project_settings(Path::new(path))
        }
        [command, path] if command == "global-swing" => show_global_swing(Path::new(path)),
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
        [command, project, pattern_id, output] if command == "render-pattern-samplers" => {
            render_pattern_samplers(
                Path::new(project),
                parse_u16(pattern_id, "pattern id")?,
                Path::new(output),
                SamplerPatternRenderOptions::default(),
            )
        }
        [command, project, pattern_id, output, sample_rate]
            if command == "render-pattern-samplers" =>
        {
            render_pattern_samplers(
                Path::new(project),
                parse_u16(pattern_id, "pattern id")?,
                Path::new(output),
                SamplerPatternRenderOptions {
                    sample_rate: parse_u32(sample_rate, "sample rate")?,
                    ..SamplerPatternRenderOptions::default()
                },
            )
        }
        [command, project, pattern_id, output, sample_rate, voice_limit]
            if command == "render-pattern-samplers" =>
        {
            render_pattern_samplers(
                Path::new(project),
                parse_u16(pattern_id, "pattern id")?,
                Path::new(output),
                SamplerPatternRenderOptions {
                    sample_rate: parse_u32(sample_rate, "sample rate")?,
                    voice_limit: parse_usize(voice_limit, "voice limit")?,
                    ..SamplerPatternRenderOptions::default()
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
        [command, path, pattern_id] if command == "pattern-controllers" => {
            list_pattern_controllers(Path::new(path), parse_u16(pattern_id, "pattern id")?)
        }
        [command, input, output, pattern_id, controller_index, position, value]
            if command == "edit-pattern-controller" =>
        {
            edit_pattern_controller(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_usize(controller_index, "Pattern controller index")?,
                PatternControllerEdit {
                    position: parse_optional_u32(position, "controller position")?,
                    value: parse_optional_f32(value, "controller value")?,
                },
            )
        }
        [command, input, output, pattern_id, template_index, position, value]
            if command == "duplicate-pattern-controller" =>
        {
            duplicate_pattern_controller(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_usize(template_index, "Pattern controller template index")?,
                parse_u32(position, "controller position")?,
                parse_f32(value, "controller value")?,
            )
        }
        [command, input, output, pattern_id, controller_index]
            if command == "delete-pattern-controller" =>
        {
            delete_pattern_controller(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_usize(controller_index, "Pattern controller index")?,
            )
        }
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
        [command, project, output, pattern_id] if command == "export-midi-pattern" => {
            export_midi_pattern(
                Path::new(project),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                MidiChannelMapping::PreserveNoteChannels,
            )
        }
        [command, project, output, pattern_id, mapping] if command == "export-midi-pattern" => {
            export_midi_pattern(
                Path::new(project),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_midi_channel_mapping(mapping)?,
            )
        }
        [command, project, output, arrangement_id] if command == "export-midi-song" => {
            export_midi_song(
                Path::new(project),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                MidiChannelMapping::PreserveNoteChannels,
            )
        }
        [command, project, output, arrangement_id, mapping] if command == "export-midi-song" => {
            export_midi_song(
                Path::new(project),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_midi_channel_mapping(mapping)?,
            )
        }
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
        [command, input, output] if command == "create-pattern" => {
            create_pattern(Path::new(input), Path::new(output))
        }
        [command, input, output, title, author, genre, comments, web_link]
            if command == "set-project-info" =>
        {
            write_project_info(
                Path::new(input),
                Path::new(output),
                ProjectInfoEdit {
                    title: project_info_argument(title),
                    author: project_info_argument(author),
                    genre: project_info_argument(genre),
                    comments: project_info_argument(comments),
                    web_link: project_info_argument(web_link),
                },
            )
        }
        [command, input, output, play_truncated, fast_declick, pan_law]
            if command == "set-project-settings" =>
        {
            write_project_settings(
                Path::new(input),
                Path::new(output),
                ProjectSettingsEdit {
                    play_truncated_notes_in_clips: parse_optional_bool(
                        play_truncated,
                        "Play truncated notes setting",
                    )?,
                    fast_declick_for_cut_groups: parse_optional_bool(
                        fast_declick,
                        "Fast declick setting",
                    )?,
                    pan_law_raw: parse_optional_u8(pan_law, "raw pan law")?,
                    ..ProjectSettingsEdit::default()
                },
            )
        }
        [command, input, output, play_truncated, fast_declick]
            if command == "set-project-settings" =>
        {
            write_project_settings(
                Path::new(input),
                Path::new(output),
                ProjectSettingsEdit {
                    play_truncated_notes_in_clips: parse_optional_bool(
                        play_truncated,
                        "Play truncated notes setting",
                    )?,
                    fast_declick_for_cut_groups: parse_optional_bool(
                        fast_declick,
                        "Fast declick setting",
                    )?,
                    ..ProjectSettingsEdit::default()
                },
            )
        }
        [command, input, output, numerator, denominator]
            if command == "set-project-time-signature" =>
        {
            write_project_settings(
                Path::new(input),
                Path::new(output),
                ProjectSettingsEdit {
                    time_signature: Some((
                        parse_u8(numerator, "time-signature numerator")?,
                        parse_u8(denominator, "time-signature denominator")?,
                    )),
                    ..ProjectSettingsEdit::default()
                },
            )
        }
        [command, input, output, pattern_id, position, numerator, denominator]
            if command == "set-pattern-time-signature" =>
        {
            set_pattern_time_signature(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u32(position, "pattern time-signature position")?,
                parse_u8(numerator, "time-signature numerator")?,
                parse_u8(denominator, "time-signature denominator")?,
            )
        }
        [command, input, output, pattern_id, position]
            if command == "delete-pattern-time-signature" =>
        {
            delete_pattern_time_signature(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u32(position, "pattern time-signature position")?,
            )
        }
        [command, input, output, percent] if command == "set-global-swing" => {
            let percent = parse_u32(percent, "global swing mix percentage")?;
            if percent > 100 {
                return Err("global swing mix must be between 0 and 100 percent".to_owned());
            }
            write_global_swing(Path::new(input), Path::new(output), percent as u8)
        }
        [command, input, output, channel_id, name] if command == "rename-channel" => {
            let channel_id = channel_id
                .parse::<u16>()
                .map_err(|_| "channel id must be an integer from 0 through 65535".to_owned())?;
            rename_channel(Path::new(input), Path::new(output), channel_id, name)
        }
        [command, input, output, channel_id, color] if command == "set-channel-color" => {
            set_channel_color(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_rgb_hex(color)?,
            )
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
        [command, input, output, channel_id, percent] if command == "set-channel-swing" => {
            let percent = parse_u32(percent, "channel swing mix percentage")?;
            if percent > 100 {
                return Err("channel swing mix must be between 0 and 100 percent".to_owned());
            }
            let swing_mix = ((percent * 128 + 50) / 100) as u16;
            set_channel_swing(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                percent as u8,
                swing_mix,
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
        [command, input, output, channel_id, random, crossfade]
            if command == "set-layer-flags" =>
        {
            set_layer_flags(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "Layer channel id")?,
                parse_optional_bool(random, "Layer Random")?,
                parse_optional_bool(crossfade, "Layer Crossfade")?,
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
        [command, input, output, pattern_id, channel_id, note_index, flags, group, fine_pitch, release, midi_channel, pan, mod_x, mod_y]
            if command == "edit-note-properties" =>
        {
            edit_note(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                parse_usize(note_index, "channel note index")?,
                PatternNoteEdit {
                    flags: parse_optional_u16(flags, "note flags")?,
                    group: parse_optional_u16(group, "note group")?,
                    fine_pitch: parse_optional_u8(fine_pitch, "fine pitch")?,
                    release: parse_optional_u8(release, "release")?,
                    midi_channel: parse_optional_u8(midi_channel, "MIDI channel")?,
                    pan: parse_optional_u8(pan, "note pan")?,
                    mod_x: parse_optional_u8(mod_x, "modulation X")?,
                    mod_y: parse_optional_u8(mod_y, "modulation Y")?,
                    ..PatternNoteEdit::default()
                },
            )
        }
        [command, input, output, pattern_id, channel_id, grid_ticks, strength, swing]
            if command == "quantize-notes" =>
        {
            quantize_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                parse_u32(grid_ticks, "quantize grid ticks")?,
                parse_f64(strength, "quantize strength")?,
                parse_f64(swing, "quantize swing")?,
            )
        }
        [command, input, output, pattern_id, channel_id] if command == "legato-notes" => {
            legato_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
            )
        }
        [command, input, output, pattern_id, channel_id, divisions]
            if command == "chop-notes" =>
        {
            chop_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                parse_u8(divisions, "note chop divisions")?,
            )
        }
        [command, input, output, pattern_id, channel_id] if command == "glue-notes" => {
            glue_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
            )
        }
        [command, input, output, pattern_id, channel_id] if command == "flip-notes" => {
            flip_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
            )
        }
        [command, input, output, pattern_id, channel_id, spread_ticks, direction]
            if command == "strum-notes" =>
        {
            let descending = match direction.as_str() {
                "up" => false,
                "down" => true,
                _ => return Err("strum direction must be 'up' or 'down'".to_owned()),
            };
            strum_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                parse_u32(spread_ticks, "strum spread")?,
                descending,
            )
        }
        [command, input, output, pattern_id, channel_id, stroke_ticks, velocity, placement]
            if command == "flam-notes" =>
        {
            let before = match placement.as_str() {
                "before" => true,
                "after" => false,
                _ => return Err("flam placement must be 'before' or 'after'".to_owned()),
            };
            flam_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                parse_u32(stroke_ticks, "flam stroke time")?,
                parse_u8(velocity, "flam velocity")?,
                before,
            )
        }
        [command, input, output, pattern_id, channel_id, seed, velocity_amount, pan_amount,
            pitch_range, bipolar, reset_levels]
            if command == "randomize-notes" =>
        {
            randomize_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomizerOptions {
                    seed: parse_u64(seed, "randomizer seed")?,
                    velocity_amount_percent: parse_i16(velocity_amount, "velocity amount")?,
                    pan_amount_percent: parse_i16(pan_amount, "pan amount")?,
                    pitch_range_semitones: parse_u8(pitch_range, "pitch range")?,
                    bipolar: parse_bool(bipolar, "bipolar")?,
                    reset_levels: parse_bool(reset_levels, "reset levels")?,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, seed, timing_range, velocity_variation]
            if command == "humanize-notes" =>
        {
            humanize_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                parse_u64(seed, "humanize seed")?,
                parse_u32(timing_range, "timing range")?,
                parse_u8(velocity_variation, "velocity variation")?,
            )
        }
        [command, input, output, pattern_id, channel_id, minimum_key, maximum_key]
            if command == "limit-notes" =>
        {
            limit_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                parse_u16(minimum_key, "minimum key")?,
                parse_u16(maximum_key, "maximum key")?,
            )
        }
        [command, input, output, pattern_id, channel_id, step_ticks, range_octaves, gate, direction]
            if command == "arp-notes" =>
        {
            let direction = parse_arpeggio_direction(direction)?;
            arpeggiate_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                ArpeggioOptions {
                    step_ticks: parse_u32(step_ticks, "arpeggiator step")?,
                    range_octaves: parse_u8(range_octaves, "arpeggiator range")?,
                    gate_percent: parse_u8(gate, "arpeggiator gate")?,
                    direction,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, position_ticks]
            if command == "slice-notes" =>
        {
            slice_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                parse_u32(position_ticks, "slice position")?,
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
        [command, input, output, track_id, enabled, grouped] if command == "edit-playlist-track" => {
            edit_playlist_track(
                Path::new(input),
                Path::new(output),
                parse_u32(track_id, "Playlist track id")?,
                PlaylistTrackEdit {
                    enabled: parse_optional_bool(enabled, "Playlist track enabled state")?,
                    grouped: parse_optional_bool(grouped, "Playlist track grouping state")?,
                },
            )
        }
        [command, input, output, arrangement_id, clip_index, item_index, raw_track_index, group, item_flags, start_offset, end_offset, scale]
            if command == "edit-clip-properties" =>
        {
            edit_clip_properties(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
                PlaylistClipEdit {
                    item_index: parse_optional_u16(item_index, "raw item index")?,
                    raw_track_index: parse_optional_u16(raw_track_index, "raw track index")?,
                    group: parse_optional_u16(group, "raw clip group")?,
                    item_flags: parse_optional_u16(item_flags, "raw clip flags")?,
                    start_offset: parse_optional_f32(start_offset, "clip start offset")?,
                    end_offset: parse_optional_f32(end_offset, "clip end offset")?,
                    scale: parse_optional_f64(scale, "clip scale")?,
                    ..PlaylistClipEdit::default()
                },
            )
        }
        [command, input, output, arrangement_id, clip_index, position, raw_track_index]
            if command == "duplicate-clip" =>
        {
            duplicate_clip(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
                parse_optional_u32(position, "duplicate clip position")?,
                parse_optional_u16(raw_track_index, "duplicate clip track index")?,
            )
        }
        [command, input, output, arrangement_id, clip_index, split_position, source_length]
            if command == "split-audio-clip" =>
        {
            split_audio_clip(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
                parse_u32(split_position, "split position ticks")?,
                parse_optional_f32(source_length, "full source length in milliseconds")?,
            )
        }
        [command, input, output, arrangement_id, left_clip_index, right_clip_index]
            if command == "join-audio-clips" =>
        {
            join_audio_clips(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(left_clip_index, "left clip index")?,
                parse_usize(right_clip_index, "right clip index")?,
            )
        }
        [command, input, output, arrangement_id, left_clip_index, right_clip_index]
            if command == "join-pattern-clips" =>
        {
            join_pattern_clips(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(left_clip_index, "left clip index")?,
                parse_usize(right_clip_index, "right clip index")?,
            )
        }
        [command, input, output, arrangement_id, clip_indices]
            if command == "merge-pattern-clips" =>
        {
            merge_pattern_clips(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_clip_indices(clip_indices)?,
            )
        }
        [command, input, output, arrangement_id, clip_index, delta_ms, source_length_ms]
            if command == "slip-audio-clip" =>
        {
            slip_audio_clip(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
                parse_f64(delta_ms, "slip distance in milliseconds")?,
                parse_f32(source_length_ms, "sample length in milliseconds")?,
            )
        }
        [command, input, output, arrangement_id, marker_index, position, is_signature, numerator, denominator, name]
            if command == "edit-time-marker" =>
        {
            edit_time_marker(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(marker_index, "time marker index")?,
                TimeMarkerEdit {
                    position_ticks: parse_optional_u32(position, "time marker position")?,
                    is_signature: parse_optional_bool(is_signature, "time signature marker flag")?,
                    numerator: parse_optional_u8(numerator, "time signature numerator")?,
                    denominator: parse_optional_u8(denominator, "time signature denominator")?,
                    name: project_info_argument(name),
                },
            )
        }
        [command, input, output, arrangement_id, position, is_signature, numerator, denominator, name]
            if command == "create-time-marker" =>
        {
            let is_signature = parse_optional_bool(is_signature, "time signature marker flag")?
                .ok_or_else(|| "time signature marker flag must be 0 or 1".to_owned())?;
            create_time_marker(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                TimeMarkerEdit {
                    position_ticks: Some(parse_u32(position, "time marker position")?),
                    is_signature: Some(is_signature),
                    numerator: parse_optional_u8(numerator, "time signature numerator")?,
                    denominator: parse_optional_u8(denominator, "time signature denominator")?,
                    name: project_info_argument(name),
                },
            )
        }
        [command, input, output, arrangement_id, marker_index]
            if command == "delete-time-marker" =>
        {
            delete_time_marker(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(marker_index, "time marker index")?,
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
            "  flp-rebuild preset-info <file.fst>\n",
            "  flp-rebuild project-info <file.flp>\n",
            "  flp-rebuild project-settings <file.flp>\n",
            "  flp-rebuild global-swing <file.flp>\n",
            "  flp-rebuild midi-info <file.mid>\n",
            "  flp-rebuild export-midi-pattern <project.flp> <output.mid> <pattern-id> [stored|channels]\n",
            "  flp-rebuild export-midi-song <project.flp> <output.mid> <arrangement-id> [stored|channels]\n",
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
            "  flp-rebuild render-pattern-samplers <project.flp> <pattern-id> <output.wav> [sample-rate] [voice-limit]\n",
            "  flp-rebuild plugin-states <file.flp>\n",
            "  flp-rebuild channel-events <file.flp> <channel-id>\n",
            "  flp-rebuild patterns <file.flp>\n",
            "  flp-rebuild pattern-controllers <file.flp> <pattern-id>\n",
            "  flp-rebuild edit-pattern-controller <input.flp> <output.flp> <pattern-id> <controller-index> <position-ticks|-> <value|->\n",
            "  flp-rebuild duplicate-pattern-controller <input.flp> <output.flp> <pattern-id> <template-index> <position-ticks> <value>\n",
            "  flp-rebuild delete-pattern-controller <input.flp> <output.flp> <pattern-id> <controller-index>\n",
            "  flp-rebuild playlist <file.flp> [start] [count]\n",
            "  flp-rebuild notes <file.flp> <pattern-id> [start] [count]\n",
            "  flp-rebuild events <file.flp> [start] [count]\n",
            "  flp-rebuild extract-event <file.flp> <index> <output.bin>\n",
            "  flp-rebuild roundtrip <input.flp> <output.flp>\n",
            "  flp-rebuild set-tempo <input.flp> <output.flp> <bpm>\n",
            "  flp-rebuild create-pattern <input.flp> <output.flp>\n",
            "  flp-rebuild set-project-info <input.flp> <output.flp> <title|-> <author|-> <genre|-> <comments|-> <web-link|->\n",
            "  flp-rebuild set-project-settings <input.flp> <output.flp> <play-truncated:0|1|-> <fast-declick:0|1|-> [pan-law-raw|-]\n",
            "  flp-rebuild set-project-time-signature <input.flp> <output.flp> <numerator> <denominator>\n",
            "  flp-rebuild set-pattern-time-signature <input.flp> <output.flp> <pattern-id> <position-ticks> <numerator> <denominator>\n",
            "  flp-rebuild delete-pattern-time-signature <input.flp> <output.flp> <pattern-id> <position-ticks>\n",
            "  flp-rebuild set-global-swing <input.flp> <output.flp> <percent-0..100>\n",
            "  flp-rebuild rename-channel <input.flp> <output.flp> <channel-id> <name>\n",
            "  flp-rebuild set-channel-color <input.flp> <output.flp> <channel-id> <RRGGBB>\n",
            "  flp-rebuild set-channel-levels <input.flp> <output.flp> <channel-id> <volume-0..12800> <pan-0..12800>\n",
            "  flp-rebuild set-channel-swing <input.flp> <output.flp> <channel-id> <percent-0..100>\n",
            "  flp-rebuild set-layer-children <input.flp> <output.flp> <layer-channel-id> <child-ids-comma-separated|->\n",
            "  flp-rebuild set-layer-flags <input.flp> <output.flp> <layer-channel-id> <random:0|1|-> <crossfade:0|1|->\n",
            "  flp-rebuild edit-automation-point <input.flp> <output.flp> <channel-id> <point-index> <position-beats> <value> <tension>\n",
            "  flp-rebuild insert-automation-point <input.flp> <output.flp> <channel-id> <insertion-slot> <position-beats> <value> <tension>\n",
            "  flp-rebuild delete-automation-point <input.flp> <output.flp> <channel-id> <point-index>\n",
            "  flp-rebuild edit-note <input.flp> <output.flp> <pattern-id> <channel-id> <note-index> <position> <length> <key> <velocity>\n",
            "  flp-rebuild edit-note-properties <input.flp> <output.flp> <pattern-id> <channel-id> <note-index> <flags-raw|-> <group-raw|-> <fine-pitch|-> <release|-> <midi-channel-raw|-> <pan|-> <mod-x|-> <mod-y|->\n",
            "  flp-rebuild quantize-notes <input.flp> <output.flp> <pattern-id> <channel-id> <grid-ticks> <strength-0..1> <swing-0..1>\n",
            "  flp-rebuild legato-notes <input.flp> <output.flp> <pattern-id> <channel-id>\n",
            "  flp-rebuild chop-notes <input.flp> <output.flp> <pattern-id> <channel-id> <divisions-2..64>\n",
            "  flp-rebuild glue-notes <input.flp> <output.flp> <pattern-id> <channel-id>\n",
            "  flp-rebuild flip-notes <input.flp> <output.flp> <pattern-id> <channel-id>\n",
            "  flp-rebuild strum-notes <input.flp> <output.flp> <pattern-id> <channel-id> <spread-ticks> <up|down>\n",
            "  flp-rebuild flam-notes <input.flp> <output.flp> <pattern-id> <channel-id> <stroke-ticks> <velocity-0..127> <before|after>\n",
            "  flp-rebuild randomize-notes <input.flp> <output.flp> <pattern-id> <channel-id> <seed> <velocity-amount:-100..100> <pan-amount:-100..100> <pitch-range:0..24> <bipolar:0|1> <reset:0|1>\n",
            "  flp-rebuild humanize-notes <input.flp> <output.flp> <pattern-id> <channel-id> <seed> <timing-range-ticks> <velocity-variation-0..100>\n",
            "  flp-rebuild limit-notes <input.flp> <output.flp> <pattern-id> <channel-id> <minimum-key-0..127> <maximum-key-0..127>\n",
            "  flp-rebuild arp-notes <input.flp> <output.flp> <pattern-id> <channel-id> <step-ticks> <range-octaves-1..4> <gate-1..100> <up|down|up-down>\n",
            "  flp-rebuild slice-notes <input.flp> <output.flp> <pattern-id> <channel-id> <position-ticks>\n",
            "  flp-rebuild add-note <input.flp> <output.flp> <pattern-id> <channel-id> <position> <length> <key> <velocity>\n",
            "  flp-rebuild delete-note <input.flp> <output.flp> <pattern-id> <channel-id> <note-index>\n",
            "  flp-rebuild import-midi <input.flp> <input.mid> <output.flp> <track> <pattern-id> <channel-id>\n",
            "  flp-rebuild edit-clip <input.flp> <output.flp> <arrangement-id> <clip-index> <position> <length>\n",
            "  flp-rebuild edit-playlist-track <input.flp> <output.flp> <track-id> <enabled:0|1|-> <grouped:0|1|->\n",
            "  flp-rebuild edit-clip-properties <input.flp> <output.flp> <arrangement-id> <clip-index> <item-index-raw|-> <track-index-raw|-> <group-raw|-> <flags-raw|-> <start-offset|-> <end-offset|-> <scale|->\n",
            "  flp-rebuild duplicate-clip <input.flp> <output.flp> <arrangement-id> <clip-index> <position|-> <track-index-raw|->\n",
            "  flp-rebuild split-audio-clip <input.flp> <output.flp> <arrangement-id> <clip-index> <split-position-ticks> <source-length-ms|->\n",
            "  flp-rebuild join-audio-clips <input.flp> <output.flp> <arrangement-id> <left-clip-index> <right-clip-index>\n",
            "  flp-rebuild join-pattern-clips <input.flp> <output.flp> <arrangement-id> <left-clip-index> <right-clip-index>\n",
            "  flp-rebuild merge-pattern-clips <input.flp> <output.flp> <arrangement-id> <clip-index,clip-index,...>\n",
            "  flp-rebuild slip-audio-clip <input.flp> <output.flp> <arrangement-id> <clip-index> <delta-ms> <sample-length-ms>\n",
            "  flp-rebuild edit-time-marker <input.flp> <output.flp> <arrangement-id> <marker-index> <position-ticks|-> <signature:0|1|-> <numerator|-> <denominator|-> <name|->\n",
            "  flp-rebuild create-time-marker <input.flp> <output.flp> <arrangement-id> <position-ticks> <signature:0|1> <numerator|-> <denominator|-> <name|->\n",
            "  flp-rebuild delete-time-marker <input.flp> <output.flp> <arrangement-id> <marker-index>"
        )
        .to_owned()),
    }
}

fn parse_usize(value: &str, description: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|_| format!("{description} must be a non-negative integer"))
}

fn parse_clip_indices(value: &str) -> Result<Vec<usize>, String> {
    let mut indices = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err("clip indexes must be comma-separated non-negative integers".to_owned());
        }
        indices.push(parse_usize(part, "clip index")?);
    }
    if indices.len() < 2 {
        return Err("merge-pattern-clips requires at least two clip indexes".to_owned());
    }
    Ok(indices)
}

fn parse_midi_channel_mapping(value: &str) -> Result<MidiChannelMapping, String> {
    match value {
        "stored" => Ok(MidiChannelMapping::PreserveNoteChannels),
        "channels" => Ok(MidiChannelMapping::AssignProjectChannels),
        _ => Err("MIDI channel mapping must be 'stored' or 'channels'".to_owned()),
    }
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

fn parse_i16(value: &str, description: &str) -> Result<i16, String> {
    value
        .parse::<i16>()
        .map_err(|_| format!("{description} must be an integer from -32768 through 32767"))
}

fn parse_u64(value: &str, description: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{description} must be a non-negative 64-bit integer"))
}

fn parse_bool(value: &str, description: &str) -> Result<bool, String> {
    match value {
        "0" | "false" => Ok(false),
        "1" | "true" => Ok(true),
        _ => Err(format!("{description} must be 0 or 1")),
    }
}

fn parse_arpeggio_direction(value: &str) -> Result<ArpeggioDirection, String> {
    match value {
        "up" => Ok(ArpeggioDirection::Up),
        "down" => Ok(ArpeggioDirection::Down),
        "up-down" => Ok(ArpeggioDirection::UpDown),
        _ => Err("arpeggiator direction must be 'up', 'down', or 'up-down'".to_owned()),
    }
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

fn project_info_argument(value: &str) -> Option<String> {
    (value != "-").then(|| value.to_owned())
}

fn parse_optional_bool(value: &str, description: &str) -> Result<Option<bool>, String> {
    match value.to_ascii_lowercase().as_str() {
        "-" => Ok(None),
        "0" | "false" => Ok(Some(false)),
        "1" | "true" => Ok(Some(true)),
        _ => Err(format!("{description} must be 0, 1, true, false, or -")),
    }
}

fn parse_optional_u8(value: &str, description: &str) -> Result<Option<u8>, String> {
    if value == "-" {
        Ok(None)
    } else {
        parse_u8(value, description).map(Some)
    }
}

fn parse_optional_u16(value: &str, description: &str) -> Result<Option<u16>, String> {
    if value == "-" {
        Ok(None)
    } else {
        parse_u16(value, description).map(Some)
    }
}

fn parse_optional_u32(value: &str, description: &str) -> Result<Option<u32>, String> {
    if value == "-" {
        Ok(None)
    } else {
        parse_u32(value, description).map(Some)
    }
}

fn parse_optional_f32(value: &str, description: &str) -> Result<Option<f32>, String> {
    if value == "-" {
        Ok(None)
    } else {
        parse_f32(value, description).map(Some)
    }
}

fn parse_optional_f64(value: &str, description: &str) -> Result<Option<f64>, String> {
    if value == "-" {
        Ok(None)
    } else {
        parse_f64(value, description).map(Some)
    }
}

fn parse_u32(value: &str, description: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|_| format!("{description} must be a non-negative 32-bit integer"))
}

fn parse_rgb_hex(value: &str) -> Result<[u8; 3], String> {
    let digits = value.strip_prefix('#').unwrap_or(value);
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("channel color must be six hexadecimal digits (RRGGBB)".to_owned());
    }
    let parse_component = |start| {
        u8::from_str_radix(&digits[start..start + 2], 16)
            .map_err(|_| "channel color must be six hexadecimal digits (RRGGBB)".to_owned())
    };
    Ok([
        parse_component(0)?,
        parse_component(2)?,
        parse_component(4)?,
    ])
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
    let channel_swing_mix_raw = document
        .channels()
        .into_iter()
        .find(|channel| channel.id() == channel_id)
        .map_or(128, |channel| channel.swing_mix());
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
            global_swing_mix_raw: document.metadata().global_swing_mix(),
            channel_swing_mix_raw,
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

fn render_pattern_samplers(
    project_path: &Path,
    pattern_id: u16,
    output_path: &Path,
    options: SamplerPatternRenderOptions,
) -> Result<(), String> {
    let (_, document) = load_document(project_path)?;
    let summary = render_sampler_pattern_to_wav(
        &document,
        project_path,
        SamplerPatternRenderOptions {
            pattern_id,
            ..options
        },
        output_path,
    )?;
    println!(
        "rendered pattern {} Samplers to {}: {} notes, {} channels, {} source files, {:.2} seconds, {} Hz, {} voices stolen",
        pattern_id,
        output_path.display(),
        summary.notes_rendered,
        summary.sampler_channels_rendered,
        summary.source_files,
        summary.frames as f64 / f64::from(summary.sample_rate),
        summary.sample_rate,
        summary.voices_stolen,
    );
    if !summary.unresolved_sample_channels.is_empty() {
        println!(
            "skipped {} notes with unresolved samples on channels {:?}",
            summary.notes_skipped_unresolved_sample, summary.unresolved_sample_channels,
        );
    }
    println!(
        "render uses saved channel roots or WAVE root metadata (default C5); sampler envelopes, Playlist arrangement, automation, Mixer routing, and effects are not applied"
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
    if let Some(title) = document.metadata().title() {
        println!("title: {title:?}");
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

fn inspect_preset(path: &Path) -> Result<(), String> {
    let bytes =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let preset = FstPreset::parse(&bytes).map_err(|error| error.to_string())?;
    let document = preset.document();
    println!("file: {}", path.display());
    println!("size: {} bytes", bytes.len());
    println!("state format: {}", document.header().format());
    println!("state kind: {}", preset.kind());
    println!(
        "FL Studio version: {}",
        document.project_version().unwrap_or("unknown")
    );
    println!("events: {}", document.events().len());
    println!("channel records: {}", document.channels().len());
    println!(
        "channel plug-in states: {}",
        document.channel_plugin_states().len()
    );
    println!("Mixer inserts: {}", document.mixer_inserts().len());
    println!(
        "automation channels: {}",
        document
            .automation_channels()
            .map_or(0, |items| items.len())
    );
    println!("trailing bytes: {}", document.trailing_bytes().len());
    Ok(())
}

fn show_project_info(path: &Path) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let metadata = document.metadata();
    println!("title: {:?}", metadata.title());
    println!("author: {:?}", metadata.author());
    println!("genre: {:?}", metadata.genre());
    println!("comments: {:?}", metadata.comments());
    println!("web link: {:?}", metadata.web_link());
    Ok(())
}

fn show_project_settings(path: &Path) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    if let Some(settings) = document.project_settings() {
        println!(
            "play truncated notes in clips: {}",
            settings.play_truncated_notes_in_clips
        );
        println!(
            "fast declick for cut groups: {}",
            settings.fast_declick_for_cut_groups
        );
    } else {
        println!("advanced project settings: unsupported event layout");
    }
    match document.metadata().pan_law_raw() {
        Some(0) => println!("pan law: Circular (raw 0)"),
        Some(2) => println!("pan law: Triangular (raw 2)"),
        Some(raw) => println!("pan law: unknown (raw {raw})"),
        None => println!("pan law: Circular (default; event absent or ambiguous)"),
    }
    match document.metadata().time_signature() {
        Some((numerator, denominator)) => {
            println!("project time signature: {numerator}/{denominator}");
        }
        None => println!("project time signature: not uniquely stored"),
    }
    Ok(())
}

fn show_global_swing(path: &Path) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let metadata = document.metadata();
    match metadata.global_swing_mix_raw() {
        Some(raw) => {
            if raw <= 128 {
                let percent = (u32::from(raw) * 100 + 64) / 128;
                println!("global swing mix: {percent}% (raw {raw})");
            } else {
                println!("global swing mix: invalid raw {raw} (expected 0–128)");
            }
        }
        None => println!("global swing mix: 0% (event absent; defaults to 0)"),
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
    } else if let Some(time_base) = midi.smpte_time_base() {
        println!(
            "time division: SMPTE {:.3} frames per second, {} ticks per frame",
            time_base.frames_per_second(),
            time_base.ticks_per_frame()
        );
    } else {
        println!(
            "time division: unsupported SMPTE encoding 0x{:04X}",
            midi.division()
        );
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

fn export_midi_pattern(
    project: &Path,
    output: &Path,
    pattern_id: u16,
    channel_mapping: MidiChannelMapping,
) -> Result<(), String> {
    let (_, document) = load_document(project)?;
    let bytes = MidiFile::encode_project_pattern(&document, pattern_id, channel_mapping)
        .map_err(|error| error.to_string())?;
    fs::write(output, &bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "exported pattern {pattern_id} as {} ({} bytes)",
        output.display(),
        bytes.len()
    );
    Ok(())
}

fn export_midi_song(
    project: &Path,
    output: &Path,
    arrangement_id: u16,
    channel_mapping: MidiChannelMapping,
) -> Result<(), String> {
    let (_, document) = load_document(project)?;
    let bytes = MidiFile::encode_project_song(&document, arrangement_id, channel_mapping)
        .map_err(|error| error.to_string())?;
    fs::write(output, &bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "exported arrangement {arrangement_id} as {} ({} bytes)",
        output.display(),
        bytes.len()
    );
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
            "id={} kind={} type={:?} enabled={} color={:?} volume={:?} pan={:?} swing_mix_raw={:?} layer_children={:?} layer_flags={:?} plugin={} name={} sample_path={:?} events={:?}",
            channel.id(),
            channel
                .kind()
                .map_or_else(|| "unknown".to_owned(), |kind| kind.to_string()),
            channel.channel_type(),
            channel
                .enabled()
                .map_or_else(|| "unknown".to_owned(), |enabled| enabled.to_string()),
            channel.color(),
            channel.volume(),
            channel.pan(),
            channel.swing_mix_raw(),
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
    match host.restore_flp_channel_state(info.id, &state) {
        Ok(()) => {
            let snapshot = host.save_state(info.id)?;
            println!(
                "FLP VST3 component/controller state restored: FLP event bytes={} nested state bytes={} host snapshot bytes={}",
                state.data_payload().len(),
                state
                    .vst_metadata()
                    .and_then(|metadata| metadata.state_bytes())
                    .map_or_else(|| "unknown".to_owned(), |bytes| bytes.to_string()),
                snapshot.len()
            );
        }
        Err(error) => println!(
            "FLP VST3 state restore failed: FLP event bytes={} nested state bytes={} error={error}",
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
            "id={} name={} length_ticks={} notes={} controllers={} channels=[{}] time_signatures=[{}]",
            pattern.id,
            pattern.name.as_deref().unwrap_or("(unnamed)"),
            pattern
                .length_ticks
                .map_or_else(|| "default".to_owned(), |length| length.to_string()),
            pattern.notes.len(),
            pattern.controllers.len(),
            channel_summary,
            pattern
                .time_markers
                .iter()
                .filter(|marker| marker.is_signature())
                .map(|marker| format!(
                    "{}:{}/{}",
                    marker.position_ticks(),
                    marker
                        .numerator()
                        .map_or_else(|| "?".to_owned(), |value| value.to_string()),
                    marker
                        .denominator()
                        .map_or_else(|| "?".to_owned(), |value| value.to_string()),
                ))
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    Ok(())
}

fn list_pattern_controllers(path: &Path, pattern_id: u16) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let patterns = document.patterns().map_err(|error| error.to_string())?;
    let Some(pattern) = patterns.iter().find(|pattern| pattern.id == pattern_id) else {
        return Err(format!("pattern id {pattern_id} was not found"));
    };
    println!(
        "pattern={} controllers={}",
        pattern.id,
        pattern.controllers.len()
    );
    for (index, controller) in pattern.controllers.iter().enumerate() {
        println!(
            "{index:06} position={} channel_raw={} flags=0x{:02X} reserved={:02X}{:02X} value={} value_bits=0x{:08X}",
            controller.position,
            controller.channel,
            controller.flags,
            controller.reserved[0],
            controller.reserved[1],
            controller.value(),
            controller.value_bits,
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

fn write_project_info(input: &Path, output: &Path, edit: ProjectInfoEdit) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_project_info(edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!("wrote updated Project Info to {}", output.display());
    Ok(())
}

fn write_project_settings(
    input: &Path,
    output: &Path,
    edit: ProjectSettingsEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_project_settings(edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!("wrote updated Project settings to {}", output.display());
    Ok(())
}

fn write_global_swing(input: &Path, output: &Path, percent: u8) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let raw = ((u32::from(percent) * 128 + 50) / 100) as u8;
    document
        .set_global_swing_mix(raw)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set global swing mix to {percent}% (raw {raw}) in {}",
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

fn set_channel_color(
    input: &Path,
    output: &Path,
    channel_id: u16,
    rgb: [u8; 3],
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_channel_color(channel_id, rgb)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set channel {channel_id} color to #{:02X}{:02X}{:02X} in {}",
        rgb[0],
        rgb[1],
        rgb[2],
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

fn set_channel_swing(
    input: &Path,
    output: &Path,
    channel_id: u16,
    percent: u8,
    swing_mix: u16,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_channel_swing_mix(channel_id, swing_mix)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set channel {channel_id} swing mix to {percent}% in {}",
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

fn set_layer_flags(
    input: &Path,
    output: &Path,
    layer_channel_id: u16,
    random: Option<bool>,
    crossfade: Option<bool>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_layer_flags(layer_channel_id, random, crossfade)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "updated Layer channel {layer_channel_id} flags in {}",
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

fn edit_pattern_controller(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    controller_index: usize,
    edit: PatternControllerEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_pattern_controller(pattern_id, controller_index, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "edited controller {controller_index} in pattern {pattern_id} in {}",
        output.display()
    );
    Ok(())
}

fn duplicate_pattern_controller(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    template_controller_index: usize,
    position: u32,
    value: f32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let controller_index = document
        .duplicate_pattern_controller(pattern_id, template_controller_index, position, value)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "duplicated controller point {template_controller_index} as point {controller_index} in pattern {pattern_id} in {}",
        output.display()
    );
    Ok(())
}

fn delete_pattern_controller(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    controller_index: usize,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .delete_pattern_controller(pattern_id, controller_index)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "deleted controller point {controller_index} from pattern {pattern_id} in {}",
        output.display()
    );
    Ok(())
}

fn quantize_notes(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    grid_ticks: u32,
    strength: f64,
    swing: f64,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = document
        .quantize_pattern_notes(pattern_id, channel_id, grid_ticks, strength, swing)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "quantized {changed} notes in pattern {pattern_id}, channel {channel_id} to {}",
        output.display()
    );
    Ok(())
}

fn legato_notes(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = document
        .legato_pattern_notes(pattern_id, channel_id)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "extended {changed} notes in pattern {pattern_id}, channel {channel_id} to {}",
        output.display()
    );
    Ok(())
}

fn chop_notes(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    divisions: u8,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let created = document
        .chop_pattern_notes(pattern_id, channel_id, divisions)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "created {created} chopped notes in pattern {pattern_id}, channel {channel_id} using {divisions} divisions in {}",
        output.display()
    );
    Ok(())
}

fn glue_notes(input: &Path, output: &Path, pattern_id: u16, channel_id: u16) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let removed = document
        .glue_pattern_notes(pattern_id, channel_id)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "joined notes and removed {removed} records in pattern {pattern_id}, channel {channel_id} to {}",
        output.display()
    );
    Ok(())
}

fn flip_notes(input: &Path, output: &Path, pattern_id: u16, channel_id: u16) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = document
        .flip_pattern_notes(pattern_id, channel_id)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "flipped {changed} note positions in pattern {pattern_id}, channel {channel_id} to {}",
        output.display()
    );
    Ok(())
}

fn strum_notes(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    spread_ticks: u32,
    descending: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = document
        .strum_pattern_notes(pattern_id, channel_id, spread_ticks, descending)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "strummed {changed} notes in pattern {pattern_id}, channel {channel_id} with {spread_ticks} ticks of spread ({}) to {}",
        if descending { "down" } else { "up" },
        output.display()
    );
    Ok(())
}

fn flam_notes(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    stroke_ticks: u32,
    velocity: u8,
    before: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let created = document
        .flam_pattern_notes(pattern_id, channel_id, stroke_ticks, velocity, before)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "added {created} flam strokes in pattern {pattern_id}, channel {channel_id} ({}) to {}",
        if before {
            "before notes"
        } else {
            "after notes"
        },
        output.display()
    );
    Ok(())
}

fn randomize_notes(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    options: RandomizerOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = document
        .randomize_pattern_notes(pattern_id, channel_id, options)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "randomized {changed} notes in pattern {pattern_id}, channel {channel_id} with seed {} to {}",
        options.seed,
        output.display()
    );
    Ok(())
}

fn humanize_notes(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    seed: u64,
    timing_range_ticks: u32,
    velocity_variation_percent: u8,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = document
        .humanize_pattern_notes(
            pattern_id,
            channel_id,
            seed,
            timing_range_ticks,
            velocity_variation_percent,
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "humanized {changed} notes in pattern {pattern_id}, channel {channel_id} with seed {seed} to {}",
        output.display()
    );
    Ok(())
}

fn limit_notes(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    minimum_key: u16,
    maximum_key: u16,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = document
        .limit_pattern_note_range(pattern_id, channel_id, minimum_key, maximum_key)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "limited {changed} note pitches in pattern {pattern_id}, channel {channel_id} to {minimum_key}..={maximum_key} in {}",
        output.display()
    );
    Ok(())
}

fn arpeggiate_notes(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    options: ArpeggioOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let created = document
        .arpeggiate_pattern_notes(pattern_id, channel_id, options)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "generated {created} arpeggiated notes in pattern {pattern_id}, channel {channel_id} to {}",
        output.display()
    );
    Ok(())
}

fn slice_notes(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    position_ticks: u32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let created = document
        .slice_pattern_notes(pattern_id, channel_id, position_ticks)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "sliced {created} notes at tick {position_ticks} in pattern {pattern_id}, channel {channel_id} to {}",
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

fn create_pattern(input: &Path, output: &Path) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let pattern_id = document
        .create_pattern()
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!("created empty pattern {pattern_id} in {}", output.display());
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

fn edit_clip_properties(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_index: usize,
    edit: PlaylistClipEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_playlist_clip(arrangement_id, clip_index, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "updated arrangement {arrangement_id} clip {clip_index} properties in {}",
        output.display()
    );
    Ok(())
}

fn edit_playlist_track(
    input: &Path,
    output: &Path,
    track_id: u32,
    edit: PlaylistTrackEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_playlist_track(track_id, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!("updated Playlist track {track_id} in {}", output.display());
    Ok(())
}

fn duplicate_clip(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_index: usize,
    position_ticks: Option<u32>,
    raw_track_index: Option<u16>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let duplicate_index = document
        .duplicate_playlist_clip(arrangement_id, clip_index, position_ticks, raw_track_index)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "duplicated arrangement {arrangement_id} clip {clip_index} as clip {duplicate_index} in {}",
        output.display()
    );
    Ok(())
}

fn split_audio_clip(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_index: usize,
    split_position_ticks: u32,
    full_source_length_ms: Option<f32>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let right_clip_index = document
        .split_playlist_audio_clip(
            arrangement_id,
            clip_index,
            split_position_ticks,
            full_source_length_ms,
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "split arrangement {arrangement_id} Audio Clip {clip_index} at tick {split_position_ticks}; right clip is {right_clip_index} in {}",
        output.display()
    );
    Ok(())
}

fn join_audio_clips(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    left_clip_index: usize,
    right_clip_index: usize,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let joined_clip_index = document
        .join_adjacent_playlist_audio_clips(arrangement_id, left_clip_index, right_clip_index)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "joined arrangement {arrangement_id} Audio Clips {left_clip_index} and {right_clip_index} as clip {joined_clip_index} in {}",
        output.display()
    );
    Ok(())
}

fn join_pattern_clips(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    left_clip_index: usize,
    right_clip_index: usize,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let joined_clip_index = document
        .join_adjacent_playlist_pattern_clips(arrangement_id, left_clip_index, right_clip_index)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "joined arrangement {arrangement_id} Pattern Clips {left_clip_index} and {right_clip_index} as clip {joined_clip_index} in {}",
        output.display()
    );
    Ok(())
}

fn merge_pattern_clips(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_indices: Vec<usize>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let merged_clip_index = document
        .merge_playlist_pattern_clips(arrangement_id, &clip_indices)
        .map_err(|error| error.to_string())?;
    let merged_pattern_id = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == arrangement_id)
        .and_then(|arrangement| arrangement.clips.get(merged_clip_index).cloned())
        .and_then(|clip| match clip.target() {
            flp_rebuild::PlaylistClipTarget::Pattern { id } => Some(id),
            flp_rebuild::PlaylistClipTarget::Channel { .. } => None,
        })
        .ok_or_else(|| "the merged Playlist clip could not be found".to_owned())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    let indexes = clip_indices
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "merged arrangement {arrangement_id} Pattern Clips {indexes} into clip {merged_clip_index} (Pattern {merged_pattern_id}) in {}",
        output.display()
    );
    Ok(())
}

fn slip_audio_clip(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_index: usize,
    delta_ms: f64,
    sample_length_ms: f32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .slip_playlist_audio_clip(arrangement_id, clip_index, delta_ms, sample_length_ms)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "slipped arrangement {arrangement_id} Audio Clip {clip_index} by {delta_ms} ms in {}",
        output.display()
    );
    Ok(())
}

fn edit_time_marker(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    marker_index: usize,
    edit: TimeMarkerEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_time_marker(arrangement_id, marker_index, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "edited time marker {marker_index} in arrangement {arrangement_id} to {}",
        output.display()
    );
    Ok(())
}

fn set_pattern_time_signature(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    position_ticks: u32,
    numerator: u8,
    denominator: u8,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let marker_index = document
        .set_pattern_time_signature(pattern_id, position_ticks, numerator, denominator)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Pattern {pattern_id} time-signature marker {marker_index} to {numerator}/{denominator} at tick {position_ticks} in {}",
        output.display()
    );
    Ok(())
}

fn delete_pattern_time_signature(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    position_ticks: u32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .delete_pattern_time_signature(pattern_id, position_ticks)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "deleted Pattern {pattern_id} time signature at tick {position_ticks} from {}",
        output.display()
    );
    Ok(())
}

fn create_time_marker(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    edit: TimeMarkerEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let marker_index = document
        .create_time_marker(arrangement_id, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "created time marker {marker_index} in arrangement {arrangement_id} in {}",
        output.display()
    );
    Ok(())
}

fn delete_time_marker(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    marker_index: usize,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .delete_time_marker(arrangement_id, marker_index)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "deleted time marker {marker_index} from arrangement {arrangement_id} in {}",
        output.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_clip_indices, parse_rgb_hex};

    #[test]
    fn parses_channel_rgb_in_hex_form_with_optional_prefix() {
        assert_eq!(parse_rgb_hex("1aB2c3"), Ok([0x1A, 0xB2, 0xC3]));
        assert_eq!(parse_rgb_hex("#102030"), Ok([0x10, 0x20, 0x30]));
        assert!(parse_rgb_hex("#12345").is_err());
        assert!(parse_rgb_hex("xyzxyz").is_err());
    }

    #[test]
    fn parses_comma_separated_playlist_clip_indexes() {
        assert_eq!(parse_clip_indices("2, 0,7"), Ok(vec![2, 0, 7]));
        assert!(parse_clip_indices("1").is_err());
        assert!(parse_clip_indices("1,,2").is_err());
        assert!(parse_clip_indices("one,2").is_err());
    }
}
