//! grow：目标解析、工具调用与挂载清理。

use super::*;

/// 本次扩容要动的**那一段**与它里面的文件系统：`scope` 是既有的范围表达（普通 FS = 分区
/// 本身；OpenWrt overlay 的 RW 层 = 分区内的一段），`fstype` 是 identify 口径的类型。
/// 判断与执行因此同源——调用点拿到它之后只把区间交给 [`GrowTarget::resize_fs`]，
/// 不自己推算 overlay 的偏移
pub struct GrowTarget {
    pub scope: DeviceScope,
    pub fstype: FsKind,
}

impl GrowTarget {
    /// 按判断期的同一份结论执行扩容
    pub fn resize_fs(&self, src: &FileSource) -> Result<Option<MountLeak>, FsError> {
        resize_fs_in(src, &self.scope, Some(self.fstype))
    }
}

/// [`grow_target_at`] 的结论。四种结果对调用方的含义不同，故不塌缩成"能不能扩"一个布尔：
/// 有活要干的只有 `Target`；其余三种都没有可扩的文件系统，区别在那块空间**有没有承担者**——
/// 未初始化的 overlay RW 层由首次挂载建满，PV 的由 pvresize 链吸收，
/// [`SwapUnactivatable`](Growable::SwapUnactivatable) 的落在一条待办上，分区层到此即完成；
/// 而"没有足够信息"没有承担者，后置条件含 FS 的命令据此拒绝（[`check_grow_step`]）
pub enum Growable {
    /// 有可扩的一段：普通分区就是它自己，OpenWrt 的只读根取分区尾部的 RW 层
    Target(GrowTarget),
    /// 尾部 RW 层里没有认得出的文件系统：`identify` 的 `None` 分不出"尚未格式化"、"是格式化
    /// 过的格式但本工具认不出"与"可供识别的字节不足"三者，本变体也不替它下结论——它只说
    /// "这段没有可扩的文件系统"，本次因此没有可写的后置条件。
    /// 放行的前提是**本命令不写这段区域**（空间随后由首次挂载时的 fstools 建满）：将来若有
    /// 路径要在放行后往里写，必须先按可用判据把三者分开，不能沿用这个"无需处理"的结论
    OverlayPending,
    /// 元数据是 swap、页格式却在本机激活不了（创建机用了 32K 页）：identify 按 swapon 口径
    /// 回 "unknown"，但这不是"没有信息"——[`unactivatable_swap`](crate::fsid::unactivatable_swap)
    /// 认得它，扩容后"swap 未重建"是一条**真实的待办**，故不并入 `NoFilesystem`
    SwapUnactivatable,
    /// 区域里没有**可扩**的文件系统：空区域、类型未识别、LVM PV。携带识别出的类型（含
    /// `None` = 未识别），供调用方分辨情形与措辞——"未识别"不等于"里面没有文件系统"，
    /// 措辞不得替它下这个结论。PV 有承担者（pvresize 链，见 `resize --grow-lv`）；
    /// 未识别没有，故分区层默认拒绝它
    NoFilesystem(Option<FsKind>),
}

