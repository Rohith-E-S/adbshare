//! The pairing client: TLS 1.3 with our ADB key as client certificate, a
//! TLS exporter as SPAKE2 nonce material, then `PeerInfo` over AES-GCM.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring::default_provider;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection};

use crate::cipher::PairingCipher;
use crate::error::Error;
use crate::packet::{self, PEER_INFO_SIZE};
use crate::spake2::Spake2;

/// Accept any server certificate. Authentication comes from SPAKE2 and the
/// subsequent `PeerInfo` exchange, not from the TLS certificate — the same
/// posture AOSP takes with `SetCertVerifyCallback([](auto*) { return 1; })`.
#[derive(Debug)]
struct AnyCertVerifier;

impl ServerCertVerifier for AnyCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer,
        _intermediates: &[CertificateDer],
        _server_name: &ServerName,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Build a TLS 1.3 client config with our ADB key as the client certificate.
///
/// The pairing certificate is derived from exactly this key — and so is the
/// later `adb connect` — which is what makes the phone accept the connect
/// without a fresh authorization prompt.
fn client_config(pkcs8_der: &[u8]) -> Result<ClientConfig, Error> {
    let key_pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(
        &rustls::pki_types::PrivatePkcs8KeyDer::from(pkcs8_der.to_vec()),
        &rcgen::PKCS_RSA_SHA256,
    )
    .map_err(|_| Error::Key("load signing key"))?;
    let cert_params = rcgen::CertificateParams::default();
    let signed = cert_params
        .self_signed(&key_pair)
        .map_err(|_| Error::Key("self-sign"))?;
    let cert_der = signed.der().clone().into_owned();
    // rustls/ring wants the PKCS#1 form; the key is stored PKCS#8.
    let pkcs1 = {
        use rsa::pkcs8::DecodePrivateKey;
        let raw =
            rsa::RsaPrivateKey::from_pkcs8_der(pkcs8_der).map_err(|_| Error::Key("parse pkcs8"))?;
        use pkcs1::EncodeRsaPrivateKey;
        raw.to_pkcs1_der()
            .map_err(|_| Error::Key("encode pkcs1"))?
            .to_bytes()
            .to_vec()
    };
    let config = ClientConfig::builder_with_provider(default_provider().into())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| Error::Tls("TLS 1.3 unsupported".into()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertVerifier))
        .with_client_auth_cert(
            vec![cert_der],
            rustls::pki_types::PrivateKeyDer::Pkcs1(rustls::pki_types::PrivatePkcs1KeyDer::from(
                pkcs1,
            )),
        )
        .map_err(|e| Error::Tls(e.to_string()))?;
    Ok(config)
}

fn read_exact(
    stream: &mut ClientConnection,
    tcp: &mut TcpStream,
    buf: &mut [u8],
    deadline: std::time::Instant,
) -> Result<(), Error> {
    let mut filled = 0;
    while filled < buf.len() {
        if std::time::Instant::now() > deadline {
            return Err(Error::Timeout);
        }
        // Borrow trick: read through a Stream wrapper that owns both the
        // connection and the socket, chunk by chunk, so a stalled peer hits
        // the socket timeout and then the deadline.
        let n = {
            let mut io = rustls::Stream::new(stream, tcp);
            io.read(&mut buf[filled..]).map_err(|e| {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    Error::Io("peer closed during pairing".into())
                } else {
                    Error::Io(e.to_string())
                }
            })?
        };
        if n == 0 {
            return Err(Error::Io("peer closed during pairing".into()));
        }
        filled += n;
    }
    Ok(())
}

fn write_all(
    tcp: &mut TcpStream,
    stream: &mut ClientConnection,
    buf: &[u8],
    deadline: std::time::Instant,
) -> Result<(), Error> {
    let mut written = 0;
    while written < buf.len() {
        if std::time::Instant::now() > deadline {
            return Err(Error::Timeout);
        }
        let mut io = rustls::Stream::new(stream, tcp);
        let n = io
            .write(&buf[written..])
            .map_err(|e| Error::Io(e.to_string()))?;
        if n == 0 {
            return Err(Error::Io("peer closed during pairing".into()));
        }
        written += n;
    }
    Ok(())
}

