//! 平移后修正 NTFS 引导扇区里的 `HiddenSectors`。
//!
//! NTFS 的引导扇区把"本卷起点相对磁盘起点的扇区数"写在偏移 `0x1C`。卷被平移到
//! 新位置后这个值就过期了：Windows 用它与分区表对账，不一致时拒绝挂载。卷尾的
//! 备份引导扇区保存同一份值，即使主扇区改对了，只修主扇区也会让 NTFS 检查器判
//! 备份损坏。
//!
//! 两个扇区都在搬移步骤声明的目标区间内；先读出并核对两处的 NTFS 签名，再改写
//! 各自的 4 字节。签名不符时不写任何字节——这说明目标内容不是可识别的 NTFS，
//! 继续写会把非 NTFS 数据改坏。

use diskedit_core::region::Region;

use super::{ExecError, WriteChannel};
use crate::app::execute::token::Recorded;

/// NTFS 引导扇区签名在扇区内的起始偏移。
const SIGNATURE_OFFSET: usize = 3;
/// NTFS 引导扇区签名。
const SIGNATURE: &[u8; 8] = b"NTFS    ";
/// `HiddenSectors` 在引导扇区内的偏移。
const HIDDEN_SECTORS_OFFSET: usize = 0x1C;
/// `HiddenSectors` 的字节数。
const HIDDEN_SECTORS_LEN: usize = 4;

/// 把目标区间首尾两个引导扇区的 `HiddenSectors` 改成新区间的起点。
pub fn fix_ntfs_hidden(
    channel: &mut WriteChannel<'_>,
    token: &Recorded,
    destination: Region,
    sector: u64,
) -> Result<(), ExecError> {
    let sector_len = usize::try_from(sector)
        .map_err(|_| ExecError::InvalidPlan("扇区尺寸超出表示范围".to_owned()))?;
    if sector_len < HIDDEN_SECTORS_OFFSET + HIDDEN_SECTORS_LEN
        || destination.len() < sector.saturating_mul(2)
    {
        return Err(ExecError::InvalidPlan(
            "NTFS 引导扇区修正需要至少两个完整扇区".to_owned(),
        ));
    }
    let start = destination.start();
    let backup = destination.end() - sector;
    // Windows 只读低 32 位；起点超出 `u32` 个扇区时写 0，与 NTFS 自身的表示一致。
    let hidden = u32::try_from(start / sector).unwrap_or(0).to_le_bytes();
    let mut patches = Vec::with_capacity(2);
    for offset in [start, backup] {
        let mut boot = channel.read_exact(offset, sector_len)?;
        if boot.get(SIGNATURE_OFFSET..SIGNATURE_OFFSET + SIGNATURE.len()) != Some(SIGNATURE) {
            return Err(ExecError::Postcondition(format!(
                "LBA {} 处不是 NTFS 引导扇区，拒绝改写 HiddenSectors",
                offset / sector
            )));
        }
        boot[HIDDEN_SECTORS_OFFSET..HIDDEN_SECTORS_OFFSET + HIDDEN_SECTORS_LEN]
            .copy_from_slice(&hidden);
        let region = Region::sized(offset, sector)
            .map_err(|error| ExecError::InvalidPlan(format!("引导扇区区间无效: {error:?}")))?;
        patches.push((region, boot));
    }
    for (region, boot) in patches {
        channel.write(token, region, &boot)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ntfs_sector(hidden: u32) -> Vec<u8> {
        let mut sector = vec![0u8; 512];
        sector[SIGNATURE_OFFSET..SIGNATURE_OFFSET + SIGNATURE.len()].copy_from_slice(SIGNATURE);
        sector[HIDDEN_SECTORS_OFFSET..HIDDEN_SECTORS_OFFSET + HIDDEN_SECTORS_LEN]
            .copy_from_slice(&hidden.to_le_bytes());
        sector
    }

    #[test]
    fn recognises_ntfs_signature() {
        assert_eq!(&ntfs_sector(7)[3..11], SIGNATURE);
        let mut other = ntfs_sector(7);
        other[3] = b'X';
        assert_ne!(&other[3..11], SIGNATURE);
    }

    #[test]
    fn hidden_value_sits_at_fixed_offset() {
        let sector = ntfs_sector(0x1234_5678);
        assert_eq!(
            u32::from_le_bytes(sector[0x1C..0x20].try_into().unwrap()),
            0x1234_5678
        );
    }
}
