#!/usr/bin/env python3
"""One-shot Edison script runner for fl-studio-rebuild.

Executes a single `.pyscript` file against a sample buffer, following the
workflow in Image-Line's FL Studio manual (Edison > Tools > Scripting)
and the `enveditor` module documented by Image-Line's FL Studio API
stubs (il-group.github.io/FL-Studio-API-Stubs, Edison scripting):

* Scripts run top to bottom with `__name__ == "__main__"` and use
  `from enveditor import *` to reach `EditorSample`, `Editor`, `Utils`,
  `ScriptDialog` and `Sample`.
* `ScriptDialog` collects knob/combo inputs. Because this runner is a
  one-shot child process, dialog handling is two-phase: on the first run
  `Execute()` aborts the script and reports the dialog spec to the host,
  which shows the dialog natively; on the second run the host passes the
  chosen values back and `Execute()` returns `True` immediately.
* Errors abort the run and the traceback is reported to the host, which
  shows it in an error dialog (the manual: "a dialog is shown explaining
  the error").

Sample data is exchanged as raw little-endian f32, channel-major, in the
files named by the job JSON passed as argv[1].
"""

import json
import math
import sys
import traceback
import types
from array import array


class _DialogRequested(Exception):
    """Raised by ScriptDialog.Execute() when the host must ask the user."""

    def __init__(self, dialog):
        super().__init__("dialog requested")
        self.dialog = dialog


class _RunState:
    def __init__(self):
        self.messages = []
        self.progress = None
        self.dialog_values = None


_STATE = _RunState()


class Region:
    """A region or marker within a sample (enveditor.Region)."""

    def __init__(self, start, end):
        self.SampleStart = start
        self.SampleEnd = end


