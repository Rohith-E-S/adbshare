//! Roundtrip tests for the proxy protocol's wire types.

#[cfg(test)]
mod tests {
    use super::super::ops::*;

    #[tokio::test]
    async fn copy_file_uses_one_connection_and_existing_response_framing() {
        use crate::{ProxyClient, ProxyError};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for status in [0, 0x05, 0x0B, 0] {
                let mut header = [0; 5];
                stream.read_exact(&mut header).await.unwrap();
                assert_eq!(header[0], 0x12);
                let len = u32::from_le_bytes(header[1..].try_into().unwrap()) as usize;
                let mut args = vec![0; len];
                stream.read_exact(&mut args).await.unwrap();
                let mut expected = Vec::new();
                for path in ["/source", "/destination"] {
                    expected.extend_from_slice(&(path.len() as u32).to_le_bytes());
                    expected.extend_from_slice(path.as_bytes());
                }
                assert_eq!(args, expected);
                let body = if status == 0 { b"".as_slice() } else { b"copy error" };
                stream.write_all(&((1 + body.len()) as u32).to_le_bytes()).await.unwrap();
                stream.write_all(&[status]).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });
        let client = ProxyClient::connect(addr, 1).await.unwrap();
        let copy = || tokio::time::timeout(std::time::Duration::from_secs(3), client.copy_file("/source", "/destination"));
        copy().await.unwrap().unwrap();
        for expected in [Status::Exists, Status::NotSupported] {
            assert!(matches!(copy().await.unwrap(), Err(ProxyError::Status(status, message))
                if status == expected && message == "copy error"));
        }
        copy().await.unwrap().unwrap();
        server.await.unwrap();
    }

    async fn copy_file_ambiguous_failure(disconnect: bool) {
        use crate::{ProxyClient, ProxyError};
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = ProxyClient::connect(listener.local_addr().unwrap().to_string(), 1).await.unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        let copy = client.copy_file("/source", "/destination");
        let receive = async {
            let mut header = [0; 5];
            stream.read_exact(&mut header).await.unwrap();
            assert_eq!(header[0], Op::CopyFile as u8);
            let mut args = vec![0; u32::from_le_bytes(header[1..].try_into().unwrap()) as usize];
            stream.read_exact(&mut args).await.unwrap();
            let mut expected = Vec::new();
            for path in ["/source", "/destination"] {
                expected.extend_from_slice(&(path.len() as u32).to_le_bytes());
                expected.extend_from_slice(path.as_bytes());
            }
            assert_eq!(args, expected);
            if disconnect {
                stream.shutdown().await.unwrap();
            }
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(35), async {
            tokio::join!(copy, receive)
        }).await.unwrap();
        let error = result.unwrap_err();
        assert!(matches!(error, ProxyError::Other(_)));
        assert_eq!(error.to_string(), format!(
            "{}; completion unknown; destination may be incomplete or still copying",
            if disconnect { "connection closed" } else { "request timed out" }
        ));
        let mut extra = [0; 1];
        assert_eq!(tokio::time::timeout(Duration::from_secs(3), stream.read(&mut extra)).await.unwrap().unwrap(), 0);
        assert!(tokio::time::timeout(Duration::from_millis(100), listener.accept()).await.is_err());

        let server = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = [0; 5];
            stream.read_exact(&mut header).await.unwrap();
            assert_eq!(header[0], Op::CopyFile as u8);
            let mut args = vec![0; u32::from_le_bytes(header[1..].try_into().unwrap()) as usize];
            stream.read_exact(&mut args).await.unwrap();
            assert!(args.ends_with(b"/second-destination"));
            stream.write_all(&[1, 0, 0, 0, 0]).await.unwrap();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(client.copy_file("/source", "/second-destination"), server)
        }).await.unwrap();
        result.unwrap();
    }

    #[tokio::test]
    async fn copy_file_disconnect_reports_unknown_without_retry_or_delete() {
        copy_file_ambiguous_failure(true).await;
    }

    #[tokio::test]
    async fn copy_file_timeout_reports_unknown_without_retry_or_delete() {
        copy_file_ambiguous_failure(false).await;
    }

    #[tokio::test]
    async fn copy_file_validates_paths_before_acquiring_connection() {
        use crate::{ProxyClient, ProxyError};
        let client = ProxyClient::connect("127.0.0.1:0", 0).await.unwrap();
        for path in ["", "relative", "/nul\0hidden", &"/".repeat(4097)] {
            for (src, dst) in [(path, "/destination"), ("/source", path)] {
                let result = tokio::time::timeout(std::time::Duration::from_secs(1), client.copy_file(src, dst)).await.unwrap();
                assert!(matches!(result, Err(ProxyError::Status(Status::InvalidArg, _))));
            }
        }
    }

    #[tokio::test]
    async fn mkdir_encodes_path_before_mode() {
        use crate::ProxyClient;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let cases = [("/new-directory", 0o755u32), ("/資料/café", 0o700)];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for (path, mode) in cases {
                let mut expected = Vec::new();
                expected.extend_from_slice(&(path.len() as u32).to_le_bytes());
                expected.extend_from_slice(path.as_bytes());
                expected.extend_from_slice(&mode.to_le_bytes());
                let mut header = [0; 5];
                stream.read_exact(&mut header).await.unwrap();
                assert_eq!(header[0], 0x07);
                assert_eq!(u32::from_le_bytes(header[1..].try_into().unwrap()) as usize, expected.len());
                let mut args = vec![0; expected.len()];
                stream.read_exact(&mut args).await.unwrap();
                assert_eq!(args, expected);
                stream.write_all(&[1, 0, 0, 0, 0]).await.unwrap();
            }
        });
        let client = ProxyClient::connect(addr, 1).await.unwrap();
        for (path, mode) in cases {
            tokio::time::timeout(std::time::Duration::from_secs(3), client.mkdir(path, mode)).await.unwrap().unwrap();
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn truncate_encodes_path_before_size() {
        use crate::ProxyClient;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let cases = [("/file", 0u64), ("/資料/café.txt", 12345), ("/large-file", 4_294_967_297)];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for (path, size) in cases {
                let mut expected = Vec::new();
                expected.extend_from_slice(&(path.len() as u32).to_le_bytes());
                expected.extend_from_slice(path.as_bytes());
                expected.extend_from_slice(&size.to_le_bytes());
                let mut header = [0; 5];
                stream.read_exact(&mut header).await.unwrap();
                assert_eq!(header[0], 0x0B);
                assert_eq!(u32::from_le_bytes(header[1..].try_into().unwrap()) as usize, expected.len());
                let mut args = vec![0; expected.len()];
                stream.read_exact(&mut args).await.unwrap();
                assert_eq!(args, expected);
                stream.write_all(&[1, 0, 0, 0, 0]).await.unwrap();
            }
        });
        let client = ProxyClient::connect(addr, 1).await.unwrap();
        for (path, size) in cases {
            tokio::time::timeout(std::time::Duration::from_secs(3), client.truncate(path, size)).await.unwrap().unwrap();
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn path_operations_return_exact_length_prefixed_strings() {
        use crate::ProxyClient;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let cases = [
            (Op::ReadLink, "/link", "../target/file.txt"),
            (Op::ReadLink, "/資料/link", "../写真/café.txt"),
            (Op::RealPath, "/directory/../file", "/file"),
            (Op::RealPath, "/資料/../写真/café.txt", "/写真/café.txt"),
        ];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for (op, path, result) in cases {
                let mut expected = Vec::new();
                expected.extend_from_slice(&(path.len() as u32).to_le_bytes());
                expected.extend_from_slice(path.as_bytes());
                let mut header = [0; 5];
                stream.read_exact(&mut header).await.unwrap();
                assert_eq!(header[0], op as u8);
                assert_eq!(u32::from_le_bytes(header[1..].try_into().unwrap()) as usize, expected.len());
                let mut args = vec![0; expected.len()];
                stream.read_exact(&mut args).await.unwrap();
                assert_eq!(args, expected);
                stream.write_all(&((5 + result.len()) as u32).to_le_bytes()).await.unwrap();
                stream.write_all(&[0]).await.unwrap();
                stream.write_all(&(result.len() as u32).to_le_bytes()).await.unwrap();
                stream.write_all(result.as_bytes()).await.unwrap();
            }
        });
        let client = ProxyClient::connect(addr, 1).await.unwrap();
        for (op, path, expected) in cases {
            let result = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                match op {
                    Op::ReadLink => client.read_link(path).await,
                    Op::RealPath => client.real_path(path).await,
                    _ => unreachable!(),
                }
            }).await.unwrap().unwrap();
            assert_eq!(result, expected);
        }
        server.await.unwrap();
    }

    #[test]
    fn stat_roundtrip() {
        let stat = Stat {
            mode: FileMode(0o100644),
            size: 12345,
            mtime: 1700000000,
            atime: 1700000000,
            ctime: 1700000000,
            uid: 2000,
            gid: 2000,
            nlink: 1,
            blksize: 4096,
            blocks: 32,
        };
        let encoded = stat.encode();
        let decoded = Stat::decode(&encoded).unwrap();
        assert_eq!(decoded.size, 12345);
        assert_eq!(decoded.mode.0, 0o100644);
        assert_eq!(decoded.uid, 2000);
    }

    #[test]
    fn status_roundtrip() {
        use super::super::client::ProxyError;
        let e: ProxyError = ProxyError::Status(Status::NotFound, "x".into());
        let s = e.to_string();
        assert!(s.contains("NotFound") || s.contains("not found"));
    }

    #[test]
    fn openflags_conversion() {
        let r = OpenFlags::from_octal(0o644);
        assert!(r.contains(OpenFlags::READ));
        let rw = OpenFlags::from_octal(0o666);
        assert!(rw.contains(OpenFlags::READ));
        assert!(rw.contains(OpenFlags::WRITE));
    }
}
