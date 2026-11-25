//! 新建分区表的纯计算：容量、几何、内容、覆盖探测与冲突判定。
//!
//! 这里只回答"如果新建，应当写出哪些字节、哪些区间已有数据、是否会压到既有
//! 分区"。读盘由观察层完成，判定层把读到的字节交给这些函数；因此同一份快照
//! 总能得到同一份内容（磁盘 GUID 由调用方给定）。

use crate::region::Region;
use crate::table::gpt::entries::{PartitionEntry, PartitionName, serialize_entries};
use crate::table::gpt::header::GptHeader;
use crate::table::gpt::parse::entry_array_span;
use crate::table::gpt::write::array_crc;
use crate::table::mbr::primary::{MbrEntry, TYPE_PROTECTIVE, size_in_lba};
use crate::table::{
    ENTRY_MIN_BYTES, GPT_MIN_HEADER_SIZE, Guid, MAX_ENTRIES, PRIMARY_ARRAY_LBA, PRIMARY_HEADER_LBA,
    TableError,
};

/// 新表的条目数，与旧版保持一致。
pub const DEFAULT_ENTRY_COUNT: u32 = MAX_ENTRIES;
/// 新表的单条条目字节数，与旧版保持一致。
pub const DEFAULT_ENTRY_SIZE: u32 = ENTRY_MIN_BYTES as u32;

/// MBR 引导扇区的固定字节数。
pub const MBR_SECTOR_BYTES: usize = 512;
/// 四条 MBR 记录的起始偏移。
pub const MBR_ENTRY_BASE: usize = 446;
/// 四条 MBR 记录的结束偏移。
pub const MBR_ENTRY_END: usize = 510;
/// 引导签名所在偏移。
pub const MBR_SIGNATURE_AT: usize = 510;

/// 新建哪一种表。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Gpt,
    Mbr,
}

/// 容量不足以容纳请求的表。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacityError {
    /// GPT 至少需要这么多逻辑块。
    Gpt { min_sectors: u64, sector_size: u64 },
    /// MBR 至少需要三个逻辑块。
    Mbr { sector_size: u64 },
}

impl CapacityError {
    /// 固定的一句话成因。
    pub fn describe(self) -> String {
        match self {
            Self::Gpt {
                min_sectors,
                sector_size,
            } => format!(
                "a GPT needs at least {min_sectors} sectors of {sector_size} bytes ({} bytes)",
                min_sectors.saturating_mul(sector_size)
            ),
            Self::Mbr { sector_size } => {
                format!("an MBR needs at least 3 sectors of {sector_size} bytes")
            }
        }
    }
}

/// 条目数组占用的逻辑块数。
pub fn gpt_span(sector_size: u64) -> Result<u64, TableError> {
    entry_array_span(DEFAULT_ENTRY_COUNT, DEFAULT_ENTRY_SIZE, sector_size)
}

/// GPT 的最小逻辑块数：主头、主数组、备份数组、备份头，外加两端各一个引导块。
pub fn gpt_min_sectors(sector_size: u64) -> Result<u64, TableError> {
    Ok(gpt_span(sector_size)?.saturating_mul(2).saturating_add(4))
}

/// 容量下限检查；与旧版在预检阶段落成拒绝，不写盘。
pub fn check_capacity(
    format: Format,
    total_bytes: u64,
    sector_size: u64,
) -> Result<(), CapacityError> {
    if sector_size == 0 {
        return Err(CapacityError::Mbr { sector_size });
    }
    let total_lba = total_bytes / sector_size;
    match format {
        Format::Gpt => {
            let Ok(min_sectors) = gpt_min_sectors(sector_size) else {
                return Err(CapacityError::Gpt {
                    min_sectors: u64::MAX,
                    sector_size,
                });
            };
            if total_lba < min_sectors {
                Err(CapacityError::Gpt {
                    min_sectors,
                    sector_size,
                })
            } else {
                Ok(())
            }
        }
        Format::Mbr => {
            if total_lba < 3 {
                Err(CapacityError::Mbr { sector_size })
            } else {
                Ok(())
            }
        }
    }
}

