//! Byte-level framing and the device model of the fake Modbus endpoint: MBAP and RTU frames, the
//! RTU CRC, and the answers of a unit to function codes 1–4.

use std::collections::BTreeMap;

/// Exception 01: the function code is not supported.
pub(super) const ILLEGAL_FUNCTION: u8 = 0x01;
/// Exception 02: an address in the requested range does not exist.
pub(super) const ILLEGAL_DATA_ADDRESS: u8 = 0x02;
/// Exception 03: the quantity is out of range or the request is malformed.
pub(super) const ILLEGAL_DATA_VALUE: u8 = 0x03;
/// Exception 0B: a Modbus TCP gateway got no answer from the addressed unit.
pub(super) const GATEWAY_TARGET_FAILED: u8 = 0x0B;

/// Most coils or discrete inputs one read may ask for (Modbus application protocol 1.1b3).
pub const MAX_READ_BITS: u16 = 2000;
/// Most holding or input registers one read may ask for.
pub const MAX_READ_REGISTERS: u16 = 125;

/// Largest MBAP length field: unit identifier plus a PDU of at most 253 bytes.
const MAX_MBAP_LENGTH: usize = 254;
/// MBAP header: transaction id, protocol id, length, unit id.
const MBAP_HEADER: usize = 7;
/// Largest RTU frame (Modbus over serial line 1.02): unit, a PDU of at most 253 bytes, CRC.
const MAX_RTU_FRAME: usize = 256;

/// The four tables of one unit. An address exists once the test has set it; reading a range
/// with an address that does not exist is exception 02, as on a device with gaps in its map.
#[derive(Debug, Default)]
pub(super) struct Tables {
    pub(super) coils: BTreeMap<u16, bool>,
    pub(super) discrete_inputs: BTreeMap<u16, bool>,
    pub(super) holding_registers: BTreeMap<u16, u16>,
    pub(super) input_registers: BTreeMap<u16, u16>,
}

impl Tables {
    /// The response PDU to a request PDU: the data for function codes 1–4, otherwise an
    /// exception PDU.
    pub(super) fn answer(&self, pdu: &[u8]) -> Vec<u8> {
        let Some(&function) = pdu.first() else {
            return exception(0, ILLEGAL_FUNCTION);
        };
        if !(1..=4).contains(&function) {
            return exception(function, ILLEGAL_FUNCTION);
        }
        let Some((address, quantity)) = read_range(pdu) else {
            return exception(function, ILLEGAL_DATA_VALUE);
        };
        let limit = if function <= 2 {
            MAX_READ_BITS
        } else {
            MAX_READ_REGISTERS
        };
        // The order of the checks follows the specification: quantity (03), then the address
        // range (02).
        if quantity == 0 || quantity > limit {
            return exception(function, ILLEGAL_DATA_VALUE);
        }
        if u32::from(address) + u32::from(quantity) > 0x1_0000 {
            return exception(function, ILLEGAL_DATA_ADDRESS);
        }
        let addresses = (0..quantity).map(|offset| address + offset);
        let mut response = vec![function];
        match function {
            1 | 2 => {
                let table = if function == 1 {
                    &self.coils
                } else {
                    &self.discrete_inputs
                };
                let Some(bits) = addresses
                    .map(|address| table.get(&address).copied())
                    .collect::<Option<Vec<bool>>>()
                else {
                    return exception(function, ILLEGAL_DATA_ADDRESS);
                };
                let packed = pack_bits(&bits);
                response.push(byte_count(packed.len()));
                response.extend(packed);
            }
            _ => {
                let table = if function == 3 {
                    &self.holding_registers
                } else {
                    &self.input_registers
                };
                let Some(registers) = addresses
                    .map(|address| table.get(&address).copied())
                    .collect::<Option<Vec<u16>>>()
                else {
                    return exception(function, ILLEGAL_DATA_ADDRESS);
                };
                response.push(byte_count(registers.len() * 2));
                response.extend(registers.iter().flat_map(|value| value.to_be_bytes()));
            }
        }
        response
    }
}

/// The byte count of a read response, at most 250 for the quantities [`Tables::answer`] allows.
fn byte_count(bytes: usize) -> u8 {
    u8::try_from(bytes).unwrap_or(u8::MAX)
}

/// Bits packed into bytes, the first bit in the least significant bit of the first byte.
fn pack_bits(bits: &[bool]) -> Vec<u8> {
    bits.chunks(8)
        .map(|chunk| {
            chunk
                .iter()
                .enumerate()
                .filter(|(_, bit)| **bit)
                .fold(0u8, |byte, (index, _)| byte | (1 << index))
        })
        .collect()
}

