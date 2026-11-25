//! 分区表：GPT 与 MBR 的纯编解码、CRC 与副本分类。

pub mod create;
pub mod error;
pub mod gpt;
pub mod mbr;
pub mod stamp;

pub use create::{
    CapacityError, Conflict, Format, GptContent, GptLayout, LabeledRegion, PartitionSpan, PmbrIssue,
    PmbrSize, Segment, TableProbe, check_capacity, first_conflict, gpt_content, gpt_layout,
    new_mbr_lba0, pmbr_size_state, protective_mbr_content, regions_blank, signature_sites,
};
pub use error::TableError;
pub use stamp::table_stamp;
pub use gpt::entries::{PartitionEntry, PartitionName};
pub use gpt::header::GptHeader;
pub use gpt::parse::{GptCopy, GptEvidence, GptState, HeaderIssue};
pub use mbr::extended::walk_extended;
pub use mbr::primary::{Mbr, MbrEntry};

/// GPT 头固定字段的字节数。
pub const GPT_MIN_HEADER_SIZE: u32 = 92;
/// 单个分区条目的最小字节数。
pub const ENTRY_MIN_BYTES: usize = 128;
/// 条目内分区名的字节数（UTF-16LE）。
pub const ENTRY_NAME_BYTES: usize = 72;
/// 主头所在 LBA。
pub const PRIMARY_HEADER_LBA: u64 = 1;
/// 主条目数组起点 LBA。
pub const PRIMARY_ARRAY_LBA: u64 = 2;
/// 实现允许的最大分区条目数。
pub const MAX_ENTRIES: u32 = 128;

/// 16 字节 GUID，按盘上字节序原样保存。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    pub const ZERO: Guid = Guid([0u8; 16]);

    pub fn is_zero(self) -> bool {
        self.0 == [0u8; 16]
    }
}

impl From<[u8; 16]> for Guid {
    fn from(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

impl AsRef<[u8]> for Guid {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// GPT 使用的 CRC-32/ISO-HDLC（与 UEFI 规范一致）。
pub fn crc32(data: &[u8]) -> u32 {
    let crc = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
    let mut digest = crc.digest();
    digest.update(data);
    digest.finalize()
}