class Sample:
    """An editable audio clip (enveditor.Sample).

    Only the surface documented in Image-Line's API stubs is provided.
    Ranges are inclusive (`start`..`end`), matching the stubs, where a
    selection of `0`..`Length - 1` means the full clip.
    """

    def __init__(self, channels=None, sample_rate=44100):
        self._channels = channels if channels is not None else []
        self._sample_rate = int(sample_rate)
        self._regions = []

    # -- properties -----------------------------------------------------

    @property
    def Length(self):
        if not self._channels:
            return 0
        return len(self._channels[0])

    @property
    def NumChans(self):
        return len(self._channels)

    @NumChans.setter
    def NumChans(self, value):
        value = int(value)
        if value < 0:
            raise ValueError("NumChans cannot be negative")
        length = self.Length
        while len(self._channels) > value:
            self._channels.pop()
        while len(self._channels) < value:
            self._channels.append(array("f", bytes(4 * length)))

    @property
    def SampleRate(self):
        return self._sample_rate

    @SampleRate.setter
    def SampleRate(self, value):
        value = int(value)
        if value <= 0:
            raise ValueError("SampleRate must be positive")
        self._sample_rate = value

    @property
    def RegionCount(self):
        return len(self._regions)

    # -- helpers --------------------------------------------------------

    def _checked_range(self, start, end):
        length = self.Length
        if length == 0:
            raise ValueError("sample is empty")
        start = max(0, min(int(start), length - 1))
        end = max(start, min(int(end), length - 1))
        return start, end

    def _checked_position(self, position, channel):
        position = int(position)
        channel = int(channel)
        if not 0 <= channel < len(self._channels):
            raise IndexError(f"channel {channel} is out of range")
        if not 0 <= position < self.Length:
            raise IndexError(f"position {position} is out of range")
        return position, channel

    # -- documented sample access ---------------------------------------

    def GetSampleAt(self, position, channel):
        position, channel = self._checked_position(position, channel)
        return float(self._channels[channel][position])

    def SetSampleAt(self, position, channel, value):
        position, channel = self._checked_position(position, channel)
        self._channels[channel][position] = float(value)

    def AmpFromTo(self, start, end, volume):
        start, end = self._checked_range(start, end)
        volume = float(volume)
        for channel in self._channels:
            for index in range(start, end + 1):
                channel[index] *= volume

    def SilenceFromTo(self, start, end, volume):
        # The stubs document that every value in the range is set to 0.0;
        # the volume argument is accepted for signature compatibility.
        del volume
        start, end = self._checked_range(start, end)
        for channel in self._channels:
            for index in range(start, end + 1):
                channel[index] = 0.0

    def NormalizeFromTo(self, start, end, volume, only_if_above=False):
        # Interpretation, from the stubs and Image-Line's bundled-style
        # scripts: scale the range jointly across channels so its peak
        # magnitude becomes `volume`; with `only_if_above`, only apply
        # when the current peak is above `volume` (never boost).
        start, end = self._checked_range(start, end)
        volume = float(volume)
        peak = 0.0
        for channel in self._channels:
            for index in range(start, end + 1):
                peak = max(peak, abs(channel[index]))
        if peak == 0.0:
            return
        if only_if_above and peak <= volume:
            return
        scale = volume / peak
        for channel in self._channels:
            for index in range(start, end + 1):
                channel[index] *= scale

    def CenterFromTo(self, start, end):
        # Remove the DC offset of each channel over the range. Verified
        # against real-world scripts, which call this for a
        # "Remove DC Offset?" option; the stubs site does not list it.
        start, end = self._checked_range(start, end)
        count = end - start + 1
        for channel in self._channels:
            mean = sum(channel[start : end + 1]) / count
            for index in range(start, end + 1):
                channel[index] -= mean

    def SineFromTo(self, start, end, frequency, phase, volume=1.0):
        # Interpretation: `phase` is a radian offset and the wave is
        # generated relative to `start`, replacing the range content on
        # every channel.
        start, end = self._checked_range(start, end)
        frequency = float(frequency)
        phase = float(phase)
        volume = float(volume)
        for channel in self._channels:
            for index in range(start, end + 1):
                offset = (index - start) / self._sample_rate
                channel[index] = volume * math.sin(
                    phase + 2.0 * math.pi * frequency * offset
                )

    def PasteFromTo(self, old, start, end, mode):
        # Copy frames from `old` into this sample at `start`. The copied
        # count is the source length, bounded by the `start`..`end`
        # range. Modes per the stubs: 0 Insert, 1 Replace, 2 Mix.
        if not isinstance(old, Sample):
            raise TypeError("PasteFromTo source must be a Sample")
        if old.Length == 0:
            return
        start = max(0, int(start))
        end = int(end)
        count = old.Length if end < start else min(old.Length, end - start + 1)
        if count <= 0:
            return
        self.NumChans = max(self.NumChans, old.NumChans)
        for channel_index in range(old.NumChans):
            target = self._channels[channel_index]
            source = old._channels[channel_index]
            if mode == 0:
                for offset in range(count):
                    target.insert(start + offset, source[offset])
            elif mode in (1, 2):
                while len(target) < start + count:
                    target.append(0.0)
                for offset in range(count):
                    if mode == 1:
                        target[start + offset] = source[offset]
                    else:
                        target[start + offset] += source[offset]
            else:
                raise ValueError(f"unknown PasteFromTo mode {mode}")

    def MsToSamples(self, time):
        return int(float(time) * self._sample_rate / 1000.0)

    def NormalizeFormat(self, source, mode=7):
        # Copy properties from `source` per the bit mask in the stubs:
        # 0b001 channels, 0b010 format (a no-op here: all buffers are
        # f32), 0b100 sample rate.
        if mode & 0b001:
            self.NumChans = source.NumChans
        if mode & 0b100:
            self._sample_rate = source.SampleRate

    def GetRegion(self, index):
        return self._regions[int(index)]

    def LoadFromClipboard(self):
        raise RuntimeError(
            "LoadFromClipboard is not available in this script host"
        )


class MEEditor:
    """The Edison editor state (enveditor.Editor)."""

    def __init__(self, selection_start, selection_end):
        self._selection_start = int(selection_start)
        self._selection_end = int(selection_end)

    @property
    def SelectionStartS(self):
        return self._selection_start

    @property
    def SelectionEndS(self):
        return self._selection_end