/// An exception response PDU.
pub(super) fn exception(function: u8, code: u8) -> Vec<u8> {
    vec![function | 0x80, code]
}

/// The start address and quantity of a function code 1–4 request PDU, which is exactly five
/// bytes long.
pub(super) fn read_range(pdu: &[u8]) -> Option<(u16, u16)> {
    match pdu {
        [function, a, b, c, d] if (1..=4).contains(function) => {
            Some((u16::from_be_bytes([*a, *b]), u16::from_be_bytes([*c, *d])))
        }
        _ => None,
    }
}

/// One frame cut from the bytes a connection received.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Frame {
    /// The whole frame as received.
    pub(super) raw: Vec<u8>,
    /// Modbus TCP only.
    pub(super) transaction_id: Option<u16>,
    pub(super) unit: u8,
    /// Function code and data, without the unit identifier and the CRC.
    pub(super) pdu: Vec<u8>,
    /// Always true for Modbus TCP, which has no checksum.
    pub(super) checksum_ok: bool,
}

/// What the bytes received so far hold.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Cut {
    /// A complete frame, removed from the buffer.
    Frame(Frame),
    /// Not enough bytes yet.
    Incomplete,
    /// An MBAP header that no Modbus TCP client sends: a protocol identifier other than 0, or a
    /// length outside 2..=254. The endpoint closes the connection.
    Invalid,
}

/// Cuts one Modbus TCP frame (MBAP header and PDU) from the front of `buffer`.
pub(super) fn cut_mbap(buffer: &mut Vec<u8>) -> Cut {
    let Some(header) = buffer.get(..MBAP_HEADER) else {
        return Cut::Incomplete;
    };
    let transaction_id = u16::from_be_bytes([header[0], header[1]]);
    let protocol_id = u16::from_be_bytes([header[2], header[3]]);
    let length = usize::from(u16::from_be_bytes([header[4], header[5]]));
    let unit = header[6];
    if protocol_id != 0 || !(2..=MAX_MBAP_LENGTH).contains(&length) {
        return Cut::Invalid;
    }
    let total = 6 + length;
    if buffer.len() < total {
        return Cut::Incomplete;
    }
    let raw: Vec<u8> = buffer.drain(..total).collect();
    let pdu = raw[MBAP_HEADER..].to_vec();
    Cut::Frame(Frame {
        raw,
        transaction_id: Some(transaction_id),
        unit,
        pdu,
        checksum_ok: true,
    })
}

/// Cuts one RTU frame from the front of `buffer`.
///
/// RTU has no length field; on a serial line a frame ends with 3.5 characters of silence, which
/// TCP does not carry, so a request can arrive split over several reads or together with the
/// next one. The endpoint takes the length from the request format of the function code (see
/// `rtu_request_length`). A request without a fixed format ends with the shortest prefix whose
/// last two bytes are the CRC of the bytes before them; bytes that never form one are cut as a
/// frame with a bad CRC once they reach the RTU maximum of 256 bytes.
pub(super) fn cut_rtu(buffer: &mut Vec<u8>) -> Cut {
    let length = match rtu_request_length(buffer) {
        RtuLength::Known(length) => length,
        RtuLength::Pending => return Cut::Incomplete,
        RtuLength::Unknown => match crc_boundary(buffer) {
            Some(length) => length,
            None if buffer.len() >= MAX_RTU_FRAME => MAX_RTU_FRAME,
            None => return Cut::Incomplete,
        },
    };
    if buffer.len() < length {
        return Cut::Incomplete;
    }
    let raw: Vec<u8> = buffer.drain(..length).collect();
    let (body, crc) = raw.split_at(length - 2);
    let checksum_ok = crc16(body).to_le_bytes() == [crc[0], crc[1]];
    let unit = body[0];
    let pdu = body[1..].to_vec();
    Cut::Frame(Frame {
        raw,
        transaction_id: None,
        unit,
        pdu,
        checksum_ok,
    })
}

/// How the length of an RTU request follows from its first bytes.
enum RtuLength {
    /// The request is this many bytes, unit and CRC included.
    Known(usize),
    /// The function code has no fixed request format.
    Unknown,
    /// The bytes that decide the length have not arrived.
    Pending,
}