/// GPT 的固定几何与各段的字节区间。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GptLayout {
    pub sector_size: u64,
    pub total_lba: u64,
    pub span: u64,
    pub last_lba: u64,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub backup_array_lba: u64,
}

impl GptLayout {
    /// 保护 MBR。
    pub fn protective(&self) -> Region {
        Region::sized(0, self.sector_size).expect("扇区区间自洽")
    }

    pub fn primary_header(&self) -> Region {
        Region::sized(PRIMARY_HEADER_LBA * self.sector_size, self.sector_size)
            .expect("主头区间自洽")
    }

    pub fn primary_array(&self) -> Region {
        Region::sized(
            PRIMARY_ARRAY_LBA * self.sector_size,
            self.span * self.sector_size,
        )
        .expect("主数组区间自洽")
    }

    pub fn backup_array(&self) -> Region {
        Region::sized(
            self.backup_array_lba * self.sector_size,
            self.span * self.sector_size,
        )
        .expect("备份数组区间自洽")
    }

    pub fn backup_header(&self) -> Region {
        Region::sized(self.last_lba * self.sector_size, self.sector_size).expect("备份头区间自洽")
    }

    /// 写前/写后需要比较的全部区间，按 LBA 升序。
    pub fn regions(&self) -> [Region; 5] {
        [
            self.protective(),
            self.primary_header(),
            self.primary_array(),
            self.backup_array(),
            self.backup_header(),
        ]
    }
}

/// 计算 GPT 几何；容量不足时返回几何错误（容量拒绝由 [`check_capacity`] 先行）。
pub fn gpt_layout(total_bytes: u64, sector_size: u64) -> Result<GptLayout, TableError> {
    if sector_size == 0 {
        return Err(TableError::InvalidGeometry("sector size is zero"));
    }
    let span = gpt_span(sector_size)?;
    let min = span
        .checked_mul(2)
        .and_then(|value| value.checked_add(4))
        .ok_or(TableError::InvalidGeometry("GPT minimum overflow"))?;
    let total_lba = total_bytes / sector_size;
    if total_lba < min {
        return Err(TableError::InvalidGeometry("target is too small for a GPT"));
    }
    let last_lba = total_lba - 1;
    let first_usable_lba = PRIMARY_ARRAY_LBA
        .checked_add(span)
        .ok_or(TableError::InvalidGeometry("usable start overflow"))?;
    let backup_array_lba = last_lba - span;
    let last_usable_lba = backup_array_lba
        .checked_sub(1)
        .ok_or(TableError::InvalidGeometry("usable end underflow"))?;
    Ok(GptLayout {
        sector_size,
        total_lba,
        span,
        last_lba,
        first_usable_lba,
        last_usable_lba,
        backup_array_lba,
    })
}

/// 一段要写出的字节。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub region: Region,
    pub bytes: Vec<u8>,
}

/// 一份新 GPT 的全部写段，顺序即落盘顺序（备份在前、主副本在后）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GptContent {
    pub layout: GptLayout,
    pub segments: Vec<Segment>,
}

impl GptContent {
    /// 追加保护 MBR 写段；已合规时不写。
    pub fn with_protective(mut self, lba0: &[u8], total_lba: u64) -> Self {
        if let Some(bytes) = protective_mbr_content(lba0, total_lba) {
            self.segments.push(Segment {
                region: self.layout.protective(),
                bytes,
            });
        }
        self
    }
}

