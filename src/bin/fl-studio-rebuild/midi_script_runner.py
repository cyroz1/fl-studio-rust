import json
import math
import runpy
import sys
import types


_protocol_out = sys.stdout
sys.stdout = sys.stderr
_active_transport_actions = None
_active_transport_state = {}


def _emit(value):
    _protocol_out.write(json.dumps(value, separators=(",", ":")) + "\n")
    _protocol_out.flush()


class EventData:
    def __init__(self, value):
        self.handled = False
        self.timestamp = value.get("timestamp", 0)
        self.status = value.get("status", 0)
        self.data1 = value.get("data1", 0)
        self.data2 = value.get("data2", 0)
        self.port = 0
        self.sysex = bytes(value.get("sysex", []))
        self.pmeflags = int(value.get("pmeflags", 0))
        self.isIncrement = False
        self.res = 0.0
        self.inEv = 0
        self.outEv = 0
        self.midiId = 0
        self.midiChanEx = 0

    @property
    def note(self):
        return self.data1

    @note.setter
    def note(self, value):
        self.data1 = int(value)

    @property
    def velocity(self):
        return self.data2

    @velocity.setter
    def velocity(self, value):
        self.data2 = int(value)

    @property
    def pressure(self):
        return self.data2

    @pressure.setter
    def pressure(self, value):
        self.data2 = int(value)

    @property
    def progNum(self):
        return self.data1

    @property
    def controlNum(self):
        return self.data1

    @property
    def controlVal(self):
        return self.data2

    @controlVal.setter
    def controlVal(self, value):
        self.data2 = int(value)

    @property
    def pitchBend(self):
        return self.data1 | (self.data2 << 7)

    @pitchBend.setter
    def pitchBend(self, value):
        value = int(value)
        self.data1 = value & 0x7F
        self.data2 = (value >> 7) & 0x7F

    @property
    def midiChan(self):
        return self.status & 0x0F

    @midiChan.setter
    def midiChan(self, value):
        self.status = (self.status & 0xF0) | (int(value) & 0x0F)


def _module(name):
    value = types.ModuleType(name)
    sys.modules[name] = value
    return value


def _request_transport_action(action):
    if _active_transport_actions is not None:
        _active_transport_actions.append(action)


def _bst_component(hint, mode, default):
    try:
        return int(str(hint).split(":")[mode - 3])
    except (IndexError, TypeError, ValueError):
        return default


def _get_song_pos(mode=-1):
    if mode == -1:
        return float(_active_transport_state.get("song_position", 0.0))
    if mode == 0:
        return float(_active_transport_state.get("song_position_seconds", 0.0)) * 1000.0
    if mode == 1:
        return float(_active_transport_state.get("song_position_seconds", 0.0))
    if mode == 2:
        return int(_active_transport_state.get("song_position_ticks", 0))
    if mode in (3, 4, 5):
        return _bst_component(
            _active_transport_state.get("song_position_hint", "1:01:000"),
            mode,
            (1, 1, 0)[mode - 3],
        )
    raise NotImplementedError("transport.getSongPos() supports normalized, time, tick, and BST modes")


def _set_song_pos(position, mode=-1):
    if mode not in (-1, 0, 1, 2):
        raise NotImplementedError(
            "transport.setSongPos() supports normalized, time, and absolute-tick modes; BST setting is not implemented"
        )
    position = float(position)
    if not math.isfinite(position):
        raise ValueError("transport.setSongPos() requires a finite position")
    if _active_transport_actions is None:
        return
    if mode == 2:
        _active_transport_actions.append(
            {"action": "set_song_position_ticks", "position": max(0, round(position))}
        )
        return
    if mode in (0, 1):
        seconds = position / 1000.0 if mode == 0 else position
        microseconds = min((2**64) - 1, max(0, round(seconds * 1_000_000.0)))
        _active_transport_actions.append(
            {
                "action": "set_song_position_seconds",
                "position_microseconds": microseconds,
            }
        )
        return
    _active_transport_actions.append(
        {"action": "set_song_position", "position": min(1.0, max(0.0, position))}
    )


