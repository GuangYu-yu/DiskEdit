//! 分区表观察：按候选逻辑块大小读主/备 GPT 副本，回退 MBR。
//!
//! 镜像的容器扇区大小与表自身的逻辑块大小是两件事：GPT 条目 LBA 以表自身的大小
//! 计，MBR 条目以容器的大小计。这里把两者的换算各留在产生它的那一层。

use diskedit_core::fs::ContentKind;
use diskedit_core::table::gpt::entries::PartitionEntry;
use diskedit_core::table::gpt::header::GptHeader;
use diskedit_core::table::gpt::parse::{GptEvidence, classify};
use diskedit_core::table::mbr::extended::walk_extended;
use diskedit_core::table::mbr::primary::{self as mbr_primary, MbrEntry};
use diskedit_core::table::{Guid, MAX_ENTRIES, PRIMARY_HEADER_LBA, TableError};

use crate::platform::{PlatformError, PlatformResult, TargetHandle, probe};

/// 一个分区在目标上的视图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionView {
    pub number: u32,
    pub first_lba: u64,
    pub last_lba: u64,
    pub type_text: String,
    pub name: String,
    /// 分区起点字节，供内容探测使用。
    pub base_bytes: u64,
    /// 分区字节数。
    pub bytes: u64,
    pub content: ContentKind,
}

/// GPT 视图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GptView {
    pub sector_size: u64,
    pub disk_guid: String,
    pub last_usable_lba: u64,
    pub partitions: Vec<PartitionView>,
}

/// MBR 视图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MbrView {
    pub sector_size: u64,
    pub partitions: Vec<PartitionView>,
}

/// 目标的表形态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableView {
    /// 没有可识别的分区表。
    None,
    Gpt(GptView),
    Mbr(MbrView),
    /// 存在表签名但两份副本都不可用。
    Damaged {
        sector_size: u64,
        why: String,
    },
}

impl TableView {
    pub fn partitions(&self) -> &[PartitionView] {
        match self {
            Self::Gpt(view) => &view.partitions,
            Self::Mbr(view) => &view.partitions,
            Self::None | Self::Damaged { .. } => &[],
        }
    }

    pub fn sector_size(&self) -> Option<u64> {
        match self {
            Self::Gpt(view) => Some(view.sector_size),
            Self::Mbr(view) => Some(view.sector_size),
            Self::Damaged { sector_size, .. } => Some(*sector_size),
            Self::None => None,
        }
    }

    pub fn find(&self, number: u32) -> Option<&PartitionView> {
        self.partitions()
            .iter()
            .find(|partition| partition.number == number)
    }

    pub fn last(&self) -> Option<&PartitionView> {
        self.partitions().last()
    }
}

/// 一次 GPT 读取的结论。
enum GptRead {
    Hit(GptView),
    Absent,
    Damaged(String),
}

/// 观察目标的表；`preferred` 是用户显式给出的逻辑块大小。
pub fn observe(
    handle: &dyn TargetHandle,
    preferred: Option<u64>,
    kernel_sector: Option<u64>,
) -> PlatformResult<TableView> {
    let len = handle.len();
    let mut damaged: Option<(u64, String)> = None;
    for sector_size in candidate_sizes(preferred, kernel_sector, len) {
        match read_gpt(handle, sector_size)? {
            GptRead::Hit(view) => return Ok(TableView::Gpt(view)),
            GptRead::Absent => {}
            GptRead::Damaged(why) => damaged = Some((sector_size, why)),
        }
    }
    match read_mbr(handle)? {
        Some(view) => Ok(TableView::Mbr(view)),
        None => Ok(match damaged {
            Some((sector_size, why)) => TableView::Damaged { sector_size, why },
            None => TableView::None,
        }),
    }
}

/// 尝试的顺序：用户值、内核值、512、4096、2048、1024；去重且只保留目标能容纳的。
fn candidate_sizes(preferred: Option<u64>, kernel: Option<u64>, len: u64) -> Vec<u64> {
    let mut sizes = Vec::new();
    for size in preferred
        .into_iter()
        .chain(kernel)
        .chain([512, 4096, 2048, 1024])
    {
        if size >= 512 && size.is_power_of_two() && len >= size && !sizes.contains(&size) {
            sizes.push(size);
        }
    }
    sizes
}

