//! 外部工具的注册表与语义化调用面。
//!
//! 工具名、固定开关与读写类别只在本文件出现；调用点按操作调用这里的函数。
//! 进程的创建仍只有 [`env`](super::env) 一处，本文件只把操作翻译成 argv。

use super::*;

/// 本工具会启动的外部程序。
///
/// 一部分变体只由 Linux 专有模块（在线 resize、lvm2）构造：非 Linux 构建里
/// 它们没有构造点，名字与包名映射仍需完整以便统一报错
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tool {
    E2fsck,
    Resize2fs,
    Dumpe2fs,
    Mke2fs,
    Tune2fs,
    Ntfsresize,
    Ntfsfix,
    Ntfslabel,
    FsckF2fs,
    ResizeF2fs,
    XfsGrowfs,
    XfsRepair,
    XfsAdmin,
    Btrfs,
    Btrfstune,
    Fatresize,
    Fatlabel,
    FsckVfat,
    Exfatlabel,
    FsckExfat,
    Mkswap,
    Swaplabel,
    Mount,
    Umount,
    Losetup,
    Udevadm,
    Sfdisk,
    Partx,
    Pvs,
    Lvs,
    Vgs,
    Pvresize,
    Lvextend,
    MkfsVfat,
    MkfsExfat,
    MkfsNtfs,
    MkfsXfs,
    MkfsBtrfs,
    MkfsF2fs,
}

