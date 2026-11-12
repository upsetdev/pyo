//! Conntrack events over netlink (NETLINK_NETFILTER, the "new" and
//! "destroy" groups), parsed by hand: the few attributes Pyo needs.

use std::net::IpAddr;

/// A connection as conntrack reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub kind: Kind,
    pub id: u32,
    /// 6 TCP, 17 UDP.
    pub proto: u8,
    /// The original direction: the client and what it connected to.
    pub src: IpAddr,
    pub src_port: u16,
    pub dst: IpAddr,
    pub dst_port: u16,
    /// Client to server, and back (zero without nf_conntrack_acct).
    pub bytes_orig: u64,
    pub packets_orig: u64,
    pub bytes_reply: u64,
    pub packets_reply: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    New,
    Destroy,
}

const NLMSG_HDRLEN: usize = 16;
const NFGEN_HDRLEN: usize = 4;
const NLM_F_CREATE: u16 = 0x400;
const IPCTNL_MSG_CT_NEW: u16 = 1 << 8;
const IPCTNL_MSG_CT_DELETE: u16 = (1 << 8) | 2;

const CTA_TUPLE_ORIG: u16 = 1;
const CTA_COUNTERS_ORIG: u16 = 9;
const CTA_COUNTERS_REPLY: u16 = 10;
const CTA_ID: u16 = 12;
const CTA_TUPLE_IP: u16 = 1;
const CTA_TUPLE_PROTO: u16 = 2;
const CTA_IP_V4_SRC: u16 = 1;
const CTA_IP_V4_DST: u16 = 2;
const CTA_IP_V6_SRC: u16 = 3;
const CTA_IP_V6_DST: u16 = 4;
const CTA_PROTO_NUM: u16 = 1;
const CTA_PROTO_SRC_PORT: u16 = 2;
const CTA_PROTO_DST_PORT: u16 = 3;
const CTA_COUNTERS_PACKETS: u16 = 1;
const CTA_COUNTERS_BYTES: u16 = 2;

/// Netlink attributes: (type without flags, payload).
fn attrs(mut b: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        if b.len() < 4 {
            return None;
        }
        let len = u16::from_ne_bytes([b[0], b[1]]) as usize;
        let ty = u16::from_ne_bytes([b[2], b[3]]) & 0x3fff;
        if len < 4 || len > b.len() {
            return None;
        }
        let payload = &b[4..len];
        let next = (len + 3) & !3;
        b = if next >= b.len() { &[] } else { &b[next..] };
        Some((ty, payload))
    })
}

fn be16(p: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes(p.get(..2)?.try_into().ok()?))
}
fn be32(p: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(p.get(..4)?.try_into().ok()?))
}
fn be64(p: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(p.get(..8)?.try_into().ok()?))
}
fn ip(p: &[u8]) -> Option<IpAddr> {
    match p.len() {
        4 => Some(IpAddr::from(<[u8; 4]>::try_from(p).ok()?)),
        16 => Some(IpAddr::from(<[u8; 16]>::try_from(p).ok()?)),
        _ => None,
    }
}

fn counters(p: &[u8]) -> (u64, u64) {
    let (mut bytes, mut packets) = (0, 0);
    for (t, v) in attrs(p) {
        match t {
            CTA_COUNTERS_BYTES => bytes = be64(v).unwrap_or(0),
            CTA_COUNTERS_PACKETS => packets = be64(v).unwrap_or(0),
            _ => {}
        }
    }
    (bytes, packets)
}

