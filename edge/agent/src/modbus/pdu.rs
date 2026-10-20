//! The protocol data units of the four read functions: the request the gateway sends and the
//! response it accepts (Modbus Application Protocol Specification V1.1b3, sections 6.1–6.4 and
//! 7). The framing around them (MBAP for Modbus TCP) lives in [`super::tcp`].

use std::fmt;

use protocol::config::FunctionCode;

/// Most holding or input registers one read returns (section 6.3).
pub const MAX_REGISTERS: u16 = 125;
/// Most coils or discrete inputs one read returns (section 6.1).
pub const MAX_BITS: u16 = 2000;

/// One read request: a function and a range of addresses. Only the read function codes 1–4
/// exist in [`FunctionCode`], so no other request can be built (the gateway never writes to a
/// device), and [`Read::new`] refuses a range the function cannot return in one response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Read {
    function: FunctionCode,
    start: u16,
    count: u16,
}

/// Why [`Read::new`] refused a range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeError {
    /// The count is 0 or more than one response holds ([`MAX_REGISTERS`], [`MAX_BITS`]).
    Count(u16),
    /// The range runs past address 65535.
    PastEnd {
        /// The first address.
        start: u16,
        /// The count.
        count: u16,
    },
}

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Count(count) => write!(
                f,
                "{count} is outside 1..={MAX_REGISTERS} registers or 1..={MAX_BITS} bits"
            ),
            Self::PastEnd { start, count } => {
                write!(f, "{count} addresses from {start} run past 65535")
            }
        }
    }
}

impl std::error::Error for RangeError {}

impl Read {
    /// A read of `count` coils, inputs or registers from `start`.
    pub fn new(function: FunctionCode, start: u16, count: u16) -> Result<Self, RangeError> {
        if count == 0 || count > max_count(function) {
            return Err(RangeError::Count(count));
        }
        if u32::from(start) + u32::from(count) - 1 > u32::from(u16::MAX) {
            return Err(RangeError::PastEnd { start, count });
        }
        Ok(Self {
            function,
            start,
            count,
        })
    }

    /// The function.
    pub const fn function(&self) -> FunctionCode {
        self.function
    }

    /// The first address.
    pub const fn start(&self) -> u16 {
        self.start
    }

    /// How many coils, inputs or registers.
    pub const fn count(&self) -> u16 {
        self.count
    }

    /// The request PDU: function code, start address, count (section 6.1–6.4).
    pub fn request(&self) -> [u8; 5] {
        let [start_high, start_low] = self.start.to_be_bytes();
        let [count_high, count_low] = self.count.to_be_bytes();
        [
            self.function.number(),
            start_high,
            start_low,
            count_high,
            count_low,
        ]
    }

    /// The number of data bytes a normal response carries.
    fn data_len(&self) -> usize {
        let count = usize::from(self.count);
        if self.function.reads_bits() {
            count.div_ceil(8)
        } else {
            count * 2
        }
    }

    /// Reads the response PDU to this request: the values, or the device's exception.
    pub fn response(&self, pdu: &[u8]) -> Result<Data, ResponseError> {
        let function = self.function.number();
        match pdu {
            [code, exception] if *code == function | 0x80 => {
                Err(ResponseError::Exception(*exception))
            }
            [code, byte_count, data @ ..] if *code == function => {
                if usize::from(*byte_count) != data.len() {
                    return Err(ResponseError::Malformed(format!(
                        "byte count {byte_count} but {} data bytes",
                        data.len()
                    )));
                }
                if data.len() != self.data_len() {
                    return Err(ResponseError::Malformed(format!(
                        "{} data bytes for {} {}",
                        data.len(),
                        self.count,
                        if self.function.reads_bits() {
                            "bits"
                        } else {
                            "registers"
                        }
                    )));
                }
                Ok(if self.function.reads_bits() {
                    // Bit i of the range is bit i % 8 of byte i / 8, least significant first;
                    // the padding bits of the last byte are ignored.
                    Data::Bits(
                        (0..usize::from(self.count))
                            .map(|bit| {
                                data.get(bit / 8)
                                    .is_some_and(|byte| byte >> (bit % 8) & 1 == 1)
                            })
                            .collect(),
                    )
                } else {
                    Data::Registers(
                        // The length is checked above: 2 bytes per register, none left over.
                        data.as_chunks::<2>()
                            .0
                            .iter()
                            .map(|pair| u16::from_be_bytes(*pair))
                            .collect(),
                    )
                })
            }
            [code, ..] => Err(ResponseError::Malformed(format!(
                "function code {code} in the response to function code {function}"
            ))),
            [] => Err(ResponseError::Malformed("empty response".to_owned())),
        }
    }
}

/// The largest count one read of `function` may ask for.
pub const fn max_count(function: FunctionCode) -> u16 {
    if function.reads_bits() {
        MAX_BITS
    } else {
        MAX_REGISTERS
    }
}

