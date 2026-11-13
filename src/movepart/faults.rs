//! 崩溃注入点的唯一声明处；test-faults 构建按 DISKEDIT_FAULT 中止进程。


/// 崩溃注入点的唯一声明处：一条声明同时生成"注入"与"存根"两个版本，签名不可能只在
/// 一侧成立。test-faults 构建下比对 DISKEDIT_FAULT——无参点比 `标签`、带参点比
/// `标签:参数`——命中即 abort；其余构建下是空函数（参数照收，调用点在两种构建下同形）。
/// 注入路径不写日志、不做清理：任何额外输出都会改变被测对象
macro_rules! fault_points {
    ($(
        $(#[$doc:meta])*
        $name:ident ( $( $arg:ident : $ty:ty ),* ) = $tag:literal $(, $tag_arg:ident)?;
    )*) => {
        $(
            $(#[$doc])*
            #[cfg(feature = "test-faults")]
            pub(crate) fn $name($( $arg: $ty ),*) {
                let Ok(v) = std::env::var("DISKEDIT_FAULT") else { return };
                let want = String::from($tag);
                $( let want = format!("{want}:{}", $tag_arg); )?
                if v == want {
                    std::process::abort();
                }
            }
        )*
        $(
            $(#[$doc])*
            #[cfg(not(feature = "test-faults"))]
            pub(crate) fn $name($( $arg: $ty ),*) {
                $( let _ = $arg; )*
            }
        )*
        /// 声明清单的书写形式；未知标签的报错正文直接由它给出，不另维护一份。
        /// 带参点写作 `标签:<n>`
        #[cfg(feature = "test-faults")]
        pub(crate) fn fault_tags() -> Vec<&'static str> {
            let mut tags = Vec::new();
            $(
                tags.push(if stringify!($($ty),*).is_empty() {
                    $tag
                } else {
                    concat!($tag, ":<n>")
                });
            )*
            tags
        }
        /// 标签是否由上面某一条声明给出：无参点精确匹配；带参点还要求参数按声明类型
        /// 解析。裸标签不算命中——注入点的比较式永远带参数，放过裸标签等于静默不触发
        #[cfg(feature = "test-faults")]
        pub(crate) fn tag_declared(spec: &str) -> bool {
            $(
                $(
                    if spec
                        .strip_prefix(concat!($tag, ":"))
                        .and_then(|rest| rest.parse::<$ty>().ok())
                        .is_some()
                    {
                        return true;
                    }
                )*
                if spec == $tag && stringify!($($ty),*).is_empty() {
                    return true;
                }
            )*
            false
        }
    };
}

fault_points! {
    /// 第 N 个 chunk 拷贝完成后 abort（非自重叠搬移按批提交，该块此时可能尚未同步；
    /// 自重叠搬移逐块落盘，故此刻已同步）
    fault_chunk(n: u64) = "chunk", n;
    /// 第 N 个 chunk 的数据已同步、checkpoint 尚未写入时 abort（同步与 durable 边界的交界面）
    fault_after_data_sync(n: u64) = "after-data-sync", n;
    /// chunk 拷贝全部完成、表项 commit 前 abort
    fault_before_entry_commit() = "before-entry-commit";
    /// 表项 commit + ckpt 推进后（下一分区拷贝开始前的条目间隙）abort
    fault_after_entry_commit(mi: usize) = "after-entry-commit", mi;
    /// 全部条目完成、扩容收尾前 abort
    fault_before_grow() = "before-grow";
    /// 扩容表写完、`table_step_done` 尚未推进时 abort（表已落与记录提交之间的交界面）
    fault_after_grow_table_write() = "after-grow-table-write";
    /// swap 条目落位重建完成后 abort
    fault_after_swap_entry(mi: usize) = "after-swap-entry", mi;
    /// 修复完成、初始 ckpt 写入前 abort
    fault_after_repair() = "after-repair";
    /// 修复事实已落盘、修复的第一个目标字节尚未写出时 abort（记录与修复写入之间的交界面）
    fault_repair_recorded() = "repair-recorded";
    /// 表步围栏已落盘、表步的第一个目标字节尚未写出时 abort（记录与表写入之间的交界面）
    fault_table_step_recorded() = "table-step-recorded";
    /// 表步写完、围栏落定之后 abort（表已写与记录提交之间的交界面）
    fault_after_table_step() = "after-table-step";
    /// 现场已绑进记录、修复尚未执行时 abort（写路径上两者之间的窗口）
    fault_after_relocation_bound() = "after-relocation-bound";
    /// swap 条目 GPT commit 后、ckpt 推进前 abort（重放须幂等：GPT 绝对赋值 + 从旧位置重读 UUID 重建）
    fault_after_swap_commit(mi: usize) = "after-swap-commit", mi;
    /// FS 缩容完成（fs_shrunk 已落盘）、数据搬移前 abort
    fault_rs_after_fs_shrink() = "rs-after-fs-shrink";
    /// 数据搬移完成、表项提交前 abort
    fault_rs_before_commit() = "rs-before-commit";
    /// 表项提交后、FS 扩容前 abort
    fault_rs_after_commit() = "rs-after-commit";
    /// 表项 commit 后、NTFS HiddenSectors 修正前 abort（重放须重入该分区并补修正）
    fault_before_hidden_fix() = "before-hidden-fix";
    /// copy 的数据复制全部完成、表项提交前 abort（与 `rs-before-commit` 同形：
    /// 不可逆的一步已经迈出，而那份"发生过什么"还没写进表）
    fault_copy_before_commit() = "copy-before-commit";
    /// MBR 分区表项写完、FS 扩满前 abort（记录里的提交前区间已落盘，重跑据此收尾）
    fault_mbr_after_table_write() = "mbr-after-table-write";
    /// 事务入口：本次调用的**任何判定与写入之前**。
    /// 与 `chunk` 成对使用，"边界前 abort 必须留下完全未改动的盘"、
    /// "边界后 abort 已在盘上留下可续传的现场"这两句话才有可自动断言的落点
    fault_before_any_write() = "before-any-write";
    /// 单文件记录已落盘、目标字节尚未写出时 abort（记录与数据之间的交界面）
    fault_record_persisted() = "record-persisted";
    /// 第 N 次目标写入已写出、记录尚未提交时 abort（N=1 即第一次写入后的交界面；
    /// 更大的 N 用来留下含多条恢复区间的记录）
    fault_record_before_commit(n: u64) = "record-before-commit", n;
    /// 记录已提交、清理尚未完成时 abort（只有 Linux 会写记录，故该点只在 Linux 声明）
    #[cfg(target_os = "linux")]
    fault_record_committed() = "record-committed";
}

// 本模块内的调用不需要它；额外暴露给在线路径（online 的 sfdisk 分界）布点用。
// 在线路径仅 Linux，故非 Linux 构建下这个再导出没有使用者
#[cfg(target_os = "linux")]
pub(crate) use fault_points;

/// 校验 `DISKEDIT_FAULT`：`Some(正文)` 表示标签不在声明表里。
///
/// 解析点放在命令入口而不是各个注入点：拼错或失效的标签必须让命令直接失败，否则
/// 它的表现与"注入点没有被走到"完全相同，测试会红在毫不相干的地方。
///
/// 声明组有两处：本文件的搬移/分区表一组，在线路径一组（模块只在 Linux 编入）
#[cfg(feature = "test-faults")]
pub(crate) fn validate_fault_env() -> Option<String> {
    let given = std::env::var("DISKEDIT_FAULT").ok()?;
    #[cfg(target_os = "linux")]
    let online_declared = crate::online::faults::tag_declared(&given);
    #[cfg(not(target_os = "linux"))]
    let online_declared = false;
    if tag_declared(&given) || online_declared {
        return None;
    }
    #[cfg(target_os = "linux")]
    let available: Vec<&str> = fault_tags()
        .into_iter()
        .chain(crate::online::faults::fault_tags())
        .collect();
    #[cfg(not(target_os = "linux"))]
    let available: Vec<&str> = fault_tags();
    Some(format!(
        "DISKEDIT_FAULT={given:?} is not a declared fault point (available: {})",
        available.join(", ")
    ))
}

/// 非 test-faults 构建不读 `DISKEDIT_FAULT`：注入点在两侧同形，存根不做比对
#[cfg(not(feature = "test-faults"))]
pub(crate) fn validate_fault_env() -> Option<String> {
    None
}
