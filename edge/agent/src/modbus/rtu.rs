//! RTU framing over TCP, for an RS485 converter (Modbus over Serial Line Specification and
//! Implementation Guide V1.02, section 2.5.1): the unit address, the PDU, and a CRC-16.
//!
//! RTU has no transaction identifier and no length field, and TCP does not carry the serial
//! line's 3.5-character silence that ends a frame. The length of a response follows from its
//! function code and byte count, and the CRC decides whether bytes are a frame. Bytes that form
//! no frame are skipped one at a time until a frame starts: the stream resynchronises instead
//! of failing.
//!
//! A response counts only when it comes from the unit asked and fits the request: the same
//! function code and byte count ([`Read::response`]). Anything else is read and discarded while
//! the request keeps waiting: a frame from another unit (a converter sends every device's
//! response to every connection, Laysi/trustiot#183) is counted as a unit mismatch, a frame of
//! the same unit that does not fit is a late answer to an earlier request or another master's,
//! and a converter that echoes the request sends it back before the answer. Frames that arrive
//! between requests are dropped, and the buffer is emptied before the next request is sent:
//! without a transaction identifier, a frame cut short cannot be told from the start of the
//! answer.

use super::ReadError;
use super::pdu::{Read, ResponseError};
use super::tcp::Scan;

/// Bytes of a read request: unit, function code, start address, count, CRC.
const REQUEST_LEN: usize = 8;

/// The Modbus RTU CRC-16 (polynomial 0xA001 reflected, initial value 0xFFFF); a frame carries
/// it low byte first.
pub(super) fn crc16(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0xFFFF_u16, |crc, byte| {
        (0..8).fold(crc ^ u16::from(*byte), |crc, _| {
            if crc & 1 == 1 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            }
        })
    })
}

/// The request frame for `read` to `unit`, and what waits for its answer.
pub(super) fn request(unit: u8, read: Read) -> ([u8; REQUEST_LEN], Waiting) {
    let [function, start_high, start_low, count_high, count_low] = read.request();
    let [crc_low, crc_high] =
        crc16(&[unit, function, start_high, start_low, count_high, count_low]).to_le_bytes();
    let frame = [
        unit, function, start_high, start_low, count_high, count_low, crc_low, crc_high,
    ];
    (
        frame,
        Waiting {
            unit,
            read,
            request: frame,
            unfit: None,
            noise: 0,
        },
    )
}

/// A request that was sent: what its answer looks like, and what arrived instead so far.
#[derive(Debug)]
pub(super) struct Waiting {
    unit: u8,
    read: Read,
    /// The request as sent, to recognise a converter's echo of it.
    request: [u8; REQUEST_LEN],
    /// Why the last whole frame from the unit did not fit the request.
    unfit: Option<String>,
    /// Bytes skipped because they formed no frame.
    noise: usize,
}

impl Waiting {
    /// Takes what is at the front of `buffer` up to and including the answer, counting frames
    /// of other units in `unit_mismatches`. What may still become a frame stays.
    pub(super) fn scan(&mut self, buffer: &mut Vec<u8>, unit_mismatches: &mut u32) -> Scan {
        // Every pass drains at least one byte or returns.
        loop {
            if buffer.starts_with(&self.request) {
                buffer.drain(..REQUEST_LEN);
                continue;
            }
            match front(buffer) {
                Front::Frame(len) => {
                    let frame: Vec<u8> = buffer.drain(..len).collect();
                    let Some((unit, pdu)) = parts(&frame) else {
                        continue;
                    };
                    if unit != self.unit {
                        *unit_mismatches += 1;
                        continue;
                    }
                    match self.read.response(pdu) {
                        Ok(data) => return Scan::Answer(Ok(data)),
                        Err(ResponseError::Exception(code)) => {
                            return Scan::Answer(Err(ReadError::Exception(code)));
                        }
                        Err(ResponseError::Malformed(detail)) => self.unfit = Some(detail),
                    }
                }
                // Perhaps the first part of an echo: wait for the rest of it.
                _ if self.request.starts_with(buffer) => return Scan::More,
                Front::Noise => {
                    buffer.drain(..1);
                    self.noise += 1;
                }
                // A frame cut short, or noise that reads like the start of a long frame. In the
                // second case the answer may have arrived after it already.
                Front::Incomplete => match self.answer_after_front(buffer) {
                    Some(start) => {
                        buffer.drain(..start);
                        self.noise += start;
                    }
                    None => return Scan::More,
                },
            }
        }
    }

