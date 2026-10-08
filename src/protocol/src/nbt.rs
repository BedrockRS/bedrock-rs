//! Named Binary Tag (NBT) values in Bedrock's two encodings.
//!
//! The network flavor is little-endian with variable-length integers: `Int`
//! and `Long` are zigzag varints, string lengths are varuint32, and list and
//! array lengths are written like `Int`. Only encoding is implemented.
//!
//! The disk flavor ([`Compound::write_le`], [`Compound::read_le`]) is what
//! worlds store in LevelDB: plain little-endian, with `u16` string lengths and
//! `i32` list and array lengths. Java Edition's NBT is the same layout in big
//! endian, so its crates (such as `fastnbt`) cannot read it.

use crate::io::{DecodeError, Reader, Writer};

/// How deep compounds and lists may nest when reading, so damaged or hostile
/// data cannot exhaust the stack. Vanilla allows 512, but each level is a
/// few stack frames, and 512 overflowed a debug build's test thread (found
/// 2026-10-08); world and item data nest a handful of levels at most.
const MAX_DEPTH: usize = 128;

/// Tag type IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagKind {
    End = 0,
    Byte = 1,
    Short = 2,
    Int = 3,
    Long = 4,
    Float = 5,
    Double = 6,
    ByteArray = 7,
    String = 8,
    List = 9,
    Compound = 10,
    IntArray = 11,
    LongArray = 12,
}

