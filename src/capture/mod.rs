//! Raw packet capture.
//!
//! Linux exposes packet capture to ordinary processes through `AF_PACKET`
//! sockets. loot-ledger uses that directly: no libpcap, no kernel module, no
//! third-party crate. The only privilege required is `CAP_NET_RAW`, which means
//! the binary can run entirely unprivileged once the capability is granted to
//! the file itself.

pub mod bpf;
pub mod sys;

use std::ffi::c_int;
use std::time::Duration;

/// Largest frame we will read. A jumbo Ethernet frame is 9 KiB; anything
/// larger is not something we need.
pub const MAX_FRAME: usize = 16 * 1024;

/// Why a frame did not yield a UDP payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Shorter than a link-layer header.
    Truncated,
    /// Not IPv4 (ARP, IPv6, or something else).
    NotIpv4,
    /// IPv4 but not UDP.
    NotUdp,
    /// A non-initial IP fragment; the UDP header is not present.
    IpFragment,
    /// Headers claim more bytes than the frame actually contains.
    Malformed,
}

/// Counters describing what the capture layer has seen.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CaptureStats {
    /// Frames returned by the socket.
    pub frames: u64,
    /// Frames that carried a UDP payload.
    pub udp_frames: u64,
    /// Frames skipped, by reason.
    pub skipped: u64,
    /// `SkipReason` breakdown, indexed by discriminant.
    pub skipped_not_ipv4: u64,
    pub skipped_not_udp: u64,
    pub skipped_truncated: u64,
    pub skipped_malformed: u64,
    pub skipped_ip_fragment: u64,
}

/// A network interface available for capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub index: c_int,
    pub is_loopback: bool,
}

/// List the host's network interfaces by reading sysfs, avoiding any need to
/// call `getifaddrs`.
pub fn list_interfaces() -> Vec<Interface> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return out;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(index) = std::fs::read_to_string(entry.path().join("ifindex")) else {
            continue;
        };
        let Ok(index) = index.trim().parse::<c_int>() else {
            continue;
        };

        out.push(Interface {
            is_loopback: name == "lo",
            name,
            index,
        });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// A UDP payload lifted out of a captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Datagram<'a> {
    pub payload: &'a [u8],
    pub src_port: u16,
    pub dst_port: u16,
}

/// Pull the UDP payload out of an Ethernet frame.
///
/// Handles the 802.1Q VLAN tag and IPv4 options, and refuses non-initial IP
/// fragments, where the UDP header is not in the packet at all.
pub fn parse_frame(frame: &[u8]) -> Result<Datagram<'_>, SkipReason> {
    if frame.len() < 14 {
        return Err(SkipReason::Truncated);
    }

    let mut offset = 12;
    let mut ether_type = u16::from_be_bytes([frame[offset], frame[offset + 1]]);
    offset += 2;

    // Unwrap any number of stacked VLAN tags.
    while matches!(ether_type, 0x8100 | 0x88a8) {
        if frame.len() < offset + 4 {
            return Err(SkipReason::Truncated);
        }
        ether_type = u16::from_be_bytes([frame[offset + 2], frame[offset + 3]]);
        offset += 4;
    }

    if ether_type != 0x0800 {
        return Err(SkipReason::NotIpv4);
    }

    if frame.len() < offset + 20 {
        return Err(SkipReason::Truncated);
    }

    let version_ihl = frame[offset];
    if version_ihl >> 4 != 4 {
        return Err(SkipReason::NotIpv4);
    }

    let header_len = ((version_ihl & 0x0f) as usize) * 4;
    if header_len < 20 || frame.len() < offset + header_len {
        return Err(SkipReason::Malformed);
    }

    if frame[offset + 9] != 17 {
        return Err(SkipReason::NotUdp);
    }

    // A non-zero fragment offset means the UDP header lives in a different
    // packet, so there is nothing to hand to the Photon parser.
    let frag = u16::from_be_bytes([frame[offset + 6], frame[offset + 7]]);
    if frag & 0x1fff != 0 {
        return Err(SkipReason::IpFragment);
    }

    let udp = offset + header_len;
    if frame.len() < udp + 8 {
        return Err(SkipReason::Truncated);
    }

    let src_port = u16::from_be_bytes([frame[udp], frame[udp + 1]]);
    let dst_port = u16::from_be_bytes([frame[udp + 2], frame[udp + 3]]);
    let udp_len = u16::from_be_bytes([frame[udp + 4], frame[udp + 5]]) as usize;

    if udp_len < 8 {
        return Err(SkipReason::Malformed);
    }

    // Trust the shorter of the declared and actual lengths: a frame can be
    // padded, but it cannot be shorter than its own headers claim.
    let declared_end = udp + udp_len;
    let end = declared_end.min(frame.len());
    if end <= udp + 8 {
        return Err(SkipReason::Malformed);
    }

    Ok(Datagram {
        payload: &frame[udp + 8..end],
        src_port,
        dst_port,
    })
}

