use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use flp_rebuild::media::{SamplePathResolver, decode_audio_file};
use flp_rebuild::midi::{MidiChannelMapping, MidiExportFileFormat, MidiFile, MidiImportOptions};
use flp_rebuild::plugin_settings::load_vst3_manual_latency_offsets;
use flp_rebuild::plugins::scan_installed_plugins;
use flp_rebuild::project_package::copy_unique_sample_file;
use flp_rebuild::sample_render::{
    AudioClipRenderOptions, AudioFileRenderOptions, AudioRenderEncodingOptions, AudioRenderOutput,
    SamplerPatternRenderOptions, render_audio_clips_to_wav, render_sampler_pattern_to_wav,
};
use flp_rebuild::vst3::{Vst3AudioEffectRenderOptions, Vst3HostRuntime, Vst3PatternRenderOptions};
use flp_rebuild::{
    ArpeggioDirection, ArpeggioOptions, ArpeggioRangePattern, ArpeggioScoreOptions,
    ArpeggioScorePatternMode, ArpeggioScoreTimeMultiplier, ArpeggioSyncMode, ArticulateOptions,
    AutomationInterpolationMode, AutomationPointEdit, AutomationScaleLevelsOptions,
    ChannelLevelAdjustmentsEdit, ChannelMidiOutSettingsEdit, ChannelSampleCrossfadeEdit,
    ChannelSampleEffectsEdit, ChannelSampleFxFlagsEdit, ChannelSampleTrimEdit,
    ChannelSampleWindowEdit, ChannelSamplerFilterEdit, ChannelSortOrder,
    ChannelStretchSettingsEdit, ChannelType, ClawMachineOptions, FlamOptions, FlpDocument,
    FscControllerStreamImportOptions, FscScore, FstPreset, HumanizerOptions, LimitNoteOptions,
    LimitSnapDirection, MixerInsertEdit, MixerParameterKind, PatternControllerEdit, PatternNote,
    PatternNoteEdit, PatternNoteRotation, PlaylistClipEdit, PlaylistTrackEdit, PlaylistTrackMotion,
    PlaylistTrackPress, PlaylistTrackSync, ProjectInfoEdit, ProjectSettingsEdit,
    QuantizeNoteLevels, QuantizeNoteMode, QuantizeNoteOptions, RandomNoteChordMap,
    RandomNoteGeneratorOptions, RandomizerOptions, RiffMachineOptions, RiffMachineQuantizeMode,
    SamplerEnvelopeLfoEdit, SamplerEnvelopeLfoTarget, SamplerVolumeEnvelopeEdit,
    ScaleLevelsOptions, StrumOptions, TimeMarkerEdit,
};

