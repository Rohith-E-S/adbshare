//! Roundtrip tests for the proxy protocol's wire types.

#[cfg(test)]
mod tests {
    use super::super::ops::*;

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