def _get_song_length(mode):
    if mode == 0:
        return round(float(_active_transport_state.get("song_length_seconds", 0.0)) * 1000.0)
    if mode == 1:
        return round(float(_active_transport_state.get("song_length_seconds", 0.0)))
    if mode == 2:
        return int(_active_transport_state.get("song_length_ticks", 0))
    if mode in (3, 4, 5):
        return _bst_component(
            _active_transport_state.get("song_length_hint", "0:00:000"),
            mode,
            (0, 0, 0)[mode - 3],
        )
    raise NotImplementedError("transport.getSongLength() supports milliseconds, seconds, ticks, and BST modes")


def _get_song_pos_hint():
    return str(_active_transport_state.get("song_position_hint", "1:01:000"))


def _get_song_tick_pos(mode=0):
    position_ticks = int(_active_transport_state.get("song_position_ticks", 0))
    if mode == 0:
        return position_ticks
    if mode == 1:
        ppq = max(1, int(_active_transport_state.get("ticks_per_quarter", 96)))
        return float(position_ticks) / ppq
    raise NotImplementedError("mixer.getSongTickPos() supports integer-tick and beat modes")


def _get_song_step_pos():
    position_ticks = int(_active_transport_state.get("song_position_ticks", 0))
    ppq = max(1, int(_active_transport_state.get("ticks_per_quarter", 96)))
    return position_ticks * 4 // ppq


def _jog_marker(action, value, flags=None):
    if flags is not None:
        raise NotImplementedError("MIDI script marker jog flags are not supported")
    increment = int(value)
    if not -(2**31) <= increment < 2**31:
        raise ValueError("MIDI script marker jog increment is outside the supported range")
    _request_transport_action({"action": action, "increment": increment})


def _global_transport(command, value, pmeflags=2, flags=15):
    command = int(command)
    value = int(value)
    pmeflags = int(pmeflags)
    flags = int(flags)
    if not flags & 8 or not pmeflags & (2 | 4):
        return -1

    action = {
        10: "toggle_playback",
        11: "stop",
        12: "record",
        15: "loop_mode",
    }.get(command)
    if action is not None:
        if value != 0:
            _request_transport_action(action)
        return 8
    if command in (35, 36):
        _jog_marker("marker_jump_jog" if command == 35 else "marker_sel_jog", value)
        return 8
    return -1