impl Tool {
    /// 注册表全集：单测据此校验名字唯一与包名完备，并对账 `capabilities.rs` 的
    /// 能力清单、internals 的工具表与 CI 的安装清单
    #[cfg(test)]
    pub(crate) const ALL: &'static [Tool] = &[
        Tool::E2fsck,
        Tool::Resize2fs,
        Tool::Dumpe2fs,
        Tool::Mke2fs,
        Tool::Tune2fs,
        Tool::Ntfsresize,
        Tool::Ntfsfix,
        Tool::Ntfslabel,
        Tool::FsckF2fs,
        Tool::ResizeF2fs,
        Tool::XfsGrowfs,
        Tool::XfsRepair,
        Tool::XfsAdmin,
        Tool::Btrfs,
        Tool::Btrfstune,
        Tool::Fatresize,
        Tool::Fatlabel,
        Tool::FsckVfat,
        Tool::Exfatlabel,
        Tool::FsckExfat,
        Tool::Mkswap,
        Tool::Swaplabel,
        Tool::Mount,
        Tool::Umount,
        Tool::Losetup,
        Tool::Udevadm,
        Tool::Sfdisk,
        Tool::Partx,
        Tool::Pvs,
        Tool::Lvs,
        Tool::Vgs,
        Tool::Pvresize,
        Tool::Lvextend,
        Tool::MkfsVfat,
        Tool::MkfsExfat,
        Tool::MkfsNtfs,
        Tool::MkfsXfs,
        Tool::MkfsBtrfs,
        Tool::MkfsF2fs,
    ];

    /// 命令行上的程序名
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Tool::E2fsck => "e2fsck",
            Tool::Resize2fs => "resize2fs",
            Tool::Dumpe2fs => "dumpe2fs",
            Tool::Mke2fs => "mke2fs",
            Tool::Tune2fs => "tune2fs",
            Tool::Ntfsresize => "ntfsresize",
            Tool::Ntfsfix => "ntfsfix",
            Tool::Ntfslabel => "ntfslabel",
            Tool::FsckF2fs => "fsck.f2fs",
            Tool::ResizeF2fs => "resize.f2fs",
            Tool::XfsGrowfs => "xfs_growfs",
            Tool::XfsRepair => "xfs_repair",
            Tool::XfsAdmin => "xfs_admin",
            Tool::Btrfs => "btrfs",
            Tool::Btrfstune => "btrfstune",
            Tool::Fatresize => "fatresize",
            Tool::Fatlabel => "fatlabel",
            Tool::FsckVfat => "fsck.vfat",
            Tool::Exfatlabel => "exfatlabel",
            Tool::FsckExfat => "fsck.exfat",
            Tool::Mkswap => "mkswap",
            Tool::Swaplabel => "swaplabel",
            Tool::Mount => "mount",
            Tool::Umount => "umount",
            Tool::Losetup => "losetup",
            Tool::Udevadm => "udevadm",
            Tool::Sfdisk => "sfdisk",
            Tool::Partx => "partx",
            Tool::Pvs => "pvs",
            Tool::Lvs => "lvs",
            Tool::Vgs => "vgs",
            Tool::Pvresize => "pvresize",
            Tool::Lvextend => "lvextend",
            Tool::MkfsVfat => "mkfs.vfat",
            Tool::MkfsExfat => "mkfs.exfat",
            Tool::MkfsNtfs => "mkfs.ntfs",
            Tool::MkfsXfs => "mkfs.xfs",
            Tool::MkfsBtrfs => "mkfs.btrfs",
            Tool::MkfsF2fs => "mkfs.f2fs",
        }
    }

    /// 发行版包名（Debian 系）：写进错误信息供自动化脚本安装后重试
    pub(crate) const fn package(self) -> &'static str {
        match self {
            Tool::E2fsck | Tool::Resize2fs | Tool::Dumpe2fs | Tool::Mke2fs | Tool::Tune2fs => "e2fsprogs",
            Tool::Ntfsresize | Tool::Ntfsfix | Tool::Ntfslabel | Tool::MkfsNtfs => "ntfs-3g",
            Tool::FsckF2fs | Tool::ResizeF2fs | Tool::MkfsF2fs => "f2fs-tools",
            Tool::XfsGrowfs | Tool::XfsRepair | Tool::XfsAdmin | Tool::MkfsXfs => "xfsprogs",
            Tool::Btrfs | Tool::Btrfstune | Tool::MkfsBtrfs => "btrfs-progs",
            Tool::Fatresize => "fatresize",
            Tool::Fatlabel | Tool::FsckVfat | Tool::MkfsVfat => "dosfstools",
            Tool::Exfatlabel | Tool::FsckExfat | Tool::MkfsExfat => "exfatprogs",
            Tool::Mkswap | Tool::Swaplabel | Tool::Mount | Tool::Umount | Tool::Losetup
            | Tool::Udevadm | Tool::Sfdisk | Tool::Partx => "util-linux",
            Tool::Pvs | Tool::Lvs | Tool::Vgs | Tool::Pvresize | Tool::Lvextend => "lvm2",
        }
    }

    /// 按固定工具表解析可执行文件
    pub(crate) fn find(self) -> Result<PathBuf, FsError> {
        find_tool(self)
    }

    /// 工具自报的版本行：`--version` 的 stdout，空时取 stderr，第一条非空行。
    /// 只服务记录尾段的审计，不参与任何判定；工具缺失或输出为空时为 `None`
    pub(crate) fn version(self) -> Option<String> {
        let out = self.read(&["--version"]).ok()?;
        let text = if out.stdout.iter().any(|b| !b.is_ascii_whitespace()) {
            &out.stdout
        } else {
            &out.stderr
        };
        String::from_utf8_lossy(text)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string)
    }

    /// 写盘之前确认工具齐备
    #[cfg(target_os = "linux")]
    pub(crate) fn require(self) -> Result<(), FsError> {
        require_tool(self)
    }

    /// 工具缺失时给出带包名的拒绝文案，`what` 描述要做的事
    pub(crate) fn require_named(self, what: &str) -> Result<(), FsError> {
        self.find().map(|_| ()).map_err(|e| match e {
            FsError::ToolMissing(_) => FsError::missing(format!(
                "cannot {what}: requires `{}` (package: {}) — not found in PATH",
                self.name(),
                self.package()
            )),
            e => e,
        })
    }

    fn command(self) -> Result<GateCommand, FsError> {
        tool_command(self)
    }

    pub(crate) fn read<S: AsRef<OsStr>>(self, args: &[S]) -> Result<std::process::Output, FsError> {
        run_read(self, args)
    }

    pub(crate) fn write<S: AsRef<OsStr>>(self, args: &[S]) -> Result<std::process::Output, FsError> {
        run_write(self, args)
    }

    #[cfg(target_os = "linux")]
    fn write_input(self, args: &[&str], input: &str) -> Result<std::process::Output, FsError> {
        run_write_input(self, args, input)
    }
}

// ── ext2/3/4 ──