/// 生成一份空 GPT：重建主备头与两份条目数组。
pub fn gpt_content(
    total_bytes: u64,
    sector_size: u64,
    disk_guid: Guid,
) -> Result<GptContent, TableError> {
    let layout = gpt_layout(total_bytes, sector_size)?;
    let entries = vec![empty_entry(); DEFAULT_ENTRY_COUNT as usize];
    let array = serialize_entries(
        &entries,
        DEFAULT_ENTRY_COUNT,
        DEFAULT_ENTRY_SIZE,
        sector_size as usize,
    )?;
    let logical = DEFAULT_ENTRY_COUNT as usize * DEFAULT_ENTRY_SIZE as usize;
    let array_crc = array_crc(&array[..logical]);

    let base = GptHeader {
        revision: 0x0001_0000,
        primary_lba: PRIMARY_HEADER_LBA,
        backup_lba: layout.last_lba,
        first_usable_lba: layout.first_usable_lba,
        last_usable_lba: layout.last_usable_lba,
        disk_guid,
        partition_entry_lba: PRIMARY_ARRAY_LBA,
        number_of_partition_entries: DEFAULT_ENTRY_COUNT,
        size_of_partition_entry: DEFAULT_ENTRY_SIZE,
        header_size: GPT_MIN_HEADER_SIZE,
        entry_array_crc32: array_crc,
    };
    let primary_header = base.serialize(sector_size as usize)?;

    let backup = GptHeader {
        primary_lba: layout.last_lba,
        backup_lba: PRIMARY_HEADER_LBA,
        partition_entry_lba: layout.backup_array_lba,
        ..base
    };
    let backup_header = backup.serialize(sector_size as usize)?;

    Ok(GptContent {
        layout,
        segments: vec![
            Segment {
                region: layout.backup_array(),
                bytes: array.clone(),
            },
            Segment {
                region: layout.backup_header(),
                bytes: backup_header,
            },
            Segment {
                region: layout.primary_array(),
                bytes: array,
            },
            Segment {
                region: layout.primary_header(),
                bytes: primary_header,
            },
        ],
    })
}

fn empty_entry() -> PartitionEntry {
    PartitionEntry {
        partition_type_guid: Guid::ZERO,
        unique_partition_guid: Guid::ZERO,
        starting_lba: 0,
        ending_lba: 0,
        attributes: 0,
        name: PartitionName::EMPTY,
    }
}

/// 候选逻辑块大小：容器值优先，再补 512 / 4096；去重。
pub fn candidate_sizes(sector_size: u64) -> Vec<u64> {
    let mut sizes = vec![sector_size];
    for size in [512, 4096] {
        if size != sector_size {
            sizes.push(size);
        }
    }
    sizes
}

/// 清掉 LBA0 尾部内嵌的 GPT 签名（可能来自旧表），保留其余字节。
pub fn clear_embedded_gpt_signature(lba0: &mut [u8], sector_size: u64) {
    const SIGNATURE: &[u8; 8] = b"EFI PART";
    for size in candidate_sizes(sector_size) {
        let Ok(at) = usize::try_from(size) else {
            continue;
        };
        if at + SIGNATURE.len() <= lba0.len() && &lba0[at..at + SIGNATURE.len()] == SIGNATURE {
            lba0[at..at + SIGNATURE.len()].fill(0);
        }
    }
}

/// 新建 MBR 的 LBA0：保留引导代码，清空四条记录并置签名。
pub fn new_mbr_lba0(lba0: &[u8], sector_size: u64) -> Vec<u8> {
    let mut out = lba0.to_vec();
    if out.len() < MBR_SECTOR_BYTES {
        out.resize(MBR_SECTOR_BYTES, 0);
    }
    clear_embedded_gpt_signature(&mut out, sector_size);
    out[MBR_ENTRY_BASE..MBR_ENTRY_END].fill(0);
    out[MBR_SIGNATURE_AT] = 0x55;
    out[MBR_SIGNATURE_AT + 1] = 0xAA;
    out
}

/// 保护 MBR 的 SizeInLBA 与容器口径的关系。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PmbrIssue {
    /// 值小于规范值：容器扩容后的旧值。
    Stale,
    /// 值等于 512 字节口径的规范值。
    Compat512,
}

/// 保护 MBR 的三态结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PmbrSize {
    Normal,
    NeedsRepair {
        cause: PmbrIssue,
    },
    /// 值大于两种口径：可能是更大盘的截断副本，拒绝自动修复。
    Inconsistent,
}

