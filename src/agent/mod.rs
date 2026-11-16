//! A channel for agents and scripts: `matrix-rat serve` runs the Matrix worker without
//! the screen and listens on a private socket; `send`, `read`, `listen` and `mcp` are
//! thin clients of that socket.
//!
//! - [`proto`]: what goes over the socket (one JSON object per line, each way), with
//!   its limits.
//! - [`socket`]: where the socket is, who may reach it, and its removal.
//! - [`serve`]: the program behind the socket. It owns no SDK type: it drives the
//!   worker through [`crate::matrix::MatrixCmd`] and [`crate::matrix::MatrixEvent`]
//!   like the app does, and its only requests are the four of the protocol. It never
//!   joins, leaves or creates a room, accepts no invitation and changes no setting.
//! - [`client`]: the other end of the socket, and the `send`, `read` and `listen`
//!   commands.
//! - [`mcp`]: an MCP server on stdin and stdout for one agent, bound to one room.
//!
//! Both ends treat each other as hostile: every line has a largest size, every wait a
//! limit, and text from Matrix is cleaned for a terminal before it leaves `serve`.
//! Nothing of it is logged beyond kinds and counts.

pub mod client;
pub mod mcp;
pub mod proto;
pub mod serve;
pub mod socket;

#[cfg(test)]
pub(crate) mod testing;
