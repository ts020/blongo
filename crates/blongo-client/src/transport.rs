//! Frame transports shared by the client and `blongo serve`: WebSocket
//! (one binary message per frame) and byte streams with a big-endian `u32`
//! length prefix (SSH stdio, the server's Unix socket). Both bound the
//! frame size they accept.

use std::io;

use blongo_protocol::wire::{FrameReader, length_prefixed};
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// Receiving half: whole frames, `None` at a clean end of stream.
pub trait FrameRead: Send {
    fn read_frame(&mut self) -> BoxFuture<'_, io::Result<Option<Vec<u8>>>>;
}

/// Sending half.
pub trait FrameWrite: Send {
    fn write_frame(&mut self, frame: Vec<u8>) -> BoxFuture<'_, io::Result<()>>;
    fn close(&mut self) -> BoxFuture<'_, ()>;
}

pub type Reader = Box<dyn FrameRead>;
pub type Writer = Box<dyn FrameWrite>;

/// WebSocket limits: no message (or fragment) above `max_frame` bytes, and
/// a bounded write buffer.
pub fn ws_config(max_frame: usize) -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(max_frame),
        max_frame_size: Some(max_frame),
        write_buffer_size: 64 * 1024,
        // One outgoing frame at a time: the caller's own queue is the
        // bounded buffer, not tungstenite's.
        max_write_buffer_size: (16 << 20) + (64 << 10),
        ..Default::default()
    }
}

/// Split a WebSocket into frame halves.
pub fn ws_halves<S>(ws: WebSocketStream<S>) -> (Reader, Writer)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sink, stream) = ws.split();
    (Box::new(WsRead(stream)), Box::new(WsWrite(sink)))
}

struct WsRead<S>(futures_util::stream::SplitStream<WebSocketStream<S>>);
struct WsWrite<S>(futures_util::stream::SplitSink<WebSocketStream<S>, Message>);

fn ws_err(e: tokio_tungstenite::tungstenite::Error) -> io::Error {
    io::Error::other(e.to_string())
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> FrameRead for WsRead<S> {
    fn read_frame(&mut self) -> BoxFuture<'_, io::Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            loop {
                match self.0.next().await {
                    None => return Ok(None),
                    Some(Err(e)) => return Err(ws_err(e)),
                    Some(Ok(Message::Binary(bytes))) => return Ok(Some(bytes)),
                    Some(Ok(Message::Close(_))) => return Ok(None),
                    // Pings are answered by tungstenite; text is not ours.
                    Some(Ok(Message::Text(_))) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "text message on a binary protocol",
                        ));
                    }
                    Some(Ok(_)) => continue,
                }
            }
        })
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> FrameWrite for WsWrite<S> {
    fn write_frame(&mut self, frame: Vec<u8>) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move { self.0.send(Message::Binary(frame)).await.map_err(ws_err) })
    }

    fn close(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = self.0.close().await;
        })
    }
}

/// Length-prefixed frames over any byte stream halves.
pub fn stream_halves<R, W>(read: R, write: W, max_in: usize) -> (Reader, Writer)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    (
        Box::new(StreamRead {
            inner: read,
            frames: FrameReader::new(max_in),
            buf: vec![0; 16 * 1024],
        }),
        Box::new(StreamWrite(write)),
    )
}

struct StreamRead<R> {
    inner: R,
    frames: FrameReader,
    buf: Vec<u8>,
}

struct StreamWrite<W>(W);

impl<R: AsyncRead + Unpin + Send> FrameRead for StreamRead<R> {
    fn read_frame(&mut self) -> BoxFuture<'_, io::Result<Option<Vec<u8>>>> {
        Box::pin(async move {
            loop {
                match self.frames.next_frame() {
                    Ok(Some(frame)) => return Ok(Some(frame)),
                    Ok(None) => {}
                    Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
                }
                let n = self.inner.read(&mut self.buf).await?;
                if n == 0 {
                    return if self.frames.buffered() == 0 {
                        Ok(None)
                    } else {
                        Err(io::ErrorKind::UnexpectedEof.into())
                    };
                }
                self.frames.push(&self.buf[..n]);
            }
        })
    }
}

impl<W: AsyncWrite + Unpin + Send> FrameWrite for StreamWrite<W> {
    fn write_frame(&mut self, frame: Vec<u8>) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            self.0.write_all(&length_prefixed(&frame)).await?;
            self.0.flush().await
        })
    }

    fn close(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = self.0.shutdown().await;
        })
    }
}