/// One netlink message (header included) as an event; None for updates,
/// other messages, or anything malformed.
pub fn parse_message(m: &[u8]) -> Option<Event> {
    if m.len() < NLMSG_HDRLEN + NFGEN_HDRLEN {
        return None;
    }
    let ty = u16::from_ne_bytes([m[4], m[5]]);
    let flags = u16::from_ne_bytes([m[6], m[7]]);
    let kind = match ty {
        IPCTNL_MSG_CT_NEW if flags & NLM_F_CREATE != 0 => Kind::New,
        IPCTNL_MSG_CT_DELETE => Kind::Destroy,
        _ => return None,
    };
    let mut e = Event {
        kind,
        id: 0,
        proto: 0,
        src: IpAddr::from([0u8; 4]),
        src_port: 0,
        dst: IpAddr::from([0u8; 4]),
        dst_port: 0,
        bytes_orig: 0,
        packets_orig: 0,
        bytes_reply: 0,
        packets_reply: 0,
    };
    let mut have_tuple = false;
    for (t, v) in attrs(&m[NLMSG_HDRLEN + NFGEN_HDRLEN..]) {
        match t {
            CTA_TUPLE_ORIG => {
                for (tt, tv) in attrs(v) {
                    match tt {
                        CTA_TUPLE_IP => {
                            for (it, iv) in attrs(tv) {
                                match it {
                                    CTA_IP_V4_SRC | CTA_IP_V6_SRC => e.src = ip(iv)?,
                                    CTA_IP_V4_DST | CTA_IP_V6_DST => e.dst = ip(iv)?,
                                    _ => {}
                                }
                            }
                        }
                        CTA_TUPLE_PROTO => {
                            for (pt, pv) in attrs(tv) {
                                match pt {
                                    CTA_PROTO_NUM => e.proto = *pv.first()?,
                                    CTA_PROTO_SRC_PORT => e.src_port = be16(pv)?,
                                    CTA_PROTO_DST_PORT => e.dst_port = be16(pv)?,
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                have_tuple = true;
            }
            CTA_COUNTERS_ORIG => (e.bytes_orig, e.packets_orig) = counters(v),
            CTA_COUNTERS_REPLY => (e.bytes_reply, e.packets_reply) = counters(v),
            CTA_ID => e.id = be32(v)?,
            _ => {}
        }
    }
    have_tuple.then_some(e)
}

/// Every message in one netlink datagram.
pub fn parse_datagram(mut b: &[u8]) -> Vec<Event> {
    let mut out = vec![];
    while b.len() >= NLMSG_HDRLEN {
        let len = u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as usize;
        if len < NLMSG_HDRLEN || len > b.len() {
            break;
        }
        if let Some(e) = parse_message(&b[..len]) {
            out.push(e);
        }
        let next = (len + 3) & !3;
        if next >= b.len() {
            break;
        }
        b = &b[next..];
    }
    out
}

/// A netlink socket subscribed to new and destroyed connections.
pub struct Socket(libc::c_int);

const NETLINK_NETFILTER: libc::c_int = 12;
const NF_NETLINK_CONNTRACK_NEW: u32 = 1;
const NF_NETLINK_CONNTRACK_DESTROY: u32 = 4;

impl Socket {
    pub fn open() -> std::io::Result<Self> {
        // SAFETY: plain socket calls; the fd is owned by Socket.
        unsafe {
            let fd = libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                NETLINK_NETFILTER,
            );
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let s = Socket(fd);
            // A big buffer (needs NET_ADMIN), so bursts don't overrun it.
            let size: libc::c_int = 8 << 20;
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUFFORCE,
                &size as *const _ as *const libc::c_void,
                std::mem::size_of_val(&size) as u32,
            );
            let mut addr: libc::sockaddr_nl = std::mem::zeroed();
            addr.nl_family = libc::AF_NETLINK as u16;
            addr.nl_groups = NF_NETLINK_CONNTRACK_NEW | NF_NETLINK_CONNTRACK_DESTROY;
            if libc::bind(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as u32,
            ) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(s)
        }
    }

    /// The next datagram's events. Err(ENOBUFS) means the kernel dropped
    /// events (the socket fell behind); the caller counts it and goes on.
    pub fn recv(&self, buf: &mut [u8]) -> std::io::Result<Vec<Event>> {
        // SAFETY: buf is valid for its length.
        let n = unsafe { libc::recv(self.0, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(parse_datagram(&buf[..n as usize]))
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // SAFETY: the fd is ours.
        unsafe { libc::close(self.0) };
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn nla(ty: u16, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(((payload.len() + 4) as u16).to_ne_bytes());
        v.extend(ty.to_ne_bytes());
        v.extend(payload);
        while v.len() % 4 != 0 {
            v.push(0);
        }
        v
    }

    /// A conntrack message as the kernel sends it.
    #[allow(clippy::too_many_arguments)]
    pub fn message(
        kind: Kind,
        id: u32,
        proto: u8,
        src: [u8; 4],
        sport: u16,
        dst: [u8; 4],
        dport: u16,
        counters: Option<(u64, u64, u64, u64)>,
    ) -> Vec<u8> {
        let ip = [nla(CTA_IP_V4_SRC, &src), nla(CTA_IP_V4_DST, &dst)].concat();
        let pr = [
            nla(CTA_PROTO_NUM, &[proto]),
            nla(CTA_PROTO_SRC_PORT, &sport.to_be_bytes()),
            nla(CTA_PROTO_DST_PORT, &dport.to_be_bytes()),
        ]
        .concat();
        let tuple = nla(
            CTA_TUPLE_ORIG | 0x8000,
            &[
                nla(CTA_TUPLE_IP | 0x8000, &ip),
                nla(CTA_TUPLE_PROTO | 0x8000, &pr),
            ]
            .concat(),
        );
        let mut body = tuple;
        body.extend(nla(CTA_ID, &id.to_be_bytes()));
        if let Some((bo, po, br, pr)) = counters {
            body.extend(nla(
                CTA_COUNTERS_ORIG | 0x8000,
                &[
                    nla(CTA_COUNTERS_PACKETS, &po.to_be_bytes()),
                    nla(CTA_COUNTERS_BYTES, &bo.to_be_bytes()),
                ]
                .concat(),
            ));
            body.extend(nla(
                CTA_COUNTERS_REPLY | 0x8000,
                &[
                    nla(CTA_COUNTERS_PACKETS, &pr.to_be_bytes()),
                    nla(CTA_COUNTERS_BYTES, &br.to_be_bytes()),
                ]
                .concat(),
            ));
        }
        let (ty, flags) = match kind {
            Kind::New => (IPCTNL_MSG_CT_NEW, NLM_F_CREATE | 0x200),
            Kind::Destroy => (IPCTNL_MSG_CT_DELETE, 0),
        };
        let mut m = Vec::new();
        m.extend(((NLMSG_HDRLEN + NFGEN_HDRLEN + body.len()) as u32).to_ne_bytes());
        m.extend(ty.to_ne_bytes());
        m.extend(flags.to_ne_bytes());
        m.extend([0u8; 8]);
        m.extend([2u8, 0, 0, 0]);
        m.extend(body);
        m
    }

    #[test]
    fn parses_new_and_destroy() {
        let new = message(
            Kind::New,
            7,
            6,
            [203, 0, 113, 9],
            50000,
            [10, 0, 2, 15],
            6379,
            None,
        );
        let destroy = message(
            Kind::Destroy,
            7,
            6,
            [203, 0, 113, 9],
            50000,
            [10, 0, 2, 15],
            6379,
            Some((1200, 10, 5400, 9)),
        );
        let events = parse_datagram(&[new, destroy].concat());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, Kind::New);
        assert_eq!(
            (
                events[0].id,
                events[0].proto,
                events[0].dst_port,
                events[0].src.to_string()
            ),
            (7, 6, 6379, "203.0.113.9".into())
        );
        assert_eq!(events[1].kind, Kind::Destroy);
        assert_eq!(
            (
                events[1].bytes_orig,
                events[1].packets_orig,
                events[1].bytes_reply,
                events[1].packets_reply
            ),
            (1200, 10, 5400, 9)
        );
    }

    #[test]
    fn updates_and_garbage_are_skipped() {
        let mut update = message(Kind::New, 1, 17, [1, 1, 1, 1], 1, [2, 2, 2, 2], 53, None);
        update[6] = 0;
        update[7] = 0;
        assert!(
            parse_datagram(&update).is_empty(),
            "an update, not a new connection"
        );
        assert!(parse_datagram(&[1, 2, 3]).is_empty());
        let mut cut = message(
            Kind::Destroy,
            1,
            17,
            [1, 1, 1, 1],
            1,
            [2, 2, 2, 2],
            53,
            None,
        );
        cut.truncate(30);
        assert!(parse_datagram(&cut).is_empty());
    }
}
