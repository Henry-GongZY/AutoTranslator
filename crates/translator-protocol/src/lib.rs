//! Wire types shared by `translator-core` and the native clients.
//!
//! The message definitions live in `proto/translator.proto` and are compiled by
//! `build.rs`. Every transport frame is a 4-byte big-endian length prefix
//! followed by one `Envelope`.

pub mod framing;

/// Bumped on every breaking wire change. Clients and the core refuse to talk to
/// each other when this does not match.
pub const PROTOCOL_VERSION: u32 = 1;

pub const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\translator-core-v1";
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/translator-core-v1.sock";

include!(concat!(env!("OUT_DIR"), "/translator.v1.rs"));