    /// Where a whole frame from the unit that fits the request starts, after the first byte of
    /// `buffer`. Unit, function code, byte count and CRC all have to match, so noise in front
    /// of the answer is practically never taken for it.
    fn answer_after_front(&self, buffer: &[u8]) -> Option<usize> {
        let function = self.read.function().number();
        (1..buffer.len()).find(|&start| {
            let rest = buffer.get(start..).unwrap_or_default();
            let Some(&[unit, code]) = rest.get(..2) else {
                return false;
            };
            unit == self.unit
                && (code == function || code == function | 0x80)
                && matches!(front(rest), Front::Frame(len) if rest
                    .get(..len)
                    .and_then(parts)
                    .is_some_and(|(_, pdu)| self.fits(pdu)))
        })
    }

    /// Whether `pdu` answers the request: the values, or an exception to its function.
    fn fits(&self, pdu: &[u8]) -> bool {
        !matches!(self.read.response(pdu), Err(ResponseError::Malformed(_)))
    }

    /// Why the request got no answer by its deadline. A frame from the unit that did not fit, or
    /// bytes that never formed a frame, say more than silence does: a converter whose serial
    /// settings (baud rate, parity) differ from the devices' produces bytes that never form a
    /// frame.
    pub(super) fn timed_out(&self) -> ReadError {
        match (&self.unfit, self.noise) {
            (Some(detail), _) => ReadError::Malformed(format!(
                "a frame from unit {} did not fit the request ({detail}), and none that fits arrived",
                self.unit
            )),
            (None, 0) => ReadError::Timeout,
            (None, noise) => ReadError::Malformed(format!(
                "{noise} bytes that form no RTU frame arrived, and no answer; check the converter's serial settings"
            )),
        }
    }
}

/// Drops the whole frames at the front of `buffer`, which arrived while no request waited, and
/// returns how many came from units other than `unit`. Bytes that form no frame are dropped too;
/// a frame cut short stays, so that its rest is read with it.
pub(super) fn drop_frames(buffer: &mut Vec<u8>, unit: u8) -> u32 {
    let mut mismatches = 0;
    loop {
        match front(buffer) {
            Front::Frame(len) => {
                if buffer.first() != Some(&unit) {
                    mismatches += 1;
                }
                buffer.drain(..len);
            }
            Front::Noise => {
                buffer.drain(..1);
            }
            Front::Incomplete => return mismatches,
        }
    }
}

/// What the front of the received bytes is, read as a response frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Front {
    /// A whole frame of this many bytes with a matching CRC.
    Frame(usize),
    /// Not enough bytes yet to tell.
    Incomplete,
    /// No response frame starts at the first byte.
    Noise,
}

/// Reads the front of `bytes` as a response frame. Its length follows from the function code: a
/// read (1–4) carries a byte count that one read can return (1–250 bytes of bits, an even 2–250
/// bytes of registers); the write responses another master on the same bus may cause (5, 6, 15,
/// 16) are 8 bytes; an exception (function code with the high bit set) is 5. Any other
/// function code, an implausible byte count or a wrong CRC is noise.
fn front(bytes: &[u8]) -> Front {
    let Some(&function) = bytes.get(1) else {
        return Front::Incomplete;
    };
    let len = match function {
        1..=4 => {
            let Some(&byte_count) = bytes.get(2) else {
                return Front::Incomplete;
            };
            let plausible = if function <= 2 {
                (1..=250).contains(&byte_count)
            } else {
                byte_count % 2 == 0 && (2..=250).contains(&byte_count)
            };
            if !plausible {
                return Front::Noise;
            }
            5 + usize::from(byte_count)
        }
        5 | 6 | 15 | 16 => 8,
        0x81..=0xFF => 5,
        _ => return Front::Noise,
    };
    match bytes.get(..len) {
        None => Front::Incomplete,
        Some(frame) if crc_matches(frame) => Front::Frame(len),
        Some(_) => Front::Noise,
    }
}

/// Whether the last two bytes of `frame` are the CRC of the bytes before them.
fn crc_matches(frame: &[u8]) -> bool {
    frame
        .split_last_chunk::<2>()
        .is_some_and(|(body, crc)| crc16(body).to_le_bytes() == *crc)
}

