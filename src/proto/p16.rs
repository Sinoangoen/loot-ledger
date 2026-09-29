//! Photon binary protocol (a.k.a. "Protocol16") parameter tables.
//!
//! An incoming command payload is a one-byte message code followed by a
//! *parameter table*: a 16-bit count, then that many `(id, type, value)`
//! triples. This module decodes the table into a [`Value`] tree.

use super::reader::{ParseError, Reader, Result};

/// Parameter type codes as they appear on the wire.
///
/// The codes modelled here are the ones the reference implementation decodes
/// plus the two fixed-width scalars (`Double`, `EventDate`) whose layouts are
/// unambiguous. Codes that are *not* listed here cause the whole table to fail
/// with [`ParseError::UnknownType`] rather than guessing at a layout — a
/// dropped packet is always better than a mis-parsed one.
pub mod ty {
    pub const NIL: u8 = 0x2a;
    pub const DICTIONARY: u8 = 0x44;
    pub const STRING_SLICE: u8 = 0x61;
    pub const INT8: u8 = 0x62;
    pub const CUSTOM: u8 = 0x63;
    pub const DOUBLE: u8 = 0x64;
    pub const EVENT_DATE: u8 = 0x65;
    pub const FLOAT32: u8 = 0x66;
    pub const HASHTABLE: u8 = 0x68;
    pub const INT32: u8 = 0x69;
    pub const INT16: u8 = 0x6b;
    pub const INT64: u8 = 0x6c;
    pub const INT32_SLICE: u8 = 0x6e;
    pub const BOOLEAN: u8 = 0x6f;
    pub const STRING: u8 = 0x73;
    pub const INT8_SLICE: u8 = 0x78;
    pub const SLICE: u8 = 0x79;
    pub const OBJECT_SLICE: u8 = 0x7a;
}

/// Photon command types, as they appear in a packet's command loop.
pub mod command {
    pub const ACKNOWLEDGE: u8 = 0x01;
    pub const CONNECT: u8 = 0x02;
    pub const VERIFY_CONNECT: u8 = 0x03;
    pub const DISCONNECT: u8 = 0x04;
    pub const PING: u8 = 0x05;
    pub const SEND_RELIABLE: u8 = 0x06;
    pub const SEND_UNRELIABLE: u8 = 0x07;
    pub const SEND_RELIABLE_FRAGMENT: u8 = 0x08;
}

/// Photon message types, carried in the second byte of a reliable payload.
pub mod msg {
    pub const OPERATION_REQUEST: u8 = 0x02;
    pub const OPERATION_RESPONSE: u8 = 0x03;
    pub const EVENT_DATA: u8 = 0x04;
    pub const INTERNAL_OPERATION_REQUEST: u8 = 0x06;
    pub const INTERNAL_OPERATION_RESPONSE: u8 = 0x07;
}

/// A decoded parameter value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `Null` (type code `0x2a`).
    Nil,
    Bool(bool),
    /// Signed 8-bit integer.
    ///
    /// The JavaScript reference reads this byte as *unsigned*. Albion sends
    /// booleans as [`Value::Bool`] and genuine small counts as wider integers,
    /// so the two interpretations only diverge above 127; we follow the
    /// specification and read it signed.
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Float32(f32),
    Double(f64),
    String(String),
    /// A run of raw bytes (`byte[]`), length-prefixed with a 32-bit count.
    ByteSlice(Vec<u8>),
    /// A homogeneous array with an explicit element type.
    Slice(Vec<Value>),
    /// A key/value map, keys of any supported type.
    Dictionary(Vec<(Value, Value)>),
}