def _install_modules(script_name, device_name):
    midi = _module("midi")
    constants = {
        "FromMIDI_Max": 127,
        "PME_System": 2,
        "PME_System_Safe": 4,
        "PME_PreviewNote": 8,
        "PME_FromHost": 16,
        "PME_FromMIDI": 32,
        "GT_Cannot": -1,
        "GT_None": 0,
        "GT_Plugin": 1,
        "GT_Form": 2,
        "GT_Menu": 4,
        "GT_Global": 8,
        "GT_All": 15,
        "GT_Delayed": 5,
        "SS_Stop": 0,
        "SS_StartStep": 1,
        "SS_Start": 2,
        "SONGLENGTH_MS": 0,
        "SONGLENGTH_S": 1,
        "SONGLENGTH_ABSTICKS": 2,
        "SONGLENGTH_BARS": 3,
        "SONGLENGTH_STEPS": 4,
        "SONGLENGTH_TICKS": 5,
        "ST_Int": 0,
        "ST_Beat": 1,
        "ST_PGB": 2,
        "FPT_Jog": 0,
        "FPT_Jog2": 1,
        "FPT_Strip": 2,
        "FPT_StripJog": 3,
        "FPT_StripHold": 4,
        "FPT_Previous": 5,
        "FPT_Next": 6,
        "FPT_PreviousNext": 7,
        "FPT_MoveJog": 8,
        "FPT_Play": 10,
        "FPT_Stop": 11,
        "FPT_Record": 12,
        "FPT_Rewind": 13,
        "FPT_FastForward": 14,
        "FPT_Loop": 15,
        "FPT_Mute": 16,
        "FPT_Mode": 17,
        "FPT_Undo": 20,
        "FPT_UndoUp": 21,
        "FPT_UndoJog": 22,
        "FPT_Punch": 30,
        "FPT_PunchIn": 31,
        "FPT_PunchOut": 32,
        "FPT_AddMarker": 33,
        "FPT_AddAltMarker": 34,
        "FPT_MarkerJumpJog": 35,
        "FPT_MarkerSelJog": 36,
        "FPT_Up": 40,
        "FPT_Down": 41,
        "FPT_Left": 42,
        "FPT_Right": 43,
        "FPT_HZoomJog": 44,
        "FPT_VZoomJog": 45,
        "FPT_Snap": 48,
        "FPT_SnapMode": 49,
        "FPT_Cut": 50,
        "FPT_Copy": 51,
        "FPT_Paste": 52,
        "FPT_Insert": 53,
        "FPT_Delete": 54,
        "FPT_NextWindow": 58,
        "FPT_WindowJog": 59,
        "FPT_F1": 60,
        "FPT_F2": 61,
        "FPT_F3": 62,
        "FPT_F4": 63,
        "FPT_F5": 64,
        "FPT_F6": 65,
        "FPT_F7": 66,
        "FPT_F8": 67,
        "FPT_F9": 68,
        "FPT_F10": 69,
        "FPT_F11": 70,
        "FPT_F12": 71,
        "FPT_Enter": 80,
        "FPT_Escape": 81,
        "FPT_Yes": 82,
        "FPT_No": 83,
        "FPT_Menu": 90,
        "FPT_ItemMenu": 91,
        "FPT_Save": 92,
        "FPT_SaveNew": 93,
        "FPT_PatternJog": 100,
        "FPT_TrackJog": 101,
        "FPT_ChannelJog": 102,
        "FPT_TempoJog": 105,
        "FPT_TapTempo": 106,
        "FPT_NudgeMinus": 107,
        "FPT_NudgePlus": 108,
        "FPT_Metronome": 110,
        "FPT_WaitForInput": 111,
        "FPT_Overdub": 112,
        "FPT_LoopRecord": 113,
        "FPT_StepEdit": 114,
        "FPT_CountDown": 115,
        "FPT_NextMixerWindow": 120,
        "FPT_MixerWindowJog": 121,
        "FPT_ShuffleJog": 122,
        "REC_UpdateValue": 1,
        "REC_GetValue": 2,
        "REC_ShowHint": 4,
        "REC_UpdatePlugLabel": 16,
        "REC_UpdateControl": 32,
        "REC_FromMIDI": 64,
        "REC_Store": 128,
        "REC_SetChanged": 256,
        "REC_SetTouched": 512,
        "REC_Init": 1024,
        "REC_NoLink": 2048,
        "REC_InternalCtrl": 4096,
        "REC_PlugReserved": 8192,
        "REC_Smoothed": 16384,
        "REC_NoLastTweaked": 32768,
        "REC_NoSaveUndo": 65536,
    }
    for key, value in constants.items():
        setattr(midi, key, value)
    midi.REC_InitStore = midi.REC_Init | midi.REC_Store
    midi.REC_Control = (
        midi.REC_UpdateValue
        | midi.REC_UpdateControl
        | midi.REC_ShowHint
        | midi.REC_InitStore
        | midi.REC_SetChanged
        | midi.REC_SetTouched
    )
    midi.REC_MIDIController = midi.REC_Control | midi.REC_FromMIDI

    device = _module("device")
    device.isAssigned = lambda: False
    device.getPortNumber = lambda: -1
    device.getName = lambda: device_name

    mixer = _module("mixer")
    mixer.getSongStepPos = _get_song_step_pos
    mixer.getSongTickPos = _get_song_tick_pos

    transport = _module("transport")
    transport.start = lambda: _request_transport_action("start")
    transport.stop = lambda: _request_transport_action("stop")
    transport.record = lambda: _request_transport_action("record")
    transport.setLoopMode = lambda: _request_transport_action("loop_mode")
    transport.isPlaying = lambda: int(bool(_active_transport_state.get("is_playing", False)))
    transport.isRecording = lambda: int(bool(_active_transport_state.get("is_recording", False)))
    transport.getSongPos = _get_song_pos
    transport.setSongPos = _set_song_pos
    transport.getSongLength = _get_song_length
    transport.getSongPosHint = _get_song_pos_hint
    transport.markerJumpJog = lambda value, flags=None: _jog_marker(
        "marker_jump_jog", value, flags
    )
    transport.markerSelJog = lambda value, flags=None: _jog_marker(
        "marker_sel_jog", value, flags
    )
    transport.globalTransport = _global_transport
    transport.getLoopMode = lambda: (
        0 if _active_transport_state.get("pattern_mode", False) else 1
    )

    for name in (
        "playlist",
        "channels",
        "mixer",
        "patterns",
        "arrangement",
        "ui",
        "plugins",
        "general",
        "launchMapPages",
    ):
        if name not in sys.modules:
            _module(name)


