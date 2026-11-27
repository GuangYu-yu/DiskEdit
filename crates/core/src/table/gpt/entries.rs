//! GPT 分区条目：128 字节固定布局的解析与序列化。

use super::super::{ENTRY_MIN_BYTES, ENTRY_NAME_BYTES, Guid, MAX_ENTRIES, TableError};

const OFFSET_TYPE: usize = 0;
const OFFSET_UNIQUE: usize = 16;
const OFFSET_START: usize = 32;
const OFFSET_END: usize = 40;
const OFFSET_ATTRIBUTES: usize = 48;
const OFFSET_NAME: usize = 56;

/// 一条 GPT 分区条目。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartitionEntry {
    pub partition_type_guid: Guid,
    pub unique_partition_guid: Guid,
    pub starting_lba: u64,
    pub ending_lba: u64,
    pub attributes: u64,
    pub name: PartitionName,
}

impl PartitionEntry {
    /// 全零条目：GPT 用它表示"槽位为空"。
    pub const EMPTY: PartitionEntry = PartitionEntry {
        partition_type_guid: Guid::ZERO,
        unique_partition_guid: Guid::ZERO,
        starting_lba: 0,
        ending_lba: 0,
        attributes: 0,
        name: PartitionName::EMPTY,
    };
}

/// 分区名，盘上为 36 个 UTF-16LE 码元；保留原始字节以便逐字节往返。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PartitionName {
    raw: [u8; ENTRY_NAME_BYTES],
}

impl PartitionName {
    pub const EMPTY: PartitionName = PartitionName {
        raw: [0u8; ENTRY_NAME_BYTES],
    };

    pub fn from_bytes(raw: [u8; ENTRY_NAME_BYTES]) -> Self {
        Self { raw }
    }

    pub fn as_bytes(&self) -> &[u8; ENTRY_NAME_BYTES] {
        &self.raw
    }

    /// 按 UTF-16LE 解码并去掉尾部空码元；孤立代理项由替换字符表示。
    pub fn to_string_lossy(&self) -> String {
        let (chunks, _) = self.raw.as_chunks::<2>();
        let units: Vec<u16> = chunks
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        String::from_utf16_lossy(&units)
            .trim_end_matches('\0')
            .to_owned()
    }

    /// 由字符串编码，超出 36 码元的部分截断。
    pub fn from_str_lossy(name: &str) -> Self {
        let mut raw = [0u8; ENTRY_NAME_BYTES];
        let (chunks, _) = raw.as_chunks_mut::<2>();
        for (slot, unit) in chunks.iter_mut().zip(name.encode_utf16()) {
            *slot = unit.to_le_bytes();
        }
        Self { raw }
    }
}

impl Default for PartitionName {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl std::fmt::Debug for PartitionName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PartitionName({:?})", self.to_string_lossy())
    }
}

/// 解析一条 128 字节条目。
pub fn parse_entry(raw: &[u8]) -> Result<PartitionEntry, TableError> {
    if raw.len() < ENTRY_MIN_BYTES {
        return Err(TableError::BufferTooSmall {
            needed: ENTRY_MIN_BYTES,
            actual: raw.len(),
        });
    }
    let mut name = [0u8; ENTRY_NAME_BYTES];
    name.copy_from_slice(&raw[OFFSET_NAME..OFFSET_NAME + ENTRY_NAME_BYTES]);
    Ok(PartitionEntry {
        partition_type_guid: Guid::from(read_guid(raw, OFFSET_TYPE)),
        unique_partition_guid: Guid::from(read_guid(raw, OFFSET_UNIQUE)),
        starting_lba: read_u64(raw, OFFSET_START),
        ending_lba: read_u64(raw, OFFSET_END),
        attributes: read_u64(raw, OFFSET_ATTRIBUTES),
        name: PartitionName::from_bytes(name),
    })
}

/// 解析条目数组：`count` 条、每条 `entry_size` 字节，只读前 128 字节。
pub fn parse_entries(
    raw: &[u8],
    count: u32,
    entry_size: u32,
) -> Result<Vec<PartitionEntry>, TableError> {
    validate_shape(count, entry_size)?;
    let needed =
        (count as usize)
            .checked_mul(entry_size as usize)
            .ok_or(TableError::BufferTooSmall {
                needed: usize::MAX,
                actual: raw.len(),
            })?;
    if raw.len() < needed {
        return Err(TableError::BufferTooSmall {
            needed,
            actual: raw.len(),
        });
    }
    (0..count as usize)
        .map(|index| {
            let offset = index * entry_size as usize;
            parse_entry(&raw[offset..offset + ENTRY_MIN_BYTES])
        })
        .collect()
}

/// 序列化一条 128 字节条目。
fn write_entry(entry: &PartitionEntry) -> [u8; ENTRY_MIN_BYTES] {
    let mut raw = [0u8; ENTRY_MIN_BYTES];
    raw[OFFSET_TYPE..OFFSET_TYPE + 16].copy_from_slice(&entry.partition_type_guid.0);
    raw[OFFSET_UNIQUE..OFFSET_UNIQUE + 16].copy_from_slice(&entry.unique_partition_guid.0);
    raw[OFFSET_START..OFFSET_START + 8].copy_from_slice(&entry.starting_lba.to_le_bytes());
    raw[OFFSET_END..OFFSET_END + 8].copy_from_slice(&entry.ending_lba.to_le_bytes());
    raw[OFFSET_ATTRIBUTES..OFFSET_ATTRIBUTES + 8].copy_from_slice(&entry.attributes.to_le_bytes());
    raw[OFFSET_NAME..OFFSET_NAME + ENTRY_NAME_BYTES].copy_from_slice(entry.name.as_bytes());
    raw
}

