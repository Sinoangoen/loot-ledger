//! A bounds-checked big-endian reader over a byte slice.
//!
//! The original JavaScript implementation throws on out-of-bounds reads and
//! catches at the top level. Here every read returns a `Result`, so a truncated
//! or hostile packet can never abort the capture loop: it just fails to decode
//! and the frame is dropped.

use std::fmt;

/// Why a packet could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// A read ran past the end of the buffer.
    Eof {
        /// Human-readable description of what was being read.
        wanted: &'static str,
        /// Bytes still available when the read was attempted.
        available: usize,
    },
    /// A length field was negative or absurd.
    BadLength {
        /// Human-readable description of the field.
        field: &'static str,
        /// The offending value.
        value: i64,
    },
    /// A parameter carried a type code we do not decode.
    UnknownType(u8),
    /// A string field was not valid UTF-8.
    InvalidUtf8,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Eof { wanted, available } => {
                write!(f, "out of bounds reading {wanted} ({available} bytes left)")
            }
            ParseError::BadLength { field, value } => {
                write!(f, "invalid length for {field}: {value}")
            }
            ParseError::UnknownType(t) => write!(f, "unknown parameter type 0x{t:02x}"),
            ParseError::InvalidUtf8 => write!(f, "string field was not valid UTF-8"),
        }
    }
}

impl std::error::Error for ParseError {}

pub type Result<T> = std::result::Result<T, ParseError>;

/// Cursor over a borrowed byte slice. All multi-byte integers are big-endian,
/// matching Photon's wire format.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    /// Current offset from the start of the buffer.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Total length of the underlying buffer.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// True when every byte has been consumed.
    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> &'a [u8] {
        &self.buf[self.pos.min(self.buf.len())..]
    }

    fn take(&mut self, n: usize, wanted: &'static str) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.buf.len())
            .ok_or(ParseError::Eof {
                wanted,
                available: self.buf.len().saturating_sub(self.pos),
            })?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1, "u8")?[0])
    }

    pub fn read_i8(&mut self) -> Result<i8> {
        Ok(self.read_u8()? as i8)
    }

    pub fn read_i16(&mut self) -> Result<i16> {
        let b = self.take(2, "i16")?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    pub fn read_u16(&mut self) -> Result<u16> {
        let b = self.take(2, "u16")?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn read_i32(&mut self) -> Result<i32> {
        let b = self.take(4, "i32")?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn read_u32(&mut self) -> Result<u32> {
        let b = self.take(4, "u32")?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn read_i64(&mut self) -> Result<i64> {
        let b = self.take(8, "i64")?;
        Ok(i64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn read_f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.read_u32()?))
    }

    pub fn read_f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.read_u64()?))
    }

    pub fn read_u64(&mut self) -> Result<u64> {
        let b = self.take(8, "u64")?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Read a byte slice of exactly `n` bytes.
    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n, "bytes")
    }

    /// Read the rest of the buffer.
    pub fn read_to_end(&mut self) -> &'a [u8] {
        let out = self.remaining();
        self.pos = self.buf.len();
        out
    }

    /// Advance the cursor by `n` bytes without inspecting them.
    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n, "skip").map(|_| ())
    }

    /// Read a `u16`-length-prefixed, UTF-8 string.
    ///
    /// Photon encodes strings as a 2-byte length followed by raw bytes.
    pub fn read_string(&mut self) -> Result<&'a str> {
        let len = self.read_u16()? as usize;
        let bytes = self.read_bytes(len)?;
        std::str::from_utf8(bytes).map_err(|_| ParseError::InvalidUtf8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_big_endian_integers() {
        let mut r = Reader::new(&[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(r.read_u16().unwrap(), 0x0102);
        assert_eq!(r.read_i16().unwrap(), 0x0304);
    }

    #[test]
    fn reads_full_width_scalars() {
        let mut r = Reader::new(&[0xff; 8]);
        assert_eq!(r.read_i64().unwrap(), -1);
        let mut r = Reader::new(&[0xff; 4]);
        assert_eq!(r.read_i32().unwrap(), -1);
        let mut r = Reader::new(&[0xff; 4]);
        assert_eq!(r.read_f32().unwrap().to_bits(), u32::MAX);
    }

    #[test]
    fn read_string_round_trips() {
        let mut data = Vec::new();
        data.extend_from_slice(&4u16.to_be_bytes());
        data.extend_from_slice(b"Grim");
        let mut r = Reader::new(&data);
        assert_eq!(r.read_string().unwrap(), "Grim");
        assert!(r.is_empty());
    }

    #[test]
    fn overrun_returns_error_not_panic() {
        let mut r = Reader::new(&[0x01]);
        assert!(matches!(r.read_u32(), Err(ParseError::Eof { .. })));
        // The cursor must not have moved past the end.
        assert_eq!(r.position(), 0);
    }

    #[test]
    fn string_length_cannot_exceed_buffer() {
        // Claims a 65535-byte string but supplies 2 bytes.
        let data = [0xffu8, 0xff, b'h', b'i'];
        let mut r = Reader::new(&data);
        assert!(matches!(r.read_string(), Err(ParseError::Eof { .. })));
    }

    #[test]
    fn read_to_end_drains() {
        let mut r = Reader::new(&[1, 2, 3, 4]);
        assert_eq!(r.read_to_end(), &[1, 2, 3, 4]);
        assert!(r.is_empty());
        assert_eq!(r.read_to_end(), &[] as &[u8]);
    }
}
