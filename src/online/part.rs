//! 在线分区表缩放：sfdisk 写入与内核视图同步。

use super::*;
use crate::fsops::{run, run_input};

/// part_resize 的失败按"是否知道表已落盘"与"成因在哪一侧"分四类——这不是措辞差异：
/// - `Stale`：写前重读判定"当前盘上现状与请求不符"——目标条目被改、新尺寸压到现存分区，
///   或重读本身给出拒绝（无表、LBA0 保护布局受损）。表未写 → 拒绝（10）
/// - `NoWrite`：sfdisk 进程未启动，表确定未写
/// - `Unknown`：sfdisk 执行了但报错。脚本被拒是未写、写入中途失败则可能已写，
///   两者在 stdout/stderr 上不可区分 → 不得断言未写
/// - `Partial`：表已写，仅内核同步失败
pub(crate) enum PartResizeError {
    Stale(io::Error),
    NoWrite(io::Error),
    Unknown(io::Error),
    Partial(io::Error),
}

impl PartResizeError {
    pub(crate) fn detail(&self) -> &io::Error {
        match self {
            Self::Stale(e) | Self::NoWrite(e) | Self::Unknown(e) | Self::Partial(e) => e,
        }
    }
    /// 现状与请求不符 → 拒绝；表未写 → Infra（确定未写盘，不带"盘可能已改变"的提示）；
    /// 无法断言 → Failed；表已写 → Applied（内核视图置为过期，退出码 20）。
    /// 具体原因（含 errno 提示）在发生处打印，契约摘要由 Outcome::report 统一输出
    pub(crate) fn into_outcome(self) -> Outcome {
        self.into_outcome_with(Vec::new())
    }

    /// 表已写但内核未同步时，调用方可能还有因此没做的后续步骤（FS 扩缩）：这些待办
    /// 与"内核视图过期"是两个正交事实，必须一并浮出，否则 20 只说了其中一半
    pub(crate) fn into_outcome_with(self, pending: Vec<Pending>) -> Outcome {
        match classify_part_resize_error(&self) {
            ResizeFailClass::Refused(msg) => Outcome::refused(msg),
            ResizeFailClass::Infra(msg) => Outcome::infra(msg),
            ResizeFailClass::Failed(msg) => Outcome::failed(msg),
            ResizeFailClass::AppliedStaleKernel(msg) => {
                eprintln!("warning: partition table written but kernel sync failed: {msg}");
                let mut o = Outcome::applied_with(pending);
                o.mark_kernel_stale();
                o
            }
        }
    }
}

/// part_resize 失败的三态分类（纯函数核）：副作用（警告打印、Outcome 构造）留在
/// into_outcome，分类依据本身可单测——"知道表写没写"决定退出码语义，这是契约级判断
#[derive(Debug)]
pub(crate) enum ResizeFailClass {
    /// 请求与盘上现状不匹配，表确定未写
    Refused(String),
    /// 表确定未写（sfdisk 未启动）
    Infra(String),
    /// sfdisk 执行过但报错，表是否落盘不可断言
    Failed(String),
    /// 表已写，仅内核同步失败（布局生效、内核视图过期）
    AppliedStaleKernel(String),
}

pub(crate) fn classify_part_resize_error(e: &PartResizeError) -> ResizeFailClass {
    match e {
        PartResizeError::Stale(err) => ResizeFailClass::Refused(err.to_string()),
        PartResizeError::NoWrite(err) => ResizeFailClass::Infra(err.to_string()),
        PartResizeError::Unknown(err) => ResizeFailClass::Failed(err.to_string()),
        PartResizeError::Partial(err) => ResizeFailClass::AppliedStaleKernel(err.to_string()),
    }
}

