//! WebSocket client transport for connecting to WebSocket-based MCP backends.
//!
//! Implements tower-mcp's [`ClientTransport`](tower_mcp::client::ClientTransport) trait over a WebSocket connection
//! using `tokio-tungstenite`. Messages are sent and received as text frames
//! containing JSON-RPC payloads.
//!
//! # Example
//!
//! ```rust,no_run
//! use mcp_proxy::ws_transport::WebSocketClientTransport;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let transport = WebSocketClientTransport::connect("ws://localhost:8080/ws").await?;
//! // Pass to McpProxy::builder().backend("name", transport).await
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A WebSocket client transport for MCP backend connections.
///
/// Connects to a WebSocket endpoint and exchanges JSON-RPC messages
/// as text frames. Supports `ws://` and `wss://` (TLS) URLs.
pub struct WebSocketClientTransport {
    sink: Arc<Mutex<futures_util::stream::SplitSink<WsStream, Message>>>,
    stream: Arc<Mutex<futures_util::stream::SplitStream<WsStream>>>,
    connected: Arc<AtomicBool>,
}

impl WebSocketClientTransport {
    /// Connect to a WebSocket endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error if the WebSocket handshake fails or the URL is invalid.
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let (ws_stream, _response) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| anyhow::anyhow!("WebSocket connection failed: {e}"))?;

        let (sink, stream) = ws_stream.split();

        Ok(Self {
            sink: Arc::new(Mutex::new(sink)),
            stream: Arc::new(Mutex::new(stream)),
            connected: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Connect to a WebSocket endpoint with a bearer token for authentication.
    ///
    /// The token is sent in the `Authorization` header during the handshake.
    pub async fn connect_with_bearer_token(url: &str, token: &str) -> anyhow::Result<Self> {
        use tokio_tungstenite::tungstenite::http::Request;

        let request = Request::builder()
            .uri(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tokio_tungstenite::tungstenite::handshake::client::generate_key(),
            )
            .body(())
            .map_err(|e| anyhow::anyhow!("invalid WebSocket request: {e}"))?;

        let (ws_stream, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| anyhow::anyhow!("WebSocket connection failed: {e}"))?;

        let (sink, stream) = ws_stream.split();

        Ok(Self {
            sink: Arc::new(Mutex::new(sink)),
            stream: Arc::new(Mutex::new(stream)),
            connected: Arc::new(AtomicBool::new(true)),
        })
    }
}

#[async_trait]
impl tower_mcp::client::ClientTransport for WebSocketClientTransport {
    async fn send(&mut self, message: &str) -> tower_mcp::error::Result<()> {
        let mut sink = self.sink.lock().await;
        sink.send(Message::Text(message.into()))
            .await
            .map_err(|e| tower_mcp::error::Error::Transport(e.to_string()))?;
        Ok(())
    }

    async fn recv(&mut self) -> tower_mcp::error::Result<Option<String>> {
        let mut stream = self.stream.lock().await;
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => return Ok(Some(text.as_str().to_owned())),
                Some(Ok(Message::Close(_))) | None => {
                    self.connected.store(false, Ordering::SeqCst);
                    return Ok(None);
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {
                    // Pong is handled automatically by tungstenite; skip control frames
                    continue;
                }
                Some(Ok(Message::Binary(data))) => {
                    // Try to interpret binary as UTF-8 text
                    let text = std::str::from_utf8(&data)
                        .map_err(|e| tower_mcp::error::Error::Transport(e.to_string()))?;
                    return Ok(Some(text.to_string()));
                }
                Some(Err(e)) => {
                    self.connected.store(false, Ordering::SeqCst);
                    return Err(tower_mcp::error::Error::Transport(e.to_string()));
                }
            }
        }
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    async fn close(&mut self) -> tower_mcp::error::Result<()> {
        self.connected.store(false, Ordering::SeqCst);
        let mut sink = self.sink.lock().await;
        let _ = sink.send(Message::Close(None)).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_fails_with_invalid_url() {
        let result = WebSocketClientTransport::connect("ws://127.0.0.1:1").await;
        let err = result.err().expect("should fail").to_string();
        assert!(
            err.contains("WebSocket connection failed"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn connect_with_bearer_token_fails_with_invalid_url() {
        let result =
            WebSocketClientTransport::connect_with_bearer_token("ws://127.0.0.1:1", "tok").await;
        let err = result.err().expect("should fail").to_string();
        assert!(
            err.contains("WebSocket connection failed"),
            "unexpected error: {err}"
        );
    }
}