/// 判定保护 MBR 的 SizeInLBA；只读 LBA0 前 512 字节。
pub fn pmbr_size_state(lba0: &[u8], total_bytes: u64, table_sector_size: u64) -> PmbrSize {
    if lba0.len() < MBR_SECTOR_BYTES || table_sector_size == 0 {
        return PmbrSize::Normal;
    }
    let size = read_u32(lba0, MBR_ENTRY_BASE + 12);
    let expected = size_in_lba(total_bytes / table_sector_size);
    let compat = size_in_lba(total_bytes / 512);
    if size == expected {
        PmbrSize::Normal
    } else if size == compat {
        PmbrSize::NeedsRepair {
            cause: PmbrIssue::Compat512,
        }
    } else if size < expected {
        PmbrSize::NeedsRepair {
            cause: PmbrIssue::Stale,
        }
    } else {
        PmbrSize::Inconsistent
    }
}

/// 保护 MBR 已合规时返回 `None`，否则返回整扇区的新内容。
///
/// 合规判据覆盖重写会改动的每个字段：签名、LBA0 尾部、槽位 2–4、槽位 1 的
/// BootIndicator、两个 CHS、类型、起点与 SizeInLBA。
pub fn protective_mbr_content(lba0: &[u8], total_lba: u64) -> Option<Vec<u8>> {
    if lba0.len() < MBR_SECTOR_BYTES {
        return None;
    }
    let want_size = size_in_lba(total_lba);
    let reserved_zero = lba0[MBR_SECTOR_BYTES..].iter().all(|&byte| byte == 0);
    if has_signature(lba0) && reserved_zero {
        let rec = &lba0[MBR_ENTRY_BASE..MBR_ENTRY_BASE + 16];
        let other_slots_zero = lba0[MBR_ENTRY_BASE + 16..MBR_SIGNATURE_AT]
            .iter()
            .all(|&byte| byte == 0);
        if other_slots_zero
            && rec[4] == TYPE_PROTECTIVE
            && rec[0] == 0x00
            && rec[1..4] == [0x00, 0x02, 0x00]
            && rec[5..8] == [0xFF, 0xFF, 0xFF]
            && read_u32(rec, 8) == 1
            && read_u32(rec, 12) == want_size
        {
            return None;
        }
    }
    Some(protective_mbr_image(lba0, total_lba))
}

/// 生成规范保护 MBR：保留 `0..446`，重写槽位 1 与 `446..510`，清空尾部并置签名。
pub fn protective_mbr_image(lba0: &[u8], total_lba: u64) -> Vec<u8> {
    let mut out = lba0.to_vec();
    if out.len() < MBR_SECTOR_BYTES {
        out.resize(MBR_SECTOR_BYTES, 0);
    }
    out[MBR_ENTRY_BASE..MBR_ENTRY_END].fill(0);
    out[MBR_SECTOR_BYTES..].fill(0);
    let rec = &mut out[MBR_ENTRY_BASE..MBR_ENTRY_BASE + 16];
    rec[0] = 0x00;
    rec[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
    rec[4] = TYPE_PROTECTIVE;
    rec[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    rec[8..12].copy_from_slice(&1u32.to_le_bytes());
    rec[12..16].copy_from_slice(&size_in_lba(total_lba).to_le_bytes());
    out[MBR_SIGNATURE_AT] = 0x55;
    out[MBR_SIGNATURE_AT + 1] = 0xAA;
    out
}

fn has_signature(lba0: &[u8]) -> bool {
    lba0.len() > MBR_SIGNATURE_AT
        && lba0[MBR_SIGNATURE_AT] == 0x55
        && lba0[MBR_SIGNATURE_AT + 1] == 0xAA
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("偏移已校验"))
}

/// 覆盖门的三态探测结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableProbe {
    /// 表区与签名区全零。
    Empty,
    /// 不是可识别的表，但表区有非零字节。
    Unrecognized,
    /// 已能识别出 GPT 或 MBR。
    Recognized,
}