/// An open capture socket.
pub struct Capture {
    fd: c_int,
    filter_attached: bool,
    buffer: Vec<u8>,
}

impl Capture {
    /// Open a capture socket.
    ///
    /// `ifindex` of `None` listens on every interface; `Some(n)` restricts to
    /// one. A BPF filter for `ports` is attached when the kernel accepts it.
    pub fn open(ifindex: Option<c_int>, ports: &[u16]) -> std::io::Result<(Capture, bool)> {
        let filter = bpf::albion_filter(ports);
        let idx = ifindex.unwrap_or(0);

        match sys::open_packet_socket(idx, &filter) {
            Ok(fd) => Ok((
                Capture {
                    fd,
                    filter_attached: true,
                    buffer: vec![0u8; MAX_FRAME],
                },
                true,
            )),
            Err(e) => {
                // EPERM from SO_ATTACH_FILTER usually means seccomp blocks it.
                // Capture still works without the filter; only the CPU cost
                // changes. EINVAL means the program is malformed, which is our
                // bug and must not be papered over.
                if e.raw_os_error() == Some(22) {
                    return Err(e);
                }
                let fd = sys::open_packet_socket(idx, &[])?;
                Ok((
                    Capture {
                        fd,
                        filter_attached: false,
                        buffer: vec![0u8; MAX_FRAME],
                    },
                    false,
                ))
            }
        }
    }

    /// Whether the kernel-side filter is in place.
    pub fn has_filter(&self) -> bool {
        self.filter_attached
    }

    /// Read the next frame.
    ///
    /// `Ok(None)` means the read timed out with nothing to do, which lets the
    /// caller service its own periodic work.
    pub fn next_frame(&mut self, timeout: Duration) -> std::io::Result<Option<(usize, c_int)>> {
        sys::recv_frame(self.fd, &mut self.buffer, timeout)
    }

    /// The current frame most recently read.
    pub fn frame(&self, len: usize) -> &[u8] {
        &self.buffer[..len]
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        sys::close_fd(self.fd);
    }
}

/// Translate a capture-layer failure into the stat counters.
pub fn record_skip(stats: &mut CaptureStats, reason: SkipReason) {
    stats.skipped += 1;
    match reason {
        SkipReason::NotIpv4 => stats.skipped_not_ipv4 += 1,
        SkipReason::NotUdp => stats.skipped_not_udp += 1,
        SkipReason::Truncated => stats.skipped_truncated += 1,
        SkipReason::Malformed => stats.skipped_malformed += 1,
        SkipReason::IpFragment => stats.skipped_ip_fragment += 1,
    }
}

