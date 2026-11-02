//! Bounded seekable storage used by the main-thread VST 3 IBStream adapter.
use std::io::{self, Read, Seek, SeekFrom, Write};
const LIMIT: u64 = vst_audio_protocol::MAX_STATE_BODY_BYTES as u64;
#[derive(Debug, Default)]
pub struct StateBuffer(std::io::Cursor<Vec<u8>>);
impl StateBuffer {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(std::io::Cursor::new(bytes))
    }
    pub fn try_from_slice(bytes: &[u8]) -> io::Result<Self> {
        let mut copy = Vec::new();
        copy.try_reserve_exact(bytes.len())
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        copy.extend_from_slice(bytes);
        Ok(Self::new(copy))
    }
    pub fn try_snapshot(&self) -> io::Result<Vec<u8>> {
        let bytes = self.0.get_ref();
        let mut copy = Vec::new();
        copy.try_reserve_exact(bytes.len())
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        copy.extend_from_slice(bytes);
        Ok(copy)
    }
    pub fn position(&self) -> u64 {
        self.0.position()
    }
    /// The bytes from the current position to the end; empty past the end.
    pub fn remaining(&self) -> &[u8] {
        let bytes = self.0.get_ref();
        let start = usize::try_from(self.position()).unwrap_or(usize::MAX);
        bytes.get(start..).unwrap_or(&[])
    }
    /// Moves the position forward by `count` bytes already consumed via `remaining`.
    pub fn advance(&mut self, count: usize) {
        let position = self.position().saturating_add(count as u64);
        self.0.set_position(position);
    }
}
impl Read for StateBuffer {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
}
impl Write for StateBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self.position().saturating_add(bytes.len() as u64);
        if end > LIMIT {
            return Err(io::Error::other("state buffer limit"));
        }
        let length = self.0.get_ref().len() as u64;
        if end > length {
            self.0
                .get_mut()
                .try_reserve((end - length) as usize)
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        }
        self.0.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Seek for StateBuffer {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(n) => self.position().checked_add_signed(n),
            SeekFrom::End(n) => (self.0.get_ref().len() as u64).checked_add_signed(n),
        }
        .filter(|n| *n <= LIMIT)
        .ok_or_else(|| io::Error::other("invalid state seek"))?;
        self.0.set_position(position);
        Ok(position)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stream_buffer_read_write_seek_bounds() {
        let mut stream = StateBuffer::new(vec![1, 2]);
        let mut bytes = [0; 4];
        assert_eq!(stream.read(&mut bytes).unwrap(), 2);
        assert_eq!(stream.read(&mut bytes).unwrap(), 0);
        stream.seek(SeekFrom::End(2)).unwrap();
        stream.write_all(&[3]).unwrap();
        assert_eq!(stream.try_snapshot().unwrap(), [1, 2, 0, 0, 3]);
        assert_eq!(stream.seek(SeekFrom::Current(-1)).unwrap(), 4);
        assert!(stream.seek(SeekFrom::Current(-5)).is_err());
        assert!(stream.seek(SeekFrom::Start(LIMIT + 1)).is_err());
        stream.seek(SeekFrom::Start(LIMIT)).unwrap();
        assert!(stream.write(&[1]).is_err());
    }

    #[test]
    fn remaining_and_advance_follow_the_cursor() {
        let mut stream = StateBuffer::new(vec![1, 2, 3]);
        assert_eq!(stream.remaining(), [1, 2, 3]);
        stream.advance(2);
        assert_eq!(stream.remaining(), [3]);
        assert_eq!(stream.position(), 2);
        stream.advance(5);
        assert!(stream.remaining().is_empty());
        let mut bytes = [0; 1];
        assert_eq!(stream.read(&mut bytes).unwrap(), 0);
    }

    #[test]
    fn try_snapshot_and_try_from_slice_round_trip() {
        let stream = StateBuffer::try_from_slice(&[1, 2, 3, 4]).unwrap();
        assert_eq!(stream.try_snapshot().unwrap(), [1, 2, 3, 4]);
    }
}