def _specific_callback(event):
    status = event.status & 0xF0
    if status == 0x90 and event.data2 > 0:
        return "OnNoteOn"
    if status == 0x80 or (status == 0x90 and event.data2 == 0):
        return "OnNoteOff"
    if status == 0xB0:
        return "OnControlChange"
    if status == 0xC0:
        return "OnProgramChange"
    if status == 0xD0:
        return "OnChannelPressure"
    if status == 0xE0:
        return "OnPitchBend"
    if status == 0xA0:
        return "OnKeyPressure"
    return None


def _dispatch(namespace, value):
    global _active_transport_actions, _active_transport_state
    _active_transport_actions = []
    _active_transport_state = value.get("transport", {})
    if value.get("idle"):
        callback = namespace.get("OnIdle")
        if callback is not None:
            callback()
        return {"handled": False, "actions": _active_transport_actions}

    event = EventData(value)
    for callback_name in ("OnMidiIn", "OnMidiMsg"):
        callback = namespace.get(callback_name)
        if callback is not None:
            callback(event)
        if event.handled:
            return {"handled": True, "actions": _active_transport_actions}

    if event.status == 0xF0:
        callback_name = "OnSysEx"
    else:
        callback_name = _specific_callback(event)
    if callback_name is not None:
        callback = namespace.get(callback_name)
        if callback is not None:
            callback(event)
    return {
        "handled": bool(event.handled),
        "status": int(event.status),
        "data1": int(event.data1),
        "data2": int(event.data2),
        "sysex": list(event.sysex),
        "actions": _active_transport_actions,
    }


def main():
    script_path = sys.argv[1]
    script_name = sys.argv[2]
    device_name = sys.argv[3]
    sys.path.insert(0, __import__("os").path.dirname(script_path))
    _install_modules(script_name, device_name)
    try:
        namespace = runpy.run_path(script_path, run_name="__main__")
        callback = namespace.get("OnInit")
        if callback is not None:
            callback()
    except Exception as error:
        _emit({"ready": False, "error": "{}: {}".format(type(error).__name__, error)})
        return
    _emit({"ready": True})

    for line in sys.stdin:
        try:
            request = json.loads(line)
        except Exception:
            _emit({"error": "invalid host request"})
            continue
        if request.get("shutdown"):
            try:
                callback = namespace.get("OnDeInit")
                if callback is not None:
                    callback()
            except Exception as error:
                _emit({"error": "{}: {}".format(type(error).__name__, error)})
            break
        try:
            _emit(_dispatch(namespace, request))
        except Exception as error:
            _emit({"error": "{}: {}".format(type(error).__name__, error)})


main()