/// 这段区域里能扩的文件系统是哪个 —— **唯一的判据**。写盘前的 preflight（能不能做、工具是否
/// 齐备）与写盘后的收尾（真正执行）都取它的结论；两处各按自己掌握的区间调用：写盘前是旧条目
/// 的区间，写盘后是新条目的区间，差别只在区间，不在规则。结论见 [`Growable`]；
/// `Err` 是"认得出却不接线"（含只有只读根、没有尾部 RW 层的 squashfs/erofs）或设备读不出来
pub fn grow_target_at(src: &FileSource, part: u32, base: u64, len: u64) -> Result<Growable, FsError> {
    let fstype = crate::fsid::identify(src, base, len).map_err(FsError::from)?;
    if matches!(fstype, Some(FsKind::Squashfs | FsKind::Erofs)) {
        // OpenWrt combined 布局：同分区头部只读根 + 尾部 RW overlay（fstools rootdisk.c）。
        // fstools 只把 RW 层造成 ext4/f2fs（common.c `use_f2fs()`：内核命令行
        // fstools_overlay_fstype 可强制 ext4/f2fs，auto 时按剩余空间与 F2FS_MINSIZE 决定），
        // 故内层可扩集合就是这两者
        let rel = crate::fsid::overlay_offset_at(src, base, len)
            .map_err(FsError::from)?
            .ok_or_else(|| FsError::unsupported(format!(
                "{} rootfs without a trailing RW overlay layer — nothing to grow",
                fs_name(fstype)
            )))?;
        if rel >= len {
            return Err(FsError::invalid("overlay offset beyond partition end"));
        }
        // 偏移来自 fstools 的布局约定，只是提示：该位置到底有没有 RW 层，由下一行的内层
        // 识别回答。`base + rel` 的加法在盘上可控值上仍要 checked——回绕出的假起点会落到
        // 分区之前的字节上
        let inner_base = base.checked_add(rel).ok_or_else(|| {
            FsError::invalid(format!("overlay offset {rel} added to partition start {base} overflows"))
        })?;
        // 区间量本就是字节，直接传给按字节区间识别的 identify
        let inner = crate::fsid::identify(src, inner_base, len - rel).map_err(FsError::from)?;
        let Some(inner_kind) = inner else {
            // 内层没认出签名有三种成因，`identify` 的 `None` 区分不了：① 尾部区域不足一个魔数
            // （`overlay_offset_at` 对 `len - rel` 没有下界保证）；② 是格式化过的格式但本工具
            // 认不出；③ 确实尚未格式化。本命令在这三种下都不写这段区域，故结论同为"无需处理"；
            // 但**结论里不能夹带成因**——说成"未格式化"会把 ①② 也说成空区域
            return Ok(Growable::OverlayPending);
        };
        if !matches!(inner_kind, FsKind::Ext | FsKind::F2fs) {
            return Err(FsError::unsupported(format!(
                "overlay layer identified as {} — only ext/f2fs overlays are growable",
                inner_kind.as_str()
            )));
        }
        if src.is_block {
            // Range 路径走 loop，循环节点自身的挂载检查探不到底层分区——底层分区
            // 挂载态在此显式守卫。这是判定期的时点检查：本函数每次被调用（preflight
            // 与收尾各一次）各守一次，判定之后的执行窗口不再复核
            let node = find_block_partition_node(src, part, base)?;
            require_unmounted(&node)?;
        }
        return Ok(Growable::Target(GrowTarget { scope: DeviceScope::Range(base + rel, len - rel), fstype: inner_kind }));
    }
    // 未识别的区间里有一类并非无信息——页格式激活不了的 swap（见该变体），探测认得它；
    // 其余类型不付这次读盘
    let Some(kind) = fstype else {
        return Ok(if crate::fsid::unactivatable_swap(src, base, len).map_err(FsError::from)? {
            Growable::SwapUnactivatable
        } else {
            Growable::NoFilesystem(None)
        });
    };
    // 其余按 FS 自身的可扩性分流：三态与工具清单都取自 grow_support，不在此另列一份
    match grow_support(kind) {
        ToolSupport::Tools(_) => Ok(Growable::Target(GrowTarget { scope: DeviceScope::Partition(part), fstype: kind })),
        ToolSupport::NotApplicable => Ok(Growable::NoFilesystem(Some(kind))),
        ToolSupport::Unsupported(reason) => Err(FsError::unsupported(format!("cannot grow {kind}: {reason}"))),
    }
}

/// 分区层扩容的写盘前判定：后置条件含 FS 调整，故"这段区域里是什么、那一步做得了吗"
/// 必须在**动手之前**回答——等到写表之后才发现，留下的就是"表已改、FS 未扩"。
/// 调用点已按 `--no-fs` 跳过本判定，走到这里即"FS 那一步算数"。
/// 没有可扩 FS 的几种在此分道：`OverlayPending` 的空间由首次挂载建满，PV 由 pvresize 链
/// 吸收，`SwapUnactivatable` 在收尾被探测成待办——都不是"当成功放过去"；而 `unknown`
/// 什么信息都没给，没有谁能保证那块空间会生效，默认放行就是拿无知当许可，
/// 只改分区表须由 `--no-fs` 显式担下
pub fn check_grow_step(g: Growable, is_block: bool) -> Result<(), FsError> {
    match g {
        Growable::Target(t) => check_grow(t.fstype, is_block, needs_loop(&t.scope, is_block)),
        Growable::OverlayPending | Growable::SwapUnactivatable | Growable::NoFilesystem(Some(FsKind::Lvm2Pv)) => Ok(()),
        Growable::NoFilesystem(f) => Err(FsError::unsupported(format!(
            "cannot grow {}: the filesystem type was not identified — pass --no-fs to change the partition only",
            fs_name(f)
        ))),
    }
}

