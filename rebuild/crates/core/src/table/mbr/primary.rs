//! MBR 主表：LBA0 前 512 字节内的四条分区记录与 0x55AA 签名。

use super::super::TableError;

/// MBR 分区记录固定的字节数。
pub const MBR_ENTRY_BYTES: usize = 16;
/// 主分区记录在 LBA0 内的起始偏移。
pub const MBR_ENTRY_BASE: usize = 446;
/// 引导签名所在偏移。
pub const MBR_SIGNATURE_OFFSET: usize = 510;

/// 扩展分区类型。
pub const TYPE_EXTENDED: u8 = 0x05;
/// 扩展分区类型（LBA 寻址）。
pub const TYPE_EXTENDED_LBA: u8 = 0x0F;
/// 扩展分区类型（Linux）。
pub const TYPE_EXTENDED_LINUX: u8 = 0x85;
/// GPT 保护分区类型。
pub const TYPE_PROTECTIVE: u8 = 0xEE;

/// 一条 MBR 分区记录。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MbrEntry {
    pub boot: u8,
    pub part_type: u8,
    pub start_lba: u32,
    pub size_lba: u32,
}

impl MbrEntry {
    pub fn is_extended(self) -> bool {
        matches!(
            self.part_type,
            TYPE_EXTENDED | TYPE_EXTENDED_LBA | TYPE_EXTENDED_LINUX
        )
    }

    pub fn is_empty(self) -> bool {
        self.part_type == 0 && self.size_lba == 0
    }
}

/// 解析出的主 MBR。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mbr {
    pub entries: [MbrEntry; 4],
}

impl Mbr {
    pub fn extended(&self) -> Option<MbrEntry> {
        self.entries
            .iter()
            .copied()
            .find(|entry| entry.is_extended())
    }

    pub fn primary_partitions(&self) -> impl Iterator<Item = MbrEntry> + '_ {
        self.entries
            .iter()
            .copied()
            .filter(|entry| !entry.is_empty() && !entry.is_extended())
    }
}

/// 解析 LBA0：要求至少 512 字节且带 0x55AA 签名。
pub fn parse(sector: &[u8]) -> Result<Mbr, TableError> {
    if sector.len() < MBR_SIGNATURE_OFFSET + 2 {
        return Err(TableError::BufferTooSmall {
            needed: MBR_SIGNATURE_OFFSET + 2,
            actual: sector.len(),
        });
    }
    if sector[MBR_SIGNATURE_OFFSET] != 0x55 || sector[MBR_SIGNATURE_OFFSET + 1] != 0xAA {
        return Err(TableError::MbrSignatureMissing);
    }
    let mut entries = [MbrEntry::default(); 4];
    for (index, slot) in entries.iter_mut().enumerate() {
        let offset = MBR_ENTRY_BASE + index * MBR_ENTRY_BYTES;
        *slot = parse_entry(&sector[offset..offset + MBR_ENTRY_BYTES]);
    }
    Ok(Mbr { entries })
}

/// 把四个条目写回扇区，保留引导代码与其余字节，并置 0x55AA 签名。
pub fn write_into(mbr: &Mbr, sector: &mut [u8; 512]) {
    for (index, entry) in mbr.entries.iter().enumerate() {
        let offset = MBR_ENTRY_BASE + index * MBR_ENTRY_BYTES;
        let raw = &mut sector[offset..offset + MBR_ENTRY_BYTES];
        raw.fill(0);
        raw[0] = entry.boot;
        raw[4] = entry.part_type;
        raw[8..12].copy_from_slice(&entry.start_lba.to_le_bytes());
        raw[12..16].copy_from_slice(&entry.size_lba.to_le_bytes());
    }
    sector[MBR_SIGNATURE_OFFSET] = 0x55;
    sector[MBR_SIGNATURE_OFFSET + 1] = 0xAA;
}

