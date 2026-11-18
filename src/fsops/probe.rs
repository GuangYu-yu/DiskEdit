//! FS 字节数探测：各工具输出格式的解析。

use super::*;

/// ext 最小尺寸估算：resize2fs -P 的最小块数 × dumpe2fs -h 的块大小；
/// 输出格式锚定 man 页示例（"Estimated minimum size of the filesystem: N" / "Block size: N"）。
/// 其余 FS 无已验证的输出格式，返回 None，由工具自身在缩容前拒绝
/// （shrink_fs 先于任何数据搬移执行，失败即安全终止）
pub fn fs_min_bytes(src: &FileSource, part: u32, fstype: Option<ContentKind>) -> Result<Option<u64>, FsError> {
    if fstype != Some(ContentKind::Filesystem(FsKind::Ext)) {
        return Ok(None);
    }
    let mut result: Option<Result<u64, FsError>> = None;
    with_partition_device(src, part, |dev| {
        result = Some(min_bytes_ext(dev));
        Ok::<(), FsError>(())
    })?;
    match result {
        Some(r) => r.map(Some),
        // 闭包在 with_partition_device 内恒执行一次，None 只能是内部不变量破裂：
        // fail-closed 报错误，不冒充"无最小尺寸可报"
        None => Err(FsError::Io(io::Error::other("partition probe did not run (internal invariant broken)"))),
    }
}

/// 工具输出里取不到可用数值的两个互斥原因。"没有这个字段"与"字段在、值不可信"
/// 不是一回事：前者是输出形态不符，后者说明正在读的一个值不能用；两者都拒绝，
/// 但拒绝理由不能塌缩成"看起来像没值"
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NumError {
    /// 输出里没有以该前缀开头的行
    MissingField(String),
    /// 字段存在，但其值不是可用的尺寸
    UnusableValue { prefix: String, raw: String, why: Unusable },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unusable {
    /// 字段值不是以十进制数字开头
    NotDecimal,
    /// 0 不是可用的尺寸
    Zero,
    /// 超出 u64 范围
    Overflow,
    /// 数字后面还跟着非空白字符
    TrailingGarbage,
}

impl Unusable {
    fn text(self) -> &'static str {
        match self {
            Self::NotDecimal => "no decimal digits",
            Self::Zero => "zero",
            Self::Overflow => "beyond the u64 range",
            Self::TrailingGarbage => "characters after the digits",
        }
    }
}

impl std::fmt::Display for NumError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingField(prefix) => write!(f, "tool output has no {prefix:?} field"),
            Self::UnusableValue { prefix, raw, why } => write!(
                f,
                "tool output has {prefix:?} = {raw:?}, which is not a usable size ({})",
                why.text()
            ),
        }
    }
}

impl std::error::Error for NumError {}

/// 同一份输出的解析结果只有一个出口：`io::Error`（只读回读链）与 `FsError`（探测链）
/// 各自的转换都保住上面这条"字段缺失 ≠ 值不可用"的区分
impl From<NumError> for io::Error {
    fn from(e: NumError) -> Self {
        io::Error::other(e)
    }
}

impl From<NumError> for FsError {
    fn from(e: NumError) -> Self {
        FsError::Io(io::Error::other(e))
    }
}