/// `e2fsck -fp`：修复并强制，resize 前的一致性步骤
pub(crate) fn e2fsck_repair(dev: &str) -> Result<std::process::Output, FsError> {
    Tool::E2fsck.write(&["-fp", dev])
}

/// `resize2fs <dev>`：无尺寸即扩到设备大小
pub(crate) fn resize2fs_max(dev: &str) -> Result<std::process::Output, FsError> {
    Tool::Resize2fs.write(&[dev])
}

/// `resize2fs <dev> <sectors>`：`<n>s` 单位是 512 字节扇区（man resize2fs）
pub(crate) fn resize2fs_to(dev: &str, sectors: &str) -> Result<std::process::Output, FsError> {
    Tool::Resize2fs.write(&[dev, sectors])
}

/// `resize2fs -P <dev>`：打印最小尺寸的块数估计（只读）
pub(crate) fn resize2fs_min_blocks(dev: &str) -> Result<std::process::Output, FsError> {
    Tool::Resize2fs.read(&["-P", dev])
}

/// `dumpe2fs -h <dev>`：只读超级块
pub(crate) fn dumpe2fs_superblock(dev: &str) -> Result<std::process::Output, FsError> {
    Tool::Dumpe2fs.read(&["-h", dev])
}

// ── ntfs ──

/// `ntfsresize -f -f --no-action <dev>`：演练，不改盘
pub(crate) fn ntfsresize_simulate(dev: &str) -> Result<std::process::Output, FsError> {
    Tool::Ntfsresize.read(&["-f", "-f", "--no-action", dev])
}

/// `ntfsresize -f -f <dev>`：扩到设备大小
pub(crate) fn ntfsresize_grow(dev: &str) -> Result<std::process::Output, FsError> {
    Tool::Ntfsresize.write(&["-f", "-f", dev])
}

/// `ntfsresize -f -f -s <bytes> <dev>`：缩到字节数
pub(crate) fn ntfsresize_shrink_to(dev: &str, bytes: &str) -> Result<std::process::Output, FsError> {
    Tool::Ntfsresize.write(&["-f", "-f", "-s", bytes, dev])
}

// ── f2fs ──

/// 无参 `fsck.f2fs`：默认不修复，但 SIT 段类型不一致时会把全局元数据写回
pub(crate) fn fsck_f2fs(dev: &str) -> Result<std::process::Output, FsError> {
    Tool::FsckF2fs.write(&[dev])
}

/// `resize.f2fs <dev>`：扩到设备大小
pub(crate) fn resize_f2fs(dev: &str) -> Result<std::process::Output, FsError> {
    Tool::ResizeF2fs.write(&[dev])
}

// ── xfs / btrfs ──

/// `xfs_growfs <mnt>`：挂载态扩到分区大小
pub(crate) fn xfs_growfs(mnt: &str) -> Result<std::process::Output, FsError> {
    Tool::XfsGrowfs.write(&[mnt])
}

/// `btrfs filesystem resize max <mnt>`：挂载态扩到分区大小
pub(crate) fn btrfs_resize_max(mnt: &str) -> Result<std::process::Output, FsError> {
    Tool::Btrfs.write(&["filesystem", "resize", "max", mnt])
}

/// `btrfs filesystem resize <bytes> <mnt>`：裸数字为绝对字节数，须挂载态
pub(crate) fn btrfs_resize_to(mnt: &str, bytes: &str) -> Result<std::process::Output, FsError> {
    Tool::Btrfs.write(&["filesystem", "resize", bytes, mnt])
}

// ── vfat ──

/// `fatresize -s max <dev>`
pub(crate) fn fatresize_max(dev: &str) -> Result<std::process::Output, FsError> {
    Tool::Fatresize.write(&["-s", "max", dev])
}

// ── 挂载与 loop ──

/// `mount <dev> <mnt>`：mount 自身会重放日志，按可能写盘启动
pub(crate) fn mount(dev: &OsStr, target: &OsStr) -> Result<std::process::Output, FsError> {
    Tool::Mount.write(&[dev, target])
}

/// 跑一次 `umount <mnt>`：只读地启动，不置写事实
pub(crate) fn umount(point: &std::path::Path) -> Result<std::process::Output, FsError> {
    Tool::Umount.command()?.arg(point).stdin(Stdio::null()).output().map_err(FsError::from)
}

