//! FS 字节数探测：各工具输出格式的解析。

use super::*;

/// ext 最小尺寸估算：resize2fs -P 的最小块数 × dumpe2fs -h 的块大小；
/// 输出格式锚定 man 页示例（"Estimated minimum size of the filesystem: N" / "Block size: N"）。
/// 其余 FS 无已验证的输出格式，返回 None，由工具自身在缩容前拒绝
/// （shrink_fs 先于任何数据搬移执行，失败即安全终止）
pub fn fs_min_bytes(src: &FileSource, part: u32, fstype: Option<FsKind>) -> Result<Option<u64>, FsError> {
    if fstype != Some(FsKind::Ext) {
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
/// 不是一回事：前者是输出形态不符，后者说明我们正读着的一个值不能用；两者都拒绝，
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
/// 首个匹配前缀的行即该字段的唯一定义，它的值不可用就拒绝，不再到后面的行里另找一个
/// 能解析的值（那会把"这一个值读坏了"掩盖成"读出来了另一个数"）
pub(crate) fn parse_num_lead(text: &str, prefix: &str) -> Result<u64, NumError> {
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix(prefix) else { continue };
        let raw = rest.trim_start();
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

/// ext 最小尺寸（字节）= -P 估计块数 × 块大小。
/// 取值靠解析 CLI 输出，非稳定机器接口（输出格式随版本可能变化，故解析失败即报错、
/// 不猜默认值）：resize2fs -P 打印 -M 缩到最小的块数估计（man resize2fs OPTIONS；
/// 该 man 的 KNOWN BUGS 指出 1K/2K 块文件系统上估计值可能不准），
/// 块大小取自 dumpe2fs -h 的 "Block size:" 行（man dumpe2fs：-h 只读超级块）
pub(crate) fn min_bytes_ext(dev: &str) -> Result<u64, FsError> {
    let out = run("resize2fs", &["-P", dev])?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        // 工具跑起来了但拒绝了本设备：这是"可能已写盘"的一类，不能压成 Io
        return Err(FsError::CommandFailed(format!("resize2fs -P failed ({}): {}", describe_exit(&out.status), text.trim())));
    }
    let blocks = parse_num_lead(&text, "Estimated minimum size of the filesystem:")?;
    let out2 = run("dumpe2fs", &["-h", dev])?;
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