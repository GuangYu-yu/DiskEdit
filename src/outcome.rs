//! 布局操作的**后置条件契约**。
//!
//! 命令的承诺不是"运行了什么"，而是"完成后哪些条件成立"：
//! 默认契约 = 分区布局改变 + 空间对该分区可用（FS 扩展 / swap 重建 / LVM 链）。
//! 退出码只是这个契约满足情况的外显，因此映射集中在此处一处，
//! 不允许各调用点自行拼装
//!
//! 未完成的情形按两条**正交**的轴分三类：**本次是否写盘**（决定能不能对盘上状态作断言，
//! 从而决定给用户的下一步建议）与**成因在请求还是在环境**（决定"改参数重试"有没有意义）：
//! - 请求与目标现状不匹配 + 未写盘 → 10：无表、无此分区、工具缺失、缺少确认旗标
//! - 环境/盘内容故障 + 未写盘 → 30：I/O 失败、表结构非法（头 CRC 坏、条目越界）
//! - 已写盘 → 20（后置条件未全满足）或 30（执行失败，可能已改变）
//!
//! 因此**分类必须在知道成因的那一层做**：同一个成因（例如表读不出来）会因"无表"与
//! "表非法"落进两个变体，而 io::Error 一旦成形就再也分不出来，只会在调用点被压成一个码

pub const EXIT_OK: u8 = 0;
/// 事前拒绝：本次未写盘，成因在请求与目标现状不匹配（无表、无此分区、工具缺失、缺少确认旗标）
pub const EXIT_REFUSED: u8 = 10;
/// 部分完成：写盘已发生，但后置条件未全部满足
pub const EXIT_PARTIAL: u8 = 20;
/// 环境/盘内容故障，或写盘后失败：本次没能完成。未写盘时（表非法、I/O 故障）与
/// 可能已写盘时（执行中途失败）同为这一码——区别在报告文字里点明
pub const EXIT_INFRA: u8 = 30;

/// 后续步骤的种类：决定补救提示里出现哪条命令。
/// 目前只有两种——LVM 链失败由 resize_done 直接判定 PARTIAL，不经 Pending 通道
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    Fs,
    Swap,
}

/// 一条未满足的后置条件。补救信息由 FS 层生成（工具与包名的知识在那里），
/// 出口只负责格式化，避免"该跑什么命令"的字符串在两层各写一遍而漂移
#[derive(Debug, Clone)]
pub struct Pending {
    pub part: u32,
    pub kind: PendingKind,
    /// 失败原因（外部工具的原话）
    pub detail: String,
    /// 补救命令（可含具体设备路径）
    pub hint: String,
}

impl Pending {
    pub fn new(part: u32, kind: PendingKind, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self { part, kind, detail: detail.into(), hint: hint.into() }
    }
}

/// 内核分区视图是否已跟上盘上的表。与 `pending` 正交：`pending` 是"业务后置条件是否完成"，
/// 本项是"内核可见性"，两者可同时成立（如：表已写、内核未同步、FS 步也因此未做）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelSync {
    Synchronized,
    Stale,
}

/// 一次布局操作的结果。四态而非两个字段：`Applied` 之外有三种未完成成因，
/// 它们的退出码与报告文字都不同，必须由类型区分
#[derive(Debug, Clone)]
pub enum Outcome {
    /// 校验阶段拒绝——确定未写盘，成因在请求与目标现状不匹配
    Refused(String),
    /// 环境/盘内容故障——确定未写盘（I/O 失败、表结构非法）。
    /// 与 `Failed` 同为 30，但**不能**附"盘可能已改变"的提示：本次确实没写
    Infra { cause: String },
    /// 执行阶段失败——**不对"是否已落盘"作断言**：写调用报错时我们并不知道
    /// 实际写入多少，磁盘状态可能已改变，故只承诺"可能已改变，需验证"
    Failed { cause: String },
    /// 布局写入完成；`pending` 为空且内核视图已同步即后置条件全部满足
    Applied {
        pending: Vec<Pending>,
        kernel_sync: KernelSync,
    },
}

