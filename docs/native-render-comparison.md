# Native render comparison

The WAV comparator reports sample-level differences between a native FL Studio render and a
render from this project. It does not establish that a Mixer mapping is correct by itself;
comparisons need paired renders from the same project revision and controlled render settings.
No native reference WAVs are currently stored in this repository, so the Mixer mappings remain
unverified against native audio.

## Prepare a comparable pair

1. Save a copy of the project and record its FL Studio version. Keep the project, samples,
   plug-in versions, and plug-in states identical for both renders.
2. Render the same Pattern, song range, or Mixer stems from FL Studio and from the rebuilt app.
   Match sample rate, channel count, render range, and tail length. Use 32-bit float WAV with
   normalization and dither disabled where those options are available.
3. Save each render beside a short note recording the project revision, render settings, and
   the Mixer control being investigated. For a stem folder, use matching relative WAV names.
4. Compare the resulting WAV file or folders:

   ```sh
   python3 scripts/compare_wav_renders.py native.wav rebuilt.wav --json
   python3 scripts/compare_wav_renders.py native-stems rebuilt-stems --json
   ```

The tool accepts PCM WAV at 8, 16, 24, or 32 bits and IEEE-float WAV at 32 or 64 bits, including
WAVE_FORMAT_EXTENSIBLE PCM/float files. It requires matching sample rates and mono/stereo channel
counts. Folder mode pairs `.wav` files by case-insensitive relative path and reports any missing
counterpart. A length difference is reported while metrics are computed over the common selected
range.

## Alignment and metrics

By default, both files are compared from frame zero. If the rebuilt file begins later than the
native reference, `--offset-frames N` starts reading the rebuilt file at frame `N`; a negative
value starts the native file later. For example, `--offset-frames 48` aligns a rebuilt render
that begins 48 frames late at 48 kHz. `--skip-start-ms` and `--skip-end-ms` remove the same edge
intervals from both aligned files.

The JSON output includes reference and rebuilt RMS, raw RMSE, peak error, Pearson correlation,
and the least-squares scalar gain that best fits the rebuilt samples to the reference. The
best-fit residual RMSE helps distinguish a level difference from broader waveform differences.
The gain is a diagnostic; it does not alter either file or the raw metrics. In JSON, `null` is
used for digital silence in dBFS fields or for metrics that are undefined when a signal has no
energy or variance.

Correlation is phase- and timing-sensitive. Record any offset used, and compare both raw and
aligned results before attributing a difference to a control mapping. One render pair only
supports conclusions about that project, plug-in set, and tested control range. Store reference
pairs and their settings before changing the corresponding Mixer mapping.
