//! A Marv chat as messages: the page (a browser over marv-web's WebSocket, or Marv desktop's webview) sends
//! [`protocol::ClientMessage`]s and gets JSON events back, approvals included. [`host::run_chat`] is the one place a
//! session is driven; the transports only carry messages.

pub mod approver;
pub mod events;
pub mod host;
pub mod protocol;