/// superfloppy（无分区表，FS 即整盘）扩容：无表可写，纯 FS grow
pub fn resize_fs_whole(src: &FileSource, fstype: Option<FsKind>) -> Result<Option<MountLeak>, FsError> {
    resize_fs_in(src, &DeviceScope::Whole, fstype)
}

/// resize 分发（扩容到给定范围末端）。范围由 [`grow_target_at`] 判定，本层只管执行：
/// - ext2/3/4：先 `e2fsck -fp` 修复，再 `resize2fs <dev>` 扩满分区（离线）。
///   -fp = 强制检查 + 自动修复；退出码按位或：0-1 通过、2/3 改了 root fs 须重启、
///   4 有未修正错误即拒绝（man e2fsck EXIT CODE）。resize2fs 缺省 size = 扩到分区末端（man resize2fs）
/// - ntfs：`ntfsresize -f -f <dev>`（双 force 跳过两道确认；size 缺省 = 设备大小，man ntfsresize）
/// - f2fs：`resize.f2fs <dev>`（离线；-t 指定目标扇区数，未给定时行为以工具实现为准，man resize.f2fs）
/// - xfs：只支持挂载态扩容（man xfs_growfs）→ 临时 mount → `xfs_growfs <mnt>` → umount
/// - btrfs：临时 mount → `btrfs filesystem resize max <mnt>`（max = 占满、须挂载态，man btrfs-filesystem）→ umount
/// - vfat：`fatresize -s max <dev>`（扩满设备，man fatresize）
/// - 其余（exfat/swap…）：无已接线的扩容工具，显式拒绝
pub(crate) fn resize_fs_in(src: &FileSource, scope: &DeviceScope, fstype: Option<FsKind>) -> Result<Option<MountLeak>, FsError> {
    match fstype {
        // fsid 识别只给 0xEF53，区分不出 2/3/4；resize2fs 对三者通用（man resize2fs）
        Some(FsKind::Ext) => with_scope_device(src, scope, |dev| {
            let out = run("e2fsck", &["-fp", dev])?;
            check_e2fsck_for_resize(out.status.code().unwrap_or(-1))?;
            let out = run("resize2fs", &[dev]).map_err(|e| tool_write_maybe_started("resize2fs", dev, e))?;
            if !out.status.success() {
                return Err(FsError::command("resize2fs", &out));
            }
            Ok(())
        })
        .map(|()| None),
        Some(FsKind::Ntfs) => with_scope_device(src, scope, |dev| {
            // 先 --no-action 演练，成功才真改，失败零副作用
            let out = run("ntfsresize", &["-f", "-f", "--no-action", dev])?;
            if !out.status.success() {
                return Err(FsError::CommandFailed(format!(
                    "ntfsresize simulation failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            let out = run("ntfsresize", &["-f", "-f", dev])?;
            if !out.status.success() {
                return Err(FsError::command("ntfsresize", &out));
            }
            Ok(())
        })
        .map(|()| None),
        Some(FsKind::F2fs) => with_scope_device(src, scope, |dev| {
            // 预检：fsck.f2fs 无参 = 仅检查不修复（修复须显式 -f/-p；-a 只在内核报告 bug
            // 时才检查且默认禁用，man fsck.f2fs）。退出码负数表示失败（-1 表现为 255），
            // 非 0 即拒绝 resize
            let out = run("fsck.f2fs", &[dev])?;
            if !out.status.success() {
                return Err(FsError::CommandFailed(format!(
                    "fsck.f2fs failed (exit {}), refuse resize",
                    out.status.code().unwrap_or(-1)
                )));
            }
            let out = run("resize.f2fs", &[dev])?;
            if !out.status.success() {
                return Err(FsError::command("resize.f2fs", &out));
            }
            Ok(())
        })
        .map(|()| None),
        Some(FsKind::Xfs) => with_scope_device(src, scope, |dev| with_mount(dev, |mnt| {
            let out = run("xfs_growfs", &[mnt]).map_err(|e| tool_write_maybe_started("xfs_growfs", mnt, e))?;
            if !out.status.success() {
                return Err(FsError::command("xfs_growfs", &out));
            }
            Ok(())
        })),
        Some(FsKind::Btrfs) => {
            // 区间只解析一次：多设备闸与设备建立消费同一份 (off, len)，两次读表之间
            // 第三方改表不会让闸和实际映射看见不同的区间
            let (off, len) = scope_byte_range(src, scope)?;
            refuse_btrfs_multi_device_at(src, off, len)?;
            with_scope_device_ranged(src, scope, off, len, |dev| with_mount(dev, |mnt| {
                let out = run("btrfs", &["filesystem", "resize", "max", mnt])
                    .map_err(|e| tool_write_maybe_started("btrfs resize", mnt, e))?;
                if !out.status.success() {
                    return Err(FsError::command("btrfs resize", &out));
                }
                Ok(())
            }))
        }
        // fatresize -s max = 扩满设备（man fatresize EXAMPLES；-s max 自 1.0.4 起支持）。
        // max 可能报 "Can't have overlapping partitions"，此时降级为显式字节（分区大小-1）；
        // 属经验性 workaround，非工具规范保证。
        // FAT32 <512MB 由工具自身拒绝（man BUGS，Windows 限制）
        Some(FsKind::Vfat) => {
            // 区间与设备同源：fallback 尺寸用的 len 就是设备建立时的那一份，
            // 不再在闭包外另读一次表
            let (off, dev_len) = scope_byte_range(src, scope)?;
            with_scope_device_ranged(src, scope, off, dev_len, |dev| {
                // 兜底的字节数 = 设备长 − 1（见上）。长度 0 时减一会回绕成 u64::MAX，把
                // "无从格式化"伪装成扩到天文数字——它必须在任何工具启动之前判掉，
                // 否则就成了"工具可能已改盘"之后的一次 Refused
                let want = dev_len.checked_sub(1).ok_or_else(|| {
                    FsError::invalid(format!("vfat device length {dev_len} — cannot derive fallback size"))
                })?;
                let mut out = run("fatresize", &["-s", "max", dev])
                    .map_err(|e| tool_write_maybe_started("fatresize", dev, e))?;
                if !out.status.success() {
                    out = run("fatresize", &["-s", &want.to_string(), dev])
                        .map_err(|e| tool_write_maybe_started("fatresize", dev, e))?;
                }
                if !out.status.success() {
                    return Err(FsError::command("fatresize", &out));
                }
                Ok(())
            })
            .map(|()| None)
        }
        // swap 不是被"resize"的对象：新尺寸通过 mkswap 重建那一整块区域才生效。
        // 本命令只改 FS，不重建，故在此拒绝并指向真正会重建的入口
        Some(FsKind::Swap) => Err(FsError::unsupported(
            "swap has no in-place resize — use `resize`, which recreates the swap area so the new size takes effect",
        )),
        // squashfs/erofs 自己不可扩：能变大的是分区尾部的 RW overlay 层，那段由
        // grow_target_at 定位后交内层的分支执行。落到这里说明这个范围不是分区
        // （superfloppy），其内部没有 RW 层可扩
        Some(FsKind::Squashfs | FsKind::Erofs) => Err(FsError::unsupported(format!(
            "{} needs a trailing RW overlay layer inside a partition — nothing to grow here",
            fs_name(fstype)
        ))),
        other => Err(FsError::unsupported(format!(
            "no resize tool wired for {} (supported: ext2/3/4, ntfs, f2fs, xfs, btrfs, vfat; squashfs/erofs via their trailing RW overlay layer)",
            fs_name(other)
        ))),
    }
}

/// 本次调用的临时挂载点（纯函数：只用 PID、进程内序号与一段随机数，不碰文件系统）。
///
/// 挂载点必须唯一：只用 PID 命名时，崩溃残留的挂载点会让下一次挂载叠在同一个目录上，
/// 而 PID 复用会直接撞上别人的残留。
///
/// 名字还必须**不可预测**：`/tmp` 全局可写，可预测的名字允许别的用户抢先建出同名目录，
/// 而 `mount` 按路径解析——挂载就会落进别人的目录，回调随后在它上面跑。随机成分取自
/// `/dev/urandom`（非 unix 退到 std 的系统熵随机种子），不引入新依赖
pub(crate) fn mount_point() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "diskedit.mnt.{}.{}.{:016x}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        mount_token()
    ))
}

/// 挂载点名字里的不可预测部分
fn mount_token() -> u64 {
    #[cfg(unix)]
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut b = [0u8; 8];
        if std::io::Read::read_exact(&mut f, &mut b).is_ok() {
            return u64::from_le_bytes(b);
        }
    }
    // 没有 urandom 时可用的系统熵：`RandomState` 的键由 std 从操作系统熵源播种
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u32(std::process::id());
    h.finish()
}

