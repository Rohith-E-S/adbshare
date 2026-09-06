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
    /// RSA key used for the AUTH handshake. `None` means AUTH TOKEN replies
    /// surface as [`AdbError::Unauthorized`].
    key: Option<Arc<crate::auth::AdbKey>>,
}

/// Maximum ADB payload we are willing to buffer when reading a message body.
const MAX_PAYLOAD: usize = 1024 * 1024;

impl std::fmt::Debug for StreamTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamTransport")
            .field("kind", &self.kind)
            .field("serial", &self.serial)
            .field("has_key", &self.key.is_some())
            .finish()
    }
}

impl StreamTransport {
    pub fn new(kind: TransportKind, serial: String, reader: BoxedReader, writer: BoxedWriter) -> Self {
        Self::with_key(kind, serial, reader, writer, None)
    }

    /// Like [`StreamTransport::new`], but with an RSA key for the AUTH
    /// handshake. When the device sends an AUTH TOKEN, the token is signed and
    /// an AUTH SIGNATURE is sent; if the device still doesn't accept us, an
    /// AUTH RSAPUBLICKEY (Android public-key text format) is sent so the user
    /// can authorize the host.
    pub fn with_key(
        kind: TransportKind,
        serial: String,
        reader: BoxedReader,
        writer: BoxedWriter,
        key: Option<Arc<crate::auth::AdbKey>>,
    ) -> Self {
        Self {
            kind,
            serial,
            reader: Arc::new(Mutex::new(reader)),
            writer: Arc::new(Mutex::new(writer)),
            key,
        }
    }

    /// Read one full ADB message (header + payload) from `reader`.
    async fn read_message(reader: &mut BoxedReader) -> Result<Option<Message>> {
        let mut header = [0u8; Message::HEADER_LEN];
        match reader.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let data_len = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;
        if data_len > MAX_PAYLOAD {
            return Err(AdbError::InvalidResponse(format!(
                "ADB payload too large: {data_len}"
            )));
        }
        let mut payload = vec![0u8; data_len];
        reader.read_exact(&mut payload).await?;
        Ok(Some(Message::decode(&header, Bytes::from(payload))?))
    }

