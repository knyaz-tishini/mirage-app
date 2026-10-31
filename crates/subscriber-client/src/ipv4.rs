//! IPv4 fragmentation for the packet plane.
//!
//! The plane carries whole IP packets in QUIC datagrams, and a datagram has a
//! hard size limit ([`mirage_protocol::plane::MAX_PACKET_SIZE`]). A packet
//! that does not fit is split here, per RFC 791, before it goes into the
//! datagram.
//!
//! Only IPv4 is split, and only once: IPv6 has its own extension-header
//! rules, and a packet that is already a fragment is never split again -
//! the far side reassembles it, and a tunnel that re-fragments it would
//! break the reassembly.
//!
//! [`prepare_for_plane`] is the entry point for the packet plane: it takes
//! a packet of either version and returns the pieces that may ride the
//! plane. IPv6 rides whole or not at all - the plane does not fragment it.

/// The IPv4 header length in bytes when it carries no options.
const IPV4_MIN_HEADER: usize = 20;
/// The IPv6 header length in bytes. IPv6 has no options field, so the header
/// is always this long.
const IPV6_MIN_HEADER: usize = 40;
/// The "more fragments" flag, in the flags/fragment-offset word.
const FLAG_MORE_FRAGMENTS: u16 = 0x2000;
/// The mask of the fragment-offset field, in the flags/fragment-offset word.
const FRAGMENT_OFFSET_MASK: u16 = 0x1fff;
/// The payload of every fragment but the last is a multiple of this.
const FRAGMENT_PAYLOAD_UNIT: usize = 8;

/// Split `packet` into fragments that each fit into `budget` bytes.
///
/// The packet comes back untouched - as a single piece - when it cannot be
/// split: it already fits, it is not IPv4, it is too short to carry a
/// header, it is already a fragment, or the budget leaves no room for a
/// payload unit. A packet that does not fit and cannot be split cannot ride
/// the plane at all: the caller drops it.
pub fn fragment(packet: &[u8], budget: usize) -> Vec<Vec<u8>> {
    if packet.len() <= budget {
        return vec![packet.to_vec()];
    }
    let Some(header_len) = ipv4_header_len(packet) else {
        return vec![packet.to_vec()];
    };
    // Already a fragment: the far side reassembles it, we must not split it
    // a second time.
    let flags_offset = u16::from_be_bytes([packet[6], packet[7]]);
    if flags_offset & FRAGMENT_OFFSET_MASK != 0 || flags_offset & FLAG_MORE_FRAGMENTS != 0 {
        return vec![packet.to_vec()];
    }

    let payload = &packet[header_len..];
    let room = budget.saturating_sub(header_len);
    // Every fragment but the last carries a multiple of the payload unit.
    let chunk = room - (room % FRAGMENT_PAYLOAD_UNIT);
    if chunk == 0 {
        return vec![packet.to_vec()];
    }

    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut offset = 0usize;
    while offset < payload.len() {
        let end = (offset + chunk).min(payload.len());
        let more = end < payload.len();
        let mut piece = packet[..header_len].to_vec();
        piece.extend_from_slice(&payload[offset..end]);
        // The total length counts the header and this fragment's payload.
        let total = (header_len + (end - offset)) as u16;
        piece[2..4].copy_from_slice(&total.to_be_bytes());
        let word =
            (if more { FLAG_MORE_FRAGMENTS } else { 0 }) | ((offset / FRAGMENT_PAYLOAD_UNIT) as u16);
        piece[6..8].copy_from_slice(&word.to_be_bytes());
        // The checksum covers the header, and the header changed.
        piece[10..12].copy_from_slice(&[0, 0]);
        let sum = checksum(&piece[..header_len]);
        piece[10..12].copy_from_slice(&sum);
        out.push(piece);
        offset = end;
    }
    if out.is_empty() {
        return vec![packet.to_vec()];
    }
    out
}

