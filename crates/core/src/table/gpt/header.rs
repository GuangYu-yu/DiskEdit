//! GPT 头：固定字段的解析与序列化，CRC 覆盖 `header_size` 字节。

use super::super::{GPT_MIN_HEADER_SIZE, Guid, TableError, crc32};

const SIGNATURE: &[u8; 8] = b"EFI PART";
const OFFSET_REVISION: usize = 8;
const OFFSET_HEADER_SIZE: usize = 12;
const OFFSET_HEADER_CRC: usize = 16;
const OFFSET_MY_LBA: usize = 24;
const OFFSET_ALTERNATE_LBA: usize = 32;
const OFFSET_FIRST_USABLE: usize = 40;
const OFFSET_LAST_USABLE: usize = 48;
const OFFSET_DISK_GUID: usize = 56;
const OFFSET_ENTRY_LBA: usize = 72;
const OFFSET_ENTRY_COUNT: usize = 80;
const OFFSET_ENTRY_SIZE: usize = 84;
const OFFSET_ARRAY_CRC: usize = 88;

/// GPT 头中参与解析与重写的全部语义字段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GptHeader {
    pub revision: u32,
    pub primary_lba: u64,
    pub backup_lba: u64,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub disk_guid: Guid,
    pub partition_entry_lba: u64,
    pub number_of_partition_entries: u32,
    pub size_of_partition_entry: u32,
    pub header_size: u32,
    pub entry_array_crc32: u32,
}

impl GptHeader {
    /// 解析一个扇区中的 GPT 头；`header_size` 不大于扇区长度。
    pub fn parse(sector: &[u8]) -> Result<Self, TableError> {
        if sector.len() < GPT_MIN_HEADER_SIZE as usize {
            return Err(TableError::BufferTooSmall {
                needed: GPT_MIN_HEADER_SIZE as usize,
                actual: sector.len(),
            });
        }
        if &sector[..SIGNATURE.len()] != SIGNATURE {
            return Err(TableError::MissingSignature);
        }
        let header_size = read_u32(sector, OFFSET_HEADER_SIZE);
        if header_size < GPT_MIN_HEADER_SIZE || header_size as usize > sector.len() {
            return Err(TableError::InvalidHeaderSize(header_size));
        }
        let stored_crc = read_u32(sector, OFFSET_HEADER_CRC);
        let mut checked = sector[..header_size as usize].to_vec();
        checked[OFFSET_HEADER_CRC..OFFSET_HEADER_CRC + 4].fill(0);
        if crc32(&checked) != stored_crc {
            return Err(TableError::HeaderCrcMismatch);
        }
        Ok(Self {
            revision: read_u32(sector, OFFSET_REVISION),
            primary_lba: read_u64(sector, OFFSET_MY_LBA),
            backup_lba: read_u64(sector, OFFSET_ALTERNATE_LBA),
            first_usable_lba: read_u64(sector, OFFSET_FIRST_USABLE),
            last_usable_lba: read_u64(sector, OFFSET_LAST_USABLE),
            disk_guid: Guid(read_guid(sector, OFFSET_DISK_GUID)),
            partition_entry_lba: read_u64(sector, OFFSET_ENTRY_LBA),
            number_of_partition_entries: read_u32(sector, OFFSET_ENTRY_COUNT),
            size_of_partition_entry: read_u32(sector, OFFSET_ENTRY_SIZE),
            header_size,
            entry_array_crc32: read_u32(sector, OFFSET_ARRAY_CRC),
        })
    }

