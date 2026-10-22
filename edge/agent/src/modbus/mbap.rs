//! MBAP framing for Modbus TCP (Modbus Messaging on TCP/IP Implementation Guide V1.0b, section
//! 3.1.3): a 7-byte header with a transaction identifier, a protocol identifier, a length and
//! the unit identifier, then the PDU.
//!
//! A response counts only when both its unit identifier and its transaction identifier match
//! the request. Anything else is read and discarded while the request keeps waiting: a response
//! from another unit (a converter that sends every response to every connection,
//! Laysi/trustiot#183) is counted as a unit mismatch, and one with an earlier transaction
//! identifier is the late answer to a request that already timed out. Frames that arrive
//! between requests answer nothing that is still asked, so they are dropped before the next
//! request is sent; a frame cut short stays buffered, since its length says where the next one
//! starts.

use std::hash::{BuildHasher, RandomState};

use super::ReadError;
use super::pdu::Read;
use super::tcp::Scan;

/// Largest MBAP frame: the 7-byte header and a 253-byte PDU.
pub(super) const MAX_FRAME: usize = 260;

/// The transaction identifiers of one connection's requests.
#[derive(Debug)]
pub(super) struct Transactions {
    /// Starts at a random value, so that another client of the same device, which usually
    /// counts from 0 or 1, is unlikely to use the same identifier at the same time.
    next: u16,
}

impl Transactions {
    pub(super) fn new() -> Self {
        Self { next: random_u16() }
    }

    /// The request frame for `read` to `unit` under the next transaction identifier, and what
    /// waits for its answer.
    pub(super) fn request(&mut self, unit: u8, read: Read) -> ([u8; 12], Waiting) {
        let transaction = self.next;
        self.next = transaction.wrapping_add(1);
        (
            request_frame(transaction, unit, &read),
            Waiting {
                transaction,
                unit,
                read,
            },
        )
    }
}

/// A request that was sent: what its answer carries.
#[derive(Debug)]
pub(super) struct Waiting {
    transaction: u16,
    unit: u8,
    read: Read,
}

impl Waiting {
    /// Takes the whole frames at the front of `buffer` up to and including the answer, counting
    /// those of other units in `unit_mismatches`; a frame cut short stays.
    pub(super) fn scan(&self, buffer: &mut Vec<u8>, unit_mismatches: &mut u32) -> Scan {
        loop {
            let frame = match take_frame(buffer) {
                Ok(Some(frame)) => frame,
                Ok(None) => return Scan::More,
                Err(detail) => return Scan::Garbled(detail),
            };
            if frame.unit != self.unit {
                *unit_mismatches += 1;
                continue;
            }
            if frame.transaction != self.transaction {
                continue;
            }
            return Scan::Answer(self.read.response(&frame.pdu).map_err(ReadError::from));
        }
    }
}

/// Drops the whole frames at the front of `buffer`, which arrived while no request waited, and
/// returns how many came from units other than `unit`. A frame cut short stays. Fails when a
/// header no Modbus TCP device sends shows the stream is not framed any more.
pub(super) fn drop_frames(buffer: &mut Vec<u8>, unit: u8) -> Result<u32, String> {
    let mut mismatches = 0;
    while let Some(frame) = take_frame(buffer)? {
        if frame.unit != unit {
            mismatches += 1;
        }
    }
    Ok(mismatches)
}

/// 16 random bits from the standard library's randomly keyed hasher.
fn random_u16() -> u16 {
    let [first, second, ..] = RandomState::new().hash_one(0_u8).to_le_bytes();
    u16::from_le_bytes([first, second])
}

fn request_frame(transaction: u16, unit: u8, read: &Read) -> [u8; 12] {
    let [transaction_high, transaction_low] = transaction.to_be_bytes();
    let [function, start_high, start_low, count_high, count_low] = read.request();
    // Protocol identifier 0; the length counts the unit identifier and the 5-byte PDU.
    [
        transaction_high,
        transaction_low,
        0,
        0,
        0,
        6,
        unit,
        function,
        start_high,
        start_low,
        count_high,
        count_low,
    ]
}

/// One MBAP frame.
#[derive(Debug, PartialEq, Eq)]
struct Frame {
    transaction: u16,
    unit: u8,
    pdu: Vec<u8>,
}

