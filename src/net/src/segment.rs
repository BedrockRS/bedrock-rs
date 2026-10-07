//! NetherNet data-channel message segmentation.
//!
//! Every message on a NetherNet data channel starts with a one-byte header that
//! counts the segments still to follow it: `0` marks a complete message or the
//! final segment of a larger one. Only the reliable channel carries multi-segment
//! messages (see `docs/ARCHITECTURE.md` §3.4).

use std::num::NonZeroUsize;

/// Most segments a single message may be split into.
///
/// The header could express 256, but go-nethernet refuses to send more than 255,
/// so we stay within the same bound.
pub const MAX_SEGMENTS: usize = u8::MAX as usize;

/// Errors produced while splitting or reassembling messages.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SegmentError {
    /// The message would need more than [`MAX_SEGMENTS`] segments.
    #[error(
        "message of {len} bytes needs more than {MAX_SEGMENTS} segments of {segment_payload} bytes"
    )]
    TooManySegments { len: usize, segment_payload: usize },
    /// A received data-channel message was empty, so it had no segment header.
    #[error("data channel message has no segment header")]
    MissingHeader,
    /// A segment's countdown did not continue the message being reassembled.
    #[error("segment countdown out of order: expected {expected}, got {got}")]
    OutOfOrder { expected: u8, got: u8 },
    /// The reassembled message grew past the configured limit.
    #[error("reassembled message exceeds {max} bytes")]
    MessageTooLarge { max: usize },
}

/// Splits `payload` into data-channel messages carrying at most
/// `segment_payload` bytes each, every one prefixed with its countdown header.
///
/// An empty payload yields no segments; NetherNet never sends header-only messages.
pub fn split(payload: &[u8], segment_payload: NonZeroUsize) -> Result<Segments<'_>, SegmentError> {
    let segment_payload = segment_payload.get();
    let remaining = u8::try_from(payload.len().div_ceil(segment_payload)).map_err(|_| {
        SegmentError::TooManySegments {
            len: payload.len(),
            segment_payload,
        }
    })?;
    Ok(Segments {
        chunks: payload.chunks(segment_payload),
        remaining,
    })
}

/// Iterator over the framed segments of one message, created by [`split`].
#[derive(Debug)]
pub struct Segments<'a> {
    chunks: std::slice::Chunks<'a, u8>,
    remaining: u8,
}

impl Iterator for Segments<'_> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        let chunk = self.chunks.next()?;
        self.remaining -= 1;
        let mut segment = Vec::with_capacity(chunk.len() + 1);
        segment.push(self.remaining);
        segment.extend_from_slice(chunk);
        Some(segment)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = usize::from(self.remaining);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for Segments<'_> {}

/// Rebuilds messages from the segments received on one data channel.
///
/// Segments must arrive in order, as they do on the ordered reliable channel.
/// Any violation of the countdown discards the partial message and is reported
/// as an error, which callers should treat as a protocol violation.
#[derive(Debug)]
pub struct Reassembler {
    buffer: Vec<u8>,
    /// Countdown value expected on the next segment while a message is in progress.
    next: Option<u8>,
    max_message_size: usize,
}

impl Reassembler {
    /// Creates a reassembler that rejects messages larger than `max_message_size` bytes.
    pub fn new(max_message_size: usize) -> Self {
        Self {
            buffer: Vec::new(),
            next: None,
            max_message_size,
        }
    }

    /// Feeds one received data-channel message and returns the complete payload
    /// once its final segment has arrived.
    pub fn push(&mut self, message: &[u8]) -> Result<Option<Vec<u8>>, SegmentError> {
        let (&remaining, data) = message.split_first().ok_or(SegmentError::MissingHeader)?;
        if let Some(expected) = self.next
            && remaining != expected
        {
            self.reset();
            return Err(SegmentError::OutOfOrder {
                expected,
                got: remaining,
            });
        }
        if self.buffer.len() + data.len() > self.max_message_size {
            self.reset();
            return Err(SegmentError::MessageTooLarge {
                max: self.max_message_size,
            });
        }

        if remaining > 0 {
            self.buffer.extend_from_slice(data);
            self.next = Some(remaining - 1);
            return Ok(None);
        }
        self.next = None;
        if self.buffer.is_empty() {
            return Ok(Some(data.to_vec()));
        }
        self.buffer.extend_from_slice(data);
        Ok(Some(std::mem::take(&mut self.buffer)))
    }