impl TagKind {
    fn from_u8(id: u8) -> Result<Self, DecodeError> {
        Ok(match id {
            0 => Self::End,
            1 => Self::Byte,
            2 => Self::Short,
            3 => Self::Int,
            4 => Self::Long,
            5 => Self::Float,
            6 => Self::Double,
            7 => Self::ByteArray,
            8 => Self::String,
            9 => Self::List,
            10 => Self::Compound,
            11 => Self::IntArray,
            12 => Self::LongArray,
            other => {
                return Err(DecodeError::InvalidValue {
                    field: "NBT tag type",
                    value: other.into(),
                });
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tag {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<u8>),
    String(String),
    /// Every element must be of the given kind, which is written even for an empty list.
    List(TagKind, Vec<Tag>),
    Compound(Compound),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

impl Tag {
    pub fn kind(&self) -> TagKind {
        match self {
            Self::Byte(_) => TagKind::Byte,
            Self::Short(_) => TagKind::Short,
            Self::Int(_) => TagKind::Int,
            Self::Long(_) => TagKind::Long,
            Self::Float(_) => TagKind::Float,
            Self::Double(_) => TagKind::Double,
            Self::ByteArray(_) => TagKind::ByteArray,
            Self::String(_) => TagKind::String,
            Self::List(..) => TagKind::List,
            Self::Compound(_) => TagKind::Compound,
            Self::IntArray(_) => TagKind::IntArray,
            Self::LongArray(_) => TagKind::LongArray,
        }
    }
}

/// A compound tag whose entries keep their insertion order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Compound(pub Vec<(String, Tag)>);

impl Compound {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, name: impl Into<String>, tag: Tag) -> Self {
        self.0.push((name.into(), tag));
        self
    }

    /// The entry named `name`, if there is one.
    pub fn get(&self, name: &str) -> Option<&Tag> {
        self.0
            .iter()
            .find(|(entry, _)| entry == name)
            .map(|(_, tag)| tag)
    }

    /// Sets the entry named `name`: in its place if there is one, keeping
    /// the order, otherwise at the end.
    pub fn set(&mut self, name: &str, tag: Tag) {
        match self.0.iter_mut().find(|(entry, _)| entry == name) {
            Some((_, slot)) => *slot = tag,
            None => self.0.push((name.to_owned(), tag)),
        }
    }

    /// Writes this compound as a nameless root tag in network encoding.
    pub fn write_network(&self, writer: &mut Writer) {
        writer.u8(TagKind::Compound as u8);
        writer.string("");
        write_entries(writer, self, Flavor::Network);
    }

    /// Writes this compound as a nameless root tag in disk (little-endian)
    /// encoding.
    pub fn write_le(&self, writer: &mut Writer) {
        writer.u8(TagKind::Compound as u8);
        write_le_string(writer, "");
        write_entries(writer, self, Flavor::Disk);
    }

    /// This compound as a nameless root tag in disk encoding.
    pub fn to_le_bytes(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        self.write_le(&mut writer);
        writer.into_bytes()
    }

    /// Reads one root compound in disk encoding, returning it with its
    /// name. Data after it is left for the caller, as in sub-chunk palettes,
    /// which are root compounds one after another.
    pub fn read_le(reader: &mut Reader<'_>) -> Result<(String, Self), DecodeError> {
        let kind = TagKind::from_u8(reader.u8()?)?;
        if kind != TagKind::Compound {
            return Err(DecodeError::InvalidValue {
                field: "NBT root tag type",
                value: kind as i64,
            });
        }
        let name = read_le_string(reader)?;
        let compound = read_compound(reader, 0)?;
        Ok((name, compound))
    }

    /// The root compound that is all of `bytes`, in disk encoding.
    pub fn from_le_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(bytes);
        let (_, compound) = Self::read_le(&mut reader)?;
        reader.finish()?;
        Ok(compound)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Network,
    Disk,
}

fn write_entries(writer: &mut Writer, compound: &Compound, flavor: Flavor) {
    for (name, tag) in &compound.0 {
        writer.u8(tag.kind() as u8);
        match flavor {
            Flavor::Network => writer.string(name),
            Flavor::Disk => write_le_string(writer, name),
        }
        write_payload(writer, tag, flavor);
    }
    writer.u8(TagKind::End as u8);
}

fn write_payload(writer: &mut Writer, tag: &Tag, flavor: Flavor) {
    let network = flavor == Flavor::Network;
    let length = |writer: &mut Writer, len: usize| {
        let len = i32::try_from(len).expect("NBT lists and arrays are shorter than 2^31");
        if network {
            writer.var_i32(len);
        } else {
            writer.i32_le(len);
        }
    };
    match tag {
        Tag::Byte(value) => writer.u8(*value as u8),
        Tag::Short(value) => writer.i16_le(*value),
        Tag::Int(value) if network => writer.var_i32(*value),
        Tag::Int(value) => writer.i32_le(*value),
        Tag::Long(value) if network => writer.var_i64(*value),
        Tag::Long(value) => writer.i64_le(*value),
        Tag::Float(value) => writer.f32_le(*value),
        Tag::Double(value) => writer.f64_le(*value),
        Tag::ByteArray(bytes) => {
            length(writer, bytes.len());
            writer.raw(bytes);
        }
        Tag::String(value) if network => writer.string(value),
        Tag::String(value) => write_le_string(writer, value),
        Tag::List(kind, items) => {
            debug_assert!(items.iter().all(|item| item.kind() == *kind));
            writer.u8(*kind as u8);
            length(writer, items.len());
            for item in items {
                write_payload(writer, item, flavor);
            }
        }
        Tag::Compound(compound) => write_entries(writer, compound, flavor),
        Tag::IntArray(values) => {
            length(writer, values.len());
            for value in values {
                if network {
                    writer.var_i32(*value);
                } else {
                    writer.i32_le(*value);
                }
            }
        }
        Tag::LongArray(values) => {
            length(writer, values.len());
            for value in values {
                if network {
                    writer.var_i64(*value);
                } else {
                    writer.i64_le(*value);
                }
            }
        }
    }
}

fn write_le_string(writer: &mut Writer, value: &str) {
    let len = u16::try_from(value.len()).expect("NBT strings are shorter than 64 KiB");
    writer.u16_le(len);
    writer.raw(value.as_bytes());
}

fn read_le_string(reader: &mut Reader<'_>) -> Result<String, DecodeError> {
    let len = usize::from(reader.u16_le()?);
    let bytes = reader.take(len)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::InvalidUtf8)
}

fn read_compound(reader: &mut Reader<'_>, depth: usize) -> Result<Compound, DecodeError> {
    let depth = deeper(depth)?;
    let mut compound = Compound::new();
    loop {
        let kind = TagKind::from_u8(reader.u8()?)?;
        if kind == TagKind::End {
            return Ok(compound);
        }
        let name = read_le_string(reader)?;
        let tag = read_payload(reader, kind, depth)?;
        compound.0.push((name, tag));
    }
}

fn deeper(depth: usize) -> Result<usize, DecodeError> {
    if depth >= MAX_DEPTH {
        return Err(DecodeError::InvalidValue {
            field: "NBT nesting depth",
            value: depth as i64,
        });
    }
    Ok(depth + 1)
}

/// A list or array length, which must be possible in the bytes left, each
/// element taking at least `element_size` bytes.
fn read_length(reader: &mut Reader<'_>, element_size: usize) -> Result<usize, DecodeError> {
    let len = reader.i32_le()?;
    let len = usize::try_from(len).map_err(|_| DecodeError::InvalidValue {
        field: "NBT length",
        value: len.into(),
    })?;
    if len.saturating_mul(element_size) > reader.remaining() {
        return Err(DecodeError::UnexpectedEnd);
    }
    Ok(len)
}

fn read_payload(reader: &mut Reader<'_>, kind: TagKind, depth: usize) -> Result<Tag, DecodeError> {
    let array = |reader: &mut Reader<'_>, size: usize| -> Result<Vec<u8>, DecodeError> {
        let len = read_length(reader, size)?;
        Ok(reader.take(len * size)?.to_vec())
    };
    Ok(match kind {
        TagKind::End => {
            return Err(DecodeError::InvalidValue {
                field: "NBT tag type",
                value: 0,
            });
        }
        TagKind::Byte => Tag::Byte(reader.u8()? as i8),
        TagKind::Short => Tag::Short(reader.u16_le()? as i16),
        TagKind::Int => Tag::Int(reader.i32_le()?),
        TagKind::Long => Tag::Long(reader.u64_le()? as i64),
        TagKind::Float => Tag::Float(reader.f32_le()?),
        TagKind::Double => Tag::Double(f64::from_bits(reader.u64_le()?)),
        TagKind::ByteArray => Tag::ByteArray(array(reader, 1)?),
        TagKind::String => Tag::String(read_le_string(reader)?),
        TagKind::List => {
            let element = TagKind::from_u8(reader.u8()?)?;
            // An empty list may say its elements are End tags.
            let len = read_length(reader, 1)?;
            if element == TagKind::End && len > 0 {
                return Err(DecodeError::InvalidValue {
                    field: "NBT list element type",
                    value: 0,
                });
            }
            let depth = deeper(depth)?;
            let items = (0..len)
                .map(|_| read_payload(reader, element, depth))
                .collect::<Result<_, _>>()?;
            Tag::List(element, items)
        }
        TagKind::Compound => Tag::Compound(read_compound(reader, depth)?),
        TagKind::IntArray => Tag::IntArray(
            array(reader, 4)?
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| i32::from_le_bytes(*bytes))
                .collect(),
        ),
        TagKind::LongArray => Tag::LongArray(
            array(reader, 8)?
                .as_chunks::<8>()
                .0
                .iter()
                .map(|bytes| i64::from_le_bytes(*bytes))
                .collect(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network(compound: &Compound) -> Vec<u8> {
        let mut writer = Writer::new();
        compound.write_network(&mut writer);
        writer.into_bytes()
    }

    #[test]
    fn empty_root_compound() {
        assert_eq!(network(&Compound::new()), [10, 0, 0]);
        assert_eq!(Compound::new().to_le_bytes(), [10, 0, 0, 0]);
    }

    #[test]
    fn values_use_the_network_encoding() {
        let compound = Compound::new()
            .with("i", Tag::Int(-2))
            .with("s", Tag::String("hi".into()))
            .with("l", Tag::List(TagKind::Compound, Vec::new()))
            .with("c", Tag::Compound(Compound::new().with("b", Tag::Byte(1))));
        assert_eq!(
            network(&compound),
            [
                10, 0, // root compound, empty name
                3, 1, b'i', 3, // Int "i" = zigzag(-2)
                8, 1, b's', 2, b'h', b'i', // String "s"
                9, 1, b'l', 10, 0, // empty List "l" of compounds
                10, 1, b'c', 1, 1, b'b', 1, 0, // Compound "c" { Byte "b" = 1 }
                0, // end of root
            ]
        );
    }

    #[test]
    fn values_use_the_disk_encoding() {
        let compound = Compound::new()
            .with("i", Tag::Int(-2))
            .with("s", Tag::String("hi".into()))
            .with("l", Tag::List(TagKind::Int, vec![Tag::Int(1)]));
        assert_eq!(
            compound.to_le_bytes(),
            [
                10, 0, 0, // root compound, empty name (u16 length)
                3, 1, 0, b'i', 0xFE, 0xFF, 0xFF, 0xFF, // Int "i" = -2, i32 LE
                8, 1, 0, b's', 2, 0, b'h', b'i', // String "s", u16 length
                9, 1, 0, b'l', 3, 1, 0, 0, 0, 1, 0, 0, 0, // List "l" of one Int
                0, // end of root
            ]
        );
    }

    #[test]
    fn every_tag_survives_the_disk_encoding() {
        let compound = Compound::new()
            .with("byte", Tag::Byte(-1))
            .with("short", Tag::Short(-300))
            .with("int", Tag::Int(i32::MIN))
            .with("long", Tag::Long(i64::MAX))
            .with("float", Tag::Float(1.5))
            .with("double", Tag::Double(-2.25))
            .with("bytes", Tag::ByteArray(vec![1, 2, 255]))
            .with("string", Tag::String("héllo".into()))
            .with(
                "list",
                Tag::List(TagKind::String, vec![Tag::String("a".into())]),
            )
            .with("empty", Tag::List(TagKind::End, Vec::new()))
            .with(
                "compound",
                Tag::Compound(Compound::new().with("x", Tag::Float(0.0))),
            )
            .with("ints", Tag::IntArray(vec![-1, 7]))
            .with("longs", Tag::LongArray(vec![i64::MIN]));
        let bytes = compound.to_le_bytes();
        assert_eq!(Compound::from_le_bytes(&bytes).unwrap(), compound);
        assert_eq!(
            compound.get("short"),
            Some(&Tag::Short(-300)),
            "entries are found by name"
        );
    }

    #[test]
    fn roots_are_read_one_at_a_time() {
        let first = Compound::new().with("n", Tag::Byte(1));
        let second = Compound::new().with("n", Tag::Byte(2));
        let mut bytes = first.to_le_bytes();
        bytes.extend(second.to_le_bytes());
        let mut reader = Reader::new(&bytes);
        assert_eq!(Compound::read_le(&mut reader).unwrap().1, first);
        assert_eq!(Compound::read_le(&mut reader).unwrap().1, second);
        assert!(reader.is_empty());
    }

    #[test]
    fn damaged_data_is_an_error_not_a_panic() {
        let bytes = Compound::new()
            .with("list", Tag::List(TagKind::Int, vec![Tag::Int(1); 4]))
            .to_le_bytes();
        for len in 0..bytes.len() {
            assert!(Compound::from_le_bytes(&bytes[..len]).is_err(), "{len}");
        }
        // Not a compound at the root, an unknown tag type, a huge length.
        assert!(Compound::from_le_bytes(&[8, 0, 0, 0, 0]).is_err());
        assert!(Compound::from_le_bytes(&[10, 0, 0, 99, 0, 0, 0]).is_err());
        assert!(Compound::from_le_bytes(&[10, 0, 0, 7, 0, 0, 0xFF, 0xFF, 0xFF, 0x7F, 0]).is_err());
        // Deep nesting stops before the stack does, but nesting within the
        // limit reads.
        let nested = |levels: usize| {
            let mut bytes = vec![10, 0, 0];
            bytes.extend([10, 0, 0].repeat(levels));
            bytes.extend(vec![0; levels + 1]);
            Compound::from_le_bytes(&bytes)
        };
        assert!(nested(MAX_DEPTH - 1).is_ok());
        assert!(nested(MAX_DEPTH).is_err());
        assert!(nested(100_000).is_err());
    }
}