/// `losetup -o <off> --sizelimit <len> [--sector-size <ss>] -f --show <img>`。
/// 512 扇区路径失败时按同一区间带 `--sector-size` 重试一次，返回最后一次的输出
pub(crate) fn losetup_attach(img: &OsStr, off: u64, len: u64, sector_size: u64) -> Result<std::process::Output, FsError> {
    let base_args: Vec<String> = if sector_size != 512 {
        vec![
            "-o".into(),
            off.to_string(),
            "--sizelimit".into(),
            len.to_string(),
            "--sector-size".into(),
            sector_size.to_string(),
        ]
    } else {
        vec!["-o".into(), off.to_string(), "--sizelimit".into(), len.to_string()]
    };
    let mut show = Tool::Losetup
        .command()?
        .args(&base_args)
        .arg("-f")
        .arg("--show")
        .arg(img)
        .stdin(Stdio::null())
        .output()
        .map_err(FsError::from)?;
    if !show.status.success() && sector_size == 512 {
        show = Tool::Losetup
            .command()?
            .args(["-o", &off.to_string(), "--sizelimit", &len.to_string(), "--sector-size", &sector_size.to_string(), "-f", "--show"])
            .arg(img)
            .stdin(Stdio::null())
            .output()
            .map_err(FsError::from)?;
    }
    Ok(show)
}

/// `losetup -d <loopdev>`
pub(crate) fn losetup_detach(loopdev: &str) -> Result<std::process::ExitStatus, FsError> {
    Tool::Losetup.command()?.arg("-d").arg(loopdev).status().map_err(FsError::from)
}

/// `udevadm settle --timeout=5`：等 detach 触发的 remove 事件处理完
pub(crate) fn udev_settle() -> Result<std::process::ExitStatus, FsError> {
    Tool::Udevadm.command()?.args(["settle", "--timeout=5"]).status().map_err(FsError::from)
}

// ── swap ──

/// `mkswap [-U <uuid>] [-L <label>] <dev>`
pub(crate) fn mkswap(dev: &OsStr, uuid: Option<&str>, label: Option<&OsStr>) -> Result<std::process::Output, FsError> {
    let mut args: Vec<OsString> = Vec::new();
    if let Some(u) = uuid {
        args.extend([OsString::from("-U"), OsString::from(u)]);
    }
    if let Some(l) = label {
        args.extend([OsString::from("-L"), l.to_os_string()]);
    }
    args.push(dev.to_os_string());
    Tool::Mkswap.write(&args)
}

// ── 在线分区表 ──

/// `sfdisk --no-reread --force -N <pno> <disk>`，脚本从 stdin 喂入
#[cfg(target_os = "linux")]
pub(crate) fn sfdisk_resize(disk: &str, pno: &str, script: &str) -> Result<std::process::Output, FsError> {
    Tool::Sfdisk.write_input(&["--no-reread", "--force", "-N", pno, disk], script)
}

/// `partx -u --nr <pno> <disk>`：让内核同步新表
#[cfg(target_os = "linux")]
pub(crate) fn partx_update(disk: &str, pno: &str) -> Result<std::process::Output, FsError> {
    Tool::Partx.read(&["-u", "--nr", pno, disk])
}

// ── lvm2（在线路径，仅 Linux） ──

/// `pvs --noheadings --nosuffix --units b --separator '|' -o <五字段> <dev>`
#[cfg(target_os = "linux")]
pub(crate) fn pvs_size_row(dev: &str) -> Result<std::process::Output, FsError> {
    Tool::Pvs.read(&[
        "--noheadings",
        "--nosuffix",
        "--units",
        "b",
        "--separator",
        "|",
        "-o",
        "pv_name,dev_size,pv_size,pe_start,pv_pe_count,pv_pe_alloc_count",
        dev,
    ])
}

/// `pvs --reportformat json -o pv_name,vg_name <dev>`
#[cfg(target_os = "linux")]
pub(crate) fn pvs_vg_json(dev: &str) -> Result<std::process::Output, FsError> {
    Tool::Pvs.read(&["--reportformat", "json", "-o", "pv_name,vg_name", dev])
}