impl Outcome {
    pub fn refused(msg: impl Into<String>) -> Self {
        Self::Refused(msg.into())
    }

    pub fn infra(cause: impl Into<String>) -> Self {
        Self::Infra { cause: cause.into() }
    }

    pub fn failed(cause: impl Into<String>) -> Self {
        Self::Failed { cause: cause.into() }
    }

    /// 布局已写、后置条件待办；内核视图默认按"已同步"起算，由调用方在 resync 失败后置位
    pub fn applied_with(pending: Vec<Pending>) -> Self {
        Self::Applied { pending, kernel_sync: KernelSync::Synchronized }
    }

    /// 布局已写但内核视图未跟上（在线路径表已落盘、partx/BLKPG 未同步时直接构造）。
    /// 唯一调用方是 online（仅 Linux）——非 Linux 构建下它没有使用者
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn applied_stale_kernel() -> Self {
        Self::Applied { pending: Vec::new(), kernel_sync: KernelSync::Stale }
    }

    /// 内核重读失败：把"内核视图过期"这一事实记入结果（只对 Applied 有意义）
    pub fn mark_kernel_stale(&mut self) {
        if let Self::Applied { kernel_sync, .. } = self {
            *kernel_sync = KernelSync::Stale;
        }
    }

    pub fn is_applied(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }

    /// 后置条件全部满足：无待办且内核视图已同步。这是**唯一**对应退出码 0 的形态，
    /// 调用点要问"是不是完全成功"时用它而不是比退出码——比数字会把"部分完成"误判成成功
    pub fn is_complete(&self) -> bool {
        matches!(
            self,
            Self::Applied { pending, kernel_sync }
                if pending.is_empty() && *kernel_sync == KernelSync::Synchronized
        )
    }

    /// 唯一的 Outcome → 退出码映射点
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Refused(_) => EXIT_REFUSED,
            Self::Infra { .. } | Self::Failed { .. } => EXIT_INFRA,
            applied if applied.is_complete() => EXIT_OK,
            Self::Applied { .. } => EXIT_PARTIAL,
        }
    }

    /// 统一输出。成功且无遗留时不打印（调用方自报成功信息），
    /// 其余情况必须让用户知道盘上实际发生了什么、下一步该做什么。
    /// 调用方须在"内核重读"之后调用本函数，否则打不出内核视图过期这一项
    pub fn report(&self) {
        match self {
            Self::Refused(msg) => eprintln!("refused: {msg}"),
            // 与 Failed 同码不同话：本次没写盘，不能提示"盘可能已改变"
            Self::Infra { cause } => eprintln!("error: {cause}"),
            Self::Failed { cause } => {
                eprintln!("error: {cause}");
                eprintln!("       on-disk state may have changed — verify with `diskedit info` before retrying");
            }
            Self::Applied { pending, kernel_sync } => {
                if pending.is_empty() && *kernel_sync == KernelSync::Synchronized {
                    return;
                }
                if !pending.is_empty() {
                    eprintln!("partition table updated, but {} follow-up step(s) are pending:", pending.len());
                    for p in pending {
                        let what = match p.kind {
                            PendingKind::Fs => "fs grow",
                            PendingKind::Swap => "swap rebuild",
                        };
                        eprintln!("  [part {}] {what}: {}", p.part, p.detail);
                        if !p.hint.is_empty() {
                            eprintln!("           run manually: {}", p.hint);
                        }
                    }
                }
                if *kernel_sync == KernelSync::Stale {
                    eprintln!(
                        "partition table is on disk but the kernel partition view is stale — \
                         run `partprobe <device>` (or partx/blkpg) before relying on the new layout"
                    );
                }
            }
        }
    }
}

/// 执行期失败的三种性质，判据是上面那两条正交的轴：
/// - `Refused`：未写盘 + 成因在请求（目标现状与请求不匹配）→ 10，改参数有意义
/// - `Infra`：未写盘 + 成因在环境/盘内容（I/O 失败、结构非法）→ 30，改参数没意义
/// - `Failed`：已写盘，或无法断定没写 → 30，须提示先验证盘上状态
///
/// 必须在类型上分开，不能靠错误文本或 ErrorKind 反推
#[derive(Debug)]
pub enum Fail {
    Refused(String),
    Infra(String),
    Failed(String),
}

