//! GPT 副本解析与损坏分类。
//!
//! 解析只回答"这一份副本能不能用"；主备回退与分叉判定由 [`classify`] 给出，
//! 全部是纯函数：输入为已读出的扇区字节与容器几何，不读盘、不看时钟。

use super::super::{PRIMARY_ARRAY_LBA, PRIMARY_HEADER_LBA, TableError};
use super::entries::{PartitionEntry, parse_entries, validate_entries};
use super::header::GptHeader;
use super::write::array_crc;

/// 条目数组字节总量的下限（规范实现要求至少 16 KiB）。
const MIN_ARRAY_BYTES: usize = 16_384;

/// 主副本与备份副本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GptCopyKind {
    Primary,
    Backup,
}

/// 表头几何状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GptState {
    Valid,
    NeedsRepair { cause: HeaderIssue },
}

/// 需要重写头部的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderIssue {
    /// 备份头停在旧末端。
    BackupLbaStale { expected: u64, actual: u64 },
    /// 主副本不可用，当前表来自备份。
    PrimaryUnreadable,
    /// 主副本可用但备份不可用。
    BackupUnreadable,
    /// 两份副本各自自洽但陈述不同表。
    CopiesDiverge,
}

/// 一份可用的 GPT 副本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GptCopy {
    pub header: GptHeader,
    pub entries: Vec<PartitionEntry>,
    pub state: GptState,
}

/// 单份副本的解析结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsedCopy {
    Absent,
    Usable(GptCopy),
    Damaged(TableError),
}

/// 整盘判定结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GptEvidence {
    Absent,
    Usable(GptCopy),
    Damaged(TableError),
}

impl GptEvidence {
    pub fn usable(&self) -> Option<&GptCopy> {
        match self {
            Self::Usable(copy) => Some(copy),
            Self::Absent | Self::Damaged(_) => None,
        }
    }
}

/// 解析主副本：头部扇区、条目数组字节、容器最后 LBA 与逻辑块大小。
pub fn parse_primary(
    header_sector: &[u8],
    array: &[u8],
    last_lba: u64,
    sector_size: u64,
) -> ParsedCopy {
    parse_copy(
        header_sector,
        array,
        last_lba,
        sector_size,
        GptCopyKind::Primary,
    )
}

/// 解析备份副本；头部须自述位于容器末端、对端为 LBA1。
pub fn parse_backup(
    header_sector: &[u8],
    array: &[u8],
    last_lba: u64,
    sector_size: u64,
) -> ParsedCopy {
    match parse_copy(
        header_sector,
        array,
        last_lba,
        sector_size,
        GptCopyKind::Backup,
    ) {
        ParsedCopy::Usable(mut copy) => {
            // 副本自述的位置与本实现读到它的位置矛盾：这是可辨认的损坏，
            // 不是"没有 GPT"。折成 Absent 会让调用方把一块受损盘当作空盘重建。
            if copy.header.primary_lba != last_lba || copy.header.backup_lba != PRIMARY_HEADER_LBA {
                return ParsedCopy::Damaged(TableError::InvalidGeometry(
                    "backup header reverse pointers contradict its position",
                ));
            }
            copy.header.primary_lba = PRIMARY_HEADER_LBA;
            copy.header.backup_lba = last_lba;
            copy.state = GptState::NeedsRepair {
                cause: HeaderIssue::PrimaryUnreadable,
            };
            ParsedCopy::Usable(copy)
        }
        other @ (ParsedCopy::Absent | ParsedCopy::Damaged(_)) => other,
    }
}

