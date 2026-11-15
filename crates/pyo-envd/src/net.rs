//! The guest's network: address, default route and `/etc/resolv.conf`.
//!
//! The daemon sends `setNetwork` after every start and restore. A sandbox
//! restored from a template snapshot comes up with the template's address,
//! so this replaces it. Taking the interface down first drops the routes
//! that pointed through the old address.

use std::io;
use std::net::Ipv4Addr;

use pyo_proto::sandbox::wire::GuestNetwork;

pub fn apply(n: &GuestNetwork) -> io::Result<()> {
    if n.iface.is_empty() || n.iface.len() >= libc::IFNAMSIZ {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bad interface name",
        ));
    }
    if n.prefix > 32 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad prefix"));
    }
    let s = Sock::new()?;
    s.set_up(&n.iface, false)?;
    s.addr(&n.iface, libc::SIOCSIFADDR, n.address)?;
    s.addr(&n.iface, libc::SIOCSIFNETMASK, mask(n.prefix))?;
    s.set_up(&n.iface, true)?;
    s.default_route(&n.iface, n.gateway)?;
    write_resolv_conf(&n.dns)
}

fn mask(prefix: u8) -> Ipv4Addr {
    let bits = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix as u32)
    };
    Ipv4Addr::from(bits)
}

/// `/etc/resolv.conf` is often a symlink into `/run` in images. Replace it
/// with a plain file.
fn write_resolv_conf(dns: &[Ipv4Addr]) -> io::Result<()> {
    if dns.is_empty() {
        return Ok(());
    }
    let mut text = String::new();
    for d in dns {
        text.push_str(&format!("nameserver {d}\n"));
    }
    let _ = std::fs::create_dir_all("/etc");
    let tmp = "/etc/.resolv.conf.pyo";
    std::fs::write(tmp, text)?;
    std::fs::rename(tmp, "/etc/resolv.conf")
}

#[repr(C)]
struct IfReqFlags {
    name: [u8; libc::IFNAMSIZ],
    flags: libc::c_short,
    _pad: [u8; 22],
}

#[repr(C)]
struct IfReqAddr {
    name: [u8; libc::IFNAMSIZ],
    addr: libc::sockaddr_in,
    _pad: [u8; 8],
}

fn ifname(name: &str) -> [u8; libc::IFNAMSIZ] {
    let mut n = [0u8; libc::IFNAMSIZ];
    n[..name.len()].copy_from_slice(name.as_bytes());
    n
}

fn sockaddr(a: Ipv4Addr) -> libc::sockaddr_in {
    libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(a.octets()),
        },
        sin_zero: [0; 8],
    }
}

struct Sock(libc::c_int);

impl Drop for Sock {
    fn drop(&mut self) {
        // SAFETY: our own fd.
        unsafe { libc::close(self.0) };
    }
}

impl Sock {
    fn new() -> io::Result<Sock> {
        // SAFETY: a plain socket call.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Sock(fd))
    }

    fn ioctl<T>(&self, req: libc::c_ulong, arg: &mut T) -> io::Result<()> {
        // SAFETY: `arg` is the struct the request expects, sized for it.
        if unsafe { libc::ioctl(self.0, req as libc::Ioctl, arg as *mut T) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn set_up(&self, iface: &str, up: bool) -> io::Result<()> {
        let mut r = IfReqFlags {
            name: ifname(iface),
            flags: 0,
            _pad: [0; 22],
        };
        self.ioctl(libc::SIOCGIFFLAGS, &mut r)?;
        if up {
            r.flags |= libc::IFF_UP as libc::c_short;
        } else {
            r.flags &= !(libc::IFF_UP as libc::c_short);
        }
        self.ioctl(libc::SIOCSIFFLAGS, &mut r)
    }

    fn addr(&self, iface: &str, req: libc::c_ulong, a: Ipv4Addr) -> io::Result<()> {
        let mut r = IfReqAddr {
            name: ifname(iface),
            addr: sockaddr(a),
            _pad: [0; 8],
        };
        self.ioctl(req, &mut r)
    }

    fn default_route(&self, iface: &str, gateway: Ipv4Addr) -> io::Result<()> {
        let mut dev = ifname(iface);
        // SAFETY: rtentry is plain data, all zeroes is valid.
        let mut rt: libc::rtentry = unsafe { std::mem::zeroed() };
        let put = |dst: &mut libc::sockaddr, a: Ipv4Addr| {
            let sa = sockaddr(a);
            // SAFETY: sockaddr_in fits in sockaddr, both are 16 bytes.
            unsafe { std::ptr::write(dst as *mut libc::sockaddr as *mut libc::sockaddr_in, sa) };
        };
        put(&mut rt.rt_dst, Ipv4Addr::UNSPECIFIED);
        put(&mut rt.rt_genmask, Ipv4Addr::UNSPECIFIED);
        put(&mut rt.rt_gateway, gateway);
        rt.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
        rt.rt_dev = dev.as_mut_ptr().cast();
        match self.ioctl(libc::SIOCADDRT, &mut rt) {
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
            r => r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks() {
        assert_eq!(mask(30), Ipv4Addr::new(255, 255, 255, 252));
        assert_eq!(mask(0), Ipv4Addr::UNSPECIFIED);
        assert_eq!(mask(32), Ipv4Addr::BROADCAST);
        assert_eq!(mask(16), Ipv4Addr::new(255, 255, 0, 0));
    }
}