/// The unit and the PDU of a whole frame.
fn parts(frame: &[u8]) -> Option<(u8, &[u8])> {
    let (&unit, rest) = frame.split_first()?;
    let (pdu, _crc) = rest.split_last_chunk::<2>()?;
    Some((unit, pdu))
}

#[cfg(test)]
mod tests {
    use protocol::config::FunctionCode;

    use super::*;
    use crate::modbus::pdu::Data;

    /// A frame of `unit` with `pdu` and its CRC.
    fn frame(unit: u8, pdu: &[u8]) -> Vec<u8> {
        let mut frame = vec![unit];
        frame.extend(pdu);
        frame.extend(crc16(&frame).to_le_bytes());
        frame
    }

    /// A response frame with holding register values.
    fn registers(unit: u8, values: &[u16]) -> Vec<u8> {
        let mut pdu = vec![3, u8::try_from(values.len() * 2).unwrap()];
        pdu.extend(values.iter().flat_map(|value| value.to_be_bytes()));
        frame(unit, &pdu)
    }

    fn holding(start: u16, count: u16) -> Read {
        Read::new(FunctionCode::ReadHoldingRegisters, start, count).unwrap()
    }

    #[test]
    fn crc_matches_the_specification_example_and_testkit() {
        // Modbus over serial line 1.02: read 10 holding registers of unit 1 from address 0.
        assert_eq!(
            crc16(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x0A]).to_le_bytes(),
            [0xC5, 0xCD]
        );
        let long: Vec<u8> = (0..=255).collect();
        for bytes in [
            &[][..],
            &[0x00],
            &[0xFF; 7],
            &[0x11, 0x04, 0x02, 0xFF, 0xFF],
            &long,
        ] {
            assert_eq!(crc16(bytes), testkit::modbus::crc16(bytes), "{bytes:02X?}");
        }
    }

    #[test]
    fn requests_are_rtu_frames() {
        let (frame, _) = request(1, holding(0, 10));
        assert_eq!(frame, [0x01, 0x03, 0x00, 0x00, 0x00, 0x0A, 0xC5, 0xCD]);
        for function in [
            FunctionCode::ReadCoils,
            FunctionCode::ReadDiscreteInputs,
            FunctionCode::ReadHoldingRegisters,
            FunctionCode::ReadInputRegisters,
        ] {
            let read = Read::new(function, 0x0102, 3).unwrap();
            let (frame, _) = request(247, read);
            assert_eq!(frame[..6], [247, function.number(), 0x01, 0x02, 0x00, 0x03]);
            assert_eq!(
                frame[6..],
                testkit::modbus::crc16(&frame[..6]).to_le_bytes()
            );
        }
    }

    #[test]
    fn the_front_is_read_by_function_code() {
        // Reads: 5 bytes plus the byte count.
        let answer = registers(1, &[1, 2]);
        assert_eq!(front(&answer), Front::Frame(9));
        let mut longer = answer.clone();
        longer.extend([0xAA, 0xBB]);
        assert_eq!(front(&longer), Front::Frame(9), "what follows is not part");
        for cut in 0..answer.len() {
            assert_eq!(front(&answer[..cut]), Front::Incomplete, "{cut} bytes");
        }
        let coils = frame(1, &[1, 1, 0b101]);
        assert_eq!(front(&coils), Front::Frame(6));
        assert_eq!(front(&frame(1, &[2, 250])), Front::Incomplete);
        // Byte counts no read returns.
        for pdu in [&[1, 0][..], &[2, 251], &[3, 0], &[4, 3], &[3, 252]] {
            assert_eq!(front(&frame(1, pdu)), Front::Noise, "{pdu:?}");
        }
        // Write responses of another master: 8 bytes.
        for function in [5, 6, 15, 16] {
            let write = frame(1, &[function, 0, 1, 0, 2]);
            assert_eq!(front(&write), Front::Frame(8), "function {function}");
            assert_eq!(front(&write[..7]), Front::Incomplete);
        }
        // Exceptions: 5 bytes.
        assert_eq!(front(&frame(1, &[0x83, 2])), Front::Frame(5));
        assert_eq!(front(&frame(1, &[0xFF, 1])), Front::Frame(5));
        // Other function codes.
        for function in [0, 7, 8, 17, 43, 0x80] {
            assert_eq!(
                front(&frame(1, &[function, 2, 0, 0])),
                Front::Noise,
                "{function}"
            );
        }
        // A wrong CRC.
        let mut corrupted = registers(1, &[1]);
        if let Some(last) = corrupted.last_mut() {
            *last ^= 1;
        }
        assert_eq!(front(&corrupted), Front::Noise);
        assert_eq!(front(&[]), Front::Incomplete);
        assert_eq!(front(&[1]), Front::Incomplete);
        assert_eq!(front(&[1, 3]), Front::Incomplete);
    }

    #[test]
    fn the_answer_is_taken_and_what_follows_stays() {
        let (_, mut waiting) = request(1, holding(0, 1));
        let mut buffer = registers(1, &[0x1234]);
        buffer.extend([1, 3]);
        let mut mismatches = 0;
        assert_eq!(
            waiting.scan(&mut buffer, &mut mismatches),
            Scan::Answer(Ok(Data::Registers(vec![0x1234])))
        );
        assert_eq!(buffer, [1, 3]);
        assert_eq!(mismatches, 0);
    }

    #[test]
    fn frames_of_other_units_are_counted_and_skipped() {
        let (_, mut waiting) = request(1, holding(0, 1));
        let mut buffer = registers(9, &[999]);
        buffer.extend(frame(2, &[0x83, 2]));
        buffer.extend(registers(1, &[7]));
        let mut mismatches = 0;
        assert_eq!(
            waiting.scan(&mut buffer, &mut mismatches),
            Scan::Answer(Ok(Data::Registers(vec![7])))
        );
        assert_eq!(mismatches, 2);
        assert!(buffer.is_empty());
    }

    #[test]
    fn a_frame_of_the_unit_that_does_not_fit_is_skipped_and_remembered() {
        let (_, mut waiting) = request(1, holding(0, 2));
        // One register for a read of two, then another master's write: neither is the answer.
        let mut buffer = registers(1, &[111]);
        buffer.extend(frame(1, &[6, 0, 1, 0, 2]));
        let mut mismatches = 0;
        assert_eq!(waiting.scan(&mut buffer, &mut mismatches), Scan::More);
        assert!(buffer.is_empty());
        assert!(waiting.unfit.is_some());
        buffer.extend(registers(1, &[1, 2]));
        assert_eq!(
            waiting.scan(&mut buffer, &mut mismatches),
            Scan::Answer(Ok(Data::Registers(vec![1, 2])))
        );
        assert_eq!(mismatches, 0, "the same unit's frames are not mismatches");
    }

    #[test]
    fn an_echo_of_the_request_is_skipped() {
        let (sent, mut waiting) = request(1, holding(0, 1));
        let mut mismatches = 0;
        // The first part of the echo waits for the rest instead of being skipped as noise.
        let mut buffer = sent[..3].to_vec();
        assert_eq!(waiting.scan(&mut buffer, &mut mismatches), Scan::More);
        assert_eq!(buffer, sent[..3]);
        buffer.extend(&sent[3..]);
        assert_eq!(waiting.scan(&mut buffer, &mut mismatches), Scan::More);
        assert!(buffer.is_empty());
        buffer.extend(registers(1, &[5]));
        assert_eq!(
            waiting.scan(&mut buffer, &mut mismatches),
            Scan::Answer(Ok(Data::Registers(vec![5])))
        );
        assert_eq!(waiting.noise, 0);
    }

    #[test]
    fn noise_before_the_answer_is_skipped() {
        let (_, mut waiting) = request(1, holding(0, 1));
        let mut corrupted = registers(1, &[111]);
        if let Some(last) = corrupted.last_mut() {
            *last ^= 0xFF;
        }
        let mut buffer = vec![0x00, 0x55, 0xFF];
        buffer.extend(&corrupted);
        buffer.extend(registers(1, &[222]));
        let mut mismatches = 0;
        assert_eq!(
            waiting.scan(&mut buffer, &mut mismatches),
            Scan::Answer(Ok(Data::Registers(vec![222])))
        );
        assert_eq!(waiting.noise, 3 + corrupted.len());
    }

    #[test]
    fn noise_that_reads_as_the_start_of_a_long_frame_does_not_hide_the_answer() {
        let (_, mut waiting) = request(1, holding(0, 1));
        // Unit 1, function 3, 240 data bytes: a frame of 245 bytes, if it were one.
        let mut buffer = vec![0x01, 0x03, 0xF0];
        let mut mismatches = 0;
        assert_eq!(waiting.scan(&mut buffer, &mut mismatches), Scan::More);
        buffer.extend(registers(1, &[0xBEEF]));
        assert_eq!(
            waiting.scan(&mut buffer, &mut mismatches),
            Scan::Answer(Ok(Data::Registers(vec![0xBEEF])))
        );
        assert!(buffer.is_empty());
        assert_eq!(waiting.noise, 3);

        // A frame of another unit or one that does not fit does not end the wait that way.
        let (_, mut waiting) = request(1, holding(0, 1));
        let mut buffer = vec![0x01, 0x03, 0xF0];
        buffer.extend(registers(2, &[1]));
        buffer.extend(registers(1, &[1, 2]));
        assert_eq!(waiting.scan(&mut buffer, &mut mismatches), Scan::More);
        assert_eq!(buffer.len(), 3 + 7 + 9, "it waits for the long frame");
    }

    #[test]
    fn an_answer_arriving_byte_by_byte_is_reassembled() {
        let (_, mut waiting) = request(3, holding(0, 2));
        let answer = registers(3, &[0x0102, 0x0304]);
        let mut buffer = Vec::new();
        let mut mismatches = 0;
        let (last, first) = answer.split_last().unwrap();
        for byte in first {
            buffer.push(*byte);
            assert_eq!(waiting.scan(&mut buffer, &mut mismatches), Scan::More);
        }
        buffer.push(*last);
        assert_eq!(
            waiting.scan(&mut buffer, &mut mismatches),
            Scan::Answer(Ok(Data::Registers(vec![0x0102, 0x0304])))
        );
        assert_eq!(waiting.noise, 0);
    }

    #[test]
    fn an_exception_is_the_answer() {
        let (_, mut waiting) = request(1, holding(0, 1));
        let mut buffer = frame(1, &[0x84, 2]); // an exception to another function
        buffer.extend(frame(1, &[0x83, 2]));
        let mut mismatches = 0;
        assert_eq!(
            waiting.scan(&mut buffer, &mut mismatches),
            Scan::Answer(Err(ReadError::Exception(2)))
        );
    }

    #[test]
    fn a_request_without_an_answer_says_what_arrived_instead() {
        let mut mismatches = 0;
        let (_, mut silent) = request(1, holding(0, 1));
        let mut buffer = registers(2, &[1]);
        assert_eq!(silent.scan(&mut buffer, &mut mismatches), Scan::More);
        assert_eq!(silent.timed_out(), ReadError::Timeout);

        let (_, mut unfit) = request(1, holding(0, 1));
        let mut buffer = registers(1, &[1, 2]);
        assert_eq!(unfit.scan(&mut buffer, &mut mismatches), Scan::More);
        assert!(
            matches!(&unfit.timed_out(), ReadError::Malformed(detail)
                if detail.contains("unit 1") && detail.contains("4 data bytes for 1 registers")),
            "{:?}",
            unfit.timed_out()
        );

        let (_, mut noisy) = request(1, holding(0, 1));
        let mut buffer = vec![0x00, 0x55, 0x7F, 0x13];
        assert_eq!(noisy.scan(&mut buffer, &mut mismatches), Scan::More);
        assert_eq!(buffer, [0x13], "a single byte may still start a frame");
        assert!(
            matches!(&noisy.timed_out(), ReadError::Malformed(detail)
                if detail.starts_with("3 bytes") && detail.contains("serial settings")),
            "{:?}",
            noisy.timed_out()
        );
        assert_eq!(mismatches, 1);
    }

    #[test]
    fn frames_between_requests_are_dropped_and_a_cut_frame_stays() {
        let mut buffer = registers(1, &[7]);
        buffer.extend([0x00, 0x55]); // noise
        buffer.extend(registers(9, &[8]));
        buffer.extend(frame(4, &[0x83, 2]));
        let cut = registers(5, &[9]);
        buffer.extend(&cut[..4]);
        assert_eq!(drop_frames(&mut buffer, 1), 2);
        assert_eq!(buffer, cut[..4], "the cut frame waits for its rest");
        buffer.extend(&cut[4..]);
        assert_eq!(drop_frames(&mut buffer, 1), 1);
        assert!(buffer.is_empty());
    }
}
