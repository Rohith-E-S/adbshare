use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// The password did not match: AEAD open failed, or the peer closed
    /// while waiting for `PeerInfo`. This is the correct answer for a wrong
    /// pairing code — SPAKE2 always yields some key material, and a mismatch
    /// only shows up here.
    CodeMismatch,
    Io(String),
    Tls(String),
    Protocol(String),
    Key(&'static str),
    Timeout,
    /// adb key file problem.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::CodeMismatch => write!(f, "the pairing code did not match"),
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Tls(e) => write!(f, "tls: {e}"),
            Error::Protocol(e) => write!(f, "protocol: {e}"),
            Error::Key(e) => write!(f, "key: {e}"),
            Error::Timeout => write!(f, "timed out"),
            Error::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}
