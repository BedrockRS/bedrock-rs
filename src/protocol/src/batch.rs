//! The batch framing that carries packets in each NetherNet message.
//!
//! A batch is a run of packets, each prefixed with its varuint32 length. Until
//! NetworkSettings has been exchanged a batch is sent as is. Afterwards it starts
//! with a compression byte: `0x00` raw DEFLATE, `0x01` Snappy or `0xFF` none.
//! Unlike RakNet, NetherNet has no `0xFE` batch header and no game-level
//! encryption, since DTLS already encrypts everything (`docs/ARCHITECTURE.md` §3.5).

use std::io::{self, Read as _, Write as _};

use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;

use crate::io::{DecodeError, Reader, Writer};

/// Largest batch accepted after decompression.
pub const MAX_BATCH_SIZE: usize = 16 * 1024 * 1024;

/// Most packets accepted in one batch, the same limit gophertunnel enforces.
pub const MAX_BATCH_PACKETS: usize = 812;

/// NetworkSettings algorithm ID meaning "no compression"; `0xFF` as a batch prefix.
pub const NO_COMPRESSION: u16 = 0xFFFF;

const UNCOMPRESSED: u8 = 0xFF;
const FLATE: u8 = 0x00;
const SNAPPY: u8 = 0x01;

/// Compression algorithms a server can pick in NetworkSettings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionAlgorithm {
    /// Raw DEFLATE (RFC 1951), which Mojang calls zlib.
    Flate,
    /// Snappy's block format. gophertunnel warns it crashes some devices without AVX2.
    Snappy,
}

impl CompressionAlgorithm {
    /// The algorithm's ID in NetworkSettings.
    pub const fn id(self) -> u16 {
        match self {
            Self::Flate => FLATE as u16,
            Self::Snappy => SNAPPY as u16,
        }
    }

    const fn prefix(self) -> u8 {
        match self {
            Self::Flate => FLATE,
            Self::Snappy => SNAPPY,
        }
    }
}

/// Compression settings agreed in NetworkSettings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compression {
    pub algorithm: CompressionAlgorithm,
    /// Smallest batch that is compressed. 0 disables compression.
    pub threshold: u16,
}

/// Errors from encoding or decoding a batch.
#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error("unknown compression algorithm {0:#04x}")]
    UnknownAlgorithm(u8),
    #[error("failed to compress batch: {0}")]
    Compress(io::Error),
    #[error("failed to decompress batch: {0}")]
    Decompress(String),
    #[error("batch is larger than {MAX_BATCH_SIZE} bytes")]
    TooLarge,
    #[error("batch holds more than {MAX_BATCH_PACKETS} packets")]
    TooManyPackets,
    #[error("empty message")]
    Empty,
}

/// Frames encoded packets into one batch, compressing it once `compression` is agreed.
pub fn encode<'a>(
    packets: impl IntoIterator<Item = &'a [u8]>,
    compression: Option<Compression>,
) -> Result<Vec<u8>, BatchError> {
    let mut writer = Writer::new();
    for packet in packets {
        writer.byte_array(packet);
    }
    let batch = writer.into_bytes();

    let Some(compression) = compression else {
        return Ok(batch);
    };
    if compression.threshold == 0 || batch.len() < usize::from(compression.threshold) {
        let mut message = Vec::with_capacity(batch.len() + 1);
        message.push(UNCOMPRESSED);
        message.extend_from_slice(&batch);
        return Ok(message);
    }

    let mut message = vec![compression.algorithm.prefix()];
    match compression.algorithm {
        CompressionAlgorithm::Flate => {
            // Level 6 matches gophertunnel and Go's default.
            let mut encoder = DeflateEncoder::new(message, flate2::Compression::new(6));
            encoder.write_all(&batch).map_err(BatchError::Compress)?;
            message = encoder.finish().map_err(BatchError::Compress)?;
        }
        CompressionAlgorithm::Snappy => {
            let compressed = snap::raw::Encoder::new()
                .compress_vec(&batch)
                .map_err(|err| BatchError::Compress(io::Error::other(err)))?;
            message.extend_from_slice(&compressed);
        }
    }
    Ok(message)
}

