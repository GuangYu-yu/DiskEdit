//! 扩容后的后置条件分类：分区已经扩了、文件系统侧却没有可写的动作时，出口要带的那一条。
//!
//! 三处调用点（`resize` 的 GPT 收尾、搬移收尾、MBR 收尾）描述的是同一件事——目标分区
//! 现在处于什么状态，所以判定与措辞都在这里，调用点只负责把日志与待办交出去。

use crate::fsops::Growable;
use crate::outcome::{Pending, PendingKind};

/// 一条后置条件：日志行 + 可选的待办项。`pending` 为空表示这一态没有未完成的动作
/// （空间由别的链路生效），出口仍按"已应用"报告
pub(crate) struct GrowPost {
    pub(crate) log: String,
    pub(crate) pending: Option<Pending>,
}

/// `Target` 由调用方接走（它要执行扩容），本函数只处理另外四态
pub(crate) fn grow_post(part: u32, g: &Growable) -> GrowPost {
    match g {
        // 这段空间由目标系统启动时建成 overlay；本命令看不到那一步的成败
        Growable::OverlayPending => GrowPost {
            log: "partition extended (the overlay filesystem is created at system startup)"
                .to_string(),
            pending: Some(Pending::new(
                part,
                PendingKind::RuntimeInitialization,
                "the overlay filesystem is initialized by the target system at startup; \
                 this command did not verify that step",
                "boot the target and check that the overlay is mounted and its size covers the new space",
            )),
        },
        // 类型认不出：措辞不替区域内容下结论——空区域与认不出的类型在 identify 口径下同属 None
        Growable::NoFilesystem(None) => GrowPost {
            log: "partition extended (filesystem not resized — its type was not identified)"
                .to_string(),
            pending: Some(Pending::new(
                part,
                PendingKind::Fs,
                "the partition was extended but the filesystem inside was not resized — its type was not identified",
                "identify the filesystem inside and grow it with its own tool, or re-run with --no-fs to change only the partition",
            )),
        },
        // PV 的空间在 pvresize 链里生效，本命令没有可写的后置条件
        Growable::NoFilesystem(Some(crate::fsid::ContentKind::Lvm2Pv)) => GrowPost {
            log: "partition extended (the PV space takes effect through the pvresize chain)".to_string(),
            pending: None,
        },
        // 元数据是 swap、页格式在本机激活不了：扩容后"swap 未重建"是一条真实的待办
        Growable::SwapUnactivatable => GrowPost {
            log: "partition extended (the swap signature was found but this host cannot activate it)"
                .to_string(),
            pending: Some(Pending::new(
                part,
                PendingKind::Swap,
                "the partition was extended but the swap area was not rebuilt — its signature uses a \
                 page size this host cannot activate",
                "boot the target or a host with a matching page size, then check that swap is active \
                 and covers the new space",
            )),
        },
        // 认出了类型却没有可扩的 FS：后置条件"新空间对该分区可用"未达成
        Growable::NoFilesystem(Some(kind)) => GrowPost {
            log: format!(
                "partition extended (the {kind} inside was not resized — this tool cannot grow it)"
            ),
            pending: Some(Pending::new(
                part,
                PendingKind::Fs,
                format!("the partition was extended but the {kind} inside was not resized"),
                format!(
                    "grow the {kind} with its own tool, or re-run with --no-fs to change only the partition"
                ),
            )),
        },
        // 调用方在上一步已把 Target 接走；类型在核对与收尾之间翻转是唯一能走到这里的方式
        Growable::Target(t) => GrowPost {
            log: format!(
                "partition extended (the {} inside was not resized — its type changed between validation and finish)",
                t.fstype
            ),
            pending: Some(Pending::new(
                part,
                PendingKind::Fs,
                format!(
                    "the partition was extended but the {} inside was not resized — its type changed between validation and finish",
                    t.fstype
                ),
                format!(
                    "grow the {} with its own tool, or re-run with --no-fs to change only the partition",
                    t.fstype
                ),
            )),
        },
    }
}
