//! Transport abstraction: anything that can produce an `AdbConnection` to a device.
//!
//! - `UsbTransport`: libusb-based, talks directly to a USB-connected device's bulk endpoints.
//! - `TcpTransport`: standard adb-over-TCP (used by `adb connect` and emulators).
//!
//! All transports resolve to a `StreamTransport` after the initial `rusb::DeviceHandle` is
//! opened. The libusb handle is owned by a dedicated thread (the "USB pump thread")
//! which reads bulk-IN into a tokio mpsc channel and writes bulk-OUT from a tokio mpsc
//! channel. The `AsyncRead`/`AsyncWrite` sides are just mpsc adaptors. This sidesteps
//! the problem of bridging sync libusb into tokio cleanly.

use std::sync::Arc;
use std::task::Poll;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, oneshot, Mutex},
};

use crate::{
    connection::AdbConnection,
    error::{AdbError, Result},
    packet::{Message, ADB_VERSION},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind {
    Usb,
    Tcp,
}

#[async_trait]
pub trait Transport: Send + Sync + std::fmt::Debug {
    fn kind(&self) -> TransportKind;
    fn serial(&self) -> &str;
    async fn open(&self) -> Result<AdbConnection>;
}

type BoxedReader = Box<dyn tokio::io::AsyncRead + Send + Unpin>;
type BoxedWriter = Box<dyn tokio::io::AsyncWrite + Send + Unpin>;

/// Stream-based transport: an AsyncRead+AsyncWrite pair that already represents
/// the device's adb endpoint. Used by TcpTransport directly and as the input
/// to the USB pump-thread.
pub struct StreamTransport {
    kind: TransportKind,
    serial: String,
    reader: Arc<Mutex<BoxedReader>>,
    writer: Arc<Mutex<BoxedWriter>>,
}

impl std::fmt::Debug for StreamTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamTransport")
            .field("kind", &self.kind)
            .field("serial", &self.serial)
            .finish()
    }
}

impl StreamTransport {
    pub fn new(kind: TransportKind, serial: String, reader: BoxedReader, writer: BoxedWriter) -> Self {
        Self {
            kind,
            serial,
            reader: Arc::new(Mutex::new(reader)),
            writer: Arc::new(Mutex::new(writer)),
        }
    }
}

#[async_trait]
impl Transport for StreamTransport {
    fn kind(&self) -> TransportKind { self.kind }
    fn serial(&self) -> &str { &self.serial }

    async fn open(&self) -> Result<AdbConnection> {
        let mut writer = self.writer.lock().await;
        let banner = format!("adbshare::{}::{}", env!("CARGO_PKG_VERSION"), self.serial);
        let connect = Message::new(
            crate::packet::Command::Connect,
            ADB_VERSION,
            1 << 20,
            Bytes::from(banner),
        );
        writer.write_all(&connect.encode()).await?;
        writer.flush().await?;
        drop(writer);

        let mut reader = self.reader.lock().await;
        let mut header = [0u8; Message::HEADER_LEN];
        reader.read_exact(&mut header).await?;
        let reply = Message::decode(&header, Bytes::new())?;
        if reply.command == crate::packet::Command::Auth {
            return Err(AdbError::Unauthorized);
        }
        if reply.command != crate::packet::Command::Connect {
            return Err(AdbError::InvalidResponse(format!(
                "expected CNXN, got {:?}", reply.command
            )));
        }

        AdbConnection::from_parts(self.serial.clone(), self.reader.clone(), self.writer.clone()).await
    }
}

// ---------- TCP ----------

#[derive(Debug, Clone)]
pub struct TcpTransport {
    pub serial: String,
    pub addr: String,
}

#[async_trait]
impl Transport for TcpTransport {
    fn kind(&self) -> TransportKind { TransportKind::Tcp }
    fn serial(&self) -> &str { &self.serial }

    async fn open(&self) -> Result<AdbConnection> {
        let stream = TcpStream::connect(&self.addr).await?;
        let (r, w) = stream.into_split();
        let inner = StreamTransport::new(
            TransportKind::Tcp,
            self.serial.clone(),
            Box::new(r),
            Box::new(w),
        );
        inner.open().await
    }
}

// ---------- USB (libusb via pump thread) ----------

#[derive(Debug, Clone)]
pub struct UsbTransport {
    pub serial: String,
    pub bus_number: u8,
    pub device_address: u8,
    pub interface_number: u8,
    pub in_endpoint: u8,
    pub out_endpoint: u8,
}

#[async_trait]
impl Transport for UsbTransport {
    fn kind(&self) -> TransportKind { TransportKind::Usb }
    fn serial(&self) -> &str { &self.serial }

    async fn open(&self) -> Result<AdbConnection> {
        // Open the device in a blocking context and bridge it through a pump thread.
        let (tx, rx) = oneshot::channel::<Result<(BoxedReader, BoxedWriter)>>();
        let bus = self.bus_number;
        let addr = self.device_address;
        let iface = self.interface_number;
        let in_ep = self.in_endpoint;
        let out_ep = self.out_endpoint;
        std::thread::spawn(move || {
            let result = open_usb_pump(bus, addr, iface, in_ep, out_ep);
            let _ = tx.send(result);
        });
        let (reader, writer) = rx.await.map_err(|_| AdbError::Disconnected)??;
        let inner = StreamTransport::new(
            TransportKind::Usb,
            self.serial.clone(),
            reader,
            writer,
        );
        inner.open().await
    }
}