impl Value {
    /// Interpret the value as a boolean.
    ///
    /// Photon sends real booleans as [`Value::Bool`], but several Albion
    /// events use an integer flag instead, so any non-zero number counts as
    /// true. This mirrors the reference implementation's `if (param)` checks.
    pub fn as_bool(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Int8(n) => *n != 0,
            Value::Int16(n) => *n != 0,
            Value::Int32(n) => *n != 0,
            Value::Int64(n) => *n != 0,
            _ => false,
        }
    }

    /// Interpret the value as a 64-bit integer, if it is an integer at all.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int8(n) => Some(*n as i64),
            Value::Int16(n) => Some(*n as i64),
            Value::Int32(n) => Some(*n as i64),
            Value::Int64(n) => Some(*n),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    /// Interpret the value as a string slice, if it is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// A short, safe rendering for diagnostics and API responses.
    pub fn to_display_string(&self) -> String {
        match self {
            Value::Nil => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Int8(n) => n.to_string(),
            Value::Int16(n) => n.to_string(),
            Value::Int32(n) => n.to_string(),
            Value::Int64(n) => n.to_string(),
            Value::Float32(f) => f.to_string(),
            Value::Double(d) => d.to_string(),
            Value::String(s) => s.clone(),
            Value::ByteSlice(b) => format!("<{} bytes>", b.len()),
            Value::Slice(v) => format!("<{} items>", v.len()),
            Value::Dictionary(d) => format!("<{} entries>", d.len()),
        }
    }
}

/// A decoded parameter table: parameter id to value, in wire order.
///
/// Lookups are a linear scan. Tables hold a few dozen entries at most and the
/// keys are `u8`, so this is faster in practice than hashing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Params(Vec<(u8, Value)>);

impl Params {
    /// Build a table from `(id, value)` pairs in wire order.
    pub fn from_pairs(pairs: Vec<(u8, Value)>) -> Params {
        Params(pairs)
    }

    /// Look up a parameter by id.
    pub fn get(&self, id: u8) -> Option<&Value> {
        self.0.iter().find(|(k, _)| *k == id).map(|(_, v)| v)
    }

    /// Look up a parameter and require it to be a string.
    pub fn get_str(&self, id: u8) -> Option<&str> {
        self.get(id).and_then(Value::as_str)
    }

    /// Look up a parameter and require it to be a number.
    pub fn get_i64(&self, id: u8) -> Option<i64> {
        self.get(id).and_then(Value::as_i64)
    }

    /// Look up a parameter and evaluate it as a flag.
    pub fn get_bool(&self, id: u8) -> Option<bool> {
        self.get(id).map(Value::as_bool)
    }

    /// Iterate over every entry in wire order.
    pub fn iter(&self) -> impl Iterator<Item = (u8, &Value)> {
        self.0.iter().map(|(k, v)| (*k, v))
    }

    /// Number of parameters.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when the table carried no parameters.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Decode a parameter table.
///
/// A count of `-1` means "no parameters" and is accepted as an empty table.
pub fn decode_param_table(r: &mut Reader<'_>) -> Result<Params> {
    let count = r.read_i16()?;

    if count < 0 {
        return Ok(Params::default());
    }

    let mut out = Params(Vec::with_capacity(count as usize));

    for _ in 0..count {
        let id = r.read_u8()?;
        let ty = r.read_u8()?;
        let value = decode_value(ty, r)?;
        out.0.push((id, value));
    }

    Ok(out)
}

/// Decode a single typed value.
pub fn decode_value(ty: u8, r: &mut Reader<'_>) -> Result<Value> {
    let v = match ty {
        ty::NIL | 0 => Value::Nil,
        ty::BOOLEAN => Value::Bool(r.read_u8()? != 0),
        ty::INT8 => Value::Int8(r.read_i8()?),
        ty::INT16 => Value::Int16(r.read_i16()?),
        ty::INT32 => Value::Int32(r.read_i32()?),
        ty::INT64 | ty::EVENT_DATE => Value::Int64(r.read_i64()?),
        ty::FLOAT32 => Value::Float32(r.read_f32()?),
        ty::DOUBLE => Value::Double(r.read_f64()?),
        ty::STRING => Value::String(r.read_string()?.to_owned()),

        // `byte[]` is length-prefixed with a 32-bit count.
        ty::INT8_SLICE => {
            let n = r.read_u32()? as usize;
            Value::ByteSlice(r.read_bytes(n)?.to_vec())
        }

        // `object[]` is length-prefixed with a 16-bit count followed by a
        // single element type shared by the whole array.
        ty::SLICE => {
            let n = r.read_u16()? as usize;
            let element = r.read_u8()?;
            let mut items = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                items.push(decode_value(element, r)?);
            }
            Value::Slice(items)
        }

        ty::DICTIONARY | ty::HASHTABLE => {
            let key_ty = r.read_u8()?;
            let val_ty = r.read_u8()?;
            let n = r.read_u16()? as usize;
            let mut entries = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                let k = decode_value(key_ty, r)?;
                let v = decode_value(val_ty, r)?;
                entries.push((k, v));
            }
            Value::Dictionary(entries)
        }