fn parse_copy(
    header_sector: &[u8],
    array: &[u8],
    last_lba: u64,
    sector_size: u64,
    view: GptCopyKind,
) -> ParsedCopy {
    let header = match GptHeader::parse(header_sector) {
        Ok(header) => header,
        Err(TableError::MissingSignature) => return ParsedCopy::Absent,
        Err(error) => return ParsedCopy::Damaged(error),
    };
    let state = match validate_geometry(&header, last_lba, sector_size, view) {
        Ok(state) => state,
        Err(error) => return ParsedCopy::Damaged(error),
    };
    let logical = match (header.number_of_partition_entries as usize)
        .checked_mul(header.size_of_partition_entry as usize)
    {
        Some(logical) => logical,
        None => {
            return ParsedCopy::Damaged(TableError::InvalidGeometry("entry array size overflow"));
        }
    };
    // 条目数组字节总量下限：规范实现（testdisk 等）要求至少 16 KiB。更小的数组
    // 意味着条目表被截断或被伪造，按几何不成立拒绝，不继续把短数组当完整表读。
    if logical < MIN_ARRAY_BYTES {
        return ParsedCopy::Damaged(TableError::InvalidGeometry(
            "entry array is smaller than 16384 bytes",
        ));
    }
    if logical > array.len() {
        return ParsedCopy::Damaged(TableError::BufferTooSmall {
            needed: logical,
            actual: array.len(),
        });
    }
    let entries = match parse_entries(
        array,
        header.number_of_partition_entries,
        header.size_of_partition_entry,
    ) {
        Ok(entries) => entries,
        Err(error) => return ParsedCopy::Damaged(error),
    };
    if array_crc(&array[..logical]) != header.entry_array_crc32 {
        return ParsedCopy::Damaged(TableError::EntryArrayCrcMismatch);
    }
    if let Err(error) = validate_entries(&entries, header.first_usable_lba, header.last_usable_lba)
    {
        return ParsedCopy::Damaged(error);
    }
    ParsedCopy::Usable(GptCopy {
        header,
        entries,
        state,
    })
}

/// 几何校验：头字段自洽、不越出容器，且条目数组不侵占可用区。
pub fn validate_geometry(
    header: &GptHeader,
    last_lba: u64,
    sector_size: u64,
    view: GptCopyKind,
) -> Result<GptState, TableError> {
    let invalid = TableError::InvalidGeometry;
    // 规范版本上限：1.2（0x0001_0200）。更高版本可能改变条目语义，拒绝识别而不是
    // 按已知布局猜读（testdisk 同口径）。
    if header.revision > 0x0001_0200 {
        return Err(invalid("revision is newer than 1.2"));
    }
    let expected_my_lba = match view {
        GptCopyKind::Primary => PRIMARY_HEADER_LBA,
        GptCopyKind::Backup => last_lba,
    };
    if header.primary_lba != expected_my_lba {
        return Err(invalid("MyLBA does not match the copy position"));
    }
    if header.first_usable_lba > header.last_usable_lba {
        return Err(invalid("first_usable_lba exceeds last_usable_lba"));
    }
    if header.last_usable_lba > last_lba {
        return Err(TableError::BeyondContainer {
            field: "last_usable_lba",
            value: header.last_usable_lba,
            last_lba,
        });
    }
    if header.backup_lba > last_lba {
        return Err(TableError::BeyondContainer {
            field: "backup_lba",
            value: header.backup_lba,
            last_lba,
        });
    }
    if header.partition_entry_lba < PRIMARY_ARRAY_LBA {
        return Err(invalid(
            "partition_entry_lba covers the header or protective MBR",
        ));
    }
    if header.size_of_partition_entry < 128 {
        return Err(TableError::InvalidEntrySize(header.size_of_partition_entry));
    }
    let span = entry_array_span(
        header.number_of_partition_entries,
        header.size_of_partition_entry,
        sector_size,
    )?;
    let array_end = header
        .partition_entry_lba
        .checked_add(span)
        .ok_or(invalid("entry array range overflow"))?;
    if header.first_usable_lba < PRIMARY_ARRAY_LBA + span {
        return Err(invalid(
            "first_usable_lba leaves no room for the entry array",
        ));
    }
    if last_lba < PRIMARY_ARRAY_LBA + span {
        return Err(invalid("container too small for the entry array"));
    }
    if header.last_usable_lba > last_lba - span - 1 {
        return Err(invalid("last_usable_lba overlaps the backup entry array"));
    }
    match view {
        GptCopyKind::Primary => {
            if array_end > header.first_usable_lba {
                return Err(invalid("primary entry array overlaps the usable range"));
            }
        }
        GptCopyKind::Backup => {
            if header.partition_entry_lba <= header.last_usable_lba {
                return Err(invalid("backup entry array overlaps the usable range"));
            }
            if array_end > last_lba {
                return Err(invalid(
                    "backup entry array extends beyond the backup header",
                ));
            }
        }
    }
    Ok(if header.backup_lba == last_lba {
        GptState::Valid
    } else {
        GptState::NeedsRepair {
            cause: HeaderIssue::BackupLbaStale {
                expected: last_lba,
                actual: header.backup_lba,
            },
        }
    })
}