/// The length of the RTU request at the front of `buffer`, from the request format of each
/// public function code (application protocol 1.1b3, section 6): the unit, the function code,
/// the fields of the request, and the CRC.
fn rtu_request_length(buffer: &[u8]) -> RtuLength {
    let byte_count = |index: usize, fixed: usize| match buffer.get(index) {
        Some(&count) => RtuLength::Known(fixed + usize::from(count)),
        None => RtuLength::Pending,
    };
    let Some(&function) = buffer.get(1) else {
        return RtuLength::Pending;
    };
    match function {
        // Reads, and writes of a single coil or register: address, then quantity or value.
        1..=6 => RtuLength::Known(8),
        // Read Exception Status, Get Comm Event Counter, Get Comm Event Log, Report Server ID:
        // the function code alone.
        7 | 11 | 12 | 17 => RtuLength::Known(4),
        // Diagnostics: a sub-function and two data bytes, except Return Query Data (0), which
        // echoes any number of bytes.
        8 => match buffer.get(2..4) {
            Some([0, 0]) => RtuLength::Unknown,
            Some(_) => RtuLength::Known(8),
            None => RtuLength::Pending,
        },
        // Write Multiple Coils or Registers: address, quantity, byte count, values.
        15 | 16 => byte_count(6, 9),
        // Read or Write File Record: byte count, sub-requests.
        20 | 21 => byte_count(2, 5),
        // Mask Write Register: address, AND mask, OR mask.
        22 => RtuLength::Known(10),
        // Read/Write Multiple Registers: read address and quantity, write address and quantity,
        // byte count, values.
        23 => byte_count(10, 13),
        // Read FIFO Queue: the FIFO pointer address.
        24 => RtuLength::Known(6),
        // Read Device Identification (MEI type 14): read code and object id. Other MEI types
        // carry data of their own format.
        43 => match buffer.get(2) {
            Some(14) => RtuLength::Known(7),
            Some(_) => RtuLength::Unknown,
            None => RtuLength::Pending,
        },
        _ => RtuLength::Unknown,
    }
}

/// The length of the shortest prefix of `buffer`, from a unit, a function code and a CRC up to
/// `MAX_RTU_FRAME` bytes, whose last two bytes are the CRC of the bytes before them.
fn crc_boundary(buffer: &[u8]) -> Option<usize> {
    (4..=buffer.len().min(MAX_RTU_FRAME)).find(|&length| {
        let (body, crc) = buffer[..length].split_at(length - 2);
        crc16(body).to_le_bytes() == [crc[0], crc[1]]
    })
}

/// A Modbus TCP response frame.
pub(super) fn mbap_response(transaction_id: u16, unit: u8, pdu: &[u8]) -> Vec<u8> {
    let length = u16::try_from(pdu.len() + 1).unwrap_or(u16::MAX);
    let mut frame = Vec::with_capacity(MBAP_HEADER + pdu.len());
    frame.extend(transaction_id.to_be_bytes());
    frame.extend(0u16.to_be_bytes());
    frame.extend(length.to_be_bytes());
    frame.push(unit);
    frame.extend(pdu);
    frame
}

/// An RTU response frame with its CRC.
pub(super) fn rtu_response(unit: u8, pdu: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(pdu.len() + 3);
    frame.push(unit);
    frame.extend(pdu);
    let crc = crc16(&frame);
    frame.extend(crc.to_le_bytes());
    frame
}

