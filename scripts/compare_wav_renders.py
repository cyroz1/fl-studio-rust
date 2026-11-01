#!/usr/bin/env python3
"""Compare aligned native and rebuilt WAV renders without third-party packages."""

from __future__ import annotations

import argparse
import json
import math
import struct
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator


@dataclass(frozen=True)
class WaveFormat:
    sample_rate: int
    channels: int
    block_align: int
    bits_per_sample: int
    format_code: int


class WaveReader:
    def __init__(self, path: Path):
        self.path = path
        self.format, self.data_chunks = self._read_header()
        self.frames = sum(size for _, size in self.data_chunks) // self.format.block_align

    def _read_header(self) -> tuple[WaveFormat, list[tuple[int, int]]]:
        data_chunks: list[tuple[int, int]] = []
        fmt_chunk: bytes | None = None
        with self.path.open("rb") as source:
            header = source.read(12)
            if len(header) != 12 or header[:4] != b"RIFF" or header[8:] != b"WAVE":
                raise ValueError(f"{self.path}: expected a little-endian RIFF/WAVE file")
            riff_size = struct.unpack_from("<I", header, 4)[0]
            riff_end = min(8 + riff_size, self.path.stat().st_size)
            while source.tell() + 8 <= riff_end:
                chunk_header = source.read(8)
                if len(chunk_header) != 8:
                    break
                chunk_id, chunk_size = chunk_header[:4], struct.unpack_from("<I", chunk_header, 4)[0]
                chunk_offset = source.tell()
                if chunk_offset + chunk_size > self.path.stat().st_size:
                    raise ValueError(f"{self.path}: truncated {chunk_id!r} chunk")
                if chunk_id == b"fmt ":
                    fmt_chunk = source.read(chunk_size)
                elif chunk_id == b"data":
                    data_chunks.append((chunk_offset, chunk_size))
                source.seek(chunk_offset + chunk_size + (chunk_size & 1))

        if fmt_chunk is None or len(fmt_chunk) < 16:
            raise ValueError(f"{self.path}: missing or incomplete WAVE format chunk")
        format_code, channels, sample_rate, _, block_align, bits = struct.unpack_from(
            "<HHIIHH", fmt_chunk
        )
        if format_code == 0xFFFE:
            if len(fmt_chunk) < 40:
                raise ValueError(f"{self.path}: incomplete WAVE_FORMAT_EXTENSIBLE chunk")
            format_code = struct.unpack_from("<I", fmt_chunk, 24)[0]
        if format_code not in (1, 3):
            raise ValueError(f"{self.path}: only PCM and IEEE-float WAV files are supported")
        if channels not in (1, 2):
            raise ValueError(f"{self.path}: expected mono or stereo audio, found {channels} channels")
        if sample_rate <= 0 or bits not in (8, 16, 24, 32, 64):
            raise ValueError(f"{self.path}: unsupported sample rate or sample width")
        if format_code == 3 and bits not in (32, 64):
            raise ValueError(f"{self.path}: IEEE-float WAV must use 32- or 64-bit samples")
        if format_code == 1 and bits == 64:
            raise ValueError(f"{self.path}: PCM WAV must use 8-, 16-, 24-, or 32-bit samples")
        expected_align = channels * (bits // 8)
        if block_align != expected_align:
            raise ValueError(f"{self.path}: inconsistent WAVE block alignment")
        if not data_chunks:
            raise ValueError(f"{self.path}: missing WAVE data chunk")
        if any(size % block_align for _, size in data_chunks):
            raise ValueError(f"{self.path}: WAVE data ends with a partial sample frame")
        return WaveFormat(sample_rate, channels, block_align, bits, format_code), data_chunks

    def frames_from(self, start_frame: int, frame_count: int) -> Iterator[tuple[float, ...]]:
        """Yield a bounded frame range while decoding the WAV in small blocks."""
        sample_width = self.format.bits_per_sample // 8
        frames_to_skip = start_frame
        frames_to_read = frame_count
        chunk_frames = max(1, 65_536 // self.format.block_align)
        with self.path.open("rb") as source:
            for chunk_offset, chunk_size in self.data_chunks:
                available = chunk_size // self.format.block_align
                if frames_to_skip >= available:
                    frames_to_skip -= available
                    continue
                source.seek(chunk_offset + frames_to_skip * self.format.block_align)
                available -= frames_to_skip
                frames_to_skip = 0
                take = min(available, frames_to_read)
                while take:
                    count = min(take, chunk_frames)
                    raw = source.read(count * self.format.block_align)
                    if len(raw) != count * self.format.block_align:
                        raise ValueError(f"{self.path}: truncated WAVE sample data")
                    samples = self._decode_samples(raw, sample_width)
                    for frame_index in range(count):
                        begin = frame_index * self.format.channels
                        yield tuple(samples[begin : begin + self.format.channels])
                    take -= count
                    frames_to_read -= count
                if frames_to_read == 0:
                    return

    def _decode_samples(self, raw: bytes, sample_width: int) -> list[float]:
        bits = self.format.bits_per_sample
        code = self.format.format_code
        if code == 3:
            sample_code = "f" if bits == 32 else "d"
            values = (item[0] for item in struct.iter_unpack("<" + sample_code, raw))
        elif bits == 8:
            values = ((sample - 128) / 128.0 for sample in raw)
        elif bits in (16, 32):
            sample_code = "h" if bits == 16 else "i"
            scale = float(1 << (bits - 1))
            values = (item[0] / scale for item in struct.iter_unpack("<" + sample_code, raw))
        else:
            scale = float(1 << 23)

            def decode_24() -> Iterator[float]:
                for offset in range(0, len(raw), sample_width):
                    value = int.from_bytes(raw[offset : offset + 3], "little", signed=True)
                    yield value / scale

            values = decode_24()
        decoded = list(values)
        if any(not math.isfinite(value) for value in decoded):
            raise ValueError(f"{self.path}: WAVE contains a non-finite sample")
        return decoded


class Metrics:
    def __init__(self) -> None:
        self.count = 0
        self.ref_sum = 0.0
        self.render_sum = 0.0
        self.ref_square_sum = 0.0
        self.render_square_sum = 0.0
        self.product_sum = 0.0
        self.error_square_sum = 0.0
        self.peak_error = 0.0

    def add(self, reference: float, rendered: float) -> None:
        error = rendered - reference
        self.count += 1
        self.ref_sum += reference
        self.render_sum += rendered
        self.ref_square_sum += reference * reference
        self.render_square_sum += rendered * rendered
        self.product_sum += reference * rendered
        self.error_square_sum += error * error
        self.peak_error = max(self.peak_error, abs(error))

    def best_fit_gain(self) -> float | None:
        return (
            self.product_sum / self.render_square_sum
            if self.render_square_sum > 0.0
            else None
        )

    @staticmethod
    def _dbfs(value: float) -> float | None:
        return 20.0 * math.log10(value) if value > 0.0 else None

    def result(
        self, fitted_residual_square_sum: float | None = None
    ) -> dict[str, float | int | None]:
        count = self.count
        ref_rms = math.sqrt(self.ref_square_sum / count)
        render_rms = math.sqrt(self.render_square_sum / count)
        rmse = math.sqrt(self.error_square_sum / count)
        centered_ref = max(0.0, self.ref_square_sum - self.ref_sum * self.ref_sum / count)
        centered_render = max(
            0.0, self.render_square_sum - self.render_sum * self.render_sum / count
        )
        covariance = self.product_sum - self.ref_sum * self.render_sum / count
        correlation_denominator = math.sqrt(centered_ref * centered_render)
        correlation = (
            max(-1.0, min(1.0, covariance / correlation_denominator))
            if correlation_denominator > 0.0
            else None
        )
        gain = self.best_fit_gain()
        fitted_error = (
            math.sqrt(fitted_residual_square_sum / count)
            if fitted_residual_square_sum is not None
            else None
        )
        return {
            "sample_count": count,
            "reference_rms_dbfs": self._dbfs(ref_rms),
            "rendered_rms_dbfs": self._dbfs(render_rms),
            "rmse_dbfs": self._dbfs(rmse),
            "peak_error_dbfs": self._dbfs(self.peak_error),
            "correlation": correlation,
            "best_fit_render_gain": gain,
            "best_fit_residual_rmse_dbfs": (
                self._dbfs(fitted_error) if fitted_error is not None else None
            ),
        }


def compare_files(
    reference_path: Path,
    rendered_path: Path,
    offset_frames: int,
    skip_start_ms: float,
    skip_end_ms: float,
) -> dict[str, object]:
    reference = WaveReader(reference_path)
    rendered = WaveReader(rendered_path)
    if reference.format.sample_rate != rendered.format.sample_rate:
        raise ValueError(
            f"sample rates differ ({reference.format.sample_rate} vs "
            f"{rendered.format.sample_rate} Hz)"
        )
    if reference.format.channels != rendered.format.channels:
        raise ValueError(
            f"channel counts differ ({reference.format.channels} vs {rendered.format.channels})"
        )

    sample_rate = reference.format.sample_rate
    skip_start = round(skip_start_ms * sample_rate / 1000.0)
    skip_end = round(skip_end_ms * sample_rate / 1000.0)
    reference_start = skip_start + max(0, -offset_frames)
    rendered_start = skip_start + max(0, offset_frames)
    available = min(
        reference.frames - reference_start,
        rendered.frames - rendered_start,
    )
    compare_frames = available - skip_end
    if compare_frames <= 0:
        raise ValueError("the selected offset and skipped edges leave no frames to compare")

    channel_metrics = [Metrics() for _ in range(reference.format.channels)]
    overall = Metrics()
    reference_frames = reference.frames_from(reference_start, compare_frames)
    rendered_frames = rendered.frames_from(rendered_start, compare_frames)
    compared = 0
    for reference_frame, rendered_frame in zip(reference_frames, rendered_frames):
        compared += 1
        for channel, (reference_sample, rendered_sample) in enumerate(
            zip(reference_frame, rendered_frame)
        ):
            channel_metrics[channel].add(reference_sample, rendered_sample)
            overall.add(reference_sample, rendered_sample)
    if compared != compare_frames:
        raise ValueError("could not read the requested comparison range")

    channel_gains = [metrics.best_fit_gain() for metrics in channel_metrics]
    overall_gain = overall.best_fit_gain()
    channel_fit_errors = [0.0 if gain is not None else None for gain in channel_gains]
    overall_fit_error = 0.0 if overall_gain is not None else None
    reference_frames = reference.frames_from(reference_start, compare_frames)
    rendered_frames = rendered.frames_from(rendered_start, compare_frames)
    for reference_frame, rendered_frame in zip(reference_frames, rendered_frames):
        for channel, (reference_sample, rendered_sample) in enumerate(
            zip(reference_frame, rendered_frame)
        ):
            gain = channel_gains[channel]
            if gain is not None:
                residual = reference_sample - rendered_sample * gain
                channel_fit_errors[channel] += residual * residual
            if overall_gain is not None:
                residual = reference_sample - rendered_sample * overall_gain
                overall_fit_error += residual * residual

    return {
        "reference": str(reference_path),
        "rendered": str(rendered_path),
        "sample_rate_hz": sample_rate,
        "channels": reference.format.channels,
        "reference_frames": reference.frames,
        "rendered_frames": rendered.frames,
        "length_delta_frames": rendered.frames - reference.frames,
        "offset_frames": offset_frames,
        "compared_frames": compared,
        "skipped_start_ms": skip_start * 1000.0 / sample_rate,
        "skipped_end_ms": skip_end * 1000.0 / sample_rate,
        "overall": overall.result(overall_fit_error),
        "per_channel": [
            {
                "channel": channel + 1,
                **metrics.result(channel_fit_errors[channel]),
            }
            for channel, metrics in enumerate(channel_metrics)
        ],
    }


def wav_files(directory: Path) -> dict[str, Path]:
    files: dict[str, Path] = {}
    for path in directory.rglob("*"):
        if path.is_file() and path.suffix.lower() == ".wav":
            key = path.relative_to(directory).as_posix().casefold()
            if key in files:
                raise ValueError(f"duplicate case-insensitive WAV path under {directory}: {key}")
            files[key] = path
    return files


def metric_summary(result: dict[str, object]) -> str:
    overall = result["overall"]
    assert isinstance(overall, dict)
    correlation = overall["correlation"]
    corr_text = "n/a" if correlation is None else f"{correlation:.8f}"
    rmse = overall["rmse_dbfs"]
    rmse_text = "-inf" if rmse is None else f"{rmse:.2f} dBFS"
    gain = overall["best_fit_render_gain"]
    gain_text = "n/a" if gain is None else f"{gain:.8f}"
    return (
        f"{result['sample_rate_hz']} Hz, {result['channels']} ch, "
        f"{result['compared_frames']} frames, length Δ {result['length_delta_frames']}; "
        f"RMSE {rmse_text}, correlation {corr_text}, best-fit render gain {gain_text}"
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Measure sample-level differences between native and rebuilt WAV renders."
    )
    parser.add_argument("reference", type=Path, help="native FL Studio WAV or stem folder")
    parser.add_argument("rendered", type=Path, help="rebuilt WAV or matching stem folder")
    parser.add_argument(
        "--offset-frames",
        type=int,
        default=0,
        help="shift the rebuilt render before comparison; positive means it starts later",
    )
    parser.add_argument(
        "--skip-start-ms", type=float, default=0.0, help="skip this shared start interval"
    )
    parser.add_argument(
        "--skip-end-ms", type=float, default=0.0, help="skip this shared end interval"
    )
    parser.add_argument("--json", action="store_true", help="print machine-readable JSON")
    args = parser.parse_args()
    if args.skip_start_ms < 0 or args.skip_end_ms < 0:
        parser.error("skip intervals must be nonnegative")
    if not math.isfinite(args.skip_start_ms) or not math.isfinite(args.skip_end_ms):
        parser.error("skip intervals must be finite")
    if not args.reference.exists() or not args.rendered.exists():
        parser.error("both comparison paths must exist")
    if args.reference.is_dir() != args.rendered.is_dir():
        parser.error("reference and rendered paths must both be WAV files or both be folders")

    pairs: list[tuple[str, Path, Path]] = []
    missing: list[str] = []
    if args.reference.is_dir():
        reference_files = wav_files(args.reference)
        rendered_files = wav_files(args.rendered)
        for key in sorted(reference_files.keys() | rendered_files.keys()):
            if key in reference_files and key in rendered_files:
                pairs.append((key, reference_files[key], rendered_files[key]))
            elif key in reference_files:
                missing.append(f"missing rendered stem: {key}")
            else:
                missing.append(f"missing reference stem: {key}")
    else:
        pairs.append((args.reference.name, args.reference, args.rendered))
    if not pairs:
        parser.error("no matching WAV files were found")

    results: list[dict[str, object]] = []
    errors: list[dict[str, str]] = []
    for label, reference_path, rendered_path in pairs:
        try:
            result = compare_files(
                reference_path,
                rendered_path,
                args.offset_frames,
                args.skip_start_ms,
                args.skip_end_ms,
            )
            result["label"] = label
            results.append(result)
        except (OSError, ValueError, struct.error) as error:
            errors.append({"label": label, "error": str(error)})

    if args.json:
        print(json.dumps({"results": results, "missing": missing, "errors": errors}, indent=2))
    else:
        for result in results:
            print(f"{result['label']}: {metric_summary(result)}")
        for item in missing:
            print(f"MISSING: {item}", file=sys.stderr)
        for item in errors:
            print(f"ERROR: {item['label']}: {item['error']}", file=sys.stderr)

    return 1 if missing or errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