struct MidiControlChangeImport<'a> {
    input: &'a Path,
    midi_path: &'a Path,
    output: &'a Path,
    track_index: usize,
    pattern_id: u16,
    target_stream: (u8, u8),
    controller_number: u8,
    midi_channel: Option<u8>,
    new_stream: bool,
}

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
        [command, path] if command == "channel-groups" => {
            list_channel_groups(Path::new(path))
        }
        [command, path] if command == "mixer" => list_mixer(Path::new(path)),
        [command, input, output, enabled] if command == "set-mixer-automatic-pdc" => {
            set_mixer_automatic_pdc(
                Path::new(input),
                Path::new(output),
                parse_bool(enabled, "Automatic PDC setting")?,
            )
        }
        [command, path] if command == "automation" => list_automation(Path::new(path)),
        [command, path] if command == "automation-targets" => {
            list_automation_target_links(Path::new(path))
        }
        [command, input, output, event_index, record_index, source_id_raw, parameter_raw, destination_raw]
            if command == "set-e3-record-raw-fields" =>
        {
            set_e3_record_raw_fields(
                Path::new(input),
                Path::new(output),
                parse_usize(event_index, "0xE3 event index")?,
                parse_usize(record_index, "0xE3 record index")?,
                parse_u16(source_id_raw, "0xE3 source ID raw value")?,
                parse_u16(parameter_raw, "0xE3 parameter raw value")?,
                parse_i16(destination_raw, "0xE3 destination raw value")?,
            )
        }
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
        [command, input, bundle, output] if command == "render-audio-vst3" => {
            render_audio_vst3(
                Path::new(input),
                Path::new(bundle),
                Path::new(output),
                None,
                Vst3AudioEffectRenderOptions::default(),
            )
        }
        [command, input, bundle, output, class_uid] if command == "render-audio-vst3" => {
            render_audio_vst3(
                Path::new(input),
                Path::new(bundle),
                Path::new(output),
                Some(class_uid),
                Vst3AudioEffectRenderOptions::default(),
            )
        }
        [command, input, bundle, output, class_uid, tail] if command == "render-audio-vst3" => {
            let class_uid = (class_uid != "-").then_some(class_uid.as_str());
            render_audio_vst3(
                Path::new(input),
                Path::new(bundle),
                Path::new(output),
                class_uid,
                Vst3AudioEffectRenderOptions {
                    tail_seconds: parse_f64(tail, "effect tail")?,
                },
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
        [command, input, output, pattern_id, channel, flags, position, value]
            if command == "create-pattern-controller-stream" =>
        {
            create_pattern_controller_stream(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_raw_byte(channel, "raw controller channel")?,
                parse_raw_byte(flags, "raw controller flags")?,
                parse_u32(position, "controller position")?,
                parse_f32(value, "controller value")?,
            )
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
        [command, input, output, pattern_id, channel, flags, points]
            if command == "draw-pattern-controller-points" =>
        {
            let points = parse_pattern_controller_points(points)?;
            draw_pattern_controller_points(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                (
                    parse_u8(channel, "controller channel")?,
                    parse_u8(flags, "controller flags")?,
                ),
                &points,
            )
        }
        [command, input, output, pattern_id, channel, flags, start_tick, end_tick]
            if command == "linearize-pattern-controllers" =>
        {
            edit_pattern_controller_range(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                (
                    parse_u8(channel, "controller channel")?,
                    parse_u8(flags, "controller flags")?,
                ),
                parse_u32(start_tick, "controller range start")?,
                parse_u32(end_tick, "controller range end")?,
                PatternControllerRangeOperation::Linear,
            )
        }
        [command, input, output, pattern_id, channel, flags, start_tick, end_tick]
            if command == "spline-pattern-controllers" =>
        {
            edit_pattern_controller_range(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                (
                    parse_u8(channel, "controller channel")?,
                    parse_u8(flags, "controller flags")?,
                ),
                parse_u32(start_tick, "controller range start")?,
                parse_u32(end_tick, "controller range end")?,
                PatternControllerRangeOperation::Spline,
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
        [command, input, output, pattern_id, channel, flags, start_tick, end_tick]
            if command == "delete-pattern-controller-range" =>
        {
            edit_pattern_controller_range(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                (
                    parse_u8(channel, "controller channel")?,
                    parse_u8(flags, "controller flags")?,
                ),
                parse_u32(start_tick, "controller range start")?,
                parse_u32(end_tick, "controller range end")?,
                PatternControllerRangeOperation::Delete,
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
        [command, before, after, arrangement_id, clip_index]
            if command == "diff-playlist-clip" =>
        {
            diff_playlist_clip_records(
                Path::new(before),
                Path::new(after),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
            )
        }
        [command, path] if command == "events" => dump_events(Path::new(path), 0, 64),
        [command, path] if command == "midi-info" => inspect_midi(Path::new(path)),
        [command, project, output, pattern_id] if command == "export-midi-pattern" => {
            export_midi_pattern(
                Path::new(project),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                MidiChannelMapping::PreserveNoteChannels,
                MidiExportFileFormat::Format1,
            )
        }
        [command, project, output, pattern_id, file_format]
            if command == "export-midi-pattern"
                && matches!(file_format.as_str(), "0" | "1" | "format0" | "format1") =>
        {
            export_midi_pattern(
                Path::new(project),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                MidiChannelMapping::PreserveNoteChannels,
                parse_midi_export_file_format(file_format)?,
            )
        }
        [command, project, output, pattern_id, mapping] if command == "export-midi-pattern" => {
            export_midi_pattern(
                Path::new(project),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_midi_channel_mapping(mapping)?,
                MidiExportFileFormat::Format1,
            )
        }
        [command, project, output, pattern_id, mapping, file_format]
            if command == "export-midi-pattern" =>
        {
            export_midi_pattern(
                Path::new(project),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_midi_channel_mapping(mapping)?,
                parse_midi_export_file_format(file_format)?,
            )
        }
        [command, project, output, pattern_id, raw_channel, raw_flags, controller, midi_channel]
            if command == "export-midi-cc" =>
        {
            export_midi_controller_stream(
                Path::new(project),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                (
                    parse_u8(raw_channel, "raw controller stream channel")?,
                    parse_u8(raw_flags, "raw controller stream flags")?,
                ),
                parse_u8(controller, "MIDI Control Change number")?,
                parse_midi_channel_number(midi_channel)?,
                MidiExportFileFormat::Format1,
            )
        }
        [command, project, output, pattern_id, raw_channel, raw_flags, controller, midi_channel, file_format]
            if command == "export-midi-cc" =>
        {
            export_midi_controller_stream(
                Path::new(project),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                (
                    parse_u8(raw_channel, "raw controller stream channel")?,
                    parse_u8(raw_flags, "raw controller stream flags")?,
                ),
                parse_u8(controller, "MIDI Control Change number")?,
                parse_midi_channel_number(midi_channel)?,
                parse_midi_export_file_format(file_format)?,
            )
        }
        [command, project, output, arrangement_id] if command == "export-midi-song" => {
            export_midi_song(
                Path::new(project),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                MidiChannelMapping::PreserveNoteChannels,
                MidiExportFileFormat::Format1,
            )
        }
        [command, project, output, arrangement_id, file_format]
            if command == "export-midi-song"
                && matches!(file_format.as_str(), "0" | "1" | "format0" | "format1") =>
        {
            export_midi_song(
                Path::new(project),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                MidiChannelMapping::PreserveNoteChannels,
                parse_midi_export_file_format(file_format)?,
            )
        }
        [command, project, output, arrangement_id, mapping] if command == "export-midi-song" => {
            export_midi_song(
                Path::new(project),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_midi_channel_mapping(mapping)?,
                MidiExportFileFormat::Format1,
            )
        }
        [command, project, output, arrangement_id, mapping, file_format]
            if command == "export-midi-song" =>
        {
            export_midi_song(
                Path::new(project),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_midi_channel_mapping(mapping)?,
                parse_midi_export_file_format(file_format)?,
            )
        }
        [command, project, output, arrangement_id, raw_channel, raw_flags, controller, midi_channel]
            if command == "export-midi-song-cc" =>
        {
            export_midi_song_controller_stream(
                Path::new(project),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                (
                    parse_u8(raw_channel, "raw controller stream channel")?,
                    parse_u8(raw_flags, "raw controller stream flags")?,
                ),
                parse_u8(controller, "MIDI Control Change number")?,
                parse_midi_channel_number(midi_channel)?,
                MidiExportFileFormat::Format1,
            )
        }
        [command, project, output, arrangement_id, raw_channel, raw_flags, controller, midi_channel, file_format]
            if command == "export-midi-song-cc" =>
        {
            export_midi_song_controller_stream(
                Path::new(project),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                (
                    parse_u8(raw_channel, "raw controller stream channel")?,
                    parse_u8(raw_flags, "raw controller stream flags")?,
                ),
                parse_u8(controller, "MIDI Control Change number")?,
                parse_midi_channel_number(midi_channel)?,
                parse_midi_export_file_format(file_format)?,
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
        [command, input, output, pattern_id] if command == "duplicate-pattern" => {
            duplicate_pattern(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
            )
        }
        [command, input, output, pattern_id, name] if command == "rename-pattern" => {
            rename_pattern(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                name,
            )
        }
        [command, input, output, pattern_id, color] if command == "set-pattern-color" => {
            set_pattern_color(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_rgb_hex(color)?,
            )
        }
        [command, input, output, pattern_id] if command == "delete-pattern" => {
            delete_pattern(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
            )
        }
        [command, input, output, cents] if command == "set-project-master-pitch" => {
            write_project_settings(
                Path::new(input),
                Path::new(output),
                ProjectSettingsEdit {
                    master_pitch_cents: Some(parse_i16(cents, "Master pitch cents")?),
                    ..ProjectSettingsEdit::default()
                },
            )
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
        [command, input, output, pattern_id, position, name]
            if command == "create-pattern-time-marker" =>
        {
            edit_pattern_time_marker_file(Path::new(input), Path::new(output), parse_u16(pattern_id, "pattern id")?, None, Some(parse_u32(position, "marker position")?), Some(name))
        }
        [command, input, output, pattern_id, marker_index, position, name]
            if command == "edit-pattern-time-marker" =>
        {
            edit_pattern_time_marker_file(Path::new(input), Path::new(output), parse_u16(pattern_id, "pattern id")?, Some(parse_usize(marker_index, "marker index")?), Some(parse_u32(position, "marker position")?), Some(name))
        }
        [command, input, output, pattern_id, marker_index]
            if command == "delete-pattern-time-marker" =>
        {
            edit_pattern_time_marker_file(Path::new(input), Path::new(output), parse_u16(pattern_id, "pattern id")?, Some(parse_usize(marker_index, "marker index")?), None, None)
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
        [command, input, output, channel_id, name] if command == "duplicate-channel" => {
            duplicate_channel(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                name,
            )
        }
        [command, input, output, channel_id, name] if command == "rename-channel" => {
            let channel_id = channel_id
                .parse::<u16>()
                .map_err(|_| "channel id must be an integer from 0 through 65535".to_owned())?;
            rename_channel(Path::new(input), Path::new(output), channel_id, name)
        }
        [command, input, output, sample_path, name] if command == "create-sampler-channel" => {
            create_sample_channel(Path::new(input), Path::new(output), sample_path, name, false)
        }
        [command, input, output, sample_path, name] if command == "create-audio-channel" => {
            create_sample_channel(Path::new(input), Path::new(output), sample_path, name, true)
        }
        [command, input, output, name] if command == "create-channel-group" => {
            create_channel_group(Path::new(input), Path::new(output), name)
        }
        [command, input, output, channel_ids, name] if command == "group-channels" => {
            group_channels(
                Path::new(input),
                Path::new(output),
                &parse_channel_id_list(channel_ids)?,
                name,
            )
        }
        [command, input, output, group_index, name] if command == "rename-channel-group" => {
            rename_channel_group(
                Path::new(input),
                Path::new(output),
                parse_channel_group_index(group_index)?,
                name,
            )
        }
        [command, input, output, group_index] if command == "delete-channel-group" => {
            delete_channel_group(
                Path::new(input),
                Path::new(output),
                parse_channel_group_index(group_index)?,
            )
        }
        [command, input, output, channel_id, color] if command == "set-channel-color" => {
            set_channel_color(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_rgb_hex(color)?,
            )
        }
        [command, input, output, channel_id, target_index] if command == "move-channel" => {
            move_channel(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_usize(target_index, "target Channel Rack index")?,
            )
        }
        [command, input, output, channel_ids] if command == "reorder-channels" => {
            reorder_channels(
                Path::new(input),
                Path::new(output),
                &parse_channel_id_list(channel_ids)?,
            )
        }
        [command, input, output, order] if command == "sort-channels" => {
            sort_channels(
                Path::new(input),
                Path::new(output),
                parse_channel_sort_order(order)?,
            )
        }
        [command, input, output, channel_ids, color] if command == "set-channel-colors" => {
            set_channel_colors(
                Path::new(input),
                Path::new(output),
                &parse_channel_id_list(channel_ids)?,
                parse_rgb_hex(color)?,
            )
        }
        [command, input, output, channel_ids, start_color, end_color]
            if command == "set-channel-color-gradient" =>
        {
            set_channel_color_gradient(
                Path::new(input),
                Path::new(output),
                &parse_channel_id_list(channel_ids)?,
                parse_rgb_hex(start_color)?,
                parse_rgb_hex(end_color)?,
            )
        }
        [command, input, output, channel_id, enabled]
            if command == "set-channel-main-pitch" =>
        {
            set_channel_main_pitch(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_bool(enabled, "Enable main pitch setting")?,
            )
        }
        [command, input, output, channel_id, low_key, high_key]
            if command == "set-channel-key-region" =>
        {
            set_channel_key_region(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_u8(low_key, "low MIDI key")?,
                parse_u8(high_key, "high MIDI key")?,
            )
        }
        [command, input, output, channel_id, enabled] if command == "set-channel-enabled" => {
            set_channel_enabled(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_bool(enabled, "channel enabled setting")?,
            )
        }
        [command, input, output, channel_id, sample_path]
            if command == "set-channel-sample-path" =>
        {
            set_channel_sample_path(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                sample_path,
            )
        }
        [command, input, output, channel_id, reversed]
            if command == "set-channel-sample-reversed" =>
        {
            set_channel_sample_reversed(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_bool(reversed, "sample Reverse setting")?,
            )
        }
        [command, input, output, channel_id, normalize, reverse_polarity]
            if command == "edit-channel-sample-effects" =>
        {
            edit_channel_sample_effects(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "sample-bearing channel id")?,
                ChannelSampleEffectsEdit {
                    normalize: parse_optional_bool(normalize, "sample Normalize setting")?,
                    reverse_polarity: parse_optional_bool(
                        reverse_polarity,
                        "sample Reverse Polarity setting",
                    )?,
                    remove_dc: None,
                },
            )
        }
        [command, input, output, channel_id, normalize, reverse_polarity, remove_dc]
            if command == "edit-channel-sample-effects" =>
        {
            edit_channel_sample_effects(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "sample-bearing channel id")?,
                ChannelSampleEffectsEdit {
                    normalize: parse_optional_bool(normalize, "sample Normalize setting")?,
                    reverse_polarity: parse_optional_bool(
                        reverse_polarity,
                        "sample Reverse Polarity setting",
                    )?,
                    remove_dc: parse_optional_bool(remove_dc, "sample Remove DC setting")?,
                },
            )
        }
        [command, input, output, channel_id, fade_stereo, swap_stereo]
            if command == "edit-channel-sample-fx-flags" =>
        {
            edit_channel_sample_fx_flags(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "sample-bearing channel id")?,
                ChannelSampleFxFlagsEdit {
                    fade_stereo: parse_optional_bool(fade_stereo, "sample Fade Stereo setting")?,
                    swap_stereo: parse_optional_bool(swap_stereo, "sample Swap Stereo setting")?,
                },
            )
        }
        [command, input, output, channel_id, start_fraction, length_fraction]
            if command == "edit-channel-sample-window" =>
        {
            edit_channel_sample_window(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "sample-bearing channel id")?,
                ChannelSampleWindowEdit {
                    start_fraction: parse_optional_f64(
                        start_fraction,
                        "sample start fraction",
                    )?,
                    length_fraction: parse_optional_f64(
                        length_fraction,
                        "sample length fraction",
                    )?,
                },
            )
        }
        [command, input, output, channel_id, threshold_raw]
            if command == "edit-channel-sample-trim" =>
        {
            edit_channel_sample_trim(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "sample-bearing channel id")?,
                ChannelSampleTrimEdit {
                    threshold_raw: Some(parse_u32(threshold_raw, "sample Trim threshold")?),
                },
            )
        }
        [command, input, output, channel_id, amount_raw]
            if command == "edit-channel-sample-crossfade" =>
        {
            edit_channel_sample_crossfade(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "sample-bearing channel id")?,
                ChannelSampleCrossfadeEdit {
                    amount_raw: Some(parse_u32(amount_raw, "sample Crossfade amount")?),
                },
            )
        }
        [command, input, output, channel_id, cutoff_raw, resonance_raw, filter_type_raw]
            if command == "edit-channel-sampler-filter" =>
        {
            edit_channel_sampler_filter(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "Sampler channel id")?,
                ChannelSamplerFilterEdit {
                    cutoff_raw: parse_optional_u32(cutoff_raw, "Sampler filter cutoff")?,
                    resonance_raw: parse_optional_u32(
                        resonance_raw,
                        "Sampler filter resonance",
                    )?,
                    filter_type_raw: parse_optional_u32(filter_type_raw, "Sampler filter type")?,
                },
            )
        }
        [command, input, output, event_index, record_index, value]
            if command == "set-mixer-parameter-record-value" =>
        {
            let value = value
                .parse::<i32>()
                .map_err(|_| "Mixer parameter value must be a signed 32-bit integer".to_owned())?;
            set_mixer_parameter_record_value(
                Path::new(input),
                Path::new(output),
                parse_usize(event_index, "Mixer event index")?,
                parse_usize(record_index, "Mixer record index")?,
                value,
            )
        }
        [command, input, output, insert_ordinal, kind, value]
            if command == "set-mixer-eq-parameter-raw" =>
        {
            let kind = parse_mixer_eq_parameter_kind(kind)?;
            let value = value
                .parse::<i32>()
                .map_err(|_| "Mixer EQ raw value must be a signed 32-bit integer".to_owned())?;
            set_mixer_eq_parameter_raw(
                Path::new(input),
                Path::new(output),
                parse_usize(insert_ordinal, "Mixer insert ordinal")?,
                kind,
                value,
            )
        }
        [command, input, output, insert_ordinal, name]
            if command == "rename-mixer-insert" =>
        {
            rename_mixer_insert(
                Path::new(input),
                Path::new(output),
                parse_usize(insert_ordinal, "Mixer insert ordinal")?,
                name,
            )
        }
        [command, input, output, insert_ordinal, slot_index, enabled]
            if command == "set-mixer-effect-slot-enabled" =>
        {
            set_mixer_effect_slot_enabled(
                Path::new(input),
                Path::new(output),
                parse_usize(insert_ordinal, "Mixer insert ordinal")?,
                parse_u16(slot_index, "Mixer effect slot index")?,
                parse_bool(enabled, "effect-slot enabled setting")?,
            )
        }
        [command, input, output, insert_ordinal, slot_index, color]
            if command == "set-mixer-effect-slot-color" =>
        {
            let [red, green, blue] = parse_rgb_hex(color)?;
            let color_raw = u32::from(red) | (u32::from(green) << 8) | (u32::from(blue) << 16);
            set_mixer_effect_slot_color(
                Path::new(input),
                Path::new(output),
                parse_usize(insert_ordinal, "Mixer insert ordinal")?,
                parse_u16(slot_index, "Mixer effect slot index")?,
                color_raw,
            )
        }
        [command, input, output, insert_ordinal, slot_index, value]
            if command == "set-mixer-effect-slot-mix-raw" =>
        {
            let value = value
                .parse::<i32>()
                .map_err(|_| "effect-slot Mix value must be a signed 32-bit integer".to_owned())?;
            set_mixer_effect_slot_mix_raw(
                Path::new(input),
                Path::new(output),
                parse_usize(insert_ordinal, "Mixer insert ordinal")?,
                parse_u16(slot_index, "Mixer effect slot index")?,
                value,
            )
        }
        [command, input, output, insert_ordinal, from_slot, to_slot]
            if command == "move-mixer-effect-slot" =>
        {
            move_mixer_effect_slot(
                Path::new(input),
                Path::new(output),
                parse_usize(insert_ordinal, "Mixer insert ordinal")?,
                parse_u16(from_slot, "source Mixer effect slot index")?,
                parse_u16(to_slot, "destination Mixer effect slot index")?,
            )
        }
        [command, input, output, insert_ordinal, field, enabled]
            if command == "set-mixer-insert-flag" =>
        {
            set_mixer_insert_flag(
                Path::new(input),
                Path::new(output),
                parse_usize(insert_ordinal, "Mixer insert ordinal")?,
                field,
                parse_bool(enabled, "Mixer insert flag")?,
            )
        }
        [command, input, output, event_index, routing_flag_index, enabled]
            if command == "set-mixer-routing-flag" =>
        {
            set_mixer_routing_flag(
                Path::new(input),
                Path::new(output),
                parse_usize(event_index, "Mixer routing event index")?,
                parse_usize(routing_flag_index, "Mixer routing flag byte index")?,
                parse_bool(enabled, "Mixer routing flag")?,
            )
        }
        [command, input, output, channel_id, zipped] if command == "set-channel-zipped" => {
            set_channel_zipped(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_bool(zipped, "channel zipped setting")?,
            )
        }
        [command, input, output, channel_id, enabled]
            if command == "set-channel-add-to-key" =>
        {
            set_channel_add_to_key(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_bool(enabled, "Add to Key setting")?,
            )
        }
        [command, input, output, channel_id, mixer_track]
            if command == "set-channel-mixer-track" =>
        {
            set_channel_mixer_track(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_mixer_track(mixer_track)?,
            )
        }
        [command, input, output, channel_id, midi_channel, note_color_channel]
            if command == "set-channel-midi-out-settings" =>
        {
            let midi_channel = if midi_channel == "-" {
                None
            } else {
                Some(parse_u8(midi_channel, "MIDI Out channel number")?)
            };
            set_channel_midi_out_settings(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                ChannelMidiOutSettingsEdit {
                    midi_channel,
                    use_note_color_channel: parse_optional_bool(
                        note_color_channel,
                        "MIDI Out note-color channel setting",
                    )?,
                },
            )
        }
        [command, input, output, channel_id, output_bus_index, mixer_track]
            if command == "set-vst3-output-route" =>
        {
            set_vst3_output_route(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_usize(output_bus_index, "zero-based VST3 output bus index")?,
                parse_mixer_track(mixer_track)?,
            )
        }
        [command, input, output, channel_id, time_raw, pitch_raw, multiplier_raw, mode_raw]
            if command == "edit-channel-stretch-settings" =>
        {
            edit_channel_stretch_settings(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                ChannelStretchSettingsEdit {
                    time_raw: parse_optional_u32(time_raw, "stretch time raw")?,
                    pitch_raw: parse_optional_i32(pitch_raw, "stretch pitch raw")?,
                    multiplier_raw: parse_optional_i32(
                        multiplier_raw,
                        "stretch multiplier raw",
                    )?,
                    mode_raw: parse_optional_i32(mode_raw, "stretch mode raw")?,
                },
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
        [command, input, output, channel_id, fade_out, fade_in]
            if command == "set-sampler-quick-fades" =>
        {
            set_sampler_quick_fades(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "Sampler channel id")?,
                parse_u16(fade_out, "Sampler Fade Out raw value")?,
                parse_u16(fade_in, "Sampler Fade In raw value")?,
            )
        }
        [command, input, output, channel_id, enabled, slide_raw]
            if command == "set-sampler-portamento" =>
        {
            set_sampler_portamento(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "Sampler channel id")?,
                parse_optional_bool(enabled, "Sampler Porta enabled")?,
                parse_optional_u32(slide_raw, "Sampler Polyphony Slide raw value")?,
            )
        }
        [command, input, output, channel_id, enabled, tempo_sync, predelay, attack, hold, decay, sustain, release, amount, attack_tension, decay_tension, release_tension]
            if command == "edit-sampler-volume-envelope" =>
        {
            edit_sampler_volume_envelope(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "Sampler channel id")?,
                SamplerVolumeEnvelopeEdit {
                    enabled: parse_optional_bool(enabled, "Volume envelope enabled")?,
                    tempo_sync: parse_optional_bool(tempo_sync, "Volume envelope tempo sync")?,
                    predelay_raw: parse_optional_i32(predelay, "envelope predelay raw")?,
                    attack_raw: parse_optional_i32(attack, "envelope attack raw")?,
                    hold_raw: parse_optional_i32(hold, "envelope hold raw")?,
                    decay_raw: parse_optional_i32(decay, "envelope decay raw")?,
                    sustain_raw: parse_optional_i32(sustain, "envelope sustain raw")?,
                    release_raw: parse_optional_i32(release, "envelope release raw")?,
                    amount_raw: parse_optional_i32(amount, "envelope amount raw")?,
                    attack_tension_raw: parse_optional_i32(
                        attack_tension,
                        "envelope attack tension raw",
                    )?,
                    decay_tension_raw: parse_optional_i32(
                        decay_tension,
                        "envelope decay tension raw",
                    )?,
                    release_tension_raw: parse_optional_i32(
                        release_tension,
                        "envelope release tension raw",
                    )?,
                    ..SamplerVolumeEnvelopeEdit::default()
                },
            )
        }
        [command, input, output, channel_id, target, enabled, tempo_sync, lfo_tempo_sync, lfo_retrigger, predelay, attack, hold, decay, sustain, release, amount, attack_tension, decay_tension, release_tension, lfo_predelay, lfo_attack, lfo_amount, lfo_speed, lfo_shape]
            if command == "edit-sampler-envelope-lfo" =>
        {
            edit_sampler_envelope_lfo(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "Sampler channel id")?,
                parse_sampler_envelope_lfo_target(target)?,
                SamplerEnvelopeLfoEdit {
                    enabled: parse_optional_bool(enabled, "envelope enabled")?,
                    tempo_sync: parse_optional_bool(tempo_sync, "envelope tempo sync")?,
                    lfo_tempo_sync: parse_optional_bool(lfo_tempo_sync, "LFO tempo sync")?,
                    lfo_phase_retrigger: parse_optional_bool(lfo_retrigger, "LFO phase retrigger")?,
                    predelay_raw: parse_optional_i32(predelay, "envelope predelay raw")?,
                    attack_raw: parse_optional_i32(attack, "envelope attack raw")?,
                    hold_raw: parse_optional_i32(hold, "envelope hold raw")?,
                    decay_raw: parse_optional_i32(decay, "envelope decay raw")?,
                    sustain_raw: parse_optional_i32(sustain, "envelope sustain raw")?,
                    release_raw: parse_optional_i32(release, "envelope release raw")?,
                    amount_raw: parse_optional_i32(amount, "envelope amount raw")?,
                    attack_tension_raw: parse_optional_i32(
                        attack_tension,
                        "envelope attack tension raw",
                    )?,
                    decay_tension_raw: parse_optional_i32(
                        decay_tension,
                        "envelope decay tension raw",
                    )?,
                    release_tension_raw: parse_optional_i32(
                        release_tension,
                        "envelope release tension raw",
                    )?,
                    lfo_predelay_raw: parse_optional_u32(lfo_predelay, "LFO predelay raw")?,
                    lfo_attack_raw: parse_optional_u32(lfo_attack, "LFO attack raw")?,
                    lfo_amount_raw: parse_optional_i32(lfo_amount, "LFO amount raw")?,
                    lfo_speed_raw: parse_optional_u32(lfo_speed, "LFO speed raw")?,
                    lfo_shape_raw: parse_optional_i32(lfo_shape, "LFO shape raw")?,
                },
            )
        }
        [command, input, output, channel_id, cents] if command == "set-channel-fine-tune" => {
            let cents = cents
                .parse::<i32>()
                .map_err(|_| "channel Fine Tune must be a signed integer in -100..=100".to_owned())?;
            set_channel_fine_tune(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                cents,
            )
        }
        [command, input, output, channel_id, event_index, pan, volume, mod_x, mod_y]
            if command == "edit-channel-level-adjustments" =>
        {
            edit_channel_level_adjustments(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_usize(event_index, "event index")?,
                ChannelLevelAdjustmentsEdit {
                    pan_raw: parse_optional_i32(pan, "raw channel pan")?,
                    volume_raw: parse_optional_u32(volume, "raw channel volume")?,
                    mod_x_raw: parse_optional_i32(mod_x, "raw channel Mod X")?,
                    mod_y_raw: parse_optional_i32(mod_y, "raw channel Mod Y")?,
                },
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
        [command, input, output, channel_id, fade_raw] if command == "set-layer-fade" => {
            let fade_raw = parse_u16(fade_raw, "Layer Fade")?;
            if fade_raw > 256 {
                return Err("Layer Fade must be between 0 and 256".to_owned());
            }
            set_layer_fade(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "Layer channel id")?,
                fade_raw,
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
        [command, input, output, channel_id, point_index, trailing_bytes]
            if command == "set-automation-point-tail" =>
        {
            set_automation_point_trailing_bytes(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "automation channel id")?,
                parse_usize(point_index, "automation point index")?,
                parse_automation_point_tail(trailing_bytes)?,
            )
        }
        [command, input, output, channel_id, amount]
            if command == "set-automation-lfo-amount" =>
        {
            set_automation_lfo_amount(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "automation channel id")?,
                i32::from(parse_i16(amount, "automation LFO amount")?),
            )
        }
        [command, input, output, channel_id, minimum_raw, maximum_raw]
            if command == "set-automation-range-raw" =>
        {
            set_automation_range_raw(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "Automation channel id")?,
                parse_i32(minimum_raw, "Automation minimum raw value")?,
                parse_i32(maximum_raw, "Automation maximum raw value")?,
            )
        }
        [command, input, output, channel_id] if command == "flip-automation-points" => {
            flip_automation_points(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "automation channel id")?,
            )
        }
        [command, input, output, channel_id] if command == "normalize-automation-points" => {
            normalize_automation_points(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "automation channel id")?,
            )
        }
        [command, input, output, channel_id, center, tension, multiplier, offset]
            if command == "scale-automation-levels" =>
        {
            scale_automation_levels(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "automation channel id")?,
                AutomationScaleLevelsOptions {
                    center_percent: parse_i16(center, "Automation Scale Levels center")?,
                    tension_percent: parse_i16(tension, "Automation Scale Levels tension")?,
                    multiplier_percent: parse_i16(multiplier, "Automation Scale Levels multiplier")?,
                    offset_percent: parse_i16(offset, "Automation Scale Levels offset")?,
                },
            )
        }
        [command, input, output, source_channel_id, target_channel_id]
            if command == "copy-automation-state" =>
        {
            copy_automation_state(
                Path::new(input),
                Path::new(output),
                parse_u16(source_channel_id, "source automation channel id")?,
                parse_u16(target_channel_id, "target automation channel id")?,
            )
        }
        [command, input, output, channel_id, template_channel_id]
            if command == "init-automation-points" =>
        {
            initialize_automation_points_from_template(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "automation channel id")?,
                parse_u16(template_channel_id, "template automation channel id")?,
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
        [command, input, output, channel_id, first_point_index, last_point_index]
            if command == "interpolate-automation-points" =>
        {
            interpolate_automation_points(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_usize(first_point_index, "first automation point index")?,
                parse_usize(last_point_index, "last automation point index")?,
                AutomationInterpolationMode::Linear,
            )
        }
        [command, input, output, channel_id, first_point_index, last_point_index, mode]
            if command == "interpolate-automation-points" =>
        {
            interpolate_automation_points(
                Path::new(input),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                parse_usize(first_point_index, "first automation point index")?,
                parse_usize(last_point_index, "last automation point index")?,
                parse_automation_interpolation_mode(mode)?,
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
        [command, input, output, pattern_id, channel_id, note_index, enabled]
            if command == "set-note-porta" =>
        {
            edit_note(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                parse_usize(note_index, "channel note index")?,
                PatternNoteEdit {
                    porta: Some(parse_bool(enabled, "note Porta setting")?),
                    ..PatternNoteEdit::default()
                },
            )
        }
        [command, input, output, pattern_id, channel_id, groove_path, strength, sensitivity,
            duration_strength, mode, velocity_level, pan_level, release_level, mod_x_level,
            mod_y_level]
            if command == "quantize-notes-groove" =>
        {
            quantize_notes_with_score_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                None,
                Path::new(groove_path),
                QuantizeNoteOptions {
                    start_strength: parse_f64(strength, "quantize start strength")?,
                    sensitivity: parse_f64(sensitivity, "quantize sensitivity")?,
                    duration_strength: parse_f64(duration_strength, "quantize duration strength")?,
                    mode: parse_quantize_note_mode(mode)?,
                    levels: QuantizeNoteLevels {
                        velocity: parse_f64(velocity_level, "quantize velocity level")?,
                        pan: parse_f64(pan_level, "quantize pan level")?,
                        release: parse_f64(release_level, "quantize release level")?,
                        mod_x: parse_f64(mod_x_level, "quantize Mod X level")?,
                        mod_y: parse_f64(mod_y_level, "quantize Mod Y level")?,
                    },
                    ..QuantizeNoteOptions::default()
                },
            )
        }
        [command, input, output, pattern_id, channel_id, groove_path, strength, sensitivity,
            duration_strength, mode]
            if command == "quantize-notes-groove" =>
        {
            quantize_notes_with_score_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                None,
                Path::new(groove_path),
                QuantizeNoteOptions {
                    start_strength: parse_f64(strength, "quantize start strength")?,
                    sensitivity: parse_f64(sensitivity, "quantize sensitivity")?,
                    duration_strength: parse_f64(duration_strength, "quantize duration strength")?,
                    mode: parse_quantize_note_mode(mode)?,
                    ..QuantizeNoteOptions::default()
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, groove_path, strength,
            sensitivity, duration_strength, mode, velocity_level, pan_level, release_level,
            mod_x_level, mod_y_level]
            if command == "quantize-notes-groove-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            quantize_notes_with_score_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                Path::new(groove_path),
                QuantizeNoteOptions {
                    start_strength: parse_f64(strength, "quantize start strength")?,
                    sensitivity: parse_f64(sensitivity, "quantize sensitivity")?,
                    duration_strength: parse_f64(duration_strength, "quantize duration strength")?,
                    mode: parse_quantize_note_mode(mode)?,
                    levels: QuantizeNoteLevels {
                        velocity: parse_f64(velocity_level, "quantize velocity level")?,
                        pan: parse_f64(pan_level, "quantize pan level")?,
                        release: parse_f64(release_level, "quantize release level")?,
                        mod_x: parse_f64(mod_x_level, "quantize Mod X level")?,
                        mod_y: parse_f64(mod_y_level, "quantize Mod Y level")?,
                    },
                    ..QuantizeNoteOptions::default()
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, groove_path, strength,
            sensitivity, duration_strength, mode]
            if command == "quantize-notes-groove-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            quantize_notes_with_score_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                Path::new(groove_path),
                QuantizeNoteOptions {
                    start_strength: parse_f64(strength, "quantize start strength")?,
                    sensitivity: parse_f64(sensitivity, "quantize sensitivity")?,
                    duration_strength: parse_f64(duration_strength, "quantize duration strength")?,
                    mode: parse_quantize_note_mode(mode)?,
                    ..QuantizeNoteOptions::default()
                },
            )
        }
        [command, input, output, pattern_id, channel_id, grid_ticks, strength, swing,
            sensitivity, duration_strength, mode]
            if command == "quantize-notes" =>
        {
            quantize_notes_with_options(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                QuantizeNoteOptions {
                    grid_ticks: parse_u32(grid_ticks, "quantize grid ticks")?,
                    start_strength: parse_f64(strength, "quantize start strength")?,
                    swing: parse_f64(swing, "quantize swing")?,
                    sensitivity: parse_f64(sensitivity, "quantize sensitivity")?,
                    duration_strength: parse_f64(duration_strength, "quantize duration strength")?,
                    mode: parse_quantize_note_mode(mode)?,
                    levels: QuantizeNoteLevels::default(),
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, grid_ticks, strength, swing,
            sensitivity, duration_strength, mode]
            if command == "quantize-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            quantize_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                QuantizeNoteOptions {
                    grid_ticks: parse_u32(grid_ticks, "quantize grid ticks")?,
                    start_strength: parse_f64(strength, "quantize start strength")?,
                    swing: parse_f64(swing, "quantize swing")?,
                    sensitivity: parse_f64(sensitivity, "quantize sensitivity")?,
                    duration_strength: parse_f64(duration_strength, "quantize duration strength")?,
                    mode: parse_quantize_note_mode(mode)?,
                    levels: QuantizeNoteLevels::default(),
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
        [command, input, output, pattern_id, channel_id, note_indices, grid_ticks, strength, swing]
            if command == "quantize-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            quantize_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                QuantizeNoteOptions {
                    grid_ticks: parse_u32(grid_ticks, "quantize grid ticks")?,
                    start_strength: parse_f64(strength, "quantize strength")?,
                    swing: parse_f64(swing, "quantize swing")?,
                    ..QuantizeNoteOptions::default()
                },
            )
        }
        [command, input, output, pattern_id, channel_id, snap_ticks, direction]
            if command == "rotate-notes" =>
        {
            rotate_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                None,
                parse_u32(snap_ticks, "note rotation snap ticks")?,
                parse_note_rotation(direction)?,
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, snap_ticks, direction]
            if command == "rotate-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            rotate_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                parse_u32(snap_ticks, "note rotation snap ticks")?,
                parse_note_rotation(direction)?,
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices]
            if command == "legato-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            legato_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
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
        [command, input, output, pattern_id, channel_id, note_indices, divisions]
            if command == "chop-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            chop_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                parse_u8(divisions, "note chop divisions")?,
            )
        }
        [command, input, output, pattern_id, channel_id, snap_ticks]
            if command == "quick-chop-notes" =>
        {
            quick_chop_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                None,
                parse_u32(snap_ticks, "quick chop snap ticks")?,
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, snap_ticks]
            if command == "quick-chop-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            quick_chop_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                parse_u32(snap_ticks, "quick chop snap ticks")?,
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
        [command, input, output, pattern_id, channel_id, note_indices]
            if command == "glue-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            glue_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
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
        [command, input, output, pattern_id, channel_id, note_indices]
            if command == "flip-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            flip_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
            )
        }
        [command, input, output, pattern_id, channel_id, _, _, _, _, _, _, _, _, _, _, _]
            if command == "strum-notes" =>
        {
            strum_notes_with_options(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                parse_strum_options(&args[5..])?,
            )
        }
        [command, input, output, pattern_id, channel_id, spread_ticks, direction,
            velocity_amount, velocity_tension, preserve_end, alternate_direction]
            if command == "strum-notes" =>
        {
            strum_notes_with_options(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                StrumOptions {
                    spread_ticks: parse_u32(spread_ticks, "strum spread")?,
                    descending: parse_strum_direction(direction)?,
                    velocity_amount: parse_i16(velocity_amount, "strum velocity amount")?,
                    velocity_tension_percent: parse_i16(
                        velocity_tension,
                        "strum velocity tension",
                    )?,
                    preserve_end: parse_bool(preserve_end, "preserve end")?,
                    alternate_direction: parse_bool(alternate_direction, "alternate direction")?,
                    ..StrumOptions::default()
                },
            )
        }
        [command, input, output, pattern_id, channel_id, spread_ticks, direction,
            velocity_amount, velocity_tension, preserve_end]
            if command == "strum-notes" =>
        {
            strum_notes_with_options(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                StrumOptions {
                    spread_ticks: parse_u32(spread_ticks, "strum spread")?,
                    descending: parse_strum_direction(direction)?,
                    velocity_amount: parse_i16(velocity_amount, "strum velocity amount")?,
                    velocity_tension_percent: parse_i16(
                        velocity_tension,
                        "strum velocity tension",
                    )?,
                    preserve_end: parse_bool(preserve_end, "preserve end")?,
                    ..StrumOptions::default()
                },
            )
        }
        [command, input, output, pattern_id, channel_id, spread_ticks, direction,
            velocity_amount, velocity_tension]
            if command == "strum-notes" =>
        {
            let descending = match direction.as_str() {
                "up" => false,
                "down" => true,
                _ => return Err("strum direction must be 'up' or 'down'".to_owned()),
            };
            strum_notes_with_options(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                StrumOptions {
                    spread_ticks: parse_u32(spread_ticks, "strum spread")?,
                    descending,
                    velocity_amount: parse_i16(velocity_amount, "strum velocity amount")?,
                    velocity_tension_percent: parse_i16(
                        velocity_tension,
                        "strum velocity tension",
                    )?,
                    ..StrumOptions::default()
                },
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
        [command, input, output, pattern_id, channel_id, note_indices, _, _, _, _, _, _, _, _, _, _, _]
            if command == "strum-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            strum_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                parse_strum_options(&args[6..])?,
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, spread_ticks, direction,
            velocity_amount, velocity_tension, preserve_end, alternate_direction]
            if command == "strum-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            strum_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                StrumOptions {
                    spread_ticks: parse_u32(spread_ticks, "strum spread")?,
                    descending: parse_strum_direction(direction)?,
                    velocity_amount: parse_i16(velocity_amount, "strum velocity amount")?,
                    velocity_tension_percent: parse_i16(
                        velocity_tension,
                        "strum velocity tension",
                    )?,
                    preserve_end: parse_bool(preserve_end, "preserve end")?,
                    alternate_direction: parse_bool(alternate_direction, "alternate direction")?,
                    ..StrumOptions::default()
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, spread_ticks, direction,
            velocity_amount, velocity_tension, preserve_end]
            if command == "strum-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            strum_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                StrumOptions {
                    spread_ticks: parse_u32(spread_ticks, "strum spread")?,
                    descending: parse_strum_direction(direction)?,
                    velocity_amount: parse_i16(velocity_amount, "strum velocity amount")?,
                    velocity_tension_percent: parse_i16(
                        velocity_tension,
                        "strum velocity tension",
                    )?,
                    preserve_end: parse_bool(preserve_end, "preserve end")?,
                    ..StrumOptions::default()
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, spread_ticks, direction,
            velocity_amount, velocity_tension]
            if command == "strum-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            strum_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                StrumOptions {
                    spread_ticks: parse_u32(spread_ticks, "strum spread")?,
                    descending: parse_strum_direction(direction)?,
                    velocity_amount: parse_i16(velocity_amount, "strum velocity amount")?,
                    velocity_tension_percent: parse_i16(
                        velocity_tension,
                        "strum velocity tension",
                    )?,
                    ..StrumOptions::default()
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, spread_ticks, direction]
            if command == "strum-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            strum_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                StrumOptions {
                    spread_ticks: parse_u32(spread_ticks, "strum spread")?,
                    descending: parse_strum_direction(direction)?,
                    ..StrumOptions::default()
                },
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
        [command, input, output, pattern_id, channel_id, stroke_ticks, velocity, placement,
            group_notes]
            if command == "flam-notes" =>
        {
            let before = parse_flam_placement(placement)?;
            flam_notes_with_options(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                None,
                FlamOptions {
                    stroke_ticks: parse_u32(stroke_ticks, "flam stroke time")?,
                    velocity: parse_u8(velocity, "flam velocity")?,
                    before,
                    group_notes: parse_bool(group_notes, "group notes")?,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, stroke_ticks, velocity,
            placement]
            if command == "flam-notes-selected" =>
        {
            let before = parse_flam_placement(placement)?;
            let note_indices = parse_note_indices(note_indices)?;
            flam_notes_with_options(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                FlamOptions {
                    stroke_ticks: parse_u32(stroke_ticks, "flam stroke time")?,
                    velocity: parse_u8(velocity, "flam velocity")?,
                    before,
                    group_notes: false,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, stroke_ticks, velocity,
            placement, group_notes]
            if command == "flam-notes-selected" =>
        {
            let before = parse_flam_placement(placement)?;
            let note_indices = parse_note_indices(note_indices)?;
            flam_notes_with_options(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                FlamOptions {
                    stroke_ticks: parse_u32(stroke_ticks, "flam stroke time")?,
                    velocity: parse_u8(velocity, "flam velocity")?,
                    before,
                    group_notes: parse_bool(group_notes, "group notes")?,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, start_tick, end_tick, step_ticks,
            note_length_ticks, density, minimum_key, maximum_key, seed, stack, length_variation,
            merge_same_notes, base_octave, range_octaves, chord_map_name, chord_root]
            if command == "generate-random-notes" =>
        {
            generate_random_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomNoteGeneratorOptions {
                    seed: parse_u64(seed, "random note generator seed")?,
                    start_tick: parse_u32(start_tick, "generation start tick")?,
                    end_tick: parse_u32(end_tick, "generation end tick")?,
                    step_ticks: parse_u32(step_ticks, "generation step")?,
                    note_length_ticks: parse_u32(note_length_ticks, "generated note length")?,
                    density_percent: parse_u8(density, "generation density")?,
                    notes_per_step: parse_u8(stack, "note stack")?,
                    length_variation_percent: parse_i16(
                        length_variation,
                        "note length variation",
                    )?,
                    merge_repeated_notes: parse_bool(merge_same_notes, "merge same notes")?,
                    base_octave: Some(parse_u8(base_octave, "base octave")?),
                    range_octaves: Some(parse_u8(range_octaves, "octave range")?),
                    chord_map: Some(parse_random_note_chord_map(chord_map_name)?),
                    chord_root: parse_u8(chord_root, "chord-map root")?,
                    minimum_key: u16::from(parse_u8(minimum_key, "minimum key")?),
                    maximum_key: u16::from(parse_u8(maximum_key, "maximum key")?),
                },
                None,
            )
        }
        [command, input, output, pattern_id, channel_id, start_tick, end_tick, step_ticks,
            note_length_ticks, density, minimum_key, maximum_key, seed, scale_name, scale_root,
            stack, length_variation, merge_same_notes, base_octave, range_octaves, chord_map_name,
            chord_root]
            if command == "generate-random-notes-scale" =>
        {
            let scale_intervals = parse_scale_name(scale_name)?;
            generate_random_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomNoteGeneratorOptions {
                    seed: parse_u64(seed, "random note generator seed")?,
                    start_tick: parse_u32(start_tick, "generation start tick")?,
                    end_tick: parse_u32(end_tick, "generation end tick")?,
                    step_ticks: parse_u32(step_ticks, "generation step")?,
                    note_length_ticks: parse_u32(note_length_ticks, "generated note length")?,
                    density_percent: parse_u8(density, "generation density")?,
                    notes_per_step: parse_u8(stack, "note stack")?,
                    length_variation_percent: parse_i16(
                        length_variation,
                        "note length variation",
                    )?,
                    merge_repeated_notes: parse_bool(merge_same_notes, "merge same notes")?,
                    base_octave: Some(parse_u8(base_octave, "base octave")?),
                    range_octaves: Some(parse_u8(range_octaves, "octave range")?),
                    chord_map: Some(parse_random_note_chord_map(chord_map_name)?),
                    chord_root: parse_u8(chord_root, "chord-map root")?,
                    minimum_key: u16::from(parse_u8(minimum_key, "minimum key")?),
                    maximum_key: u16::from(parse_u8(maximum_key, "maximum key")?),
                },
                Some((parse_u8(scale_root, "scale root")?, scale_intervals)),
            )
        }
        [command, input, output, pattern_id, channel_id, start_tick, end_tick, step_ticks,
            note_length_ticks, density, minimum_key, maximum_key, seed, stack, length_variation,
            merge_same_notes, base_octave, range_octaves]
            if command == "generate-random-notes" =>
        {
            generate_random_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomNoteGeneratorOptions {
                    seed: parse_u64(seed, "random note generator seed")?,
                    start_tick: parse_u32(start_tick, "generation start tick")?,
                    end_tick: parse_u32(end_tick, "generation end tick")?,
                    step_ticks: parse_u32(step_ticks, "generation step")?,
                    note_length_ticks: parse_u32(note_length_ticks, "generated note length")?,
                    density_percent: parse_u8(density, "generation density")?,
                    notes_per_step: parse_u8(stack, "note stack")?,
                    length_variation_percent: parse_i16(
                        length_variation,
                        "note length variation",
                    )?,
                    merge_repeated_notes: parse_bool(merge_same_notes, "merge same notes")?,
                    base_octave: Some(parse_u8(base_octave, "base octave")?),
                    range_octaves: Some(parse_u8(range_octaves, "octave range")?),
                    chord_map: None,
                    chord_root: 0,
                    minimum_key: u16::from(parse_u8(minimum_key, "minimum key")?),
                    maximum_key: u16::from(parse_u8(maximum_key, "maximum key")?),
                },
                None,
            )
        }
        [command, input, output, pattern_id, channel_id, start_tick, end_tick, step_ticks,
            note_length_ticks, density, minimum_key, maximum_key, seed, scale_name, scale_root,
            stack, length_variation, merge_same_notes, base_octave, range_octaves]
            if command == "generate-random-notes-scale" =>
        {
            let scale_intervals = parse_scale_name(scale_name)?;
            generate_random_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomNoteGeneratorOptions {
                    seed: parse_u64(seed, "random note generator seed")?,
                    start_tick: parse_u32(start_tick, "generation start tick")?,
                    end_tick: parse_u32(end_tick, "generation end tick")?,
                    step_ticks: parse_u32(step_ticks, "generation step")?,
                    note_length_ticks: parse_u32(note_length_ticks, "generated note length")?,
                    density_percent: parse_u8(density, "generation density")?,
                    notes_per_step: parse_u8(stack, "note stack")?,
                    length_variation_percent: parse_i16(
                        length_variation,
                        "note length variation",
                    )?,
                    merge_repeated_notes: parse_bool(merge_same_notes, "merge same notes")?,
                    base_octave: Some(parse_u8(base_octave, "base octave")?),
                    range_octaves: Some(parse_u8(range_octaves, "octave range")?),
                    chord_map: None,
                    chord_root: 0,
                    minimum_key: u16::from(parse_u8(minimum_key, "minimum key")?),
                    maximum_key: u16::from(parse_u8(maximum_key, "maximum key")?),
                },
                Some((parse_u8(scale_root, "scale root")?, scale_intervals)),
            )
        }
        [command, input, output, pattern_id, channel_id, start_tick, end_tick, step_ticks,
            note_length_ticks, density, minimum_key, maximum_key, seed, stack, length_variation,
            merge_same_notes]
            if command == "generate-random-notes" =>
        {
            generate_random_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomNoteGeneratorOptions {
                    seed: parse_u64(seed, "random note generator seed")?,
                    start_tick: parse_u32(start_tick, "generation start tick")?,
                    end_tick: parse_u32(end_tick, "generation end tick")?,
                    step_ticks: parse_u32(step_ticks, "generation step")?,
                    note_length_ticks: parse_u32(note_length_ticks, "generated note length")?,
                    density_percent: parse_u8(density, "generation density")?,
                    notes_per_step: parse_u8(stack, "note stack")?,
                    length_variation_percent: parse_i16(
                        length_variation,
                        "note length variation",
                    )?,
                    merge_repeated_notes: parse_bool(merge_same_notes, "merge same notes")?,
                    base_octave: None,
                    range_octaves: None,
                    chord_map: None,
                    chord_root: 0,
                    minimum_key: u16::from(parse_u8(minimum_key, "minimum key")?),
                    maximum_key: u16::from(parse_u8(maximum_key, "maximum key")?),
                },
                None,
            )
        }
        [command, input, output, pattern_id, channel_id, start_tick, end_tick, step_ticks,
            note_length_ticks, density, minimum_key, maximum_key, seed, scale_name, scale_root,
            stack, length_variation, merge_same_notes]
            if command == "generate-random-notes-scale" =>
        {
            let scale_intervals = parse_scale_name(scale_name)?;
            generate_random_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomNoteGeneratorOptions {
                    seed: parse_u64(seed, "random note generator seed")?,
                    start_tick: parse_u32(start_tick, "generation start tick")?,
                    end_tick: parse_u32(end_tick, "generation end tick")?,
                    step_ticks: parse_u32(step_ticks, "generation step")?,
                    note_length_ticks: parse_u32(note_length_ticks, "generated note length")?,
                    density_percent: parse_u8(density, "generation density")?,
                    notes_per_step: parse_u8(stack, "note stack")?,
                    length_variation_percent: parse_i16(
                        length_variation,
                        "note length variation",
                    )?,
                    merge_repeated_notes: parse_bool(merge_same_notes, "merge same notes")?,
                    base_octave: None,
                    range_octaves: None,
                    chord_map: None,
                    chord_root: 0,
                    minimum_key: u16::from(parse_u8(minimum_key, "minimum key")?),
                    maximum_key: u16::from(parse_u8(maximum_key, "maximum key")?),
                },
                Some((parse_u8(scale_root, "scale root")?, scale_intervals)),
            )
        }
        [command, input, output, pattern_id, channel_id, start_tick, end_tick, step_ticks,
            note_length_ticks, density, minimum_key, maximum_key, seed, stack, length_variation]
            if command == "generate-random-notes" =>
        {
            generate_random_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomNoteGeneratorOptions {
                    seed: parse_u64(seed, "random note generator seed")?,
                    start_tick: parse_u32(start_tick, "generation start tick")?,
                    end_tick: parse_u32(end_tick, "generation end tick")?,
                    step_ticks: parse_u32(step_ticks, "generation step")?,
                    note_length_ticks: parse_u32(note_length_ticks, "generated note length")?,
                    density_percent: parse_u8(density, "generation density")?,
                    notes_per_step: parse_u8(stack, "note stack")?,
                    length_variation_percent: parse_i16(
                        length_variation,
                        "note length variation",
                    )?,
                    merge_repeated_notes: false,
                    base_octave: None,
                    range_octaves: None,
                    chord_map: None,
                    chord_root: 0,
                    minimum_key: u16::from(parse_u8(minimum_key, "minimum key")?),
                    maximum_key: u16::from(parse_u8(maximum_key, "maximum key")?),
                },
                None,
            )
        }
        [command, input, output, pattern_id, channel_id, start_tick, end_tick, step_ticks,
            note_length_ticks, density, minimum_key, maximum_key, seed, scale_name, scale_root,
            stack, length_variation]
            if command == "generate-random-notes-scale" =>
        {
            let scale_intervals = parse_scale_name(scale_name)?;
            generate_random_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomNoteGeneratorOptions {
                    seed: parse_u64(seed, "random note generator seed")?,
                    start_tick: parse_u32(start_tick, "generation start tick")?,
                    end_tick: parse_u32(end_tick, "generation end tick")?,
                    step_ticks: parse_u32(step_ticks, "generation step")?,
                    note_length_ticks: parse_u32(note_length_ticks, "generated note length")?,
                    density_percent: parse_u8(density, "generation density")?,
                    notes_per_step: parse_u8(stack, "note stack")?,
                    length_variation_percent: parse_i16(
                        length_variation,
                        "note length variation",
                    )?,
                    merge_repeated_notes: false,
                    base_octave: None,
                    range_octaves: None,
                    chord_map: None,
                    chord_root: 0,
                    minimum_key: u16::from(parse_u8(minimum_key, "minimum key")?),
                    maximum_key: u16::from(parse_u8(maximum_key, "maximum key")?),
                },
                Some((parse_u8(scale_root, "scale root")?, scale_intervals)),
            )
        }
        [command, input, output, pattern_id, channel_id, start_tick, end_tick, step_ticks,
            note_length_ticks, density, minimum_key, maximum_key, seed, stack]
            if command == "generate-random-notes" =>
        {
            generate_random_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomNoteGeneratorOptions {
                    seed: parse_u64(seed, "random note generator seed")?,
                    start_tick: parse_u32(start_tick, "generation start tick")?,
                    end_tick: parse_u32(end_tick, "generation end tick")?,
                    step_ticks: parse_u32(step_ticks, "generation step")?,
                    note_length_ticks: parse_u32(note_length_ticks, "generated note length")?,
                    density_percent: parse_u8(density, "generation density")?,
                    notes_per_step: parse_u8(stack, "note stack")?,
                    length_variation_percent: 0,
                    merge_repeated_notes: false,
                    base_octave: None,
                    range_octaves: None,
                    chord_map: None,
                    chord_root: 0,
                    minimum_key: u16::from(parse_u8(minimum_key, "minimum key")?),
                    maximum_key: u16::from(parse_u8(maximum_key, "maximum key")?),
                },
                None,
            )
        }
        [command, input, output, pattern_id, channel_id, start_tick, end_tick, step_ticks,
            note_length_ticks, density, minimum_key, maximum_key, seed, scale_name, scale_root,
            stack]
            if command == "generate-random-notes-scale" =>
        {
            let scale_intervals = parse_scale_name(scale_name)?;
            generate_random_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomNoteGeneratorOptions {
                    seed: parse_u64(seed, "random note generator seed")?,
                    start_tick: parse_u32(start_tick, "generation start tick")?,
                    end_tick: parse_u32(end_tick, "generation end tick")?,
                    step_ticks: parse_u32(step_ticks, "generation step")?,
                    note_length_ticks: parse_u32(note_length_ticks, "generated note length")?,
                    density_percent: parse_u8(density, "generation density")?,
                    notes_per_step: parse_u8(stack, "note stack")?,
                    length_variation_percent: 0,
                    merge_repeated_notes: false,
                    base_octave: None,
                    range_octaves: None,
                    chord_map: None,
                    chord_root: 0,
                    minimum_key: u16::from(parse_u8(minimum_key, "minimum key")?),
                    maximum_key: u16::from(parse_u8(maximum_key, "maximum key")?),
                },
                Some((parse_u8(scale_root, "scale root")?, scale_intervals)),
            )
        }
        [command, input, output, pattern_id, channel_id, start_tick, end_tick, step_ticks,
            note_length_ticks, density, minimum_key, maximum_key, seed]
            if command == "generate-random-notes" =>
        {
            generate_random_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomNoteGeneratorOptions {
                    seed: parse_u64(seed, "random note generator seed")?,
                    start_tick: parse_u32(start_tick, "generation start tick")?,
                    end_tick: parse_u32(end_tick, "generation end tick")?,
                    step_ticks: parse_u32(step_ticks, "generation step")?,
                    note_length_ticks: parse_u32(note_length_ticks, "generated note length")?,
                    density_percent: parse_u8(density, "generation density")?,
                    notes_per_step: 1,
                    length_variation_percent: 0,
                    merge_repeated_notes: false,
                    base_octave: None,
                    range_octaves: None,
                    chord_map: None,
                    chord_root: 0,
                    minimum_key: u16::from(parse_u8(minimum_key, "minimum key")?),
                    maximum_key: u16::from(parse_u8(maximum_key, "maximum key")?),
                },
                None,
            )
        }
        [command, input, output, pattern_id, channel_id, start_tick, end_tick, step_ticks,
            note_length_ticks, density, minimum_key, maximum_key, seed, scale_name, scale_root]
            if command == "generate-random-notes-scale" =>
        {
            let scale_intervals = parse_scale_name(scale_name)?;
            generate_random_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomNoteGeneratorOptions {
                    seed: parse_u64(seed, "random note generator seed")?,
                    start_tick: parse_u32(start_tick, "generation start tick")?,
                    end_tick: parse_u32(end_tick, "generation end tick")?,
                    step_ticks: parse_u32(step_ticks, "generation step")?,
                    note_length_ticks: parse_u32(note_length_ticks, "generated note length")?,
                    density_percent: parse_u8(density, "generation density")?,
                    notes_per_step: 1,
                    length_variation_percent: 0,
                    merge_repeated_notes: false,
                    base_octave: None,
                    range_octaves: None,
                    chord_map: None,
                    chord_root: 0,
                    minimum_key: u16::from(parse_u8(minimum_key, "minimum key")?),
                    maximum_key: u16::from(parse_u8(maximum_key, "maximum key")?),
                },
                Some((parse_u8(scale_root, "scale root")?, scale_intervals)),
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
                    release_amount_percent: 0,
                    mod_x_amount_percent: 0,
                    mod_y_amount_percent: 0,
                    pitch_range_semitones: parse_u8(pitch_range, "pitch range")?,
                    bipolar: parse_bool(bipolar, "bipolar")?,
                    reset_levels: parse_bool(reset_levels, "reset levels")?,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, seed, velocity_amount, pan_amount,
            pitch_range, bipolar, reset_levels, release_amount, mod_x_amount, mod_y_amount]
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
                    release_amount_percent: parse_i16(release_amount, "release amount")?,
                    mod_x_amount_percent: parse_i16(mod_x_amount, "Mod X amount")?,
                    mod_y_amount_percent: parse_i16(mod_y_amount, "Mod Y amount")?,
                    pitch_range_semitones: parse_u8(pitch_range, "pitch range")?,
                    bipolar: parse_bool(bipolar, "bipolar")?,
                    reset_levels: parse_bool(reset_levels, "reset levels")?,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, seed, velocity_amount, pan_amount,
            pitch_range, bipolar, reset_levels, scale_name, scale_root]
            if command == "randomize-notes-scale" =>
        {
            let scale_intervals = parse_scale_name(scale_name)?;
            randomize_notes_with_scale(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomizerOptions {
                    seed: parse_u64(seed, "randomizer seed")?,
                    velocity_amount_percent: parse_i16(velocity_amount, "velocity amount")?,
                    pan_amount_percent: parse_i16(pan_amount, "pan amount")?,
                    release_amount_percent: 0,
                    mod_x_amount_percent: 0,
                    mod_y_amount_percent: 0,
                    pitch_range_semitones: parse_u8(pitch_range, "pitch range")?,
                    bipolar: parse_bool(bipolar, "bipolar")?,
                    reset_levels: parse_bool(reset_levels, "reset levels")?,
                },
                parse_u8(scale_root, "scale root")?,
                scale_intervals,
            )
        }
        [command, input, output, pattern_id, channel_id, seed, velocity_amount, pan_amount,
            pitch_range, bipolar, reset_levels, scale_name, scale_root, release_amount,
            mod_x_amount, mod_y_amount]
            if command == "randomize-notes-scale" =>
        {
            let scale_intervals = parse_scale_name(scale_name)?;
            randomize_notes_with_scale(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                RandomizerOptions {
                    seed: parse_u64(seed, "randomizer seed")?,
                    velocity_amount_percent: parse_i16(velocity_amount, "velocity amount")?,
                    pan_amount_percent: parse_i16(pan_amount, "pan amount")?,
                    release_amount_percent: parse_i16(release_amount, "release amount")?,
                    mod_x_amount_percent: parse_i16(mod_x_amount, "Mod X amount")?,
                    mod_y_amount_percent: parse_i16(mod_y_amount, "Mod Y amount")?,
                    pitch_range_semitones: parse_u8(pitch_range, "pitch range")?,
                    bipolar: parse_bool(bipolar, "bipolar")?,
                    reset_levels: parse_bool(reset_levels, "reset levels")?,
                },
                parse_u8(scale_root, "scale root")?,
                scale_intervals,
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, seed, velocity_amount,
            pan_amount, pitch_range, bipolar, reset_levels]
            if command == "randomize-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            randomize_notes_selection(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                &note_indices,
                RandomizerOptions {
                    seed: parse_u64(seed, "randomizer seed")?,
                    velocity_amount_percent: parse_i16(velocity_amount, "velocity amount")?,
                    pan_amount_percent: parse_i16(pan_amount, "pan amount")?,
                    release_amount_percent: 0,
                    mod_x_amount_percent: 0,
                    mod_y_amount_percent: 0,
                    pitch_range_semitones: parse_u8(pitch_range, "pitch range")?,
                    bipolar: parse_bool(bipolar, "bipolar")?,
                    reset_levels: parse_bool(reset_levels, "reset levels")?,
                },
                None,
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, seed, velocity_amount,
            pan_amount, pitch_range, bipolar, reset_levels, release_amount, mod_x_amount,
            mod_y_amount]
            if command == "randomize-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            randomize_notes_selection(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                &note_indices,
                RandomizerOptions {
                    seed: parse_u64(seed, "randomizer seed")?,
                    velocity_amount_percent: parse_i16(velocity_amount, "velocity amount")?,
                    pan_amount_percent: parse_i16(pan_amount, "pan amount")?,
                    release_amount_percent: parse_i16(release_amount, "release amount")?,
                    mod_x_amount_percent: parse_i16(mod_x_amount, "Mod X amount")?,
                    mod_y_amount_percent: parse_i16(mod_y_amount, "Mod Y amount")?,
                    pitch_range_semitones: parse_u8(pitch_range, "pitch range")?,
                    bipolar: parse_bool(bipolar, "bipolar")?,
                    reset_levels: parse_bool(reset_levels, "reset levels")?,
                },
                None,
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, seed, velocity_amount,
            pan_amount, pitch_range, bipolar, reset_levels, scale_name, scale_root]
            if command == "randomize-notes-selected-scale" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            let scale_intervals = parse_scale_name(scale_name)?;
            randomize_notes_selection(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                &note_indices,
                RandomizerOptions {
                    seed: parse_u64(seed, "randomizer seed")?,
                    velocity_amount_percent: parse_i16(velocity_amount, "velocity amount")?,
                    pan_amount_percent: parse_i16(pan_amount, "pan amount")?,
                    release_amount_percent: 0,
                    mod_x_amount_percent: 0,
                    mod_y_amount_percent: 0,
                    pitch_range_semitones: parse_u8(pitch_range, "pitch range")?,
                    bipolar: parse_bool(bipolar, "bipolar")?,
                    reset_levels: parse_bool(reset_levels, "reset levels")?,
                },
                Some((parse_u8(scale_root, "scale root")?, scale_intervals)),
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, seed, velocity_amount,
            pan_amount, pitch_range, bipolar, reset_levels, scale_name, scale_root, release_amount,
            mod_x_amount, mod_y_amount]
            if command == "randomize-notes-selected-scale" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            let scale_intervals = parse_scale_name(scale_name)?;
            randomize_notes_selection(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                &note_indices,
                RandomizerOptions {
                    seed: parse_u64(seed, "randomizer seed")?,
                    velocity_amount_percent: parse_i16(velocity_amount, "velocity amount")?,
                    pan_amount_percent: parse_i16(pan_amount, "pan amount")?,
                    release_amount_percent: parse_i16(release_amount, "release amount")?,
                    mod_x_amount_percent: parse_i16(mod_x_amount, "Mod X amount")?,
                    mod_y_amount_percent: parse_i16(mod_y_amount, "Mod Y amount")?,
                    pitch_range_semitones: parse_u8(pitch_range, "pitch range")?,
                    bipolar: parse_bool(bipolar, "bipolar")?,
                    reset_levels: parse_bool(reset_levels, "reset levels")?,
                },
                Some((parse_u8(scale_root, "scale root")?, scale_intervals)),
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
                HumanizeCommandOptions::new(
                    parse_u64(seed, "humanize seed")?,
                    timing_range,
                    parse_u8(velocity_variation, "velocity variation")?,
                ),
            )
        }
        [command, input, output, pattern_id, channel_id, seed, timing_range, velocity_variation,
            pan_variation, release_variation, mod_x_variation, mod_y_variation]
            if command == "humanize-notes" =>
        {
            humanize_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                HumanizeCommandOptions {
                    pan_variation_percent: parse_u8(pan_variation, "pan variation")?,
                    release_variation_percent: parse_u8(release_variation, "release variation")?,
                    mod_x_variation_percent: parse_u8(mod_x_variation, "Mod X variation")?,
                    mod_y_variation_percent: parse_u8(mod_y_variation, "Mod Y variation")?,
                    ..HumanizeCommandOptions::new(
                        parse_u64(seed, "humanize seed")?,
                        timing_range,
                        parse_u8(velocity_variation, "velocity variation")?,
                    )
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, seed, timing_range,
            velocity_variation]
            if command == "humanize-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            humanize_notes_with_options(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                HumanizeCommandOptions::new(
                    parse_u64(seed, "humanize seed")?,
                    timing_range,
                    parse_u8(velocity_variation, "velocity variation")?,
                ),
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, seed, timing_range,
            velocity_variation, pan_variation, release_variation, mod_x_variation,
            mod_y_variation]
            if command == "humanize-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            humanize_notes_with_options(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                HumanizeCommandOptions {
                    pan_variation_percent: parse_u8(pan_variation, "pan variation")?,
                    release_variation_percent: parse_u8(release_variation, "release variation")?,
                    mod_x_variation_percent: parse_u8(mod_x_variation, "Mod X variation")?,
                    mod_y_variation_percent: parse_u8(mod_y_variation, "Mod Y variation")?,
                    ..HumanizeCommandOptions::new(
                        parse_u64(seed, "humanize seed")?,
                        timing_range,
                        parse_u8(velocity_variation, "velocity variation")?,
                    )
                },
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
        [command, input, output, pattern_id, channel_id, minimum_key, maximum_key, wrap_to_bottom,
            scale_name, scale_root, snap_direction]
            if command == "limit-notes" =>
        {
            limit_notes_with_options(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                None,
                parse_limit_note_options(
                    minimum_key,
                    maximum_key,
                    wrap_to_bottom,
                    scale_name,
                    scale_root,
                    snap_direction,
                )?,
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, minimum_key, maximum_key]
            if command == "limit-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            limit_notes_with_options(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                LimitNoteOptions {
                    minimum_key: parse_u16(minimum_key, "minimum key")?,
                    maximum_key: parse_u16(maximum_key, "maximum key")?,
                    wrap_to_bottom: false,
                    scale_root: None,
                    scale_intervals: None,
                    snap_direction: LimitSnapDirection::Up,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, minimum_key, maximum_key,
            wrap_to_bottom, scale_name, scale_root, snap_direction]
            if command == "limit-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            limit_notes_with_options(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                parse_limit_note_options(
                    minimum_key,
                    maximum_key,
                    wrap_to_bottom,
                    scale_name,
                    scale_root,
                    snap_direction,
                )?,
            )
        }
        [command, input, score_path, output, pattern_id, channel_id, range_octaves, gate, range_pattern, sync, group_notes, mode, multiplier, levels]
            if command == "arp-score" =>
        {
            arpeggiate_score_notes(
                Path::new(input),
                Path::new(score_path),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                None,
                parse_arpeggio_score_options(
                    [
                        range_octaves,
                        gate,
                        range_pattern,
                        sync,
                        group_notes,
                        mode,
                        multiplier,
                        levels,
                    ],
                )?,
            )
        }
        [command, input, score_path, output, pattern_id, channel_id, note_indices, range_octaves, gate, range_pattern, sync, group_notes, mode, multiplier, levels]
            if command == "arp-score-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            arpeggiate_score_notes(
                Path::new(input),
                Path::new(score_path),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                parse_arpeggio_score_options(
                    [
                        range_octaves,
                        gate,
                        range_pattern,
                        sync,
                        group_notes,
                        mode,
                        multiplier,
                        levels,
                    ],
                )?,
            )
        }
        [command, input, output, pattern_id, channel_id, step_ticks, range_octaves, gate, pattern, sync, group_notes]
            if command == "arp-notes" =>
        {
            arpeggiate_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                ArpeggiatorCliOptions {
                    step: step_ticks,
                    range_octaves: parse_u8(range_octaves, "arpeggiator range")?,
                    gate_percent: parse_u8(gate, "arpeggiator gate")?,
                    range_pattern: parse_arpeggio_range_pattern(pattern)?,
                    sync: parse_arpeggio_sync_mode(sync)?,
                    group_notes: parse_bool(group_notes, "group notes")?,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, step_ticks, range_octaves, gate, pattern, sync]
            if command == "arp-notes" =>
        {
            arpeggiate_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                ArpeggiatorCliOptions {
                    step: step_ticks,
                    range_octaves: parse_u8(range_octaves, "arpeggiator range")?,
                    gate_percent: parse_u8(gate, "arpeggiator gate")?,
                    range_pattern: parse_arpeggio_range_pattern(pattern)?,
                    sync: parse_arpeggio_sync_mode(sync)?,
                    group_notes: false,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, step_ticks, range_octaves, gate, pattern]
            if command == "arp-notes" =>
        {
            arpeggiate_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                ArpeggiatorCliOptions {
                    step: step_ticks,
                    range_octaves: parse_u8(range_octaves, "arpeggiator range")?,
                    gate_percent: parse_u8(gate, "arpeggiator gate")?,
                    range_pattern: parse_arpeggio_range_pattern(pattern)?,
                    sync: ArpeggioSyncMode::Block,
                    group_notes: false,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, step_ticks, range_octaves,
            gate, pattern, sync, group_notes]
            if command == "arp-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            arpeggiate_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                ArpeggiatorCliOptions {
                    step: step_ticks,
                    range_octaves: parse_u8(range_octaves, "arpeggiator range")?,
                    gate_percent: parse_u8(gate, "arpeggiator gate")?,
                    range_pattern: parse_arpeggio_range_pattern(pattern)?,
                    sync: parse_arpeggio_sync_mode(sync)?,
                    group_notes: parse_bool(group_notes, "group notes")?,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, step_ticks, range_octaves,
            gate, pattern, sync]
            if command == "arp-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            arpeggiate_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                ArpeggiatorCliOptions {
                    step: step_ticks,
                    range_octaves: parse_u8(range_octaves, "arpeggiator range")?,
                    gate_percent: parse_u8(gate, "arpeggiator gate")?,
                    range_pattern: parse_arpeggio_range_pattern(pattern)?,
                    sync: parse_arpeggio_sync_mode(sync)?,
                    group_notes: false,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, step_ticks, range_octaves,
            gate, pattern]
            if command == "arp-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            arpeggiate_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                ArpeggiatorCliOptions {
                    step: step_ticks,
                    range_octaves: parse_u8(range_octaves, "arpeggiator range")?,
                    gate_percent: parse_u8(gate, "arpeggiator gate")?,
                    range_pattern: parse_arpeggio_range_pattern(pattern)?,
                    sync: ArpeggioSyncMode::Block,
                    group_notes: false,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, options @ ..]
            if command == "riff-machine-notes" =>
        {
            let ParsedRiffMachineOptions {
                options,
                groove_score_path,
                arpeggio_score_path,
                progression_score_path,
                chord_score_path,
            } = parse_riff_machine_options(options)?;
            let groove_score = groove_score_path
                .as_deref()
                .map(load_riff_machine_score)
                .transpose()?;
            let arpeggio_score = arpeggio_score_path
                .as_deref()
                .map(load_riff_machine_score)
                .transpose()?;
            let progression_score = progression_score_path
                .as_deref()
                .map(load_riff_machine_score)
                .transpose()?;
            let chord_score = chord_score_path
                .as_deref()
                .map(load_riff_machine_score)
                .transpose()?;
            let options = RiffMachineOptions {
                groove_score: groove_score.as_ref(),
                arpeggio_score: arpeggio_score.as_ref(),
                progression_score: progression_score.as_ref(),
                chord_score: chord_score.as_ref(),
                ..options
            };
            riff_machine_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                None,
                options,
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, options @ ..]
            if command == "riff-machine-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            let ParsedRiffMachineOptions {
                options,
                groove_score_path,
                arpeggio_score_path,
                progression_score_path,
                chord_score_path,
            } = parse_riff_machine_options(options)?;
            let groove_score = groove_score_path
                .as_deref()
                .map(load_riff_machine_score)
                .transpose()?;
            let arpeggio_score = arpeggio_score_path
                .as_deref()
                .map(load_riff_machine_score)
                .transpose()?;
            let progression_score = progression_score_path
                .as_deref()
                .map(load_riff_machine_score)
                .transpose()?;
            let chord_score = chord_score_path
                .as_deref()
                .map(load_riff_machine_score)
                .transpose()?;
            let options = RiffMachineOptions {
                groove_score: groove_score.as_ref(),
                arpeggio_score: arpeggio_score.as_ref(),
                progression_score: progression_score.as_ref(),
                chord_score: chord_score.as_ref(),
                ..options
            };
            riff_machine_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                options,
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
        [command, input, output, pattern_id, channel_id, note_indices, position_ticks]
            if command == "slice-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            slice_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                parse_u32(position_ticks, "slice position")?,
            )
        }
        [command, input, output, pattern_id, note_cuts]
            if command == "slice-notes-at-positions" =>
        {
            let note_cuts = parse_note_cuts(note_cuts)?;
            slice_notes_at_positions(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                &note_cuts,
            )
        }
        [command, input, output, pattern_id, channel_id, center, tension, multiplier, offset]
            if command == "scale-levels-notes" =>
        {
            scale_levels_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                None,
                ScaleLevelsOptions {
                    center_percent: parse_i16(center, "Scale Levels center")?,
                    tension_percent: parse_i16(tension, "Scale Levels tension")?,
                    multiplier_percent: parse_u16(multiplier, "Scale Levels multiplier")?,
                    offset_percent: parse_i16(offset, "Scale Levels offset")?,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, center, tension,
            multiplier, offset]
            if command == "scale-levels-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            scale_levels_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                ScaleLevelsOptions {
                    center_percent: parse_i16(center, "Scale Levels center")?,
                    tension_percent: parse_i16(tension, "Scale Levels tension")?,
                    multiplier_percent: parse_u16(multiplier, "Scale Levels multiplier")?,
                    offset_percent: parse_i16(offset, "Scale Levels offset")?,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, multiplier, variation, seed,
            use_original_lengths, chop_chords]
            if command == "articulate-notes" =>
        {
            articulate_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                None,
                ArticulateOptions {
                    multiplier_percent: parse_u8(multiplier, "Articulate multiplier")?,
                    variation_percent: parse_u8(variation, "Articulate variation")?,
                    seed: parse_u64(seed, "Articulate seed")?,
                    use_original_lengths: parse_bool(use_original_lengths, "use original lengths")?,
                    chop_chords: parse_bool(chop_chords, "chop chords")?,
                },
                false,
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, multiplier, variation,
            seed, use_original_lengths, chop_chords, only_with_selection]
            if command == "articulate-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            articulate_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                ArticulateOptions {
                    multiplier_percent: parse_u8(multiplier, "Articulate multiplier")?,
                    variation_percent: parse_u8(variation, "Articulate variation")?,
                    seed: parse_u64(seed, "Articulate seed")?,
                    use_original_lengths: parse_bool(use_original_lengths, "use original lengths")?,
                    chop_chords: parse_bool(chop_chords, "chop chords")?,
                },
                parse_bool(only_with_selection, "only with selection")?,
            )
        }
        [command, input, output, pattern_id, channel_id, preset]
            if command == "articulate-preset" =>
        {
            articulate_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                None,
                parse_articulate_preset(preset)?,
                false,
            )
        }
        [command, input, output, pattern_id, channel_id, preset, note_indices]
            if command == "articulate-selected-preset" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            articulate_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                parse_articulate_preset(preset)?,
                false,
            )
        }
        [command, input, output, pattern_id, channel_id, period_ticks, trash_every,
            time_distortion, remove_short_notes, stretch_to_compensate]
            if command == "claw-notes" =>
        {
            claw_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                None,
                ClawMachineOptions {
                    period_ticks: parse_u32(period_ticks, "Claw Machine period")?,
                    trash_every: parse_u8(trash_every, "Claw Machine Trash every")?,
                    time_distortion_percent: parse_i16(
                        time_distortion,
                        "Claw Machine time distortion",
                    )?,
                    remove_short_notes: parse_bool(remove_short_notes, "remove short notes")?,
                    stretch_to_compensate: parse_bool(
                        stretch_to_compensate,
                        "stretch to compensate",
                    )?,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices, period_ticks,
            trash_every, time_distortion, remove_short_notes, stretch_to_compensate]
            if command == "claw-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            claw_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
                ClawMachineOptions {
                    period_ticks: parse_u32(period_ticks, "Claw Machine period")?,
                    trash_every: parse_u8(trash_every, "Claw Machine Trash every")?,
                    time_distortion_percent: parse_i16(
                        time_distortion,
                        "Claw Machine time distortion",
                    )?,
                    remove_short_notes: parse_bool(remove_short_notes, "remove short notes")?,
                    stretch_to_compensate: parse_bool(
                        stretch_to_compensate,
                        "stretch to compensate",
                    )?,
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
        [command, input, output, pattern_id, channel_id, position, length, key, velocity, quality, inversion]
            if command == "add-chord" =>
        {
            add_chord(
                Path::new(input),
                Path::new(output),
                quality,
                ChordStampCliOptions {
                    pattern_id: parse_u16(pattern_id, "pattern id")?,
                    channel_id: parse_u16(channel_id, "channel id")?,
                    position: parse_u32(position, "chord position")?,
                    length: parse_u32(length, "chord note length")?,
                    root_key: parse_u16(key, "root note key")?,
                    velocity: parse_u8(velocity, "note velocity")?,
                    inversion: parse_usize(inversion, "chord inversion")?,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, position, length, key, velocity, quality, scale, scale_root, inversion]
            if command == "add-scale-chord" =>
        {
            add_scale_chord(
                Path::new(input),
                Path::new(output),
                quality,
                scale,
                parse_u8(scale_root, "scale root")?,
                ChordStampCliOptions {
                    pattern_id: parse_u16(pattern_id, "pattern id")?,
                    channel_id: parse_u16(channel_id, "channel id")?,
                    position: parse_u32(position, "chord position")?,
                    length: parse_u32(length, "chord note length")?,
                    root_key: parse_u16(key, "chord root or top key")?,
                    velocity: parse_u8(velocity, "note velocity")?,
                    inversion: parse_usize(inversion, "chord inversion")?,
                },
            )
        }
        [command, input, output, pattern_id, channel_id, note_specs]
            if command == "add-notes" =>
        {
            let channel_id = parse_u16(channel_id, "channel id")?;
            let notes = parse_note_specs(channel_id, note_specs)?;
            add_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                &notes,
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
        [command, input, output, pattern_id, channel_id] if command == "delete-notes" => {
            delete_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                None,
            )
        }
        [command, input, output, pattern_id, channel_id, note_indices]
            if command == "delete-notes-selected" =>
        {
            let note_indices = parse_note_indices(note_indices)?;
            delete_notes_with_scope(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                Some(&note_indices),
            )
        }
        [command, input, output, arrangement_id, pattern_id, position, length, track_index]
            if command == "create-playlist-pattern-clip" =>
        {
            create_playlist_pattern_clip(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_u16(pattern_id, "pattern id")?,
                parse_u32(position, "clip position")?,
                parse_u32(length, "clip length")?,
                parse_u16(track_index, "zero-based Playlist track index")?,
            )
        }
        [command, input, output, arrangement_id, channel_id, position, length, track_index]
            if command == "create-playlist-audio-clip" =>
        {
            create_playlist_audio_clip(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_u16(channel_id, "Audio Clip channel id")?,
                parse_u32(position, "clip position")?,
                parse_u32(length, "clip length")?,
                parse_u16(track_index, "zero-based Playlist track index")?,
            )
        }
        [command, input, output, arrangement_id, channel_id, position, length, track_index]
            if command == "create-playlist-automation-clip" =>
        {
            create_playlist_automation_clip(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_u16(channel_id, "Automation channel id")?,
                parse_u32(position, "clip position")?,
                parse_u32(length, "clip length")?,
                parse_u16(track_index, "zero-based Playlist track index")?,
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
        [command, input, output, arrangement_id, clip_index]
            if command == "delete-playlist-clip" =>
        {
            delete_playlist_clip(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
            )
        }
        [command, input, output, arrangement_id, start_tick, end_tick, full_source_lengths @ ..]
            if command == "ripple-delete-playlist-time-range" =>
        {
            let mut full_source_lengths_ms = BTreeMap::new();
            for value in full_source_lengths {
                let (clip_index, length_ms) = value.split_once('=').ok_or_else(|| {
                    format!(
                        "invalid full-source length `{value}`; expected clip-index=milliseconds"
                    )
                })?;
                let clip_index = parse_usize(clip_index, "clip index")?;
                let length_ms = parse_f32(length_ms, "full-source sample length in milliseconds")?;
                if full_source_lengths_ms
                    .insert(clip_index, length_ms)
                    .is_some()
                {
                    return Err(format!(
                        "full-source sample length was provided more than once for clip {}",
                        clip_index.saturating_add(1)
                    ));
                }
            }
            ripple_delete_playlist_time_range(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_u32(start_tick, "selection start tick")?,
                parse_u32(end_tick, "selection end tick")?,
                full_source_lengths_ms,
            )
        }
        [command, input, output, arrangement_id, start_tick, end_tick, full_source_lengths @ ..]
            if command == "insert-playlist-time-range" =>
        {
            let mut full_source_lengths_ms = BTreeMap::new();
            for value in full_source_lengths {
                let (clip_index, length_ms) = value.split_once('=').ok_or_else(|| {
                    format!(
                        "invalid full-source length `{value}`; expected clip-index=milliseconds"
                    )
                })?;
                let clip_index = parse_usize(clip_index, "clip index")?;
                let length_ms = parse_f32(length_ms, "full-source sample length in milliseconds")?;
                if full_source_lengths_ms
                    .insert(clip_index, length_ms)
                    .is_some()
                {
                    return Err(format!(
                        "full-source sample length was provided more than once for clip {}",
                        clip_index.saturating_add(1)
                    ));
                }
            }
            insert_playlist_time_range(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_u32(start_tick, "selection start tick")?,
                parse_u32(end_tick, "selection end tick")?,
                full_source_lengths_ms,
            )
        }
        [command, input, output, arrangement_id, clip_index, muted]
            if command == "set-clip-muted" =>
        {
            set_clip_muted(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
                parse_bool(muted, "clip mute state")?,
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
                    press: None,
                    motion: None,
                    trigger_sync: None,
                    queued: None,
                    position_sync: None,
                    tolerant: None,
                },
            )
        }
        [command, input, output, arrangement_id, track_id, trigger_sync]
            if command == "set-playlist-track-trigger-sync" =>
        {
            set_playlist_track_trigger_sync(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_u32(track_id, "Playlist track id")?,
                parse_playlist_track_sync(trigger_sync)?,
            )
        }
        [command, input, output, arrangement_id, track_id, press]
            if command == "set-playlist-track-press" =>
        {
            set_playlist_track_press(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_u32(track_id, "Playlist track id")?,
                parse_playlist_track_press(press)?,
            )
        }
        [command, input, output, arrangement_id, track_id, motion]
            if command == "set-playlist-track-motion" =>
        {
            set_playlist_track_motion(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_u32(track_id, "Playlist track id")?,
                parse_playlist_track_motion(motion)?,
            )
        }
        [command, input, output, arrangement_id, track_id, position_sync]
            if command == "set-playlist-track-position-sync" =>
        {
            set_playlist_track_position_sync(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_u32(track_id, "Playlist track id")?,
                parse_playlist_track_sync(position_sync)?,
            )
        }
        [command, input, output, arrangement_id, track_id, tolerant]
            if command == "set-playlist-track-tolerant" =>
        {
            set_playlist_track_tolerant(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_u32(track_id, "Playlist track id")?,
                parse_bool(tolerant, "Playlist track Tolerant state")?,
            )
        }
        [command, input, output, arrangement_id, track_id, queued]
            if command == "set-playlist-track-queued" =>
        {
            set_playlist_track_queued(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_u32(track_id, "Playlist track id")?,
                parse_bool(queued, "Playlist track Queue state")?,
            )
        }
        [command, input, output, track_id, name] if command == "rename-playlist-track" => {
            rename_playlist_track(
                Path::new(input),
                Path::new(output),
                parse_u32(track_id, "Playlist track id")?,
                name,
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
        [command, input, output, arrangement_id, clip_index, source_start_tick, source_end_tick, length]
            if command == "edit-audio-clip-window-ticks" =>
        {
            edit_audio_clip_window_ticks(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
                parse_u32(source_start_tick, "source start tick")?,
                parse_u32(source_end_tick, "source end tick")?,
                parse_optional_u32(length, "timeline length ticks")?,
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
        [command, input, output, arrangement_id, clip_index, split_position]
            if command == "split-pattern-clip" =>
        {
            split_pattern_clip(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
                parse_u32(split_position, "split position ticks")?,
            )
        }
        [command, input, output, arrangement_id, clip_index, split_position]
            if command == "split-automation-clip" =>
        {
            split_automation_clip(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
                parse_u32(split_position, "split position ticks")?,
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
        [command, input, output, arrangement_id, clip_index]
            if command == "make-audio-clip-unique" =>
        {
            make_audio_clip_unique(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
            )
        }
        [command, input, output, arrangement_id, clip_index]
            if command == "make-automation-clip-unique" =>
        {
            make_automation_clip_unique(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
            )
        }
        [command, input, output, arrangement_id, clip_index]
            if command == "make-audio-clip-unique-as-sample" =>
        {
            make_audio_clip_unique_as_sample(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
            )
        }
        [command, input, output, arrangement_id, clip_index, reversed]
            if command == "set-playlist-audio-clip-reversed" =>
        {
            set_playlist_audio_clip_reversed(
                Path::new(input),
                Path::new(output),
                parse_u16(arrangement_id, "arrangement id")?,
                parse_usize(clip_index, "clip index")?,
                parse_bool(reversed, "Audio Clip Reverse setting")?,
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
        [command, input, midi_path, output, track, pattern_id, stream_channel, stream_flags, controller_number, midi_channel]
            if command == "import-midi-cc" || command == "import-midi-cc-new-stream" =>
        {
            import_midi_control_changes(
                MidiControlChangeImport {
                    input: Path::new(input),
                    midi_path: Path::new(midi_path),
                    output: Path::new(output),
                    track_index: parse_usize(track, "MIDI track index")?,
                    pattern_id: parse_u16(pattern_id, "pattern id")?,
                    target_stream: (
                    parse_raw_byte(stream_channel, "raw controller channel")?,
                    parse_raw_byte(stream_flags, "raw controller flags")?,
                    ),
                    controller_number: parse_u8(controller_number, "MIDI Control Change number")?,
                    midi_channel: parse_midi_channel_filter(midi_channel)?,
                    new_stream: command == "import-midi-cc-new-stream",
                },
            )
        }
        [command, input, score_path, output, pattern_id, channel_id]
            if command == "import-score" =>
        {
            import_score_notes(
                Path::new(input),
                Path::new(score_path),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                false,
            )
        }
        [command, input, score_path, output, pattern_id, channel_id, replace]
            if command == "import-score" =>
        {
            import_score_notes(
                Path::new(input),
                Path::new(score_path),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
                parse_bool(replace, "replace channel notes")?,
            )
        }
        [command, input, score_path, output, pattern_id, source_channel, source_flags, target_channel, target_flags, replace]
            if command == "import-score-automation" =>
        {
            import_score_automation(
                Path::new(input),
                Path::new(score_path),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                FscControllerStreamImportOptions {
                    source_stream: (
                        parse_raw_byte(source_channel, "source stream channel")?,
                        parse_raw_byte(source_flags, "source stream flags")?,
                    ),
                    target_stream: (
                        parse_raw_byte(target_channel, "target stream channel")?,
                        parse_raw_byte(target_flags, "target stream flags")?,
                    ),
                    replace_target_stream: parse_bool(replace, "replace target stream")?,
                },
            )
        }
        [command, input, output, pattern_id, channel_id] if command == "export-score" => {
            export_score_notes(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                parse_u16(channel_id, "channel id")?,
            )
        }
        [command, input, output, pattern_id, stream_channel, stream_flags]
            if command == "export-score-automation" =>
        {
            export_score_automation(
                Path::new(input),
                Path::new(output),
                parse_u16(pattern_id, "pattern id")?,
                (
                    parse_raw_byte(stream_channel, "controller stream channel")?,
                    parse_raw_byte(stream_flags, "controller stream flags")?,
                ),
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
                parse_midi_pattern_target(pattern_id)?,
                parse_u16(channel_id, "channel id")?,
                MidiImportOptions::default(),
            )
        }
        [command, input, midi_path, output, track, pattern_id, channel_id, zero_velocity_note_ons]
            if command == "import-midi" =>
        {
            import_midi_track(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_usize(track, "MIDI track number")?,
                parse_midi_pattern_target(pattern_id)?,
                parse_u16(channel_id, "channel id")?,
                MidiImportOptions {
                    zero_velocity_note_ons_as_notes: parse_bool(
                        zero_velocity_note_ons,
                        "zero-velocity Note On option",
                    )?,
                    realign_events: false,
                    midi_channel_mask: u16::MAX,
                    set_mixer_tracks_for_new_channels: false,
                },
            )
        }
        [command, input, midi_path, output, track, pattern_id, channel_id, zero_velocity_note_ons, realign_events]
            if command == "import-midi" =>
        {
            import_midi_track(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_usize(track, "MIDI track number")?,
                parse_midi_pattern_target(pattern_id)?,
                parse_u16(channel_id, "channel id")?,
                parse_midi_import_options(zero_velocity_note_ons, realign_events)?,
            )
        }
        [command, input, midi_path, output, track, pattern_id, channel_id, zero_velocity_note_ons, realign_events, midi_channels]
            if command == "import-midi" =>
        {
            import_midi_track(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_usize(track, "MIDI track number")?,
                parse_midi_pattern_target(pattern_id)?,
                parse_u16(channel_id, "channel id")?,
                parse_midi_import_options_with_channels(
                    zero_velocity_note_ons,
                    realign_events,
                    midi_channels,
                )?,
            )
        }
        [command, input, midi_path, output, pattern_id, channel_ids]
            if command == "import-midi-all" =>
        {
            let channel_ids = channel_ids
                .split(',')
                .map(|channel_id| parse_u16(channel_id, "channel id"))
                .collect::<Result<Vec<_>, _>>()?;
            import_midi_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                &channel_ids,
                MidiImportOptions::default(),
            )
        }
        [command, input, midi_path, output, pattern_id, channel_ids, zero_velocity_note_ons]
            if command == "import-midi-all" =>
        {
            let channel_ids = channel_ids
                .split(',')
                .map(|channel_id| parse_u16(channel_id, "channel id"))
                .collect::<Result<Vec<_>, _>>()?;
            import_midi_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                &channel_ids,
                MidiImportOptions {
                    zero_velocity_note_ons_as_notes: parse_bool(
                        zero_velocity_note_ons,
                        "zero-velocity Note On option",
                    )?,
                    realign_events: false,
                    midi_channel_mask: u16::MAX,
                    set_mixer_tracks_for_new_channels: false,
                },
            )
        }
        [command, input, midi_path, output, pattern_id, channel_ids, zero_velocity_note_ons, realign_events]
            if command == "import-midi-all" =>
        {
            let channel_ids = channel_ids
                .split(',')
                .map(|channel_id| parse_u16(channel_id, "channel id"))
                .collect::<Result<Vec<_>, _>>()?;
            import_midi_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                &channel_ids,
                parse_midi_import_options(zero_velocity_note_ons, realign_events)?,
            )
        }
        [command, input, midi_path, output, pattern_id, channel_ids, zero_velocity_note_ons, realign_events, midi_channels]
            if command == "import-midi-all" =>
        {
            let channel_ids = channel_ids
                .split(',')
                .map(|channel_id| parse_u16(channel_id, "channel id"))
                .collect::<Result<Vec<_>, _>>()?;
            import_midi_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                &channel_ids,
                parse_midi_import_options_with_channels(
                    zero_velocity_note_ons,
                    realign_events,
                    midi_channels,
                )?,
            )
        }
        [command, input, midi_path, output, channel_id, options @ ..]
            if command == "import-midi-named-patterns" =>
        {
            let options = match options {
                [] => MidiImportOptions::default(),
                [zero_velocity_note_ons] => {
                    parse_midi_import_options(zero_velocity_note_ons, "0")?
                }
                [zero_velocity_note_ons, realign_events] => {
                    parse_midi_import_options(zero_velocity_note_ons, realign_events)?
                }
                [zero_velocity_note_ons, realign_events, midi_channels] => {
                    parse_midi_import_options_with_channels(
                        zero_velocity_note_ons,
                        realign_events,
                        midi_channels,
                    )?
                }
                _ => {
                    return Err(
                        "import-midi-named-patterns accepts at most three MIDI import options"
                            .to_owned(),
                    );
                }
            };
            import_midi_tracks_to_named_patterns(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_u16(channel_id, "channel id")?,
                options,
            )
        }
        [command, input, midi_path, output, pattern_id, track_numbers, channel_ids]
            if command == "import-midi-selected" =>
        {
            let channel_ids = channel_ids
                .split(',')
                .map(|channel_id| parse_u16(channel_id, "channel id"))
                .collect::<Result<Vec<_>, _>>()?;
            import_midi_selected_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                track_numbers,
                &channel_ids,
                MidiImportOptions::default(),
            )
        }
        [command, input, midi_path, output, pattern_id, track_numbers, channel_ids, zero_velocity_note_ons, realign_events, midi_channels]
            if command == "import-midi-selected" =>
        {
            let channel_ids = channel_ids
                .split(',')
                .map(|channel_id| parse_u16(channel_id, "channel id"))
                .collect::<Result<Vec<_>, _>>()?;
            import_midi_selected_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                track_numbers,
                &channel_ids,
                parse_midi_import_options_with_channels(
                    zero_velocity_note_ons,
                    realign_events,
                    midi_channels,
                )?,
            )
        }
        [command, input, midi_path, output, pattern_id, template_channel_id, track_numbers]
            if command == "import-midi-sampler-tracks-selected" =>
        {
            import_midi_selected_sampler_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                parse_u16(template_channel_id, "template channel id")?,
                track_numbers,
                MidiImportOptions::default(),
            )
        }
        [command, input, midi_path, output, pattern_id, template_channel_id, track_numbers, zero_velocity_note_ons, realign_events, midi_channels, set_mixer_tracks]
            if command == "import-midi-sampler-tracks-selected" =>
        {
            import_midi_selected_sampler_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                parse_u16(template_channel_id, "template channel id")?,
                track_numbers,
                parse_midi_import_options_with_mixer_tracks(
                    zero_velocity_note_ons,
                    realign_events,
                    midi_channels,
                    set_mixer_tracks,
                )?,
            )
        }
        [command, input, midi_path, output, template_channel_id, track_numbers]
            if command == "import-midi-format2-selected" =>
        {
            import_midi_selected_format2_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_u16(template_channel_id, "template channel id")?,
                track_numbers,
                MidiImportOptions::default(),
            )
        }
        [command, input, midi_path, output, template_channel_id, track_numbers, zero_velocity_note_ons, realign_events, midi_channels, set_mixer_tracks]
            if command == "import-midi-format2-selected" =>
        {
            import_midi_selected_format2_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_u16(template_channel_id, "template channel id")?,
                track_numbers,
                parse_midi_import_options_with_mixer_tracks(
                    zero_velocity_note_ons,
                    realign_events,
                    midi_channels,
                    set_mixer_tracks,
                )?,
            )
        }
        [command, template_path, midi_path, output, template_channel_id, track_numbers]
            if command == "import-midi-new-project" =>
        {
            import_midi_into_new_project(
                Path::new(template_path),
                Path::new(midi_path),
                Path::new(output),
                parse_u16(template_channel_id, "template channel id")?,
                track_numbers,
                MidiImportOptions::default(),
            )
        }
        [command, template_path, midi_path, output, template_channel_id, track_numbers, zero_velocity_note_ons, realign_events, midi_channels, set_mixer_tracks]
            if command == "import-midi-new-project" =>
        {
            import_midi_into_new_project(
                Path::new(template_path),
                Path::new(midi_path),
                Path::new(output),
                parse_u16(template_channel_id, "template channel id")?,
                track_numbers,
                parse_midi_import_options_with_mixer_tracks(
                    zero_velocity_note_ons,
                    realign_events,
                    midi_channels,
                    set_mixer_tracks,
                )?,
            )
        }
        [command, input, midi_path, output, pattern_id, template_channel_id, track_numbers]
            if command == "import-midi-template-tracks" =>
        {
            import_midi_template_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                parse_u16(template_channel_id, "template channel id")?,
                track_numbers,
                MidiImportOptions::default(),
            )
        }
        [command, input, midi_path, output, pattern_id, template_channel_id, track_numbers, zero_velocity_note_ons, realign_events, midi_channels, set_mixer_tracks]
            if command == "import-midi-template-tracks" =>
        {
            import_midi_template_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                parse_u16(template_channel_id, "template channel id")?,
                track_numbers,
                parse_midi_import_options_with_mixer_tracks(
                    zero_velocity_note_ons,
                    realign_events,
                    midi_channels,
                    set_mixer_tracks,
                )?,
            )
        }
        [command, input, midi_path, output, template_channel_id, track_numbers]
            if command == "import-midi-template-format2" =>
        {
            import_midi_template_format2_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_u16(template_channel_id, "template channel id")?,
                track_numbers,
                MidiImportOptions::default(),
            )
        }
        [command, input, midi_path, output, template_channel_id, track_numbers, zero_velocity_note_ons, realign_events, midi_channels, set_mixer_tracks]
            if command == "import-midi-template-format2" =>
        {
            import_midi_template_format2_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_u16(template_channel_id, "template channel id")?,
                track_numbers,
                parse_midi_import_options_with_mixer_tracks(
                    zero_velocity_note_ons,
                    realign_events,
                    midi_channels,
                    set_mixer_tracks,
                )?,
            )
        }
        [command, input, midi_path, output, pattern_id, template_channel_id]
            if command == "import-midi-sampler-tracks" =>
        {
            import_midi_sampler_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                parse_u16(template_channel_id, "template channel id")?,
                MidiImportOptions::default(),
            )
        }
        [command, input, midi_path, output, pattern_id, template_channel_id, zero_velocity_note_ons]
            if command == "import-midi-sampler-tracks" =>
        {
            import_midi_sampler_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                parse_u16(template_channel_id, "template channel id")?,
                MidiImportOptions {
                    zero_velocity_note_ons_as_notes: parse_bool(
                        zero_velocity_note_ons,
                        "zero-velocity Note On option",
                    )?,
                    realign_events: false,
                    midi_channel_mask: u16::MAX,
                    set_mixer_tracks_for_new_channels: false,
                },
            )
        }
        [command, input, midi_path, output, pattern_id, template_channel_id, zero_velocity_note_ons, realign_events]
            if command == "import-midi-sampler-tracks" =>
        {
            import_midi_sampler_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                parse_u16(template_channel_id, "template channel id")?,
                parse_midi_import_options(zero_velocity_note_ons, realign_events)?,
            )
        }
        [command, input, midi_path, output, pattern_id, template_channel_id, zero_velocity_note_ons, realign_events, midi_channels]
            if command == "import-midi-sampler-tracks" =>
        {
            import_midi_sampler_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                parse_u16(template_channel_id, "template channel id")?,
                parse_midi_import_options_with_channels(
                    zero_velocity_note_ons,
                    realign_events,
                    midi_channels,
                )?,
            )
        }
        [command, input, midi_path, output, pattern_id, template_channel_id, zero_velocity_note_ons, realign_events, midi_channels, set_mixer_tracks]
            if command == "import-midi-sampler-tracks" =>
        {
            import_midi_sampler_tracks(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_midi_pattern_target(pattern_id)?,
                parse_u16(template_channel_id, "template channel id")?,
                parse_midi_import_options_with_mixer_tracks(
                    zero_velocity_note_ons,
                    realign_events,
                    midi_channels,
                    set_mixer_tracks,
                )?,
            )
        }
        [command, input, midi_path, output, template_channel_id]
            if command == "import-midi-format2" =>
        {
            import_midi_format2(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_u16(template_channel_id, "template channel id")?,
                MidiImportOptions::default(),
            )
        }
        [command, input, midi_path, output, template_channel_id, zero_velocity_note_ons]
            if command == "import-midi-format2" =>
        {
            import_midi_format2(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_u16(template_channel_id, "template channel id")?,
                MidiImportOptions {
                    zero_velocity_note_ons_as_notes: parse_bool(
                        zero_velocity_note_ons,
                        "zero-velocity Note On option",
                    )?,
                    realign_events: false,
                    midi_channel_mask: u16::MAX,
                    set_mixer_tracks_for_new_channels: false,
                },
            )
        }
        [command, input, midi_path, output, template_channel_id, zero_velocity_note_ons, realign_events]
            if command == "import-midi-format2" =>
        {
            import_midi_format2(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_u16(template_channel_id, "template channel id")?,
                parse_midi_import_options(zero_velocity_note_ons, realign_events)?,
            )
        }
        [command, input, midi_path, output, template_channel_id, zero_velocity_note_ons, realign_events, midi_channels]
            if command == "import-midi-format2" =>
        {
            import_midi_format2(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_u16(template_channel_id, "template channel id")?,
                parse_midi_import_options_with_channels(
                    zero_velocity_note_ons,
                    realign_events,
                    midi_channels,
                )?,
            )
        }
        [command, input, midi_path, output, template_channel_id, zero_velocity_note_ons, realign_events, midi_channels, set_mixer_tracks]
            if command == "import-midi-format2" =>
        {
            import_midi_format2(
                Path::new(input),
                Path::new(midi_path),
                Path::new(output),
                parse_u16(template_channel_id, "template channel id")?,
                parse_midi_import_options_with_mixer_tracks(
                    zero_velocity_note_ons,
                    realign_events,
                    midi_channels,
                    set_mixer_tracks,
                )?,
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
            "  flp-rebuild export-midi-pattern <project.flp> <output.mid> <pattern-id> [stored|channels] [format0|format1]\n",
            "  flp-rebuild export-midi-cc <project.flp> <output.mid> <pattern-id> <raw-channel> <raw-flags> <cc-0..127> <midi-channel-1..16> [format0|format1]\n",
            "  flp-rebuild export-midi-song <project.flp> <output.mid> <arrangement-id> [stored|channels] [format0|format1]\n",
            "  flp-rebuild export-midi-song-cc <project.flp> <output.mid> <arrangement-id> <raw-channel> <raw-flags> <cc-0..127> <midi-channel-1..16> [format0|format1]\n",
            "  flp-rebuild midi-events <file.mid> <track> [start] [count]\n",
            "  flp-rebuild scan <directory>\n",
            "  flp-rebuild plugin-scan\n",
            "  flp-rebuild render-pattern-vst3 <project.flp> <pattern-id> <channel-id> <plugin.vst3> <output.{wav|flac|ogg|mp3}> [tail-seconds]\n",
            "  flp-rebuild render-audio-vst3 <input-audio> <effect.vst3> <output.{wav|flac|ogg|mp3}> [class-uid|-] [tail-seconds]\n",
            "  flp-rebuild channels <file.flp>\n",
            "  flp-rebuild channel-groups <file.flp>\n",
            "  flp-rebuild duplicate-channel <input.flp> <output.flp> <channel-id> <name>\n",
            "  flp-rebuild mixer <file.flp>\n",
            "  flp-rebuild set-mixer-automatic-pdc <input.flp> <output.flp> <0|1>\n",
            "  flp-rebuild set-mixer-parameter-record-value <input.flp> <output.flp> <event-index> <record-index> <signed-value>\n",
            "  flp-rebuild set-mixer-eq-parameter-raw <input.flp> <output.flp> <insert-ordinal> <low-gain|mid-gain|high-gain|low-frequency|mid-frequency|high-frequency|low-q|mid-q|high-q> <raw-value>\n",
            "  flp-rebuild set-mixer-effect-slot-enabled <input.flp> <output.flp> <insert-ordinal> <slot-index> <0|1>\n",
            "  flp-rebuild set-mixer-effect-slot-color <input.flp> <output.flp> <insert-ordinal> <slot-index> <RRGGBB>\n",
            "  flp-rebuild set-mixer-effect-slot-mix-raw <input.flp> <output.flp> <insert-ordinal> <slot-index> <-6400..6400>\n",
            "  flp-rebuild move-mixer-effect-slot <input.flp> <output.flp> <insert-ordinal> <from-slot-0..9> <to-slot-0..9>\n",
            "  flp-rebuild rename-mixer-insert <input.flp> <output.flp> <insert-ordinal> <name>\n",
            "  flp-rebuild set-mixer-insert-flag <input.flp> <output.flp> <insert-ordinal> <enabled|soloed|polarity-reversed|swap-left-right|effects-enabled|mute-locked> <0|1>\n",
            "  flp-rebuild set-mixer-routing-flag <input.flp> <output.flp> <event-index> <flag-byte-index> <0|1>\n",
            "  flp-rebuild automation <file.flp>\n",
            "  flp-rebuild set-e3-record-raw-fields <input.flp> <output.flp> <event-index> <record-index> <source-id-raw> <parameter-raw> <destination-raw-signed>\n",
            "  flp-rebuild time-markers <file.flp>\n",
            "  flp-rebuild sample-paths <file.flp>\n",
            "  flp-rebuild audio-info <audio-file>\n",
            "  flp-rebuild render-audio-clips <project.flp> <output.wav> [arrangement-id]\n",
            "  flp-rebuild render-pattern-samplers <project.flp> <pattern-id> <output.wav> [sample-rate] [voice-limit]\n",
            "  flp-rebuild plugin-states <file.flp>\n",
            "  flp-rebuild channel-events <file.flp> <channel-id>\n",
            "  flp-rebuild patterns <file.flp>\n",
            "  flp-rebuild pattern-controllers <file.flp> <pattern-id>\n",
            "  flp-rebuild create-pattern-controller-stream <input.flp> <output.flp> <pattern-id> <channel-raw> <flags-raw-decimal|0xHEX> <position-ticks> <value>\n",
            "  flp-rebuild edit-pattern-controller <input.flp> <output.flp> <pattern-id> <controller-index> <position-ticks|-> <value|->\n",
            "  flp-rebuild duplicate-pattern-controller <input.flp> <output.flp> <pattern-id> <template-index> <position-ticks> <value>\n",
            "  flp-rebuild delete-pattern-controller <input.flp> <output.flp> <pattern-id> <controller-index>\n",
            "  flp-rebuild draw-pattern-controller-points <input.flp> <output.flp> <pattern-id> <channel:0..255> <flags:0..255> <points:tick:value,...>\n",
            "  flp-rebuild linearize-pattern-controllers <input.flp> <output.flp> <pattern-id> <channel:0..255> <flags:0..255> <start-tick> <end-tick>\n",
            "  flp-rebuild spline-pattern-controllers <input.flp> <output.flp> <pattern-id> <channel:0..255> <flags:0..255> <start-tick> <end-tick>\n",
            "  flp-rebuild delete-pattern-controller-range <input.flp> <output.flp> <pattern-id> <channel:0..255> <flags:0..255> <start-tick> <end-tick>\n",
            "  flp-rebuild playlist <file.flp> [start] [count]\n",
            "  flp-rebuild diff-playlist-clip <before.flp> <after.flp> <arrangement-id> <clip-index>\n",
            "  flp-rebuild notes <file.flp> <pattern-id> [start] [count]\n",
            "  flp-rebuild events <file.flp> [start] [count]\n",
            "  flp-rebuild extract-event <file.flp> <index> <output.bin>\n",
            "  flp-rebuild roundtrip <input.flp> <output.flp>\n",
            "  flp-rebuild set-tempo <input.flp> <output.flp> <bpm>\n",
            "  flp-rebuild create-pattern <input.flp> <output.flp>\n",
            "  flp-rebuild duplicate-pattern <input.flp> <output.flp> <pattern-id>\n",
            "  flp-rebuild rename-pattern <input.flp> <output.flp> <pattern-id> <name>\n",
            "  flp-rebuild set-pattern-color <input.flp> <output.flp> <pattern-id> <RRGGBB>\n",
            "  flp-rebuild delete-pattern <input.flp> <output.flp> <pattern-id>\n",
            "  flp-rebuild set-project-info <input.flp> <output.flp> <title|-> <author|-> <genre|-> <comments|-> <web-link|->\n",
            "  flp-rebuild set-project-settings <input.flp> <output.flp> <play-truncated:0|1|-> <fast-declick:0|1|-> [pan-law-raw|-]\n",
            "  flp-rebuild set-project-master-pitch <input.flp> <output.flp> <cents:-1200..1200>\n",
            "  flp-rebuild set-project-time-signature <input.flp> <output.flp> <numerator> <denominator>\n",
            "  flp-rebuild set-pattern-time-signature <input.flp> <output.flp> <pattern-id> <position-ticks> <numerator> <denominator>\n",
            "  flp-rebuild create-pattern-time-marker <input.flp> <output.flp> <pattern-id> <position-ticks> <name>\n",
            "  flp-rebuild edit-pattern-time-marker <input.flp> <output.flp> <pattern-id> <marker-index> <position-ticks> <name>\n",
            "  flp-rebuild delete-pattern-time-marker <input.flp> <output.flp> <pattern-id> <marker-index>\n",
            "  flp-rebuild delete-pattern-time-signature <input.flp> <output.flp> <pattern-id> <position-ticks>\n",
            "  flp-rebuild set-global-swing <input.flp> <output.flp> <percent-0..100>\n",
            "  flp-rebuild rename-channel <input.flp> <output.flp> <channel-id> <name>\n",
            "  flp-rebuild create-sampler-channel <input.flp> <output.flp> <sample-path> <name>\n",
            "  flp-rebuild create-audio-channel <input.flp> <output.flp> <sample-path> <name>\n",
            "  flp-rebuild create-channel-group <input.flp> <output.flp> <name>\n",
            "  flp-rebuild group-channels <input.flp> <output.flp> <channel-ids:comma-separated> <name>\n",
            "  flp-rebuild rename-channel-group <input.flp> <output.flp> <group-index> <name>\n",
            "  flp-rebuild delete-channel-group <input.flp> <output.flp> <group-index>\n",
            "  flp-rebuild set-channel-color <input.flp> <output.flp> <channel-id> <RRGGBB>\n",
            "  flp-rebuild move-channel <input.flp> <output.flp> <channel-id> <target-index:zero-based>\n",
            "  flp-rebuild reorder-channels <input.flp> <output.flp> <complete-channel-id-order:comma-separated>\n",
            "  flp-rebuild sort-channels <input.flp> <output.flp> <color|mixer-track|name|type>\n",
            "  flp-rebuild set-channel-colors <input.flp> <output.flp> <channel-ids:comma-separated> <RRGGBB>\n",
            "  flp-rebuild set-channel-color-gradient <input.flp> <output.flp> <channel-ids:comma-separated-in-order> <start-RRGGBB> <end-RRGGBB>\n",
            "  flp-rebuild set-channel-levels <input.flp> <output.flp> <channel-id> <volume-0..12800> <pan-0..12800>\n",
            "  flp-rebuild set-sampler-quick-fades <input.flp> <output.flp> <sampler-channel-id> <fade-out-0..1024> <fade-in-0..1024>\n",
            "  flp-rebuild set-sampler-portamento <input.flp> <output.flp> <sampler-channel-id> <porta:0|1|-> <slide-raw-0..1660|->\n",
            "  flp-rebuild edit-sampler-volume-envelope <input.flp> <output.flp> <sampler-channel-id> <enabled:0|1|-> <tempo-sync:0|1|-> <predelay-raw|-> <attack-raw|-> <hold-raw|-> <decay-raw|-> <sustain-raw|-> <release-raw|-> <amount-raw|-> <attack-tension-raw|-> <decay-tension-raw|-> <release-tension-raw|->\n",
            "  flp-rebuild edit-sampler-envelope-lfo <input.flp> <output.flp> <sampler-channel-id> <target:panning|volume|mod-x|mod-y|pitch> <enabled:0|1|-> <envelope-tempo-sync:0|1|-> <lfo-tempo-sync:0|1|-> <lfo-phase-retrigger:0|1|-> <predelay-raw|-> <attack-raw|-> <hold-raw|-> <decay-raw|-> <sustain-raw|-> <release-raw|-> <amount-raw|-> <attack-tension-raw|-> <decay-tension-raw|-> <release-tension-raw|-> <lfo-predelay-raw|-> <lfo-attack-raw|-> <lfo-amount-raw|-> <lfo-speed-raw|-> <lfo-shape:0|1|2|->\n",
            "  flp-rebuild set-channel-fine-tune <input.flp> <output.flp> <channel-id> <-100..100-cents>\n",
            "  flp-rebuild set-channel-enabled <input.flp> <output.flp> <channel-id> <0|1>\n",
            "  flp-rebuild set-channel-sample-path <input.flp> <output.flp> <channel-id> <sample-path>\n",
            "  flp-rebuild set-channel-sample-reversed <input.flp> <output.flp> <channel-id> <0|1>\n",
            "  flp-rebuild edit-channel-sample-effects <input.flp> <output.flp> <channel-id> <normalize:0|1|-> <reverse-polarity:0|1|-> [remove-dc:0|1|->]\n",
            "  flp-rebuild edit-channel-sample-fx-flags <input.flp> <output.flp> <channel-id> <fade-stereo:0|1|-> <swap-stereo:0|1|->\n",
            "  flp-rebuild edit-channel-sample-window <input.flp> <output.flp> <channel-id> <start:0..1|-> <length:0..1|->\n",
            "  flp-rebuild edit-channel-sample-trim <input.flp> <output.flp> <channel-id> <threshold-0..256>\n",
            "  flp-rebuild edit-channel-sample-crossfade <input.flp> <output.flp> <channel-id> <amount-0..256>\n",
            "  flp-rebuild edit-channel-sampler-filter <input.flp> <output.flp> <channel-id> <cutoff-0..256|-> <resonance-0..256|-> <type-0..7|->\n",
            "  flp-rebuild set-channel-zipped <input.flp> <output.flp> <channel-id> <0|1>\n",
            "  flp-rebuild set-channel-main-pitch <input.flp> <output.flp> <channel-id> <0|1>\n",
            "  flp-rebuild set-channel-key-region <input.flp> <output.flp> <channel-id> <low-key:0..127> <high-key:0..127>\n",
            "  flp-rebuild set-channel-add-to-key <input.flp> <output.flp> <channel-id> <0|1>\n",
            "  flp-rebuild set-channel-mixer-track <input.flp> <output.flp> <channel-id> <mixer-track:-1=Current|0=Master|1..127=insert>\n",
            "  flp-rebuild set-channel-midi-out-settings <input.flp> <output.flp> <channel-id> <midi-channel:1..16|-> <use-note-color-channel:0|1|->\n",
            "  flp-rebuild set-vst3-output-route <input.flp> <output.flp> <channel-id> <output-bus-index:0-based> <mixer-track:-1=Current|0=Master|1..127=insert>\n",
            "  flp-rebuild edit-channel-stretch-settings <input.flp> <output.flp> <channel-id> <time-raw|-> <pitch-raw|-> <multiplier-raw|-> <mode:-1..9|->\n",
            "  flp-rebuild edit-channel-level-adjustments <input.flp> <output.flp> <channel-id> <event-index> <pan-raw|-> <volume-raw|-> <mod-x-raw|-> <mod-y-raw|->\n",
            "  flp-rebuild set-channel-swing <input.flp> <output.flp> <channel-id> <percent-0..100>\n",
            "  flp-rebuild set-layer-children <input.flp> <output.flp> <layer-channel-id> <child-ids-comma-separated|->\n",
            "  flp-rebuild set-layer-flags <input.flp> <output.flp> <layer-channel-id> <random:0|1|-> <crossfade:0|1|->\n",
            "  flp-rebuild set-layer-fade <input.flp> <output.flp> <layer-channel-id> <fade:0..256>\n",
            "  flp-rebuild edit-automation-point <input.flp> <output.flp> <channel-id> <point-index> <position-beats> <value> <tension>\n",
            "  flp-rebuild set-automation-point-tail <input.flp> <output.flp> <channel-id> <point-index> <8-hex-digits>\n",
            "  flp-rebuild set-automation-range-raw <input.flp> <output.flp> <channel-id> <minimum-i32> <maximum-i32>\n",
            "  flp-rebuild flip-automation-points <input.flp> <output.flp> <channel-id>\n",
            "  flp-rebuild normalize-automation-points <input.flp> <output.flp> <channel-id>\n",
            "  flp-rebuild scale-automation-levels <input.flp> <output.flp> <channel-id> <center:-100..100> <tension:-100..100> <multiply:-200..200> <offset:-100..100>\n",
            "  flp-rebuild copy-automation-state <input.flp> <output.flp> <source-channel-id> <target-channel-id>\n",
            "  flp-rebuild set-automation-lfo-amount <input.flp> <output.flp> <channel-id> <-128..128>\n",
            "  flp-rebuild init-automation-points <input.flp> <output.flp> <channel-id> <template-channel-id>\n",
            "  flp-rebuild insert-automation-point <input.flp> <output.flp> <channel-id> <insertion-slot> <position-beats> <value> <tension>\n",
            "  flp-rebuild delete-automation-point <input.flp> <output.flp> <channel-id> <point-index>\n",
            "  flp-rebuild interpolate-automation-points <input.flp> <output.flp> <channel-id> <first-point-index> <last-point-index> [linear|smooth]\n",
            "  flp-rebuild edit-note <input.flp> <output.flp> <pattern-id> <channel-id> <note-index> <position> <length> <key> <velocity>\n",
            "  flp-rebuild edit-note-properties <input.flp> <output.flp> <pattern-id> <channel-id> <note-index> <flags-raw|-> <group-raw|-> <fine-pitch|-> <release|-> <midi-channel-raw|-> <pan|-> <mod-x|-> <mod-y|->\n",
            "  flp-rebuild set-note-porta <input.flp> <output.flp> <pattern-id> <channel-id> <note-index> <enabled:0|1>\n",
            "  flp-rebuild quantize-notes <input.flp> <output.flp> <pattern-id> <channel-id> <grid-ticks> <start-strength-0..1> <swing-0..1> [sensitivity-0..1 duration-strength-0..1 leave-duration|leave-end|quantize-duration|quantize-end]\n",
            "  flp-rebuild quantize-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <grid-ticks> <start-strength-0..1> <swing-0..1> [sensitivity-0..1 duration-strength-0..1 leave-duration|leave-end|quantize-duration|quantize-end]\n",
            "  flp-rebuild quantize-notes-groove <input.flp> <output.flp> <pattern-id> <channel-id> <groove.fsc> <start-strength-0..1> <sensitivity-0..1> <duration-strength-0..1> <leave-duration|leave-end|quantize-duration|quantize-end> [velocity-level-0..1 pan-level-0..1 release-level-0..1 modx-level-0..1 mody-level-0..1]\n",
            "  flp-rebuild quantize-notes-groove-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <groove.fsc> <start-strength-0..1> <sensitivity-0..1> <duration-strength-0..1> <leave-duration|leave-end|quantize-duration|quantize-end> [velocity-level-0..1 pan-level-0..1 release-level-0..1 modx-level-0..1 mody-level-0..1]\n",
            "  flp-rebuild rotate-notes <input.flp> <output.flp> <pattern-id> <channel-id> <snap-ticks> <left|right>\n",
            "  flp-rebuild rotate-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <snap-ticks> <left|right>\n",
            "  flp-rebuild legato-notes <input.flp> <output.flp> <pattern-id> <channel-id>\n",
            "  flp-rebuild legato-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based>\n",
            "  flp-rebuild chop-notes <input.flp> <output.flp> <pattern-id> <channel-id> <divisions-2..64>\n",
            "  flp-rebuild chop-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <divisions-2..64>\n",
            "  flp-rebuild quick-chop-notes <input.flp> <output.flp> <pattern-id> <channel-id> <snap-ticks>\n",
            "  flp-rebuild quick-chop-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <snap-ticks>\n",
            "  flp-rebuild glue-notes <input.flp> <output.flp> <pattern-id> <channel-id>\n",
            "  flp-rebuild glue-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based>\n",
            "  flp-rebuild flip-notes <input.flp> <output.flp> <pattern-id> <channel-id>\n",
            "  flp-rebuild flip-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based>\n",
            "  flp-rebuild strum-notes <input.flp> <output.flp> <pattern-id> <channel-id> <spread-ticks> <up|down> [velocity-amount:-127..127 velocity-tension:-100..100 preserve-end:0|1 alternate-direction:0|1 start-tension:-100..100 end-shift:i32 end-tension:-100..100 trigger-ahead:0|1 chop-chords:0|1]\n",
            "  flp-rebuild strum-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <spread-ticks> <up|down> [velocity-amount:-127..127 velocity-tension:-100..100 preserve-end:0|1 alternate-direction:0|1 start-tension:-100..100 end-shift:i32 end-tension:-100..100 trigger-ahead:0|1 chop-chords:0|1]\n",
            "  flp-rebuild flam-notes <input.flp> <output.flp> <pattern-id> <channel-id> <stroke-ticks> <velocity-0..127> <before|after> [group-notes:0|1]\n",
            "  flp-rebuild flam-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <stroke-ticks> <velocity-0..127> <before|after> [group-notes:0|1]\n",
            "  flp-rebuild generate-random-notes <input.flp> <output.flp> <pattern-id> <channel-id> <start-ticks> <end-ticks-exclusive> <step-ticks> <note-length-ticks> <density-0..100> <minimum-key-0..127> <maximum-key-0..127> <seed> [stack-1..16 [length-variation:-100..100 [merge-same-notes:0|1 [base-octave-0..10 range-octaves-1..11 [chord-map-name chord-root-0..11]]]]]\n",
            "  flp-rebuild generate-random-notes-scale <input.flp> <output.flp> <pattern-id> <channel-id> <start-ticks> <end-ticks-exclusive> <step-ticks> <note-length-ticks> <density-0..100> <minimum-key-0..127> <maximum-key-0..127> <seed> <scale-name> <scale-root-0..11> [stack-1..16 [length-variation:-100..100 [merge-same-notes:0|1 [base-octave-0..10 range-octaves-1..11 [chord-map-name chord-root-0..11]]]]]\n",
            "  flp-rebuild randomize-notes <input.flp> <output.flp> <pattern-id> <channel-id> <seed> <velocity:-100..100> <pan:-100..100> <pitch-range:0..24> <bipolar:0|1> <reset:0|1> [release:-100..100 mod-x:-100..100 mod-y:-100..100]\n",
            "  flp-rebuild randomize-notes-scale <input.flp> <output.flp> <pattern-id> <channel-id> <seed> <velocity:-100..100> <pan:-100..100> <pitch-range:0..24> <bipolar:0|1> <reset:0|1> <scale-name> <scale-root:0..11> [release:-100..100 mod-x:-100..100 mod-y:-100..100]\n",
            "  flp-rebuild randomize-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <seed> <velocity:-100..100> <pan:-100..100> <pitch-range:0..24> <bipolar:0|1> <reset:0|1> [release:-100..100 mod-x:-100..100 mod-y:-100..100]\n",
            "  flp-rebuild randomize-notes-selected-scale <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <seed> <velocity:-100..100> <pan:-100..100> <pitch-range:0..24> <bipolar:0|1> <reset:0|1> <scale-name> <scale-root:0..11> [release:-100..100 mod-x:-100..100 mod-y:-100..100]\n",
            "  flp-rebuild humanize-notes <input.flp> <output.flp> <pattern-id> <channel-id> <seed> <timing-range-ticks|1/n> <velocity-variation-0..100> [pan-variation-0..100 release-variation-0..100 mod-x-variation-0..100 mod-y-variation-0..100]\n",
            "  flp-rebuild humanize-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <seed> <timing-range-ticks|1/n> <velocity-variation-0..100> [pan-variation-0..100 release-variation-0..100 mod-x-variation-0..100 mod-y-variation-0..100]\n",
            "    Humanize timing accepts ticks or 1/1, 1/2, 1/4, 1/8, 1/16, 1/32, 1/64, or 1/128, rounded to project ticks using PPQ.\n",
            "  flp-rebuild limit-notes <input.flp> <output.flp> <pattern-id> <channel-id> <minimum-key-0..127> <maximum-key-0..127> [wrap-to-bottom:0|1 scale-name|none scale-root:0..11|-> snap-direction:up|down|alternate]\n",
            "  flp-rebuild limit-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <minimum-key-0..127> <maximum-key-0..127> [wrap-to-bottom:0|1 scale-name|none scale-root:0..11|-> snap-direction:up|down|alternate]\n",
            "  flp-rebuild arp-notes <input.flp> <output.flp> <pattern-id> <channel-id> <step-ticks|1/n> <range-octaves-1..4> <gate-1..100> <up|down|up-down|alternate> [time|block|chord [group-notes:0|1]]\n",
            "  flp-rebuild arp-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <step-ticks|1/n> <range-octaves-1..4> <gate-1..100> <up|down|up-down|alternate> [time|block|chord [group-notes:0|1]]\n",
            "  flp-rebuild arp-score <input.flp> <input.fsc> <output.flp> <pattern-id> <channel-id> <range-octaves-1..4> <gate-1..100> <up|down|up-down|alternate> <time|block|chord> <group-notes:0|1> <normal|flip-vertical|alternate> <time-multiplier:1/8|1/4|1/2|1|2|3|4> <levels:0..100>\n",
            "  flp-rebuild arp-score-selected <input.flp> <input.fsc> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <range-octaves-1..4> <gate-1..100> <up|down|up-down|alternate> <time|block|chord> <group-notes:0|1> <normal|flip-vertical|alternate> <time-multiplier:1/8|1/4|1/2|1|2|3|4> <levels:0..100>\n",
            "    Arpeggiator step accepts ticks or 1/1, 1/2, 1/4, 1/8, 1/16, 1/32, 1/64, or 1/128, rounded to project ticks using PPQ.\n",
            "  flp-rebuild riff-machine-notes <input.flp> <output.flp> <pattern-id> <channel-id> [--option=value ...]\n",
            "  flp-rebuild riff-machine-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> [--option=value ...]\n",
            "  flp-rebuild slice-notes <input.flp> <output.flp> <pattern-id> <channel-id> <position-ticks>\n",
            "  flp-rebuild slice-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <position-ticks>\n",
            "  flp-rebuild slice-notes-at-positions <input.flp> <output.flp> <pattern-id> <cuts:channel-id:note-index:tick,...>\n",
            "  flp-rebuild scale-levels-notes <input.flp> <output.flp> <pattern-id> <channel-id> <center:-100..100> <tension:-100..100> <multiplier:0..200> <offset:-100..100>\n",
            "  flp-rebuild scale-levels-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <center:-100..100> <tension:-100..100> <multiplier:0..200> <offset:-100..100>\n",
            "  flp-rebuild articulate-notes <input.flp> <output.flp> <pattern-id> <channel-id> <multiplier:10..100> <variation:0..100> <seed> <use-original-lengths:0|1> <chop-chords:0|1>\n",
            "  flp-rebuild articulate-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <multiplier:10..100> <variation:0..100> <seed> <use-original-lengths:0|1> <chop-chords:0|1> <only-with-selection:0|1>\n",
            "  flp-rebuild articulate-preset <input.flp> <output.flp> <pattern-id> <channel-id> <legato|portato|staccato|small-gap|chop-chords>\n",
            "  flp-rebuild articulate-selected-preset <input.flp> <output.flp> <pattern-id> <channel-id> <legato|portato|staccato|small-gap|chop-chords> <note-indices:comma-separated-zero-based>\n",
            "    Articulate presets use the Piano roll quick settings with variation disabled.\n",
            "  flp-rebuild claw-notes <input.flp> <output.flp> <pattern-id> <channel-id> <period-ticks:16+> <trash-every:2..16> <time-distortion:-100..100> <remove-short-notes:0|1> <stretch-to-compensate:0|1>\n",
            "  flp-rebuild claw-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based> <period-ticks:16+> <trash-every:2..16> <time-distortion:-100..100> <remove-short-notes:0|1> <stretch-to-compensate:0|1>\n",
            "  flp-rebuild add-note <input.flp> <output.flp> <pattern-id> <channel-id> <position> <length> <key> <velocity>\n",
            "  flp-rebuild add-chord <input.flp> <output.flp> <pattern-id> <channel-id> <position> <length> <root-key> <velocity> <quality> <inversion>\n",
            "  flp-rebuild add-scale-chord <input.flp> <output.flp> <pattern-id> <channel-id> <position> <length> <root-or-top-key> <velocity> <quality> <scale|none> <scale-root:0..11> <inversion>\n",
            "    Scale qualities: scale-triad, scale-seventh, scale-ninth, scale-eleventh, scale-thirteenth, top-down-triad, top-down-seventh.\n",
            "    Scale intervals stack diatonic thirds; top-down stamps put the snapped key at the top. With scale=none, scale chords use major intervals.\n",
            "    Manual qualities: major, minor, diminished, augmented, sus2, sus4, power, dominant7, major7, minor7, major6, minor6, add9, minor-add9, diminished7, half-diminished7, minor-major7, dominant7-flat5, dominant7-sharp5, dominant7-flat9, dominant7-sharp9, dominant9, major9, minor9, dominant11, minor11, dominant13.\n",
            "    Inversion 0 keeps root position; each inversion raises the lowest pitch by one octave.\n",
            "  flp-rebuild add-notes <input.flp> <output.flp> <pattern-id> <channel-id> <notes:position:length:key:velocity,...>\n",
            "  flp-rebuild delete-note <input.flp> <output.flp> <pattern-id> <channel-id> <note-index>\n",
            "  flp-rebuild delete-notes <input.flp> <output.flp> <pattern-id> <channel-id>\n",
            "  flp-rebuild delete-notes-selected <input.flp> <output.flp> <pattern-id> <channel-id> <note-indices:comma-separated-zero-based>\n",
            "  flp-rebuild create-playlist-pattern-clip <input.flp> <output.flp> <arrangement-id> <pattern-id> <position-ticks> <length-ticks> <track-index:zero-based>\n",
            "  flp-rebuild create-playlist-audio-clip <input.flp> <output.flp> <arrangement-id> <channel-id> <position-ticks> <length-ticks> <track-index:zero-based>\n",
            "  flp-rebuild create-playlist-automation-clip <input.flp> <output.flp> <arrangement-id> <channel-id> <position-ticks> <length-ticks> <track-index:zero-based>\n",
            "  flp-rebuild import-midi <input.flp> <input.mid> <output.flp> <track> <pattern-id|new> <channel-id> [zero-velocity-note-ons:0|1] [realign-events:0|1] [midi-channels:all|none|1,2,...,16]\n",
            "  flp-rebuild import-score <input.flp> <input.fsc> <output.flp> <pattern-id> <channel-id> [replace-channel-notes:0|1]\n",
            "  flp-rebuild export-score <input.flp> <output.fsc> <pattern-id> <channel-id>\n",
            "  flp-rebuild import-score-automation <input.flp> <input.fsc> <output.flp> <pattern-id> <source-channel-0..255> <source-flags-0..255> <target-channel-0..255> <target-flags-0..255> <replace-target-stream:0|1>\n",
            "  flp-rebuild export-score-automation <input.flp> <output.fsc> <pattern-id> <stream-channel-0..255> <stream-flags-0..255>\n",
            "  flp-rebuild import-midi-cc <input.flp> <input.mid> <output.flp> <track-index> <pattern-id> <raw-channel> <raw-flags> <cc-0..127> <midi-channel-1..16|all>\n",
            "  flp-rebuild import-midi-cc-new-stream <input.flp> <input.mid> <output.flp> <track-index> <pattern-id> <raw-channel> <raw-flags> <cc-0..127> <midi-channel-1..16|all>\n",
            "  flp-rebuild import-midi-all <input.flp> <input.mid> <output.flp> <pattern-id|new> <channel-ids-comma-separated> [zero-velocity-note-ons:0|1] [realign-events:0|1] [midi-channels:all|none|1,2,...,16]\n",
            "  flp-rebuild import-midi-named-patterns <input.flp> <input.mid> <output.flp> <channel-id> [zero-velocity-note-ons:0|1] [realign-events:0|1] [midi-channels:all|none|1,2,...,16]\n",
            "  flp-rebuild import-midi-selected <input.flp> <input.mid> <output.flp> <pattern-id|new> <track-numbers:all|1,2,...> <selected-track-channel-ids-comma-separated> [zero-velocity-note-ons:0|1 realign-events:0|1 midi-channels:all|none|1,2,...,16]\n",
            "  flp-rebuild import-midi-sampler-tracks-selected <input.flp> <input.mid> <output.flp> <pattern-id|new> <template-channel-id> <track-numbers:all|1,2,...> [zero-velocity-note-ons:0|1 realign-events:0|1 midi-channels:all|none|1,2,...,16 set-mixer-tracks:0|1]\n",
            "  flp-rebuild import-midi-format2-selected <input.flp> <input.mid> <output.flp> <template-channel-id> <track-numbers:all|1,2,...> [zero-velocity-note-ons:0|1 realign-events:0|1 midi-channels:all|none|1,2,...,16 set-mixer-tracks:0|1]\n",
            "  flp-rebuild import-midi-new-project <template.flp> <input.mid> <output.flp> <template-channel-id> <track-numbers:all|1,2,...> [zero-velocity-note-ons:0|1 realign-events:0|1 midi-channels:all|none|1,2,...,16 set-mixer-tracks:0|1]\n",
            "  flp-rebuild import-midi-template-tracks <input.flp> <input.mid> <output.flp> <pattern-id|new> <template-channel-id> <track-numbers:all|1,2,...> [zero-velocity-note-ons:0|1 realign-events:0|1 midi-channels:all|none|1,2,...,16 set-mixer-tracks:0|1]\n",
            "  flp-rebuild import-midi-template-format2 <input.flp> <input.mid> <output.flp> <template-channel-id> <track-numbers:all|1,2,...> [zero-velocity-note-ons:0|1 realign-events:0|1 midi-channels:all|none|1,2,...,16 set-mixer-tracks:0|1]\n",
            "  flp-rebuild import-midi-sampler-tracks <input.flp> <input.mid> <output.flp> <pattern-id|new> <template-channel-id> [zero-velocity-note-ons:0|1] [realign-events:0|1] [midi-channels:all|none|1,2,...,16] [set-mixer-tracks:0|1]\n",
            "  flp-rebuild import-midi-format2 <input.flp> <input.mid> <output.flp> <template-channel-id> [zero-velocity-note-ons:0|1] [realign-events:0|1] [midi-channels:all|none|1,2,...,16] [set-mixer-tracks:0|1]\n",
            "  flp-rebuild edit-clip <input.flp> <output.flp> <arrangement-id> <clip-index> <position> <length>\n",
            "  flp-rebuild delete-playlist-clip <input.flp> <output.flp> <arrangement-id> <clip-index>\n",
            "  flp-rebuild ripple-delete-playlist-time-range <input.flp> <output.flp> <arrangement-id> <start-tick> <end-tick> [clip-index=full-source-ms ...]\n",
            "  flp-rebuild insert-playlist-time-range <input.flp> <output.flp> <arrangement-id> <start-tick> <end-tick> [clip-index=full-source-ms ...]\n",
            "  flp-rebuild set-clip-muted <input.flp> <output.flp> <arrangement-id> <clip-index> <muted:0|1>\n",
            "  flp-rebuild edit-playlist-track <input.flp> <output.flp> <track-id> <enabled:0|1|-> <grouped:0|1|->\n",
            "  flp-rebuild set-playlist-track-press <input.flp> <output.flp> <arrangement-id> <track-id> <retrigger|hold-stop|hold-motion|latch>\n",
            "  flp-rebuild set-playlist-track-motion <input.flp> <output.flp> <arrangement-id> <track-id> <stay|one-shot|march-wrap|march-stay|march-stop|random|exclusive-random>\n",
            "  flp-rebuild set-playlist-track-queued <input.flp> <output.flp> <arrangement-id> <track-id> <queued:0|1>\n",
            "  flp-rebuild rename-playlist-track <input.flp> <output.flp> <track-id> <name>\n",
            "  flp-rebuild edit-clip-properties <input.flp> <output.flp> <arrangement-id> <clip-index> <item-index-raw|-> <track-index-raw|-> <group-raw|-> <flags-raw|-> <start-offset|-> <end-offset|-> <scale|->\n",
            "  flp-rebuild edit-audio-clip-window-ticks <input.flp> <output.flp> <arrangement-id> <clip-index> <source-start-tick> <source-end-tick> <timeline-length|->\n",
            "  flp-rebuild duplicate-clip <input.flp> <output.flp> <arrangement-id> <clip-index> <position|-> <track-index-raw|->\n",
            "  flp-rebuild split-audio-clip <input.flp> <output.flp> <arrangement-id> <clip-index> <split-position-ticks> <source-length-ms|->\n",
            "  flp-rebuild split-pattern-clip <input.flp> <output.flp> <arrangement-id> <clip-index> <split-position-ticks>\n",
            "  flp-rebuild split-automation-clip <input.flp> <output.flp> <arrangement-id> <clip-index> <split-position-ticks>\n",
            "  flp-rebuild join-audio-clips <input.flp> <output.flp> <arrangement-id> <left-clip-index> <right-clip-index>\n",
            "  flp-rebuild make-audio-clip-unique <input.flp> <output.flp> <arrangement-id> <clip-index>\n",
            "  flp-rebuild make-automation-clip-unique <input.flp> <output.flp> <arrangement-id> <clip-index>\n",
            "  flp-rebuild make-audio-clip-unique-as-sample <input.flp> <output.flp> <arrangement-id> <clip-index>\n",
            "  flp-rebuild set-playlist-audio-clip-reversed <input.flp> <output.flp> <arrangement-id> <clip-index> <reversed:0|1>\n",
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

fn parse_note_indices(value: &str) -> Result<Vec<usize>, String> {
    let indices = value
        .split(',')
        .map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return Err("note indexes must be comma-separated non-negative integers".to_owned());
            }
            parse_usize(part, "note index")
        })
        .collect::<Result<Vec<_>, _>>()?;
    if indices.is_empty() {
        return Err("at least one channel-local note index is required".to_owned());
    }
    Ok(indices)
}