/// The Modbus RTU CRC-16 (polynomial 0xA001 reflected, initial value 0xFFFF); the frame carries
/// it low byte first.
pub fn crc16(bytes: &[u8]) -> u16 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_specification_example() {
        // Modbus over serial line 1.02, read 10 holding registers of unit 1 from address 0.
        assert_eq!(
            crc16(&[0x01, 0x03, 0x00, 0x00, 0x00, 0x0A]).to_le_bytes(),
            [0xC5, 0xCD]
        );
    }

    #[test]
    fn bits_are_packed_lsb_first() {
        let bits = [
            true, false, true, true, false, false, true, true, true, false,
        ];
        assert_eq!(pack_bits(&bits), vec![0b1100_1101, 0b0000_0001]);
    }

    #[test]
    fn reads_check_quantity_before_address() {
        let tables = Tables::default();
        assert_eq!(tables.answer(&[3, 0xFF, 0xFF, 0, 126]), vec![0x83, 3]);
        assert_eq!(tables.answer(&[3, 0xFF, 0xFF, 0, 2]), vec![0x83, 2]);
        assert_eq!(tables.answer(&[1, 0, 0, 0x07, 0xD1]), vec![0x81, 3]);
        assert_eq!(tables.answer(&[2, 0, 0, 0, 0]), vec![0x82, 3]);
        assert_eq!(tables.answer(&[4, 0, 0, 0, 1, 9]), vec![0x84, 3]);
        assert_eq!(tables.answer(&[6, 0, 0, 0, 1]), vec![0x86, 1]);
    }

    #[test]
    fn a_split_mbap_frame_is_cut_once_complete() {
        let mut buffer = vec![0, 1, 0, 0, 0, 6, 9, 3, 0];
        assert_eq!(cut_mbap(&mut buffer), Cut::Incomplete);
        buffer.extend([0, 0, 2, 0xAA]);
        let Cut::Frame(frame) = cut_mbap(&mut buffer) else {
            panic!("expected a frame");
        };
        assert_eq!(frame.transaction_id, Some(1));
        assert_eq!(frame.unit, 9);
        assert_eq!(frame.pdu, vec![3, 0, 0, 0, 2]);
        assert_eq!(buffer, vec![0xAA]);
        assert_eq!(
            cut_mbap(&mut vec![0, 1, 0, 1, 0, 6, 9]),
            Cut::Invalid,
            "protocol id 1"
        );
        assert_eq!(
            cut_mbap(&mut vec![0, 1, 0, 0, 0, 1, 9]),
            Cut::Invalid,
            "length 1"
        );
    }

    #[test]
    fn rtu_frames_are_cut_by_function_code() {
        let mut buffer = vec![0x01, 0x03, 0x00, 0x00, 0x00, 0x0A, 0xC5];
        assert_eq!(cut_rtu(&mut buffer), Cut::Incomplete);
        buffer.extend([0xCD, 0x02]);
        let Cut::Frame(frame) = cut_rtu(&mut buffer) else {
            panic!("expected a frame");
        };
        assert!(frame.checksum_ok);
        assert_eq!(frame.unit, 1);
        assert_eq!(buffer, vec![0x02]);

        let mut write_multiple = rtu_response(1, &[16, 0, 0, 0, 1, 2, 0, 7]);
        let length = write_multiple.len();
        let Cut::Frame(frame) = cut_rtu(&mut write_multiple) else {
            panic!("expected a frame");
        };
        assert_eq!(frame.raw.len(), length);
        assert!(frame.checksum_ok);

        let mut corrupted = vec![0x01, 0x03, 0x00, 0x00, 0x00, 0x0A, 0xC5, 0xCE];
        let Cut::Frame(frame) = cut_rtu(&mut corrupted) else {
            panic!("expected a frame");
        };
        assert!(!frame.checksum_ok);
    }

    #[test]
    fn rtu_requests_are_cut_however_tcp_splits_or_joins_them() {
        let requests = [
            // Read Exception Status: the function code alone.
            rtu_response(1, &[7]),
            // Diagnostics, Return Diagnostic Register.
            rtu_response(1, &[8, 0, 2, 0, 0]),
            // Diagnostics, Return Query Data: only the CRC tells the length.
            rtu_response(1, &[8, 0, 0, 0x12, 0x34, 0x56, 0x78]),
            // Mask Write Register.
            rtu_response(1, &[22, 0, 4, 0, 0xF2, 0, 0x25]),
            // Read/Write Multiple Registers writing one register.
            rtu_response(1, &[23, 0, 3, 0, 6, 0, 14, 0, 1, 2, 0, 0xFF]),
            // Read FIFO Queue.
            rtu_response(1, &[24, 0x04, 0xDE]),
            // Read Device Identification.
            rtu_response(1, &[43, 14, 1, 0]),
            // A user-defined function code: only the CRC tells the length.
            rtu_response(1, &[65, 1, 2, 3]),
        ];
        let next = rtu_response(1, &[3, 0, 0, 0, 1]);
        for request in &requests {
            for split in 1..request.len() {
                let mut buffer = request[..split].to_vec();
                assert_eq!(
                    cut_rtu(&mut buffer),
                    Cut::Incomplete,
                    "{request:02X?} split after {split} bytes"
                );
                buffer.extend(&request[split..]);
                buffer.extend(&next);
                let Cut::Frame(frame) = cut_rtu(&mut buffer) else {
                    panic!("expected {request:02X?} split after {split} bytes");
                };
                assert_eq!(
                    &frame.raw, request,
                    "{request:02X?} split after {split} bytes"
                );
                assert!(frame.checksum_ok);
                assert_eq!(buffer, next, "the next request stays in the buffer");
            }
        }
    }

    #[test]
    fn rtu_bytes_that_never_end_with_a_valid_crc_are_cut_at_the_rtu_maximum() {
        // A user-defined function code, so only a CRC could end the frame.
        let mut buffer = vec![1, 65];
        while buffer.len() < MAX_RTU_FRAME {
            let length = buffer.len();
            assert_eq!(cut_rtu(&mut buffer), Cut::Incomplete, "{length} bytes");
            buffer.push(0x55);
        }
        buffer.push(0x55);
        let Cut::Frame(frame) = cut_rtu(&mut buffer) else {
            panic!("expected a frame at {MAX_RTU_FRAME} bytes");
        };
        assert_eq!(frame.raw.len(), MAX_RTU_FRAME);
        assert!(!frame.checksum_ok);
        assert_eq!(buffer, vec![0x55]);
    }
}
