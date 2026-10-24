import json
import runpy
import sys
import types


_protocol_out = sys.stdout
sys.stdout = sys.stderr
_active_transport_actions = None


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
        self.pmeflags = 0
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


def _install_modules(script_name, device_name):
    midi = _module("midi")
    constants = {
        "FromMIDI_Max": 127,
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

    transport = _module("transport")
    transport.start = lambda: _request_transport_action("start")
    transport.stop = lambda: _request_transport_action("stop")
    transport.record = lambda: _request_transport_action("record")

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
    global _active_transport_actions
    _active_transport_actions = []
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