        ty::STRING_SLICE => {
            let n = r.read_u16()? as usize;
            let mut items = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                items.push(Value::String(r.read_string()?.to_owned()));
            }
            Value::Slice(items)
        }

        // Deliberately not modelled: `Custom`, `Int32[]` and `Object[]` have
        // layouts we have not been able to verify against real traffic. Guessing
        // would desynchronise the cursor and corrupt every parameter after it,
        // so we fail the table instead.
        other => return Err(ParseError::UnknownType(other)),
    };

    Ok(v)
}

/// A decoded operation request or response.
#[derive(Debug, Clone, PartialEq)]
pub struct Operation {
    /// Operation code carried in the first payload byte.
    pub code: u8,
    /// Decoded parameter table.
    pub params: Params,
}

/// A decoded event.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Event code carried in the first payload byte.
    ///
    /// Albion sends `1` for every event; the specific event is identified by
    /// the parameter with id [`EVENT_ID_KEY`].
    pub code: u8,
    /// Decoded parameter table.
    pub params: Params,
}

/// Parameter id that carries Albion's internal event identifier.
pub const EVENT_ID_KEY: u8 = 252;

/// Parameter id that carries Albion's internal operation identifier.
pub const OP_ID_KEY: u8 = 253;

/// Decode a Photon message: a one-byte code followed by a parameter table.
///
/// The message *kind* is not carried on the wire — it comes from the Photon
/// framing layer (see [`crate::proto::photon`]) — so the caller passes the
/// kind it observed and gets back a typed body.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Event(Event),
    Operation(Operation),
}

