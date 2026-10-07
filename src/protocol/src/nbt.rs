//! Named Binary Tag (NBT) values in Bedrock's network encoding.
//!
//! The network flavor is little-endian with variable-length integers: `Int`
//! and `Long` are zigzag varints, string lengths are varuint32, and list
//! lengths are written like `Int`. Only encoding is implemented so far.

use crate::io::Writer;

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
    String = 8,
    List = 9,
    Compound = 10,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tag {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    String(String),
    /// Every element must be of the given kind, which is written even for an empty list.
    List(TagKind, Vec<Tag>),
    Compound(Compound),
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
            Self::String(_) => TagKind::String,
            Self::List(..) => TagKind::List,
            Self::Compound(_) => TagKind::Compound,
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

    /// Writes this compound as a nameless root tag in network encoding.
    pub fn write_network(&self, writer: &mut Writer) {
        writer.u8(TagKind::Compound as u8);
        writer.string("");
        write_entries(writer, self);
    }
}

fn write_entries(writer: &mut Writer, compound: &Compound) {
    for (name, tag) in &compound.0 {
        writer.u8(tag.kind() as u8);
        writer.string(name);
        write_payload(writer, tag);
    }
    writer.u8(TagKind::End as u8);
}

fn write_payload(writer: &mut Writer, tag: &Tag) {
    match tag {
        Tag::Byte(value) => writer.u8(*value as u8),
        Tag::Short(value) => writer.i16_le(*value),
        Tag::Int(value) => writer.var_i32(*value),
        Tag::Long(value) => writer.var_i64(*value),
        Tag::Float(value) => writer.f32_le(*value),
        Tag::Double(value) => writer.f64_le(*value),
        Tag::String(value) => writer.string(value),
        Tag::List(kind, items) => {
            debug_assert!(items.iter().all(|item| item.kind() == *kind));
            writer.u8(*kind as u8);
            writer.var_i32(i32::try_from(items.len()).expect("lists are shorter than 2^31"));
            for item in items {
                write_payload(writer, item);
            }
        }
        Tag::Compound(compound) => write_entries(writer, compound),
    }
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
}
