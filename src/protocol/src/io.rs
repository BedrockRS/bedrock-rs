//! Bedrock's binary encoding primitives.

/// Errors from decoding malformed data.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("unexpected end of data")]
    UnexpectedEnd,
    #[error("variable-length integer is too long")]
    VarIntTooLong,
    #[error("string is not valid UTF-8")]
    InvalidUtf8,
    #[error("{0} unexpected trailing bytes")]
    TrailingBytes(usize),
    #[error("invalid {field}: {value}")]
    InvalidValue { field: &'static str, value: i64 },
}

/// Reads Bedrock-encoded values from a byte slice.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    pub fn remaining(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Fails unless every byte has been read.
    pub fn finish(self) -> Result<(), DecodeError> {
        match self.data.len() {
            0 => Ok(()),
            trailing => Err(DecodeError::TrailingBytes(trailing)),
        }
    }

    pub fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        if self.data.len() < len {
            return Err(DecodeError::UnexpectedEnd);
        }
        let (head, rest) = self.data.split_at(len);
        self.data = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut array = [0; N];
        array.copy_from_slice(self.take(N)?);
        Ok(array)
    }

    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }

    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        Ok(self.u8()? != 0)
    }

    pub fn u16_le(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32_le(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn i32_le(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    pub fn i32_be(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    pub fn u64_le(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub fn f32_le(&mut self) -> Result<f32, DecodeError> {
        Ok(f32::from_le_bytes(self.array()?))
    }

    /// Unsigned LEB128, at most 5 bytes.
    pub fn var_u32(&mut self) -> Result<u32, DecodeError> {
        let mut value = 0u32;
        for shift in (0..35).step_by(7) {
            let byte = self.u8()?;
            // The fifth byte may only carry the top four bits.
            if shift == 28 && byte > 0x0F {
                return Err(DecodeError::VarIntTooLong);
            }
            value |= u32::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(DecodeError::VarIntTooLong)
    }

    /// Zigzag-encoded signed LEB128.
    pub fn var_i32(&mut self) -> Result<i32, DecodeError> {
        let value = self.var_u32()?;
        Ok((value >> 1) as i32 ^ -((value & 1) as i32))
    }

    /// Unsigned LEB128, at most 10 bytes.
    pub fn var_u64(&mut self) -> Result<u64, DecodeError> {
        let mut value = 0u64;
        for shift in (0..70).step_by(7) {
            let byte = self.u8()?;
            // The tenth byte may only carry the top bit.
            if shift == 63 && byte > 0x01 {
                return Err(DecodeError::VarIntTooLong);
            }
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(DecodeError::VarIntTooLong)
    }

    /// Zigzag-encoded signed LEB128.
    pub fn var_i64(&mut self) -> Result<i64, DecodeError> {
        let value = self.var_u64()?;
        Ok((value >> 1) as i64 ^ -((value & 1) as i64))
    }

    /// Bytes prefixed with their varuint32 length.
    pub fn byte_array(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.var_u32()?;
        self.take(len as usize)
    }

    /// UTF-8 text prefixed with its varuint32 length.
    pub fn string(&mut self) -> Result<&'a str, DecodeError> {
        std::str::from_utf8(self.byte_array()?).map_err(|_| DecodeError::InvalidUtf8)
    }

    /// A UUID's 16 bytes in wire order.
    pub fn uuid(&mut self) -> Result<[u8; 16], DecodeError> {
        self.array()
    }
}

/// Writes Bedrock-encoded values into a growing buffer.
#[derive(Debug, Clone, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    pub fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    pub fn u16_le(&mut self, value: u16) {
        self.raw(&value.to_le_bytes());
    }

    pub fn i16_le(&mut self, value: i16) {
        self.raw(&value.to_le_bytes());
    }

    pub fn u32_le(&mut self, value: u32) {
        self.raw(&value.to_le_bytes());
    }

    pub fn i32_le(&mut self, value: i32) {
        self.raw(&value.to_le_bytes());
    }

    pub fn i32_be(&mut self, value: i32) {
        self.raw(&value.to_be_bytes());
    }

    pub fn u64_le(&mut self, value: u64) {
        self.raw(&value.to_le_bytes());
    }

    pub fn i64_le(&mut self, value: i64) {
        self.raw(&value.to_le_bytes());
    }

    pub fn f32_le(&mut self, value: f32) {
        self.raw(&value.to_le_bytes());
    }

    pub fn f64_le(&mut self, value: f64) {
        self.raw(&value.to_le_bytes());
    }

    pub fn var_u32(&mut self, value: u32) {
        self.var_u64(u64::from(value));
    }

    pub fn var_i32(&mut self, value: i32) {
        self.var_u32(((value << 1) ^ (value >> 31)) as u32);
    }

    pub fn var_u64(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.buf.push(value as u8 | 0x80);
            value >>= 7;
        }
        self.buf.push(value as u8);
    }

    pub fn var_i64(&mut self, value: i64) {
        self.var_u64(((value << 1) ^ (value >> 63)) as u64);
    }

    pub fn byte_array(&mut self, bytes: &[u8]) {
        let len = u32::try_from(bytes.len()).expect("byte arrays are shorter than 4 GiB");
        self.var_u32(len);
        self.raw(bytes);
    }

    pub fn string(&mut self, value: &str) {
        self.byte_array(value.as_bytes());
    }

    pub fn uuid(&mut self, uuid: [u8; 16]) {
        self.raw(&uuid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_at_their_boundaries() {
        for value in [0, 1, 127, 128, 255, 16_383, 16_384, u32::MAX] {
            let mut writer = Writer::new();
            writer.var_u32(value);
            assert_eq!(Reader::new(&writer.into_bytes()).var_u32(), Ok(value));
        }
        for value in [0, -1, 1, i32::MIN, i32::MAX] {
            let mut writer = Writer::new();
            writer.var_i32(value);
            assert_eq!(Reader::new(&writer.into_bytes()).var_i32(), Ok(value));
        }
        for value in [0, u64::MAX, 1 << 63] {
            let mut writer = Writer::new();
            writer.var_u64(value);
            assert_eq!(Reader::new(&writer.into_bytes()).var_u64(), Ok(value));
        }
        for value in [i64::MIN, -1, i64::MAX] {
            let mut writer = Writer::new();
            writer.var_i64(value);
            assert_eq!(Reader::new(&writer.into_bytes()).var_i64(), Ok(value));
        }
    }

    #[test]
    fn varint_encodings_match_the_wire() {
        let mut writer = Writer::new();
        writer.var_u32(193);
        writer.var_i32(-1);
        writer.var_i32(41);
        assert_eq!(writer.into_bytes(), [0xC1, 0x01, 0x01, 0x52]);
    }

    #[test]
    fn overlong_varints_are_rejected() {
        assert_eq!(
            Reader::new(&[0xFF, 0xFF, 0xFF, 0xFF, 0x1F]).var_u32(),
            Err(DecodeError::VarIntTooLong)
        );
        assert_eq!(
            Reader::new(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]).var_u32(),
            Err(DecodeError::VarIntTooLong)
        );
        assert_eq!(
            Reader::new(&[0x80]).var_u32(),
            Err(DecodeError::UnexpectedEnd)
        );
    }

    #[test]
    fn strings_are_length_prefixed_utf8() {
        let mut writer = Writer::new();
        writer.string("héllo");
        let bytes = writer.into_bytes();
        assert_eq!(bytes[0], 6);
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.string(), Ok("héllo"));
        assert_eq!(reader.finish(), Ok(()));
        assert_eq!(
            Reader::new(&[1, 0xFF]).string(),
            Err(DecodeError::InvalidUtf8)
        );
        assert_eq!(
            Reader::new(&[1, 2, 3]).finish(),
            Err(DecodeError::TrailingBytes(3))
        );
    }
}
