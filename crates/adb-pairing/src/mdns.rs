//! Minimal mDNS responder discovery for `_adb-tls-pairing._tcp` and
//! `_adb-tls-connect._tcp`.
//!
//! The adb server does this discovery on the host for the CLI flows; a QR
//! pairing client has no adb server in the loop (the whole point is that the
//! phone talks to *us*), so the client must find the phone's pairing server
//! itself. Only queries and the record types we need (PTR/SRV/TXT/A) are
//! implemented — this is a discovery client, not an mDNS implementation.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use crate::error::Error;

pub const PAIRING_TYPE: &str = "_adb-tls-pairing._tcp.local";
pub const CONNECT_TYPE: &str = "_adb-tls-connect._tcp.local";
const MULTICAST_ADDR: (Ipv4Addr, u16) = (Ipv4Addr::new(224, 0, 0, 251), 5353);

/// One discovered service instance.
#[derive(Debug, Clone)]
pub struct ServiceEndpoint {
    pub instance: String,
    pub host: String,
    pub ip: IpAddr,
    pub port: u16,
    /// TXT properties of the service instance.
    pub txt: HashMap<String, String>,
}

/// Build a DNS query for `_tcp` service instances of `service_type`.
fn build_query(query_id: u16, service_type: &str) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(&query_id.to_be_bytes());
    msg.extend_from_slice(&[0x00, 0x00]); // flags: query
    msg.extend_from_slice(&[0x00, 0x01]); // qdcount
    msg.extend_from_slice(&[0x00, 0x00]); // ancount
    msg.extend_from_slice(&[0x00, 0x00]); // nscount
    msg.extend_from_slice(&[0x00, 0x00]); // arcount
    write_name(&mut msg, service_type);
    msg.extend_from_slice(&[0x00, 0x0C]); // TYPE PTR
    msg.extend_from_slice(&[0x00, 0x01]); // CLASS IN
    msg
}

fn write_name(msg: &mut Vec<u8>, name: &str) {
    for label in name.split('.') {
        if label.is_empty() {
            continue;
        }
        msg.push(label.len() as u8);
        msg.extend_from_slice(label.as_bytes());
    }
    msg.push(0);
}

fn read_name(msg: &[u8], offset: &mut usize) -> Result<String, Error> {
    let mut parts: Vec<String> = Vec::new();
    loop {
        let len = *msg
            .get(*offset)
            .ok_or(Error::Protocol("dns name overrun".into()))? as usize;
        *offset += 1;
        if len == 0 {
            break;
        }
        if len & 0xC0 == 0xC0 {
            // Compression pointer.
            let hi = len & 0x3F;
            let lo = *msg
                .get(*offset)
                .ok_or(Error::Protocol("dns ptr overrun".into()))? as usize;
            *offset += 1;
            let mut ptr = (hi << 8) | lo;
            let suffix = read_name_at(msg, ptr)?;
            if !suffix.is_empty() {
                let mut all = parts.join(".");
                all.push('.');
                all.push_str(&suffix);
                return Ok(all);
            }
            ptr = 0;
            let _ = ptr;
            return Ok(parts.join("."));
        }
        let label = msg
            .get(*offset..*offset + len)
            .ok_or(Error::Protocol("dns label overrun".into()))?;
        parts.push(String::from_utf8_lossy(label).into_owned());
        *offset += len;
    }
    Ok(parts.join("."))
}

fn read_name_at(msg: &[u8], offset: usize) -> Result<String, Error> {
    let mut off = offset;
    read_name(msg, &mut off)
}

#[derive(Debug, Clone)]
enum Record {
    Ptr(String),
    Srv { target: String, port: u16 },
    Txt(Vec<String>),
    A(IpAddr),
}

/// Parse a DNS response packet into (owner_name, record) pairs.
fn parse_response(msg: &[u8]) -> Vec<(String, Record)> {
    let mut out = Vec::new();
    if msg.len() < 12 {
        return out;
    }
    let counts = |i: usize| u16::from_be_bytes([msg[i], msg[i + 1]]) as usize;
    let (qd, an, ns, ar) = (counts(4), counts(6), counts(8), counts(10));
    let mut off = 12usize;
    for _ in 0..qd {
        let Ok(name) = read_name(msg, &mut off) else {
            return out;
        };
        let _ = name;
        off += 4;
    }
    for _ in 0..(an + ns + ar) {
        let Ok(owner) = read_name(msg, &mut off) else {
            return out;
        };
        let Some(hdr) = msg.get(off..off + 10) else {
            return out;
        };
        let (rtype_hi, rtype_lo, len_hi, len_lo) = (hdr[0], hdr[1], hdr[8], hdr[9]);
        let rtype = u16::from_be_bytes([rtype_hi, rtype_lo]);
        let rdlen = u16::from_be_bytes([len_hi, len_lo]) as usize;
        off += 10;
        let rdata_start = off;
        let rdata_end = off + rdlen;
        let record = match rtype {
            12 => read_name(msg, &mut off).ok().map(Record::Ptr),
            33 => {
                let port = u16::from_be_bytes([
                    *msg.get(off + 4).unwrap_or(&0),
                    *msg.get(off + 5).unwrap_or(&0),
                ]);
                off += 6;
                read_name(msg, &mut off)
                    .ok()
                    .map(|target| Record::Srv { target, port })
            }
            16 => {
                let mut strings = Vec::new();
                let mut p = off;
                while p < rdata_end {
                    let slen = *msg.get(p).unwrap_or(&0) as usize;
                    p += 1;
                    let s = msg.get(p..p + slen).unwrap_or(&[]);
                    strings.push(String::from_utf8_lossy(s).into_owned());
                    p += slen;
                }
                off = rdata_end;
                Some(Record::Txt(strings))
            }
            1 if rdlen == 4 => {
                let ip = Ipv4Addr::new(msg[off], msg[off + 1], msg[off + 2], msg[off + 3]);
                off = rdata_end;
                Some(Record::A(ip.into()))
            }
            _ => {
                off = rdata_end;
                None
            }
        };
        if let Some(record) = record {
            out.push((owner, record));
        }
        let _ = rdata_start;
    }
    out
}