/// Decode the payload of a reliable message.
///
/// Returns `Ok(None)` for message kinds this application does not model
/// (keep-alive, disconnect, and the encrypted forms).
pub fn decode_body(payload: &[u8]) -> Result<Option<Body>> {
    // Reliable payloads start with a flag byte. 0xF3 and 0xFD both introduce a
    // serialised message; anything else is a control or acknowledgement that
    // happens to share the reliable command slot.
    let mut r = Reader::new(payload);
    let flag = r.read_u8()?;

    if !matches!(flag, 0xF3 | 0xFD) {
        return Ok(None);
    }

    let message_type = r.read_u8()?;

    // Messages with the high bit set are encrypted. We never read those, and
    // the payloads are opaque without a session key, so skip them.
    if message_type > 128 {
        return Ok(None);
    }

    match message_type {
        msg::EVENT_DATA => {
            let code = r.read_u8()?;
            let params = decode_param_table(&mut r)?;
            Ok(Some(Body::Event(Event { code, params })))
        }
        msg::OPERATION_REQUEST
        | msg::OPERATION_RESPONSE
        | msg::INTERNAL_OPERATION_REQUEST
        | msg::INTERNAL_OPERATION_RESPONSE => {
            let code = r.read_u8()?;
            if message_type == msg::OPERATION_RESPONSE {
                // Response: code, returnCode (u16), paramType, debugMessage.
                r.read_i16()?;
                let param_ty = r.read_u8()?;
                decode_value(param_ty, &mut r)?;
            }
            let params = decode_param_table(&mut r)?;
            Ok(Some(Body::Operation(Operation { code, params })))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a string parameter: id, type, u16 length, bytes.
    fn string_param(id: u8, s: &str) -> Vec<u8> {
        let mut v = vec![id, ty::STRING];
        v.extend_from_slice(&(s.len() as u16).to_be_bytes());
        v.extend_from_slice(s.as_bytes());
        v
    }

    /// Prefix a parameter table with its parameter *count* (not byte length).
    fn wrap_params(count: i16, body: &[u8]) -> Vec<u8> {
        let mut v = count.to_be_bytes().to_vec();
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn decodes_mixed_parameter_table() {
        let mut body = string_param(1, "Grim");
        body.extend_from_slice(&[2, ty::BOOLEAN, 1]);
        body.extend_from_slice(&[4, ty::INT32]);
        body.extend_from_slice(&1234i32.to_be_bytes());
        body.extend_from_slice(&[5, ty::INT32]);
        body.extend_from_slice(&3i32.to_be_bytes());

        let table = wrap_params(4, &body);
        let mut r = Reader::new(&table);
        let params = decode_param_table(&mut r).unwrap();

        assert_eq!(params.len(), 4);
        assert_eq!(params.get_str(1), Some("Grim"));
        assert_eq!(params.get_bool(2), Some(true));
        assert_eq!(params.get_i64(4), Some(1234));
        assert_eq!(params.get_i64(5), Some(3));
        assert!(r.is_empty());
    }

    #[test]
    fn negative_count_means_no_parameters() {
        let table = (-1i16).to_be_bytes().to_vec();
        let mut r = Reader::new(&table);
        let params = decode_param_table(&mut r).unwrap();
        assert!(params.is_empty());
    }

    #[test]
    fn decodes_slice_and_dictionary() {
        let mut body = vec![1, ty::SLICE];
        body.extend_from_slice(&2u16.to_be_bytes());
        body.push(ty::INT32);
        body.extend_from_slice(&10i32.to_be_bytes());
        body.extend_from_slice(&20i32.to_be_bytes());

        body.extend_from_slice(&[2, ty::DICTIONARY]);
        body.push(ty::STRING);
        body.push(ty::INT32);
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(&1u16.to_be_bytes());
        body.push(b'x');
        body.extend_from_slice(&7i32.to_be_bytes());

        let table = wrap_params(2, &body);
        let mut r = Reader::new(&table);
        let params = decode_param_table(&mut r).unwrap();

        match params.get(1).unwrap() {
            Value::Slice(items) => assert_eq!(items.len(), 2),
            other => panic!("expected slice, got {other:?}"),
        }
        match params.get(2).unwrap() {
            Value::Dictionary(d) => assert_eq!(d.len(), 1),
            other => panic!("expected dictionary, got {other:?}"),
        }
        assert!(r.is_empty());
    }

    #[test]
    fn unknown_type_fails_loudly_rather_than_guessing() {
        let body = vec![1, ty::CUSTOM, 0xff, 0xff];
        let table = wrap_params(1, &body);
        let mut r = Reader::new(&table);
        assert!(matches!(
            decode_param_table(&mut r),
            Err(ParseError::UnknownType(0x63))
        ));
    }

    #[test]
    fn truncated_table_errors_instead_of_panicking() {
        let body = string_param(1, "Grim");
        let mut table = 1i16.to_be_bytes().to_vec();
        table.extend_from_slice(&body);
        table.truncate(table.len() - 3); // truncate mid-string

        let mut r = Reader::new(&table);
        assert!(decode_param_table(&mut r).is_err());
    }

    #[test]
    fn decodes_event_body() {
        let mut payload = vec![0xF3, msg::EVENT_DATA, 1];
        let body = {
            let mut b = vec![EVENT_ID_KEY, ty::INT32];
            b.extend_from_slice(&275i32.to_be_bytes());
            b
        };
        payload.extend_from_slice(&wrap_params(1, &body));

        let decoded = decode_body(&payload).unwrap().unwrap();
        match decoded {
            Body::Event(e) => {
                assert_eq!(e.code, 1);
                assert_eq!(e.params.get_i64(EVENT_ID_KEY), Some(275));
            }
            other => panic!("expected event, got {other:?}"),
        }
    }

    #[test]
    fn decodes_operation_request_body() {
        let mut payload = vec![0xF3, msg::OPERATION_REQUEST, 29];
        payload.extend_from_slice(&wrap_params(0, &[]));

        let decoded = decode_body(&payload).unwrap().unwrap();
        match decoded {
            Body::Operation(o) => {
                assert_eq!(o.code, 29);
                assert!(o.params.is_empty());
            }
            other => panic!("expected operation, got {other:?}"),
        }
    }

    #[test]
    fn skips_encrypted_and_unknown_messages() {
        assert!(decode_body(&[0xF3, 0xFF]).unwrap().is_none());
        assert!(decode_body(&[0x10, 0x01, 0x02]).unwrap().is_none());
    }
}