/// 临时挂载 → 回调 → 卸载（xfs/btrfs 只支持挂载态扩容）。挂载点用后即删。
/// 卸载的成败不改写回调的成败：它是一条独立的清理待办，随 `Ok` 浮出
pub(crate) fn with_mount<F>(dev: &str, f: F) -> Result<Option<MountLeak>, FsError>
where
    F: FnOnce(&str) -> Result<(), FsError>,
{
    let mnt = mount_point();
    // PID 复用会撞上崩溃残留的同名目录：先试着清掉——remove_dir 只对空目录成功，
    // 非空说明残留目录上可能还挂着上次留下的文件系统，叠挂会把两个 FS 混在同一目录。
    // 同名符号链接在这一步必然 remove_dir 失败（ENOTDIR），不会被当作可复用的挂载点
    if mnt.exists() {
        std::fs::remove_dir(&mnt).map_err(|e| FsError::from(io::Error::other(format!(
            "stale mountpoint {} from a previous run is not removable (check for a residual mount there): {e}",
            mnt.display()
        ))))?;
    }
    // 目录权限 0700：xfs/btrfs 的扩容期间目标分区挂在这里数秒到数十秒，0777 & ~umask
    // 的目录会让本机其他用户能遍历并读取其中的文件。`create_dir` 而非 `create_dir_all`：
    // 父目录已存在，且目标是"本进程独占新建"——同名条目被他人抢先建出时立即失败
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&mnt).map_err(FsError::from)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir(&mnt).map_err(FsError::from)?;
    // 建目录时记下它的身份：从这一刻到 mount 之间，路径条目仍可能被同一目录里的
    // rename 替换（`/tmp` 可写），而 mount 按路径解析
    #[cfg(unix)]
    let created = std::fs::metadata(&mnt).map_err(FsError::from)?;
    // 执行走 `fsops::run`：spawn 失败 = 工具没跑起来（Io），等待失败 = 工具已启动、
    // 写没写目标无法断言（CommandFailed）。mount 自己会重放日志（对目标的一次写），
    // 把后者折成 Io 会让命令以"确定未写盘"退出
    let args = [std::ffi::OsString::from(dev), mnt.clone().into_os_string()];
    let out = run("mount", &args)?;
    if !out.status.success() {
        // mount 没成功就谈不上 umount：对从未挂载的目录卸载必然失败，
        // 那条"may remain"的告警只会把用户引向一个不存在的残留挂载点。
        // 临时目录仍要清——它是本次调用建的，mount 失败不代表它可以留下
        crate::dev::best_effort_rmdir(&mnt);
        return Err(FsError::command("mount", &out));
    }
    // 挂载后复核：路径上现在是建目录时那个 inode 才说明这次挂载落在自己的目录上。
    // 被替换时先卸载再拒绝——mount 已触发目标 FS 的日志回放（一次写），故记
    // CommandFailed；此时回调一个字节都没跑，回调后的清理也不必再做
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let same = std::fs::metadata(&mnt)
            .is_ok_and(|after| created.dev() == after.dev() && created.ino() == after.ino());
        if !same {
            // 卸载失败时没有更好的补救：拒绝理由已经说明这条路径不再属于本进程，
            // 残留挂载点由用户按报错里的路径处置
            #[allow(clippy::let_underscore_must_use)]
            let _ = run("umount", &[&mnt]);
            crate::dev::best_effort_rmdir(&mnt);
            return Err(FsError::CommandFailed(format!(
                "the mountpoint {} was replaced between its creation and the mount — the filesystem was \
                 mounted through a path this process no longer owns and has been unmounted again",
                mnt.display()
            )));
        }
    }
    let mut guard = MountGuard { point: mnt.clone(), armed: true };
    let res = f(&mnt.to_string_lossy());
    guard.armed = false;
    let leak = leak_from(&mnt.to_string_lossy(), release_mount(&mnt));
    crate::dev::best_effort_rmdir(&mnt);
    // 残留挂载点只有一条出路：核心成功时随 `Ok` 浮出成待办、核心失败时拼进主错误。
    // 两条路都进返回的 `Outcome`，不再另打一条只有 stderr 看得见的告警
    settle_release(res, leak)
}

