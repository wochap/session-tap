//! Process, filesystem, socket, and transport helpers shared by every
//! SessionTap binary. Each security-sensitive check has exactly one
//! implementation here; `sessiontap-core` stays free of IO side effects.

pub mod config;
pub mod fs;
pub mod http;
pub mod json;
pub mod multiplexer;
pub mod process;
pub mod socket;
pub mod sqlite;
pub mod token;