impl TableProbe {
    /// 识别优先；无法识别时再看表区是否全零。
    pub fn classify(recognized: bool, blank: bool) -> Self {
        if recognized {
            Self::Recognized
        } else if blank {
            Self::Empty
        } else {
            Self::Unrecognized
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Unrecognized => "unrecognized",
            Self::Recognized => "recognized",
        }
    }
}

/// 既有分区占用的字节区间。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionSpan {
    pub label: String,
    pub start: u64,
    pub end: u64,
}

/// 一段带名字的写入区间，用于冲突消息。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabeledRegion {
    pub label: &'static str,
    pub region: Region,
}

/// 新表写入与既有分区的一次重叠。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub partition: String,
    pub written: &'static str,
    pub written_region: Region,
}

/// 第一个与写入区间重叠的既有分区。
pub fn first_conflict(regions: &[LabeledRegion], spans: &[PartitionSpan]) -> Option<Conflict> {
    for span in spans {
        let occupied = Region::new(span.start, span.end).ok()?;
        for written in regions {
            if written.region.overlaps(occupied) {
                return Some(Conflict {
                    partition: span.label.clone(),
                    written: written.label,
                    written_region: written.region,
                });
            }
        }
    }
    None
}

/// 所有采集区间是否全零（至少一段）。
pub fn regions_blank(regions: &[(Region, Vec<u8>)]) -> bool {
    !regions.is_empty()
        && regions
            .iter()
            .all(|(_, bytes)| bytes.iter().all(|&b| b == 0))
}

/// MBR 表写入需要清掉的 GPT 签名绝对偏移（不含表头/表尾三段本身）。
pub fn signature_sites(total_bytes: u64, sector_size: u64) -> Vec<u64> {
    let mut sites = Vec::new();
    for size in candidate_sizes(sector_size) {
        if total_bytes < size {
            continue;
        }
        let first = size;
        let last = (total_bytes / size - 1) * size;
        for offset in [first, last] {
            if !sites.contains(&offset) {
                sites.push(offset);
            }
        }
    }
    sites
}