/// `lvs --reportformat json -o lv_name,lv_path,devices <vg>`
#[cfg(target_os = "linux")]
pub(crate) fn lvs_json(vg: &str) -> Result<std::process::Output, FsError> {
    Tool::Lvs.read(&["--reportformat", "json", "-o", "lv_name,lv_path,devices", vg])
}

/// `lvs --noheadings --nosuffix --units b --separator '|' -o <五字段> <vg>`
#[cfg(target_os = "linux")]
pub(crate) fn lvs_state_row(vg: &str) -> Result<std::process::Output, FsError> {
    Tool::Lvs.read(&[
        "--noheadings",
        "--nosuffix",
        "--units",
        "b",
        "--separator",
        "|",
        "-o",
        "lv_name,vg_name,lv_path,lv_size,lv_attr",
        vg,
    ])
}

/// `vgs --reportformat json --units b --nosuffix -o vg_extent_size <vg>`
#[cfg(target_os = "linux")]
pub(crate) fn vgs_extent_json(vg: &str) -> Result<std::process::Output, FsError> {
    Tool::Vgs.read(&["--reportformat", "json", "--units", "b", "--nosuffix", "-o", "vg_extent_size", vg])
}

/// `pvresize <dev>`
#[cfg(target_os = "linux")]
pub(crate) fn pvresize(dev: &str) -> Result<std::process::Output, FsError> {
    Tool::Pvresize.write(&[dev])
}

/// `pvresize --yes --setphysicalvolumesize <bytes>b <dev>`：把 PV 元数据缩到指定字节
#[cfg(target_os = "linux")]
pub(crate) fn pvresize_to(dev: &str, bytes: u64) -> Result<std::process::Output, FsError> {
    Tool::Pvresize.write(&["--yes", "--setphysicalvolumesize", &format!("{bytes}b"), dev])
}

/// `pvs --segments --noheadings --nosuffix --separator '|' -o <四字段> <dev>`；
/// `pvseg_start`/`pvseg_size` 的单位是 extent（pvs(8) DESCRIPTION）
#[cfg(target_os = "linux")]
pub(crate) fn pvs_segments_row(dev: &str) -> Result<std::process::Output, FsError> {
    Tool::Pvs.read(&[
        "--segments",
        "--noheadings",
        "--nosuffix",
        "--separator",
        "|",
        "-o",
        "pv_name,pvseg_start,pvseg_size,lv_name",
        dev,
    ])
}

/// `lvextend -l +<n> -r <lv>`
#[cfg(target_os = "linux")]
pub(crate) fn lvextend(lv_path: &str, extents: u64) -> Result<std::process::Output, FsError> {
    Tool::Lvextend.write(&["-l", &format!("+{extents}"), "-r", lv_path])
}

// ── 一致性检查 ──

/// 一次只读一致性检查对应的工具与固定开关。
///
/// e2fsck / ntfsfix / xfs_repair / fsck.vfat / fsck.exfat 各以自己的 `-n` 表达
/// "只读、不修复"（man e2fsck(8)「Open the file system read-only, and assume an
/// answer of `no' to all questions」；man ntfsfix(8)「Do not write anything, just
/// show what would have been done」）。fsck.f2fs 没有等价的只读开关：无参默认
/// 不修复，但 SIT 段类型不一致时其 force 分支会写回全局元数据，故按可能写盘启动
#[derive(Clone, Copy)]
pub(crate) enum Check {
    Ext,
    Ntfs,
    F2fs,
    Xfs,
    Btrfs,
    Vfat,
    Exfat,
}

impl Check {
    pub(crate) fn for_fstype(fstype: Option<ContentKind>) -> Option<Self> {
        match fstype {
            Some(ContentKind::Filesystem(FsKind::Ext)) => Some(Check::Ext),
            Some(ContentKind::Filesystem(FsKind::Ntfs)) => Some(Check::Ntfs),
            Some(ContentKind::Filesystem(FsKind::F2fs)) => Some(Check::F2fs),
            Some(ContentKind::Filesystem(FsKind::Xfs)) => Some(Check::Xfs),
            Some(ContentKind::Filesystem(FsKind::Btrfs)) => Some(Check::Btrfs),
            Some(ContentKind::Filesystem(FsKind::Vfat)) => Some(Check::Vfat),
            Some(ContentKind::Filesystem(FsKind::Exfat)) => Some(Check::Exfat),
            _ => None,
        }
    }

