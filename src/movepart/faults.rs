//! 崩溃注入点的唯一声明处；test-faults 构建按 DISKEDIT_FAULT 中止进程。


// ---------- apply ----------

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
            #[cfg(not(feature = "test-faults"))]
            pub(crate) fn $name($( $arg: $ty ),*) {
                $( let _ = $arg; )*
            }
        )*
    };
}

fault_points! {
    /// 第 N 个 chunk 落盘后 abort
    fault_chunk(n: u64) = "chunk", n;
    /// chunk 拷贝全部完成、表项 commit 前 abort
    fault_before_entry_commit() = "before-entry-commit";
    /// 表项 commit + ckpt 推进后（下一分区拷贝开始前的条目间隙）abort
    fault_after_entry_commit(mi: usize) = "after-entry-commit", mi;
    /// 全部条目完成、扩容收尾前 abort
    fault_before_grow() = "before-grow";
    /// swap 条目落位重建完成后 abort
    fault_after_swap_entry(mi: usize) = "after-swap-entry", mi;
    /// 修复完成、初始 ckpt 写入前 abort
    fault_after_repair() = "after-repair";
    /// swap 条目 GPT commit 后、ckpt 推进前 abort（重放须幂等：GPT 绝对赋值 + 从旧位置重读 UUID 重建）
    fault_after_swap_commit(mi: usize) = "after-swap-commit", mi;
    /// resize_part 路径第 N 个 chunk 落盘后 abort
    fault_rs_chunk(n: u64) = "rs-chunk", n;
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
    /// 事务入口：本次调用的**任何判定与写入之前**。
    /// 与下面的 `chunk`/`rs-chunk` 成对使用，"边界前 abort 必须留下完全未改动的盘"、
    /// "边界后 abort 已在盘上留下可续传的现场"这两句话才有可自动断言的落点
    fault_before_any_write() = "before-any-write";
}

// 本模块内的调用不需要它；额外暴露给在线路径（online 的 sfdisk 分界）布点用。
// 在线路径仅 Linux，故非 Linux 构建下这个再导出没有使用者
#[cfg(target_os = "linux")]
pub(crate) use fault_points;