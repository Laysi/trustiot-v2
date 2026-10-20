//! Merging a model's data points into as few reads as possible (rebuild checklist F1): points
//! of the same function whose addresses are at most [`MAX_GAP`] apart share one read, up to
//! what one response holds ([`super::pdu::MAX_REGISTERS`] registers or
//! [`super::pdu::MAX_BITS`] bits).

use std::fmt;

use protocol::config::{DataPoint, DataType, FunctionCode};

use super::pdu::{Read, max_count};

/// Most unread addresses between two points that still share a read.
pub const MAX_GAP: u16 = 10;

/// One read and the data points it returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedRead {
    /// The request.
    pub read: Read,
    /// The points in the response, in address order.
    pub points: Vec<Slot>,
}

/// Where one data point sits in a [`PlannedRead`]'s response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    /// The point's position in the model's `points`.
    pub index: usize,
    /// Its first coil, input or register, counted from the read's start.
    pub offset: u16,
    /// How many coils, inputs or registers it spans.
    pub width: u16,
}

/// A data point that cannot be read. [`protocol::config::DesiredConfig`] refuses such
/// configurations, so these errors mean the plan was given points that were never validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// The point's size is unknown or too large for one read: a string without a valid
    /// `length`.
    Width {
        /// The point's position in the model's `points`.
        index: usize,
    },
    /// The point runs past address 65535.
    PastEnd {
        /// The point's position in the model's `points`.
        index: usize,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Width { index } => write!(
                f,
                "points[{index}] has no size that one read can return; validate the model first"
            ),
            Self::PastEnd { index } => write!(f, "points[{index}] runs past address 65535"),
        }
    }
}

impl std::error::Error for PlanError {}

/// How many coils, inputs or registers a point spans; `None` when unknown.
pub fn width(point: &DataPoint) -> Option<u16> {
    if point.function_code.reads_bits() {
        return Some(1);
    }
    match point.data_type {
        DataType::String => point
            .length
            .filter(|length| (1..=max_count(point.function_code)).contains(length)),
        other => other.fixed_registers(),
    }
}

/// Plans the reads for `points`: ordered by function code, then address. Two points of one
/// function share a read when at most [`MAX_GAP`] addresses lie between them and the read stays
/// within one response; the addresses in between are read and ignored.
pub fn plan(points: &[DataPoint]) -> Result<Vec<PlannedRead>, PlanError> {
    // (function, first address, width, index)
    let mut spans = Vec::with_capacity(points.len());
    for (index, point) in points.iter().enumerate() {
        let width = width(point).ok_or(PlanError::Width { index })?;
        if u32::from(point.address) + u32::from(width) - 1 > u32::from(u16::MAX) {
            return Err(PlanError::PastEnd { index });
        }
        spans.push((point.function_code, point.address, width, index));
    }
    spans.sort_unstable_by_key(|&(function, address, _, index)| (function, address, index));

    let mut reads = Vec::new();
    let mut current: Option<Group> = None;
    for (function, address, width, index) in spans {
        let last = u32::from(address) + u32::from(width) - 1;
        if let Some(group) = current.as_mut()
            && group.function == function
            && u32::from(address) <= group.last + 1 + u32::from(MAX_GAP)
            && group.last.max(last) - u32::from(group.start) < u32::from(max_count(function))
        {
            group.last = group.last.max(last);
            group.points.push((index, address, width));
            continue;
        }
        if let Some(group) = current.take() {
            reads.push(group.finish());
        }
        current = Some(Group {
            function,
            start: address,
            last,
            points: vec![(index, address, width)],
        });
    }
    if let Some(group) = current {
        reads.push(group.finish());
    }
    Ok(reads)
}

/// Points being merged into one read: (index, address, width) each.
struct Group {
    function: FunctionCode,
    start: u16,
    last: u32,
    points: Vec<(usize, u16, u16)>,
}