/// 回调 panic 时的挂载守卫：栈展开会跳过 [`with_mount`] 顺序里的卸载与删目录，
/// 目标分区就保持挂载到进程重启。正常路径在回调返回后 `armed = false`——仍处
/// armed 只可能是展开，此时没有结果可报，残留只能告警
struct MountGuard {
    point: PathBuf,
    armed: bool,
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(leak) = leak_from(&self.point.to_string_lossy(), release_mount(&self.point)) {
            eprintln!("warning: the filesystem may still be mounted at {} ({})", leak.point, leak.why);
        }
        crate::dev::best_effort_rmdir(&self.point);
    }
}

/// 临时挂载点的清理待办：核心 FS 操作已经完成，挂载点却没能卸掉（工具非零退出或
/// 工具缺失），系统上仍留着占用。它是**后置条件**而不是本次操作的失败，故与
/// `Result` 的 `Err` 分道——压在 `Ok` 上随成功结果一并浮出，由调用方决定怎么表达
pub(crate) struct MountLeak {
    /// 残留的挂载点路径
    pub point: String,
    /// 没能卸掉的原因（外部工具原话，或工具缺失 / 启动失败）
    pub why: String,
}

impl MountLeak {
    /// 转成一条后置条件待办：类别标签在这里唯一给出，调用点不各写一份。
    /// 它的语义是"核心操作已完成、清理未完成"——与 `PendingKind::Fs`（FS 步没做成）
    /// 分属两件事，报告里必须看得出区别
    pub fn into_pending(self, part: u32) -> crate::outcome::Pending {
        let hint = self.hint();
        crate::outcome::Pending::new(
            part,
            crate::outcome::PendingKind::Other("mount cleanup"),
            self.why,
            hint,
        )
    }