fn parse_note_specs(channel_id: u16, value: &str) -> Result<Vec<PatternNote>, String> {
    let mut notes = Vec::new();
    for note in value.split(',') {
        let mut fields = note.split(':');
        let position = fields.next().unwrap_or_default();
        let length = fields.next().unwrap_or_default();
        let key = fields.next().unwrap_or_default();
        let velocity = fields.next().unwrap_or_default();
        if position.is_empty()
            || length.is_empty()
            || key.is_empty()
            || velocity.is_empty()
            || fields.next().is_some()
        {
            return Err(
                "notes must be comma-separated position:length:key:velocity tuples".to_owned(),
            );
        }
        notes.push(PatternNote {
            channel_id,
            position: parse_u32(position, "note position")?,
            length: parse_u32(length, "note length")?,
            key: parse_u16(key, "note key")?,
            velocity: parse_u8(velocity, "note velocity")?,
            ..PatternNote::default()
        });
    }
    Ok(notes)
}

fn parse_pattern_controller_points(value: &str) -> Result<Vec<(u32, f32)>, String> {
    let mut points = Vec::new();
    for point in value.split(',') {
        let mut fields = point.split(':');
        let position = fields.next().unwrap_or_default();
        let value = fields.next().unwrap_or_default();
        if position.is_empty() || value.is_empty() || fields.next().is_some() {
            return Err("controller points must be comma-separated tick:value pairs".to_owned());
        }
        points.push((
            parse_u32(position, "controller position")?,
            parse_f32(value, "controller value")?,
        ));
    }
    Ok(points)
}