/// Splits one received message into its encoded packets. `compressed` is whether
/// NetworkSettings has been exchanged, after which every batch has a compression byte.
pub fn decode(message: &[u8], compressed: bool) -> Result<Vec<Vec<u8>>, BatchError> {
    let decompressed;
    let batch = if compressed {
        let (&algorithm, data) = message.split_first().ok_or(BatchError::Empty)?;
        match algorithm {
            UNCOMPRESSED => data,
            FLATE => {
                decompressed = inflate(data)?;
                &decompressed[..]
            }
            SNAPPY => {
                decompressed = unsnap(data)?;
                &decompressed[..]
            }
            other => return Err(BatchError::UnknownAlgorithm(other)),
        }
    } else {
        message
    };
    if batch.len() > MAX_BATCH_SIZE {
        return Err(BatchError::TooLarge);
    }

    let mut reader = Reader::new(batch);
    let mut packets = Vec::new();
    while !reader.is_empty() {
        if packets.len() == MAX_BATCH_PACKETS {
            return Err(BatchError::TooManyPackets);
        }
        packets.push(reader.byte_array()?.to_vec());
    }
    Ok(packets)
}

fn inflate(data: &[u8]) -> Result<Vec<u8>, BatchError> {
    let mut batch = Vec::new();
    DeflateDecoder::new(data)
        .take(MAX_BATCH_SIZE as u64 + 1)
        .read_to_end(&mut batch)
        .map_err(|err| BatchError::Decompress(err.to_string()))?;
    if batch.len() > MAX_BATCH_SIZE {
        return Err(BatchError::TooLarge);
    }
    Ok(batch)
}

fn unsnap(data: &[u8]) -> Result<Vec<u8>, BatchError> {
    let len =
        snap::raw::decompress_len(data).map_err(|err| BatchError::Decompress(err.to_string()))?;
    if len > MAX_BATCH_SIZE {
        return Err(BatchError::TooLarge);
    }
    snap::raw::Decoder::new()
        .decompress_vec(data)
        .map_err(|err| BatchError::Decompress(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets() -> Vec<Vec<u8>> {
        vec![
            b"first".to_vec(),
            vec![7; 3000],
            Vec::new(),
            b"last".to_vec(),
        ]
    }

    #[test]
    fn first_client_message_is_an_uncompressed_batch() {
        // RequestNetworkSettings(2193), exactly as a 1.26.51 client sent it.
        let message = [0x06, 0xC1, 0x01, 0x00, 0x00, 0x08, 0x91];
        assert_eq!(
            decode(&message, false).unwrap(),
            vec![message[1..].to_vec()]
        );
    }

    #[test]
    fn round_trips_with_every_algorithm() {
        let packets = packets();
        for compression in [
            None,
            Some(Compression {
                algorithm: CompressionAlgorithm::Flate,
                threshold: 1,
            }),
            Some(Compression {
                algorithm: CompressionAlgorithm::Snappy,
                threshold: 1,
            }),
        ] {
            let message = encode(packets.iter().map(Vec::as_slice), compression).unwrap();
            if let Some(compression) = compression {
                assert_eq!(message[0], compression.algorithm.prefix());
                assert!(message.len() < 3000, "the repeated bytes should compress");
            }
            assert_eq!(decode(&message, compression.is_some()).unwrap(), packets);
        }
    }

    #[test]
    fn small_batches_skip_compression() {
        let compression = Some(Compression {
            algorithm: CompressionAlgorithm::Flate,
            threshold: 256,
        });
        let message = encode([&b"tiny"[..]], compression).unwrap();
        assert_eq!(message, [UNCOMPRESSED, 4, b't', b'i', b'n', b'y']);

        let disabled = Some(Compression {
            algorithm: CompressionAlgorithm::Flate,
            threshold: 0,
        });
        assert_eq!(
            encode([&[0u8; 1000][..]], disabled).unwrap()[0],
            UNCOMPRESSED
        );
    }

    #[test]
    fn rejects_malformed_batches() {
        assert!(matches!(decode(&[], true), Err(BatchError::Empty)));
        assert!(matches!(
            decode(&[0x07, 1, 2], true),
            Err(BatchError::UnknownAlgorithm(7))
        ));
        assert!(matches!(
            decode(&[5, 1, 2], false),
            Err(BatchError::Decode(DecodeError::UnexpectedEnd))
        ));
        // 0x07 opens a DEFLATE block of the reserved type 3.
        assert!(matches!(
            decode(&[FLATE, 0x07, 0x00], true),
            Err(BatchError::Decompress(_))
        ));

        let too_many = vec![0u8; MAX_BATCH_PACKETS + 1];
        assert!(matches!(
            decode(&too_many, false),
            Err(BatchError::TooManyPackets)
        ));
    }

    #[test]
    fn decompression_is_bounded() {
        let mut encoder = DeflateEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&vec![0; MAX_BATCH_SIZE + 1]).unwrap();
        let mut message = vec![FLATE];
        message.extend(encoder.finish().unwrap());
        assert!(matches!(decode(&message, true), Err(BatchError::TooLarge)));
    }
}
