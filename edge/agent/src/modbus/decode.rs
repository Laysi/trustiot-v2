//! Turning a data point's coils or registers into the value sent in telemetry: the type and
//! byte order of [`DataType`] and [`ByteOrder`] (rebuild checklist F1), then `scale` and
//! `offset` (rebuild checklist F5).

use std::fmt;

use protocol::config::{ByteOrder, DataPoint, DataType};
use protocol::{Value, ValueError};

/// A data point's share of a response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Raw<'a> {
    /// One coil or discrete input.
    Bit(bool),
    /// The point's registers, in the order they were read.
    Registers(&'a [u16]),
}

/// Why a data point has no value that can be sent; the agent counts it as
/// [`protocol::health::FailureKind::InvalidValue`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InvalidValue {
    /// The response did not hold as many registers as the type needs, or held registers for a
    /// coil. [`super::plan`] cuts responses so that this does not happen.
    Width {
        /// Registers the type needs.
        expected: usize,
        /// Registers given.
        got: usize,
    },
    /// A `u64` above `i64::MAX` without scaling: an integer value is sent as `i64`.
    IntegerOutOfRange(u64),
    /// The float is NaN or infinite, as read or after scaling.
    NotFinite(f64),
    /// A string that cannot be sent.
    Text(ValueError),
}

impl fmt::Display for InvalidValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Width { expected, got } => {
                write!(f, "{got} registers for a value of {expected}")
            }
            Self::IntegerOutOfRange(value) => {
                write!(f, "u64 value {value} is above the largest sendable integer")
            }
            Self::NotFinite(value) => write!(f, "value {value} is not a finite number"),
            Self::Text(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for InvalidValue {}

/// Decodes one data point: the type and byte order, then `scale` and `offset` when either is
/// set. A scaled value is always [`Value::Float`]: `raw * scale + offset`, with a missing scale
/// taken as 1 and a missing offset as 0. Integers convert to `f64` first, so one above 2^53
/// loses its lowest bits; a `u64` above `i64::MAX` is sent only when scaled.
pub fn decode(point: &DataPoint, raw: Raw<'_>) -> Result<Value, InvalidValue> {
    let registers = match raw {
        // Bits carry no byte order, and validation refuses scaling a bool.
        Raw::Bit(bit) => return Ok(Value::Bool(bit)),
        Raw::Registers(registers) => registers,
    };
    let number = match point.data_type {
        DataType::Bool => {
            let [register] = fixed::<1>(registers)?;
            return Ok(Value::Bool(register != 0));
        }
        DataType::String => return text(registers, point.byte_order),
        DataType::U16 => Number::Unsigned(u64::from(u16::from_be_bytes(bytes(
            registers,
            point.byte_order,
        )?))),
        DataType::I16 => Number::Signed(i64::from(i16::from_be_bytes(bytes(
            registers,
            point.byte_order,
        )?))),
        DataType::U32 => Number::Unsigned(u64::from(u32::from_be_bytes(bytes(
            registers,
            point.byte_order,
        )?))),
        DataType::I32 => Number::Signed(i64::from(i32::from_be_bytes(bytes(
            registers,
            point.byte_order,
        )?))),
        DataType::U64 => Number::Unsigned(u64::from_be_bytes(bytes(registers, point.byte_order)?)),
        DataType::I64 => Number::Signed(i64::from_be_bytes(bytes(registers, point.byte_order)?)),
        DataType::F32 => Number::Float(f64::from(f32::from_be_bytes(bytes(
            registers,
            point.byte_order,
        )?))),
        DataType::F64 => Number::Float(f64::from_be_bytes(bytes(registers, point.byte_order)?)),
    };
    if point.scale.is_none() && point.offset.is_none() {
        return match number {
            Number::Signed(value) => Ok(Value::Int(value)),
            Number::Unsigned(value) => i64::try_from(value)
                .map(Value::Int)
                .map_err(|_| InvalidValue::IntegerOutOfRange(value)),
            Number::Float(value) => finite(value),
        };
    }
    // A scaled value is a float by definition; the rounding above 2^53 is documented above.
    let raw = match number {
        Number::Signed(value) => value as f64,
        Number::Unsigned(value) => value as f64,
        Number::Float(value) => value,
    };
    finite(raw * point.scale.unwrap_or(1.0) + point.offset.unwrap_or(0.0))
}

/// A register value before scaling.
enum Number {
    Signed(i64),
    Unsigned(u64),
    Float(f64),
}

fn finite(value: f64) -> Result<Value, InvalidValue> {
    if value.is_finite() {
        Ok(Value::Float(value))
    } else {
        Err(InvalidValue::NotFinite(value))
    }
}

/// Exactly `N` registers.
fn fixed<const N: usize>(registers: &[u16]) -> Result<[u16; N], InvalidValue> {
    registers.try_into().map_err(|_| InvalidValue::Width {
        expected: N,
        got: registers.len(),
    })
}

/// The bytes of a value `N` bytes wide, most significant first: the registers in the order the
/// byte order says the value was stored. [`ByteOrder`] is two swaps: `cdab` and `dcba` reverse
/// the registers, `badc` and `dcba` swap the two bytes inside each.
fn bytes<const N: usize>(registers: &[u16], order: ByteOrder) -> Result<[u8; N], InvalidValue> {
    if registers.len() * 2 != N {
        return Err(InvalidValue::Width {
            expected: N / 2,
            got: registers.len(),
        });
    }
    let mut value = [0; N];
    for (index, target) in value.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let register = if order.swaps_words() {
            registers[registers.len() - 1 - index]
        } else {
            registers[index]
        };
        let [high, low] = register.to_be_bytes();
        *target = if order.swaps_bytes() {
            [low, high]
        } else {
            [high, low]
        };
    }
    Ok(value)
}