fn parse_note_cuts(value: &str) -> Result<Vec<(u16, usize, u32)>, String> {
    let mut cuts = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for cut in value.split(',') {
        let mut fields = cut.split(':');
        let channel_id = fields.next().unwrap_or_default();
        let note_index = fields.next().unwrap_or_default();
        let position_ticks = fields.next().unwrap_or_default();
        if channel_id.is_empty()
            || note_index.is_empty()
            || position_ticks.is_empty()
            || fields.next().is_some()
        {
            return Err(
                "note cuts must be comma-separated channel-id:note-index:tick triples".to_owned(),
            );
        }
        let channel_id = parse_u16(channel_id, "note cut channel id")?;
        let note_index = parse_usize(note_index, "note cut channel-local index")?;
        let position_ticks = parse_u32(position_ticks, "note cut position")?;
        if !seen.insert((channel_id, note_index)) {
            return Err(format!(
                "note cut for channel {channel_id}, note {note_index} was supplied more than once"
            ));
        }
        cuts.push((channel_id, note_index, position_ticks));
    }
    Ok(cuts)
}

fn parse_flam_placement(value: &str) -> Result<bool, String> {
    match value {
        "before" => Ok(true),
        "after" => Ok(false),
        _ => Err("flam placement must be 'before' or 'after'".to_owned()),
    }
}

fn parse_tempo_relative_ticks(
    value: &str,
    document: &FlpDocument,
    tick_description: &str,
) -> Result<u32, String> {
    let Some((numerator, denominator)) = value.split_once('/') else {
        return parse_u32(value, tick_description);
    };
    if numerator != "1" {
        return Err("note division must be written as 1/<denominator>".to_owned());
    }
    let denominator = parse_u16(denominator, "note division denominator")?;
    if !matches!(denominator, 1 | 2 | 4 | 8 | 16 | 32 | 64 | 128) {
        return Err(
            "note division must be 1/1, 1/2, 1/4, 1/8, 1/16, 1/32, 1/64, or 1/128".to_owned(),
        );
    }

    let ppq = document.header().ppq();
    if ppq == 0 {
        return Err("note division cannot be mapped with a zero project PPQ".to_owned());
    }
    let quarter_note_ticks = u64::from(ppq) * 4;
    let denominator = u64::from(denominator);
    let ticks = (quarter_note_ticks + denominator / 2) / denominator;
    if ticks == 0 {
        return Err("note division rounds to less than one project tick".to_owned());
    }
    u32::try_from(ticks).map_err(|_| "note division exceeds the supported tick range".to_owned())
}

fn parse_humanize_timing_range(value: &str, document: &FlpDocument) -> Result<u32, String> {
    parse_tempo_relative_ticks(value, document, "humanize timing range ticks")
}

#[derive(Clone, Copy)]
struct ChordStampCliOptions {
    pattern_id: u16,
    channel_id: u16,
    position: u32,
    length: u32,
    root_key: u16,
    velocity: u8,
    inversion: usize,
}

#[derive(Clone, Copy)]
struct HumanizeCommandOptions<'a> {
    seed: u64,
    timing_range: &'a str,
    velocity_variation_percent: u8,
    pan_variation_percent: u8,
    release_variation_percent: u8,
    mod_x_variation_percent: u8,
    mod_y_variation_percent: u8,
}

impl<'a> HumanizeCommandOptions<'a> {
    fn new(seed: u64, timing_range: &'a str, velocity_variation_percent: u8) -> Self {
        let defaults = HumanizerOptions::default();
        Self {
            seed,
            timing_range,
            velocity_variation_percent,
            pan_variation_percent: defaults.pan_variation_percent,
            release_variation_percent: defaults.release_variation_percent,
            mod_x_variation_percent: defaults.mod_x_variation_percent,
            mod_y_variation_percent: defaults.mod_y_variation_percent,
        }
    }

    fn resolve(self, document: &FlpDocument) -> Result<HumanizerOptions, String> {
        Ok(HumanizerOptions {
            timing_range_ticks: parse_humanize_timing_range(self.timing_range, document)?,
            velocity_variation_percent: self.velocity_variation_percent,
            pan_variation_percent: self.pan_variation_percent,
            release_variation_percent: self.release_variation_percent,
            mod_x_variation_percent: self.mod_x_variation_percent,
            mod_y_variation_percent: self.mod_y_variation_percent,
        })
    }
}

fn parse_limit_note_options(
    minimum_key: &str,
    maximum_key: &str,
    wrap_to_bottom: &str,
    scale_name: &str,
    scale_root: &str,
    snap_direction: &str,
) -> Result<LimitNoteOptions<'static>, String> {
    let (scale_root, scale_intervals) = if scale_name == "none" {
        if scale_root != "-" {
            return Err("scale root must be '-' when scale name is 'none'".to_owned());
        }
        (None, None)
    } else {
        if scale_root == "-" {
            return Err("a scale root is required when a scale name is provided".to_owned());
        }
        let root = parse_u8(scale_root, "scale root")?;
        if root > 11 {
            return Err("scale root must be between 0 and 11".to_owned());
        }
        (Some(root), Some(parse_scale_name(scale_name)?))
    };
    let snap_direction = match snap_direction {
        "up" => LimitSnapDirection::Up,
        "down" => LimitSnapDirection::Down,
        "alternate" => LimitSnapDirection::Alternate,
        _ => return Err("limit snap direction must be 'up', 'down', or 'alternate'".to_owned()),
    };
    Ok(LimitNoteOptions {
        minimum_key: parse_u16(minimum_key, "minimum key")?,
        maximum_key: parse_u16(maximum_key, "maximum key")?,
        wrap_to_bottom: parse_bool(wrap_to_bottom, "wrap to bottom")?,
        scale_root,
        scale_intervals,
        snap_direction,
    })
}

fn parse_midi_channel_mapping(value: &str) -> Result<MidiChannelMapping, String> {
    match value {
        "stored" => Ok(MidiChannelMapping::PreserveNoteChannels),
        "channels" => Ok(MidiChannelMapping::AssignProjectChannels),
        _ => Err("MIDI channel mapping must be 'stored' or 'channels'".to_owned()),
    }
}

fn parse_midi_export_file_format(value: &str) -> Result<MidiExportFileFormat, String> {
    match value {
        "0" | "format0" => Ok(MidiExportFileFormat::Format0),
        "1" | "format1" => Ok(MidiExportFileFormat::Format1),
        _ => Err("MIDI file format must be '0'/'format0' or '1'/'format1'".to_owned()),
    }
}

fn parse_midi_channel_number(value: &str) -> Result<u8, String> {
    let channel = parse_u8(value, "MIDI channel")?;
    if (1..=16).contains(&channel) {
        Ok(channel - 1)
    } else {
        Err("MIDI channel must be from 1 through 16".to_owned())
    }
}

fn parse_midi_import_options(
    zero_velocity_note_ons: &str,
    realign_events: &str,
) -> Result<MidiImportOptions, String> {
    Ok(MidiImportOptions {
        zero_velocity_note_ons_as_notes: parse_bool(
            zero_velocity_note_ons,
            "zero-velocity Note On option",
        )?,
        realign_events: parse_bool(realign_events, "realign events option")?,
        ..MidiImportOptions::default()
    })
}

fn parse_midi_import_options_with_channels(
    zero_velocity_note_ons: &str,
    realign_events: &str,
    midi_channels: &str,
) -> Result<MidiImportOptions, String> {
    let mut options = parse_midi_import_options(zero_velocity_note_ons, realign_events)?;
    options.midi_channel_mask = parse_midi_channel_mask(midi_channels)?;
    Ok(options)
}

fn parse_midi_import_options_with_mixer_tracks(
    zero_velocity_note_ons: &str,
    realign_events: &str,
    midi_channels: &str,
    set_mixer_tracks: &str,
) -> Result<MidiImportOptions, String> {
    let mut options = parse_midi_import_options_with_channels(
        zero_velocity_note_ons,
        realign_events,
        midi_channels,
    )?;
    options.set_mixer_tracks_for_new_channels =
        parse_bool(set_mixer_tracks, "set Mixer tracks for new channels option")?;
    Ok(options)
}