/// The device's peer info after a successful pairing.
pub struct PairingOutcome {
    /// The raw `PeerInfo` from the device (type 1 = device GUID).
    pub peer_info: Vec<u8>,
}

/// Pair with the device's pairing server. `password` is the shared secret
/// (QR password or six-digit code bytes), `pkcs8_der` our ADB private key in
/// PKCS#8 DER, and `pubkey` the Android public-key string for `PeerInfo`.
pub fn pair(
    addr: SocketAddr,
    password: &[u8],
    pkcs8_der: &[u8],
    pubkey: &str,
    timeout: Duration,
) -> Result<PairingOutcome, Error> {
    let deadline = std::time::Instant::now() + timeout;
    let mut tcp =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| Error::Io(e.to_string()))?;
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;
    let config = client_config(pkcs8_der)?;
    let server_name =
        ServerName::try_from("localhost").map_err(|_| Error::Tls("bad server name".into()))?;
    let mut stream = ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| Error::Tls(e.to_string()))?;
    // Drive the handshake, then the exporter.
    let mut io_tcp = tcp.try_clone().map_err(|e| Error::Io(e.to_string()))?;
    stream
        .complete_io(&mut io_tcp)
        .map_err(|e| Error::Tls(e.to_string()))?;
    let mut exporter = [0u8; 64];
    stream
        .export_keying_material(&mut exporter, b"adb-label\0", None)
        .map_err(|_| Error::Tls("exporter".into()))?;
    // SPAKE2 password: shared secret bytes, then the exporter.
    let mut pswd = password.to_vec();
    pswd.extend_from_slice(&exporter);
    let mut spake = Spake2::new(true, &pswd);
    let msg = spake.generate_msg();
    let header = packet::encode_header(packet::TYPE_SPAKE2_MSG, msg.len())?;
    write_all(&mut tcp, &mut stream, &header, deadline)?;
    write_all(&mut tcp, &mut stream, &msg, deadline)?;
    let mut header_buf = [0u8; packet::HEADER_LEN];
    read_exact(&mut stream, &mut tcp, &mut header_buf, deadline)?;
    let (kind, len) = packet::decode_header(&header_buf)?;
    if kind != packet::TYPE_SPAKE2_MSG {
        return Err(Error::Protocol("expected a SPAKE2 message".into()));
    }
    let mut their_msg = vec![0u8; len];
    read_exact(&mut stream, &mut tcp, &mut their_msg, deadline)?;
    if their_msg.len() != 32 {
        return Err(Error::Protocol("SPAKE2 message has wrong size".into()));
    }
    let mut their_msg32 = [0u8; 32];
    their_msg32.copy_from_slice(&their_msg);
    let shared = spake.process_msg(&their_msg32)?;
    let mut cipher = PairingCipher::new(&shared);
    // Our PeerInfo (RSA public key) sealed.
    let ours = packet::peer_info(pubkey)?;
    let sealed = cipher.encrypt(&ours);
    let header = packet::encode_header(packet::TYPE_PEER_INFO, sealed.len())?;
    write_all(&mut tcp, &mut stream, &header, deadline)?;
    write_all(&mut tcp, &mut stream, &sealed, deadline)?;
    // Their PeerInfo sealed: a wrong secret only fails here, at AEAD open.
    let mut header_buf = [0u8; packet::HEADER_LEN];
    read_exact(&mut stream, &mut tcp, &mut header_buf, deadline)?;
    let (kind, len) = packet::decode_header(&header_buf)?;
    if kind != packet::TYPE_PEER_INFO {
        return Err(Error::Protocol("expected PeerInfo".into()));
    }
    let mut sealed = vec![0u8; len];
    read_exact(&mut stream, &mut tcp, &mut sealed, deadline)?;
    let plain = cipher.decrypt(&sealed)?;
    if plain.len() != PEER_INFO_SIZE {
        return Err(Error::Protocol("PeerInfo has wrong size".into()));
    }
    Ok(PairingOutcome { peer_info: plain })
}