    /// 跑一次检查。e2fsck 以 `-n` 跑，退出非零即检查失败；fsck.f2fs 无参也可能写回
    /// 全局元数据，按可能写盘启动
    pub(crate) fn run(self, dev: &str) -> Result<(), FsError> {
        match self {
            Check::Ext => {
                let out = Tool::E2fsck.read(&["-n", dev])?;
                match out.status.code() {
                    Some(code) => check_e2fsck_for_check(code),
                    None => Err(FsError::CommandFailed(format!("e2fsck {}", describe_exit(&out.status)))),
                }
            }
            Check::Ntfs => {
                let out = Tool::Ntfsfix.read(&["-n", dev])?;
                if !out.status.success() {
                    return Err(FsError::command(Tool::Ntfsfix.name(), &out));
                }
                Ok(())
            }
            Check::F2fs => {
                let out = Tool::FsckF2fs.write(&[dev])?;
                if !out.status.success() {
                    return Err(FsError::command(Tool::FsckF2fs.name(), &out));
                }
                Ok(())
            }
            Check::Xfs => {
                let out = Tool::XfsRepair.read(&["-n", dev])?;
                if !out.status.success() {
                    return Err(FsError::command(Tool::XfsRepair.name(), &out));
                }
                Ok(())
            }
            Check::Btrfs => {
                let out = Tool::Btrfs.read(&["check", dev])?;
                if !out.status.success() {
                    return Err(FsError::command(Tool::Btrfs.name(), &out));
                }
                Ok(())
            }
            Check::Vfat => {
                let out = Tool::FsckVfat.read(&["-n", dev])?;
                if !out.status.success() {
                    return Err(FsError::command(Tool::FsckVfat.name(), &out));
                }
                Ok(())
            }
            Check::Exfat => {
                let out = Tool::FsckExfat.read(&["-n", dev])?;
                if !out.status.success() {
                    return Err(FsError::command(Tool::FsckExfat.name(), &out));
                }
                Ok(())
            }
        }
    }
}

// ── 卷标 ──

/// 设置卷标的工具与命令形态
#[derive(Clone, Copy)]
pub(crate) enum Label {
    Ext,
    Xfs,
    Btrfs,
    Ntfs,
    Vfat,
    Exfat,
    Swap,
}

impl Label {
    pub(crate) fn for_fstype(fstype: Option<ContentKind>) -> Option<Self> {
        match fstype {
            Some(ContentKind::Filesystem(FsKind::Ext)) => Some(Label::Ext),
            Some(ContentKind::Filesystem(FsKind::Xfs)) => Some(Label::Xfs),
            Some(ContentKind::Filesystem(FsKind::Btrfs)) => Some(Label::Btrfs),
            Some(ContentKind::Filesystem(FsKind::Ntfs)) => Some(Label::Ntfs),
            Some(ContentKind::Filesystem(FsKind::Vfat)) => Some(Label::Vfat),
            Some(ContentKind::Filesystem(FsKind::Exfat)) => Some(Label::Exfat),
            Some(ContentKind::Swap) => Some(Label::Swap),
            _ => None,
        }
    }

    pub(crate) fn tool(self) -> Tool {
        match self {
            Label::Ext => Tool::Tune2fs,
            Label::Xfs => Tool::XfsAdmin,
            Label::Btrfs => Tool::Btrfs,
            Label::Ntfs => Tool::Ntfslabel,
            Label::Vfat => Tool::Fatlabel,
            Label::Exfat => Tool::Exfatlabel,
            Label::Swap => Tool::Swaplabel,
        }
    }

    pub(crate) fn write(self, dev: &str, label: &str) -> Result<std::process::Output, FsError> {
        match self {
            Label::Ext => Tool::Tune2fs.write(&["-L", label, dev]),
            Label::Xfs => Tool::XfsAdmin.write(&["-L", label, dev]),
            Label::Btrfs => Tool::Btrfs.write(&["filesystem", "label", dev, label]),
            Label::Ntfs => Tool::Ntfslabel.write(&[dev, label]),
            Label::Vfat => Tool::Fatlabel.write(&[dev, label]),
            Label::Exfat => Tool::Exfatlabel.write(&[dev, label]),
            Label::Swap => Tool::Swaplabel.write(&["-L", label, dev]),
        }
    }
}

