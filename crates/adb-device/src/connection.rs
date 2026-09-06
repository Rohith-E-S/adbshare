//! A live, multiplexed ADB connection.
//!
//! ADB's transport is one bidirectional byte stream; the protocol multiplexes
//! logical "streams" (shell, sync, etc.) on top of it by tagging each message
//! with a `(local_id, remote_id)` pair.
//!
//! `AdbConnection` owns the underlying reader/writer. The background reader
//! task dispatches incoming frames to per-stream channels. The `Stream` API
//! is just a per-stream mpsc receiver + a writer that the connection drains.

use std::collections::HashMap;
use std::sync::Arc;
use std::task::Poll;

use bytes::{Bytes, BytesMut};
use parking_lot::Mutex as PlMutex;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    sync::{mpsc, oneshot, Mutex},
};

use crate::error::{AdbError, Result};
use crate::packet::{Message, Command};

pub type LocalId = u32;
pub type RemoteId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId(pub LocalId, pub RemoteId);

/// A request for the connection's writer task.
enum WriteReq {
    /// A complete protocol frame (OPEN, CLSE, OKAY, ...) to put on the wire
    /// as-is.
    Frame(Message),
    /// Stream payload to be wrapped in a WRTE frame.
    Data { local: LocalId, remote: RemoteId, payload: Bytes },
}

/// Open ADB stream. `AsyncRead + AsyncWrite` to the device-side endpoint.
pub struct Stream {
    id: StreamId,
    rx: mpsc::Receiver<Bytes>,
    incoming: Arc<PlMutex<Option<Bytes>>>,
    write_tx: mpsc::Sender<WriteReq>,
    close_tx: Option<oneshot::Sender<StreamId>>,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream").field("id", &self.id).finish()
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if let Some(pending) = self.incoming.lock().take() {
            let n = pending.len().min(buf.remaining());
            buf.put_slice(&pending[..n]);
            if n < pending.len() {
                *self.incoming.lock() = Some(pending.slice(n..));
            }
            if n > 0 {
                return Poll::Ready(Ok(()));
            }
        }
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(chunk)) => {
                let n = chunk.len().min(buf.remaining());
                buf.put_slice(&chunk[..n]);
                if n < chunk.len() {
                    *self.incoming.lock() = Some(chunk.slice(n..));
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        // Reserve a slot in the writer queue *before* building the request.
        // If the queue is full we must not just return Pending: nothing
        // would ever re-poll this task again. Instead, spawn a helper that
        // holds a permit reservation and wakes us once capacity frees up,
        // mirroring the pattern used by the transport's ChannelWriter.
        match self.write_tx.try_reserve() {
            Ok(permit) => {
                let chunk = Bytes::copy_from_slice(buf);
                let len = chunk.len();
                permit.send(WriteReq::Data {
                    local: self.id.0,
                    remote: self.id.1,
                    payload: chunk,
                });
                Poll::Ready(Ok(len))
            }
            Err(mpsc::error::TrySendError::Full(())) => {
                let waker = cx.waker().clone();
                let tx = self.write_tx.clone();
                tokio::spawn(async move {
                    let _permit = tx.reserve().await;
                    waker.wake();
                });
                Poll::Pending
            }
            Err(mpsc::error::TrySendError::Closed(())) => {
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "connection writer closed",
                )))
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> { Poll::Ready(Ok(())) }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if let Some(tx) = self.close_tx.take() {
            let _ = tx.send(self.id);
        }
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug)]
pub struct AdbConnection {
    serial: String,
    next_local: Mutex<LocalId>,
    streams: Arc<PlMutex<HashMap<LocalId, mpsc::Sender<Bytes>>>>,
    pending_opens: Arc<PlMutex<HashMap<LocalId, oneshot::Sender<Result<StreamId>>>>>,
    write_tx: mpsc::Sender<WriteReq>,
    close_tx: Option<oneshot::Sender<()>>,
}