/// The values of a normal response, one per address of the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Data {
    /// Coils or discrete inputs (function codes 1 and 2).
    Bits(Vec<bool>),
    /// Holding or input registers (function codes 3 and 4).
    Registers(Vec<u16>),
}

/// A response that carries no values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResponseError {
    /// The device answered with this Modbus exception code.
    Exception(u8),
    /// The response does not fit the request: another function, or the wrong number of bytes.
    Malformed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [FunctionCode; 4] = [
        FunctionCode::ReadCoils,
        FunctionCode::ReadDiscreteInputs,
        FunctionCode::ReadHoldingRegisters,
        FunctionCode::ReadInputRegisters,
    ];

    #[test]
    fn requests_carry_only_read_function_codes() {
        for function in ALL {
            let read = Read::new(function, 0x1234, 1).unwrap();
            let pdu = read.request();
            assert!((1..=4).contains(&pdu[0]), "{pdu:?}");
            assert_eq!(pdu, [function.number(), 0x12, 0x34, 0x00, 0x01]);
        }
    }

    #[test]
    fn counts_are_bounded_by_one_response() {
        let registers = FunctionCode::ReadHoldingRegisters;
        let bits = FunctionCode::ReadCoils;
        assert_eq!(Read::new(registers, 0, 0), Err(RangeError::Count(0)));
        assert!(Read::new(registers, 0, 125).is_ok());
        assert_eq!(Read::new(registers, 0, 126), Err(RangeError::Count(126)));
        assert!(Read::new(bits, 0, 2000).is_ok());
        assert_eq!(Read::new(bits, 0, 2001), Err(RangeError::Count(2001)));
        assert!(Read::new(FunctionCode::ReadInputRegisters, 65_411, 125).is_ok());
        assert_eq!(
            Read::new(FunctionCode::ReadInputRegisters, 65_412, 125),
            Err(RangeError::PastEnd {
                start: 65_412,
                count: 125
            })
        );
        assert!(Read::new(FunctionCode::ReadDiscreteInputs, 65_535, 1).is_ok());
    }

    #[test]
    fn register_responses_are_big_endian() {
        let read = Read::new(FunctionCode::ReadInputRegisters, 0, 2).unwrap();
        assert_eq!(
            read.response(&[4, 4, 0x12, 0x34, 0xAB, 0xCD]),
            Ok(Data::Registers(vec![0x1234, 0xABCD]))
        );
    }

    #[test]
    fn bit_responses_are_least_significant_first() {
        // Spec example (section 6.1): coils 20-38 = CD 6B 05.
        let read = Read::new(FunctionCode::ReadCoils, 19, 19).unwrap();
        let Ok(Data::Bits(bits)) = read.response(&[1, 3, 0xCD, 0x6B, 0x05]) else {
            panic!("expected bits");
        };
        let expected: Vec<bool> = [
            1, 0, 1, 1, 0, 0, 1, 1, // 0xCD
            1, 1, 0, 1, 0, 1, 1, 0, // 0x6B
            1, 0, 1, // 0x05, padding ignored
        ]
        .iter()
        .map(|bit| *bit == 1)
        .collect();
        assert_eq!(bits, expected);
        // Padding bits that are set do not matter.
        let read = Read::new(FunctionCode::ReadDiscreteInputs, 0, 1).unwrap();
        assert_eq!(read.response(&[2, 1, 0xFE]), Ok(Data::Bits(vec![false])));
    }

    #[test]
    fn exceptions_are_reported() {
        let read = Read::new(FunctionCode::ReadHoldingRegisters, 0, 1).unwrap();
        assert_eq!(
            read.response(&[0x83, 0x02]),
            Err(ResponseError::Exception(2))
        );
        // An exception for another function is not this request's answer.
        assert!(matches!(
            read.response(&[0x84, 0x02]),
            Err(ResponseError::Malformed(_))
        ));
    }

    #[test]
    fn responses_that_do_not_fit_are_malformed() {
        let read = Read::new(FunctionCode::ReadHoldingRegisters, 0, 2).unwrap();
        for pdu in [
            &[][..],
            &[3],
            &[3, 4, 0, 1, 0],          // byte count says 4, 3 bytes follow
            &[3, 2, 0, 1],             // one register for two
            &[3, 6, 0, 1, 0, 2, 0, 3], // three registers for two
            &[4, 4, 0, 1, 0, 2],       // another function
            &[0x83, 2, 0],             // an exception with extra bytes
        ] {
            assert!(
                matches!(read.response(pdu), Err(ResponseError::Malformed(_))),
                "{pdu:?}"
            );
        }
        let bits = Read::new(FunctionCode::ReadCoils, 0, 9).unwrap();
        assert!(matches!(
            bits.response(&[1, 1, 0xFF]),
            Err(ResponseError::Malformed(_))
        ));
        assert!(bits.response(&[1, 2, 0xFF, 0x01]).is_ok());
    }
}
