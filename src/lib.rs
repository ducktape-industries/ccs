//! `ccs` as a library: everything the command line does, reachable without a
//! process boundary. The binary in `main.rs` is one thin caller; the app is
//! another, which keeps the gateway and the watcher in its own process.

pub mod api;
pub mod cli;
pub mod cmd;
pub mod codex;
pub mod creds;
pub mod env;
pub mod fsx;
pub mod lock;
pub mod login;
pub mod model;
pub mod notify;
pub mod pen;
pub mod picker;
pub mod render;
pub mod serve;
pub mod sha256;
pub mod stash;
pub mod usage;
pub mod watch;

pub mod route_picker;
pub mod routed;
pub mod routing;