fn parse_midi_channel_mask(value: &str) -> Result<u16, String> {
    if value == "all" {
        return Ok(u16::MAX);
    }
    if value == "none" {
        return Ok(0);
    }

    let mut mask = 0u16;
    for part in value.split(',') {
        let channel = parse_u8(part.trim(), "MIDI channel number")?;
        if !(1..=16).contains(&channel) {
            return Err("MIDI channel numbers must be from 1 through 16".to_owned());
        }
        mask |= 1u16 << u32::from(channel - 1);
    }
    Ok(mask)
}

fn parse_midi_channel_filter(value: &str) -> Result<Option<u8>, String> {
    if value == "all" {
        return Ok(None);
    }
    let channel = parse_u8(value, "MIDI channel number")?;
    if !(1..=16).contains(&channel) {
        return Err("MIDI channel number must be from 1 through 16, or 'all'".to_owned());
    }
    Ok(Some(channel - 1))
}

fn parse_midi_track_selection(value: &str, track_count: usize) -> Result<Vec<usize>, String> {
    if value == "all" {
        return Ok((0..track_count).collect());
    }
    if value == "none" {
        return Err("select at least one MIDI track to import".to_owned());
    }

    let mut indices = Vec::new();
    for part in value.split(',') {
        let track_number = parse_usize(part.trim(), "MIDI track number")?;
        if track_number == 0 || track_number > track_count {
            return Err(format!(
                "MIDI track numbers must be from 1 through {track_count}"
            ));
        }
        let track_index = track_number - 1;
        if indices.contains(&track_index) {
            return Err(format!(
                "MIDI track {track_number} was selected more than once"
            ));
        }
        indices.push(track_index);
    }
    if indices.is_empty() {
        return Err("select at least one MIDI track to import".to_owned());
    }
    Ok(indices)
}

fn parse_midi_pattern_target(value: &str) -> Result<Option<u16>, String> {
    if value == "new" {
        return Ok(None);
    }
    let pattern_id = parse_u16(value, "pattern id")?;
    if pattern_id == 0 {
        return Err("pattern id must be greater than zero".to_owned());
    }
    Ok(Some(pattern_id))
}

fn parse_u8(value: &str, description: &str) -> Result<u8, String> {
    value
        .parse::<u8>()
        .map_err(|_| format!("{description} must be an integer from 0 through 255"))
}

fn parse_raw_byte(value: &str, description: &str) -> Result<u8, String> {
    if let Some(hex_value) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u8::from_str_radix(hex_value, 16).map_err(|_| {
            format!("{description} must be an integer from 0 through 255, optionally in hex")
        })
    } else {
        parse_u8(value, description)
    }
}

fn parse_automation_point_tail(value: &str) -> Result<[u8; 4], String> {
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if digits.len() != 8 {
        return Err("automation point tail must contain exactly 8 hexadecimal digits".to_owned());
    }
    let mut bytes = [0; 4];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let start = index * 2;
        let pair = digits.get(start..start + 2).ok_or_else(|| {
            "automation point tail must contain exactly 8 hexadecimal digits".to_owned()
        })?;
        *byte = u8::from_str_radix(pair, 16)
            .map_err(|_| "automation point tail must contain exactly 8 hexadecimal digits")?;
    }
    Ok(bytes)
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

fn parse_i32(value: &str, description: &str) -> Result<i32, String> {
    value
        .parse::<i32>()
        .map_err(|_| format!("{description} must be a signed 32-bit integer"))
}

fn parse_mixer_track(value: &str) -> Result<i8, String> {
    let mixer_track = value.parse::<i8>().map_err(|_| {
        "Mixer track must be -1 (Current), 0 (Master), or an insert from 1 through 127".to_owned()
    })?;
    if mixer_track < -1 {
        return Err(
            "Mixer track must be -1 (Current), 0 (Master), or an insert from 1 through 127"
                .to_owned(),
        );
    }
    Ok(mixer_track)
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

fn parse_automation_interpolation_mode(value: &str) -> Result<AutomationInterpolationMode, String> {
    match value {
        "linear" => Ok(AutomationInterpolationMode::Linear),
        "smooth" => Ok(AutomationInterpolationMode::Smooth),
        _ => Err(format!(
            "invalid automation interpolation mode {value:?}; expected linear or smooth"
        )),
    }
}

fn parse_random_note_chord_map(value: &str) -> Result<RandomNoteChordMap, String> {
    RandomNoteChordMap::ALL
        .into_iter()
        .find(|chord_map| chord_map.cli_name() == value)
        .ok_or_else(|| format!("unknown random note chord map: {value}"))
}

fn parse_scale_name(value: &str) -> Result<&'static [u8], String> {
    match value {
        "major" => Ok(&[0, 2, 4, 5, 7, 9, 11]),
        "major-locrian" => Ok(&[0, 2, 4, 5, 6, 8, 10]),
        "harmonic-major" => Ok(&[0, 2, 4, 5, 7, 8, 11]),
        "double-harmonic-major" => Ok(&[0, 1, 4, 5, 7, 8, 11]),
        "ionian-sharp-5" => Ok(&[0, 2, 4, 5, 8, 9, 11]),
        "ionian-sharp-2" => Ok(&[0, 3, 4, 5, 7, 9, 11]),
        "ionian-sharp-2-sharp-6" => Ok(&[0, 3, 4, 5, 7, 10, 11]),
        "ionian-augmented-sharp-2" => Ok(&[0, 3, 4, 5, 8, 9, 11]),
        "dorian" => Ok(&[0, 2, 3, 5, 7, 9, 10]),
        "dorian-b2" => Ok(&[0, 1, 3, 5, 7, 9, 10]),
        "ukrainian-dorian-b2" => Ok(&[0, 1, 3, 6, 7, 9, 10]),
        "dorian-sharp-4" => Ok(&[0, 2, 3, 6, 7, 9, 10]),
        "dorian-b5" => Ok(&[0, 2, 3, 5, 6, 9, 10]),
        "phrygian" => Ok(&[0, 1, 3, 5, 7, 8, 10]),
        "phrygian-b4" => Ok(&[0, 1, 3, 4, 7, 8, 10]),
        "ultraphrygian" => Ok(&[0, 1, 3, 4, 7, 8, 9]),
        "ultraphrygian-bb3" => Ok(&[0, 1, 2, 4, 7, 8, 9]),
        "phrygian-dominant" => Ok(&[0, 1, 4, 5, 7, 8, 10]),
        "lydian" => Ok(&[0, 2, 4, 6, 7, 9, 11]),
        "lydian-sharp-2" => Ok(&[0, 3, 4, 6, 7, 9, 11]),
        "lydian-sharp-2-sharp-6" => Ok(&[0, 3, 4, 6, 7, 10, 11]),
        "lydian-sharp-3-sharp-6" => Ok(&[0, 2, 5, 6, 7, 10, 11]),
        "lydian-sharp-6" => Ok(&[0, 2, 4, 6, 7, 10, 11]),
        "lydian-b3" => Ok(&[0, 2, 3, 6, 7, 9, 11]),
        "lydian-augmented-sharp-2" => Ok(&[0, 3, 4, 6, 8, 9, 11]),
        "lydian-augmented-sharp-3" => Ok(&[0, 2, 5, 6, 8, 9, 11]),
        "lydian-augmented-sharp-6" => Ok(&[0, 2, 4, 6, 8, 10, 11]),
        "lydian-augmented-dominant" => Ok(&[0, 2, 4, 6, 8, 9, 10]),
        "lydian-augmented" => Ok(&[0, 2, 4, 6, 8, 9, 11]),
        "lydian-dominant" => Ok(&[0, 2, 4, 6, 7, 9, 10]),
        "lydian-dominant-b6" => Ok(&[0, 2, 4, 6, 7, 8, 10]),
        "mixolydian" => Ok(&[0, 2, 4, 5, 7, 9, 10]),
        "mixolydian-augmented" => Ok(&[0, 2, 4, 5, 8, 9, 10]),
        "mixolydian-augmented-sharp-2" => Ok(&[0, 3, 4, 5, 8, 9, 10]),
        "mixolydian-b2" => Ok(&[0, 1, 4, 5, 7, 9, 10]),
        "mixolydian-b6" => Ok(&[0, 2, 4, 5, 7, 8, 10]),
        "natural-minor" => Ok(&[0, 2, 3, 5, 7, 8, 10]),
        "locrian" => Ok(&[0, 1, 3, 5, 6, 8, 10]),
        "locrian-dominant" => Ok(&[0, 1, 4, 5, 6, 8, 10]),
        "locrian-natural-2" => Ok(&[0, 2, 3, 5, 6, 8, 10]),
        "locrian-natural-6" => Ok(&[0, 1, 3, 5, 6, 9, 10]),
        "locrian-bb7" => Ok(&[0, 1, 3, 5, 6, 8, 9]),
        "locrian-bb3-bb7" => Ok(&[0, 1, 2, 5, 6, 8, 9]),
        "ultra-locrian-bb3" => Ok(&[0, 1, 2, 4, 6, 8, 9]),
        "harmonic-minor" => Ok(&[0, 2, 3, 5, 7, 8, 11]),
        "harmonic-minor-b5" => Ok(&[0, 2, 3, 5, 6, 8, 11]),
        "melodic-minor" => Ok(&[0, 2, 3, 5, 7, 9, 11]),
        "melodic-minor-sharp-5" => Ok(&[0, 2, 3, 5, 8, 9, 11]),
        "neapolitan-minor" => Ok(&[0, 1, 3, 5, 7, 8, 11]),
        "neapolitan-minor-sharp-4" => Ok(&[0, 1, 3, 6, 7, 8, 11]),
        "romanian-minor" => Ok(&[0, 2, 3, 6, 7, 8, 10]),
        "neapolitan-major" => Ok(&[0, 1, 3, 5, 7, 9, 11]),
        "hungarian-minor" => Ok(&[0, 2, 3, 6, 7, 8, 11]),
        "hungarian-major" => Ok(&[0, 3, 4, 6, 7, 9, 10]),
        "hungarian-major-mode-2" => Ok(&[0, 1, 3, 4, 6, 7, 9]),
        "chromatic-hypophrygian-inverse" => Ok(&[0, 1, 2, 5, 6, 7, 9]),
        "persian" => Ok(&[0, 1, 4, 5, 6, 8, 11]),
        "oriental" => Ok(&[0, 1, 4, 5, 6, 9, 10]),
        "blues" => Ok(&[0, 3, 5, 6, 7, 10]),
        "major-pentatonic" => Ok(&[0, 2, 4, 7, 9]),
        "minor-pentatonic" => Ok(&[0, 3, 5, 7, 10]),
        "whole-tone" => Ok(&[0, 2, 4, 6, 8, 10]),
        "whole-half-diminished" => Ok(&[0, 2, 3, 5, 6, 8, 9, 11]),
        "half-whole-diminished" => Ok(&[0, 1, 3, 4, 6, 7, 9, 10]),
        "half-diminished-b4" => Ok(&[0, 2, 3, 4, 6, 8, 10]),
        "altered" => Ok(&[0, 1, 3, 4, 6, 8, 10]),
        "altered-dominant-natural-6" => Ok(&[0, 1, 3, 4, 6, 9, 10]),
        "altered-dominant-bb3" => Ok(&[0, 1, 2, 4, 6, 8, 10]),
        "super-locrian-bb7" => Ok(&[0, 1, 3, 4, 6, 8, 9]),
        _ => Err("scale name must be major, major-locrian, harmonic-major, double-harmonic-major, ionian-sharp-5, ionian-sharp-2, ionian-sharp-2-sharp-6, ionian-augmented-sharp-2, dorian, dorian-b2, ukrainian-dorian-b2, dorian-sharp-4, dorian-b5, phrygian, phrygian-b4, ultraphrygian, ultraphrygian-bb3, phrygian-dominant, lydian, lydian-sharp-2, lydian-sharp-2-sharp-6, lydian-sharp-3-sharp-6, lydian-sharp-6, lydian-b3, lydian-augmented-sharp-2, lydian-augmented-sharp-3, lydian-augmented-sharp-6, lydian-augmented-dominant, lydian-augmented, lydian-dominant, lydian-dominant-b6, mixolydian, mixolydian-augmented, mixolydian-augmented-sharp-2, mixolydian-b2, mixolydian-b6, natural-minor, locrian, locrian-dominant, locrian-natural-2, locrian-natural-6, locrian-bb7, locrian-bb3-bb7, ultra-locrian-bb3, harmonic-minor, harmonic-minor-b5, melodic-minor, melodic-minor-sharp-5, neapolitan-minor, neapolitan-minor-sharp-4, romanian-minor, neapolitan-major, hungarian-minor, hungarian-major, hungarian-major-mode-2, chromatic-hypophrygian-inverse, persian, oriental, blues, major-pentatonic, minor-pentatonic, whole-tone, whole-half-diminished, half-whole-diminished, half-diminished-b4, altered, altered-dominant-natural-6, altered-dominant-bb3, or super-locrian-bb7".to_owned()),
    }
}

fn parse_chord_intervals(value: &str) -> Result<&'static [u8], String> {
    match value {
        "major" => Ok(&[0, 4, 7]),
        "minor" => Ok(&[0, 3, 7]),
        "diminished" => Ok(&[0, 3, 6]),
        "augmented" => Ok(&[0, 4, 8]),
        "sus2" => Ok(&[0, 2, 7]),
        "sus4" => Ok(&[0, 5, 7]),
        "power" => Ok(&[0, 7, 12]),
        "dominant7" => Ok(&[0, 4, 7, 10]),
        "major7" => Ok(&[0, 4, 7, 11]),
        "minor7" => Ok(&[0, 3, 7, 10]),
        "major6" => Ok(&[0, 4, 7, 9]),
        "minor6" => Ok(&[0, 3, 7, 9]),
        "add9" => Ok(&[0, 4, 7, 14]),
        "minor-add9" => Ok(&[0, 3, 7, 14]),
        "diminished7" => Ok(&[0, 3, 6, 9]),
        "half-diminished7" => Ok(&[0, 3, 6, 10]),
        "minor-major7" => Ok(&[0, 3, 7, 11]),
        "dominant7-flat5" => Ok(&[0, 4, 6, 10]),
        "dominant7-sharp5" => Ok(&[0, 4, 8, 10]),
        "dominant7-flat9" => Ok(&[0, 4, 7, 10, 13]),
        "dominant7-sharp9" => Ok(&[0, 4, 7, 10, 15]),
        "dominant9" => Ok(&[0, 4, 7, 10, 14]),
        "major9" => Ok(&[0, 4, 7, 11, 14]),
        "minor9" => Ok(&[0, 3, 7, 10, 14]),
        "dominant11" => Ok(&[0, 4, 7, 10, 14, 17]),
        "minor11" => Ok(&[0, 3, 7, 10, 14, 17]),
        "dominant13" => Ok(&[0, 4, 7, 10, 14, 17, 21]),
        _ => Err("chord quality must be one of the supported manual stamp names".to_owned()),
    }
}

fn parse_chord_scale(value: &str) -> Result<&'static [u8], String> {
    if value == "none" {
        Ok(&[0, 2, 4, 5, 7, 9, 11])
    } else {
        parse_scale_name(value)
    }
}

fn scale_chord_pitches(
    root_key: u16,
    scale_intervals: &[u8],
    scale_root: u8,
    quality: &str,
) -> Result<Vec<u16>, String> {
    if scale_root > 11 {
        return Err("scale root must be between 0 and 11".to_owned());
    }
    if root_key > 127 {
        return Err("chord root or top key must be between 0 and 127".to_owned());
    }
    if scale_intervals.is_empty() {
        return Err("the selected chord scale has no intervals".to_owned());
    }

    let (degrees, top_down_note_count): (&[usize], Option<usize>) = match quality {
        "scale-triad" => (&[0, 2, 4], None),
        "scale-seventh" => (&[0, 2, 4, 6], None),
        "scale-ninth" => (&[0, 2, 4, 6, 8], None),
        "scale-eleventh" => (&[0, 2, 4, 6, 8, 10], None),
        "scale-thirteenth" => (&[0, 2, 4, 6, 8, 10, 12], None),
        "top-down-triad" => (&[], Some(3)),
        "top-down-seventh" => (&[], Some(4)),
        _ => {
            return Err("scale chord quality must be scale-triad, scale-seventh, scale-ninth, scale-eleventh, scale-thirteenth, top-down-triad, or top-down-seventh".to_owned());
        }
    };

    let contains_scale_pitch = |key: u16| {
        let relative_pitch_class = ((key % 12 + 12 - u16::from(scale_root)) % 12) as u8;
        scale_intervals.contains(&relative_pitch_class)
    };
    let snap_to_scale = |target: u16| {
        (0..=127u16)
            .filter(|key| contains_scale_pitch(*key))
            .min_by_key(|key| key.abs_diff(target))
            .ok_or_else(|| "the selected scale contains no MIDI pitches".to_owned())
    };

    let pitches = if let Some(note_count) = top_down_note_count {
        let top_key = snap_to_scale(root_key)?;
        let relative_pitch_class = ((top_key % 12 + 12 - u16::from(scale_root)) % 12) as u8;
        let top_degree = scale_intervals
            .iter()
            .position(|interval| *interval == relative_pitch_class)
            .ok_or_else(|| "could not locate the top note in the selected scale".to_owned())?;
        let scale_anchor = i32::from(top_key) - i32::from(scale_intervals[top_degree]);
        let scale_len = i32::try_from(scale_intervals.len())
            .map_err(|_| "the selected scale has too many intervals".to_owned())?;
        let top_degree = i32::try_from(top_degree)
            .map_err(|_| "the selected scale has too many intervals".to_owned())?;
        let mut pitches = Vec::new();
        for voice in 0..note_count {
            let voice = i32::try_from(voice)
                .map_err(|_| "the selected chord has too many voices".to_owned())?;
            let scale_index = top_degree - voice * 2;
            let octave_offset = scale_index.div_euclid(scale_len);
            let degree = usize::try_from(scale_index.rem_euclid(scale_len))
                .map_err(|_| "could not calculate the selected scale degree".to_owned())?;
            let key = scale_anchor + octave_offset * 12 + i32::from(scale_intervals[degree]);
            if !(0..=127).contains(&key) {
                return Err(
                    "the selected top-down chord does not fit the MIDI key range".to_owned(),
                );
            }
            pitches
                .push(u16::try_from(key).map_err(|_| "could not encode a chord pitch".to_owned())?);
        }
        pitches
    } else {
        let snapped_root = snap_to_scale(root_key)?;
        let scale_degree = scale_intervals
            .iter()
            .position(|interval| {
                (u16::from(scale_root) + u16::from(*interval)) % 12 == snapped_root % 12
            })
            .ok_or_else(|| "could not locate the chord root in the selected scale".to_owned())?;
        let scale_anchor = i32::from(snapped_root) - i32::from(scale_intervals[scale_degree]);
        let scale_len = i32::try_from(scale_intervals.len())
            .map_err(|_| "the selected scale has too many intervals".to_owned())?;
        degrees
            .iter()
            .filter_map(|degree| {
                let scale_index =
                    i32::try_from(scale_degree).ok()? + i32::try_from(*degree).ok()?;
                let octave_offset = scale_index / scale_len;
                let degree = usize::try_from(scale_index % scale_len).ok()?;
                let key = scale_anchor + octave_offset * 12 + i32::from(scale_intervals[degree]);
                if !(0..=127).contains(&key) {
                    return None;
                }
                u16::try_from(key).ok()
            })
            .collect()
    };
    Ok(pitches)
}

fn unscaled_scale_chord_pitches(root_key: u16, quality: &str) -> Result<Vec<u16>, String> {
    if root_key > 127 {
        return Err("chord root or top key must be between 0 and 127".to_owned());
    }
    let intervals: &[u8] = match quality {
        "scale-triad" => &[0, 4, 7],
        "scale-seventh" => &[0, 4, 7, 11],
        "scale-ninth" => &[0, 4, 7, 11, 14],
        "scale-eleventh" => &[0, 4, 7, 11, 14, 17],
        "scale-thirteenth" => &[0, 4, 7, 11, 14, 17, 21],
        _ => {
            return Err("scale chord quality must be scale-triad, scale-seventh, scale-ninth, scale-eleventh, or scale-thirteenth when no scale is selected".to_owned());
        }
    };
    Ok(intervals
        .iter()
        .filter_map(|interval| root_key.checked_add(u16::from(*interval)))
        .filter(|key| *key <= 127)
        .collect())
}

fn apply_chord_inversion(pitches: &mut Vec<u16>, inversion: usize) -> Result<(), String> {
    pitches.sort_unstable();
    if inversion >= pitches.len() && inversion != 0 {
        return Err(format!(
            "chord inversion must be between 0 and {} for this chord",
            pitches.len().saturating_sub(1)
        ));
    }
    for _ in 0..inversion {
        let lowest = pitches
            .first()
            .copied()
            .ok_or_else(|| "the selected chord contains no MIDI pitches".to_owned())?;
        let raised = lowest
            .checked_add(12)
            .filter(|key| *key <= 127)
            .ok_or_else(|| "the selected chord inversion exceeds MIDI key 127".to_owned())?;
        pitches.remove(0);
        pitches.push(raised);
    }
    pitches.sort_unstable();
    Ok(())
}

fn parse_articulate_preset(value: &str) -> Result<ArticulateOptions, String> {
    let (multiplier_percent, use_original_lengths, chop_chords) = match value {
        "legato" => (100, false, false),
        "portato" => (90, false, false),
        "staccato" => (50, true, false),
        "small-gap" => (98, false, false),
        "chop-chords" => (100, true, true),
        _ => {
            return Err(
                "Articulate preset must be legato, portato, staccato, small-gap, or chop-chords"
                    .to_owned(),
            );
        }
    };
    Ok(ArticulateOptions {
        multiplier_percent,
        variation_percent: 0,
        seed: 1,
        use_original_lengths,
        chop_chords,
    })
}

fn parse_arpeggio_range_pattern(value: &str) -> Result<ArpeggioRangePattern, String> {
    match value {
        "up" => Ok(ArpeggioRangePattern::Up),
        "down" => Ok(ArpeggioRangePattern::Down),
        "up-down" => Ok(ArpeggioRangePattern::UpDown),
        "alternate" => Ok(ArpeggioRangePattern::Alternate),
        _ => Err(
            "arpeggiator range pattern must be 'up', 'down', 'up-down', or 'alternate'".to_owned(),
        ),
    }
}

fn parse_arpeggio_direction(value: &str) -> Result<ArpeggioDirection, String> {
    match value {
        "up" => Ok(ArpeggioDirection::Up),
        "down" => Ok(ArpeggioDirection::Down),
        "up-down" => Ok(ArpeggioDirection::UpDown),
        _ => Err("direction must be 'up', 'down', or 'up-down'".to_owned()),
    }
}

fn parse_arpeggio_sync_mode(value: &str) -> Result<ArpeggioSyncMode, String> {
    match value {
        "time" => Ok(ArpeggioSyncMode::Time),
        "block" => Ok(ArpeggioSyncMode::Block),
        "chord" => Ok(ArpeggioSyncMode::Chord),
        _ => Err("arpeggiator sync mode must be 'time', 'block', or 'chord'".to_owned()),
    }
}

fn parse_arpeggio_score_mode(value: &str) -> Result<ArpeggioScorePatternMode, String> {
    match value {
        "normal" => Ok(ArpeggioScorePatternMode::Normal),
        "flip-vertical" => Ok(ArpeggioScorePatternMode::FlipVertical),
        "alternate" => Ok(ArpeggioScorePatternMode::Alternate),
        _ => Err(
            "arpeggiator score mode must be 'normal', 'flip-vertical', or 'alternate'".to_owned(),
        ),
    }
}

fn parse_arpeggio_score_time_multiplier(
    value: &str,
) -> Result<ArpeggioScoreTimeMultiplier, String> {
    match value {
        "1/2" => Ok(ArpeggioScoreTimeMultiplier::Faster2),
        "1/4" => Ok(ArpeggioScoreTimeMultiplier::Faster4),
        "1/8" => Ok(ArpeggioScoreTimeMultiplier::Faster8),
        "1" => Ok(ArpeggioScoreTimeMultiplier::Normal),
        "2" => Ok(ArpeggioScoreTimeMultiplier::Slower2),
        "3" => Ok(ArpeggioScoreTimeMultiplier::Slower3),
        "4" => Ok(ArpeggioScoreTimeMultiplier::Slower4),
        _ => Err("score time multiplier must be 1/8, 1/4, 1/2, 1, 2, 3, or 4".to_owned()),
    }
}

fn parse_arpeggio_score_options(values: [&str; 8]) -> Result<ArpeggioScoreOptions, String> {
    let [
        range_octaves,
        gate,
        range_pattern,
        sync,
        group_notes,
        pattern_mode,
        time_multiplier,
        levels,
    ] = values;
    Ok(ArpeggioScoreOptions {
        arpeggio: ArpeggioOptions {
            range_octaves: parse_u8(range_octaves, "arpeggiator range")?,
            gate_percent: parse_u8(gate, "arpeggiator gate")?,
            range_pattern: parse_arpeggio_range_pattern(range_pattern)?,
            sync: parse_arpeggio_sync_mode(sync)?,
            group_notes: parse_bool(group_notes, "group notes")?,
            ..ArpeggioOptions::default()
        },
        pattern_mode: parse_arpeggio_score_mode(pattern_mode)?,
        time_multiplier: parse_arpeggio_score_time_multiplier(time_multiplier)?,
        levels_percent: parse_u8(levels, "arpeggiator score levels")?,
    })
}

fn parse_riff_machine_quantize_mode(value: &str) -> Result<RiffMachineQuantizeMode, String> {
    match value {
        "leave-duration" => Ok(RiffMachineQuantizeMode::LeaveDuration),
        "leave-end" => Ok(RiffMachineQuantizeMode::LeaveEnd),
        "quantize-duration" => Ok(RiffMachineQuantizeMode::QuantizeDuration),
        "quantize-end" => Ok(RiffMachineQuantizeMode::QuantizeEnd),
        _ => Err("Riff Machine Groove mode must be leave-duration, leave-end, quantize-duration, or quantize-end".to_owned()),
    }
}

struct ParsedRiffMachineOptions {
    options: RiffMachineOptions<'static>,
    groove_score_path: Option<PathBuf>,
    arpeggio_score_path: Option<PathBuf>,
    progression_score_path: Option<PathBuf>,
    chord_score_path: Option<PathBuf>,
}

fn parse_riff_machine_options(values: &[String]) -> Result<ParsedRiffMachineOptions, String> {
    let mut options = RiffMachineOptions::default();
    let mut groove_score_path = None;
    let mut arpeggio_score_path = None;
    let mut progression_score_path = None;
    let mut chord_score_path = None;
    let mut seen = std::collections::HashSet::new();
    for argument in values {
        let option = argument.strip_prefix("--").ok_or_else(|| {
            format!("Riff Machine options must use --name=value syntax: {argument}")
        })?;
        let (name, value) = option.split_once('=').ok_or_else(|| {
            format!("Riff Machine option must use --name=value syntax: {argument}")
        })?;
        if name.is_empty() || value.is_empty() {
            return Err(format!(
                "Riff Machine option needs a name and value: {argument}"
            ));
        }
        if !seen.insert(name) {
            return Err(format!(
                "Riff Machine option was supplied more than once: --{name}"
            ));
        }
        match name {
            "scale" => options.scale_intervals = parse_scale_name(value)?,
            "root" => {
                options.scale_root = parse_u8(value, "Riff Machine scale root")?;
                if options.scale_root > 11 {
                    return Err("Riff Machine scale root must be between 0 and 11".to_owned());
                }
            }
            "min-key" => options.minimum_key = parse_u16(value, "Riff Machine minimum key")?,
            "max-key" => options.maximum_key = parse_u16(value, "Riff Machine maximum key")?,
            "wrap-to-bottom" => {
                options.wrap_to_bottom = parse_bool(value, "Riff Machine wrap to bottom")?
            }
            "snap-direction" => {
                options.snap_direction = match value {
                    "up" => LimitSnapDirection::Up,
                    "down" => LimitSnapDirection::Down,
                    "alternate" => LimitSnapDirection::Alternate,
                    _ => {
                        return Err(
                            "Riff Machine snap direction must be up, down, or alternate".to_owned()
                        );
                    }
                }
            }
            "step-ticks" => options.step_ticks = parse_u32(value, "Riff Machine step")?,
            "range-octaves" => {
                options.range_octaves = parse_u8(value, "Riff Machine octave range")?
            }
            "gate-percent" => options.gate_percent = parse_u8(value, "Riff Machine gate")?,
            "sync" => options.arpeggiation_sync = parse_arpeggio_sync_mode(value)?,
            "progression-score" => progression_score_path = Some(PathBuf::from(value)),
            "progression-time-multiplier" => {
                options.progression_time_multiplier = parse_arpeggio_score_time_multiplier(value)?
            }
            "chord-score" => chord_score_path = Some(PathBuf::from(value)),
            "chord-time-multiplier" => {
                options.chord_time_multiplier = parse_arpeggio_score_time_multiplier(value)?
            }
            "chord-progression" => {
                options.chord_progression_enabled = parse_bool(value, "chord progression stage")?
            }
            "arpeggiation" => {
                options.arpeggiation_enabled = parse_bool(value, "arpeggiation stage")?
            }
            "mirroring" => options.mirroring_enabled = parse_bool(value, "mirroring stage")?,
            "levels" => options.levels_enabled = parse_bool(value, "levels stage")?,
            "articulation" => {
                options.articulation_enabled = parse_bool(value, "articulation stage")?
            }
            "groove" => options.groove_enabled = parse_bool(value, "groove stage")?,
            "fit" => options.fit_enabled = parse_bool(value, "fit stage")?,
            "arpeggio-score" => arpeggio_score_path = Some(PathBuf::from(value)),
            "arpeggio-mode" => options.arpeggio_score_mode = parse_arpeggio_score_mode(value)?,
            "arpeggio-time-multiplier" => {
                options.arpeggio_score_time_multiplier =
                    parse_arpeggio_score_time_multiplier(value)?
            }
            "arpeggio-levels-percent" => {
                options.arpeggio_score_levels_percent =
                    parse_u8(value, "Riff Machine Arpeggiation score levels")?
            }
            "length-variation-percent" => {
                options.length_variation_percent = parse_u8(value, "Riff Machine length variation")?
            }
            "articulation-seed" => {
                options.articulation_seed = parse_u64(value, "Riff Machine Articulation seed")?
            }
            "use-original-lengths" => {
                options.use_original_lengths = parse_bool(value, "use original lengths")?
            }
            "direction" => options.direction = parse_arpeggio_direction(value)?,
            "mirror-horizontal" => {
                options.mirror_horizontal = parse_bool(value, "horizontal mirror")?
            }
            "preserve-start-times" => {
                options.preserve_start_times = parse_bool(value, "preserve start times")?
            }
            "mirror-vertical" => options.mirror_vertical = parse_bool(value, "vertical mirror")?,
            "group-notes" => options.group_notes = parse_bool(value, "group notes")?,
            "groove-score" => groove_score_path = Some(PathBuf::from(value)),
            "groove-snap-ticks" => {
                options.groove_snap_ticks = if value == "none" {
                    None
                } else {
                    Some(parse_u32(value, "Riff Machine Groove snap ticks")?)
                }
            }
            "groove-start-percent" => {
                options.groove_start_percent = parse_u8(value, "Groove start percentage")?
            }
            "groove-sensitivity-percent" => {
                options.groove_sensitivity_percent =
                    parse_u8(value, "Groove sensitivity percentage")?
            }
            "groove-duration-percent" => {
                options.groove_duration_percent = parse_u8(value, "Groove duration percentage")?
            }
            "groove-velocity-level-percent" => {
                options.groove_velocity_level_percent =
                    parse_u8(value, "Groove velocity level percentage")?
            }
            "groove-pan-level-percent" => {
                options.groove_pan_level_percent = parse_u8(value, "Groove pan level percentage")?
            }
            "groove-mod-x-level-percent" => {
                options.groove_mod_x_level_percent =
                    parse_u8(value, "Groove Mod X level percentage")?
            }
            "groove-mod-y-level-percent" => {
                options.groove_mod_y_level_percent =
                    parse_u8(value, "Groove Mod Y level percentage")?
            }
            "groove-pitch-level-percent" => {
                options.groove_pitch_level_percent =
                    parse_u8(value, "Groove pitch level percentage")?
            }
            "groove-mode" => {
                options.groove_quantize_mode = parse_riff_machine_quantize_mode(value)?
            }
            "pan-variation-percent" => {
                options.pan_variation_percent = parse_u8(value, "pan variation")?
            }
            "length-multiplier-percent" => {
                options.length_multiplier_percent = parse_u8(value, "length multiplier")?
            }
            "velocity-variation-percent" => {
                options.velocity_variation_percent = parse_u8(value, "velocity variation")?
            }
            "release-variation-percent" => {
                options.release_variation_percent = parse_u8(value, "release variation")?
            }
            "mod-x-variation-percent" => {
                options.mod_x_variation_percent = parse_u8(value, "Mod X variation")?
            }
            "mod-y-variation-percent" => {
                options.mod_y_variation_percent = parse_u8(value, "Mod Y variation")?
            }
            "pitch-variation-semitones" => {
                options.pitch_variation_semitones = parse_u8(value, "pitch variation")?
            }
            "reset-levels" => options.reset_levels = parse_bool(value, "reset levels")?,
            "bipolar-levels" => options.bipolar_levels = parse_bool(value, "bipolar levels")?,
            "seed" => options.seed = parse_u64(value, "Riff Machine seed")?,
            _ => return Err(format!("unknown Riff Machine option: --{name}")),
        }
    }
    Ok(ParsedRiffMachineOptions {
        options,
        groove_score_path,
        arpeggio_score_path,
        progression_score_path,
        chord_score_path,
    })
}

fn load_riff_machine_score(path: &Path) -> Result<FscScore, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("could not read Riff Machine score: {error}"))?;
    FscScore::parse(&bytes).map_err(|error| format!("invalid Riff Machine score: {error}"))
}

fn parse_note_rotation(value: &str) -> Result<PatternNoteRotation, String> {
    match value {
        "left" => Ok(PatternNoteRotation::Left),
        "right" => Ok(PatternNoteRotation::Right),
        _ => Err("note rotation direction must be 'left' or 'right'".to_owned()),
    }
}

fn parse_quantize_note_mode(value: &str) -> Result<QuantizeNoteMode, String> {
    match value {
        "leave-duration" => Ok(QuantizeNoteMode::LeaveDuration),
        "leave-end" => Ok(QuantizeNoteMode::LeaveEnd),
        "quantize-duration" => Ok(QuantizeNoteMode::QuantizeDuration),
        "quantize-end" => Ok(QuantizeNoteMode::QuantizeEnd),
        _ => Err(
            "quantize mode must be leave-duration, leave-end, quantize-duration, or quantize-end"
                .to_owned(),
        ),
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
                return Err("channel IDs must be comma-separated integers".to_owned());
            }
            parse_u16(child_id, "channel id")
        })
        .collect()
}

fn parse_channel_group_index(value: &str) -> Result<i32, String> {
    let index = parse_usize(value, "Channel Rack group index")?;
    i32::try_from(index)
        .map_err(|_| "Channel Rack group index must fit within a signed 32-bit integer".to_owned())
}

fn parse_channel_sort_order(value: &str) -> Result<ChannelSortOrder, String> {
    match value.to_ascii_lowercase().as_str() {
        "color" => Ok(ChannelSortOrder::Color),
        "mixer-track" => Ok(ChannelSortOrder::MixerTrack),
        "name" => Ok(ChannelSortOrder::Name),
        "type" => Ok(ChannelSortOrder::Type),
        _ => Err("channel sort order must be color, mixer-track, name, or type".to_owned()),
    }
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

fn parse_sampler_envelope_lfo_target(value: &str) -> Result<SamplerEnvelopeLfoTarget, String> {
    match value.to_ascii_lowercase().as_str() {
        "panning" | "pan" => Ok(SamplerEnvelopeLfoTarget::Panning),
        "volume" => Ok(SamplerEnvelopeLfoTarget::Volume),
        "mod-x" | "modx" => Ok(SamplerEnvelopeLfoTarget::ModX),
        "mod-y" | "mody" => Ok(SamplerEnvelopeLfoTarget::ModY),
        "pitch" => Ok(SamplerEnvelopeLfoTarget::Pitch),
        _ => Err(
            "Sampler envelope/LFO target must be panning, volume, mod-x, mod-y, or pitch"
                .to_owned(),
        ),
    }
}

fn parse_playlist_track_sync(value: &str) -> Result<PlaylistTrackSync, String> {
    match value.to_ascii_lowercase().as_str() {
        "off" => Ok(PlaylistTrackSync::Off),
        "quarter-beat" => Ok(PlaylistTrackSync::QuarterBeat),
        "half-beat" => Ok(PlaylistTrackSync::HalfBeat),
        "beat" => Ok(PlaylistTrackSync::Beat),
        "two-beats" => Ok(PlaylistTrackSync::TwoBeats),
        "four-beats" => Ok(PlaylistTrackSync::FourBeats),
        "auto" => Ok(PlaylistTrackSync::Auto),
        _ => Err(format!(
            "Playlist Trigger Sync must be off, quarter-beat, half-beat, beat, two-beats, four-beats, or auto; got {value:?}"
        )),
    }
}

fn parse_playlist_track_press(value: &str) -> Result<PlaylistTrackPress, String> {
    match value.to_ascii_lowercase().as_str() {
        "retrigger" => Ok(PlaylistTrackPress::Retrigger),
        "hold-stop" => Ok(PlaylistTrackPress::HoldStop),
        "hold-motion" => Ok(PlaylistTrackPress::HoldMotion),
        "latch" => Ok(PlaylistTrackPress::Latch),
        _ => Err(format!(
            "Playlist track Press must be retrigger, hold-stop, hold-motion, or latch; got {value:?}"
        )),
    }
}

fn parse_playlist_track_motion(value: &str) -> Result<PlaylistTrackMotion, String> {
    match value.to_ascii_lowercase().as_str() {
        "stay" => Ok(PlaylistTrackMotion::Stay),
        "one-shot" => Ok(PlaylistTrackMotion::OneShot),
        "march-wrap" => Ok(PlaylistTrackMotion::MarchWrap),
        "march-stay" => Ok(PlaylistTrackMotion::MarchStay),
        "march-stop" => Ok(PlaylistTrackMotion::MarchStop),
        "random" => Ok(PlaylistTrackMotion::Random),
        "exclusive-random" => Ok(PlaylistTrackMotion::ExclusiveRandom),
        _ => Err(format!(
            "Playlist track Motion must be stay, one-shot, march-wrap, march-stay, march-stop, random, or exclusive-random; got {value:?}"
        )),
    }
}

fn playlist_track_sync_cli_label(sync: PlaylistTrackSync) -> &'static str {
    match sync {
        PlaylistTrackSync::Off => "off",
        PlaylistTrackSync::QuarterBeat => "quarter-beat",
        PlaylistTrackSync::HalfBeat => "half-beat",
        PlaylistTrackSync::Beat => "beat",
        PlaylistTrackSync::TwoBeats => "two-beats",
        PlaylistTrackSync::FourBeats => "four-beats",
        PlaylistTrackSync::Auto => "auto",
    }
}

