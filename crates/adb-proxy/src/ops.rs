//! Wire types and op codes for the proxy protocol.

use bitflags::bitflags;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Open = 0x01,
    Close = 0x02,
    Read = 0x03,
    Write = 0x04,
    Stat = 0x05,
    ListDir = 0x06,
    Mkdir = 0x07,
    Unlink = 0x08,
    Rmdir = 0x09,
    Rename = 0x0A,
    Truncate = 0x0B,
    RealPath = 0x0C,
    ReadLink = 0x0D,
    Symlink = 0x0E,
    Lstat = 0x0F,
    Utime = 0x10,
    DiskUsage = 0x11,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok = 0x00,
    NotFound = 0x01,
    PermissionDenied = 0x02,
    IsDir = 0x03,
    NotDir = 0x04,
    Exists = 0x05,
    NotEmpty = 0x06,
    IoError = 0x07,
    InvalidArg = 0x08,
    NoSpace = 0x09,
    NameTooLong = 0x0A,
    NotSupported = 0x0B,
}

impl Status {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0x00 => Status::Ok,
            0x01 => Status::NotFound,
            0x02 => Status::PermissionDenied,
            0x03 => Status::IsDir,
            0x04 => Status::NotDir,
            0x05 => Status::Exists,
            0x06 => Status::NotEmpty,
            0x07 => Status::IoError,
            0x08 => Status::InvalidArg,
            0x09 => Status::NoSpace,
            0x0A => Status::NameTooLong,
            _ => Status::NotSupported,
        }
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct OpenFlags: u32 {
        const READ = 0x1;
        const WRITE = 0x2;
        const CREATE = 0x4;
        const EXCL = 0x8;
        const TRUNC = 0x10;
        const APPEND = 0x20;
    }
}

impl OpenFlags {
    pub fn from_octal(mode: u32) -> OpenFlags {
        let mut f = OpenFlags::empty();
        if mode & 0o4 != 0 { f |= OpenFlags::READ; }
        if mode & 0o2 != 0 { f |= OpenFlags::WRITE; }
        f
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMode(pub u32);

impl FileMode {
    pub const S_IFMT: u32 = 0o170000;
    pub const S_IFREG: u32 = 0o100000;
    pub const S_IFDIR: u32 = 0o040000;
    pub const S_IFLNK: u32 = 0o120000;

    pub fn is_dir(self) -> bool { self.0 & Self::S_IFMT == Self::S_IFDIR }
    pub fn is_reg(self) -> bool { self.0 & Self::S_IFMT == Self::S_IFREG }
    pub fn is_symlink(self) -> bool { self.0 & Self::S_IFMT == Self::S_IFLNK }
    pub fn permissions(self) -> u32 { self.0 & 0o7777 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub mode: FileMode,
    pub size: u64,
    pub mtime: i64,
    pub atime: i64,
    pub ctime: i64,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub blksize: u32,
    pub blocks: u64,
}

impl Stat {
    /// Encode as a 60-byte little-endian struct for the wire.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(60);
        buf.extend_from_slice(&self.mode.0.to_le_bytes());
        buf.extend_from_slice(&self.size.to_le_bytes());
        buf.extend_from_slice(&self.mtime.to_le_bytes());
        buf.extend_from_slice(&self.atime.to_le_bytes());
        buf.extend_from_slice(&self.ctime.to_le_bytes());
        buf.extend_from_slice(&self.uid.to_le_bytes());
        buf.extend_from_slice(&self.gid.to_le_bytes());
        buf.extend_from_slice(&self.nlink.to_le_bytes());
        buf.extend_from_slice(&self.blksize.to_le_bytes());
        buf.extend_from_slice(&self.blocks.to_le_bytes());
        buf
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 60 { return None; }
        Some(Self {
            mode: FileMode(u32::from_le_bytes(bytes[0..4].try_into().ok()?)),
            size: u64::from_le_bytes(bytes[4..12].try_into().ok()?),
            mtime: i64::from_le_bytes(bytes[12..20].try_into().ok()?),
            atime: i64::from_le_bytes(bytes[20..28].try_into().ok()?),
            ctime: i64::from_le_bytes(bytes[28..36].try_into().ok()?),
            uid: u32::from_le_bytes(bytes[36..40].try_into().ok()?),
            gid: u32::from_le_bytes(bytes[40..44].try_into().ok()?),
            nlink: u32::from_le_bytes(bytes[44..48].try_into().ok()?),
            blksize: u32::from_le_bytes(bytes[48..52].try_into().ok()?),
            blocks: u64::from_le_bytes(bytes[52..60].try_into().ok()?),
        })
    }
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub stat: Stat,
}
