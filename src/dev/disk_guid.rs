//! 目标盘面自述的 Disk GUID 凭据：编码与比较的唯一判据。

use std::io;

/// 凭据段长度：1 字节种类 + 16 字节 GUID
pub(crate) const CREDENTIAL_LEN: usize = 1 + 16;

/// 目标盘面自述的 Disk GUID（GPT 头 56..72，UEFI 2.10 §5.3.2 Table 5.5）的三态。
///
/// `GeneratedThisOperation` 表示"这张表由写下这条记录的同一个操作生出"。重写前后的
/// GUID 都由本操作自己产生，不是外部证据：两种取值都不参与比对。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskGuidState {
    /// 目标上没有 GPT（MBR / 裸盘）：凭据不可得。比较时它不是证据，也绝不算相符
    Unavailable,
    /// 读到 GPT 头里的 Disk GUID
    Present([u8; 16]),
    /// 写下这条记录的操作本次造出了整张表：GUID 由它产生，前后值都不作为身份证据
    GeneratedThisOperation,
}

impl DiskGuidState {
    const TAG_UNAVAILABLE: u8 = 0;
    const TAG_PRESENT: u8 = 1;
    const TAG_GENERATED: u8 = 2;

    fn tag(self) -> u8 {
        match self {
            Self::Unavailable => Self::TAG_UNAVAILABLE,
            Self::Present(_) => Self::TAG_PRESENT,
            Self::GeneratedThisOperation => Self::TAG_GENERATED,
        }
    }

    pub(crate) fn encode(self) -> [u8; CREDENTIAL_LEN] {
        let mut out = [0u8; CREDENTIAL_LEN];
        out[0] = self.tag();
        if let Self::Present(g) = self {
            out[1..].copy_from_slice(&g);
        }
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        // 记录字节来自盘上文件：截断或畸形（CRC 一致也可能）必须落错误而不是 panic
        let Some((&tag, rest)) = bytes.split_first() else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "disk identity is empty"));
        };
        let guid: [u8; 16] = rest.try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "disk identity GUID is not 16 bytes")
        })?;
        match tag {
            Self::TAG_UNAVAILABLE => Ok(Self::Unavailable),
            Self::TAG_PRESENT => Ok(Self::Present(guid)),
            Self::TAG_GENERATED => Ok(Self::GeneratedThisOperation),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "implausible disk-identity kind",
            )),
        }
    }
}

/// 凭据比对的唯一判据。`Some(why)` = 不符；`None` = 不提供额外证据——不可得与本次生成
/// 都不是"相符"，只是没有可比的值。`Unavailable` 与 `Present([0; 16])` 是两种不同的
/// 取值，在数据与代码里都不会混为一谈
pub(crate) fn disk_identity_mismatch(
    recorded: DiskGuidState,
    current: DiskGuidState,
) -> Option<String> {
    let (DiskGuidState::Present(a), DiskGuidState::Present(b)) = (recorded, current) else {
        return match (recorded, current) {
            (DiskGuidState::Present(_), DiskGuidState::Unavailable) => Some(
                "the record states a GPT disk GUID for the target, but the target now carries no readable GPT"
                    .to_string(),
            ),
            (DiskGuidState::Unavailable, DiskGuidState::Present(_)) => Some(
                "the record was written for a target without a GPT, but the target now carries a GPT with its own \
                 disk GUID"
                    .to_string(),
            ),
            // GeneratedThisOperation 在记录侧是"本操作自己造的表"，在目标侧永远不会出现；
            // 两侧都不可得也不是证据
            _ => None,
        };
    };
    (a != b).then(|| {
        format!(
            "the record states GPT disk GUID {} but the target now reports {}",
            crate::support::hex_guid(&a),
            crate::support::hex_guid(&b)
        )
    })
}