fn playlist_track_press_cli_label(press: PlaylistTrackPress) -> &'static str {
    match press {
        PlaylistTrackPress::Retrigger => "retrigger",
        PlaylistTrackPress::HoldStop => "hold-stop",
        PlaylistTrackPress::HoldMotion => "hold-motion",
        PlaylistTrackPress::Latch => "latch",
    }
}

fn playlist_track_motion_cli_label(motion: PlaylistTrackMotion) -> &'static str {
    match motion {
        PlaylistTrackMotion::Stay => "stay",
        PlaylistTrackMotion::OneShot => "one-shot",
        PlaylistTrackMotion::MarchWrap => "march-wrap",
        PlaylistTrackMotion::MarchStay => "march-stay",
        PlaylistTrackMotion::MarchStop => "march-stop",
        PlaylistTrackMotion::Random => "random",
        PlaylistTrackMotion::ExclusiveRandom => "exclusive-random",
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

fn parse_optional_i32(value: &str, description: &str) -> Result<Option<i32>, String> {
    if value == "-" {
        Ok(None)
    } else {
        value
            .parse::<i32>()
            .map(Some)
            .map_err(|_| format!("{description} must be a signed 32-bit integer or -"))
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
        return Err("color must be six hexadecimal digits (RRGGBB)".to_owned());
    }
    let parse_component = |start| {
        u8::from_str_radix(&digits[start..start + 2], 16)
            .map_err(|_| "color must be six hexadecimal digits (RRGGBB)".to_owned())
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
    let channel = document
        .channels()
        .into_iter()
        .find(|channel| channel.id() == channel_id)
        .ok_or_else(|| format!("channel id {channel_id} was not found"))?;

    let class_uid = matching_project_class_uid(&document, channel_id, bundle_path);
    let channel_swing_mix_raw = channel.swing_mix();
    let pitch_offset_cents = channel
        .master_pitch_render_offset_cents(document.metadata().master_pitch_cents())
        .saturating_add(channel.keyboard_fine_tune_cents().unwrap_or_default() as i16);
    let note_pitch_offset_semitones =
        channel.master_pitch_note_offset_semitones(document.metadata().master_pitch_cents());
    let mut host = Vst3HostRuntime::new(48_000.0, 512)?;
    let plugin = host.load(bundle_path, class_uid.as_deref())?;
    let output_extension = output_path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let output_format = match output_extension.as_str() {
        "wav" => AudioRenderOutput::Wav,
        "flac" => AudioRenderOutput::Flac {
            bits_per_sample: 24,
        },
        "ogg" => AudioRenderOutput::Ogg { bitrate_kbps: 192 },
        "mp3" => AudioRenderOutput::Mp3 { bitrate_kbps: 192 },
        _ => return Err("VST3 pattern output must use .wav, .flac, .ogg, or .mp3".to_owned()),
    };
    let summary = host.render_pattern_channel_to_file(
        plugin.id,
        &pattern.notes,
        Vst3PatternRenderOptions {
            channel_id,
            ppq: document.header().ppq(),
            tempo_bpm: document.metadata().tempo_bpm().unwrap_or(120.0),
            tail_seconds,
            global_swing_mix_raw: document.metadata().global_swing_mix(),
            channel_swing_mix_raw,
            pitch_offset_cents,
            note_pitch_offset_semitones,
        },
        output_path,
        AudioFileRenderOptions {
            output_format,
            encoding: AudioRenderEncodingOptions::default(),
        },
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

fn render_audio_vst3(
    input_path: &Path,
    bundle_path: &Path,
    output_path: &Path,
    class_uid: Option<&str>,
    options: Vst3AudioEffectRenderOptions,
) -> Result<(), String> {
    let audio = decode_audio_file(input_path)?;
    let output_extension = output_path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let output_format = match output_extension.as_str() {
        "wav" => AudioRenderOutput::Wav,
        "flac" => AudioRenderOutput::Flac {
            bits_per_sample: 24,
        },
        "ogg" => AudioRenderOutput::Ogg { bitrate_kbps: 192 },
        "mp3" => AudioRenderOutput::Mp3 { bitrate_kbps: 192 },
        _ => return Err("VST3 effect output must use .wav, .flac, .ogg, or .mp3".to_owned()),
    };
    let mut host = Vst3HostRuntime::new(f64::from(audio.sample_rate), 512)?;
    host.set_manual_latency_offsets(&load_vst3_manual_latency_offsets());
    let effect = host.load_audio_effect(bundle_path, class_uid)?;
    let summary = host.render_audio_through_effect_to_file(
        effect.id,
        &audio,
        options,
        output_path,
        AudioFileRenderOptions {
            output_format,
            encoding: AudioRenderEncodingOptions::default(),
        },
    )?;
    println!(
        "rendered {} input frames through {} at {} Hz to {} ({} output frames, {} latency frames compensated, {} tail frames)",
        summary.input_frames,
        summary.plugin_name,
        summary.sample_rate,
        output_path.display(),
        summary.output_frames,
        summary.latency_compensated_frames,
        summary.tail_frames
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
    let channels = document.channels();
    println!("channel records: {}", channels.len());
    for channel in channels {
        println!(
            "channel id={} type={:?} name={:?} sample_path={:?}",
            channel.id(),
            channel.channel_type(),
            channel.display_name(),
            channel.sample_path()
        );
        if let Some(envelopes) = channel.sampler_envelope_lfos() {
            for envelope in envelopes.into_iter().flatten() {
                println!("  envelope/LFO raw: {}", envelope.raw_summary());
            }
        }
    }
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
    match document.metadata().master_pitch_cents() {
        Some(cents) => println!("master pitch: {cents} cents"),
        None => println!("master pitch: not uniquely stored (0 cents when the event is absent)"),
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
    file_format: MidiExportFileFormat,
) -> Result<(), String> {
    let (_, document) = load_document(project)?;
    let bytes = MidiFile::encode_project_pattern_with_format(
        &document,
        pattern_id,
        channel_mapping,
        file_format,
    )
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

fn export_midi_controller_stream(
    project: &Path,
    output: &Path,
    pattern_id: u16,
    raw_stream: (u8, u8),
    midi_controller: u8,
    midi_channel: u8,
    file_format: MidiExportFileFormat,
) -> Result<(), String> {
    let (_, document) = load_document(project)?;
    let bytes = MidiFile::encode_project_pattern_controller_stream_with_format(
        &document,
        pattern_id,
        raw_stream,
        midi_controller,
        midi_channel,
        file_format,
    )
    .map_err(|error| error.to_string())?;
    fs::write(output, &bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "exported Pattern {pattern_id} raw controller stream {} / 0x{:02X} as MIDI CC {midi_controller} on channel {} to {} ({} bytes)",
        raw_stream.0,
        raw_stream.1,
        midi_channel + 1,
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
    file_format: MidiExportFileFormat,
) -> Result<(), String> {
    let (_, document) = load_document(project)?;
    let bytes = MidiFile::encode_project_song_with_format(
        &document,
        arrangement_id,
        channel_mapping,
        file_format,
    )
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

fn export_midi_song_controller_stream(
    project: &Path,
    output: &Path,
    arrangement_id: u16,
    raw_stream: (u8, u8),
    midi_controller: u8,
    midi_channel: u8,
    file_format: MidiExportFileFormat,
) -> Result<(), String> {
    let (_, document) = load_document(project)?;
    let bytes = MidiFile::encode_project_song_controller_stream_with_format(
        &document,
        arrangement_id,
        raw_stream,
        midi_controller,
        midi_channel,
        file_format,
    )
    .map_err(|error| error.to_string())?;
    fs::write(output, &bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "exported arrangement {arrangement_id} raw controller stream {} / 0x{:02X} as MIDI CC {midi_controller} on channel {} to {} ({} bytes)",
        raw_stream.0,
        raw_stream.1,
        midi_channel + 1,
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
            "id={} kind={} type={:?} enabled={} zipped={} main_pitch={} add_to_key={} fine_tune_raw={:?} key_region={:?} stretch_settings={:?} sampler_fade_out_raw={:?} sampler_fade_in_raw={:?} sampler_filter_cutoff_raw={:?} sampler_filter_resonance_raw={:?} sampler_filter_type_raw={:?} sample_fade_stereo={} sample_swap_stereo={} sample_remove_dc={} sample_normalize={} sample_reverse_polarity={} sample_start_fraction={:?} sample_length_fraction={:?} sample_trim_threshold_raw={:?} sample_crossfade_raw={:?} sampler_portamento={:?} sampler_polyphony_slide_raw={:?} sampler_volume_envelope={:?} sampler_envelopes={:?} color={:?} volume={:?} pan={:?} automation_range_raw={:?} level_adjusts_raw={:?} midi_out_settings={:?} swing_mix_raw={:?} layer_children={:?} layer_flags={:?} plugin={} name={} sample_path={:?} events={:?}",
            channel.id(),
            channel
                .kind()
                .map_or_else(|| "unknown".to_owned(), |kind| kind.to_string()),
            channel.channel_type(),
            channel
                .enabled()
                .map_or_else(|| "unknown".to_owned(), |enabled| enabled.to_string()),
            channel.zipped(),
            channel.keyboard_main_pitch_enabled(),
            channel.keyboard_add_to_key_enabled(),
            channel.keyboard_fine_tune_raw(),
            channel.keyboard_key_region(),
            channel.stretch_settings(),
            channel.sampler_fade_out_raw(),
            channel.sampler_fade_in_raw(),
            channel.sampler_filter_cutoff_raw(),
            channel.sampler_filter_resonance_raw(),
            channel.sampler_filter_type_raw(),
            channel.sample_fade_stereo_enabled(),
            channel.sample_swap_stereo_enabled(),
            channel.sample_remove_dc_enabled(),
            channel.sample_normalize_enabled(),
            channel.sample_reverse_polarity_enabled(),
            channel.sample_start_fraction(),
            channel.sample_length_fraction(),
            channel.sample_trim_threshold_raw(),
            channel.sample_crossfade_raw(),
            channel.sampler_portamento_enabled(),
            channel.sampler_polyphony_slide_raw(),
            channel.sampler_volume_envelope(),
            channel.sampler_envelope_lfos(),
            channel.color(),
            channel.volume(),
            channel.pan(),
            channel.automation_range_raw(),
            channel.level_adjustments(),
            channel.midi_out_settings(),
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

fn list_channel_groups(path: &Path) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let groups = document.channel_groups();
    let channels = document.channels();
    println!("channel groups: {}", groups.len());
    for group in &groups {
        let channel_ids = channels
            .iter()
            .filter(|channel| channel.group_number() == Some(group.index()))
            .map(|channel| channel.id().to_string())
            .collect::<Vec<_>>();
        println!(
            "index={} name={:?} channel_ids={}",
            group.index(),
            group.name().unwrap_or("(undecoded)"),
            if channel_ids.is_empty() {
                "-".to_owned()
            } else {
                channel_ids.join(",")
            }
        );
    }
    let unassigned = channels
        .iter()
        .filter(|channel| channel.group_number().is_none())
        .map(|channel| channel.id().to_string())
        .collect::<Vec<_>>();
    println!(
        "unassigned_channel_ids={}",
        if unassigned.is_empty() {
            "-".to_owned()
        } else {
            unassigned.join(",")
        }
    );
    let unresolved = channels
        .iter()
        .filter_map(|channel| {
            let index = channel.group_number()?;
            (!groups.iter().any(|group| group.index() == index))
                .then(|| format!("{}:{index}", channel.id()))
        })
        .collect::<Vec<_>>();
    if !unresolved.is_empty() {
        println!(
            "unresolved_channel_group_assignments={}",
            unresolved.join(",")
        );
    }
    Ok(())
}

fn edit_channel_groups(
    input: &Path,
    output: &Path,
    edit: impl FnOnce(&mut FlpDocument) -> Result<String, String>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let description = edit(&mut document)?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!("{description} in {}", output.display());
    Ok(())
}

fn create_channel_group(input: &Path, output: &Path, name: &str) -> Result<(), String> {
    edit_channel_groups(input, output, |document| {
        let index = document
            .add_channel_group(name)
            .map_err(|error| error.to_string())?;
        Ok(format!("created Channel Rack group {index} named {name:?}"))
    })
}

fn group_channels(
    input: &Path,
    output: &Path,
    channel_ids: &[u16],
    name: &str,
) -> Result<(), String> {
    edit_channel_groups(input, output, |document| {
        let index = document
            .group_channels(channel_ids, name)
            .map_err(|error| error.to_string())?;
        Ok(format!(
            "assigned channel IDs {channel_ids:?} to Channel Rack group {index} named {name:?}"
        ))
    })
}

fn rename_channel_group(
    input: &Path,
    output: &Path,
    group_index: i32,
    name: &str,
) -> Result<(), String> {
    edit_channel_groups(input, output, |document| {
        document
            .rename_channel_group(group_index, name)
            .map_err(|error| error.to_string())?;
        Ok(format!(
            "renamed Channel Rack group {group_index} to {name:?}"
        ))
    })
}

fn delete_channel_group(input: &Path, output: &Path, group_index: i32) -> Result<(), String> {
    edit_channel_groups(input, output, |document| {
        document
            .delete_channel_group(group_index)
            .map_err(|error| error.to_string())?;
        Ok(format!("deleted Channel Rack group {group_index}"))
    })
}

fn list_mixer(path: &Path) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let inserts = document.mixer_inserts();
    let effect_slots = document.mixer_effect_slots();
    let parameters = document
        .mixer_parameter_records()
        .map_err(|error| format!("could not decode Mixer parameter records: {error}"))?;
    let sends = document
        .mixer_send_summaries()
        .map_err(|error| format!("could not decode Mixer sends: {error}"))?;
    println!(
        "automatic PDC: {:?}",
        document.mixer_automatic_pdc_enabled()
    );
    println!("recognized insert records: {}", inserts.len());
    for insert in inserts {
        println!(
            "ordinal={} name={} enabled={:?} soloed={:?} polarity_reversed={:?} swap_left_right={:?} effects_enabled={:?} mute_locked={:?} flags={:?} input_raw={} output_raw={} color_raw=0x{:08X} icon_raw={:?} events={:?}",
            insert.ordinal(),
            insert.name().unwrap_or("(unnamed)"),
            insert.enabled(),
            insert.soloed(),
            insert.polarity_reversed(),
            insert.swap_left_right(),
            insert.effects_enabled(),
            insert.mute_locked(),
            insert.flags(),
            insert.input_raw(),
            insert.output_raw(),
            insert.color_raw(),
            insert.icon_raw(),
            insert.event_range()
        );
    }
    let eq_parameters = document
        .mixer_eq_parameter_summaries()
        .map_err(|error| format!("could not decode Mixer EQ parameters: {error}"))?;
    println!("recognized Mixer EQ values: {}", eq_parameters.len());
    for parameter in eq_parameters {
        println!(
            "insert={} kind={:?} raw={} event={} record={}",
            parameter.insert_ordinal(),
            parameter.kind(),
            parameter.value(),
            parameter.parameter_location().0,
            parameter.parameter_location().1,
        );
    }
    println!("recognized effect slot records: {}", effect_slots.len());
    for slot in effect_slots {
        let vst3_metadata = slot
            .vst_metadata()
            .filter(|metadata| metadata.format_marker() == 12 && metadata.fourcc().is_none());
        let vst_class_uid = vst3_metadata
            .and_then(|metadata| metadata.class_uid())
            .unwrap_or_else(|| "-".to_owned());
        let vst_state_bytes = vst3_metadata.and_then(|_| slot.vst_state_bytes().map(<[u8]>::len));
        let color_raw = slot
            .color_raw()
            .map_or_else(|| "none".to_owned(), |raw| format!("0x{raw:08X}"));
        println!(
            "insert={} slot={} plugin={} name={} state_events={} vst3_class_uid={} vst_field_53_bytes={:?} color_raw={} enabled={:?} mix_raw={:?}",
            slot.insert_ordinal(),
            slot.slot_index(),
            slot.plugin_identifier().unwrap_or("(unknown)"),
            slot.display_name().unwrap_or("(unnamed)"),
            slot.state_event_count(),
            vst_class_uid,
            vst_state_bytes,
            color_raw,
            slot.enabled(),
            slot.mix_raw(),
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
    for record in &parameters {
        println!(
            "0xE1 event={} record={} id=0x{:02X} kind={:?} target={} slot={} scope_raw={} value={}",
            record.event_index(),
            record.record_index(),
            record.parameter_id(),
            record.kind(),
            record.target_index(),
            record.slot_index(),
            record.target_scope_raw(),
            record.value()
        );
    }
    println!("Mixer sends: {}", sends.len());
    for send in sends {
        let (parameter_event, parameter_record) = send.parameter_location();
        let routing_flag = send
            .routing_flag_location()
            .map(|(event, byte)| format!("{event}:{byte}"))
            .unwrap_or_else(|| "-".to_owned());
        println!(
            "source_insert={} route_id=0x{:02X} parameter_event={} parameter_record={} value={} enabled={:?} routing_flag_event_byte={}",
            send.insert_ordinal(),
            send.route_parameter_id(),
            parameter_event,
            parameter_record,
            send.value(),
            send.enabled(),
            routing_flag
        );
    }
    Ok(())
}

fn set_mixer_automatic_pdc(input: &Path, output: &Path, enabled: bool) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_mixer_automatic_pdc_enabled(enabled)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Mixer Automatic PDC to {} in {}",
        u8::from(enabled),
        output.display()
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
            "channel={} name={} lfo_amount={:?} points={} event={:?}",
            channel.channel_id(),
            channel.display_name().unwrap_or("(unnamed)"),
            channel.lfo_amount(),
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

fn pattern_color_label(color: Option<[u8; 4]>) -> String {
    color.map_or_else(
        || "default".to_owned(),
        |[red, green, blue, _]| format!("#{red:02X}{green:02X}{blue:02X}"),
    )
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
            "id={} name={} color={} length_ticks={} notes={} controllers={} channels=[{}] time_signatures=[{}]",
            pattern.id,
            pattern.name.as_deref().unwrap_or("(unnamed)"),
            pattern_color_label(pattern.color),
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

fn create_pattern_controller_stream(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel: u8,
    flags: u8,
    position: u32,
    value: f32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let controller_index = document
        .create_pattern_controller_stream(pattern_id, channel, flags, position, value)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "created raw controller point {controller_index} in pattern {pattern_id}, channel {channel}, flags 0x{flags:02X} at {position} ticks in {}",
        output.display()
    );
    Ok(())
}

fn playlist_pattern_color_label(
    target: &flp_rebuild::PlaylistClipTarget,
    patterns: Option<&[flp_rebuild::Pattern]>,
) -> String {
    match target {
        flp_rebuild::PlaylistClipTarget::Pattern { id } => {
            let Some(patterns) = patterns else {
                return "unavailable".to_owned();
            };
            patterns
                .iter()
                .find(|pattern| pattern.id == *id)
                .map_or_else(
                    || "unknown".to_owned(),
                    |pattern| pattern_color_label(pattern.color),
                )
        }
        flp_rebuild::PlaylistClipTarget::Channel { .. } => "none".to_owned(),
    }
}

fn list_playlist(path: &Path, start: usize, count: usize) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let playlist_tracks = document.playlist_tracks();
    let channels = document.channels();
    let patterns = document.patterns().ok();
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
            let pattern_color = playlist_pattern_color_label(&target, patterns.as_deref());
            println!(
                "  clip={} position={} length={} track={} track_name={:?} raw_track={} target={:?} target_name={:?} pattern_color={} group={} muted={} flags=0x{:04X} offsets={:.6}..{:.6} scale={:?} clip_id={} record_size={} opaque_tail={}",
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
                pattern_color,
                clip.group,
                clip.is_muted(),
                clip.item_flags,
                clip.start_offset,
                clip.end_offset,
                clip.scale,
                clip.clip_id
                    .map_or_else(|| "none".to_owned(), |id| id.to_string()),
                clip.record_size,
                clip.has_opaque_record_tail(),
            );
        }
    }
    Ok(())
}

fn diff_playlist_clip_records(
    before_path: &Path,
    after_path: &Path,
    arrangement_id: u16,
    clip_index: usize,
) -> Result<(), String> {
    let (_, before_document) = load_document(before_path)?;
    let (_, after_document) = load_document(after_path)?;
    let before_record = before_document
        .playlist_clip_record_bytes(arrangement_id, clip_index)
        .map_err(|error| format!("could not read the before clip record: {error}"))?;
    let after_record = after_document
        .playlist_clip_record_bytes(arrangement_id, clip_index)
        .map_err(|error| format!("could not read the after clip record: {error}"))?;

    println!(
        "arrangement={arrangement_id} clip={clip_index} before_bytes={} after_bytes={}",
        before_record.len(),
        after_record.len()
    );
    let mut changed_bytes = 0usize;
    for offset in 0..before_record.len().max(after_record.len()) {
        let before = before_record.get(offset).copied();
        let after = after_record.get(offset).copied();
        if before == after {
            continue;
        }
        changed_bytes += 1;
        let before = before.map_or_else(|| "--".to_owned(), |value| format!("{value:02X}"));
        let after = after.map_or_else(|| "--".to_owned(), |value| format!("{value:02X}"));
        println!("  offset={offset:02} (0x{offset:02X}) {before} -> {after}");
    }
    if changed_bytes == 0 {
        println!("no raw record differences");
    } else {
        println!("changed bytes: {changed_bytes}");
    }
    Ok(())
}

fn list_automation_target_links(path: &Path) -> Result<(), String> {
    let (_, document) = load_document(path)?;
    let channels = document.channels();
    let mixer_inserts = document.mixer_inserts();
    let mixer_effect_slots = document.mixer_effect_slots();
    let links = document
        .automation_target_links()
        .map_err(|error| format!("could not decode candidate automation target links: {error}"))?;
    println!("candidate 0xE3 records: {}", links.len());
    for link in links {
        let source_id = link.source_channel_id();
        let mut source_matches = channels.iter().filter(|channel| channel.id() == source_id);
        let source_channel = match (source_matches.next(), source_matches.next()) {
            (Some(channel), None) => Some(channel),
            _ => None,
        };
        let source_name = source_channel.and_then(|channel| channel.display_name());
        let source_type = link.source_channel_type();
        let source_range = source_channel.and_then(|channel| channel.automation_range_raw());
        let source_class = match source_type {
            Some(ChannelType::Automation) => "automation",
            Some(_) => "other-channel",
            None => "unresolved",
        };
        // Project-level 0xE3 records also describe non-Automation remote-control mappings.
        // Keep those records visible as raw data, but only interpret their parameter and
        // destination fields as Automation links when the source resolves to a type-5 channel.
        let automation_source = source_class == "automation";
        let channel_target_id = automation_source
            .then(|| link.candidate_channel_target_id())
            .flatten();
        let mixer_target = automation_source
            .then(|| link.candidate_mixer_effect_target())
            .flatten();
        let target = if let Some(channel_id) = channel_target_id {
            format!("channel:{channel_id}")
        } else if let Some((insert, slot)) = mixer_target {
            format!("mixer-insert:{insert}/slot:{slot}")
        } else if automation_source {
            "unclassified".to_owned()
        } else {
            "not-interpreted".to_owned()
        };
        let target_channel = channel_target_id
            .and_then(|channel_id| channels.iter().find(|channel| channel.id() == channel_id));
        let target_name = target_channel.and_then(|channel| channel.display_name());
        let target_type = target_channel.and_then(|channel| channel.channel_type());
        let target_insert_name = mixer_target
            .and_then(|(insert, _)| {
                mixer_inserts
                    .iter()
                    .find(|item| item.ordinal() == usize::from(insert))
            })
            .and_then(|insert| insert.name());
        let target_effect = mixer_target.and_then(|(insert, slot)| {
            mixer_effect_slots.iter().find(|item| {
                item.insert_ordinal() == usize::from(insert) && item.slot_index() == slot
            })
        });
        let target_effect_name =
            target_effect.and_then(|effect| effect.display_name().or(effect.plugin_identifier()));
        let candidate_parameter = link
            .candidate_parameter_index()
            .map_or_else(|| "not-interpreted".to_owned(), |value| value.to_string());
        let candidate_vst_parameter = link
            .candidate_vst_parameter()
            .map_or_else(|| "not-interpreted".to_owned(), |value| value.to_string());
        println!(
            "event={} record={} source_channel={} source_name={:?} source_class={} source_type={:?} source_automation_range_raw={:?} candidate_parameter={} parameter_raw=0x{:04X} candidate_vst_parameter={} destination_raw={} candidate_target={} candidate_target_name={:?} candidate_target_type={:?} candidate_target_insert_name={:?} candidate_target_effect_name={:?} prefix={:02X?} suffix={:02X?}",
            link.event_index(),
            link.record_index(),
            source_id,
            source_name,
            source_class,
            source_type,
            source_range,
            candidate_parameter,
            link.parameter_raw(),
            candidate_vst_parameter,
            link.destination_raw(),
            target,
            target_name,
            target_type,
            target_insert_name,
            target_effect_name,
            link.opaque_prefix(),
            link.opaque_suffix()
        );
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
                "{:06} channel={} channel_note_index={} position={} length={} key={} velocity={} pan={} release={} flags=0x{:04X} group={} fine_pitch={} midi_channel={} porta={} midi_channel_raw=0x{:02X} mod_x={} mod_y={}",
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
                note.midi_channel_index(),
                note.is_porta_note(),
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

fn duplicate_channel(
    input: &Path,
    output: &Path,
    channel_id: u16,
    name: &str,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let new_channel_id = document
        .duplicate_channel(channel_id, name)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "duplicated channel {channel_id} as channel {new_channel_id} named {name:?} in {}",
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

fn create_sample_channel(
    input: &Path,
    output: &Path,
    sample_path: &str,
    name: &str,
    audio_clip: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let channel_id = if audio_clip {
        document.create_audio_channel(sample_path, name)
    } else {
        document.create_sampler_channel(sample_path, name)
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    let channel_type = if audio_clip { "Audio Clip" } else { "Sampler" };
    println!(
        "created {channel_type} channel {channel_id} named {name:?} with sample {sample_path:?} in {}",
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

fn set_channel_colors(
    input: &Path,
    output: &Path,
    channel_ids: &[u16],
    rgb: [u8; 3],
) -> Result<(), String> {
    edit_channel_colors(
        input,
        output,
        format!(
            "set color #{:02X}{:02X}{:02X} on channels {channel_ids:?}",
            rgb[0], rgb[1], rgb[2]
        ),
        |document| {
            document
                .set_channel_colors(channel_ids, rgb)
                .map_err(|error| error.to_string())
        },
    )
}

fn set_channel_color_gradient(
    input: &Path,
    output: &Path,
    channel_ids: &[u16],
    start_rgb: [u8; 3],
    end_rgb: [u8; 3],
) -> Result<(), String> {
    edit_channel_colors(
        input,
        output,
        format!(
            "applied color gradient #{:02X}{:02X}{:02X}..#{:02X}{:02X}{:02X} to channels {channel_ids:?}",
            start_rgb[0], start_rgb[1], start_rgb[2], end_rgb[0], end_rgb[1], end_rgb[2]
        ),
        |document| {
            document
                .set_channel_color_gradient(channel_ids, start_rgb, end_rgb)
                .map_err(|error| error.to_string())
        },
    )
}

fn edit_channel_colors(
    input: &Path,
    output: &Path,
    description: String,
    edit: impl FnOnce(&mut FlpDocument) -> Result<(), String>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    edit(&mut document)?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!("{description} in {}", output.display());
    Ok(())
}

fn move_channel(
    input: &Path,
    output: &Path,
    channel_id: u16,
    target_index: usize,
) -> Result<(), String> {
    edit_channel_order(
        input,
        output,
        format!("moved channel {channel_id} to Channel Rack index {target_index}"),
        |document| {
            document
                .move_channel(channel_id, target_index)
                .map_err(|error| error.to_string())
        },
    )
}

fn reorder_channels(input: &Path, output: &Path, channel_ids: &[u16]) -> Result<(), String> {
    edit_channel_order(
        input,
        output,
        format!("reordered Channel Rack channels as {channel_ids:?}"),
        |document| {
            document
                .reorder_channels(channel_ids)
                .map_err(|error| error.to_string())
        },
    )
}

fn sort_channels(input: &Path, output: &Path, order: ChannelSortOrder) -> Result<(), String> {
    edit_channel_order(
        input,
        output,
        format!("sorted Channel Rack channels by {order:?}"),
        |document| {
            document
                .sort_channels(order)
                .map_err(|error| error.to_string())
        },
    )
}

fn edit_channel_order(
    input: &Path,
    output: &Path,
    description: String,
    edit: impl FnOnce(&mut FlpDocument) -> Result<(), String>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    edit(&mut document)?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!("{description} in {}", output.display());
    Ok(())
}

fn set_mixer_parameter_record_value(
    input: &Path,
    output: &Path,
    event_index: usize,
    record_index: usize,
    value: i32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_mixer_parameter_record_value(event_index, record_index, value)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Mixer 0xE1 event {event_index} record {record_index} value to {value} in {}",
        output.display()
    );
    Ok(())
}

fn parse_mixer_eq_parameter_kind(value: &str) -> Result<MixerParameterKind, String> {
    match value {
        "low-gain" => Ok(MixerParameterKind::LowEqGain),
        "mid-gain" => Ok(MixerParameterKind::MidEqGain),
        "high-gain" => Ok(MixerParameterKind::HighEqGain),
        "low-frequency" => Ok(MixerParameterKind::LowEqFrequency),
        "mid-frequency" => Ok(MixerParameterKind::MidEqFrequency),
        "high-frequency" => Ok(MixerParameterKind::HighEqFrequency),
        "low-q" => Ok(MixerParameterKind::LowEqQ),
        "mid-q" => Ok(MixerParameterKind::MidEqQ),
        "high-q" => Ok(MixerParameterKind::HighEqQ),
        _ => Err(format!(
            "unknown Mixer EQ parameter {value:?}; use low-gain, mid-gain, high-gain, low-frequency, mid-frequency, high-frequency, low-q, mid-q, or high-q"
        )),
    }
}

fn set_mixer_eq_parameter_raw(
    input: &Path,
    output: &Path,
    insert_ordinal: usize,
    kind: MixerParameterKind,
    value: i32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_mixer_eq_parameter_raw(insert_ordinal, kind, value)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Mixer insert {insert_ordinal} {kind:?} raw value to {value} in {}",
        output.display()
    );
    Ok(())
}

fn set_mixer_effect_slot_enabled(
    input: &Path,
    output: &Path,
    insert_ordinal: usize,
    slot_index: u16,
    enabled: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_mixer_effect_slot_enabled(insert_ordinal, slot_index, enabled)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Mixer insert {insert_ordinal} effect slot {slot_index} enabled to {enabled} in {}",
        output.display()
    );
    Ok(())
}

fn set_mixer_effect_slot_color(
    input: &Path,
    output: &Path,
    insert_ordinal: usize,
    slot_index: u16,
    color_raw: u32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_mixer_effect_slot_color(insert_ordinal, slot_index, color_raw)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Mixer insert {insert_ordinal} effect slot {slot_index} color to #{:02X}{:02X}{:02X} in {}",
        (color_raw & 0xFF) as u8,
        ((color_raw >> 8) & 0xFF) as u8,
        ((color_raw >> 16) & 0xFF) as u8,
        output.display()
    );
    Ok(())
}

fn set_mixer_effect_slot_mix_raw(
    input: &Path,
    output: &Path,
    insert_ordinal: usize,
    slot_index: u16,
    mix_raw: i32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_mixer_effect_slot_mix_raw(insert_ordinal, slot_index, mix_raw)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Mixer insert {insert_ordinal} effect slot {slot_index} Mix raw value to {mix_raw} in {}",
        output.display()
    );
    Ok(())
}

fn move_mixer_effect_slot(
    input: &Path,
    output: &Path,
    insert_ordinal: usize,
    from_slot_index: u16,
    to_slot_index: u16,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .move_mixer_effect_slot(insert_ordinal, from_slot_index, to_slot_index)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "moved Mixer insert {insert_ordinal} effect slot {from_slot_index} to slot {to_slot_index} in {}",
        output.display()
    );
    Ok(())
}

fn rename_mixer_insert(
    input: &Path,
    output: &Path,
    insert_ordinal: usize,
    name: &str,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_mixer_insert_name(insert_ordinal, name)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "renamed Mixer insert {insert_ordinal} to {name:?} in {}",
        output.display()
    );
    Ok(())
}

fn set_mixer_insert_flag(
    input: &Path,
    output: &Path,
    insert_ordinal: usize,
    field: &str,
    enabled: bool,
) -> Result<(), String> {
    let edit = match field {
        "enabled" => MixerInsertEdit {
            enabled: Some(enabled),
            ..MixerInsertEdit::default()
        },
        "soloed" => MixerInsertEdit {
            soloed: Some(enabled),
            ..MixerInsertEdit::default()
        },
        "polarity-reversed" => MixerInsertEdit {
            polarity_reversed: Some(enabled),
            ..MixerInsertEdit::default()
        },
        "swap-left-right" => MixerInsertEdit {
            swap_left_right: Some(enabled),
            ..MixerInsertEdit::default()
        },
        "effects-enabled" => MixerInsertEdit {
            effects_enabled: Some(enabled),
            ..MixerInsertEdit::default()
        },
        "mute-locked" => MixerInsertEdit {
            mute_locked: Some(enabled),
            ..MixerInsertEdit::default()
        },
        _ => {
            return Err(format!(
                "unknown Mixer insert flag {field:?}; expected enabled, soloed, polarity-reversed, swap-left-right, effects-enabled, or mute-locked"
            ));
        }
    };
    let (_, mut document) = load_document(input)?;
    document
        .edit_mixer_insert_flags(insert_ordinal, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Mixer insert {insert_ordinal} flag {field} to {} in {}",
        u8::from(enabled),
        output.display()
    );
    Ok(())
}

fn set_mixer_routing_flag(
    input: &Path,
    output: &Path,
    event_index: usize,
    routing_flag_index: usize,
    enabled: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_mixer_routing_flag(event_index, routing_flag_index, enabled)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Mixer routing event {event_index} flag byte {routing_flag_index} to {} in {}",
        u8::from(enabled),
        output.display()
    );
    Ok(())
}

fn set_channel_sample_path(
    input: &Path,
    output: &Path,
    channel_id: u16,
    sample_path: &str,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_channel_sample_path(channel_id, sample_path)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set channel {channel_id} sample path to {sample_path:?} in {}",
        output.display()
    );
    Ok(())
}

fn set_channel_sample_reversed(
    input: &Path,
    output: &Path,
    channel_id: u16,
    reversed: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_channel_sample_reversed(channel_id, reversed)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "{} sample source on channel {channel_id} in {}",
        if reversed { "reversed" } else { "restored" },
        output.display()
    );
    Ok(())
}

fn edit_channel_sample_effects(
    input: &Path,
    output: &Path,
    channel_id: u16,
    edit: ChannelSampleEffectsEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_channel_sample_effects(channel_id, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "edited sample effect flags on channel {channel_id} in {}",
        output.display()
    );
    Ok(())
}

fn edit_channel_sample_fx_flags(
    input: &Path,
    output: &Path,
    channel_id: u16,
    edit: ChannelSampleFxFlagsEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_channel_sample_fx_flags(channel_id, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "edited sample FX flags on channel {channel_id} in {}",
        output.display()
    );
    Ok(())
}

fn edit_channel_sample_window(
    input: &Path,
    output: &Path,
    channel_id: u16,
    edit: ChannelSampleWindowEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_channel_sample_window(channel_id, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "edited sample window on channel {channel_id} in {}",
        output.display()
    );
    Ok(())
}

fn edit_channel_sample_trim(
    input: &Path,
    output: &Path,
    channel_id: u16,
    edit: ChannelSampleTrimEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_channel_sample_trim_threshold(channel_id, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set sample Trim threshold on channel {channel_id} in {}",
        output.display()
    );
    Ok(())
}

fn edit_channel_sample_crossfade(
    input: &Path,
    output: &Path,
    channel_id: u16,
    edit: ChannelSampleCrossfadeEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_channel_sample_crossfade(channel_id, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set sample Crossfade amount on channel {channel_id} in {}",
        output.display()
    );
    Ok(())
}

fn edit_channel_sampler_filter(
    input: &Path,
    output: &Path,
    channel_id: u16,
    edit: ChannelSamplerFilterEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_channel_sampler_filter(channel_id, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "updated Sampler filter settings on channel {channel_id} in {}",
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

fn edit_channel_level_adjustments(
    input: &Path,
    output: &Path,
    channel_id: u16,
    event_index: usize,
    edit: ChannelLevelAdjustmentsEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_channel_level_adjustments(channel_id, event_index, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "edited channel {channel_id} Level Adjusts event {event_index} in {}",
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

fn set_channel_main_pitch(
    input: &Path,
    output: &Path,
    channel_id: u16,
    enabled: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_channel_main_pitch_enabled(channel_id, enabled)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "{} main pitch on channel {channel_id} in {}",
        if enabled { "enabled" } else { "disabled" },
        output.display()
    );
    Ok(())
}

fn set_channel_key_region(
    input: &Path,
    output: &Path,
    channel_id: u16,
    low_key: u8,
    high_key: u8,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_channel_key_region(channel_id, low_key, high_key)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set channel {channel_id} playable key range to {low_key}–{high_key} in {}",
        output.display()
    );
    Ok(())
}

fn set_channel_enabled(
    input: &Path,
    output: &Path,
    channel_id: u16,
    enabled: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_channel_enabled(channel_id, enabled)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "{} channel {channel_id} in {}",
        if enabled { "enabled" } else { "disabled" },
        output.display()
    );
    Ok(())
}

fn set_channel_zipped(
    input: &Path,
    output: &Path,
    channel_id: u16,
    zipped: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_channel_zipped(channel_id, zipped)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "{} channel {channel_id} in {}",
        if zipped { "zipped" } else { "unzipped" },
        output.display()
    );
    Ok(())
}

fn set_channel_add_to_key(
    input: &Path,
    output: &Path,
    channel_id: u16,
    enabled: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_channel_add_to_key_enabled(channel_id, enabled)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "{} Add to Key on channel {channel_id} in {}",
        if enabled { "enabled" } else { "disabled" },
        output.display()
    );
    Ok(())
}

fn set_channel_mixer_track(
    input: &Path,
    output: &Path,
    channel_id: u16,
    mixer_track: i8,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_channel_mixer_track(channel_id, mixer_track)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "routed channel {channel_id} to Mixer track {mixer_track} in {}",
        output.display()
    );
    Ok(())
}

fn set_channel_midi_out_settings(
    input: &Path,
    output: &Path,
    channel_id: u16,
    edit: ChannelMidiOutSettingsEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_channel_midi_out_settings(channel_id, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "edited candidate MIDI Out settings for channel {channel_id} in {}",
        output.display()
    );
    Ok(())
}

fn set_vst3_output_route(
    input: &Path,
    output: &Path,
    channel_id: u16,
    output_bus_index: usize,
    mixer_track: i8,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_vst3_channel_output_mixer_track(channel_id, output_bus_index, mixer_track)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "routed channel {channel_id} VST3 output bus {output_bus_index} to Mixer track {mixer_track} in {}",
        output.display()
    );
    Ok(())
}

fn edit_channel_stretch_settings(
    input: &Path,
    output: &Path,
    channel_id: u16,
    edit: ChannelStretchSettingsEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_channel_stretch_settings(channel_id, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "edited channel {channel_id} time-stretch settings in {}",
        output.display()
    );
    Ok(())
}

fn set_sampler_quick_fades(
    input: &Path,
    output: &Path,
    channel_id: u16,
    fade_out_raw: u16,
    fade_in_raw: u16,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_sampler_quick_fades(channel_id, fade_out_raw, fade_in_raw)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Sampler channel {channel_id} precomputed Fade Out to {fade_out_raw} and Fade In to {fade_in_raw} in {}",
        output.display()
    );
    Ok(())
}