fn parse_entry(raw: &[u8]) -> MbrEntry {
    MbrEntry {
        boot: raw[0],
        part_type: raw[4],
        start_lba: u32::from_le_bytes(raw[8..12].try_into().expect("偏移已校验")),
        size_lba: u32::from_le_bytes(raw[12..16].try_into().expect("偏移已校验")),
    }
}

/// 规范的保护 MBR SizeInLBA：整盘 LBA 数减一，超出 32 位按饱和写。
pub fn size_in_lba(total_lba: u64) -> u32 {
    total_lba.saturating_sub(1).min(u32::MAX as u64) as u32
}

/// 生成覆盖整盘的保护性 MBR（UEFI 保护 MBR 形态）。
pub fn protective_mbr(total_lba: u64) -> [u8; 512] {
    let mut sector = [0u8; 512];
    sector[MBR_ENTRY_BASE + 4] = TYPE_PROTECTIVE;
    sector[MBR_ENTRY_BASE + 8..MBR_ENTRY_BASE + 12].copy_from_slice(&1u32.to_le_bytes());
    sector[MBR_ENTRY_BASE + 12..MBR_ENTRY_BASE + 16]
        .copy_from_slice(&size_in_lba(total_lba).to_le_bytes());
    sector[MBR_SIGNATURE_OFFSET] = 0x55;
    sector[MBR_SIGNATURE_OFFSET + 1] = 0xAA;
    sector
}

/// 判断一条记录是否为保护分区：类型 0xEE 且覆盖从 LBA1 起的整盘。
pub fn is_protective(entry: MbrEntry, total_lba: u64) -> bool {
    entry.part_type == TYPE_PROTECTIVE
        && entry.start_lba == 1
        && entry.size_lba == size_in_lba(total_lba)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sector_with(entries: &[(u8, u32, u32)]) -> [u8; 512] {
        let mut sector = [0u8; 512];
        for (index, &(part_type, start_lba, size_lba)) in entries.iter().enumerate() {
            let offset = MBR_ENTRY_BASE + index * MBR_ENTRY_BYTES;
            sector[offset + 4] = part_type;
            sector[offset + 8..offset + 12].copy_from_slice(&start_lba.to_le_bytes());
            sector[offset + 12..offset + 16].copy_from_slice(&size_lba.to_le_bytes());
        }
        sector[MBR_SIGNATURE_OFFSET] = 0x55;
        sector[MBR_SIGNATURE_OFFSET + 1] = 0xAA;
        sector
    }

    #[test]
    fn parses_primary_entries() {
        let sector = sector_with(&[(0x83, 2048, 4096), (0x82, 8192, 1024)]);
        let mbr = parse(&sector).expect("应可解析");
        let partitions: Vec<_> = mbr.primary_partitions().collect();
        assert_eq!(partitions.len(), 2);
        assert_eq!(partitions[0].start_lba, 2048);
        assert_eq!(partitions[1].part_type, 0x82);
    }

    #[test]
    fn rejects_missing_signature_and_short_buffer() {
        let mut sector = sector_with(&[]);
        sector[510] = 0;
        assert_eq!(parse(&sector), Err(TableError::MbrSignatureMissing));
        assert!(matches!(
            parse(&[0u8; 100]),
            Err(TableError::BufferTooSmall { .. })
        ));
    }

    #[test]
    fn detects_extended_entries() {
        let sector = sector_with(&[(0x83, 2048, 100), (0x0F, 4096, 1000)]);
        let mbr = parse(&sector).expect("应可解析");
        assert_eq!(mbr.extended().map(|entry| entry.part_type), Some(0x0F));
    }

    #[test]
    fn protective_mbr_covers_disk() {
        let sector = protective_mbr(1_000_000);
        let mbr = parse(&sector).expect("保护 MBR 应可解析");
        assert!(is_protective(mbr.entries[0], 1_000_000));
        assert_eq!(mbr.entries[0].size_lba, 999_999);
    }

    #[test]
    fn size_saturates_for_large_disks() {
        assert_eq!(size_in_lba(0), 0);
        assert_eq!(size_in_lba(1), 0);
        assert_eq!(size_in_lba(u64::MAX), u32::MAX);
    }
}