// ── UUID ──

/// UUID 写入命令：显式值与"生成新值"在各工具里的写法不同
#[derive(Clone, Copy)]
pub(crate) enum UuidSet {
    ExtExplicit,
    ExtRandom,
    XfsExplicit,
    XfsRandom,
    BtrfsExplicit,
    BtrfsRandom,
    SwapExplicit,
    NtfsRandom,
}

impl UuidSet {
    /// 请求形式 × 类型 → 命令。`None` 表示该组合没有接线的工具
    pub(crate) fn for_request(fstype: Option<ContentKind>, random: bool) -> Option<Self> {
        match (fstype, random) {
            (Some(ContentKind::Filesystem(FsKind::Ext)), false) => Some(UuidSet::ExtExplicit),
            (Some(ContentKind::Filesystem(FsKind::Xfs)), false) => Some(UuidSet::XfsExplicit),
            (Some(ContentKind::Filesystem(FsKind::Btrfs)), false) => Some(UuidSet::BtrfsExplicit),
            (Some(ContentKind::Swap), false) => Some(UuidSet::SwapExplicit),
            (Some(ContentKind::Filesystem(FsKind::Ext)), true) => Some(UuidSet::ExtRandom),
            (Some(ContentKind::Filesystem(FsKind::Xfs)), true) => Some(UuidSet::XfsRandom),
            (Some(ContentKind::Filesystem(FsKind::Btrfs)), true) => Some(UuidSet::BtrfsRandom),
            (Some(ContentKind::Filesystem(FsKind::Ntfs)), true) => Some(UuidSet::NtfsRandom),
            _ => None,
        }
    }

    pub(crate) fn tool(self) -> Tool {
        match self {
            UuidSet::ExtExplicit | UuidSet::ExtRandom => Tool::Tune2fs,
            UuidSet::XfsExplicit | UuidSet::XfsRandom => Tool::XfsAdmin,
            UuidSet::BtrfsExplicit | UuidSet::BtrfsRandom => Tool::Btrfstune,
            UuidSet::SwapExplicit => Tool::Swaplabel,
            UuidSet::NtfsRandom => Tool::Ntfslabel,
        }
    }

    pub(crate) fn write(self, dev: &str, uuid: &str) -> Result<std::process::Output, FsError> {
        match self {
            UuidSet::ExtExplicit => Tool::Tune2fs.write(&["-U", uuid, dev]),
            UuidSet::ExtRandom => Tool::Tune2fs.write(&["-U", "random", dev]),
            UuidSet::XfsExplicit => Tool::XfsAdmin.write(&["-U", uuid, dev]),
            UuidSet::XfsRandom => Tool::XfsAdmin.write(&["-U", "generate", dev]),
            UuidSet::BtrfsExplicit => Tool::Btrfstune.write(&["-f", "-U", uuid, dev]),
            UuidSet::BtrfsRandom => Tool::Btrfstune.write(&["-f", "-u", dev]),
            UuidSet::SwapExplicit => Tool::Swaplabel.write(&["-U", uuid, dev]),
            UuidSet::NtfsRandom => Tool::Ntfslabel.write(&["--new-serial", dev]),
        }
    }
}

/// UUID 回读命令。ntfs 的 serial 没有回读出口，`for_fstype` 对它返回 `None`
#[derive(Clone, Copy)]
pub(crate) enum UuidRead {
    Ext,
    Xfs,
    Btrfs,
    Swap,
}

impl UuidRead {
    pub(crate) fn for_fstype(fstype: Option<ContentKind>) -> Option<Self> {
        match fstype {
            Some(ContentKind::Filesystem(FsKind::Ext)) => Some(UuidRead::Ext),
            Some(ContentKind::Filesystem(FsKind::Xfs)) => Some(UuidRead::Xfs),
            Some(ContentKind::Filesystem(FsKind::Btrfs)) => Some(UuidRead::Btrfs),
            Some(ContentKind::Swap) => Some(UuidRead::Swap),
            _ => None,
        }
    }