/// 序列化条目数组，末端补齐到整扇区；CRC 只覆盖 `count * entry_size` 字节。
pub fn serialize_entries(
    entries: &[PartitionEntry],
    count: u32,
    entry_size: u32,
    sector_size: usize,
) -> Result<Vec<u8>, TableError> {
    validate_shape(count, entry_size)?;
    if entries.len() != count as usize {
        return Err(TableError::InvalidGeometry(
            "entry count does not match array length",
        ));
    }
    let logical = count as usize * entry_size as usize;
    let padded = logical.div_ceil(sector_size) * sector_size;
    let mut raw = vec![0u8; padded];
    for (index, entry) in entries.iter().enumerate() {
        let offset = index * entry_size as usize;
        raw[offset..offset + ENTRY_MIN_BYTES].copy_from_slice(&write_entry(entry));
    }
    Ok(raw)
}

/// 已使用条目的判据：类型 GUID 非零。
pub fn is_used(entry: &PartitionEntry) -> bool {
    !entry.partition_type_guid.is_zero()
}

/// 条目级校验：已使用条目起止有序且落在可用区间内。
pub fn validate_entries(
    entries: &[PartitionEntry],
    first_usable: u64,
    last_usable: u64,
) -> Result<(), TableError> {
    for (index, entry) in entries.iter().enumerate() {
        if !is_used(entry) {
            continue;
        }
        if entry.starting_lba > entry.ending_lba {
            return Err(TableError::InvalidEntry {
                index: index + 1,
                start: entry.starting_lba,
                end: entry.ending_lba,
            });
        }
        if entry.starting_lba < first_usable || entry.ending_lba > last_usable {
            return Err(TableError::BeyondUsable {
                index: index + 1,
                start: entry.starting_lba,
                end: entry.ending_lba,
                first: first_usable,
                last: last_usable,
            });
        }
    }
    Ok(())
}

fn validate_shape(count: u32, entry_size: u32) -> Result<(), TableError> {
    if entry_size < ENTRY_MIN_BYTES as u32 {
        return Err(TableError::InvalidEntrySize(entry_size));
    }
    if count > MAX_ENTRIES {
        return Err(TableError::TooManyEntries {
            count,
            max: MAX_ENTRIES,
        });
    }
    Ok(())
}

fn read_u64(buffer: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(buffer[offset..offset + 8].try_into().expect("偏移已校验"))
}

fn read_guid(buffer: &[u8], offset: usize) -> [u8; 16] {
    buffer[offset..offset + 16].try_into().expect("偏移已校验")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn used(start: u64, end: u64) -> PartitionEntry {
        PartitionEntry {
            partition_type_guid: Guid([1; 16]),
            unique_partition_guid: Guid([2; 16]),
            starting_lba: start,
            ending_lba: end,
            attributes: 4,
            name: PartitionName::from_str_lossy("root"),
        }
    }

    fn unused() -> PartitionEntry {
        PartitionEntry {
            partition_type_guid: Guid::ZERO,
            unique_partition_guid: Guid::ZERO,
            starting_lba: 0,
            ending_lba: 0,
            attributes: 0,
            name: PartitionName::EMPTY,
        }
    }

    #[test]
    fn entry_round_trip() {
        let entries = vec![used(34, 100), unused(), used(200, 300)];
        let raw = serialize_entries(&entries, 3, 128, 512).expect("序列化应成功");
        assert_eq!(raw.len(), 512);
        assert_eq!(parse_entries(&raw, 3, 128), Ok(entries));
    }

    #[test]
    fn name_round_trip_and_truncation() {
        let name = PartitionName::from_str_lossy("系统盘");
        assert_eq!(name.to_string_lossy(), "系统盘");
        let long = "a".repeat(100);
        let truncated = PartitionName::from_str_lossy(&long);
        assert_eq!(truncated.to_string_lossy().chars().count(), 36);
    }

    #[test]
    fn rejects_short_entry_size_and_excess_count() {
        assert_eq!(
            parse_entries(&[0u8; 128], 1, 64),
            Err(TableError::InvalidEntrySize(64))
        );
        assert_eq!(
            parse_entries(&[0u8; 16], 1, 128),
            Err(TableError::BufferTooSmall {
                needed: 128,
                actual: 16
            })
        );
        assert_eq!(
            parse_entries(&[0u8; 129 * 128], 129, 128),
            Err(TableError::TooManyEntries {
                count: 129,
                max: 128
            })
        );
    }

    #[test]
    fn validates_used_entries_only() {
        let entries = vec![used(34, 100), unused()];
        assert_eq!(validate_entries(&entries, 34, 100), Ok(()));
        assert!(matches!(
            validate_entries(&[used(100, 34)], 34, 200),
            Err(TableError::InvalidEntry { .. })
        ));
        assert!(matches!(
            validate_entries(&[used(1, 100)], 34, 200),
            Err(TableError::BeyondUsable { .. })
        ));
    }

    #[test]
    fn zero_type_guid_marks_unused_even_with_leftover_lba() {
        let leftover = PartitionEntry {
            starting_lba: 50,
            ending_lba: 60,
            ..unused()
        };
        assert!(!is_used(&leftover));
        assert_eq!(validate_entries(&[leftover], 34, 100), Ok(()));
    }
}