/// Open the USB device and return AsyncRead/AsyncWrite that delegate to a
/// dedicated "pump" thread. The pump thread owns the libusb handle and
/// shuttles bytes through two mpsc channels.
fn open_usb_pump(
    bus: u8,
    addr: u8,
    iface: u8,
    in_ep: u8,
    out_ep: u8,
) -> Result<(BoxedReader, BoxedWriter)> {
    use rusb::UsbContext;

    let context = rusb::Context::new()?;
    let device = context
        .devices()?
        .iter()
        .find(|d| d.bus_number() == bus && d.address() == addr)
        .ok_or_else(|| AdbError::DeviceNotFound(format!("usb:{}:{}", bus, addr)))?;
    let mut handle = device.open()?;
    handle.claim_interface(iface)?;
    if handle.kernel_driver_active(iface).unwrap_or(false) {
        let _ = handle.detach_kernel_driver(iface);
    }

    // Channels: pump -> app (bytes from device), app -> pump (bytes to device).
    let (tx_to_app, rx_to_app) = mpsc::channel::<bytes::Bytes>(256);
    let (tx_from_app, mut rx_from_app) = mpsc::channel::<bytes::Bytes>(256);

    let mut handle_for_thread = handle;
    let context_for_thread = context;

    std::thread::Builder::new()
        .name("adb-usb-pump".into())
        .spawn(move || {
            use std::io::Write;
            let mut buf = vec![0u8; 64 * 1024];
            let read_timeout = std::time::Duration::from_millis(50);
            let write_timeout = std::time::Duration::from_millis(5_000);
            loop {
                // Read from device -> channel.
                match handle_for_thread.read_bulk(in_ep, &mut buf, read_timeout) {
                    Ok(0) => continue,
                    Ok(n) => {
                        let chunk = bytes::Bytes::copy_from_slice(&buf[..n]);
                        if tx_to_app.blocking_send(chunk).is_err() {
                            break;
                        }
                    }
                    Err(rusb::Error::Timeout) => {}
                    Err(rusb::Error::Io) | Err(rusb::Error::Overflow) | Err(rusb::Error::Other) => break,
                    Err(_) => break,
                }

                // Read from channel -> device (non-blocking try_recv).
                match rx_from_app.try_recv() {
                    Ok(chunk) => {
                        if handle_for_thread.write_bulk(out_ep, &chunk, write_timeout).is_err() {
                            break;
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => {}
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                }
            }
            drop(context_for_thread);
        })?;

    // Wrap mpsc receivers/senders as AsyncRead/AsyncWrite.
    let reader = ChannelReader { rx: rx_to_app, pending: None };
    let writer = ChannelWriter { tx: tx_from_app };
    Ok((Box::new(reader), Box::new(writer)))
}

struct ChannelReader {
    rx: mpsc::Receiver<bytes::Bytes>,
    /// Leftover bytes from a USB chunk that exceeded the caller's read buffer.
    /// These must be served before pulling a new chunk, otherwise data is lost.
    pending: Option<bytes::Bytes>,
}

impl tokio::io::AsyncRead for ChannelReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let mut total = 0usize;
        loop {
            let unfilled = buf.remaining();
            if unfilled == 0 {
                return Poll::Ready(Ok(()));
            }
            // Serve leftover bytes from a previously oversized chunk first.
            if let Some(mut pending) = this.pending.take() {
                let take = pending.len().min(unfilled);
                buf.put_slice(&pending[..take]);
                total += take;
                if take < pending.len() {
                    this.pending = Some(pending.split_off(take));
                }
                if total >= unfilled {
                    return Poll::Ready(Ok(()));
                }
                continue;
            }
            match this.rx.try_recv() {
                Ok(mut chunk) => {
                    let take = chunk.len().min(unfilled);
                    buf.put_slice(&chunk[..take]);
                    total += take;
                    if take < chunk.len() {
                        // Keep the remainder for the next poll_read; every 24-byte
                        // ADB header read leaves ~4KB of a 64KB USB transfer behind.
                        let rest = chunk.split_off(take);
                        this.pending = Some(rest);
                        return Poll::Ready(Ok(()));
                    }
                    if total >= unfilled {
                        return Poll::Ready(Ok(()));
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    // Need to wait for more data.
                    let waker = cx.waker().clone();
                    let rx = &mut this.rx;
                    tokio::spawn(async move {
                        // Tiny delay then wake. (ChannelReceiver doesn't have a waker API
                        // without `tokio::sync::Notify`; this is a pragmatic shim.)
                        tokio::time::sleep(std::time::Duration::from_micros(500)).await;
                        waker.wake();
                    });
                    let _ = rx;
                    return Poll::Pending;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

struct ChannelWriter {
    tx: mpsc::Sender<bytes::Bytes>,
}

impl tokio::io::AsyncWrite for ChannelWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let chunk = bytes::Bytes::copy_from_slice(buf);
        match this.tx.try_send(chunk) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(mpsc::error::TrySendError::Full(_)) => {
                let waker = cx.waker().clone();
                let tx = this.tx.clone();
                tokio::spawn(async move {
                    tx.reserve().await;
                    waker.wake();
                });
                Poll::Pending
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "usb pump closed")))
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