fn read_sector(handle: &dyn TargetHandle, lba: u64, sector_size: u64) -> PlatformResult<Vec<u8>> {
    let offset = lba
        .checked_mul(sector_size)
        .ok_or_else(|| PlatformError::parse("sector offset overflow"))?;
    let mut buffer = vec![0u8; sector_size as usize];
    read_exact(handle, offset, &mut buffer)?;
    Ok(buffer)
}

fn read_exact(handle: &dyn TargetHandle, offset: u64, buffer: &mut [u8]) -> PlatformResult<()> {
    let mut filled = 0;
    while filled < buffer.len() {
        let at = offset
            .checked_add(filled as u64)
            .ok_or_else(|| PlatformError::parse("read offset overflow"))?;
        let read = handle.read_at(at, &mut buffer[filled..])?;
        if read == 0 {
            return Err(PlatformError::io(
                handle.path(),
                std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
            ));
        }
        filled += read;
    }
    Ok(())
}

/// 读一份 GPT；签名在场但不可用时给出原因。
fn read_gpt(handle: &dyn TargetHandle, sector_size: u64) -> PlatformResult<GptRead> {
    let last_lba = handle.len() / sector_size;
    if last_lba < 2 {
        return Ok(GptRead::Absent);
    }
    let last_lba = last_lba - 1;
    let header_bytes = read_sector(handle, PRIMARY_HEADER_LBA, sector_size)?;
    let header = match GptHeader::parse(&header_bytes) {
        Ok(header) => header,
        Err(TableError::MissingSignature) => return Ok(GptRead::Absent),
        Err(error) => return Ok(GptRead::Damaged(format!("primary header: {error:?}"))),
    };
    let array_bytes = header
        .number_of_partition_entries
        .checked_mul(header.size_of_partition_entry)
        .ok_or_else(|| PlatformError::parse("GPT entry array size overflow"))?;
    if array_bytes == 0 || header.number_of_partition_entries > MAX_ENTRIES {
        return Ok(GptRead::Damaged(
            "GPT entry array is out of range".to_owned(),
        ));
    }
    let array_lba = header.partition_entry_lba;
    let primary_array = read_bytes(handle, array_lba * sector_size, array_bytes as usize)?;
    let span = (array_bytes as u64).div_ceil(sector_size);
    let backup_header_offset = last_lba * sector_size;
    let backup_header = read_or_empty(handle, backup_header_offset, sector_size as usize)?;
    let backup_array_lba = last_lba.saturating_sub(span);
    let backup_array = read_or_empty(handle, backup_array_lba * sector_size, array_bytes as usize)?;
    match classify(
        &header_bytes,
        &primary_array,
        &backup_header,
        &backup_array,
        last_lba,
        sector_size,
    ) {
        GptEvidence::Absent => Ok(GptRead::Absent),
        GptEvidence::Damaged(error) => Ok(GptRead::Damaged(format!("GPT: {error:?}"))),
        GptEvidence::Usable(copy) => {
            let partitions = copy
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| !entry.partition_type_guid.is_zero())
                .map(|(index, entry)| gpt_partition(handle, sector_size, index, entry))
                .collect::<PlatformResult<Vec<_>>>()?;
            Ok(GptRead::Hit(GptView {
                sector_size,
                disk_guid: guid_text(copy.header.disk_guid),
                last_usable_lba: copy.header.last_usable_lba,
                partitions,
            }))
        }
    }
}

fn gpt_partition(
    handle: &dyn TargetHandle,
    sector_size: u64,
    index: usize,
    entry: &PartitionEntry,
) -> PlatformResult<PartitionView> {
    let first_lba = entry.starting_lba;
    let last_lba = entry.ending_lba;
    let bytes = last_lba
        .saturating_sub(first_lba)
        .saturating_add(1)
        .saturating_mul(sector_size);
    let base_bytes = first_lba.saturating_mul(sector_size);
    let content = probe::identify(handle, base_bytes, bytes)?;
    Ok(PartitionView {
        number: u32::try_from(index + 1).unwrap_or(u32::MAX),
        first_lba,
        last_lba,
        type_text: guid_text(entry.partition_type_guid),
        name: entry.name.to_string_lossy(),
        base_bytes,
        bytes,
        content,
    })
}

