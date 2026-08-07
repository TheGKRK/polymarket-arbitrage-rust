//! Web dashboard (mirrors the Python `dashboard/` package): shared state,
//! the axum HTTP/WebSocket server, and the bridge from live bot components
//! into that state.

pub mod integration;
pub mod server;
pub mod state;