impl Group {
    fn finish(self) -> PlannedRead {
        // The group starts at an address and never grows beyond one response, so the count and
        // every offset fit a u16.
        let count = u16::try_from(self.last - u32::from(self.start) + 1).unwrap_or(u16::MAX);
        let read = Read::new(self.function, self.start, count)
            .unwrap_or_else(|_| unreachable!("a group stays within one response and 0..=65535"));
        PlannedRead {
            read,
            points: self
                .points
                .into_iter()
                .map(|(index, address, width)| Slot {
                    index,
                    offset: address - self.start,
                    width,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::PointKey;
    use protocol::config::ByteOrder;

    use super::*;

    fn point(function_code: FunctionCode, address: u16, data_type: DataType) -> DataPoint {
        DataPoint {
            key: PointKey::new(format!("p{}_{address}", function_code.number())).unwrap(),
            function_code,
            address,
            data_type,
            byte_order: ByteOrder::Abcd,
            length: None,
            scale: None,
            offset: None,
        }
    }

    fn holding(address: u16, data_type: DataType) -> DataPoint {
        point(FunctionCode::ReadHoldingRegisters, address, data_type)
    }

    fn ranges(reads: &[PlannedRead]) -> Vec<(u8, u16, u16)> {
        reads
            .iter()
            .map(|planned| {
                (
                    planned.read.function().number(),
                    planned.read.start(),
                    planned.read.count(),
                )
            })
            .collect()
    }

    #[test]
    fn adjacent_points_share_a_read() {
        let reads = plan(&[
            holding(0, DataType::U16),
            holding(1, DataType::F32),
            holding(3, DataType::F64),
        ])
        .unwrap();
        assert_eq!(ranges(&reads), [(3, 0, 7)]);
        assert_eq!(
            reads[0].points,
            [
                Slot {
                    index: 0,
                    offset: 0,
                    width: 1
                },
                Slot {
                    index: 1,
                    offset: 1,
                    width: 2
                },
                Slot {
                    index: 2,
                    offset: 3,
                    width: 4
                },
            ]
        );
    }

    #[test]
    fn a_gap_of_ten_merges_and_eleven_splits() {
        // Point 0 ends at 1; 12 leaves 10 unread addresses (2..=11), 13 leaves 11.
        let ten = plan(&[holding(0, DataType::U32), holding(12, DataType::U16)]).unwrap();
        assert_eq!(ranges(&ten), [(3, 0, 13)]);
        assert_eq!(ten[0].points[1].offset, 12);
        let eleven = plan(&[holding(0, DataType::U32), holding(13, DataType::U16)]).unwrap();
        assert_eq!(ranges(&eleven), [(3, 0, 2), (3, 13, 1)]);
    }

    #[test]
    fn a_read_holds_at_most_125_registers() {
        // 0, 10, ..., 120, then 124: exactly 125 registers.
        let mut points: Vec<DataPoint> = (0..13).map(|i| holding(i * 10, DataType::U16)).collect();
        points.push(holding(124, DataType::U16));
        assert_eq!(ranges(&plan(&points).unwrap()), [(3, 0, 125)]);
        // A point that would end at 125 starts a new read, although it is within the gap.
        points.pop();
        points.push(holding(124, DataType::U32));
        assert_eq!(ranges(&plan(&points).unwrap()), [(3, 0, 121), (3, 124, 2)]);
        // A chain of close points is split where the limit is reached: 0..=120, 130..=250,
        // 260..=290.
        let chain: Vec<DataPoint> = (0..30).map(|i| holding(i * 10, DataType::U16)).collect();
        assert_eq!(
            ranges(&plan(&chain).unwrap()),
            [(3, 0, 121), (3, 130, 121), (3, 260, 31)]
        );
    }

    #[test]
    fn a_read_holds_at_most_2000_bits() {
        let coil = |address| point(FunctionCode::ReadCoils, address, DataType::Bool);
        // 0, 10, ..., 1990, then 1999: exactly 2000 bits; 2000 starts a new read.
        let mut coils: Vec<DataPoint> = (0..200).map(|i| coil(i * 10)).collect();
        coils.push(coil(1999));
        assert_eq!(ranges(&plan(&coils).unwrap()), [(1, 0, 2000)]);
        coils.push(coil(2000));
        assert_eq!(ranges(&plan(&coils).unwrap()), [(1, 0, 2000), (1, 2000, 1)]);
    }

    #[test]
    fn bits_merge_across_gaps_of_ten() {
        let input = |address| point(FunctionCode::ReadDiscreteInputs, address, DataType::Bool);
        // 0, 11, 22, ...: 10 unread inputs between neighbours, so they merge up to 1991
        // (the last multiple of 11 below 2000) and again from 2002 to 2189.
        let inputs: Vec<DataPoint> = (0..200).map(|i| input(i * 11)).collect();
        assert_eq!(
            ranges(&plan(&inputs).unwrap()),
            [(2, 0, 1992), (2, 2002, 188)]
        );
    }

    #[test]
    fn function_codes_are_separate_address_spaces() {
        let reads = plan(&[
            point(FunctionCode::ReadInputRegisters, 0, DataType::U16),
            point(FunctionCode::ReadHoldingRegisters, 1, DataType::U16),
            point(FunctionCode::ReadDiscreteInputs, 0, DataType::Bool),
            point(FunctionCode::ReadCoils, 2, DataType::Bool),
            point(FunctionCode::ReadHoldingRegisters, 0, DataType::U16),
        ])
        .unwrap();
        assert_eq!(ranges(&reads), [(1, 2, 1), (2, 0, 1), (3, 0, 2), (4, 0, 1)]);
        assert_eq!(
            reads[2]
                .points
                .iter()
                .map(|slot| slot.index)
                .collect::<Vec<_>>(),
            [4, 1]
        );
    }

    #[test]
    fn strings_span_their_length() {
        let mut name = holding(10, DataType::String);
        name.length = Some(8);
        let reads = plan(&[name.clone(), holding(18, DataType::U16)]).unwrap();
        assert_eq!(ranges(&reads), [(3, 10, 9)]);
        assert_eq!(reads[0].points[0].width, 8);

        name.length = Some(125);
        assert_eq!(ranges(&plan(&[name.clone()]).unwrap()), [(3, 10, 125)]);
        for length in [None, Some(0), Some(126)] {
            name.length = length;
            assert_eq!(plan(&[name.clone()]), Err(PlanError::Width { index: 0 }));
        }
    }

    #[test]
    fn points_past_the_last_address_are_refused() {
        assert!(plan(&[holding(65_535, DataType::U16)]).is_ok());
        assert_eq!(
            ranges(&plan(&[holding(65_534, DataType::U32)]).unwrap()),
            [(3, 65_534, 2)]
        );
        assert_eq!(
            plan(&[holding(0, DataType::U16), holding(65_535, DataType::U32)]),
            Err(PlanError::PastEnd { index: 1 })
        );
        let coil = point(FunctionCode::ReadCoils, 65_535, DataType::Bool);
        assert_eq!(ranges(&plan(&[coil]).unwrap()), [(1, 65_535, 1)]);
    }

    #[test]
    fn no_points_need_no_reads() {
        assert_eq!(plan(&[]), Ok(Vec::new()));
    }
}
