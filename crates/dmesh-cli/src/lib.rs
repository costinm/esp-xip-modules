//! Direct, bearer-neutral device-session client.
//!
//! `dmesh-cli` is the host-facing shell and library for QUIC-lite sessions
//! over a selected UART, UDP endpoint, or named device profile.  It owns no
//! managed UART forwarding. Physical UART ownership and diagnostics live in
//! `uart`; there is no standalone forwarding service or control socket.

pub mod client;
mod device;
mod http;
mod node_client;
mod schema;
pub mod uart;
pub mod xip_wire;

pub use client::{run_dmesh_cli, run_dmesh_cli_args};
pub use uart::{DeviceSession, DeviceSessionEvent};

/// Send a structured service record over the normal UART QUIC transport.
/// The returned bytes are the tagged response record, with no text conversion.
pub async fn request_uart_record(
    path: &str,
    baud: Option<u32>,
    record: &[u8],
    timeout: std::time::Duration,
) -> Result<Vec<u8>, String> {
    node_client::request_uart(path, baud, record, timeout)
        .await
        .map(|response| response.bytes)
}

/// Keep the UART owner active after a response to collect diagnostic sideband logs.
pub async fn request_uart_record_with_linger(
    path: &str,
    record: &[u8],
    timeout: std::time::Duration,
    linger: std::time::Duration,
) -> Result<Vec<u8>, String> {
    node_client::request_uart_monitor(path, None, || Ok(record.to_vec()), timeout,
        None, linger, |_| {}).await.map(|response| response.bytes)
}