fn read_bytes(handle: &dyn TargetHandle, offset: u64, len: usize) -> PlatformResult<Vec<u8>> {
    let mut buffer = vec![0u8; len];
    read_exact(handle, offset, &mut buffer)?;
    Ok(buffer)
}

fn read_or_empty(handle: &dyn TargetHandle, offset: u64, len: usize) -> PlatformResult<Vec<u8>> {
    if offset.saturating_add(len as u64) > handle.len() {
        return Ok(Vec::new());
    }
    read_bytes(handle, offset, len)
}

fn read_mbr(handle: &dyn TargetHandle) -> PlatformResult<Option<MbrView>> {
    let sector = read_or_empty(handle, 0, 512)?;
    if sector.len() < 512 {
        return Ok(None);
    }
    let mbr = match mbr_primary::parse(&sector) {
        Ok(mbr) => mbr,
        Err(TableError::MbrSignatureMissing) => return Ok(None),
        Err(error) => return Err(PlatformError::parse(format!("MBR: {error:?}"))),
    };
    if is_protective(&mbr) {
        // 保护性 MBR 只是 GPT 的占位；GPT 不可用时不算 MBR 表。
        return Ok(None);
    }
    let mut partitions = Vec::new();
    for (index, entry) in mbr.entries.iter().enumerate() {
        if entry.is_empty() || entry.is_extended() {
            continue;
        }
        partitions.push(mbr_partition(
            handle,
            u32::try_from(index + 1).unwrap_or(u32::MAX),
            *entry,
        )?);
    }
    if let Some(extended) = mbr.extended() {
        let logicals = walk_extended(extended.start_lba.into(), |lba| {
            let offset = lba * 512;
            let mut buffer = vec![0u8; 512];
            read_exact(handle, offset, &mut buffer)
                .map_err(|_| TableError::InvalidGeometry("cannot read extended boot record"))?;
            Ok(buffer)
        })
        .map_err(|error| PlatformError::parse(format!("MBR extended chain: {error:?}")))?;
        for (index, entry) in logicals.into_iter().enumerate() {
            partitions.push(mbr_partition(
                handle,
                u32::try_from(index + 5).unwrap_or(u32::MAX),
                entry,
            )?);
        }
    }
    Ok(Some(MbrView {
        sector_size: 512,
        partitions,
    }))
}

fn mbr_partition(
    handle: &dyn TargetHandle,
    number: u32,
    entry: MbrEntry,
) -> PlatformResult<PartitionView> {
    let first_lba = u64::from(entry.start_lba);
    let bytes = u64::from(entry.size_lba) * 512;
    let base_bytes = first_lba * 512;
    let content = probe::identify(handle, base_bytes, bytes)?;
    Ok(PartitionView {
        number,
        first_lba,
        last_lba: first_lba
            .saturating_add(u64::from(entry.size_lba))
            .saturating_sub(1),
        type_text: format!("{:02X}", entry.part_type),
        name: String::new(),
        base_bytes,
        bytes,
        content,
    })
}

fn is_protective(mbr: &mbr_primary::Mbr) -> bool {
    mbr.entries
        .iter()
        .filter(|entry| !entry.is_empty())
        .all(|entry| entry.part_type == 0xEE)
}

/// GPT GUID 的规范文本：前三组按小端还原。
pub fn guid_text(guid: Guid) -> String {
    let b = guid.0;
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_renders_the_canonical_mixed_endian_form() {
        let guid = Guid([
            0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e,
            0xc9, 0x3b,
        ]);
        assert_eq!(guid_text(guid), "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
    }

    #[test]
    fn candidate_order_is_preferred_then_kernel_then_defaults() {
        assert_eq!(
            candidate_sizes(Some(4096), Some(512), 1 << 20),
            vec![4096, 512, 2048, 1024]
        );
        assert_eq!(
            candidate_sizes(None, None, 4096),
            vec![512, 4096, 2048, 1024]
        );
    }
}
