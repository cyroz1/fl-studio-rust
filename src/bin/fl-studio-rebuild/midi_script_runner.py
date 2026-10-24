import json
import math
import runpy
import sys
import types


_protocol_out = sys.stdout
sys.stdout = sys.stderr
_active_transport_actions = None
_active_transport_state = {}
_midi_out_new_messages = {}


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


def _short_message_length(status):
    if 0x80 <= status <= 0xEF:
        return 2 if (status & 0xF0) in (0xC0, 0xD0) else 3
    if status in (0xF1, 0xF3):
        return 2
    if status in (0xF2,):
        return 3
    if status in (0xF6, 0xF8, 0xFA, 0xFB, 0xFC, 0xFE, 0xFF):
        return 1
    return 0


def _queue_midi_output(message_bytes, action="midi_out_message"):
    if _active_transport_actions is not None:
        _active_transport_actions.append(
            {"action": action, "bytes": list(message_bytes)}
        )


def _midi_out_msg(*arguments):
    if len(arguments) == 1:
        try:
            packed = int(arguments[0])
        except (TypeError, ValueError, OverflowError) as error:
            raise ValueError("device.midiOutMsg() requires a packed integer message") from error
        if not 0 <= packed <= 0xFFFFFF:
            raise ValueError("device.midiOutMsg() message must fit in 24 bits")
        status = packed & 0xFF
        data = [(packed >> 8) & 0xFF, (packed >> 16) & 0xFF]
    elif len(arguments) == 4:
        try:
            midi_id, channel, data1, data2 = (int(value) for value in arguments)
        except (TypeError, ValueError, OverflowError) as error:
            raise ValueError("device.midiOutMsg() arguments must be integers") from error
        if not 0x80 <= midi_id <= 0xE0 or midi_id & 0x0F:
            raise ValueError("device.midiOutMsg() midiId must be a channel-voice status type")
        if not 0 <= channel <= 15:
            raise ValueError("device.midiOutMsg() channel must be between 0 and 15")
        status = midi_id | channel
        data = [data1, data2]
    else:
        raise TypeError("device.midiOutMsg() expects one or four arguments")

    length = _short_message_length(status)
    if not length:
        raise NotImplementedError("device.midiOutMsg() does not support this MIDI status")
    data = data[: length - 1]
    if any(value < 0 or value > 127 for value in data):
        raise ValueError("device.midiOutMsg() data bytes must be between 0 and 127")
    _queue_midi_output([status] + data)


def _midi_out_new_msg(slot_index, message):
    try:
        slot_index = int(slot_index)
        packed = int(message)
    except (TypeError, ValueError, OverflowError) as error:
        raise ValueError("device.midiOutNewMsg() requires integer arguments") from error
    if _midi_out_new_messages.get(slot_index) == packed:
        return
    _midi_out_msg(packed)
    _midi_out_new_messages[slot_index] = packed


def _midi_out_sysex(message):
    if isinstance(message, str):
        try:
            message = bytes(ord(character) for character in message)
        except ValueError as error:
            raise ValueError("device.midiOutSysex() string must contain byte characters") from error
    elif isinstance(message, (bytes, bytearray)):
        message = bytes(message)
    else:
        raise TypeError("device.midiOutSysex() expects a byte string")
    if len(message) < 2 or len(message) > 65_536:
        raise ValueError("device.midiOutSysex() requires 2–65,536 bytes")
    if any(byte > 0x7F for byte in message[1:-1]):
        raise ValueError("device.midiOutSysex() payload bytes must be between 0 and 127")
    _queue_midi_output(message, "midi_out_sysex")


def _event_message_bytes(event):
    status = int(event.status)
    if status == 0xF0:
        message = bytes(event.sysex)
        if len(message) < 2 or message[0] != 0xF0 or message[-1] != 0xF7:
            raise ValueError("MIDI event does not contain a complete SysEx message")
        if any(byte > 0x7F for byte in message[1:-1]):
            raise ValueError("MIDI SysEx payload bytes must be between 0 and 127")
        return message
    length = _short_message_length(status)
    if not length:
        raise NotImplementedError("device.directFeedback() does not support this MIDI status")
    data = [int(event.data1), int(event.data2)][: length - 1]
    if any(value < 0 or value > 127 for value in data):
        raise ValueError("MIDI event data bytes must be between 0 and 127")
    return bytes([status] + data)