/// 取字段值**开头**的十进制数字。回读用的行里数值后面还跟着人读注解
/// （`Current volume size: 33554944 bytes (34 MB)`），故不能要求整段就是一个数。
/// 数字必须止于行尾或空白——`0x…` 这类前缀不是本字段的十进制值；0 也不是可用的尺寸。
/// 首个匹配前缀的行即该字段的唯一定义，它的值不可用就拒绝，不到后面的行里另找一个
/// 能解析的值（那会把"这一个值读坏了"掩盖成"读出来了另一个数"）
pub(crate) fn parse_num_lead(text: &str, prefix: &str) -> Result<u64, NumError> {
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix(prefix) else { continue };
        let raw = rest.trim_start();
        // ntfsresize --info 的字段写成 `Cluster size       : 4096 bytes`：冒号前的空格数
        // 随标签长度对齐，把冒号也算进前缀会让解析跟着对齐变化；标签作为前缀，冒号在这里跳过
        let raw = raw.strip_prefix(':').map_or(raw, |r| r.trim_start());
        let digits: String = raw.chars().take_while(char::is_ascii_digit).collect();
        let tail = &raw[digits.len()..];
        let parsed = if digits.is_empty() {
            Err(Unusable::NotDecimal)
        } else if !(tail.is_empty() || tail.starts_with(char::is_whitespace)) {
            Err(Unusable::TrailingGarbage)
        } else {
            digits.parse::<u64>().map_err(|_| Unusable::Overflow)
        };
        let value = match parsed {
            Ok(0) => Err(Unusable::Zero),
            Ok(v) => Ok(v),
            Err(why) => Err(why),
        };
        return value.map_err(|why| NumError::UnusableValue {
            prefix: prefix.to_string(),
            raw: raw.to_string(),
            why,
        });
    }
    Err(NumError::MissingField(prefix.to_string()))
}

/// 文件系统尺寸核对结论。缩容与扩容共用：两处都只回答"工具成功返回后，文件系统实际
/// 落在哪儿"，差别在各自的判据——缩容要求落点 ≤ 请求值，扩容要求落点覆盖到区间末端。
/// "回读不出来"在两种方向上都只有一种含义：操作**可能已经完成**，只是落点无法证实。
/// 它不是失败，由各自的运行结果携带成一条待办
pub enum SizeVerify {
    /// 回读到的实际尺寸（字节）
    Confirmed(u64),
    /// 回读不出来（工具缺失、输出解析不了、卷被工具标成待检查而拒答）
    Unverifiable { why: String, hint: String },
}

/// 回读到的文件系统几何：总字节 + 分配单元。
///
/// 分配单元是"扩到区间末端"判据的组成部分：工具按自己的单元放置，实际尺寸与区间
/// 末端之间允许差一个单元（ext 按块组、ntfs 按簇、btrfs 按 sectorsize）
pub(crate) struct FsGeometry {
    pub bytes: u64,
    pub unit: u64,
}

/// 回读文件系统几何。三者都是只读查询：
/// - ext：`dumpe2fs -h`（man dumpe2fs：-h 只打印超级块）的 `Block count:` × `Block size:`，
///   分配单元 = `Block size:` × `Blocks per group:`。resize2fs 按块组放置：末段装不下该组
///   元数据时整段留空（实测 1.47.2 在 400 MiB 卷扩到 900 MiB 时止于 896 MiB），
///   故容差取一个块组而非一个块
/// - ntfs：`ntfsresize --info` 的 `Current volume size:` 与 `Cluster size`。`--info` 是查询，
///   不调整大小，故只读。`-f` 是必需的：真实调整大小会把卷标成"待检查"，不带 `-f` 的
///   `--info` 会以该理由拒答
/// - btrfs：`btrfs inspect-internal dump-super -f`（-f = --full）的 `total_bytes` 与
///   `sectorsize`；后者在磁盘格式里是超级块偏移 0x20 的 "minimal I/O size (sector size)"
///   （btrfs 文档 On-disk-format）。该命令除签名验证外不做全面合理性检查，故只用来报尺寸、
///   不用来判断卷是否健康
///
/// 输出格式非稳定机器接口（随工具版本可能变化），解析失败即报错、不猜默认值
pub(crate) fn read_fs_geometry(kind: FsKind, dev: &str) -> io::Result<FsGeometry> {
    match kind {
        FsKind::Ext => {
            let text = dumpe2fs_text(dev)?;
            Ok(FsGeometry { bytes: ext_bytes(&text)?, unit: ext_unit(&text)? })
        }
        FsKind::Ntfs => {
            let text = ntfsresize_info_text(dev)?;
            Ok(FsGeometry {
                bytes: parse_num_lead(&text, "Current volume size:")?,
                unit: parse_num_lead(&text, "Cluster size")?,
            })
        }
        FsKind::Btrfs => {
            let text = btrfs_superblock_text(dev)?;
            Ok(FsGeometry {
                bytes: parse_num_lead(&text, "total_bytes")?,
                unit: parse_num_lead(&text, "sectorsize")?,
            })
        }
        other => Err(io::Error::other(format!(
            "no size read-back wired for {}",
            content_name(ContentKind::Filesystem(other))
        ))),
    }
}

