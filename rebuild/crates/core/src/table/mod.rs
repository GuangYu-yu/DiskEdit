//! 分区表：GPT 与 MBR 的纯编解码、CRC 与副本分类。

pub mod create;
pub mod edit;
pub mod error;
pub mod gpt;
pub mod mbr;
pub mod relocate;
pub mod stamp;

pub use create::{
    CapacityError, Conflict, Format, GptContent, GptLayout, LabeledRegion, PartitionSpan,
    PmbrIssue, PmbrSize, Segment, TableProbe, check_capacity, first_conflict, gpt_content,
    gpt_layout, new_mbr_lba0, pmbr_size_state, protective_mbr_content, regions_blank,
    signature_sites,
};
pub use edit::{EditError, GptFlag, GptTable, MbrFlag, first_fit, fixed_start};
pub use error::TableError;
pub use gpt::entries::{PartitionEntry, PartitionName};
pub use gpt::header::GptHeader;
pub use gpt::parse::{GptCopy, GptEvidence, GptState, HeaderIssue};
pub use mbr::extended::walk_extended;
pub use mbr::primary::{Mbr, MbrEntry};
pub use relocate::{ChunkSpan, Layout, Resume, layout, relocation_overlaps, resume};
pub use stamp::table_stamp;

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

    /// 由标准文本解析：8-4-4-4-12（连字符可省略），大小写不限。
    ///
    /// 文本的前三组按大端写、前三个字段在盘上按小端存，这里完成这次换序；
    /// 形状不符或含非十六进制字符一律返回 `None`。
    pub fn parse(text: &str) -> Option<Self> {
        let bare = text.len() == 32 && text.bytes().all(|byte| byte.is_ascii_hexdigit());
        let hyphened = text.len() == 36
            && text
                .match_indices('-')
                .map(|(index, _)| index)
                .eq([8, 13, 18, 23])
            && text
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-');
        if !bare && !hyphened {
            return None;
        }
        let mut digits = [0u8; 16];
        let mut cursor = 0;
        let bytes = text.as_bytes();
        for slot in &mut digits {
            while cursor < bytes.len() && bytes[cursor] == b'-' {
                cursor += 1;
            }
            let high = hex_value(bytes.get(cursor)?)?;
            let low = hex_value(bytes.get(cursor + 1)?)?;
            *slot = (high << 4) | low;
            cursor += 2;
        }
        let mut out = [0u8; 16];
        out[0] = digits[3];
        out[1] = digits[2];
        out[2] = digits[1];
        out[3] = digits[0];
        out[4] = digits[5];
        out[5] = digits[4];
        out[6] = digits[7];
        out[7] = digits[6];
        out[8..].copy_from_slice(&digits[8..]);
        Some(Self(out))
    }

    /// 规范文本：前三组按盘上小端还原。
    pub fn to_text(self) -> String {
        let b = self.0;
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
}

fn hex_value(byte: &u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_text_round_trips_through_the_disk_order() {
        let text = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
        let guid = Guid::parse(text).expect("合法 GUID 文本");
        assert_eq!(guid.to_text(), text);
        assert_eq!(
            guid.0,
            [
                0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e,
                0xc9, 0x3b,
            ]
        );
        assert_eq!(Guid::parse("c12a7328f81f11d2ba4b00a0c93ec93b"), Some(guid));
    }

    #[test]
    fn guid_parse_rejects_wrong_shapes() {
        for text in [
            "",
            "C12A7328-F81F-11D2-BA4B-00A0C93EC93",
            "C12A7328F81F-11D2-BA4B-00A0C93EC93B",
            "G12A7328-F81F-11D2-BA4B-00A0C93EC93B",
            "0x83",
        ] {
            assert!(Guid::parse(text).is_none(), "{text}");
        }
    }
}