    /// 序列化为 `sector_len` 字节的整扇区，保留区清零，CRC 按 `header_size` 计算。
    pub fn serialize(&self, sector_len: usize) -> Result<Vec<u8>, TableError> {
        if (self.header_size as usize) < GPT_MIN_HEADER_SIZE as usize
            || self.header_size as usize > sector_len
        {
            return Err(TableError::InvalidHeaderSize(self.header_size));
        }
        let mut out = vec![0u8; sector_len];
        out[..SIGNATURE.len()].copy_from_slice(SIGNATURE);
        write_u32(&mut out, OFFSET_REVISION, self.revision);
        write_u32(&mut out, OFFSET_HEADER_SIZE, self.header_size);
        write_u64(&mut out, OFFSET_MY_LBA, self.primary_lba);
        write_u64(&mut out, OFFSET_ALTERNATE_LBA, self.backup_lba);
        write_u64(&mut out, OFFSET_FIRST_USABLE, self.first_usable_lba);
        write_u64(&mut out, OFFSET_LAST_USABLE, self.last_usable_lba);
        out[OFFSET_DISK_GUID..OFFSET_DISK_GUID + 16].copy_from_slice(&self.disk_guid.0);
        write_u64(&mut out, OFFSET_ENTRY_LBA, self.partition_entry_lba);
        write_u32(
            &mut out,
            OFFSET_ENTRY_COUNT,
            self.number_of_partition_entries,
        );
        write_u32(&mut out, OFFSET_ENTRY_SIZE, self.size_of_partition_entry);
        write_u32(&mut out, OFFSET_ARRAY_CRC, self.entry_array_crc32);
        let crc = crc32(&out[..self.header_size as usize]);
        write_u32(&mut out, OFFSET_HEADER_CRC, crc);
        Ok(out)
    }
}

fn read_u32(buffer: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buffer[offset..offset + 4].try_into().expect("偏移已校验"))
}

fn read_u64(buffer: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(buffer[offset..offset + 8].try_into().expect("偏移已校验"))
}

fn read_guid(buffer: &[u8], offset: usize) -> [u8; 16] {
    buffer[offset..offset + 16].try_into().expect("偏移已校验")
}

fn write_u32(buffer: &mut [u8], offset: usize, value: u32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(buffer: &mut [u8], offset: usize, value: u64) {
    buffer[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GptHeader {
        GptHeader {
            revision: 0x0001_0000,
            primary_lba: 1,
            backup_lba: 1023,
            first_usable_lba: 34,
            last_usable_lba: 990,
            disk_guid: Guid([7; 16]),
            partition_entry_lba: 2,
            number_of_partition_entries: 128,
            size_of_partition_entry: 128,
            header_size: 92,
            entry_array_crc32: 42,
        }
    }

    #[test]
    fn round_trip_preserves_fields() {
        let header = sample();
        let bytes = header.serialize(512).expect("序列化应成功");
        assert_eq!(GptHeader::parse(&bytes), Ok(header));
    }

    #[test]
    fn detects_signature_and_size_damage() {
        let bytes = sample().serialize(512).expect("序列化应成功");
        let mut no_signature = bytes.clone();
        no_signature[0] = b'X';
        assert_eq!(
            GptHeader::parse(&no_signature),
            Err(TableError::MissingSignature)
        );

        let mut short = bytes.clone();
        short[OFFSET_HEADER_SIZE..OFFSET_HEADER_SIZE + 4].copy_from_slice(&80u32.to_le_bytes());
        assert_eq!(
            GptHeader::parse(&short),
            Err(TableError::InvalidHeaderSize(80))
        );
    }

    #[test]
    fn detects_crc_damage() {
        let bytes = sample().serialize(512).expect("序列化应成功");
        let mut damaged = bytes.clone();
        damaged[OFFSET_FIRST_USABLE] ^= 0xFF;
        assert_eq!(
            GptHeader::parse(&damaged),
            Err(TableError::HeaderCrcMismatch)
        );
    }

    #[test]
    fn crc_covers_header_size_not_whole_sector() {
        let header = sample();
        let bytes = header.serialize(4096).expect("序列化应成功");
        let mut changed_tail = bytes.clone();
        changed_tail[200] = 0xAA;
        assert_eq!(GptHeader::parse(&changed_tail), Ok(header));
    }

    #[test]
    fn rejects_truncated_sector() {
        assert_eq!(
            GptHeader::parse(&[0u8; 16]),
            Err(TableError::BufferTooSmall {
                needed: GPT_MIN_HEADER_SIZE as usize,
                actual: 16
            })
        );
    }
}