/// A string register value: two ASCII characters per register, first character in the high
/// byte (`abcd`) or the low byte (`badc`; the word swap does not apply to strings), ending at
/// the first NUL and trimmed. Bytes that are not UTF-8 become U+FFFD.
fn text(registers: &[u16], order: ByteOrder) -> Result<Value, InvalidValue> {
    let mut bytes = Vec::with_capacity(registers.len() * 2);
    for register in registers {
        let [high, low] = register.to_be_bytes();
        if order.swaps_bytes() {
            bytes.extend([low, high]);
        } else {
            bytes.extend([high, low]);
        }
    }
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let value = Value::Text(
        String::from_utf8_lossy(bytes.get(..end).unwrap_or_default())
            .trim()
            .to_owned(),
    );
    value.validate().map_err(InvalidValue::Text)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use protocol::PointKey;
    use protocol::config::FunctionCode;

    use super::*;

    const ORDERS: [ByteOrder; 4] = [
        ByteOrder::Abcd,
        ByteOrder::Badc,
        ByteOrder::Cdab,
        ByteOrder::Dcba,
    ];

    fn point(data_type: DataType, byte_order: ByteOrder) -> DataPoint {
        DataPoint {
            key: PointKey::new("value").unwrap(),
            function_code: FunctionCode::ReadHoldingRegisters,
            address: 0,
            data_type,
            byte_order,
            length: None,
            scale: None,
            offset: None,
        }
    }

    /// The registers a device stores `value` (most significant byte first) in, for each byte
    /// order, built straight from the definitions in [`ByteOrder`]'s documentation.
    fn stored(value: &[u8], order: ByteOrder) -> Vec<u16> {
        let mut registers: Vec<u16> = value
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| match order {
                ByteOrder::Abcd | ByteOrder::Cdab => u16::from_be_bytes([pair[0], pair[1]]),
                ByteOrder::Badc | ByteOrder::Dcba => u16::from_be_bytes([pair[1], pair[0]]),
            })
            .collect();
        if matches!(order, ByteOrder::Cdab | ByteOrder::Dcba) {
            registers.reverse();
        }
        registers
    }

    fn read(
        data_type: DataType,
        order: ByteOrder,
        registers: &[u16],
    ) -> Result<Value, InvalidValue> {
        decode(&point(data_type, order), Raw::Registers(registers))
    }

    #[test]
    fn the_documented_byte_orders_of_0xaabbccdd() {
        // ByteOrder's documentation: where the bytes of 0xAABBCCDD end up in the registers.
        for (order, registers) in [
            (ByteOrder::Abcd, [0xAABB, 0xCCDD]),
            (ByteOrder::Badc, [0xBBAA, 0xDDCC]),
            (ByteOrder::Cdab, [0xCCDD, 0xAABB]),
            (ByteOrder::Dcba, [0xDDCC, 0xBBAA]),
        ] {
            assert_eq!(
                stored(&0xAABB_CCDD_u32.to_be_bytes(), order),
                registers,
                "{order:?}"
            );
            assert_eq!(
                read(DataType::U32, order, &registers),
                Ok(Value::Int(0xAABB_CCDD)),
                "{order:?}"
            );
        }
        // A 64-bit value in cdab is GH EF CD AB.
        assert_eq!(
            read(
                DataType::U64,
                ByteOrder::Cdab,
                &[0x7788, 0x5566, 0x3344, 0x1122]
            ),
            Ok(Value::Int(0x1122_3344_5566_7788))
        );
    }

    #[test]
    fn sixteen_bit_types_use_only_the_byte_swap() {
        for order in ORDERS {
            let swapped = order.swaps_bytes();
            let register = if swapped { 0x39_30 } else { 0x30_39 }; // 12345
            assert_eq!(
                read(DataType::U16, order, &[register]),
                Ok(Value::Int(12_345))
            );
            let register = if swapped { 0xC7_CF } else { 0xCF_C7 }; // -12345
            assert_eq!(
                read(DataType::I16, order, &[register]),
                Ok(Value::Int(-12_345))
            );
            assert_eq!(
                read(DataType::U16, order, &[0xFFFF]),
                Ok(Value::Int(65_535))
            );
            assert_eq!(read(DataType::I16, order, &[0xFFFF]), Ok(Value::Int(-1)));
        }
    }

    #[test]
    fn every_multi_register_type_in_every_byte_order() {
        for order in ORDERS {
            let cases: [(DataType, Vec<u8>, Value); 9] = [
                (
                    DataType::U32,
                    4_000_000_000_u32.to_be_bytes().to_vec(),
                    Value::Int(4_000_000_000),
                ),
                (
                    DataType::U32,
                    0x0001_0002_u32.to_be_bytes().to_vec(),
                    Value::Int(65_538),
                ),
                (
                    DataType::I32,
                    (-123_456_789_i32).to_be_bytes().to_vec(),
                    Value::Int(-123_456_789),
                ),
                (
                    DataType::I32,
                    i32::MIN.to_be_bytes().to_vec(),
                    Value::Int(i64::from(i32::MIN)),
                ),
                (
                    DataType::U64,
                    0x0102_0304_0506_0708_u64.to_be_bytes().to_vec(),
                    Value::Int(0x0102_0304_0506_0708),
                ),
                (
                    DataType::I64,
                    (-9_876_543_210_123_i64).to_be_bytes().to_vec(),
                    Value::Int(-9_876_543_210_123),
                ),
                (
                    DataType::I64,
                    i64::MIN.to_be_bytes().to_vec(),
                    Value::Int(i64::MIN),
                ),
                (
                    DataType::F32,
                    230.5_f32.to_be_bytes().to_vec(),
                    Value::Float(230.5),
                ),
                (
                    DataType::F64,
                    (-0.000_123_456_789_f64).to_be_bytes().to_vec(),
                    Value::Float(-0.000_123_456_789),
                ),
            ];
            for (data_type, value, expected) in cases {
                assert_eq!(
                    read(data_type, order, &stored(&value, order)),
                    Ok(expected),
                    "{data_type:?} {order:?}"
                );
            }
            // f32 values keep their exact f32 meaning.
            let third = 1.0_f32 / 3.0;
            assert_eq!(
                read(DataType::F32, order, &stored(&third.to_be_bytes(), order)),
                Ok(Value::Float(f64::from(third)))
            );
        }
    }

    #[test]
    fn u64_above_i64_max_is_invalid_unless_scaled() {
        let registers = stored(&u64::MAX.to_be_bytes(), ByteOrder::Abcd);
        assert_eq!(
            read(DataType::U64, ByteOrder::Abcd, &registers),
            Err(InvalidValue::IntegerOutOfRange(u64::MAX))
        );
        let largest = stored(
            &u64::try_from(i64::MAX).unwrap().to_be_bytes(),
            ByteOrder::Abcd,
        );
        assert_eq!(
            read(DataType::U64, ByteOrder::Abcd, &largest),
            Ok(Value::Int(i64::MAX))
        );
        let mut scaled = point(DataType::U64, ByteOrder::Abcd);
        scaled.scale = Some(0.5);
        assert_eq!(
            decode(&scaled, Raw::Registers(&registers)),
            Ok(Value::Float(u64::MAX as f64 * 0.5))
        );
    }

    #[test]
    fn non_finite_floats_are_invalid() {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let registers = stored(&value.to_be_bytes(), ByteOrder::Abcd);
            assert!(matches!(
                read(DataType::F32, ByteOrder::Abcd, &registers),
                Err(InvalidValue::NotFinite(_))
            ));
        }
        let registers = stored(&f64::NAN.to_be_bytes(), ByteOrder::Dcba);
        assert!(matches!(
            read(DataType::F64, ByteOrder::Dcba, &registers),
            Err(InvalidValue::NotFinite(_))
        ));
    }

    #[test]
    fn scale_and_offset_give_floats() {
        // An MS300-style register: 2301 tenths of a volt.
        let mut voltage = point(DataType::U16, ByteOrder::Abcd);
        voltage.scale = Some(0.1);
        assert_eq!(
            decode(&voltage, Raw::Registers(&[2301])),
            Ok(Value::Float(2301.0 * 0.1))
        );

        let mut temperature = point(DataType::I16, ByteOrder::Abcd);
        temperature.scale = Some(0.01);
        temperature.offset = Some(-40.0);
        assert_eq!(
            decode(&temperature, Raw::Registers(&[0xFFFF])),
            Ok(Value::Float(-0.01 - 40.0))
        );

        // Offset alone still makes a float.
        let mut counter = point(DataType::U32, ByteOrder::Cdab);
        counter.offset = Some(1000.0);
        assert_eq!(
            decode(&counter, Raw::Registers(&[0x0005, 0x0000])),
            Ok(Value::Float(1005.0))
        );

        // A float register is scaled too, and so is a negative scale.
        let mut power = point(DataType::F32, ByteOrder::Abcd);
        power.scale = Some(-1000.0);
        let registers = stored(&1.5_f32.to_be_bytes(), ByteOrder::Abcd);
        assert_eq!(
            decode(&power, Raw::Registers(&registers)),
            Ok(Value::Float(-1500.0))
        );

        // Scaling that overflows is a read failure, not an infinite value.
        let mut huge = point(DataType::F64, ByteOrder::Abcd);
        huge.scale = Some(1e300);
        let registers = stored(&1e300_f64.to_be_bytes(), ByteOrder::Abcd);
        assert_eq!(
            decode(&huge, Raw::Registers(&registers)),
            Err(InvalidValue::NotFinite(f64::INFINITY))
        );
    }

    #[test]
    fn booleans_from_bits_and_registers() {
        let coil = DataPoint {
            function_code: FunctionCode::ReadCoils,
            ..point(DataType::Bool, ByteOrder::Abcd)
        };
        assert_eq!(decode(&coil, Raw::Bit(true)), Ok(Value::Bool(true)));
        assert_eq!(decode(&coil, Raw::Bit(false)), Ok(Value::Bool(false)));
        for order in ORDERS {
            assert_eq!(read(DataType::Bool, order, &[0]), Ok(Value::Bool(false)));
            assert_eq!(
                read(DataType::Bool, order, &[0x0100]),
                Ok(Value::Bool(true))
            );
            assert_eq!(read(DataType::Bool, order, &[1]), Ok(Value::Bool(true)));
        }
    }

    #[test]
    fn strings_end_at_nul_and_are_trimmed() {
        let name = |order| DataPoint {
            length: Some(4),
            ..point(DataType::String, order)
        };
        // "MS300" padded with NUL: 'M' 'S' | '3' '0' | '0' NUL | NUL NUL
        let abcd = [0x4D53, 0x3330, 0x3000, 0x0000];
        assert_eq!(
            decode(&name(ByteOrder::Abcd), Raw::Registers(&abcd)),
            Ok(Value::Text("MS300".into()))
        );
        let badc = [0x534D, 0x3033, 0x0030, 0x0000];
        assert_eq!(
            decode(&name(ByteOrder::Badc), Raw::Registers(&badc)),
            Ok(Value::Text("MS300".into()))
        );
        // Space padding is trimmed; anything after the first NUL is ignored.
        let padded = [0x2041, 0x4220, 0x0043, 0x4400];
        assert_eq!(
            decode(&name(ByteOrder::Abcd), Raw::Registers(&padded)),
            Ok(Value::Text("AB".into()))
        );
        // Bytes that are not UTF-8 are replaced, not refused.
        let latin1 = [0x41E9, 0x0000, 0x0000, 0x0000];
        assert_eq!(
            decode(&name(ByteOrder::Abcd), Raw::Registers(&latin1)),
            Ok(Value::Text("A\u{FFFD}".into()))
        );
        // All NUL is the empty string.
        assert_eq!(
            decode(&name(ByteOrder::Abcd), Raw::Registers(&[0; 4])),
            Ok(Value::Text(String::new()))
        );
        // The longest string, 125 registers of non-UTF-8 bytes, still fits a text value.
        let longest = vec![0xFFFF; 125];
        let Ok(Value::Text(text)) = read(DataType::String, ByteOrder::Abcd, &longest) else {
            panic!("expected text");
        };
        assert_eq!(text.chars().count(), 250);
    }

    #[test]
    fn the_wrong_number_of_registers_is_invalid() {
        assert_eq!(
            read(DataType::U32, ByteOrder::Abcd, &[1]),
            Err(InvalidValue::Width {
                expected: 2,
                got: 1
            })
        );
        assert_eq!(
            read(DataType::F64, ByteOrder::Abcd, &[1, 2, 3, 4, 5]),
            Err(InvalidValue::Width {
                expected: 4,
                got: 5
            })
        );
        assert_eq!(
            read(DataType::Bool, ByteOrder::Abcd, &[]),
            Err(InvalidValue::Width {
                expected: 1,
                got: 0
            })
        );
    }
}