/// 条目数组占用的 LBA 数：字节数按逻辑块向上取整。
pub fn entry_array_span(count: u32, entry_size: u32, sector_size: u64) -> Result<u64, TableError> {
    if sector_size == 0 {
        return Err(TableError::InvalidGeometry("sector size is zero"));
    }
    let bytes = (count as u64)
        .checked_mul(entry_size as u64)
        .ok_or(TableError::InvalidGeometry("entry array size overflow"))?;
    Ok(bytes.div_ceil(sector_size))
}

/// 两份副本是否陈述同一张表：指向各自副本的 LBA 字段不参与比较。
fn same_table(left: &GptCopy, right: &GptCopy) -> bool {
    left.entries == right.entries
        && left.header.revision == right.header.revision
        && left.header.first_usable_lba == right.header.first_usable_lba
        && left.header.last_usable_lba == right.header.last_usable_lba
        && left.header.disk_guid == right.header.disk_guid
        && left.header.number_of_partition_entries == right.header.number_of_partition_entries
        && left.header.size_of_partition_entry == right.header.size_of_partition_entry
        && left.header.entry_array_crc32 == right.header.entry_array_crc32
}

/// 整盘判定：主副本优先；主副本不可用时回退备份并标记需要修复。
pub fn classify(
    primary_header: &[u8],
    primary_array: &[u8],
    backup_header: &[u8],
    backup_array: &[u8],
    last_lba: u64,
    sector_size: u64,
) -> GptEvidence {
    let mut damaged = None;
    match parse_primary(primary_header, primary_array, last_lba, sector_size) {
        ParsedCopy::Usable(mut copy) => {
            if copy.state == GptState::Valid {
                match parse_backup(backup_header, backup_array, last_lba, sector_size) {
                    ParsedCopy::Usable(backup) => {
                        if !same_table(&copy, &backup) {
                            copy.state = GptState::NeedsRepair {
                                cause: HeaderIssue::CopiesDiverge,
                            };
                        }
                    }
                    ParsedCopy::Absent | ParsedCopy::Damaged(_) => {
                        copy.state = GptState::NeedsRepair {
                            cause: HeaderIssue::BackupUnreadable,
                        };
                    }
                }
            }
            return GptEvidence::Usable(copy);
        }
        ParsedCopy::Absent => {}
        ParsedCopy::Damaged(error) => damaged = Some(error),
    }
    match parse_backup(backup_header, backup_array, last_lba, sector_size) {
        ParsedCopy::Usable(copy) => return GptEvidence::Usable(copy),
        ParsedCopy::Absent => {}
        ParsedCopy::Damaged(error) => damaged = damaged.or(Some(error)),
    }
    match damaged {
        Some(error) => GptEvidence::Damaged(error),
        None => GptEvidence::Absent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::Guid;
    use crate::table::gpt::entries::serialize_entries;
    use crate::table::gpt::entries::{PartitionEntry, PartitionName};

    const SECTOR: usize = 512;
    const SS: u64 = 512;

    fn primary_header(backup_lba: u64, last_lba: u64, array_crc: u32) -> GptHeader {
        GptHeader {
            revision: 0x0001_0000,
            primary_lba: 1,
            backup_lba,
            first_usable_lba: 34,
            last_usable_lba: last_lba - 34,
            disk_guid: Guid([9; 16]),
            partition_entry_lba: 2,
            number_of_partition_entries: 128,
            size_of_partition_entry: 128,
            header_size: 92,
            entry_array_crc32: array_crc,
        }
    }

    fn backup_header(last_lba: u64, array_crc: u32) -> GptHeader {
        GptHeader {
            revision: 0x0001_0000,
            primary_lba: last_lba,
            backup_lba: 1,
            first_usable_lba: 34,
            last_usable_lba: last_lba - 34,
            disk_guid: Guid([9; 16]),
            partition_entry_lba: last_lba - 32,
            number_of_partition_entries: 128,
            size_of_partition_entry: 128,
            header_size: 92,
            entry_array_crc32: array_crc,
        }
    }

    fn entries() -> Vec<PartitionEntry> {
        let mut entries = vec![
            PartitionEntry {
                partition_type_guid: Guid::ZERO,
                unique_partition_guid: Guid::ZERO,
                starting_lba: 0,
                ending_lba: 0,
                attributes: 0,
                name: PartitionName::EMPTY,
            };
            128
        ];
        entries[0] = PartitionEntry {
            partition_type_guid: Guid([3; 16]),
            unique_partition_guid: Guid([4; 16]),
            starting_lba: 40,
            ending_lba: 60,
            attributes: 0,
            name: PartitionName::from_str_lossy("data"),
        };
        entries
    }

    fn pair(last_lba: u64) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
        let array = serialize_entries(&entries(), 128, 128, SECTOR).expect("序列化应成功");
        let crc = array_crc(&array[..128 * 128]);
        let primary = primary_header(last_lba, last_lba, crc)
            .serialize(SECTOR)
            .expect("序列化应成功");
        let backup = backup_header(last_lba, crc)
            .serialize(SECTOR)
            .expect("序列化应成功");
        (primary, array.clone(), backup, array)
    }

    #[test]
    fn a_revision_above_1_2_is_rejected() {
        let last = 2047;
        let (_, array, _, _) = pair(last);
        let mut header = primary_header(last, last, array_crc(&array[..128 * 128]));
        header.revision = 0x0001_0201;
        let sector = header.serialize(SECTOR).expect("序列化应成功");
        assert_eq!(
            parse_primary(&sector, &array, last, SS),
            ParsedCopy::Damaged(TableError::InvalidGeometry("revision is newer than 1.2"))
        );
    }

    #[test]
    fn an_entry_array_below_16_kib_is_rejected() {
        let last = 2047;
        let mut header = primary_header(last, last, 0);
        header.number_of_partition_entries = 64;
        let sector = header.serialize(SECTOR).expect("序列化应成功");
        let array = vec![0u8; 64 * 128];
        assert_eq!(
            parse_primary(&sector, &array, last, SS),
            ParsedCopy::Damaged(TableError::InvalidGeometry(
                "entry array is smaller than 16384 bytes"
            ))
        );
    }

    #[test]
    fn primary_round_trip_is_usable() {
        let last = 2047;
        let (sector, array, _, _) = pair(last);
        let ParsedCopy::Usable(copy) = parse_primary(&sector, &array, last, SS) else {
            panic!("主副本应可用");
        };
        assert_eq!(copy.state, GptState::Valid);
        assert_eq!(copy.entries[0].starting_lba, 40);
    }

    #[test]
    fn crc_damaged_array_is_reported() {
        let last = 2047;
        let (sector, mut array, _, _) = pair(last);
        array[0] ^= 0xFF;
        assert_eq!(
            parse_primary(&sector, &array, last, SS),
            ParsedCopy::Damaged(TableError::EntryArrayCrcMismatch)
        );
    }

    #[test]
    fn missing_signature_is_absent_not_damaged() {
        let last = 2047;
        let (mut sector, array, _, _) = pair(last);
        sector[0] = 0;
        assert_eq!(parse_primary(&sector, &array, last, SS), ParsedCopy::Absent);
    }

    #[test]
    fn stale_backup_lba_needs_repair() {
        let last = 4095;
        let (_, array, _, _) = pair(last);
        let crc = array_crc(&array[..128 * 128]);
        let stale = primary_header(2047, last, crc)
            .serialize(SECTOR)
            .expect("序列化应成功");
        let ParsedCopy::Usable(copy) = parse_primary(&stale, &array, last, SS) else {
            panic!("主副本应可用");
        };
        assert_eq!(
            copy.state,
            GptState::NeedsRepair {
                cause: HeaderIssue::BackupLbaStale {
                    expected: last,
                    actual: 2047
                }
            }
        );
    }

    #[test]
    fn classify_falls_back_to_backup() {
        let last = 2047;
        let (_, array, backup, backup_array) = pair(last);
        let bad_primary = vec![0u8; SECTOR];
        let evidence = classify(&bad_primary, &array, &backup, &backup_array, last, SS);
        let copy = evidence.usable().expect("应回退到备份");
        assert_eq!(
            copy.state,
            GptState::NeedsRepair {
                cause: HeaderIssue::PrimaryUnreadable
            }
        );
    }

    #[test]
    fn classify_flags_diverging_copies() {
        let last = 2047;
        let (primary, array, _backup, _) = pair(last);
        let mut changed = entries();
        changed[0].ending_lba = 61;
        let changed_array = serialize_entries(&changed, 128, 128, SECTOR).expect("序列化应成功");
        let crc = array_crc(&changed_array[..128 * 128]);
        let changed_backup = backup_header(last, crc)
            .serialize(SECTOR)
            .expect("序列化应成功");
        let evidence = classify(&primary, &array, &changed_backup, &changed_array, last, SS);
        let copy = evidence.usable().expect("主副本应可用");
        assert_eq!(
            copy.state,
            GptState::NeedsRepair {
                cause: HeaderIssue::CopiesDiverge
            }
        );
    }

    #[test]
    fn a_backup_with_a_contradictory_reverse_pointer_is_damaged_not_absent() {
        let last = 2047;
        let (_, array, _, backup_array) = pair(last);
        let crc = array_crc(&array[..128 * 128]);
        let mut header = backup_header(last, crc);
        // 备份头自称对端在别处，而不是 LBA1：副本自相矛盾。
        header.backup_lba = 999;
        let sector = header.serialize(SECTOR).expect("序列化应成功");
        assert_eq!(
            parse_backup(&sector, &backup_array, last, SS),
            ParsedCopy::Damaged(TableError::InvalidGeometry(
                "backup header reverse pointers contradict its position"
            ))
        );
    }

    #[test]
    fn a_contradictory_backup_does_not_fold_to_absent() {
        let last = 2047;
        let (_, array, _, backup_array) = pair(last);
        let crc = array_crc(&array[..128 * 128]);
        let mut header = backup_header(last, crc);
        header.backup_lba = 999;
        let backup = header.serialize(SECTOR).expect("序列化应成功");
        let zeros = vec![0u8; SECTOR];
        assert_eq!(
            classify(&zeros, &zeros, &backup, &backup_array, last, SS),
            GptEvidence::Damaged(TableError::InvalidGeometry(
                "backup header reverse pointers contradict its position"
            ))
        );
    }

    #[test]
    fn classify_reports_absent_when_both_missing() {
        let last = 2047;
        let zeros = vec![0u8; SECTOR];
        assert_eq!(
            classify(&zeros, &zeros, &zeros, &zeros, last, SS),
            GptEvidence::Absent
        );
    }
}
