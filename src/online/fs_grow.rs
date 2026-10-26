//! 在线 FS 扩容的工具调用（ext/xfs/btrfs）。

use super::*;
use crate::dev::FileSource;
use crate::fsops::run;

/// FS 在线 grow 到整分区（ext: resize2fs 对挂载设备无 size = 扩满分区，man resize2fs；
/// xfs: xfs_growfs 挂载点；btrfs: resize max 挂载点）。
/// 程序与参数只写这一处：补救提示复用同一份映射（fs_grow_hint），避免两处漂移
pub(crate) fn fs_grow_cmd(t: &OnlineTarget, fstype: Option<crate::fsid::FsKind>) -> Option<(String, Vec<String>)> {
    // xfs_growfs 与 btrfs resize 都以挂载点为参数：目标不是从挂载点解析来时没有这个参数，
    // 与"该 FS 没有在线工具"同归 None（调用方统一报"换离线路径"）
    let mnt = || t.mnt.as_deref().map(|p| p.to_string_lossy().into_owned());
    let dev = t.part_dev.to_string_lossy().into_owned();
    match fstype {
        Some(crate::fsid::FsKind::Ext) => Some(("resize2fs".to_string(), vec![dev])),
        Some(crate::fsid::FsKind::Xfs) => Some(("xfs_growfs".to_string(), vec![mnt()?])),
        Some(crate::fsid::FsKind::Btrfs) => {
            Some(("btrfs".to_string(), vec!["filesystem".into(), "resize".into(), "max".into(), mnt()?]))
        }
        _ => None,
    }
}

/// fs_grow 的三态失败：**该 FS 没有在线工具**意味着本次一个进程都没起，
/// 成因在请求（换离线路径才有解）⇒ 拒绝；**工具没跑起来**（找不到 / 无法执行）
/// 是环境故障，FS 同样未被触碰 ⇒ infra；**非零退出**说明工具可能改了一半 ⇒ failed。
/// 三者出口语义各异，压成一个错误类型会让调用点无从区分
pub(crate) enum FsGrowError {
    /// 该 FS 无在线 grow 工具（`fs_grow_cmd` 映射缺失）
    NoTool(String),
    /// 工具没跑起来：`fsops::run` 的失败侧（工具缺失 / 执行失败）
    Spawn(crate::fsops::FsError),
    Exit(String),
}

impl FsGrowError {
    pub(crate) fn detail(&self) -> String {
        match self {
            FsGrowError::NoTool(m) | FsGrowError::Exit(m) => m.clone(),
            FsGrowError::Spawn(e) => e.to_string(),
        }
    }
}

pub(crate) fn fs_grow(t: &OnlineTarget, fstype: Option<crate::fsid::FsKind>) -> Result<(), FsGrowError> {
    let Some((prog, args)) = fs_grow_cmd(t, fstype) else {
        return Err(FsGrowError::NoTool(format!(
            "{} has no online grow tool; unmount the partition and use the offline path where the FS is supported",
            crate::fsid::fs_name(fstype)
        )));
    };
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    // spawn 失败 = 工具没跑起来；启动后拿不到退出状态 = 可能已改 FS，归 Exit（failed）
    let out = match run(&prog, &argv) {
        Ok(o) => o,
        Err(crate::fsops::FsError::CommandFailed(m)) => return Err(FsGrowError::Exit(m)),
        Err(e) => return Err(FsGrowError::Spawn(e)),
    };
    if !out.status.success() {
        return Err(FsGrowError::Exit(format!(
            "online FS grow failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// 在线 FS 步的补救命令（与 fs_grow 共用命令映射；无在线工具时回落到离线提示）
pub(crate) fn fs_grow_hint(t: &OnlineTarget, fstype: Option<crate::fsid::FsKind>) -> String {
    match fs_grow_cmd(t, fstype) {
        Some((prog, args)) => format!("{prog} {}", args.join(" ")),
        None => crate::fsops::rescue_hint(fstype, &t.part_dev.to_string_lossy()),
    }
}

pub(crate) fn fstype_of(t: &OnlineTarget) -> io::Result<Option<crate::fsid::FsKind>> {
    // 挂载中的分区不能以 O_EXCL 读写打开（dev::FileSource::open 会失败），只读识别
    let src = FileSource::open_read_only(&t.part_dev)?;
    crate::fsid::identify(&src, 0, t.part_len_bytes)
}

/// btrfs 多设备防线（在线路径）：多设备 btrfs 的 resize 须按设备逐个手动处理
/// （btrfs-filesystem(8)），与离线路径共用 fsops 的同一道判定；只对 btrfs 生效，
/// 其余 FS 的该偏移另有布局。调用点须在任何写盘步骤之前
pub(crate) fn refuse_btrfs_multi_device(t: &OnlineTarget, fstype: Option<crate::fsid::FsKind>) -> Result<(), crate::fsops::FsError> {
    if fstype != Some(crate::fsid::FsKind::Btrfs) {
        return Ok(());
    }
    let src = FileSource::open_read_only(&t.part_dev)?;
    crate::fsops::refuse_btrfs_multi_device_at(&src, 0, t.part_len_bytes)
}