/// Prepare an IP packet for the packet plane.
///
/// The plane carries whole IP packets in QUIC datagrams, and a datagram has a
/// hard size limit. This function returns the pieces of `packet` that may
/// ride the plane:
///
/// - An IPv4 packet that fits `budget` rides as one piece.
/// - An IPv4 packet that does not fit is split into fragments (see
///   [`fragment`]). A packet that cannot be split comes back as one piece
///   that is still too big: it cannot ride the plane, and the caller drops
///   it.
/// - An IPv6 packet that fits `budget` rides as one piece.
/// - An IPv6 packet that does not fit is dropped: the plane does not
///   fragment IPv6, and the far side has no fragment header to reassemble.
///
/// Returns an empty vector for a packet that cannot ride the plane at all:
/// one that is not IP, one too short to carry a header of its version.
pub fn prepare_for_plane(packet: &[u8], budget: usize) -> Vec<Vec<u8>> {
    // The version is the first four bits of the first byte. Without that
    // byte there is nothing to tell IPv4 from IPv6 from noise.
    let Some(&first) = packet.first() else {
        return Vec::new();
    };
    match first >> 4 {
        4 => {
            if packet.len() < IPV4_MIN_HEADER {
                return Vec::new();
            }
            if packet.len() <= budget {
                vec![packet.to_vec()]
            } else {
                fragment(packet, budget)
            }
        }
        6 => {
            if packet.len() < IPV6_MIN_HEADER {
                return Vec::new();
            }
            if packet.len() <= budget {
                vec![packet.to_vec()]
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

/// The length of the IPv4 header of `packet`, or `None` when `packet` is not
/// an IPv4 packet this crate may touch.
fn ipv4_header_len(packet: &[u8]) -> Option<usize> {
    if packet.len() < IPV4_MIN_HEADER {
        return None;
    }
    if packet[0] >> 4 != 4 {
        return None;
    }
    let header_len = (packet[0] & 0x0f) as usize * 4;
    if header_len < IPV4_MIN_HEADER || header_len > packet.len() {
        return None;
    }
    Some(header_len)
}

/// The Internet checksum of `bytes` (RFC 1071) - the one's complement of the
/// one's complement sum of the 16-bit words.
fn checksum(bytes: &[u8]) -> [u8; 2] {
    let mut sum: u32 = 0;
    let mut chunks = bytes.chunks_exact(2);
    for chunk in &mut chunks {
        sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
    }
    if let Some(&last) = chunks.remainder().first() {
        sum += (last as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    (!(sum as u16)).to_be_bytes()
}

/// The Internet checksum of a header that carries its own checksum sums to
/// zero.
#[cfg(test)]
fn checksum_is_valid(header: &[u8]) -> bool {
    let mut sum: u32 = 0;
    for chunk in header.chunks_exact(2) {
        sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum == 0xffff
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An IPv4 packet with a 20-byte header and `payload_len` payload bytes.
    fn ipv4_packet(payload_len: usize) -> Vec<u8> {
        let mut packet = vec![0u8; IPV4_MIN_HEADER + payload_len];
        packet[0] = 0x45; // version 4, header length 5 words
        packet[2..4].copy_from_slice(&((IPV4_MIN_HEADER + payload_len) as u16).to_be_bytes());
        packet[4..6].copy_from_slice(&0x1234u16.to_be_bytes()); // identification
        packet[8] = 64; // time to live
        packet[9] = 17; // protocol: UDP
        for (i, byte) in packet[IPV4_MIN_HEADER..].iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }
        let sum = checksum(&packet[..IPV4_MIN_HEADER]);
        packet[10..12].copy_from_slice(&sum);
        packet
    }

    /// An IPv6 packet with a 40-byte header and `payload_len` payload bytes.
    fn ipv6_packet(payload_len: usize) -> Vec<u8> {
        let mut packet = vec![0u8; IPV6_MIN_HEADER + payload_len];
        packet[0] = 0x60; // version 6, no traffic class
        packet[4..6].copy_from_slice(&((payload_len) as u16).to_be_bytes()); // payload length
        packet[6] = 17; // next header: UDP
        packet[7] = 64; // hop limit
        for (i, byte) in packet[IPV6_MIN_HEADER..].iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }
        packet
    }

    #[test]
    fn a_large_packet_is_split_into_fragments_that_fit() {
        let packet = ipv4_packet(2980);
        let pieces = fragment(&packet, 1280);

        assert!(pieces.len() > 1, "the packet must be split");
        for piece in &pieces {
            assert!(piece.len() <= 1280, "a fragment must fit the budget");
            assert!(checksum_is_valid(&piece[..IPV4_MIN_HEADER]), "the header checksum must be right");
        }

        // The payloads concatenate back into the original payload.
        let mut payload = Vec::new();
        for piece in &pieces {
            payload.extend_from_slice(&piece[IPV4_MIN_HEADER..]);
        }
        assert_eq!(payload, packet[IPV4_MIN_HEADER..]);

        // The offsets are the payload unit, and every fragment but the last
        // says that more are coming.
        let mut expected_offset = 0usize;
        for (i, piece) in pieces.iter().enumerate() {
            let word = u16::from_be_bytes([piece[6], piece[7]]);
            assert_eq!(word & FRAGMENT_OFFSET_MASK, (expected_offset / FRAGMENT_PAYLOAD_UNIT) as u16);
            let more = word & FLAG_MORE_FRAGMENTS != 0;
            assert_eq!(more, i + 1 < pieces.len(), "only the last fragment ends the chain");
            expected_offset += piece.len() - IPV4_MIN_HEADER;
        }
    }

    #[test]
    fn a_packet_that_fits_is_untouched() {
        let packet = ipv4_packet(100);
        let pieces = fragment(&packet, 1280);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0], packet);
    }

    #[test]
    fn a_non_ipv4_packet_is_untouched() {
        let mut packet = vec![0u8; 3000];
        packet[0] = 0x60; // version 6
        let pieces = fragment(&packet, 1280);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0], packet);
    }

    #[test]
    fn an_already_fragmented_packet_is_untouched() {
        let mut packet = ipv4_packet(2980);
        // more-fragments set, offset zero: the head of a fragment chain.
        packet[6..8].copy_from_slice(&FLAG_MORE_FRAGMENTS.to_be_bytes());
        let pieces = fragment(&packet, 1280);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0], packet);
    }

    #[test]
    fn a_packet_that_cannot_be_split_is_untouched() {
        let packet = ipv4_packet(2980);
        // A budget that leaves no room for a payload unit.
        let pieces = fragment(&packet, IPV4_MIN_HEADER);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0], packet);
    }

    #[test]
    fn an_ipv4_packet_that_fits_rides_as_one_piece() {
        let packet = ipv4_packet(100);
        let pieces = prepare_for_plane(&packet, 1280);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0], packet);
    }

    #[test]
    fn an_ipv4_packet_that_does_not_fit_is_fragmented() {
        let packet = ipv4_packet(2980);
        let pieces = prepare_for_plane(&packet, 1280);

        assert!(pieces.len() > 1, "the packet must be split");
        for piece in &pieces {
            assert!(piece.len() <= 1280, "a fragment must fit the budget");
            assert!(checksum_is_valid(&piece[..IPV4_MIN_HEADER]), "the header checksum must be right");
        }

        // The payloads concatenate back into the original payload.
        let mut payload = Vec::new();
        for piece in &pieces {
            payload.extend_from_slice(&piece[IPV4_MIN_HEADER..]);
        }
        assert_eq!(payload, packet[IPV4_MIN_HEADER..]);
    }

    #[test]
    fn an_ipv6_packet_that_fits_rides_as_one_piece() {
        let packet = ipv6_packet(100);
        let pieces = prepare_for_plane(&packet, 1280);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0], packet);
    }

    #[test]
    fn an_ipv6_packet_that_does_not_fit_is_dropped() {
        let packet = ipv6_packet(2000);
        let pieces = prepare_for_plane(&packet, 1280);
        assert!(pieces.is_empty(), "IPv6 is never fragmented, so it cannot ride");
    }

    #[test]
    fn a_packet_that_is_not_ip_is_dropped() {
        // No version nibble at all.
        assert!(prepare_for_plane(&[], 1280).is_empty());
        // A version nibble that is neither 4 nor 6.
        let mut packet = vec![0u8; 100];
        packet[0] = 0x50;
        assert!(prepare_for_plane(&packet, 1280).is_empty());
        // Too short to carry the header of its version.
        assert!(prepare_for_plane(&[0x45, 0, 0], 1280).is_empty());
        assert!(prepare_for_plane(&[0x60; 10], 1280).is_empty());
    }
}
