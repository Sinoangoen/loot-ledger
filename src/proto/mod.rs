//! Wire-format decoding: bytes in, Albion protocol structures out.
//!
//! Two layers, matching the protocol's own structure:
//!
//! * [`photon`] — packet framing, fragment reassembly, CRC verification.
//! * [`p16`] — the serialised parameter table carried inside each message.

pub mod p16;
pub mod photon;
pub mod reader;

#[allow(unused_imports)]
pub use p16::{Body, Event, Operation, Params, Value};
#[allow(unused_imports)]
pub use photon::{ParseStats, Parser};
#[allow(unused_imports)]
pub use reader::ParseError;