/// Takes the first frame off `buffer` once it is whole. A header no Modbus TCP device sends
/// means the stream is not framed any more.
fn take_frame(buffer: &mut Vec<u8>) -> Result<Option<Frame>, String> {
    let Some(&[t0, t1, p0, p1, l0, l1, unit]) = buffer.get(..7) else {
        return Ok(None);
    };
    let protocol = u16::from_be_bytes([p0, p1]);
    let length = u16::from_be_bytes([l0, l1]);
    if protocol != 0 {
        return Err(format!("MBAP protocol identifier {protocol} instead of 0"));
    }
    // The length counts the unit identifier and a PDU of 1 to 253 bytes.
    if !(2..=254).contains(&length) {
        return Err(format!("MBAP length {length} outside 2..=254"));
    }
    let end = 6 + usize::from(length);
    let Some(pdu) = buffer.get(7..end) else {
        return Ok(None);
    };
    let frame = Frame {
        transaction: u16::from_be_bytes([t0, t1]),
        unit,
        pdu: pdu.to_vec(),
    };
    buffer.drain(..end);
    Ok(Some(frame))
}

#[cfg(test)]
mod tests {
    use protocol::config::FunctionCode;

    use super::*;
    use crate::modbus::pdu::Data;

    #[test]
    fn requests_are_mbap_frames() {
        let read = Read::new(FunctionCode::ReadInputRegisters, 0x0102, 0x0003).unwrap();
        assert_eq!(
            request_frame(0xABCD, 17, &read),
            [0xAB, 0xCD, 0, 0, 0, 6, 17, 4, 0x01, 0x02, 0x00, 0x03]
        );
    }

    #[test]
    fn frames_are_taken_once_whole() {
        let mut buffer = vec![0x00, 0x07, 0, 0, 0, 5, 1, 3, 2];
        assert_eq!(take_frame(&mut buffer), Ok(None));
        buffer.extend([0x12, 0x34, 0x00, 0x08]); // the rest, and the start of the next frame
        assert_eq!(
            take_frame(&mut buffer),
            Ok(Some(Frame {
                transaction: 7,
                unit: 1,
                pdu: vec![3, 2, 0x12, 0x34],
            }))
        );
        assert_eq!(buffer, [0x00, 0x08]);
        assert_eq!(take_frame(&mut buffer), Ok(None));
    }

    #[test]
    fn headers_no_device_sends_end_the_framing() {
        let mut other_protocol = vec![0, 1, 0, 1, 0, 3, 1, 0x83, 2];
        assert!(take_frame(&mut other_protocol).is_err());
        for length in [0_u16, 1, 255, 0xFFFF] {
            let [high, low] = length.to_be_bytes();
            let mut buffer = vec![0, 1, 0, 0, high, low, 1];
            assert!(take_frame(&mut buffer).is_err(), "length {length}");
        }
        let mut longest = vec![0, 1, 0, 0, 0, 254, 1];
        longest.extend([3; 253]);
        assert!(matches!(take_frame(&mut longest), Ok(Some(frame)) if frame.pdu.len() == 253));
    }

    #[test]
    fn only_the_matching_unit_and_transaction_answers() {
        let mut transactions = Transactions { next: 7 };
        let read = Read::new(FunctionCode::ReadHoldingRegisters, 0, 1).unwrap();
        let (frame, waiting) = transactions.request(1, read);
        assert_eq!(frame[..2], [0, 7]);
        assert_eq!(transactions.next, 8);
        let mut buffer = vec![0, 6, 0, 0, 0, 5, 1, 3, 2, 0, 1]; // a late answer
        buffer.extend([0, 7, 0, 0, 0, 5, 9, 3, 2, 0, 2]); // another unit's
        buffer.extend([0, 7, 0, 0, 0, 5, 1, 3, 2, 0, 3]); // the answer
        buffer.extend([0, 8]); // the start of a frame after it
        let mut mismatches = 0;
        assert_eq!(
            waiting.scan(&mut buffer, &mut mismatches),
            Scan::Answer(Ok(Data::Registers(vec![3])))
        );
        assert_eq!(mismatches, 1);
        assert_eq!(buffer, [0, 8], "what follows the answer stays");
        assert_eq!(waiting.scan(&mut buffer, &mut mismatches), Scan::More);
    }

    #[test]
    fn frames_between_requests_are_dropped_and_a_cut_frame_stays() {
        let mut buffer = vec![0, 5, 0, 0, 0, 5, 1, 3, 2, 0, 7];
        buffer.extend([0, 6, 0, 0, 0, 5, 9, 3, 2, 0, 8]);
        buffer.extend([0, 7, 0, 0]);
        assert_eq!(drop_frames(&mut buffer, 1), Ok(1));
        assert_eq!(buffer, [0, 7, 0, 0]);
        buffer.extend([1, 0, 9]); // length 256
        assert!(drop_frames(&mut buffer, 1).is_err());
    }

    #[test]
    fn transactions_start_at_random() {
        // 8 connections all starting at the same identifier: probability 2^-112.
        let firsts: std::collections::HashSet<u16> =
            (0..8).map(|_| Transactions::new().next).collect();
        assert!(firsts.len() > 1, "{firsts:?}");
    }
}