    fn command_parts(self, dev: &str) -> (Tool, Vec<&str>) {
        match self {
            UuidRead::Ext => (Tool::Tune2fs, vec!["-l", dev]),
            UuidRead::Xfs => (Tool::XfsAdmin, vec!["-u", dev]),
            UuidRead::Btrfs => (Tool::Btrfs, vec!["filesystem", "show", dev]),
            UuidRead::Swap => (Tool::Swaplabel, vec![dev]),
        }
    }

    pub(crate) fn read(self, dev: &str, fstype: Option<ContentKind>) -> Result<Option<String>, FsError> {
        let (tool, args) = self.command_parts(dev);
        let out = tool.read(&args)?;
        if !out.status.success() {
            return Err(FsError::CommandFailed(format!(
                "{} {} failed while reading the uuid back ({})",
                tool.name(),
                args.join(" "),
                describe_exit(&out.status)
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        uuid_from_output(fstype, &text).map(Some).ok_or_else(|| {
            FsError::CommandFailed(format!("{} {} printed no uuid in its output", tool.name(), args.join(" ")))
        })
    }
}

// ── 在线 FS 扩容 ──

/// 在线扩容工具：ext 对设备、xfs/btrfs 对挂载点
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
pub(crate) enum OnlineGrow {
    Ext,
    Xfs,
    Btrfs,
}

#[cfg(target_os = "linux")]
impl OnlineGrow {
    pub(crate) fn for_fstype(fstype: Option<ContentKind>) -> Option<Self> {
        match fstype {
            Some(ContentKind::Filesystem(FsKind::Ext)) => Some(OnlineGrow::Ext),
            Some(ContentKind::Filesystem(FsKind::Xfs)) => Some(OnlineGrow::Xfs),
            Some(ContentKind::Filesystem(FsKind::Btrfs)) => Some(OnlineGrow::Btrfs),
            _ => None,
        }
    }

    /// xfs/btrfs 的 resize 以挂载点为参数
    pub(crate) fn needs_mount(self) -> bool {
        !matches!(self, OnlineGrow::Ext)
    }

    pub(crate) fn tool(self) -> Tool {
        match self {
            OnlineGrow::Ext => Tool::Resize2fs,
            OnlineGrow::Xfs => Tool::XfsGrowfs,
            OnlineGrow::Btrfs => Tool::Btrfs,
        }
    }

    pub(crate) fn run(self, dev: &str, mnt: &str) -> Result<std::process::Output, FsError> {
        match self {
            OnlineGrow::Ext => Tool::Resize2fs.write(&[dev]),
            OnlineGrow::Xfs => Tool::XfsGrowfs.write(&[mnt]),
            OnlineGrow::Btrfs => Tool::Btrfs.write(&["filesystem", "resize", "max", mnt]),
        }
    }

    /// 补救提示与执行共用同一份命令映射
    pub(crate) fn command_line(self, dev: &str, mnt: &str) -> String {
        match self {
            OnlineGrow::Ext => format!("{} {dev}", Tool::Resize2fs.name()),
            OnlineGrow::Xfs => format!("{} {mnt}", Tool::XfsGrowfs.name()),
            OnlineGrow::Btrfs => format!("{} filesystem resize max {mnt}", Tool::Btrfs.name()),
        }
    }
}

// ── 只读文本 ──

/// 只读命令的合并输出；非零退出按命令失败上报
fn run_text(tool: Tool, args: &[&str]) -> io::Result<String> {
    let out = tool.read(args).map_err(io::Error::from)?;
    if !out.status.success() {
        return Err(io::Error::from(FsError::command(tool.name(), &out)));
    }
    Ok(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

pub(crate) fn dumpe2fs_text(dev: &str) -> io::Result<String> {
    run_text(Tool::Dumpe2fs, &["-h", dev])
}

pub(crate) fn ntfsresize_info_text(dev: &str) -> io::Result<String> {
    run_text(Tool::Ntfsresize, &["-f", "--info", dev])
}

pub(crate) fn btrfs_superblock_text(dev: &str) -> io::Result<String> {
    run_text(Tool::Btrfs, &["inspect-internal", "dump-super", "-f", dev])
}