    /// 补救命令
    pub(crate) fn hint(&self) -> String {
        format!("umount {}", self.point)
    }
}

impl std::fmt::Display for MountLeak {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "temp mount point {} was left behind ({}) — release it with `umount`", self.point, self.why)
    }
}

/// 一次卸载动作归一到三态：判定与"怎么调工具"无关，故这一层可脱离真实工具单测
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReleaseOutcome {
    /// 工具跑完且退出码 0
    Released,
    /// 工具跑完但非零退出（携带工具原话）
    Failed(String),
    /// 工具缺失或启动不了
    Unavailable(String),
}

/// 卸载结果 → 清理待办。只有"跑完且退出码 0"算没有待办：启动失败与工具缺失都不能
/// 证明已卸掉，而挂载中的目录 rmdir 必然失败，"用后即删"就此失效
pub(crate) fn leak_from(point: &str, outcome: ReleaseOutcome) -> Option<MountLeak> {
    match outcome {
        ReleaseOutcome::Released => None,
        ReleaseOutcome::Failed(why) | ReleaseOutcome::Unavailable(why) => {
            Some(MountLeak { point: point.to_string(), why })
        }
    }
}

/// 核心操作结果与清理待办的合成：核心成功时待办随 `Ok` 浮出；核心失败时残留挂载点
/// 拼进主错误的文案（分类不变），不再另走一条只有 stderr 能看到的告警——报告的
/// 错误行与 stderr 因此同源
pub(crate) fn settle_release(
    core: Result<(), FsError>,
    leak: Option<MountLeak>,
) -> Result<Option<MountLeak>, FsError> {
    match (core, leak) {
        (Ok(()), leak) => Ok(leak),
        (Err(e), Some(l)) => Err(e.with_note(&format!(
            "additionally the filesystem may still be mounted at {} ({}) — unmount it with `umount {}`",
            l.point, l.why, l.point
        ))),
        (Err(e), None) => Err(e),
    }
}

/// 跑一次 umount，把结果归一到 [`ReleaseOutcome`]
pub(crate) fn release_mount(mnt: &std::path::Path) -> ReleaseOutcome {
    let mut cmd = match tool_command("umount") {
        Ok(c) => c,
        Err(e) => return ReleaseOutcome::Unavailable(format!("umount not available ({e})")),
    };
    match cmd.arg(mnt).stdin(Stdio::null()).output() {
        Ok(o) if o.status.success() => ReleaseOutcome::Released,
        Ok(o) => ReleaseOutcome::Failed(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => ReleaseOutcome::Unavailable(format!("umount did not start: {e}")),
    }
}