/// 持久化分区缩放：sfdisk 改写该分区表项（start 不变，仅 size）→ partx -u
/// 同步内核；partx 失败以 BLKPG 兜底。挂载中分区的 BLKRRPART 重读必失败，
/// 故 --no-reread；--force 关闭 sfdisk 一致性检查，故写前先锁下重验盘上表
/// （见函数体首段注释），边界与重叠仍由调用方预检。
pub(crate) fn part_resize(t: &OnlineTarget, new_len_bytes: u64) -> Result<(), PartResizeError> {
    let pno_str = t.pno.to_string();
    let disk_str = t.disk_dev.to_string_lossy().into_owned();
    // 写前锁下重验（最小 TOCTOU 防线，**只覆盖目标分区自身与邻居集**）：sysfs 快照
    // 取自内核视图，取锁到 sfdisk 之间第三方（不守本工具的锁）可能已改写盘上表；
    // sfdisk 的 --force 关闭全部一致性检查，不会替我们拒绝由此造成的异常。这里
    // 直接重读**盘上表**：目标条目与邻居集都从这一次读里取，不符即拒——邻居维度
    // 若只信取锁时的 sysfs 快照，跨界重叠就只能靠内核 -EBUSY/-EINVAL 兜底，那时
    // 盘上已经躺着一张重叠的表。残余窗口（重验之后、sfdisk 执行）连目标维度也
    // 无法在本进程内消除（sfdisk 内部自读盘表）
    let disk = crate::dev::FileSource::open_read_only(&t.disk_dev)
        .map_err(|e| PartResizeError::NoWrite(io::Error::other(format!("pre-write re-read failed: {e}"))))?;
    match crate::gpt_policy::disk_snapshot(&disk, t.pno) {
        Ok(snap) => {
            let (start, len) = snap.target;
            if (start, len) != (t.start_bytes, t.part_len_bytes) {
                // 端点换算 saturating（数据源是盘上重读结果）：饱和只影响提示文字，
                // 不影响拒绝判定本身
                let now_end = start.saturating_add(len);
                let snap_end = t.start_bytes.saturating_add(t.part_len_bytes);
                return Err(PartResizeError::Stale(io::Error::other(format!(
                    "partition {} on disk changed since the snapshot (now at {start}..{now_end}, snapshot {}..{snap_end}) — \
                     either another tool rewrote the table, or an earlier refresh of this command's own table write did not \
                     reach the kernel; run partprobe on the disk and re-run; refusing to overwrite an edit this command \
                     cannot account for",
                    t.pno,
                    t.start_bytes,
                ))));
            }
            if !range_free(t.start_bytes, new_len_bytes, &snap.others) {
                return Err(PartResizeError::Stale(io::Error::other(format!(
                    "resizing partition {} to {new_len_bytes} bytes would overlap a partition present on disk now ({} neighbour(s)) — refusing over a stale snapshot",
                    t.pno,
                    snap.others.len(),
                ))));
            }
        }
        Err(f) => {
            // 写前重读的分类原样保留：`Refused` 是"现状本身不允许继续"（无表、LBA0 保护
            // 布局受损、条目已变）→ 拒绝，出口 10；`Infra` 是读不出来（确定未写盘）；
            // `Failed` 是不可断言。一律压成 `NoWrite` 会把拒绝说成环境故障，退出码与
            // 建议都随之走偏
            let text = |m: String| {
                io::Error::other(format!("cannot re-read the on-disk partition table before writing: {m}"))
            };
            return Err(match f {
                crate::outcome::Fail::Refused(m) => PartResizeError::Stale(text(m)),
                crate::outcome::Fail::Infra(m) => PartResizeError::NoWrite(text(m)),
                crate::outcome::Fail::Failed(m) => PartResizeError::Unknown(text(m)),
            });
        }
    }
    // sfdisk 脚本里的裸数字按"设备扇区"解释（libfdisk/src/script.c：脚本不带
    // `sector-size:` 头时 `recount_script2device_sectors` 直接返回，已解析的数值就是
    // 设备逻辑扇区数），即 4Kn = 4096B 而非恒 512B，故按 t.logical_block 换算。
    // -N：只改指定分区、未指定字段保持原值（空 start/size 继承现值，sfdisk(8)）
    let script = format!(",{}", new_len_bytes / t.logical_block);
    // 在线路径的 durable boundary 就在这一次 sfdisk：之前 abort 表确定未写、
    // 之后 abort 表可能已写。成对注入才能把这条分界变成可断言的事实
    fault_online_before_write();
    let out = run_input(
        "sfdisk",
        &["--no-reread", "--force", "-N", &pno_str, &disk_str],
        &script,
    )
    .map_err(|e| match e {
        // 进程已启动、输入没能送达：可能读过一部分脚本并动过盘，不得断言未写
        crate::fsops::FsError::CommandFailed(m) => PartResizeError::Unknown(io::Error::other(m)),
        other => PartResizeError::NoWrite(other.into()),
    })?;
    fault_online_after_write();
    if !out.status.success() {
        return Err(PartResizeError::Unknown(io::Error::other(format!(
            "sfdisk resize failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))));
    }
    let partx = run("partx", &["-u", "--nr", &pno_str, &disk_str]);
    let partx_ok = partx.as_ref().map(|o| o.status.success()).unwrap_or(false);
    if partx_ok {
        return Ok(());
    }
    // BLKPG 兜底成功时用户只会看到兜底结果，partx 的根因若不在此带出就再也不可观察
    let why = match partx {
        Err(e) => format!("partx could not be run: {e}"),
        Ok(o) => format!("partx -u failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
    };
    match blkpg_resize(t, new_len_bytes) {
        Ok(()) => {
            eprintln!("warning: {why} — the kernel partition view was resized through the BLKPG fallback instead");
            Ok(())
        }
        Err(e) => Err(PartResizeError::Partial(io::Error::other(format!(
            "{e} (kernel sync attempts: {why})"
        )))),
    }
}

/// 内核同步最终判据：/sys/dev/block/<maj>:<min>/size 须等于期望值。
/// 该 sysfs-block ABI 属性以 512-byte sectors 表示（与逻辑块大小无关，故除 512，
/// 与 part_resize 按逻辑扇区换算不同）
pub(crate) fn verify_part_size(t: &OnlineTarget, new_len_bytes: u64) -> io::Result<()> {
    let meta = fs::metadata(&t.part_dev)?;
    let (maj, min) = (libc::major(meta.st_rdev()), libc::minor(meta.st_rdev()));
    let got = sysfs_u64(&Path::new("/sys/dev/block").join(format!("{maj}:{min}")).join("size"))?;
    let want = new_len_bytes / 512;
    if got != want {
        return Err(io::Error::other(format!(
            "kernel reports partition size {got} sectors, expected {want}"
        )));
    }
    Ok(())
}