/// True when the running process can open a raw socket.
///
/// Checked before doing any setup so the user gets a clear instruction rather
/// than a bare `Permission denied` from deep inside the socket setup.
pub fn have_capture_privilege() -> bool {
    // CAP_NET_RAW is capability 13; the effective set lives in /proc/self/status.
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return false;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("CapEff:") {
            let Ok(bits) = u64::from_str_radix(rest.trim(), 16) else {
                return false;
            };
            return bits & (1 << 13) != 0;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eth_ip_udp(src: u16, dst: u16, payload: &[u8]) -> Vec<u8> {
        let udp_len = 8 + payload.len();
        let mut f = vec![0xaa; 6];
        f.extend_from_slice(&[0xbb; 6]);
        f.extend_from_slice(&0x0800u16.to_be_bytes());
        f.extend_from_slice(&[0x45, 0x00]);
        f.extend_from_slice(&((20 + udp_len) as u16).to_be_bytes());
        f.extend_from_slice(&[0x00, 0x00]);
        f.extend_from_slice(&[0x40, 0x00]);
        f.extend_from_slice(&[64, 17]);
        f.extend_from_slice(&[0x00, 0x00]);
        f.extend_from_slice(&[10, 0, 0, 1]);
        f.extend_from_slice(&[10, 0, 0, 2]);
        f.extend_from_slice(&src.to_be_bytes());
        f.extend_from_slice(&dst.to_be_bytes());
        f.extend_from_slice(&(udp_len as u16).to_be_bytes());
        f.extend_from_slice(&[0x00, 0x00]);
        f.extend_from_slice(payload);
        f
    }

    #[test]
    fn extracts_udp_payload() {
        let frame = eth_ip_udp(5056, 12345, b"hello");
        let d = parse_frame(&frame).unwrap();
        assert_eq!(d.payload, b"hello");
        assert_eq!(d.src_port, 5056);
        assert_eq!(d.dst_port, 12345);
    }

    #[test]
    fn handles_ipv4_options() {
        let mut frame = eth_ip_udp(5056, 1, b"hi");
        // IHL 6 => 24-byte IPv4 header with 4 bytes of options.
        frame[14] = 0x46; // IHL 6
        for _ in 0..4 {
            frame.insert(34, 0); // 4 bytes of options
        }
        let d = parse_frame(&frame).unwrap();
        assert_eq!(d.payload, b"hi");
    }

    #[test]
    fn handles_vlan_tagged_frames() {
        let mut frame = vec![0xaa; 12];
        frame.extend_from_slice(&0x8100u16.to_be_bytes()); // VLAN
        frame.extend_from_slice(&0x0064u16.to_be_bytes()); // priority
        frame.extend_from_slice(&0x0800u16.to_be_bytes()); // inner IPv4
        let inner = eth_ip_udp(5056, 1, b"vlan");
        frame.extend_from_slice(&inner[14..]);
        assert_eq!(parse_frame(&frame).unwrap().payload, b"vlan");
    }

    #[test]
    fn rejects_non_initial_ip_fragments() {
        let mut frame = eth_ip_udp(5056, 1, b"x");
        frame[20..22].copy_from_slice(&0x0001u16.to_be_bytes()); // offset 1
        assert_eq!(parse_frame(&frame), Err(SkipReason::IpFragment));
    }

    #[test]
    fn rejects_non_ipv4() {
        let mut frame = eth_ip_udp(5056, 1, b"x");
        frame[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
        assert_eq!(parse_frame(&frame), Err(SkipReason::NotIpv4));
    }

    #[test]
    fn rejects_non_udp() {
        let mut frame = eth_ip_udp(5056, 1, b"x");
        frame[23] = 6; // TCP
        assert_eq!(parse_frame(&frame), Err(SkipReason::NotUdp));
    }

    #[test]
    fn rejects_runt_and_garbage_without_panicking() {
        for junk in [vec![], vec![0u8; 13], vec![0xffu8; 14], vec![0xaa; 40]] {
            let _ = parse_frame(&junk);
        }
    }

    #[test]
    fn udp_length_larger_than_frame_is_clamped_not_trusted() {
        let mut frame = eth_ip_udp(5056, 1, b"abc");
        frame[38..40].copy_from_slice(&9999u16.to_be_bytes()); // absurd length
        let d = parse_frame(&frame).unwrap();
        assert_eq!(d.payload, b"abc");
    }

    #[test]
    fn skip_reasons_are_recorded() {
        let mut stats = CaptureStats::default();
        record_skip(&mut stats, SkipReason::NotIpv4);
        record_skip(&mut stats, SkipReason::NotUdp);
        assert_eq!(stats.skipped, 2);
        assert_eq!(stats.skipped_not_ipv4, 1);
        assert_eq!(stats.skipped_not_udp, 1);
    }

    #[test]
    fn interface_listing_includes_loopback() {
        // The test host always has `lo`.
        let ifaces = list_interfaces();
        assert!(ifaces.iter().any(|i| i.is_loopback));
    }
}
