//! Small value types shared by packets.

use uuid::Uuid;

use crate::io::{DecodeError, Reader, Writer};

/// A block position; each coordinate is a zigzag varint on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockPos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl BlockPos {
    pub fn write(&self, writer: &mut Writer) {
        writer.var_i32(self.x);
        writer.var_i32(self.y);
        writer.var_i32(self.z);
    }
}

/// A position or direction in floating point, three little-endian f32s on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub fn write(&self, writer: &mut Writer) {
        writer.f32_le(self.x);
        writer.f32_le(self.y);
        writer.f32_le(self.z);
    }
}

impl Vec3 {
    pub fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            x: reader.f32_le()?,
            y: reader.f32_le()?,
            z: reader.f32_le()?,
        })
    }

    /// Whether every coordinate is a finite number.
    pub fn is_finite(&self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
}

/// A pair of floats, such as a rotation or a 2D input vector: two little-endian f32s.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            x: reader.f32_le()?,
            y: reader.f32_le()?,
        })
    }

    pub fn write(&self, writer: &mut Writer) {
        writer.f32_le(self.x);
        writer.f32_le(self.y);
    }
}

/// A UUID's bytes in Bedrock's wire order: the most significant half, then the
/// least significant half, each as a little-endian u64.
pub fn uuid_bytes(uuid: &Uuid) -> [u8; 16] {
    let (high, low) = uuid.as_u64_pair();
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&high.to_le_bytes());
    bytes[8..].copy_from_slice(&low.to_le_bytes());
    bytes
}

impl BlockPos {
    pub fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            x: reader.var_i32()?,
            y: reader.var_i32()?,
            z: reader.var_i32()?,
        })
    }

    /// The block a point is in.
    pub fn containing(position: Vec3) -> Self {
        Self {
            x: position.x.floor() as i32,
            y: position.y.floor() as i32,
            z: position.z.floor() as i32,
        }
    }
}

/// A chunk column's coordinates: block coordinates divided by 16, rounded down.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ChunkPos {
    pub x: i32,
    pub z: i32,
}

impl ChunkPos {
    pub fn new(x: i32, z: i32) -> Self {
        Self { x, z }
    }

    /// The chunk column a block is in.
    pub fn of_block(block: BlockPos) -> Self {
        Self::new(block.x >> 4, block.z >> 4)
    }

    /// Squared distance to `other`, in chunks.
    pub fn distance_squared(self, other: Self) -> i64 {
        let (dx, dz) = (
            i64::from(self.x) - i64::from(other.x),
            i64::from(self.z) - i64::from(other.z),
        );
        dx * dx + dz * dz
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_round_towards_negative_infinity() {
        let chunk = |x, z| ChunkPos::of_block(BlockPos::containing(Vec3 { x, y: 0.0, z }));
        assert_eq!(chunk(0.0, 15.99), ChunkPos::new(0, 0));
        assert_eq!(chunk(16.0, -0.01), ChunkPos::new(1, -1));
        assert_eq!(chunk(-16.0, -16.01), ChunkPos::new(-1, -2));
        assert_eq!(
            ChunkPos::new(0, 0).distance_squared(ChunkPos::new(3, -4)),
            25
        );
    }
}
