//! `translator-core`: realtime system-audio subtitle engine.
//!
//! The core owns no UI and never touches platform audio APIs. It receives raw
//! PCM over a local pipe/socket, runs the VAD + recognition pipeline and streams
//! subtitle events back.

pub mod asr;
pub mod audio;
pub mod error;
pub mod ipc;
pub mod session;
pub mod subtitle;
pub mod vad;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