fn set_sampler_portamento(
    input: &Path,
    output: &Path,
    channel_id: u16,
    enabled: Option<bool>,
    slide_raw: Option<u32>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_sampler_portamento(channel_id, enabled, slide_raw)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "updated Sampler channel {channel_id} Porta={enabled:?} Slide={slide_raw:?} in {}",
        output.display()
    );
    Ok(())
}

fn edit_sampler_volume_envelope(
    input: &Path,
    output: &Path,
    channel_id: u16,
    edit: SamplerVolumeEnvelopeEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_sampler_volume_envelope(channel_id, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "edited Sampler channel {channel_id} Volume envelope in {}",
        output.display()
    );
    Ok(())
}

fn edit_sampler_envelope_lfo(
    input: &Path,
    output: &Path,
    channel_id: u16,
    target: SamplerEnvelopeLfoTarget,
    edit: SamplerEnvelopeLfoEdit,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_sampler_envelope_lfo(channel_id, target, edit)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "edited Sampler channel {channel_id} {} envelope/LFO in {}",
        target.label(),
        output.display()
    );
    Ok(())
}

fn set_channel_fine_tune(
    input: &Path,
    output: &Path,
    channel_id: u16,
    cents: i32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_channel_fine_tune_cents(channel_id, cents)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set channel {channel_id} Fine Tune to {cents} cents in {}",
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

fn set_layer_fade(
    input: &Path,
    output: &Path,
    layer_channel_id: u16,
    fade_raw: u16,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_layer_fade(layer_channel_id, fade_raw)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Layer channel {layer_channel_id} Fade to {fade_raw} in {}",
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

fn set_automation_point_trailing_bytes(
    input: &Path,
    output: &Path,
    channel_id: u16,
    point_index: usize,
    trailing_bytes: [u8; 4],
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_automation_point_trailing_bytes(channel_id, point_index, trailing_bytes)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set automation channel {channel_id} point {point_index} raw tail to {trailing_bytes:02X?} in {}",
        output.display()
    );
    Ok(())
}

fn set_automation_lfo_amount(
    input: &Path,
    output: &Path,
    channel_id: u16,
    amount: i32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_automation_lfo_amount(channel_id, amount)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set automation channel {channel_id} LFO amount to {amount} in {}",
        output.display()
    );
    Ok(())
}

fn set_automation_range_raw(
    input: &Path,
    output: &Path,
    channel_id: u16,
    minimum_raw: i32,
    maximum_raw: i32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_automation_range_raw(channel_id, minimum_raw, maximum_raw)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Automation channel {channel_id} raw Min/Max to {minimum_raw}/{maximum_raw} in {}",
        output.display()
    );
    Ok(())
}

fn set_e3_record_raw_fields(
    input: &Path,
    output: &Path,
    event_index: usize,
    record_index: usize,
    source_id_raw: u16,
    parameter_raw: u16,
    destination_raw: i16,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_e3_record_raw_fields(
            event_index,
            record_index,
            source_id_raw,
            parameter_raw,
            destination_raw,
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "updated raw 0xE3 record fields at event {event_index}, record {record_index} in {}",
        output.display()
    );
    Ok(())
}

fn copy_automation_state(
    input: &Path,
    output: &Path,
    source_channel_id: u16,
    target_channel_id: u16,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .copy_automation_state(source_channel_id, target_channel_id)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "copied Automation curve state from channel {source_channel_id} to channel {target_channel_id} in {}",
        output.display()
    );
    Ok(())
}

fn initialize_automation_points_from_template(
    input: &Path,
    output: &Path,
    channel_id: u16,
    template_channel_id: u16,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .create_automation_point_blob_from_template(channel_id, template_channel_id)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "initialized empty automation data in channel {channel_id} from template channel {template_channel_id} in {}",
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

fn interpolate_automation_points(
    input: &Path,
    output: &Path,
    channel_id: u16,
    first_point_index: usize,
    last_point_index: usize,
    mode: AutomationInterpolationMode,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let interpolated_count = document
        .interpolate_automation_points_with_mode(
            channel_id,
            first_point_index,
            last_point_index,
            mode,
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "interpolated {interpolated_count} automation points ({mode:?}) between indices {first_point_index} and {last_point_index} in channel {channel_id} in {}",
        output.display()
    );
    Ok(())
}

fn flip_automation_points(input: &Path, output: &Path, channel_id: u16) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let point_count = document
        .flip_automation_points(channel_id)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "flipped {point_count} automation point values in channel {channel_id} to {}",
        output.display()
    );
    Ok(())
}

fn normalize_automation_points(input: &Path, output: &Path, channel_id: u16) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let point_count = document
        .normalize_automation_points(channel_id)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "normalized {point_count} automation point values to 0..=1 in channel {channel_id} in {}",
        output.display()
    );
    Ok(())
}

fn scale_automation_levels(
    input: &Path,
    output: &Path,
    channel_id: u16,
    options: AutomationScaleLevelsOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let point_count = document
        .scale_automation_levels(channel_id, options)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "scaled {point_count} Automation levels in channel {channel_id} in {}",
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

fn draw_pattern_controller_points(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    stream: (u8, u8),
    points: &[(u32, f32)],
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let added = document
        .draw_pattern_controller_points(pattern_id, stream, points)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "drew {} controller points in pattern {pattern_id}, channel {}, flags {} (added {added}) to {}",
        points.len(),
        stream.0,
        stream.1,
        output.display()
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum PatternControllerRangeOperation {
    Linear,
    Spline,
    Delete,
}

fn edit_pattern_controller_range(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    stream: (u8, u8),
    start_tick: u32,
    end_tick: u32,
    operation: PatternControllerRangeOperation,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = match operation {
        PatternControllerRangeOperation::Linear => {
            document.linearize_pattern_controller_range(pattern_id, stream, start_tick, end_tick)
        }
        PatternControllerRangeOperation::Spline => {
            document.spline_pattern_controller_range(pattern_id, stream, start_tick, end_tick)
        }
        PatternControllerRangeOperation::Delete => {
            document.delete_pattern_controller_range(pattern_id, stream, start_tick, end_tick)
        }
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    let operation = match operation {
        PatternControllerRangeOperation::Linear => "linearized",
        PatternControllerRangeOperation::Spline => "spline-interpolated",
        PatternControllerRangeOperation::Delete => "deleted",
    };
    println!(
        "{operation} {changed} raw controller points in pattern {pattern_id}, channel {}, flags {}, ticks {}..={} to {}",
        stream.0,
        stream.1,
        start_tick.min(end_tick),
        start_tick.max(end_tick),
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
    quantize_notes_with_options(
        input,
        output,
        pattern_id,
        channel_id,
        QuantizeNoteOptions {
            grid_ticks,
            start_strength: strength,
            swing,
            ..QuantizeNoteOptions::default()
        },
    )
}

fn quantize_notes_with_options(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    options: QuantizeNoteOptions,
) -> Result<(), String> {
    quantize_notes_with_scope(input, output, pattern_id, channel_id, None, options)
}

fn quantize_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    options: QuantizeNoteOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = match note_indices {
        Some(note_indices) => document.quantize_pattern_note_selection_with_options(
            pattern_id,
            channel_id,
            note_indices,
            options,
        ),
        None => document.quantize_pattern_notes_with_options(pattern_id, channel_id, options),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "quantized {changed} notes in pattern {pattern_id}, channel {channel_id} ({}) to {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        output.display()
    );
    Ok(())
}

fn quantize_notes_with_score_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    groove_path: &Path,
    options: QuantizeNoteOptions,
) -> Result<(), String> {
    let score_bytes = fs::read(groove_path)
        .map_err(|error| format!("could not read {}: {error}", groove_path.display()))?;
    let score = FscScore::parse(&score_bytes)
        .map_err(|error| format!("could not parse {}: {error}", groove_path.display()))?;
    let (_, mut document) = load_document(input)?;
    let changed = match note_indices {
        Some(note_indices) => document.quantize_pattern_note_selection_with_score(
            pattern_id,
            channel_id,
            note_indices,
            options,
            &score,
        ),
        None => document.quantize_pattern_notes_with_score(pattern_id, channel_id, options, &score),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "quantized {changed} notes in pattern {pattern_id}, channel {channel_id} ({}) with groove {} to {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        groove_path.display(),
        output.display()
    );
    Ok(())
}

fn rotate_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    snap_ticks: u32,
    direction: PatternNoteRotation,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = match note_indices {
        Some(note_indices) => document.rotate_pattern_note_selection(
            pattern_id,
            channel_id,
            note_indices,
            snap_ticks,
            direction,
        ),
        None => document.rotate_pattern_notes(pattern_id, channel_id, snap_ticks, direction),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "rotated {changed} notes in pattern {pattern_id}, channel {channel_id} ({}) {snap_ticks} ticks {} in {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        match direction {
            PatternNoteRotation::Left => "left",
            PatternNoteRotation::Right => "right",
        },
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
    legato_notes_with_scope(input, output, pattern_id, channel_id, None)
}

fn legato_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = match note_indices {
        Some(note_indices) => {
            document.legato_pattern_note_selection(pattern_id, channel_id, note_indices)
        }
        None => document.legato_pattern_notes(pattern_id, channel_id),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "extended {changed} notes in pattern {pattern_id}, channel {channel_id} ({}) to {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
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
    chop_notes_with_scope(input, output, pattern_id, channel_id, None, divisions)
}

fn chop_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    divisions: u8,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let created = match note_indices {
        Some(note_indices) => {
            document.chop_pattern_note_selection(pattern_id, channel_id, note_indices, divisions)
        }
        None => document.chop_pattern_notes(pattern_id, channel_id, divisions),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "created {created} chopped notes in pattern {pattern_id}, channel {channel_id} ({}) using {divisions} divisions in {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        output.display()
    );
    Ok(())
}

fn quick_chop_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    snap_ticks: u32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let created = match note_indices {
        Some(note_indices) => document.quick_chop_pattern_note_selection(
            pattern_id,
            channel_id,
            note_indices,
            snap_ticks,
        ),
        None => document.quick_chop_pattern_notes(pattern_id, channel_id, snap_ticks),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "created {created} Quick Chop notes in pattern {pattern_id}, channel {channel_id} ({}) at {snap_ticks} ticks in {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        output.display()
    );
    Ok(())
}

fn glue_notes(input: &Path, output: &Path, pattern_id: u16, channel_id: u16) -> Result<(), String> {
    glue_notes_with_scope(input, output, pattern_id, channel_id, None)
}

fn glue_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let removed = match note_indices {
        Some(note_indices) => {
            document.glue_pattern_note_selection(pattern_id, channel_id, note_indices)
        }
        None => document.glue_pattern_notes(pattern_id, channel_id),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "joined notes and removed {removed} records in pattern {pattern_id}, channel {channel_id} ({}) to {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        output.display()
    );
    Ok(())
}

fn flip_notes(input: &Path, output: &Path, pattern_id: u16, channel_id: u16) -> Result<(), String> {
    flip_notes_with_scope(input, output, pattern_id, channel_id, None)
}

fn flip_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = match note_indices {
        Some(note_indices) => {
            document.flip_pattern_note_selection(pattern_id, channel_id, note_indices)
        }
        None => document.flip_pattern_notes(pattern_id, channel_id),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "flipped {changed} note positions in pattern {pattern_id}, channel {channel_id} ({}) to {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
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
    strum_notes_with_options(
        input,
        output,
        pattern_id,
        channel_id,
        StrumOptions {
            spread_ticks,
            descending,
            ..StrumOptions::default()
        },
    )
}

fn strum_notes_with_options(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    options: StrumOptions,
) -> Result<(), String> {
    strum_notes_with_scope(input, output, pattern_id, channel_id, None, options)
}

fn strum_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    options: StrumOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = match note_indices {
        Some(note_indices) => document.strum_pattern_note_selection_with_options(
            pattern_id,
            channel_id,
            note_indices,
            options,
        ),
        None => document.strum_pattern_notes_with_options(pattern_id, channel_id, options),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "strummed {changed} notes in pattern {pattern_id}, channel {channel_id} ({}) with {} ticks of spread, start tension {}%, end shift {} ticks with {}% tension, velocity amount {}, velocity tension {}%, preserve-end={}, trigger-ahead={}, alternate-direction={}, chop-chords={} ({}) to {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        options.spread_ticks,
        options.start_tension_percent,
        options.end_time_ticks,
        options.end_tension_percent,
        options.velocity_amount,
        options.velocity_tension_percent,
        options.preserve_end,
        options.trigger_ahead,
        options.alternate_direction,
        options.chop_chords,
        if options.descending { "down" } else { "up" },
        output.display()
    );
    Ok(())
}

fn parse_strum_direction(value: &str) -> Result<bool, String> {
    match value {
        "up" => Ok(false),
        "down" => Ok(true),
        _ => Err("strum direction must be 'up' or 'down'".to_owned()),
    }
}

fn parse_strum_options(arguments: &[String]) -> Result<StrumOptions, String> {
    let [
        spread_ticks,
        direction,
        velocity_amount,
        velocity_tension,
        preserve_end,
        alternate_direction,
        start_tension,
        end_time_ticks,
        end_tension,
        trigger_ahead,
        chop_chords,
    ] = arguments
    else {
        return Err("advanced Strum options require eleven values".to_owned());
    };
    Ok(StrumOptions {
        spread_ticks: parse_u32(spread_ticks, "strum spread")?,
        descending: parse_strum_direction(direction)?,
        velocity_amount: parse_i16(velocity_amount, "strum velocity amount")?,
        velocity_tension_percent: parse_i16(velocity_tension, "strum velocity tension")?,
        preserve_end: parse_bool(preserve_end, "preserve end")?,
        alternate_direction: parse_bool(alternate_direction, "alternate direction")?,
        start_tension_percent: parse_i16(start_tension, "strum start tension")?,
        end_time_ticks: parse_i32(end_time_ticks, "strum end time shift")?,
        end_tension_percent: parse_i16(end_tension, "strum end tension")?,
        trigger_ahead: parse_bool(trigger_ahead, "trigger ahead")?,
        chop_chords: parse_bool(chop_chords, "chop chords")?,
    })
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
    flam_notes_with_options(
        input,
        output,
        pattern_id,
        channel_id,
        None,
        FlamOptions {
            stroke_ticks,
            velocity,
            before,
            group_notes: false,
        },
    )
}

fn flam_notes_with_options(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    options: FlamOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let created = match note_indices {
        Some(note_indices) => document.flam_pattern_note_selection_with_options(
            pattern_id,
            channel_id,
            note_indices,
            options,
        ),
        None => document.flam_pattern_notes_with_options(pattern_id, channel_id, options),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "added {created} flam strokes in pattern {pattern_id}, channel {channel_id}, {} ({}, {}) to {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        if options.before {
            "before notes"
        } else {
            "after notes"
        },
        if options.group_notes {
            "grouped"
        } else {
            "not grouped"
        },
        output.display()
    );
    Ok(())
}

fn generate_random_notes(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    options: RandomNoteGeneratorOptions,
    scale: Option<(u8, &[u8])>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let created = match scale {
        Some((scale_root, scale_intervals)) => document.generate_random_pattern_notes_with_scale(
            pattern_id,
            channel_id,
            options,
            scale_root,
            scale_intervals,
        ),
        None => document.generate_random_pattern_notes(pattern_id, channel_id, options),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "generated {created} seeded notes in pattern {pattern_id}, channel {channel_id} from tick {} to {} using seed {} to {}",
        options.start_tick,
        options.end_tick,
        options.seed,
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
    randomize_notes_inner(input, output, pattern_id, channel_id, None, options, None)
}

fn randomize_notes_with_scale(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    options: RandomizerOptions,
    scale_root: u8,
    scale_intervals: &[u8],
) -> Result<(), String> {
    randomize_notes_inner(
        input,
        output,
        pattern_id,
        channel_id,
        None,
        options,
        Some((scale_root, scale_intervals)),
    )
}

fn randomize_notes_selection(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: &[usize],
    options: RandomizerOptions,
    scale: Option<(u8, &[u8])>,
) -> Result<(), String> {
    randomize_notes_inner(
        input,
        output,
        pattern_id,
        channel_id,
        Some(note_indices),
        options,
        scale,
    )
}

fn randomize_notes_inner(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    options: RandomizerOptions,
    scale: Option<(u8, &[u8])>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = match (note_indices, scale) {
        (Some(note_indices), Some((scale_root, scale_intervals))) => document
            .randomize_pattern_note_selection_with_scale(
                pattern_id,
                channel_id,
                note_indices,
                options,
                scale_root,
                scale_intervals,
            ),
        (Some(note_indices), None) => {
            document.randomize_pattern_note_selection(pattern_id, channel_id, note_indices, options)
        }
        (None, Some((scale_root, scale_intervals))) => document.randomize_pattern_notes_with_scale(
            pattern_id,
            channel_id,
            options,
            scale_root,
            scale_intervals,
        ),
        (None, None) => document.randomize_pattern_notes(pattern_id, channel_id, options),
    }
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
    options: HumanizeCommandOptions<'_>,
) -> Result<(), String> {
    humanize_notes_with_options(input, output, pattern_id, channel_id, None, options)
}

fn humanize_notes_with_options(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    options: HumanizeCommandOptions<'_>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let humanizer_options = options.resolve(&document)?;
    let changed = match note_indices {
        Some(note_indices) => document.humanize_pattern_note_selection_with_options(
            pattern_id,
            channel_id,
            note_indices,
            options.seed,
            humanizer_options,
        ),
        None => document.humanize_pattern_notes_with_options(
            pattern_id,
            channel_id,
            options.seed,
            humanizer_options,
        ),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "humanized {changed} {} in pattern {pattern_id}, channel {channel_id} with seed {} to {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "channel notes"
        },
        options.seed,
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

fn limit_notes_with_options(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    options: LimitNoteOptions<'_>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = match note_indices {
        Some(note_indices) => document.limit_pattern_note_selection_range_with_options(
            pattern_id,
            channel_id,
            note_indices,
            options,
        ),
        None => document.limit_pattern_note_range_with_options(pattern_id, channel_id, options),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "limited {changed} note pitches in pattern {pattern_id}, channel {channel_id} ({}) to {}..={} in {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        options.minimum_key,
        options.maximum_key,
        output.display()
    );
    Ok(())
}

#[derive(Clone, Copy)]
struct ArpeggiatorCliOptions<'a> {
    step: &'a str,
    range_octaves: u8,
    gate_percent: u8,
    range_pattern: ArpeggioRangePattern,
    sync: ArpeggioSyncMode,
    group_notes: bool,
}

fn arpeggiate_notes(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    options: ArpeggiatorCliOptions<'_>,
) -> Result<(), String> {
    arpeggiate_notes_with_scope(input, output, pattern_id, channel_id, None, options)
}

fn arpeggiate_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    options: ArpeggiatorCliOptions<'_>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let options = ArpeggioOptions {
        step_ticks: parse_tempo_relative_ticks(options.step, &document, "arpeggiator step ticks")?,
        range_octaves: options.range_octaves,
        gate_percent: options.gate_percent,
        range_pattern: options.range_pattern,
        sync: options.sync,
        group_notes: options.group_notes,
    };
    let created = match note_indices {
        Some(note_indices) => document.arpeggiate_pattern_note_selection(
            pattern_id,
            channel_id,
            note_indices,
            options,
        ),
        None => document.arpeggiate_pattern_notes(pattern_id, channel_id, options),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "generated {created} arpeggiated notes in pattern {pattern_id}, channel {channel_id} ({}) to {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        output.display()
    );
    Ok(())
}

fn arpeggiate_score_notes(
    input: &Path,
    score_path: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    options: ArpeggioScoreOptions,
) -> Result<(), String> {
    let score_bytes = fs::read(score_path)
        .map_err(|error| format!("could not read {}: {error}", score_path.display()))?;
    let score = FscScore::parse(&score_bytes)
        .map_err(|error| format!("could not parse {}: {error}", score_path.display()))?;
    let (_, mut document) = load_document(input)?;
    let created = match note_indices {
        Some(note_indices) => document.arpeggiate_pattern_note_selection_with_score(
            pattern_id,
            channel_id,
            note_indices,
            &score,
            options,
        ),
        None => {
            document.arpeggiate_pattern_notes_with_score(pattern_id, channel_id, &score, options)
        }
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "generated {created} notes from score {} in pattern {pattern_id}, channel {channel_id} ({}) to {}",
        score_path.display(),
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        output.display()
    );
    Ok(())
}

fn riff_machine_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    options: RiffMachineOptions<'_>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let created = match note_indices {
        Some(note_indices) => document.riff_machine_pattern_note_selection(
            pattern_id,
            channel_id,
            note_indices,
            options,
        ),
        None => document.riff_machine_pattern_notes(pattern_id, channel_id, options),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "Riff Machine produced {created} notes in pattern {pattern_id}, channel {channel_id} ({}) with seed {} in {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        options.seed,
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
    slice_notes_with_scope(input, output, pattern_id, channel_id, None, position_ticks)
}

fn slice_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    position_ticks: u32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let created = match note_indices {
        Some(note_indices) => document.slice_pattern_note_selection(
            pattern_id,
            channel_id,
            note_indices,
            position_ticks,
        ),
        None => document.slice_pattern_notes(pattern_id, channel_id, position_ticks),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "sliced {created} notes at tick {position_ticks} in pattern {pattern_id}, channel {channel_id} ({}) to {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        output.display()
    );
    Ok(())
}

fn slice_notes_at_positions(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    note_cuts: &[(u16, usize, u32)],
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let created = document
        .slice_pattern_notes_at_positions(pattern_id, note_cuts)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "created {created} sliced notes in pattern {pattern_id} from {} note cuts in {}",
        note_cuts.len(),
        output.display()
    );
    Ok(())
}

fn scale_levels_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    options: ScaleLevelsOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = match note_indices {
        Some(note_indices) => document.scale_pattern_note_selection_levels_with_options(
            pattern_id,
            channel_id,
            note_indices,
            options,
        ),
        None => document.scale_pattern_note_levels_with_options(pattern_id, channel_id, options),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "scaled velocity levels for {changed} notes in pattern {pattern_id}, channel {channel_id} ({}) to {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        output.display()
    );
    Ok(())
}

fn articulate_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    options: ArticulateOptions,
    only_with_selection: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = match note_indices {
        Some(note_indices) => document.articulate_pattern_note_selection(
            pattern_id,
            channel_id,
            note_indices,
            options,
            only_with_selection,
        ),
        None => document.articulate_pattern_notes(pattern_id, channel_id, options),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "articulated lengths for {changed} notes in pattern {pattern_id}, channel {channel_id} ({}) to {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        output.display()
    );
    Ok(())
}

fn claw_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
    options: ClawMachineOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let changed = match note_indices {
        Some(note_indices) => {
            document.claw_pattern_note_selection(pattern_id, channel_id, note_indices, options)
        }
        None => document.claw_pattern_notes(pattern_id, channel_id, options),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "applied Claw Machine to {changed} notes in pattern {pattern_id}, channel {channel_id} ({}) to {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
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

fn add_chord(
    input: &Path,
    output: &Path,
    quality: &str,
    options: ChordStampCliOptions,
) -> Result<(), String> {
    let ChordStampCliOptions {
        pattern_id,
        channel_id,
        position,
        length,
        root_key,
        velocity,
        inversion,
    } = options;
    if root_key > 127 {
        return Err("root note key must be between 0 and 127".to_owned());
    }
    if velocity > 127 {
        return Err("note velocity must be between 0 and 127".to_owned());
    }
    if length == 0 {
        return Err("chord note length must be greater than zero".to_owned());
    }
    let intervals = parse_chord_intervals(quality)?;
    let mut pitches = intervals
        .iter()
        .filter_map(|interval| root_key.checked_add(u16::from(*interval)))
        .filter(|key| *key <= 127)
        .collect::<Vec<_>>();
    if pitches.is_empty() {
        return Err("the selected chord has no notes in the MIDI key range".to_owned());
    }
    if inversion >= pitches.len() {
        return Err(format!(
            "chord inversion must be between 0 and {} for this quality",
            pitches.len() - 1
        ));
    }
    for _ in 0..inversion {
        let lowest = pitches.remove(0);
        let raised = lowest
            .checked_add(12)
            .filter(|key| *key <= 127)
            .ok_or_else(|| "the selected chord inversion exceeds MIDI key 127".to_owned())?;
        pitches.push(raised);
    }
    pitches.sort_unstable();
    let notes = pitches
        .into_iter()
        .map(|key| PatternNote {
            position,
            channel_id,
            length,
            key,
            velocity,
            ..PatternNote::default()
        })
        .collect::<Vec<_>>();

    let (_, mut document) = load_document(input)?;
    document
        .add_pattern_notes(pattern_id, &notes)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "added {} {quality} chord notes at tick {position} in pattern {pattern_id}, channel {channel_id} to {}",
        notes.len(),
        output.display()
    );
    Ok(())
}

fn add_scale_chord(
    input: &Path,
    output: &Path,
    quality: &str,
    scale_name: &str,
    scale_root: u8,
    options: ChordStampCliOptions,
) -> Result<(), String> {
    let ChordStampCliOptions {
        pattern_id,
        channel_id,
        position,
        length,
        root_key,
        velocity,
        inversion,
    } = options;
    if length == 0 {
        return Err("chord note length must be greater than zero".to_owned());
    }
    if velocity > 127 {
        return Err("note velocity must be between 0 and 127".to_owned());
    }
    if scale_root > 11 {
        return Err("scale root must be between 0 and 11".to_owned());
    }
    let mut pitches = if scale_name == "none" && quality.starts_with("scale-") {
        unscaled_scale_chord_pitches(root_key, quality)?
    } else {
        let scale_intervals = parse_chord_scale(scale_name)?;
        scale_chord_pitches(root_key, scale_intervals, scale_root, quality)?
    };
    if pitches.is_empty() {
        return Err("the selected chord has no notes in the MIDI key range".to_owned());
    }
    apply_chord_inversion(&mut pitches, inversion)?;
    let notes = pitches
        .into_iter()
        .map(|key| PatternNote {
            position,
            channel_id,
            length,
            key,
            velocity,
            ..PatternNote::default()
        })
        .collect::<Vec<_>>();

    let (_, mut document) = load_document(input)?;
    document
        .add_pattern_notes(pattern_id, &notes)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "added {} {quality} chord notes in {scale_name} at tick {position} in pattern {pattern_id}, channel {channel_id} to {}",
        notes.len(),
        output.display()
    );
    Ok(())
}

fn add_notes(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    notes: &[PatternNote],
) -> Result<(), String> {
    let channel_id = notes
        .first()
        .ok_or_else(|| "at least one note must be supplied".to_owned())?
        .channel_id;
    let (_, mut document) = load_document(input)?;
    document
        .add_pattern_notes(pattern_id, notes)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "added {} notes to pattern {pattern_id}, channel {} in {}",
        notes.len(),
        channel_id,
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

fn duplicate_pattern(input: &Path, output: &Path, pattern_id: u16) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let duplicate_id = document
        .duplicate_pattern(pattern_id)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "duplicated pattern {pattern_id} as pattern {duplicate_id} in {}",
        output.display()
    );
    Ok(())
}

fn rename_pattern(input: &Path, output: &Path, pattern_id: u16, name: &str) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_pattern_name(pattern_id, name)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "renamed Pattern {pattern_id} to {name:?} in {}",
        output.display()
    );
    Ok(())
}

fn set_pattern_color(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    rgb: [u8; 3],
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .set_pattern_color(pattern_id, rgb)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Pattern {pattern_id} color to #{:02X}{:02X}{:02X} in {}",
        rgb[0],
        rgb[1],
        rgb[2],
        output.display()
    );
    Ok(())
}

fn delete_pattern(input: &Path, output: &Path, pattern_id: u16) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .delete_pattern(pattern_id)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "deleted unused Pattern {pattern_id} in {}",
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

fn delete_notes_with_scope(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    note_indices: Option<&[usize]>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let deleted = match note_indices {
        Some(note_indices) => {
            document.delete_pattern_note_selection(pattern_id, channel_id, note_indices)
        }
        None => document.delete_pattern_notes(pattern_id, channel_id),
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "deleted {deleted} notes in pattern {pattern_id}, channel {channel_id} ({}) to {}",
        if note_indices.is_some() {
            "selected notes"
        } else {
            "all channel notes"
        },
        output.display()
    );
    Ok(())
}

fn import_score_notes(
    input: &Path,
    score_path: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
    replace_channel_notes: bool,
) -> Result<(), String> {
    let score_bytes = fs::read(score_path)
        .map_err(|error| format!("could not read {}: {error}", score_path.display()))?;
    let score = FscScore::parse(&score_bytes)
        .map_err(|error| format!("could not parse {}: {error}", score_path.display()))?;
    let (_, mut document) = load_document(input)?;
    let imported = document
        .import_score_notes(pattern_id, channel_id, &score, replace_channel_notes)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "imported {imported} score notes into pattern {pattern_id}, channel {channel_id} ({}) to {}",
        if replace_channel_notes {
            "replaced notes"
        } else {
            "appended notes"
        },
        output.display()
    );
    Ok(())
}

fn import_score_automation(
    input: &Path,
    score_path: &Path,
    output: &Path,
    pattern_id: u16,
    options: FscControllerStreamImportOptions,
) -> Result<(), String> {
    let score_bytes = fs::read(score_path)
        .map_err(|error| format!("could not read {}: {error}", score_path.display()))?;
    let score = FscScore::parse(&score_bytes)
        .map_err(|error| format!("could not parse {}: {error}", score_path.display()))?;
    let (_, mut document) = load_document(input)?;
    let imported = document
        .import_score_controller_stream(pattern_id, &score, options)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "imported {imported} automation points from stream {} / 0x{:02X} into Pattern {pattern_id}, stream {} / 0x{:02X} ({}) to {}",
        options.source_stream.0,
        options.source_stream.1,
        options.target_stream.0,
        options.target_stream.1,
        if options.replace_target_stream {
            "replaced target stream"
        } else {
            "appended to target stream"
        },
        output.display()
    );
    Ok(())
}

fn export_score_notes(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    channel_id: u16,
) -> Result<(), String> {
    let (_, document) = load_document(input)?;
    let bytes = document
        .export_score_notes(pattern_id, channel_id)
        .map_err(|error| error.to_string())?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "exported pattern {pattern_id}, channel {channel_id} as score to {}",
        output.display()
    );
    Ok(())
}

fn export_score_automation(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    stream: (u8, u8),
) -> Result<(), String> {
    let (_, document) = load_document(input)?;
    let bytes = document
        .export_score_controller_stream(pattern_id, stream)
        .map_err(|error| error.to_string())?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "exported Pattern {pattern_id} raw automation stream {} / 0x{:02X} as score to {}",
        stream.0,
        stream.1,
        output.display()
    );
    Ok(())
}

fn import_midi_track(
    input: &Path,
    midi_path: &Path,
    output: &Path,
    track_index: usize,
    pattern_id: Option<u16>,
    channel_id: u16,
    options: MidiImportOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let midi_bytes = fs::read(midi_path)
        .map_err(|error| format!("could not read {}: {error}", midi_path.display()))?;
    let midi = MidiFile::parse(&midi_bytes)
        .map_err(|error| format!("could not parse {}: {error}", midi_path.display()))?;
    let (pattern_id, imported) = if let Some(pattern_id) = pattern_id {
        let imported = document
            .import_midi_track_with_options(&midi, track_index, pattern_id, channel_id, options)
            .map_err(|error| error.to_string())?;
        (pattern_id, imported)
    } else {
        document
            .import_midi_track_to_new_pattern_with_options(&midi, track_index, channel_id, options)
            .map_err(|error| error.to_string())?
    };
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

fn import_midi_control_changes(request: MidiControlChangeImport<'_>) -> Result<(), String> {
    let MidiControlChangeImport {
        input,
        midi_path,
        output,
        track_index,
        pattern_id,
        target_stream,
        controller_number,
        midi_channel,
        new_stream,
    } = request;
    let (_, mut document) = load_document(input)?;
    let midi_bytes = fs::read(midi_path)
        .map_err(|error| format!("could not read {}: {error}", midi_path.display()))?;
    let midi = MidiFile::parse(&midi_bytes)
        .map_err(|error| format!("could not parse {}: {error}", midi_path.display()))?;
    let imported = if new_stream {
        document.import_midi_control_changes_to_new_stream(
            &midi,
            track_index,
            pattern_id,
            target_stream,
            controller_number,
            midi_channel,
        )
    } else {
        document.import_midi_control_changes(
            &midi,
            track_index,
            pattern_id,
            target_stream,
            controller_number,
            midi_channel,
        )
    }
    .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    let source_channel = midi_channel.map_or_else(
        || "all channels".to_owned(),
        |channel| format!("MIDI channel {}", channel + 1),
    );
    let stream_kind = if new_stream {
        "new raw stream"
    } else {
        "raw stream"
    };
    println!(
        "imported {imported} MIDI CC {controller_number} events from track {track_index} ({source_channel}) into pattern {pattern_id}, {stream_kind} {} / 0x{:02X} in {}",
        target_stream.0,
        target_stream.1,
        output.display()
    );
    Ok(())
}

fn import_midi_tracks(
    input: &Path,
    midi_path: &Path,
    output: &Path,
    pattern_id: Option<u16>,
    channel_ids: &[u16],
    options: MidiImportOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let midi_bytes = fs::read(midi_path)
        .map_err(|error| format!("could not read {}: {error}", midi_path.display()))?;
    let midi = MidiFile::parse(&midi_bytes)
        .map_err(|error| format!("could not parse {}: {error}", midi_path.display()))?;
    let (pattern_id, imported) = if let Some(pattern_id) = pattern_id {
        let imported = document
            .import_midi_tracks_with_options(&midi, pattern_id, channel_ids, options)
            .map_err(|error| error.to_string())?;
        (pattern_id, imported)
    } else {
        document
            .import_midi_tracks_to_new_pattern_with_options(&midi, channel_ids, options)
            .map_err(|error| error.to_string())?
    };
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "imported {imported} MIDI notes from {} tracks into pattern {pattern_id} in {}",
        midi.tracks().len(),
        output.display()
    );
    Ok(())
}

fn import_midi_tracks_to_named_patterns(
    input: &Path,
    midi_path: &Path,
    output: &Path,
    channel_id: u16,
    options: MidiImportOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let midi_bytes = fs::read(midi_path)
        .map_err(|error| format!("could not read {}: {error}", midi_path.display()))?;
    let midi = MidiFile::parse(&midi_bytes)
        .map_err(|error| format!("could not parse {}: {error}", midi_path.display()))?;
    let imported_tracks = document
        .import_midi_tracks_to_new_patterns_with_options(&midi, channel_id, options)
        .map_err(|error| error.to_string())?;
    let imported_notes = imported_tracks
        .iter()
        .map(|(_, imported)| *imported)
        .sum::<usize>();
    let pattern_ids = imported_tracks
        .iter()
        .map(|(pattern_id, _)| pattern_id.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "imported {} MIDI tracks ({imported_notes} notes) into named patterns [{pattern_ids}] on channel {channel_id} in {}",
        imported_tracks.len(),
        output.display()
    );
    Ok(())
}

fn import_midi_selected_tracks(
    input: &Path,
    midi_path: &Path,
    output: &Path,
    pattern_id: Option<u16>,
    track_numbers: &str,
    channel_ids: &[u16],
    options: MidiImportOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let midi_bytes = fs::read(midi_path)
        .map_err(|error| format!("could not read {}: {error}", midi_path.display()))?;
    let midi = MidiFile::parse(&midi_bytes)
        .map_err(|error| format!("could not parse {}: {error}", midi_path.display()))?;
    let track_indices = parse_midi_track_selection(track_numbers, midi.tracks().len())?;
    let (pattern_id, imported) = if let Some(pattern_id) = pattern_id {
        let imported = document
            .import_midi_selected_tracks_with_options(
                &midi,
                pattern_id,
                &track_indices,
                channel_ids,
                options,
            )
            .map_err(|error| error.to_string())?;
        (pattern_id, imported)
    } else {
        document
            .import_midi_selected_tracks_to_new_pattern_with_options(
                &midi,
                &track_indices,
                channel_ids,
                options,
            )
            .map_err(|error| error.to_string())?
    };
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "imported {imported} MIDI notes from tracks {} into pattern {pattern_id} in {}",
        track_indices
            .iter()
            .map(|index| (index + 1).to_string())
            .collect::<Vec<_>>()
            .join(","),
        output.display()
    );
    Ok(())
}

