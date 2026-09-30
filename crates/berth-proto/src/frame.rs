//! Length-prefixed frames: a little-endian `u32` length, then the payload.

use crate::Error;

/// The largest payload either side accepts. A full screen is tens of
/// kilobytes and a history range is capped well below this; anything larger
/// is a bug or an attack, and is refused before any of it is buffered.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

const PREFIX: usize = 4;

/// A payload as a frame, ready to write.
pub fn frame(payload: &[u8]) -> Result<Vec<u8>, Error> {
    if payload.len() > MAX_FRAME {
        return Err(Error::FrameTooLarge {
            len: payload.len(),
            max: MAX_FRAME,
        });
    }
    let mut out = Vec::with_capacity(PREFIX + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Turns a byte stream, in whatever pieces it arrives, back into payloads.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
    /// Where the next frame starts in `buf`. Consumed bytes are dropped in
    /// bulk rather than shifted on every frame.
    start: usize,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add bytes as they were read.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.start > 0 && self.start >= self.buf.len() / 2 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole payload, or `None` until more bytes arrive.
    ///
    /// A length over [`MAX_FRAME`] is an error as soon as its prefix is seen,
    /// and the stream cannot be trusted after it: the caller closes the
    /// connection.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, Error> {
        let pending = &self.buf[self.start..];
        let Some(prefix) = pending.first_chunk::<PREFIX>() else {
            return Ok(None);
        };
        let len = u32::from_le_bytes(*prefix) as usize;
        if len > MAX_FRAME {
            return Err(Error::FrameTooLarge {
                len,
                max: MAX_FRAME,
            });
        }
        let Some(payload) = pending.get(PREFIX..PREFIX + len) else {
            return Ok(None);
        };
        let payload = payload.to_vec();
        self.start += PREFIX + len;
        Ok(Some(payload))
    }

    /// Bytes received and not yet returned as a frame.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.start
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_frame_is_its_length_then_its_bytes() {
        assert_eq!(frame(b"abc").unwrap(), [3, 0, 0, 0, b'a', b'b', b'c']);
    }

    #[test]
    fn a_frame_comes_back_whole() {
        let mut reader = FrameReader::new();
        reader.push(&frame(b"hello").unwrap());
        assert_eq!(reader.next_frame().unwrap(), Some(b"hello".to_vec()));
        assert_eq!(reader.next_frame().unwrap(), None);
        assert_eq!(reader.buffered(), 0);
    }

    #[test]
    fn a_partial_frame_waits_for_the_rest() {
        let bytes = frame(b"hello").unwrap();
        let mut reader = FrameReader::new();
        reader.push(&bytes[..2]);
        assert_eq!(reader.next_frame().unwrap(), None);
        reader.push(&bytes[2..6]);
        assert_eq!(reader.next_frame().unwrap(), None);
        reader.push(&bytes[6..]);
        assert_eq!(reader.next_frame().unwrap(), Some(b"hello".to_vec()));
    }

    #[test]
    fn an_empty_payload_is_a_frame() {
        let mut reader = FrameReader::new();
        reader.push(&frame(b"").unwrap());
        assert_eq!(reader.next_frame().unwrap(), Some(Vec::new()));
    }

    /// Refused on the prefix alone, before 4 GB is waited for or buffered.
    #[test]
    fn an_oversized_length_is_refused_at_once() {
        let mut reader = FrameReader::new();
        reader.push(&u32::MAX.to_le_bytes());
        assert!(matches!(
            reader.next_frame(),
            Err(Error::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn an_oversized_payload_is_not_framed() {
        let big = vec![0u8; MAX_FRAME + 1];
        assert!(matches!(frame(&big), Err(Error::FrameTooLarge { .. })));
    }

    proptest! {
        /// However the stream is cut, the same frames come out in order.
        #[test]
        fn any_split_gives_back_the_same_frames(
            payloads in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..300), 0..20),
            cuts in prop::collection::vec(1usize..64, 1..200),
        ) {
            let stream: Vec<u8> = payloads.iter().flat_map(|p| frame(p).unwrap()).collect();
            let mut reader = FrameReader::new();
            let mut out = Vec::new();
            let mut at = 0;
            let mut cuts = cuts.into_iter().cycle();
            while at < stream.len() {
                let end = (at + cuts.next().unwrap_or(1)).min(stream.len());
                reader.push(&stream[at..end]);
                at = end;
                while let Some(p) = reader.next_frame().unwrap() {
                    out.push(p);
                }
            }
            prop_assert_eq!(out, payloads);
            prop_assert_eq!(reader.buffered(), 0);
        }
    }
}