def _direct_feedback(event):
    message = _event_message_bytes(event)
    _queue_midi_output(message, "midi_out_sysex" if message[0] == 0xF0 else "midi_out_message")


def _repeat_midi_event(event, delay=300, rate=300):
    try:
        delay = int(delay)
        rate = int(rate)
    except (TypeError, ValueError, OverflowError) as error:
        raise ValueError("device.repeatMidiEvent() delay and rate must be integers") from error
    if delay < 0 or rate <= 0 or delay > 0x7FFFFFFF or rate > 0x7FFFFFFF:
        raise ValueError("device.repeatMidiEvent() requires nonnegative delay and positive rate")
    message = _event_message_bytes(event)
    if _active_transport_actions is not None:
        _active_transport_actions.append(
            {
                "action": "repeat_midi_event",
                "bytes": list(message),
                "delay_ms": delay,
                "rate_ms": rate,
            }
        )


def _stop_repeat_midi_event():
    if _active_transport_actions is not None:
        _active_transport_actions.append({"action": "stop_repeat_midi_event"})


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


def _get_rec_ppq():
    return max(1, int(_active_transport_state.get("ticks_per_quarter", 96)))


def _get_rec_ppb():
    numerator = max(1, int(_active_transport_state.get("time_signature_numerator", 4)))
    return _get_rec_ppq() * numerator


def _get_song_tick_pos(mode=0):
    position_ticks = int(_active_transport_state.get("song_position_ticks", 0))
    if mode == 0:
        return position_ticks
    if mode == 1:
        ppq = max(1, int(_active_transport_state.get("ticks_per_quarter", 96)))
        return float(position_ticks) / ppq
    raise NotImplementedError("mixer.getSongTickPos() supports integer-tick and beat modes")


def _get_current_tempo(asInt=0):
    try:
        asInt = int(asInt)
    except (TypeError, ValueError, OverflowError) as error:
        raise ValueError("mixer.getCurrentTempo() asInt must be an integer") from error
    milli_bpm = max(1, int(_active_transport_state.get("current_tempo_milli_bpm", 140_000)))
    tempo = milli_bpm / 1000.0
    return int(tempo) if asInt else tempo


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


def _move_song_position(speed, start_stop):
    try:
        speed = float(speed)
        start_stop = int(start_stop)
    except (TypeError, ValueError, OverflowError) as error:
        raise ValueError("song-position move speed and startStop must be numeric") from error
    if not math.isfinite(speed):
        raise ValueError("song-position move speed must be finite")
    if start_stop == 1:
        raise NotImplementedError("SS_StartStep song-position moves are not supported")
    if start_stop not in (0, 2):
        raise ValueError("song-position move startStop must be SS_Stop or SS_Start")
    speed_millionths = int(round(speed * 1_000_000.0))
    if not -(2**31) <= speed_millionths < 2**31:
        raise ValueError("song-position move speed is outside the supported range")
    _request_transport_action(
        {
            "action": "move_song_position",
            "speed_millionths": speed_millionths,
            "start": start_stop == 2,
        }
    )


def _set_playback_speed(speed_multiplier):
    try:
        speed_multiplier = float(speed_multiplier)
    except (TypeError, ValueError, OverflowError) as error:
        raise ValueError("playback speed must be numeric") from error
    if not math.isfinite(speed_multiplier) or not 0.25 <= speed_multiplier <= 4.0:
        raise ValueError("playback speed must be between 0.25 and 4.0")
    _request_transport_action(
        {
            "action": "set_playback_speed",
            "speed_millionths": int(round(speed_multiplier * 1_000_000.0)),
        }
    )


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
    if command in (13, 14):
        if value not in (0, 2):
            return -1
        _move_song_position(-1 if command == 13 else 1, value)
        return 8
    if command in (35, 36):
        _jog_marker("marker_jump_jog" if command == 35 else "marker_sel_jog", value)
        return 8
    if command == 100:
        if value != 0:
            _request_transport_action(
                {"action": "pattern_jog", "increment": value}
            )
        return 8
    if command == 105:
        if value != 0:
            _request_transport_action({"action": "tempo_jog", "increment": value})
        return 8
    return -1


