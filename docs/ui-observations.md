# FL Studio desktop interface observations

These observations come from the installed FL Studio 2026 desktop application and a loaded project. They record visible organization and navigation behavior for the clean-room rewrite; they do not define pixel measurements or claim full interface parity.

## Main window

- The main window fills the display and keeps the project name, transport, tempo, elapsed time, and view controls in a narrow top area.
- The top-level menu contains File, Edit, Add, Patterns, View, Options, Tools, and Help.
- Keyboard shortcuts open the Playlist (F5), Channel Rack (F6), Piano roll (F7), and Mixer (F9).
- The Browser occupies a narrow left column. The work area to its right changes between editor views.

## Channel Rack

- Each channel row combines enable/mute and pan controls, a mixer assignment, a colored channel name, and a step-sequencer lane.
- Instrument, sample, automation, and pattern-linked channels share the rack, while row controls and color vary by channel type.
- A channel's instrument or effect opens in its own floating window; the project observed here had an instrument window over the Channel Rack.

## Playlist

- The Playlist has a timeline ruler, a track-name column, colored clips, and vertical and horizontal scrolling.
- Pattern, audio, automation, and tempo content appear on separate tracks. Track names and clip names can differ.
- The arrangement selector and Playlist tools live in the view toolbar above the timeline.
- The observed project used colored track groups and displayed a long arrangement across more than one hundred bars.

## Piano roll

- The left piano keyboard aligns note pitch with a bar-and-beat grid.
- Notes are editable blocks on the grid, with a separate lower control lane for velocity and other note properties.
- The toolbar above the grid includes drawing, selection, snapping, and editing tools; the selected channel name appears beside the Piano roll label.

## Mixer

- Mixer inserts are displayed as vertical strips with color, names, pan controls, volume faders, meters, and routing controls.
- The selected insert exposes ten effect slots in a right-side panel, with equalizer controls below.
- The observed project used many named, color-coded inserts and several effects per insert.

## Current rewrite gap

The Rust shell currently places the Browser and one editor view inside a single application window. Its Playlist, Channel Rack, Piano roll, and Mixer are simplified drawings, and the Mixer state and effect slots are not decoded yet. The native FL plug-in windows, floating editor windows, clip tools, menus, transport behavior, channel controls, and project-level plug-in restoration still need reverse engineering and implementation.