/// 默认按"可能已改变"归 `Failed`：写调用报错时我们并不知道实际写入多少，
/// 这是唯一安全的缺省。**写盘前的环境故障必须显式写 `Fail::infra`**——
/// 与 table::GptError 只提供单向压平同理：需要区分的地方由编译器强制它显式表态
impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        Self::failed(e.to_string())
    }
}

/// FS 层的失败分类 → 出口语义。**这是 `FsError` 唯一的解释处**：fsops 只回答
/// "操作层面发生了什么"，落成哪个退出码属应用层 policy，故不在 fsops 里做。
///
/// 判据仍是那两条：成因在请求还是在环境。「类型没有接线 / 参数与目标现状不符」是前者，
/// 改参数（或换命令）有意义 → 拒绝（10）；工具缺失、环境故障、外部工具非零退出都是后者，
/// 改参数无用 → 30。写盘之后（`execute_*` 里）拿不到这个映射：那里 `FsError` 已被
/// 压平成 io::Error，结论只能是 Failed
impl From<crate::fsops::FsError> for Fail {
    fn from(e: crate::fsops::FsError) -> Self {
        use crate::fsops::FsError;
        match e {
            FsError::UnsupportedFs(m) | FsError::InvalidArgument(m) => Self::refused(m),
            FsError::ToolMissing(m) => Self::infra(m),
            FsError::Io(err) => Self::infra(err.to_string()),
            FsError::CommandFailed(m) => Self::infra(m),
        }
    }
}

impl Fail {
    pub fn refused(msg: impl Into<String>) -> Self {
        Self::Refused(msg.into())
    }

    /// 写盘前的环境/盘内容故障（表结构非法、I/O 失败）。与 `Failed` 同为 30，
    /// 但不带"盘可能已改变"的提示——调用点必须确实尚未写盘才可用它
    pub fn infra(cause: impl Into<String>) -> Self {
        Self::Infra(cause.into())
    }

    /// 已写盘或无法断定时的执行失败（io 失败经 `From<io::Error> for Fail` 落到这一形态）
    pub fn failed(cause: impl Into<String>) -> Self {
        Self::Failed(cause.into())
    }

    /// 写盘前的 io 失败。存在的意义是让调用点**显式表态**：这个 `?` 位于本次调用的首次
    /// 目标写盘之前。默认的 `From<io::Error>` 给不出这个断言，故不设默认、必须逐点写明。
    ///
    /// 判据只关于**本次调用**写没写目标盘。上一次中断的运行留下的改动不在此编码范围内——
    /// 重试时工具会重读盘上几何（表项 / ckpt），故不因省略"盘可能已改变"的提示而失去安全性；
    /// 而真正需要那句提示的是写调用报错（`Failed`），那种情况下写入量确实无法断定
    pub fn infra_io(e: std::io::Error) -> Self {
        Self::Infra(e.to_string())
    }

    /// 给原因加上本层上下文（变体与退出码不变）。存在的意义是让调用点补一句"当时在做什么"
    /// 而不必丢弃已经得出的分类——为了拼文案而改用 `Fail::infra(..)` 会把
    /// `Refused` 悄悄升格成 `Infra`
    pub fn context(self, what: &str) -> Self {
        match self {
            Self::Refused(m) => Self::Refused(format!("{what}: {m}")),
            Self::Infra(m) => Self::Infra(format!("{what}: {m}")),
            Self::Failed(m) => Self::Failed(format!("{what}: {m}")),
        }
    }
}

/// 把内层 `Fail` 并入一个"已越界"的调用点：此时对外结论只能是 `Failed`（盘可能已改变），
/// 内层是 Refused 也只是因为它自己那一层还没写盘——对本层已经不算数了。
/// 存在的意义是让这种"层级差"显式可见，而不是靠 `?` 悄悄把 Refused 透传出去。
/// 不提供 `From<Fail> for io::Error`：压平必须逐点写明，理由同上
pub fn into_io_error(e: Fail) -> std::io::Error {
    let msg = match e {
        Fail::Refused(m) | Fail::Infra(m) | Fail::Failed(m) => m,
    };
    std::io::Error::other(msg)
}

