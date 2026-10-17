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
    let mut result: Option<io::Result<u64>> = None;
    with_partition_device(src, part, |dev| {
        result = Some(min_bytes_ext(dev));
        Ok::<(), FsError>(())
    })?;
    match result {
        Some(r) => r.map(Some).map_err(FsError::from),
        // 闭包在 with_partition_device 内恒执行一次，None 只能是内部不变量破裂：
        // fail-closed 报错误，不冒充"无最小尺寸可报"
        None => Err(FsError::Io(io::Error::other("partition probe did not run (internal invariant broken)"))),
    }
}

pub(crate) fn parse_num_field(text: &str, prefix: &str) -> io::Result<u64> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix(prefix)?.trim().parse().ok())
        .ok_or_else(|| io::Error::other(format!("unexpected tool output, missing {prefix:?}")))
}

/// 取字段值**开头**的十进制数字。回读用的行里数值后面还跟着人读注解
/// （`Current volume size: 33554944 bytes (34 MB)`），故不能要求整段就是一个数
pub(crate) fn parse_num_lead(text: &str, prefix: &str) -> io::Result<u64> {
    text.lines()
        .find_map(|l| {
            let rest = l.trim().strip_prefix(prefix)?.trim_start();
            rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
        })
        .ok_or_else(|| io::Error::other(format!("unexpected tool output, missing {prefix:?}")))
}

/// ext 最小尺寸（字节）= -P 估计块数 × 块大小。
/// 取值靠解析 CLI 输出，非稳定机器接口（输出格式随版本可能变化，故解析失败即报错、
/// 不猜默认值）：resize2fs -P 打印 -M 缩到最小的块数估计（man resize2fs OPTIONS；
/// 该 man 的 KNOWN BUGS 指出 1K/2K 块文件系统上估计值可能不准），
/// 块大小取自 dumpe2fs -h 的 "Block size:" 行（man dumpe2fs：-h 只读超级块）
pub(crate) fn min_bytes_ext(dev: &str) -> io::Result<u64> {
    let out = run("resize2fs", &["-P", dev])?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return Err(io::Error::other(format!("resize2fs -P failed: {}", text.trim())));
    }
    let blocks = parse_num_field(&text, "Estimated minimum size of the filesystem:")?;
    let out2 = run("dumpe2fs", &["-h", dev])?;
    let text2 = format!("{}{}", String::from_utf8_lossy(&out2.stdout), String::from_utf8_lossy(&out2.stderr));
    if !out2.status.success() {
        return Err(io::Error::other(format!("dumpe2fs -h failed: {}", text2.trim())));
    }
    let bs = parse_num_field(&text2, "Block size:")?;
    // 两个因子都出自外部工具的 stdout：回绕的"最小尺寸"会放行本应拒绝的缩容请求
    //（分区末端切进 FS 元数据），方向危险，溢出按工具输出异常上抛
    blocks.checked_mul(bs)
        .ok_or_else(|| io::Error::other(format!("resize2fs -P × dumpe2fs block size overflows ({blocks} × {bs})")))
}