def _install_modules(script_name, device_name):
    midi = _module("midi")
    constants = {
        "FromMIDI_Max": 127,
        "PL_Start": 0,
        "PL_LoadOk": 100,
        "PL_LoadError": 101,
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
    device.isAssigned = lambda: int(
        int(_active_transport_state.get("midi_output_port_number", -1)) >= 0
    )
    device.getPortNumber = lambda: int(
        _active_transport_state.get("midi_output_port_number", -1)
    )
    device.getName = lambda: device_name
    device.midiOutMsg = _midi_out_msg
    device.midiOutNewMsg = _midi_out_new_msg
    device.midiOutSysex = _midi_out_sysex
    device.directFeedback = _direct_feedback
    device.repeatMidiEvent = _repeat_midi_event
    device.stopRepeatMidiEvent = _stop_repeat_midi_event

    mixer = _module("mixer")
    mixer.getCurrentTempo = _get_current_tempo
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
    transport.getHWBeatLEDState = lambda: int(
        _active_transport_state.get("hardware_beat_led_state", 0)
    )
    transport.rewind = lambda startStop, flags=15: (
        _move_song_position(-1, startStop) if int(flags) & 8 else None
    )
    transport.fastForward = lambda speed: _move_song_position(1, speed)
    transport.continuousMove = lambda speed, startStop: _move_song_position(
        speed, startStop
    )
    transport.continuousMovePos = lambda speed, startStop: _move_song_position(
        speed, startStop
    )
    transport.setPlaybackSpeed = _set_playback_speed
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

    general = _module("general")
    general.getRecPPQ = _get_rec_ppq
    general.getRecPPB = _get_rec_ppb
    general.getVersion = lambda: 1
    general.getChangedFlag = lambda: int(
        _active_transport_state.get("project_changed_flag", 0)
    )
    general.getProjectTitle = lambda: str(
        _active_transport_state.get("project_title", "")
    )
    general.getProjectAuthor = lambda: str(
        _active_transport_state.get("project_author", "")
    )
    general.getProjectGenre = lambda: str(
        _active_transport_state.get("project_genre", "")
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
    if value.get("initialize"):
        callback = namespace.get("OnInit")
        if callback is not None:
            callback()
        return {"handled": False, "actions": _active_transport_actions}
    if value.get("deinitialize"):
        callback = namespace.get("OnDeInit")
        if callback is not None:
            callback()
        return {"handled": False, "actions": _active_transport_actions}
    if "project_load_status" in value:
        status = int(value["project_load_status"])
        callback = namespace.get("OnProjectLoad")
        if callback is not None:
            callback(status)
        if status == 100:
            callback = namespace.get("OnDoFullRefresh")
            if callback is not None:
                callback()
        return {"handled": False, "actions": _active_transport_actions}
    if "beat_indicator" in value:
        callback = namespace.get("OnUpdateBeatIndicator")
        if callback is not None:
            callback(int(value["beat_indicator"]))
        return {"handled": False, "actions": _active_transport_actions}
    if value.get("first_connect"):
        callback = namespace.get("OnFirstConnect")
        if callback is not None:
            callback()
        return {"handled": False, "actions": _active_transport_actions}
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
                _emit(
                    _dispatch(
                        namespace,
                        {"deinitialize": True, "transport": request.get("transport", {})},
                    )
                )
            except Exception as error:
                _emit({"error": "{}: {}".format(type(error).__name__, error)})
            break
        try:
            _emit(_dispatch(namespace, request))
        except Exception as error:
            _emit({"error": "{}: {}".format(type(error).__name__, error)})


main()