/// 旧 MBR 记录（仅供测试构造夹具时复用）。
pub fn protective_entry(total_lba: u64) -> MbrEntry {
    MbrEntry {
        boot: 0,
        part_type: TYPE_PROTECTIVE,
        start_lba: 1,
        size_lba: size_in_lba(total_lba),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SS: u64 = 512;
    const TOTAL: u64 = 4 * 1024 * 1024;

    #[test]
    fn gpt_minimum_matches_the_classic_layout() {
        assert_eq!(gpt_span(SS).unwrap(), 32);
        assert_eq!(gpt_min_sectors(SS).unwrap(), 68);
        assert!(check_capacity(Format::Gpt, 67 * SS, SS).is_err());
        assert!(check_capacity(Format::Gpt, 68 * SS, SS).is_ok());
    }

    #[test]
    fn gpt_content_writes_both_copies_with_consistent_crc() {
        let content = gpt_content(TOTAL, SS, Guid([7; 16])).expect("应可构造");
        assert_eq!(content.segments.len(), 4);
        let layout = content.layout;
        assert_eq!(layout.last_lba, TOTAL / SS - 1);
        assert_eq!(layout.first_usable_lba, 34);
        assert_eq!(layout.last_usable_lba, layout.last_lba - 33);

        let primary = &content.segments[3];
        let backup = &content.segments[1];
        let header = GptHeader::parse(&primary.bytes).expect("主头可解析");
        assert_eq!(header.primary_lba, 1);
        assert_eq!(header.backup_lba, layout.last_lba);
        let backup_header = GptHeader::parse(&backup.bytes).expect("备份头可解析");
        assert_eq!(backup_header.primary_lba, layout.last_lba);
        assert_eq!(backup_header.partition_entry_lba, layout.backup_array_lba);
        assert_eq!(header.entry_array_crc32, backup_header.entry_array_crc32);
    }

    #[test]
    fn protective_mbr_is_rewritten_only_when_non_canonical() {
        let canonical = protective_mbr_image(&[0u8; 512], 2048);
        assert!(protective_mbr_content(&canonical, 2048).is_none());

        let mut stale = canonical.clone();
        stale[446 + 12..446 + 16].copy_from_slice(&7u32.to_le_bytes());
        let rewritten = protective_mbr_content(&stale, 2048).expect("应重写");
        assert_eq!(&rewritten[446 + 12..446 + 16], &2047u32.to_le_bytes());

        let mut dirty_tail = canonical;
        dirty_tail.resize(4096, 0);
        dirty_tail[3000] = 1;
        assert!(protective_mbr_content(&dirty_tail, 2048).is_some());
    }

    #[test]
    fn pmbr_size_state_distinguishes_stale_compat_and_inconsistent() {
        let mut lba0 = vec![0u8; 4096];
        lba0[446 + 4] = TYPE_PROTECTIVE;
        // 4Kn 容器：规范值是 total/4096 - 1。
        lba0[446 + 12..446 + 16].copy_from_slice(&1023u32.to_le_bytes());
        assert_eq!(
            pmbr_size_state(&lba0, 4 * 1024 * 1024, 4096),
            PmbrSize::Normal
        );
        lba0[446 + 12..446 + 16].copy_from_slice(&32u32.to_le_bytes());
        assert_eq!(
            pmbr_size_state(&lba0, 4 * 1024 * 1024, 4096),
            PmbrSize::NeedsRepair {
                cause: PmbrIssue::Stale
            }
        );
        lba0[446 + 12..446 + 16].copy_from_slice(&(4u32 * 1024 * 1024 / 512 - 1).to_le_bytes());
        assert_eq!(
            pmbr_size_state(&lba0, 4 * 1024 * 1024, 4096),
            PmbrSize::NeedsRepair {
                cause: PmbrIssue::Compat512
            }
        );
        lba0[446 + 12..446 + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            pmbr_size_state(&lba0, 4 * 1024 * 1024, 4096),
            PmbrSize::Inconsistent
        );
    }

    #[test]
    fn mbr_creation_preserves_boot_code_and_clears_entries() {
        let mut lba0 = vec![0xAAu8; 4096];
        lba0[0] = 0xEB;
        lba0[100] = 0x90;
        lba0[512..520].copy_from_slice(b"EFI PART");
        let out = new_mbr_lba0(&lba0, 512);
        assert_eq!(&out[..2], &[0xEB, 0xAA]);
        assert_eq!(out[100], 0x90);
        assert!(out[446..510].iter().all(|&b| b == 0));
        assert_eq!(&out[510..512], &[0x55, 0xAA]);
        assert!(out[512..520].iter().all(|&b| b == 0));
    }

    #[test]
    fn probe_prefers_recognition_then_emptiness() {
        assert_eq!(TableProbe::classify(true, false), TableProbe::Recognized);
        assert_eq!(TableProbe::classify(false, true), TableProbe::Empty);
        assert_eq!(TableProbe::classify(false, false), TableProbe::Unrecognized);
        assert!(!regions_blank(&[]));
    }

    #[test]
    fn conflict_reports_the_first_overlap() {
        let regions = [LabeledRegion {
            label: "primary entry array",
            region: Region::new(1024, 4096).unwrap(),
        }];
        let spans = [PartitionSpan {
            label: "GPT partition 1".to_owned(),
            start: 2048,
            end: 4096,
        }];
        let conflict = first_conflict(&regions, &spans).expect("应重叠");
        assert_eq!(conflict.partition, "GPT partition 1");
        assert_eq!(conflict.written, "primary entry array");
    }

    #[test]
    fn signature_sites_are_unique_and_include_both_ends() {
        let sites = signature_sites(1 << 20, 512);
        assert!(sites.contains(&512));
        assert!(sites.contains(&((1 << 20) - 512)));
        assert!(sites.contains(&4096));
    }
}