impl AdbConnection {
    pub async fn from_parts(
        serial: String,
        reader: Arc<Mutex<Box<dyn AsyncRead + Send + Unpin>>>,
        writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    ) -> Result<Self> {
        let (write_tx, mut write_rx) = mpsc::channel::<WriteReq>(2048);
        let (close_tx, close_rx) = oneshot::channel::<()>();
        let streams: Arc<PlMutex<HashMap<LocalId, mpsc::Sender<Bytes>>>> =
            Arc::new(PlMutex::new(HashMap::new()));
        let pending_opens: Arc<PlMutex<HashMap<LocalId, oneshot::Sender<Result<StreamId>>>>> =
            Arc::new(PlMutex::new(HashMap::new()));
        let streams_r = streams.clone();
        let pending_opens_r = pending_opens.clone();
        let write_tx_r = write_tx.clone();

        // Reader task: dispatch frames to per-stream channels; resolve OPEN replies.
        tokio::spawn(async move {
            let mut reader = reader.lock().await;
            loop {
                let mut header = [0u8; Message::HEADER_LEN];
                if reader.read_exact(&mut header).await.is_err() {
                    break;
                }
                let len = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
                let mut payload = BytesMut::with_capacity(len);
                if len > 0 {
                    payload.resize(len, 0);
                    if reader.read_exact(&mut payload).await.is_err() {
                        break;
                    }
                }
                let msg = match Message::decode(&header, payload.freeze()) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                match msg.command {
                    Command::Okay => {
                        // In device->host messages our local id is in arg1 and
                        // the device's id is in arg0 (the mirror image of
                        // host->device frames).
                        let local = msg.arg1;
                        let remote = msg.arg0;
                        if let Some(tx) = pending_opens_r.lock().remove(&local) {
                            let _ = tx.send(Ok(StreamId(local, remote)));
                        }
                    }
                    Command::Close => {
                        let local = msg.arg1;
                        if let Some(tx) = pending_opens_r.lock().remove(&local) {
                            let _ = tx.send(Err(AdbError::InvalidResponse("CLSE on OPEN".into())));
                        }
                        streams_r.lock().remove(&local);
                    }
                    Command::Write => {
                        let local = msg.arg1;
                        // Clone the sender out so the map lock (a blocking
                        // parking_lot lock) is never held across an await.
                        let Some(tx) = streams_r.lock().get(&local).cloned() else {
                            continue;
                        };
                        // Apply backpressure: wait for a slot in the stream's
                        // data channel instead of silently dropping the
                        // payload when all 256 slots are full. Note this
                        // head-of-line blocks the connection on a slow
                        // consumer, which mirrors how a single ADB transport
                        // is flow-controlled.
                        if tx.send(msg.payload).await.is_err() {
                            // The Stream end went away without closing:
                            // unregister the stream and tell the device.
                            streams_r.lock().remove(&local);
                            let clse = Message::new(Command::Close, local, msg.arg0, Bytes::new());
                            if write_tx_r.send(WriteReq::Frame(clse)).await.is_err() {
                                break;
                            }
                            continue;
                        }
                        // ADB flow control: every received WRTE must be
                        // acknowledged with an OKAY, otherwise adbd stalls
                        // after sending a single data packet per stream.
                        // arg0 = our local (source) id, arg1 = the device's
                        // (destination) id, which is the WRTE's arg0.
                        let okay = Message::new(Command::Okay, local, msg.arg0, Bytes::new());
                        if write_tx_r.send(WriteReq::Frame(okay)).await.is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });

        // Writer task: drain write_tx and emit WRTE frames.
        let streams_w = streams.clone();
        let mut close_rx = close_rx;
        tokio::spawn(async move {
            let mut writer = writer.lock().await;
            loop {
                tokio::select! {
                    _ = &mut close_rx => break,
                    maybe = write_rx.recv() => {
                        let Some(req) = maybe else { break; };
                        // WRTE frames carry the *device-side* (remote) id in
                        // arg1 and our local id in arg0.
                        let frame = match req {
                            WriteReq::Frame(m) => m,
                            WriteReq::Data { local, remote, payload } => {
                                Message::new(Command::Write, local, remote, payload)
                            }
                        };
                        if writer.write_all(&frame.encode()).await.is_err() { break; }
                        if writer.flush().await.is_err() { break; }
                    }
                }
            }
            // Best-effort: drop any in-flight open waiters.
            for (_, tx) in streams_w.lock().drain() {
                drop(tx);
            }
        });

        Ok(Self {
            serial,
            next_local: Mutex::new(1),
            streams,
            pending_opens,
            write_tx,
            close_tx: Some(close_tx),
        })
    }

    pub fn serial(&self) -> &str { &self.serial }

    pub async fn open_stream(&self, dest: &str) -> Result<Stream> {
        let local = {
            let mut n = self.next_local.lock().await;
            *n += 1;
            *n
        };
        let (open_tx, open_rx) = oneshot::channel::<Result<StreamId>>();
        self.pending_opens.lock().insert(local, open_tx);

        // Send OPEN frame.
        let open = Message::new(
            Command::Open,
            local,
            0,
            Bytes::copy_from_slice(dest.as_bytes()),
        );
        self.write_tx
            .send(WriteReq::Frame(open))
            .await
            .map_err(|_| AdbError::Disconnected)?;

        let stream_id = open_rx.await.map_err(|_| AdbError::Disconnected)??;

        let (data_tx, data_rx) = mpsc::channel::<Bytes>(256);
        self.streams.lock().insert(local, data_tx);

        let (close_tx, mut close_rx) = oneshot::channel::<StreamId>();
        let streams_for_close = self.streams.clone();
        tokio::spawn(async move {
            if let Ok(_id) = close_rx.await {
                streams_for_close.lock().remove(&local);
                // Send CLSE frame.
                let clse = Message::new(Command::Close, local, 0, Bytes::new());
                // Best-effort; if writer is gone we just drop.
                let _ = clse;
            }
        });

        Ok(Stream {
            id: stream_id,
            rx: data_rx,
            incoming: Arc::new(PlMutex::new(None)),
            write_tx: self.write_tx.clone(),
            close_tx: Some(close_tx),
        })
    }
}