fn import_midi_selected_sampler_tracks(
    input: &Path,
    midi_path: &Path,
    output: &Path,
    pattern_id: Option<u16>,
    template_channel_id: u16,
    track_numbers: &str,
    options: MidiImportOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let midi_bytes = fs::read(midi_path)
        .map_err(|error| format!("could not read {}: {error}", midi_path.display()))?;
    let midi = MidiFile::parse(&midi_bytes)
        .map_err(|error| format!("could not parse {}: {error}", midi_path.display()))?;
    let track_indices = parse_midi_track_selection(track_numbers, midi.tracks().len())?;
    let (pattern_id, imported, channel_ids) = if let Some(pattern_id) = pattern_id {
        let (imported, channel_ids) = document
            .import_midi_selected_tracks_to_sampler_channels_with_options(
                &midi,
                pattern_id,
                template_channel_id,
                &track_indices,
                options,
            )
            .map_err(|error| error.to_string())?;
        (pattern_id, imported, channel_ids)
    } else {
        document
            .import_midi_selected_tracks_to_sampler_channels_in_new_pattern_with_options(
                &midi,
                template_channel_id,
                &track_indices,
                options,
            )
            .map_err(|error| error.to_string())?
    };
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    let channel_ids = channel_ids
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "created Sampler channels {channel_ids} from MIDI tracks {} and imported {imported} notes into pattern {pattern_id} in {}",
        track_indices
            .iter()
            .map(|index| (index + 1).to_string())
            .collect::<Vec<_>>()
            .join(","),
        output.display()
    );
    Ok(())
}

fn import_midi_selected_format2_tracks(
    input: &Path,
    midi_path: &Path,
    output: &Path,
    template_channel_id: u16,
    track_numbers: &str,
    options: MidiImportOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let midi_bytes = fs::read(midi_path)
        .map_err(|error| format!("could not read {}: {error}", midi_path.display()))?;
    let midi = MidiFile::parse(&midi_bytes)
        .map_err(|error| format!("could not parse {}: {error}", midi_path.display()))?;
    let track_indices = parse_midi_track_selection(track_numbers, midi.tracks().len())?;
    let (imported, destinations) = document
        .import_midi_selected_format2_tracks_to_sampler_patterns_with_options(
            &midi,
            template_channel_id,
            &track_indices,
            options,
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    let destinations = destinations
        .iter()
        .map(|(pattern_id, channel_id)| format!("{pattern_id}:{channel_id}"))
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "imported {imported} notes from Format-2 tracks {} into Pattern:channel pairs {destinations} in {}",
        track_indices
            .iter()
            .map(|index| (index + 1).to_string())
            .collect::<Vec<_>>()
            .join(","),
        output.display()
    );
    Ok(())
}

fn import_midi_sampler_tracks(
    input: &Path,
    midi_path: &Path,
    output: &Path,
    pattern_id: Option<u16>,
    template_channel_id: u16,
    options: MidiImportOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let midi_bytes = fs::read(midi_path)
        .map_err(|error| format!("could not read {}: {error}", midi_path.display()))?;
    let midi = MidiFile::parse(&midi_bytes)
        .map_err(|error| format!("could not parse {}: {error}", midi_path.display()))?;
    let (pattern_id, imported, channel_ids) = if let Some(pattern_id) = pattern_id {
        let (imported, channel_ids) = document
            .import_midi_tracks_to_sampler_channels_with_options(
                &midi,
                pattern_id,
                template_channel_id,
                options,
            )
            .map_err(|error| error.to_string())?;
        (pattern_id, imported, channel_ids)
    } else {
        document
            .import_midi_tracks_to_sampler_channels_in_new_pattern_with_options(
                &midi,
                template_channel_id,
                options,
            )
            .map_err(|error| error.to_string())?
    };
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    let channel_ids = channel_ids
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "created Sampler channels {channel_ids} and imported {imported} MIDI notes into pattern {pattern_id} in {}",
        output.display()
    );
    Ok(())
}

fn import_midi_template_tracks(
    input: &Path,
    midi_path: &Path,
    output: &Path,
    pattern_id: Option<u16>,
    template_channel_id: u16,
    track_numbers: &str,
    options: MidiImportOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let midi_bytes = fs::read(midi_path)
        .map_err(|error| format!("could not read {}: {error}", midi_path.display()))?;
    let midi = MidiFile::parse(&midi_bytes)
        .map_err(|error| format!("could not parse {}: {error}", midi_path.display()))?;
    let track_indices = parse_midi_track_selection(track_numbers, midi.tracks().len())?;
    let (pattern_id, imported, channel_ids) = if let Some(pattern_id) = pattern_id {
        let (imported, channel_ids) = document
            .import_midi_selected_tracks_to_template_channels_with_options(
                &midi,
                pattern_id,
                template_channel_id,
                &track_indices,
                options,
            )
            .map_err(|error| error.to_string())?;
        (pattern_id, imported, channel_ids)
    } else {
        document
            .import_midi_selected_tracks_to_template_channels_in_new_pattern_with_options(
                &midi,
                template_channel_id,
                &track_indices,
                options,
            )
            .map_err(|error| error.to_string())?
    };
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    let channel_ids = channel_ids
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "created template channels {channel_ids} from MIDI tracks {} and imported {imported} notes into pattern {pattern_id} in {}",
        track_indices
            .iter()
            .map(|index| (index + 1).to_string())
            .collect::<Vec<_>>()
            .join(","),
        output.display()
    );
    Ok(())
}

fn import_midi_template_format2_tracks(
    input: &Path,
    midi_path: &Path,
    output: &Path,
    template_channel_id: u16,
    track_numbers: &str,
    options: MidiImportOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let midi_bytes = fs::read(midi_path)
        .map_err(|error| format!("could not read {}: {error}", midi_path.display()))?;
    let midi = MidiFile::parse(&midi_bytes)
        .map_err(|error| format!("could not parse {}: {error}", midi_path.display()))?;
    let track_indices = parse_midi_track_selection(track_numbers, midi.tracks().len())?;
    let (imported, destinations) = document
        .import_midi_selected_format2_tracks_to_template_patterns_with_options(
            &midi,
            template_channel_id,
            &track_indices,
            options,
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    let destinations = destinations
        .iter()
        .map(|(pattern_id, channel_id)| format!("{pattern_id}:{channel_id}"))
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "imported {imported} Format-2 notes from tracks {} into template-channel Pattern:channel pairs {destinations} in {}",
        track_indices
            .iter()
            .map(|index| (index + 1).to_string())
            .collect::<Vec<_>>()
            .join(","),
        output.display()
    );
    Ok(())
}

fn import_midi_into_new_project(
    template_path: &Path,
    midi_path: &Path,
    output: &Path,
    template_channel_id: u16,
    track_numbers: &str,
    options: MidiImportOptions,
) -> Result<(), String> {
    let template_bytes = fs::read(template_path)
        .map_err(|error| format!("could not read {}: {error}", template_path.display()))?;
    let template = FlpDocument::parse(&template_bytes).map_err(|error| {
        format!(
            "could not parse project template {}: {error}",
            template_path.display()
        )
    })?;
    let midi_bytes = fs::read(midi_path)
        .map_err(|error| format!("could not read {}: {error}", midi_path.display()))?;
    let midi = MidiFile::parse(&midi_bytes)
        .map_err(|error| format!("could not parse {}: {error}", midi_path.display()))?;
    let track_indices = parse_midi_track_selection(track_numbers, midi.tracks().len())?;
    let import_result = FlpDocument::new_project_from_midi_selected_tracks_with_options(
        &template,
        &midi,
        template_channel_id,
        &track_indices,
        options,
    )
    .map_err(|error| error.to_string())?;
    let bytes = import_result
        .project
        .encode_lossless()
        .map_err(|error| format!("could not encode new project: {error}"))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    let destinations = import_result
        .destinations
        .iter()
        .map(|(pattern_id, channel_id)| format!("{pattern_id}:{channel_id}"))
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "created new project from {} with {} MIDI notes in Pattern:channel pairs {destinations} at {}",
        template_path.display(),
        import_result.imported_notes,
        output.display()
    );
    Ok(())
}

fn import_midi_format2(
    input: &Path,
    midi_path: &Path,
    output: &Path,
    template_channel_id: u16,
    options: MidiImportOptions,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let midi_bytes = fs::read(midi_path)
        .map_err(|error| format!("could not read {}: {error}", midi_path.display()))?;
    let midi = MidiFile::parse(&midi_bytes)
        .map_err(|error| format!("could not parse {}: {error}", midi_path.display()))?;
    let (imported, destinations) = document
        .import_midi_format2_tracks_to_sampler_patterns_with_options(
            &midi,
            template_channel_id,
            options,
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    let destinations = destinations
        .iter()
        .map(|(pattern_id, channel_id)| format!("{pattern_id}:{channel_id}"))
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "imported {imported} Format-2 MIDI notes into Pattern:channel pairs {destinations} in {}",
        output.display()
    );
    Ok(())
}

fn create_playlist_pattern_clip(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    pattern_id: u16,
    position_ticks: u32,
    length_ticks: u32,
    track_index: u16,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let clip_index = document
        .create_playlist_pattern_clip(
            arrangement_id,
            pattern_id,
            position_ticks,
            length_ticks,
            track_index,
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "created Playlist Pattern Clip {clip_index} for pattern {pattern_id} at tick {position_ticks}, length {length_ticks}, track {track_index} in {}",
        output.display()
    );
    Ok(())
}

fn create_playlist_audio_clip(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    channel_id: u16,
    position_ticks: u32,
    length_ticks: u32,
    track_index: u16,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let clip_index = document
        .create_playlist_audio_clip(
            arrangement_id,
            channel_id,
            position_ticks,
            length_ticks,
            track_index,
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "created Playlist Audio Clip {clip_index} for channel {channel_id} at tick {position_ticks}, length {length_ticks}, track {track_index} in {}",
        output.display()
    );
    Ok(())
}

fn create_playlist_automation_clip(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    channel_id: u16,
    position_ticks: u32,
    length_ticks: u32,
    track_index: u16,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let clip_index = document
        .create_playlist_automation_clip(
            arrangement_id,
            channel_id,
            position_ticks,
            length_ticks,
            track_index,
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "created Playlist Automation Clip {clip_index} for channel {channel_id} at tick {position_ticks}, length {length_ticks}, track {track_index} in {}",
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

fn delete_playlist_clip(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_index: usize,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .delete_playlist_clip(arrangement_id, clip_index)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "deleted Playlist clip {clip_index} from arrangement {arrangement_id} in {}",
        output.display()
    );
    Ok(())
}

fn ripple_delete_playlist_time_range(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    start_tick: u32,
    end_tick: u32,
    full_source_lengths_ms: BTreeMap<usize, f32>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .ripple_delete_playlist_time_range(
            arrangement_id,
            start_tick,
            end_tick,
            &full_source_lengths_ms,
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "ripple-deleted Playlist time {start_tick}..{end_tick} in arrangement {arrangement_id} to {}",
        output.display()
    );
    Ok(())
}

fn insert_playlist_time_range(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    start_tick: u32,
    end_tick: u32,
    full_source_lengths_ms: BTreeMap<usize, f32>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .insert_playlist_time_range(
            arrangement_id,
            start_tick,
            end_tick,
            &full_source_lengths_ms,
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "inserted Playlist time {start_tick}..{end_tick} in arrangement {arrangement_id} to {}",
        output.display()
    );
    Ok(())
}

fn set_clip_muted(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_index: usize,
    muted: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_playlist_clip(
            arrangement_id,
            clip_index,
            PlaylistClipEdit {
                muted: Some(muted),
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
        "set arrangement {arrangement_id} clip {clip_index} muted={muted} in {}",
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

fn edit_audio_clip_window_ticks(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_index: usize,
    source_start_tick: u32,
    source_end_tick: u32,
    length_ticks: Option<u32>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_playlist_clip(
            arrangement_id,
            clip_index,
            PlaylistClipEdit {
                source_window_ticks: Some((source_start_tick, source_end_tick)),
                length_ticks,
                ..PlaylistClipEdit::default()
            },
        )
        .map_err(|error| error.to_string())?;
    let sample_path = playlist_audio_clip_sample_path(&document, arrangement_id, clip_index)?;
    let sample_file = SamplePathResolver::new(input)
        .resolve(&sample_path)
        .map_err(|error| format!("cannot validate Audio Clip source duration: {error}"))?;
    let audio = decode_audio_file(&sample_file).map_err(|error| {
        format!("cannot decode Audio Clip sample for source-window validation: {error}")
    })?;
    let ppq = document.header().ppq();
    let tempo_bpm = document.metadata().tempo_bpm().unwrap_or(140.0);
    if ppq == 0 || !tempo_bpm.is_finite() || tempo_bpm <= 0.0 || audio.sample_rate == 0 {
        return Err("Audio Clip source duration cannot be mapped to project ticks".to_owned());
    }
    let duration_milliseconds =
        (audio.frame_count() as f64 * 1_000.0 / f64::from(audio.sample_rate)) as f32;
    let duration_ticks = f64::from(duration_milliseconds) * f64::from(ppq) * tempo_bpm / 60_000.0;
    if !duration_ticks.is_finite() || duration_ticks <= 0.0 || duration_ticks > f64::from(u32::MAX)
    {
        return Err("Audio Clip sample duration is outside the supported tick range".to_owned());
    }
    let maximum_source_tick = duration_ticks.floor() as u32;
    if source_end_tick > maximum_source_tick {
        return Err(format!(
            "Audio Clip source end tick {source_end_tick} exceeds the sample duration of {maximum_source_tick} ticks"
        ));
    }
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "updated arrangement {arrangement_id} Audio Clip {clip_index} source window to ticks {source_start_tick}..{source_end_tick} in {}",
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

fn set_playlist_track_trigger_sync(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    track_id: u32,
    trigger_sync: PlaylistTrackSync,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_playlist_track_in_arrangement(
            arrangement_id,
            track_id,
            PlaylistTrackEdit {
                trigger_sync: Some(trigger_sync),
                ..PlaylistTrackEdit::default()
            },
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Playlist track {track_id} Trigger Sync to {} in arrangement {arrangement_id} in {}",
        playlist_track_sync_cli_label(trigger_sync),
        output.display()
    );
    Ok(())
}

fn set_playlist_track_position_sync(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    track_id: u32,
    position_sync: PlaylistTrackSync,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_playlist_track_in_arrangement(
            arrangement_id,
            track_id,
            PlaylistTrackEdit {
                position_sync: Some(position_sync),
                ..PlaylistTrackEdit::default()
            },
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Playlist track {track_id} Position Sync to {} in arrangement {arrangement_id} in {}",
        playlist_track_sync_cli_label(position_sync),
        output.display()
    );
    Ok(())
}

fn set_playlist_track_tolerant(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    track_id: u32,
    tolerant: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_playlist_track_in_arrangement(
            arrangement_id,
            track_id,
            PlaylistTrackEdit {
                tolerant: Some(tolerant),
                ..PlaylistTrackEdit::default()
            },
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "{} Tolerant launch for Playlist track {track_id} in arrangement {arrangement_id} in {}",
        if tolerant { "enabled" } else { "disabled" },
        output.display()
    );
    Ok(())
}

fn set_playlist_track_queued(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    track_id: u32,
    queued: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_playlist_track_in_arrangement(
            arrangement_id,
            track_id,
            PlaylistTrackEdit {
                queued: Some(queued),
                ..PlaylistTrackEdit::default()
            },
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "{} Queue for Playlist track {track_id} in arrangement {arrangement_id} in {}",
        if queued { "enabled" } else { "disabled" },
        output.display()
    );
    Ok(())
}

fn set_playlist_track_press(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    track_id: u32,
    press: PlaylistTrackPress,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_playlist_track_in_arrangement(
            arrangement_id,
            track_id,
            PlaylistTrackEdit {
                press: Some(press),
                ..PlaylistTrackEdit::default()
            },
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Playlist track {track_id} Press to {} in arrangement {arrangement_id} in {}",
        playlist_track_press_cli_label(press),
        output.display()
    );
    Ok(())
}

fn set_playlist_track_motion(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    track_id: u32,
    motion: PlaylistTrackMotion,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .edit_playlist_track_in_arrangement(
            arrangement_id,
            track_id,
            PlaylistTrackEdit {
                motion: Some(motion),
                ..PlaylistTrackEdit::default()
            },
        )
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "set Playlist track {track_id} Motion to {} in arrangement {arrangement_id} in {}",
        playlist_track_motion_cli_label(motion),
        output.display()
    );
    Ok(())
}

fn rename_playlist_track(
    input: &Path,
    output: &Path,
    track_id: u32,
    name: &str,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    document
        .rename_playlist_track(track_id, name)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!("renamed Playlist track {track_id} in {}", output.display());
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

fn split_pattern_clip(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_index: usize,
    split_position_ticks: u32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let right_clip_index = document
        .split_playlist_pattern_clip(arrangement_id, clip_index, split_position_ticks)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "split arrangement {arrangement_id} Pattern Clip {clip_index} at tick {split_position_ticks}; right clip is {right_clip_index} in {}",
        output.display()
    );
    Ok(())
}

fn split_automation_clip(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_index: usize,
    split_position_ticks: u32,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let right_clip_index = document
        .split_playlist_automation_clip(arrangement_id, clip_index, split_position_ticks)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "split arrangement {arrangement_id} Automation Clip {clip_index} at tick {split_position_ticks}; right clip is {right_clip_index} in {}",
        output.display()
    );
    Ok(())
}

fn make_automation_clip_unique(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_index: usize,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let new_channel_id = document
        .make_playlist_automation_clip_unique(arrangement_id, clip_index)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "made arrangement {arrangement_id} Automation Clip {clip_index} unique as channel {new_channel_id} in {}",
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

fn make_audio_clip_unique(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_index: usize,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let channel_id = document
        .make_playlist_audio_clip_unique(arrangement_id, clip_index)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "made arrangement {arrangement_id} Audio Clip {clip_index} unique as channel {channel_id} in {}",
        output.display()
    );
    Ok(())
}

fn set_playlist_audio_clip_reversed(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_index: usize,
    reversed: bool,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let channel_id = document
        .set_playlist_audio_clip_reversed(arrangement_id, clip_index, reversed)
        .map_err(|error| error.to_string())?;
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "{} Reverse on arrangement {arrangement_id} Audio Clip {clip_index} (channel {channel_id}) in {}",
        if reversed { "enabled" } else { "disabled" },
        output.display()
    );
    Ok(())
}

fn make_audio_clip_unique_as_sample(
    input: &Path,
    output: &Path,
    arrangement_id: u16,
    clip_index: usize,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    let source_sample_path =
        playlist_audio_clip_sample_path(&document, arrangement_id, clip_index)?;
    let source_sample = SamplePathResolver::new(input)
        .resolve(&source_sample_path)
        .map_err(|error| format!("could not find the Audio Clip sample: {error}"))?;
    let channel_id = document
        .make_playlist_audio_clip_unique(arrangement_id, clip_index)
        .map_err(|error| error.to_string())?;
    let output_directory = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let sample_directory = output_directory.join("Samples");
    let copied_sample = copy_unique_sample_file(&source_sample, &sample_directory)?;
    let Some(filename) = copied_sample.file_name().and_then(|name| name.to_str()) else {
        let _ = fs::remove_file(&copied_sample);
        return Err("the copied sample filename cannot be stored in the project".to_owned());
    };
    let sample_reference = Path::new("Samples")
        .join(filename)
        .to_string_lossy()
        .into_owned();
    if let Err(error) = document.set_channel_sample_path(channel_id, &sample_reference) {
        let _ = fs::remove_file(&copied_sample);
        return Err(error.to_string());
    }
    let bytes = match document.encode_lossless() {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = fs::remove_file(&copied_sample);
            return Err(format!("could not encode {}: {error}", input.display()));
        }
    };
    if let Err(error) = fs::write(output, bytes) {
        let _ = fs::remove_file(&copied_sample);
        return Err(format!("could not write {}: {error}", output.display()));
    }
    println!(
        "made arrangement {arrangement_id} Audio Clip {clip_index} unique as channel {channel_id} with sample {} in {}",
        copied_sample.display(),
        output.display()
    );
    Ok(())
}

fn playlist_audio_clip_sample_path(
    document: &FlpDocument,
    arrangement_id: u16,
    clip_index: usize,
) -> Result<String, String> {
    let arrangement = document
        .arrangements()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|arrangement| arrangement.id == arrangement_id)
        .ok_or_else(|| "the requested arrangement does not exist".to_owned())?;
    let clip = arrangement
        .clips
        .get(clip_index)
        .ok_or_else(|| "the requested Playlist clip does not exist".to_owned())?;
    let flp_rebuild::PlaylistClipTarget::Channel { id } = clip.target() else {
        return Err("Make unique as sample applies to Playlist Audio Clips".to_owned());
    };
    let channels = document.channels();
    let mut matching_channels = channels
        .iter()
        .filter(|channel| channel.id() == id && channel.kind() == Some(4));
    let Some(channel) = matching_channels.next() else {
        return Err("the Playlist Audio Clip channel does not exist".to_owned());
    };
    if matching_channels.next().is_some() {
        return Err(format!("Audio Clip channel ID {id} is ambiguous"));
    }
    channel
        .sample_path()
        .map(str::to_owned)
        .ok_or_else(|| "the Audio Clip channel has no sample path".to_owned())
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

fn edit_pattern_time_marker_file(
    input: &Path,
    output: &Path,
    pattern_id: u16,
    marker_index: Option<usize>,
    position: Option<u32>,
    name: Option<&str>,
) -> Result<(), String> {
    let (_, mut document) = load_document(input)?;
    match (marker_index, position, name) {
        (None, Some(position), Some(name)) => {
            document
                .create_pattern_time_marker(pattern_id, position, name)
                .map_err(|error| error.to_string())?;
        }
        (Some(index), Some(position), Some(name)) => {
            document
                .edit_pattern_time_marker(pattern_id, index, position, name)
                .map_err(|error| error.to_string())?;
        }
        (Some(index), None, None) => {
            document
                .delete_pattern_time_marker(pattern_id, index)
                .map_err(|error| error.to_string())?;
        }
        _ => return Err("invalid Pattern time marker operation".to_owned()),
    }
    let bytes = document
        .encode_lossless()
        .map_err(|error| format!("could not encode {}: {error}", input.display()))?;
    fs::write(output, bytes)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!(
        "updated Pattern {pattern_id} time markers in {}",
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
    use super::{
        parse_automation_interpolation_mode, parse_clip_indices, parse_midi_channel_filter,
        parse_midi_channel_mask, parse_midi_channel_number, parse_midi_import_options,
        parse_midi_import_options_with_channels, parse_midi_import_options_with_mixer_tracks,
        parse_midi_pattern_target, parse_midi_track_selection, parse_mixer_eq_parameter_kind,
        parse_mixer_track, parse_raw_byte, parse_rgb_hex, parse_sampler_envelope_lfo_target,
        parse_scale_name, pattern_color_label, playlist_pattern_color_label,
    };
    use flp_rebuild::{
        AutomationInterpolationMode, MixerParameterKind, SamplerEnvelopeLfoTarget,
        midi::MidiImportOptions,
    };

    #[test]
    fn parses_automation_interpolation_modes() {
        assert_eq!(
            parse_automation_interpolation_mode("linear"),
            Ok(AutomationInterpolationMode::Linear)
        );
        assert_eq!(
            parse_automation_interpolation_mode("smooth"),
            Ok(AutomationInterpolationMode::Smooth)
        );
        assert!(parse_automation_interpolation_mode("ease").is_err());
    }

    #[test]
    fn parses_mixer_eq_parameter_kinds() {
        for (name, expected) in [
            ("low-gain", MixerParameterKind::LowEqGain),
            ("mid-gain", MixerParameterKind::MidEqGain),
            ("high-gain", MixerParameterKind::HighEqGain),
            ("low-frequency", MixerParameterKind::LowEqFrequency),
            ("mid-frequency", MixerParameterKind::MidEqFrequency),
            ("high-frequency", MixerParameterKind::HighEqFrequency),
            ("low-q", MixerParameterKind::LowEqQ),
            ("mid-q", MixerParameterKind::MidEqQ),
            ("high-q", MixerParameterKind::HighEqQ),
        ] {
            assert_eq!(parse_mixer_eq_parameter_kind(name), Ok(expected));
        }
        assert!(parse_mixer_eq_parameter_kind("frequency").is_err());
    }

    #[test]
    fn parses_sampler_envelope_lfo_targets() {
        assert_eq!(
            parse_sampler_envelope_lfo_target("panning"),
            Ok(SamplerEnvelopeLfoTarget::Panning)
        );
        assert_eq!(
            parse_sampler_envelope_lfo_target("PAN"),
            Ok(SamplerEnvelopeLfoTarget::Panning)
        );
        assert_eq!(
            parse_sampler_envelope_lfo_target("volume"),
            Ok(SamplerEnvelopeLfoTarget::Volume)
        );
        assert_eq!(
            parse_sampler_envelope_lfo_target("mod-x"),
            Ok(SamplerEnvelopeLfoTarget::ModX)
        );
        assert_eq!(
            parse_sampler_envelope_lfo_target("mod-y"),
            Ok(SamplerEnvelopeLfoTarget::ModY)
        );
        assert_eq!(
            parse_sampler_envelope_lfo_target("pitch"),
            Ok(SamplerEnvelopeLfoTarget::Pitch)
        );
        assert!(parse_sampler_envelope_lfo_target("filter").is_err());
    }

    #[test]
    fn parses_midi_import_options() {
        assert_eq!(
            parse_midi_import_options("1", "0"),
            Ok(MidiImportOptions {
                zero_velocity_note_ons_as_notes: true,
                realign_events: false,
                midi_channel_mask: u16::MAX,
                set_mixer_tracks_for_new_channels: false,
            })
        );
        assert_eq!(
            parse_midi_import_options("0", "1"),
            Ok(MidiImportOptions {
                zero_velocity_note_ons_as_notes: false,
                realign_events: true,
                midi_channel_mask: u16::MAX,
                set_mixer_tracks_for_new_channels: false,
            })
        );
        assert!(parse_midi_import_options("0", "2").is_err());
        assert_eq!(
            parse_midi_import_options_with_channels("0", "1", "2,3"),
            Ok(MidiImportOptions {
                zero_velocity_note_ons_as_notes: false,
                realign_events: true,
                midi_channel_mask: (1u16 << 1) | (1u16 << 2),
                set_mixer_tracks_for_new_channels: false,
            })
        );
        assert_eq!(
            parse_midi_import_options_with_mixer_tracks("0", "0", "all", "1"),
            Ok(MidiImportOptions {
                set_mixer_tracks_for_new_channels: true,
                ..MidiImportOptions::default()
            })
        );
        assert!(parse_midi_import_options_with_mixer_tracks("0", "0", "all", "2").is_err());
    }

    #[test]
    fn parses_midi_channel_masks() {
        assert_eq!(parse_midi_channel_mask("all"), Ok(u16::MAX));
        assert_eq!(parse_midi_channel_mask("none"), Ok(0));
        assert_eq!(parse_midi_channel_mask("1, 3,16"), Ok(0x8005));
        assert!(parse_midi_channel_mask("0").is_err());
        assert!(parse_midi_channel_mask("17").is_err());
        assert!(parse_midi_channel_mask("2,,3").is_err());
    }

    #[test]
    fn parses_a_single_midi_channel_filter_or_all() {
        assert_eq!(parse_midi_channel_filter("all"), Ok(None));
        assert_eq!(parse_midi_channel_filter("1"), Ok(Some(0)));
        assert_eq!(parse_midi_channel_filter("16"), Ok(Some(15)));
        assert!(parse_midi_channel_filter("0").is_err());
        assert!(parse_midi_channel_filter("17").is_err());
        assert!(parse_midi_channel_filter("1,2").is_err());
    }

    #[test]
    fn parses_required_one_based_midi_channels() {
        assert_eq!(parse_midi_channel_number("1"), Ok(0));
        assert_eq!(parse_midi_channel_number("16"), Ok(15));
        assert!(parse_midi_channel_number("0").is_err());
        assert!(parse_midi_channel_number("17").is_err());
    }

    #[test]
    fn parses_midi_track_selections_as_one_based_unique_indices() {
        assert_eq!(parse_midi_track_selection("all", 3), Ok(vec![0, 1, 2]));
        assert_eq!(parse_midi_track_selection("1, 3", 3), Ok(vec![0, 2]));
        assert!(parse_midi_track_selection("none", 3).is_err());
        assert!(parse_midi_track_selection("0", 3).is_err());
        assert!(parse_midi_track_selection("4", 3).is_err());
        assert!(parse_midi_track_selection("2,2", 3).is_err());
        assert!(parse_midi_track_selection("1,,3", 3).is_err());
    }

    #[test]
    fn parses_existing_or_new_midi_pattern_targets() {
        assert_eq!(parse_midi_pattern_target("7"), Ok(Some(7)));
        assert_eq!(parse_midi_pattern_target("new"), Ok(None));
        assert!(parse_midi_pattern_target("-1").is_err());
        assert!(parse_midi_pattern_target("0").is_err());
    }

    #[test]
    fn parses_channel_mixer_track_assignments() {
        assert_eq!(parse_mixer_track("-1"), Ok(-1));
        assert_eq!(parse_mixer_track("0"), Ok(0));
        assert_eq!(parse_mixer_track("127"), Ok(127));
        assert!(parse_mixer_track("-2").is_err());
        assert!(parse_mixer_track("128").is_err());
    }

    #[test]
    fn parses_rgb_color_in_hex_form_with_optional_prefix() {
        assert_eq!(parse_rgb_hex("1aB2c3"), Ok([0x1A, 0xB2, 0xC3]));
        assert_eq!(parse_rgb_hex("#102030"), Ok([0x10, 0x20, 0x30]));
        assert!(parse_rgb_hex("#12345").is_err());
        assert!(parse_rgb_hex("xyzxyz").is_err());
    }

    #[test]
    fn formats_pattern_color_without_changing_reserved_alpha() {
        assert_eq!(
            pattern_color_label(Some([0x12, 0x34, 0x56, 0xA5])),
            "#123456"
        );
    }

    #[test]
    fn formats_missing_pattern_color_as_default() {
        assert_eq!(pattern_color_label(None), "default");
    }

    #[test]
    fn playlist_cli_reports_pattern_color_and_distinguishes_missing_targets() {
        let patterns = [
            flp_rebuild::Pattern {
                id: 7,
                color: Some([0x12, 0x34, 0x56, 0xA5]),
                ..flp_rebuild::Pattern::default()
            },
            flp_rebuild::Pattern {
                id: 8,
                ..flp_rebuild::Pattern::default()
            },
        ];
        assert_eq!(
            playlist_pattern_color_label(
                &flp_rebuild::PlaylistClipTarget::Pattern { id: 7 },
                Some(&patterns),
            ),
            "#123456"
        );
        assert_eq!(
            playlist_pattern_color_label(
                &flp_rebuild::PlaylistClipTarget::Pattern { id: 8 },
                Some(&patterns),
            ),
            "default"
        );
        assert_eq!(
            playlist_pattern_color_label(
                &flp_rebuild::PlaylistClipTarget::Pattern { id: 9 },
                Some(&patterns),
            ),
            "unknown"
        );
        assert_eq!(
            playlist_pattern_color_label(
                &flp_rebuild::PlaylistClipTarget::Channel { id: 1 },
                Some(&patterns),
            ),
            "none"
        );
        assert_eq!(
            playlist_pattern_color_label(&flp_rebuild::PlaylistClipTarget::Pattern { id: 7 }, None,),
            "unavailable"
        );
    }

    #[test]
    fn parses_raw_pattern_controller_bytes_as_decimal_or_hex() {
        assert_eq!(parse_raw_byte("145", "flags"), Ok(145));
        assert_eq!(parse_raw_byte("0x91", "flags"), Ok(145));
        assert_eq!(parse_raw_byte("0XFF", "channel"), Ok(255));
        assert!(parse_raw_byte("256", "channel").is_err());
        assert!(parse_raw_byte("0xGG", "flags").is_err());
    }

    #[test]
    fn parses_comma_separated_playlist_clip_indexes() {
        assert_eq!(parse_clip_indices("2, 0,7"), Ok(vec![2, 0, 7]));
        assert!(parse_clip_indices("1").is_err());
        assert!(parse_clip_indices("1,,2").is_err());
        assert!(parse_clip_indices("one,2").is_err());
    }

    #[test]
    fn parses_diatonic_modes_for_scale_constrained_randomization() {
        assert_eq!(
            parse_scale_name("double-harmonic-major"),
            Ok(&[0, 1, 4, 5, 7, 8, 11][..])
        );
        assert_eq!(
            parse_scale_name("ionian-sharp-5"),
            Ok(&[0, 2, 4, 5, 8, 9, 11][..])
        );
        assert_eq!(
            parse_scale_name("ionian-augmented-sharp-2"),
            Ok(&[0, 3, 4, 5, 8, 9, 11][..])
        );
        assert_eq!(parse_scale_name("dorian"), Ok(&[0, 2, 3, 5, 7, 9, 10][..]));
        assert_eq!(
            parse_scale_name("ukrainian-dorian-b2"),
            Ok(&[0, 1, 3, 6, 7, 9, 10][..])
        );
        assert_eq!(
            parse_scale_name("harmonic-minor-b5"),
            Ok(&[0, 2, 3, 5, 6, 8, 11][..])
        );
        assert_eq!(
            parse_scale_name("melodic-minor-sharp-5"),
            Ok(&[0, 2, 3, 5, 8, 9, 11][..])
        );
        assert_eq!(
            parse_scale_name("dorian-sharp-4"),
            Ok(&[0, 2, 3, 6, 7, 9, 10][..])
        );
        assert_eq!(
            parse_scale_name("dorian-b5"),
            Ok(&[0, 2, 3, 5, 6, 9, 10][..])
        );
        assert_eq!(
            parse_scale_name("phrygian"),
            Ok(&[0, 1, 3, 5, 7, 8, 10][..])
        );
        assert_eq!(
            parse_scale_name("phrygian-b4"),
            Ok(&[0, 1, 3, 4, 7, 8, 10][..])
        );
        assert_eq!(
            parse_scale_name("ultraphrygian"),
            Ok(&[0, 1, 3, 4, 7, 8, 9][..])
        );
        assert_eq!(parse_scale_name("lydian"), Ok(&[0, 2, 4, 6, 7, 9, 11][..]));
        assert_eq!(
            parse_scale_name("lydian-sharp-2"),
            Ok(&[0, 3, 4, 6, 7, 9, 11][..])
        );
        assert_eq!(
            parse_scale_name("lydian-sharp-2-sharp-6"),
            Ok(&[0, 3, 4, 6, 7, 10, 11][..])
        );
        assert_eq!(
            parse_scale_name("lydian-b3"),
            Ok(&[0, 2, 3, 6, 7, 9, 11][..])
        );
        assert_eq!(
            parse_scale_name("lydian-augmented-sharp-2"),
            Ok(&[0, 3, 4, 6, 8, 9, 11][..])
        );
        assert_eq!(
            parse_scale_name("lydian-augmented-sharp-3"),
            Ok(&[0, 2, 5, 6, 8, 9, 11][..])
        );
        assert_eq!(
            parse_scale_name("mixolydian"),
            Ok(&[0, 2, 4, 5, 7, 9, 10][..])
        );
        assert_eq!(
            parse_scale_name("mixolydian-b2"),
            Ok(&[0, 1, 4, 5, 7, 9, 10][..])
        );
        assert_eq!(parse_scale_name("locrian"), Ok(&[0, 1, 3, 5, 6, 8, 10][..]));
        assert_eq!(
            parse_scale_name("locrian-natural-2"),
            Ok(&[0, 2, 3, 5, 6, 8, 10][..])
        );
        assert_eq!(
            parse_scale_name("locrian-natural-6"),
            Ok(&[0, 1, 3, 5, 6, 9, 10][..])
        );
        assert_eq!(
            parse_scale_name("locrian-bb7"),
            Ok(&[0, 1, 3, 5, 6, 8, 9][..])
        );
        assert_eq!(
            parse_scale_name("locrian-bb3-bb7"),
            Ok(&[0, 1, 2, 5, 6, 8, 9][..])
        );
        assert_eq!(
            parse_scale_name("melodic-minor"),
            Ok(&[0, 2, 3, 5, 7, 9, 11][..])
        );
        assert_eq!(
            parse_scale_name("neapolitan-minor"),
            Ok(&[0, 1, 3, 5, 7, 8, 11][..])
        );
        assert_eq!(
            parse_scale_name("neapolitan-major"),
            Ok(&[0, 1, 3, 5, 7, 9, 11][..])
        );
        assert_eq!(
            parse_scale_name("hungarian-minor"),
            Ok(&[0, 2, 3, 6, 7, 8, 11][..])
        );
        assert_eq!(
            parse_scale_name("hungarian-major"),
            Ok(&[0, 3, 4, 6, 7, 9, 10][..])
        );
        assert_eq!(
            parse_scale_name("hungarian-major-mode-2"),
            Ok(&[0, 1, 3, 4, 6, 7, 9][..])
        );
        assert_eq!(parse_scale_name("persian"), Ok(&[0, 1, 4, 5, 6, 8, 11][..]));
        assert_eq!(
            parse_scale_name("oriental"),
            Ok(&[0, 1, 4, 5, 6, 9, 10][..])
        );
        assert_eq!(parse_scale_name("blues"), Ok(&[0, 3, 5, 6, 7, 10][..]));
        assert_eq!(parse_scale_name("whole-tone"), Ok(&[0, 2, 4, 6, 8, 10][..]));
        assert_eq!(
            parse_scale_name("whole-half-diminished"),
            Ok(&[0, 2, 3, 5, 6, 8, 9, 11][..])
        );
        assert_eq!(
            parse_scale_name("half-whole-diminished"),
            Ok(&[0, 1, 3, 4, 6, 7, 9, 10][..])
        );
        assert_eq!(
            parse_scale_name("super-locrian-bb7"),
            Ok(&[0, 1, 3, 4, 6, 8, 9][..])
        );
        assert_eq!(
            parse_scale_name("altered-dominant-natural-6"),
            Ok(&[0, 1, 3, 4, 6, 9, 10][..])
        );
        assert!(parse_scale_name("unknown").is_err());
    }
}