/// Discover instances of `service_type`, optionally filtered to an exact
/// instance name, until `timeout`. Returns the first match (already-resolved
/// IP and port), or an error on timeout.
pub fn discover(
    service_type: &str,
    instance: Option<&str>,
    timeout: Duration,
) -> Result<ServiceEndpoint, Error> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.join_multicast_v4(&MULTICAST_ADDR.0, &Ipv4Addr::UNSPECIFIED)?;
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut query_id = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0)
        % 65535) as u16;
    let deadline = std::time::Instant::now() + timeout;
    let mut ptrs: HashMap<String, ()> = HashMap::new();
    let mut srvs: HashMap<String, (String, u16)> = HashMap::new();
    let mut txts: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut addrs: HashMap<String, IpAddr> = HashMap::new();
    let mut buffer = [0u8; 4096];
    loop {
        if std::time::Instant::now() >= deadline {
            break;
        }
        query_id = query_id.wrapping_add(1);
        let query = build_query(query_id, service_type);
        let _ = socket.send_to(&query, MULTICAST_ADDR);
        let _ = socket.send_to(&query, SocketAddr::from(([255, 255, 255, 255], 5353)));
        match socket.recv_from(&mut buffer) {
            Ok((len, _)) => {
                for (owner, record) in parse_response(&buffer[..len]) {
                    match record {
                        Record::Ptr(target) => {
                            let inst = service_instance_name(service_type, &target);
                            if instance.is_none_or(|want| inst == want) {
                                ptrs.insert(target, ());
                            }
                        }
                        Record::Srv { target, port } => {
                            srvs.insert(target.clone(), (target.clone(), port));
                        }
                        Record::Txt(strings) => {
                            let mut map = HashMap::new();
                            for s in strings {
                                if let Some((k, v)) = s.split_once('=') {
                                    map.insert(k.to_string(), v.to_string());
                                }
                            }
                            txts.insert(owner, map);
                        }
                        Record::A(ip) => {
                            addrs.insert(owner, ip);
                        }
                    }
                }
                if let Some(ep) = resolve(service_type, instance, &ptrs, &srvs, &txts, &addrs) {
                    return Ok(ep);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => continue,
        }
    }
    Err(Error::Timeout)
}

fn resolve(
    service_type: &str,
    instance: Option<&str>,
    ptrs: &HashMap<String, ()>,
    srvs: &HashMap<String, (String, u16)>,
    txts: &HashMap<String, HashMap<String, String>>,
    addrs: &HashMap<String, IpAddr>,
) -> Option<ServiceEndpoint> {
    for target in ptrs.keys() {
        let inst = service_instance_name(service_type, target);
        if let Some(want) = instance
            && inst != want
        {
            continue;
        }
        let (host, port) = srvs.get(target)?;
        let ip = addrs.get(host)?;
        return Some(ServiceEndpoint {
            instance: inst,
            host: host.clone(),
            ip: *ip,
            port: *port,
            txt: txts.get(target).cloned().unwrap_or_default(),
        });
    }
    None
}

/// The bare instance name from a PTR target like
/// `studio-AbC123._adb-tls-pairing._tcp.local`.
fn service_instance_name(service_type: &str, target: &str) -> String {
    let type_len = service_type.len();
    let trimmed = target
        .strip_suffix(&format!(".{service_type}"))
        .unwrap_or(target);
    let _ = type_len;
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_round_trip_shape() {
        let query = build_query(7, PAIRING_TYPE);
        assert_eq!(&query[0..2], &[0, 7]);
        assert_eq!(&query[4..6], &[0, 1]); // qdcount
    }

    #[test]
    fn instance_name_extracts() {
        assert_eq!(
            service_instance_name(PAIRING_TYPE, "studio-AbC123._adb-tls-pairing._tcp.local"),
            "studio-AbC123"
        );
    }
}