    /// Whether no message is currently being reassembled.
    pub fn is_idle(&self) -> bool {
        self.next.is_none()
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.next = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn small_message_is_one_segment_with_zero_header() {
        let segments: Vec<_> = split(b"hello", size(10)).unwrap().collect();
        assert_eq!(segments, vec![b"\0hello".to_vec()]);
    }

    #[test]
    fn empty_message_yields_no_segments() {
        assert_eq!(split(&[], size(10)).unwrap().count(), 0);
    }

    #[test]
    fn headers_count_down_to_zero() {
        let segments: Vec<_> = split(&payload(25), size(10)).unwrap().collect();
        let headers: Vec<u8> = segments.iter().map(|s| s[0]).collect();
        assert_eq!(headers, vec![2, 1, 0]);
        assert_eq!(segments[2].len(), 1 + 5);
    }

    #[test]
    fn split_then_reassemble_round_trips_every_boundary() {
        for segment_payload in 1..=6 {
            for len in 1..=(segment_payload * 5 + 1) {
                let original = payload(len);
                let segments = split(&original, size(segment_payload)).unwrap();
                assert_eq!(segments.len(), len.div_ceil(segment_payload));

                let mut reassembler = Reassembler::new(usize::MAX);
                let mut complete = Vec::new();
                for segment in segments {
                    assert!(segment.len() <= segment_payload + 1);
                    if let Some(message) = reassembler.push(&segment).unwrap() {
                        complete.push(message);
                    }
                }
                assert_eq!(
                    complete,
                    vec![original],
                    "len {len}, segment {segment_payload}"
                );
                assert!(reassembler.is_idle());
            }
        }
    }

    #[test]
    fn segment_count_is_capped() {
        assert_eq!(
            split(&payload(MAX_SEGMENTS), size(1)).unwrap().len(),
            MAX_SEGMENTS
        );
        assert_eq!(
            split(&payload(MAX_SEGMENTS + 1), size(1)).unwrap_err(),
            SegmentError::TooManySegments {
                len: MAX_SEGMENTS + 1,
                segment_payload: 1
            }
        );
    }

    #[test]
    fn empty_data_channel_message_is_rejected() {
        let mut reassembler = Reassembler::new(1024);
        assert_eq!(reassembler.push(&[]), Err(SegmentError::MissingHeader));
    }

    #[test]
    fn skipped_countdown_value_is_rejected_and_state_resets() {
        let mut reassembler = Reassembler::new(1024);
        assert_eq!(reassembler.push(&[2, 1]), Ok(None));
        assert_eq!(
            reassembler.push(&[0, 2]),
            Err(SegmentError::OutOfOrder {
                expected: 1,
                got: 0
            })
        );
        assert!(reassembler.is_idle());
        assert_eq!(reassembler.push(&[0, 9]), Ok(Some(vec![9])));
    }

    #[test]
    fn restarting_mid_message_is_rejected() {
        let mut reassembler = Reassembler::new(1024);
        assert_eq!(reassembler.push(&[1, 1]), Ok(None));
        assert_eq!(
            reassembler.push(&[1, 1]),
            Err(SegmentError::OutOfOrder {
                expected: 0,
                got: 1
            })
        );
    }

    #[test]
    fn oversized_message_is_rejected() {
        let mut reassembler = Reassembler::new(4);
        assert_eq!(reassembler.push(&[1, 1, 2, 3]), Ok(None));
        assert_eq!(
            reassembler.push(&[0, 4, 5]),
            Err(SegmentError::MessageTooLarge { max: 4 })
        );
        assert!(reassembler.is_idle());
        assert_eq!(
            reassembler.push(&[0, 1, 2, 3, 4, 5]),
            Err(SegmentError::MessageTooLarge { max: 4 })
        );
    }
}