/// ext 的总字节数：`Block count:` × `Block size:`。
/// 与 [`ext_unit`] 分开：只判字节数的调用点不因 `Blocks per group:` 的形态变化而失效
fn ext_bytes(text: &str) -> io::Result<u64> {
    let blocks = parse_num_lead(text, "Block count:")?;
    let block_size = parse_num_lead(text, "Block size:")?;
    blocks.checked_mul(block_size).ok_or_else(|| {
        io::Error::other(format!("dumpe2fs block count × block size overflows ({blocks} × {block_size})"))
    })
}

/// ext 的放置单元：`Block size:` × `Blocks per group:`
fn ext_unit(text: &str) -> io::Result<u64> {
    let block_size = parse_num_lead(text, "Block size:")?;
    let per_group = parse_num_lead(text, "Blocks per group:")?;
    block_size.checked_mul(per_group).ok_or_else(|| {
        io::Error::other(format!(
            "dumpe2fs block size × blocks per group overflows ({block_size} × {per_group})"
        ))
    })
}

/// 只取总字节：缩容核对只需要尺寸，分配单元在那一侧不参与判据。
/// 每种 FS 只解析自己的尺寸字段，不经过 [`read_fs_geometry`] 的分配单元
pub(crate) fn read_fs_bytes(kind: FsKind, dev: &str) -> io::Result<u64> {
    match kind {
        FsKind::Ext => ext_bytes(&dumpe2fs_text(dev)?),
        FsKind::Ntfs => Ok(parse_num_lead(&ntfsresize_info_text(dev)?, "Current volume size:")?),
        FsKind::Btrfs => Ok(parse_num_lead(&btrfs_superblock_text(dev)?, "total_bytes")?),
        other => Err(io::Error::other(format!(
            "no size read-back wired for {}",
            content_name(ContentKind::Filesystem(other))
        ))),
    }
}

/// ext 最小尺寸（字节）= -P 估计块数 × 块大小。
/// 取值靠解析 CLI 输出，非稳定机器接口（输出格式随版本可能变化，故解析失败即报错、
/// 不猜默认值）：resize2fs -P 打印 -M 缩到最小的块数估计（man resize2fs OPTIONS；
/// 该 man 的 KNOWN BUGS 指出 1K/2K 块文件系统上估计值可能不准），
/// 块大小取自 dumpe2fs -h 的 "Block size:" 行（man dumpe2fs：-h 只读超级块）
pub(crate) fn min_bytes_ext(dev: &str) -> Result<u64, FsError> {
    let out = resize2fs_min_blocks(dev)?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        // 工具跑起来了但退非零。`-P` 是只读查询，实际未写盘；这里按 `CommandFailed`
        // 报出，执行层据此把"工具已启动"的失败折成 5
        return Err(FsError::CommandFailed(format!("resize2fs -P failed ({}): {}", describe_exit(&out.status), text.trim())));
    }
    let blocks = parse_num_lead(&text, "Estimated minimum size of the filesystem:")?;
    let out2 = dumpe2fs_superblock(dev)?;
    let text2 = format!("{}{}", String::from_utf8_lossy(&out2.stdout), String::from_utf8_lossy(&out2.stderr));
    if !out2.status.success() {
        return Err(FsError::CommandFailed(format!("dumpe2fs -h failed ({}): {}", describe_exit(&out2.status), text2.trim())));
    }
    let bs = parse_num_lead(&text2, "Block size:")?;
    // 两个因子都出自外部工具的 stdout：回绕的"最小尺寸"会放行本应拒绝的缩容请求
    //（分区末端切进 FS 元数据），方向危险，溢出按工具输出异常上抛
    blocks.checked_mul(bs).ok_or_else(|| {
        FsError::InvalidArgument(format!("resize2fs -P × dumpe2fs block size overflows ({blocks} × {bs})"))
    })
}