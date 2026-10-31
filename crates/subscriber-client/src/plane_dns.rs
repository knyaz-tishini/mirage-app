//! A minimal DNS client for the plane stack.
//!
//! The stack resolves names THROUGH the plane: the query leaves the client as
//! an ordinary UDP datagram towards a public resolver, the box forwards it
//! like any other packet, and the answer comes back the same way. Nothing
//! here touches the host resolver, so a name never resolves outside the
//! tunnel.
//!
//! Only what a SOCKS5 client needs is implemented: one question at a time,
//! `A` and `AAAA`, and the addresses of the answer section. The TCP side is
//! smoltcp's - this is the DNS wire format, which is a header, a question
//! and a list of records.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The `A` record type - an IPv4 address.
pub const TYPE_A: u16 = 1;
/// The `AAAA` record type - an IPv6 address.
pub const TYPE_AAAA: u16 = 28;
/// The `IN` class - the Internet.
pub const CLASS_IN: u16 = 1;

/// The flag that says "this is a response".
const FLAG_RESPONSE: u16 = 0x8000;
/// The flag that says "recursion desired" - the client asks the resolver to
/// do the walking for it.
const FLAG_RECURSION_DESIRED: u16 = 0x0100;

/// Build the query for `host` of type `qtype`.
///
/// `None` when the name cannot be put on the wire: an empty label, a label
/// longer than 63 bytes, or a name longer than 255 bytes.
pub fn build_query(id: u16, host: &str, qtype: u16) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(32 + host.len());
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&FLAG_RECURSION_DESIRED.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // one question
    out.extend_from_slice(&0u16.to_be_bytes()); // no answers yet
    out.extend_from_slice(&0u16.to_be_bytes()); // no authority records
    out.extend_from_slice(&0u16.to_be_bytes()); // no additional records
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    if out.len() > 255 {
        return None;
    }
    out.push(0); // the end of the name
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    Some(out)
}

/// The addresses of the answer section of the response `buf`.
///
/// Only the answers whose type matches `qtype` are returned, and only the
/// ones of class `IN`. A response that does not answer our question (wrong
/// id, not a response, truncated) yields nothing - the caller treats that as
/// a failed lookup, never as "the name has no addresses".
pub fn parse_answers(buf: &[u8], id: u16, qtype: u16) -> Vec<IpAddr> {
    let mut out = Vec::new();
    if buf.len() < 12 {
        return out;
    }
    if u16::from_be_bytes([buf[0], buf[1]]) != id {
        return out;
    }
    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    if flags & FLAG_RESPONSE == 0 {
        return out;
    }
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;

    let mut pos = 12usize;
    for _ in 0..qdcount {
        let Some(next) = skip_name(buf, pos) else {
            return out;
        };
        // the question's type and class
        pos = next + 4;
        if pos > buf.len() {
            return out;
        }
    }
    for _ in 0..ancount {
        let Some(next) = skip_name(buf, pos) else {
            break;
        };
        pos = next;
        if pos + 10 > buf.len() {
            break;
        }
        let rtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let rdlen = u16::from_be_bytes([buf[pos + 8], buf[pos + 9]]) as usize;
        pos += 10;
        if pos + rdlen > buf.len() {
            break;
        }
        if rtype == qtype {
            match (rtype, rdlen) {
                (TYPE_A, 4) => out.push(IpAddr::V4(Ipv4Addr::new(
                    buf[pos],
                    buf[pos + 1],
                    buf[pos + 2],
                    buf[pos + 3],
                ))),
                (TYPE_AAAA, 16) => {
                    let mut octets = [0u8; 16];
                    octets.copy_from_slice(&buf[pos..pos + 16]);
                    out.push(IpAddr::V6(Ipv6Addr::from(octets)));
                }
                _ => {}
            }
        }
        pos += rdlen;
    }
    out
}

/// The position right after the name that starts at `pos`.
///
/// A name is a sequence of labels, each prefixed by its length, and it ends
/// with a zero byte. A label whose two top bits are set is a compression
/// pointer: it is two bytes long and the name it points at lives elsewhere
/// in the message - the records after it still start right after the
/// pointer.
fn skip_name(buf: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *buf.get(pos)? as usize;
        if len == 0 {
            return Some(pos + 1);
        }
        if len & 0xc0 == 0xc0 {
            return Some(pos + 2);
        }
        pos += 1 + len;
        if pos >= buf.len() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_carries_the_name_and_the_type() {
        let query = build_query(0x1234, "example.com", TYPE_A).expect("a query");

        assert_eq!(&query[0..2], &0x1234u16.to_be_bytes());
        assert_eq!(&query[2..4], &FLAG_RECURSION_DESIRED.to_be_bytes());
        assert_eq!(&query[4..6], &1u16.to_be_bytes());
        // the name, label by label
        assert_eq!(query[12], 7);
        assert_eq!(&query[13..20], b"example");
        assert_eq!(query[20], 3);
        assert_eq!(&query[21..24], b"com");
        assert_eq!(query[24], 0);
        assert_eq!(&query[25..27], &TYPE_A.to_be_bytes());
        assert_eq!(&query[27..29], &CLASS_IN.to_be_bytes());
    }

    #[test]
    fn a_name_that_cannot_be_put_on_the_wire_is_refused() {
        assert!(build_query(1, "", TYPE_A).is_none());
        assert!(build_query(1, "example..com", TYPE_A).is_none());
        assert!(build_query(1, &"a".repeat(64), TYPE_A).is_none());
    }

    /// A response with one A answer for `example.com`.
    fn response(id: u16, addr: Ipv4Addr) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&(FLAG_RESPONSE | FLAG_RECURSION_DESIRED).to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes()); // one question
        out.extend_from_slice(&1u16.to_be_bytes()); // one answer
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        // the question
        out.push(7);
        out.extend_from_slice(b"example");
        out.push(3);
        out.extend_from_slice(b"com");
        out.push(0);
        out.extend_from_slice(&TYPE_A.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        // the answer, with a compression pointer back to the question's name
        out.extend_from_slice(&[0xc0, 0x0c]);
        out.extend_from_slice(&TYPE_A.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&60u32.to_be_bytes()); // ttl
        out.extend_from_slice(&4u16.to_be_bytes()); // rdlength
        out.extend_from_slice(&addr.octets());
        out
    }

    #[test]
    fn the_answer_of_a_response_is_read() {
        let buf = response(0x4321, Ipv4Addr::new(93, 184, 216, 34));
        let answers = parse_answers(&buf, 0x4321, TYPE_A);

        assert_eq!(answers, vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]);
    }

    #[test]
    fn a_response_to_another_question_is_not_ours() {
        let buf = response(0x4321, Ipv4Addr::new(93, 184, 216, 34));
        assert!(parse_answers(&buf, 0x1111, TYPE_A).is_empty());
    }

    #[test]
    fn a_response_of_another_type_is_not_ours() {
        let buf = response(0x4321, Ipv4Addr::new(93, 184, 216, 34));
        assert!(parse_answers(&buf, 0x4321, TYPE_AAAA).is_empty());
    }

    #[test]
    fn a_truncated_response_yields_nothing() {
        let buf = response(0x4321, Ipv4Addr::new(93, 184, 216, 34));
        assert!(parse_answers(&buf[..20], 0x4321, TYPE_A).is_empty());
        assert!(parse_answers(&[], 0x4321, TYPE_A).is_empty());
    }
}