/// 内部执行体与对外入口的分界：执行体用 `?` 传播（io 错误默认按"可能已改变"归 Failed），
/// 入口负责换算为 Outcome。这样"退出码"只在入口一处出现
pub fn finish(result: Result<(), Fail>, pending: Vec<Pending>) -> Outcome {
    match result {
        Ok(()) => Outcome::applied_with(pending),
        Err(Fail::Refused(m)) => Outcome::refused(m),
        Err(Fail::Infra(m)) => Outcome::infra(m),
        Err(Fail::Failed(m)) => Outcome::failed(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsops::FsError;

    /// FsError 的分类只在这一处落成出口语义：不认得的类型 / 参数与目标现状不符 ⇒ 10
    /// （改参数有意义），工具缺失 / 环境故障 / 外部工具非零退出 ⇒ 30（改参数无意义）
    #[test]
    fn fs_error_maps_to_exit_codes() {
        let code = |e: FsError| finish(Err(Fail::from(e)), Vec::new()).exit_code();
        assert_eq!(code(FsError::UnsupportedFs("no resize tool".into())), EXIT_REFUSED);
        assert_eq!(code(FsError::InvalidArgument("bad size".into())), EXIT_REFUSED);
        assert_eq!(code(FsError::ToolMissing("no mkfs.xfs".into())), EXIT_INFRA);
        assert_eq!(code(FsError::Io(std::io::Error::other("EIO"))), EXIT_INFRA);
        assert_eq!(code(FsError::CommandFailed("mkswap failed".into())), EXIT_INFRA);
    }

    /// 越过 durable boundary 之后，同一分类不再有出口语义：压平即只剩 Failed
    /// （报告里必须带"盘可能已改变"，此时确实无法断言）
    #[test]
    fn fs_error_flattens_to_failed_after_the_boundary() {
        let after: std::io::Error = FsError::UnsupportedFs("no resize tool".into()).into();
        assert!(matches!(Fail::from(after), Fail::Failed(_)));
    }

    /// 上下文只改文案，不改分类——为了补一句"当时在做什么"而改用 `Fail::infra(..)`
    /// 会把 10 悄悄升格成 30
    #[test]
    fn context_keeps_the_variant() {
        assert!(matches!(Fail::refused("x").context("doing y"), Fail::Refused(m) if m == "doing y: x"));
        assert!(matches!(Fail::infra("x").context("doing y"), Fail::Infra(m) if m == "doing y: x"));
        assert!(matches!(Fail::failed("x").context("doing y"), Fail::Failed(m) if m == "doing y: x"));
    }

    /// 本模块是"后置条件契约 + 退出码的唯一映射点"，不认识任何具体文件系统或工具——
    /// 那类知识属于 fsops/movepart。一旦落进来，它就会以"某类走某工具"的形式改变分类结论，
    /// 而分类错了是静默的。层的归属靠自觉守不住，故设门禁：写下一个名字就立刻红。
    /// 只扫代码：注解描述契约本身（含它覆盖哪些机制）是这一层的本分
    #[test]
    fn knows_no_concrete_filesystem_or_tool() {
        let src = include_str!("outcome.rs");
        let code = src[..src.find("#[cfg(test)]").unwrap_or(src.len())]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .map(|l| &l[..l.find("//").unwrap_or(l.len())])
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        // 门禁自己也要有门禁：截取或去注释若吃掉了全文，下面的断言会全部空过
        assert!(code.contains("exit_refused"), "门禁没扫到代码：截取或去注释把它吃空了");
        for t in [
            // 文件系统类型
            "ntfs", "ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "vfat", "exfat", "msdos",
            // 文件系统与卷管理工具
            "mkfs", "mkswap", "fsck", "tune2fs", "resize2fs", "lvm", "pvresize", "lvextend",
        ] {
            assert!(!code.contains(t), "本模块不该提到 {t:?}：那是 fsops/movepart 的知识");
        }
    }
}