class ScriptDialog:
    """A script input dialog (enveditor.ScriptDialog)."""

    def __init__(self, title, description):
        self._title = str(title)
        self._description = str(description)
        self._inputs = []
        self._resolved = {}

    def _add_input(self, entry):
        self._inputs.append(entry)
        self._resolved[entry["name"]] = entry["default"]

    def AddInput(self, name, default_value):
        default = min(1.0, max(0.0, float(default_value)))
        self._add_input(
            {
                "name": str(name),
                "kind": "value",
                "default": default,
                "min": 0.0,
                "max": 1.0,
                "options": [],
            }
        )

    def AddInputKnob(self, name, default_value, min_value, max_value):
        self._add_input(
            {
                "name": str(name),
                "kind": "knob",
                "default": float(default_value),
                "min": float(min_value),
                "max": float(max_value),
                "options": [],
            }
        )

    def AddInputCombo(self, name, values, default_value):
        options = [option for option in str(values).split(",")]
        default = float(int(default_value))
        self._add_input(
            {
                "name": str(name),
                "kind": "combo",
                "default": default,
                "min": 0.0,
                "max": float(max(0, len(options) - 1)),
                "options": options,
            }
        )

    def Execute(self):
        if _STATE.dialog_values is None:
            raise _DialogRequested(self)
        for entry in self._inputs:
            provided = _STATE.dialog_values.get(entry["name"])
            if provided is not None:
                self._resolved[entry["name"]] = float(provided)
        return True

    def GetInputValue(self, name):
        return float(self._resolved[str(name)])

    def spec(self):
        return {
            "title": self._title,
            "description": self._description,
            "inputs": self._inputs,
        }


class TUtils:
    """Utility functions exposed to scripts (enveditor.Utils)."""

    def ProgressMsg(self, message, position, total):
        _STATE.progress = {
            "message": str(message),
            "position": int(position),
            "total": int(total),
        }

    def ShowMessage(self, message):
        _STATE.messages.append(str(message))


def _read_channels(path, num_chans, length):
    channels = []
    with open(path, "rb") as handle:
        data = handle.read()
    expected = 4 * num_chans * length
    if len(data) != expected:
        raise ValueError(
            f"sample buffer holds {len(data)} bytes, expected {expected}"
        )
    for channel_index in range(num_chans):
        chunk = array("f")
        offset = 4 * length * channel_index
        chunk.frombytes(data[offset : offset + 4 * length])
        if sys.byteorder != "little":
            chunk.byteswap()
        channels.append(chunk)
    return channels


def _write_channels(path, channels):
    with open(path, "wb") as handle:
        for channel in channels:
            data = array("f", channel)
            if sys.byteorder != "little":
                data.byteswap()
            handle.write(data.tobytes())


def main(argv):
    with open(argv[1], "r", encoding="utf-8") as handle:
        job = json.load(handle)

    channels = _read_channels(
        job["sample_file"], int(job["num_chans"]), int(job["length"])
    )
    editor_sample = Sample(channels, int(job["sample_rate"]))
    editor = MEEditor(job["selection_start"], job["selection_end"])
    _STATE.dialog_values = job.get("dialog_values")

    enveditor = types.ModuleType("enveditor")
    enveditor.Sample = Sample
    enveditor.Region = Region
    enveditor.ScriptDialog = ScriptDialog
    enveditor.MEEditor = MEEditor
    enveditor.EditorSample = editor_sample
    enveditor.Editor = editor
    enveditor.Utils = TUtils()
    sys.modules["enveditor"] = enveditor

    with open(job["script_path"], "r", encoding="utf-8") as handle:
        source = handle.read()

    script_globals = {
        "__name__": "__main__",
        "__file__": job["script_path"],
        "Sample": Sample,
        "Region": Region,
        "ScriptDialog": ScriptDialog,
        "EditorSample": editor_sample,
        "Editor": editor,
        "Utils": enveditor.Utils,
    }

    result = {"messages": [], "progress": None}
    try:
        code = compile(source, job["script_path"], "exec")
        exec(code, script_globals)
        result["status"] = "completed"
    except _DialogRequested as request:
        result["status"] = "dialog"
        result["dialog"] = request.dialog.spec()
    except Exception:
        result["status"] = "error"
        result["error"] = traceback.format_exc()
    result["messages"] = _STATE.messages
    result["progress"] = _STATE.progress

    if result["status"] == "completed":
        _write_channels(job["out_sample_file"], editor_sample._channels)
        result["sample_rate"] = editor_sample.SampleRate
        result["num_chans"] = editor_sample.NumChans
        result["length"] = editor_sample.Length

    with open(job["result_file"], "w", encoding="utf-8") as handle:
        json.dump(result, handle)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
