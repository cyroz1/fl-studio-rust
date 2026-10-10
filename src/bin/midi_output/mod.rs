use midir::{MidiOutput, MidiOutputConnection as MidirOutputConnection};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MidiOutputDevice {
    pub id: String,
    pub name: String,
}

pub struct ConnectedMidiOutput {
    connection: MidirOutputConnection,
    pub device_id: String,
    pub device_name: String,
}

pub const MIDI_TEST_NOTE_ON: [u8; 3] = [0x90, 60, 96];
pub const MIDI_TEST_NOTE_OFF: [u8; 3] = [0x80, 60, 0];

pub fn enumerate_output_devices() -> Result<Vec<MidiOutputDevice>, String> {
    let output = MidiOutput::new("FL Studio Rebuild MIDI Output")
        .map_err(|error| format!("could not initialize MIDI output: {error}"))?;
    output
        .ports()
        .into_iter()
        .map(|port| {
            let name = output
                .port_name(&port)
                .map_err(|error| format!("could not read MIDI output name: {error}"))?;
            Ok(MidiOutputDevice {
                id: port.id(),
                name,
            })
        })
        .collect()
}

pub fn connect_output_device(device_id: &str) -> Result<ConnectedMidiOutput, String> {
    let output = MidiOutput::new("FL Studio Rebuild MIDI Output")
        .map_err(|error| format!("could not initialize MIDI output: {error}"))?;
    let port = output
        .ports()
        .into_iter()
        .find(|port| port.id() == device_id)
        .ok_or_else(|| "the selected MIDI output is no longer available".to_owned())?;
    let device_name = output
        .port_name(&port)
        .map_err(|error| format!("could not read MIDI output name: {error}"))?;
    let connection = output
        .connect(&port, "FL Studio Rebuild")
        .map_err(|error| format!("could not connect to MIDI output: {error}"))?;
    Ok(ConnectedMidiOutput {
        connection,
        device_id: device_id.to_owned(),
        device_name,
    })
}

impl ConnectedMidiOutput {
    pub fn send(&mut self, message: &[u8]) -> Result<(), String> {
        self.connection
            .send(message)
            .map_err(|error| format!("could not send MIDI output: {error}"))
    }

    pub fn send_all_notes_off(&mut self) -> Result<(), String> {
        let mut first_error = None;
        for channel in 0..16 {
            if let Err(error) = self.send(&all_notes_off_message(channel))
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for ConnectedMidiOutput {
    fn drop(&mut self) {
        let _ = self.send_all_notes_off();
    }
}

fn all_notes_off_message(channel: u8) -> [u8; 3] {
    [0xB0 | (channel & 0x0F), 123, 0]
}

#[cfg(test)]
mod tests {
    use super::all_notes_off_message;

    #[test]
    fn all_notes_off_uses_the_requested_midi_channel() {
        assert_eq!(all_notes_off_message(0), [0xB0, 123, 0]);
        assert_eq!(all_notes_off_message(15), [0xBF, 123, 0]);
    }
}