    async fn write_message(
        writer: &mut BoxedWriter,
        msg: &Message,
    ) -> Result<()> {
        writer.write_all(&msg.encode()).await?;
        writer.flush().await?;
        Ok(())
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
        Self::write_message(&mut writer, &connect).await?;
        drop(writer);

        let mut reader = self.reader.lock().await;
        let mut msg = match Self::read_message(&mut reader).await? {
            Some(m) => m,
            None => return Err(AdbError::Disconnected),
        };

        // AUTH handshake: the device sends AUTH TOKEN; we answer with
        // AUTH SIGNATURE, and if the device still doesn't know us, with
        // AUTH RSAPUBLICKEY so the user can authorize this host. The
        // handshake ends when the device sends CNXN.
        while msg.command == crate::packet::Command::Auth {
            let key = self.key.as_ref().ok_or(AdbError::Unauthorized)?;
            if msg.arg0 != crate::packet::AUTH_TOKEN {
                return Err(AdbError::InvalidResponse(format!(
                    "unexpected AUTH type {}",
                    msg.arg0
                )));
            }
            let token = msg.payload.clone();
            let sig = key.sign(&token)?;
            let sig_msg = Message::new(
                crate::packet::Command::Auth,
                crate::packet::AUTH_SIGNATURE,
                0,
                Bytes::from(sig),
            );
            {
                let mut writer = self.writer.lock().await;
                Self::write_message(&mut writer, &sig_msg).await?;
            }

            msg = match Self::read_message(&mut reader).await? {
                Some(m) => m,
                None => return Err(AdbError::Disconnected),
            };

            if msg.command == crate::packet::Command::Auth && msg.arg0 == crate::packet::AUTH_TOKEN {
                // Signature rejected: send our public key (Android public-key
                // text format, NUL-terminated) and wait for CNXN.
                let pk_msg = Message::new(
                    crate::packet::Command::Auth,
                    crate::packet::AUTH_RSAPUBLICKEY,
                    0,
                    Bytes::from(key.ssh_public().to_vec()),
                );
                let mut writer = self.writer.lock().await;
                Self::write_message(&mut writer, &pk_msg).await?;
                msg = match Self::read_message(&mut reader).await? {
                    Some(m) => m,
                    None => return Err(AdbError::Disconnected),
                };
            }
        }

        if msg.command != crate::packet::Command::Connect {
            return Err(AdbError::InvalidResponse(format!(
                "expected CNXN, got {:?}",
                msg.command
            )));
        }

        drop(reader);
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
    // Detach any kernel driver BEFORE claiming: claim_interface fails with
    // Busy if a kernel driver (e.g. usbfs-bound adbd helper or a modem
    // driver) still holds the interface.
    if handle.set_auto_detach_kernel_driver(true).is_err() {
        // libusb may not support auto-detach on this platform; fall back to
        // a manual detach.
        if handle.kernel_driver_active(iface).unwrap_or(false) {
            if let Err(e) = handle.detach_kernel_driver(iface) {
                tracing::warn!(error = ?e, interface = iface, "failed to detach kernel driver");
            }
        }
    }
    handle.claim_interface(iface)?;

    // Channels: pump -> app (bytes from device), app -> pump (bytes to device).
    let (tx_to_app, rx_to_app) = mpsc::channel::<bytes::Bytes>(256);
    let (tx_from_app, mut rx_from_app) = mpsc::channel::<bytes::Bytes>(256);

    let mut handle_for_thread = handle;
    let context_for_thread = context;

    std::thread::Builder::new()
        .name("adb-usb-pump".into())
        .spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            let read_timeout = std::time::Duration::from_millis(2);
            let write_timeout = std::time::Duration::from_millis(5_000);
            'pump: loop {
                // Drain ALL pending outgoing writes before blocking on a read.
                // The previous single-chunk-per-iteration design capped write
                // throughput at ~20 chunks x 4KB per 50ms loop (~1.25 MB/s) and
                // a blocking write_bulk delayed incoming reads as well.
                loop {
                    match rx_from_app.try_recv() {
                        Ok(chunk) => {
                            if let Err(e) =
                                handle_for_thread.write_bulk(out_ep, &chunk, write_timeout)
                            {
                                tracing::error!(error = ?e, "adb-usb-pump: write_bulk failed");
                                break 'pump;
                            }
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => break 'pump,
                    }
                }

                // Read from device -> channel.
                match handle_for_thread.read_bulk(in_ep, &mut buf, read_timeout) {
                    Ok(0) => {
                        // Zero-length transfer: log it and back off briefly so we
                        // don't spin the pump thread in a tight loop.
                        tracing::warn!("adb-usb-pump: zero-length bulk read");
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }
                    Ok(n) => {
                        let chunk = bytes::Bytes::copy_from_slice(&buf[..n]);
                        if tx_to_app.blocking_send(chunk).is_err() {
                            break;
                        }
                    }
                    Err(rusb::Error::Timeout) => {}
                    Err(e) => {
                        tracing::error!(error = ?e, "adb-usb-pump: read_bulk failed");
                        break;
                    }
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
        // Serve leftover bytes from a previously oversized chunk first.
        if buf.remaining() > 0 {
            if let Some(mut pending) = this.pending.take() {
                let take = pending.len().min(buf.remaining());
                buf.put_slice(&pending[..take]);
                if take < pending.len() {
                    this.pending = Some(pending.split_off(take));
                }
                return Poll::Ready(Ok(()));
            }
            match this.rx.poll_recv(cx) {
                Poll::Ready(Some(mut chunk)) => {
                    let take = chunk.len().min(buf.remaining());
                    buf.put_slice(&chunk[..take]);
                    if take < chunk.len() {
                        // Keep the remainder for the next poll_read; every 24-byte
                        // ADB header read leaves ~4KB of a 64KB USB transfer behind.
                        this.pending = Some(chunk.split_off(take));
                    }
                    return Poll::Ready(Ok(()));
                }
                // Pump thread gone: report EOF.
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                // Waker registered with the channel; no manual re-poll needed.
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(()))
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
