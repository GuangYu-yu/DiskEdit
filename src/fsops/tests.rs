#![allow(clippy::let_underscore_must_use)] // 清理临时文件有意忽略失败
use super::{check_e2fsck_for_check, check_e2fsck_for_resize, check_grow_step, classify_shrink, erase_ranges, grow_target_at, leak_from, mkfs_tool, parse_fstype, parse_num_field, parse_num_lead, partition_byte_range, settle_release, swap_rebuild_support, uuid_support, DeviceScope, Growable, MkfsFs, MountLeak, ReleaseOutcome, ShrinkVerify, ToolSupport, UuidSupport};
use super::FsError;
use crate::dev::FileSource;
use crate::fsid::{self, FsKind};

/// e2fsck 2/3/4 归 `CommandFailed`：三者都说明工具运行过——"确定未写盘"
/// 的 Infra 承诺不成立。exit 2 的人工复现（e2fsck -fp 改过 root fs）无需真实环境：
/// 判定是纯函数，逐码断言变体即可
#[test]
pub(crate) fn e2fsck_exit_codes_do_not_claim_no_write() {
    assert!(check_e2fsck_for_resize(0).is_ok());
    assert!(check_e2fsck_for_resize(1).is_ok());
    for c in [2, 3, 4] {
        assert!(matches!(check_e2fsck_for_resize(c), Err(FsError::CommandFailed(_))), "exit {c}");
    }
    // 基础设施故障（8/16 等）同样走 CommandFailed，与 2/3/4 同口径
    assert!(matches!(check_e2fsck_for_resize(8), Err(FsError::CommandFailed(_))));
}

/// check 语境的判定独立于 resize：修复成功（0..=3）是 check 的本职成果，
/// 不得照搬 resize 的 REBOOT 拒绝——照搬会把修复成功报成 Failed(30)。
/// 仅 4（未修正错误）与基础设施故障是失败
#[test]
pub(crate) fn e2fsck_check_context_treats_repair_as_success() {
    for ok in [0, 1, 2, 3] {
        assert!(check_e2fsck_for_check(ok).is_ok(), "exit {ok} must pass");
    }
    assert!(matches!(check_e2fsck_for_check(4), Err(FsError::CommandFailed(_))));
    assert!(matches!(check_e2fsck_for_check(8), Err(FsError::CommandFailed(_))));
}

/// swap 的 label/uuid 走 swaplabel（util-linux）：能力判定归 Yes（只收显式值，
/// `--random` 由命令层按 Yes+NewRandom 拒绝）。这条接线若回退，set 会退回
/// "no label tool wired for swap" 的拒绝
#[test]
pub(crate) fn swap_label_and_uuid_are_wired_to_swaplabel() {
    assert!(matches!(uuid_support(Some(FsKind::Swap)), UuidSupport::Yes));
    assert!(matches!(uuid_support(Some(FsKind::Vfat)), UuidSupport::No(_)));
}

/// 搬移 swap 的重建工具矩阵：`mkswap` 恒需；镜像目标的重建对象是 loop 分区设备，
/// 还要 `losetup`；块设备用现成分区节点，不得因探测 loop 工具而拒绝
#[test]
pub(crate) fn swap_rebuild_tools_follow_the_target_kind() {
    match (swap_rebuild_support(false), swap_rebuild_support(true)) {
        (ToolSupport::Tools(image), ToolSupport::Tools(block)) => {
            assert_eq!(image, &["mkswap", "losetup"]);
            assert_eq!(block, &["mkswap"]);
        }
        _ => panic!("swap rebuild must stay a tool-list requirement"),
    }
}

/// btrfs 多设备检查的尺寸闸：分区装不下 superblock 头（< 0x10090）时在读之前
/// 拒绝——分区之外读到的是邻居的字节，比读失败更危险。装得下时全零镜像的
/// num_devices = 0（≤1）放行
#[test]
pub(crate) fn btrfs_multi_device_gate_refuses_undersized_partition() {
    let path = std::env::temp_dir().join(format!("diskedit_btrfs_gate_{}.img", std::process::id()));
    std::fs::write(&path, vec![0u8; 128 * 1024]).unwrap();
    let src = FileSource::open(&path, None).unwrap();
    assert!(super::refuse_btrfs_multi_device_at(&src, 0, 4096).is_err(), "a partition smaller than the superblock must be refused before any read");
    assert!(super::refuse_btrfs_multi_device_at(&src, 0, 128 * 1024).is_ok(), "num_devices = 0 must pass the multi-device check");
    let _ = std::fs::remove_file(&path);
}

/// `as_str` 是全仓唯一的规范名来源：任何一字漂移都会改动 info JSON 与文案，
/// 这里逐项钉死对外名
#[test]
pub(crate) fn fs_kind_and_mkfs_fs_strings_are_frozen() {
    assert_eq!(FsKind::Squashfs.as_str(), "squashfs");
    assert_eq!(FsKind::Erofs.as_str(), "erofs");
    assert_eq!(FsKind::Ext.as_str(), "ext");
    assert_eq!(FsKind::Xfs.as_str(), "xfs");
    assert_eq!(FsKind::Btrfs.as_str(), "btrfs");
    assert_eq!(FsKind::F2fs.as_str(), "f2fs");
    assert_eq!(FsKind::Exfat.as_str(), "exfat");
    assert_eq!(FsKind::Vfat.as_str(), "vfat");
    assert_eq!(FsKind::Ntfs.as_str(), "ntfs");
    assert_eq!(FsKind::HfsPlus.as_str(), "hfsplus");
    assert_eq!(FsKind::Apfs.as_str(), "apfs");
    assert_eq!(FsKind::Swap.as_str(), "swap");
    assert_eq!(FsKind::Lvm2Pv.as_str(), "lvm2_pv");

    assert_eq!(fsid::fs_name(None), "unknown");
    assert_eq!(fsid::fs_name(Some(FsKind::Ext)), "ext");

    // CLI 域自有字面量：ext 只在这里分裂到 2/3/4
    assert_eq!(MkfsFs::Ext2.as_str(), "ext2");
    assert_eq!(MkfsFs::Ext3.as_str(), "ext3");
    assert_eq!(MkfsFs::Ext4.as_str(), "ext4");
    // 交集转调共享：CLI 名必须与识别名逐字节一致
    for (mk, kind) in [
        (MkfsFs::Vfat, FsKind::Vfat),
        (MkfsFs::Exfat, FsKind::Exfat),
        (MkfsFs::Ntfs, FsKind::Ntfs),
        (MkfsFs::Xfs, FsKind::Xfs),
        (MkfsFs::Btrfs, FsKind::Btrfs),
        (MkfsFs::F2fs, FsKind::F2fs),
        (MkfsFs::Swap, FsKind::Swap),
        (MkfsFs::Lvm2Pv, FsKind::Lvm2Pv),
    ] {
        assert_eq!(mk.as_str(), kind.as_str());
    }
}

/// 接受集往返：11 个规范名解析回自身
#[test]
pub(crate) fn cli_fstype_names_round_trip() {
    for k in [
        MkfsFs::Ext2, MkfsFs::Ext3, MkfsFs::Ext4, MkfsFs::Vfat, MkfsFs::Exfat, MkfsFs::Ntfs,
        MkfsFs::Xfs, MkfsFs::Btrfs, MkfsFs::F2fs, MkfsFs::Swap, MkfsFs::Lvm2Pv,
    ] {
        assert_eq!(MkfsFs::from_cli_name(k.as_str()), Some(k), "{}", k.as_str());
    }
}

/// 拒绝面：识别域独有的名、大小写变体、空串一律不接受
#[test]
pub(crate) fn cli_fstype_rejects_non_canonical() {
    for s in ["ext", "unknown", "EXT4", "", "squashfs", "erofs", "hfsplus", "apfs"] {
        assert_eq!(MkfsFs::from_cli_name(s), None, "{s:?}");
    }
}

/// ext 家族不得被归一化：mke2fs 的 `-t` 需要具体到 ext2/3/4，
/// 分裂若塌回 `Ext`，`-t ext` 会成为非法参数
#[test]
pub(crate) fn mkfs_tool_keeps_ext_family_split() {
    for k in [MkfsFs::Ext2, MkfsFs::Ext3, MkfsFs::Ext4] {
        assert_eq!(mkfs_tool(k).ok().and_then(|t| t.ext_type), Some(k));
    }
    assert!(mkfs_tool(MkfsFs::Xfs).ok().is_some_and(|t| t.ext_type.is_none()));
}

/// 非法 `--fs` 的拒绝文案与分类：`UnsupportedFs`（拒绝/10），逐字节冻结
#[test]
pub(crate) fn unsupported_fstype_message_is_frozen() {
    let e = parse_fstype("zfs").unwrap_err();
    assert!(matches!(e, FsError::UnsupportedFs(_)));
    assert_eq!(
        e.to_string(),
        "unsupported fstype zfs (supported: ext2/3/4, xfs, btrfs, f2fs, vfat, exfat, ntfs, swap)"
    );
}

/// `grow_target_at` 是"这段区域里能扩的是什么"的唯一判据：普通分区取 FS 自己，
/// OpenWrt 的只读根取分区尾部的 RW 层。结论必须分开——**尚未格式化的 RW 层、空区域 /
/// 未识别 / PV 都"没有可扩的文件系统"**，但它们的空间各有下家（首次挂载建满 / 一条待办 /
/// pvresize 链），不是同一件事；认得出却不接线仍是拒绝。混为一谈要么让 FS 层命令报出一件
/// 没做过的事，要么让首启现场报假失败
#[test]
pub(crate) fn grow_target_at_separates_nothing_to_do_from_unsupported() {
    // squashfs 的 bytes_used → RW 层起点（上对齐 64K）；区域长 128K，内层自 64K 起
    const LEN: u64 = 128 * 1024;
    const REL: u64 = 64 * 1024;
    let squashfs_head = |inner: &[u8]| {
        let mut d = vec![0u8; LEN as usize];
        d[0..4].copy_from_slice(b"hsqs"); // SQUASHFS_MAGIC @0
        d[0x1c..0x1e].copy_from_slice(&4u16.to_le_bytes()); // s_major @0x1c
        d[0x28..0x30].copy_from_slice(&4096u64.to_le_bytes()); // bytes_used @0x28
        d[REL as usize..REL as usize + inner.len()].copy_from_slice(inner);
        d
    };
    let inner_ext = {
        let mut f = vec![0u8; 4096];
        f[0x438..0x43A].copy_from_slice(&0xEF53u16.to_le_bytes()); // s_magic @sb+0x38
        f
    };

    // 取 `Target`：其余结论一律视为用例失败，免去每处写一遍三臂 match
    let target = |r: Result<Growable, FsError>| match r {
        Ok(Growable::Target(t)) => t,
        Ok(Growable::OverlayPending) => panic!("expected a grow target, got OverlayPending"),
        Ok(Growable::SwapUnactivatable) => panic!("expected a grow target, got SwapUnactivatable"),
        Ok(Growable::NoFilesystem(ft)) => panic!("expected a grow target, got NoFilesystem({})", fsid::fs_name(ft)),
        Err(e) => panic!("expected a grow target, got {e}"),
    };

    // 内层 ext：可扩，且 scope 指向内层那段（不是整个分区）
    let t = target(grow_target_at(&fs_fixture("ovl_ext", squashfs_head(&inner_ext)), 3, 0, LEN));
    assert_eq!(t.fstype, FsKind::Ext);
    assert!(matches!(t.scope, DeviceScope::Range(off, len) if off == REL && len == LEN - REL));

    // 内层还是零（首启尚未格式化）：没有可扩的文件系统，但那块空间会被首次挂载的
    // fstools 建满——与下面"区域里压根没有 FS"是两种事
    assert!(matches!(
        grow_target_at(&fs_fixture("ovl_raw", squashfs_head(&[])), 3, 0, LEN),
        Ok(Growable::OverlayPending)
    ));

    // 内层是认得出却不接线的类型：拒绝，并说清是内层的类型
    let mut xfs = vec![0u8; 4096];
    xfs[0..4].copy_from_slice(b"XFSB");
    match grow_target_at(&fs_fixture("ovl_xfs", squashfs_head(&xfs)), 3, 0, LEN) {
        Err(FsError::UnsupportedFs(m)) => assert!(m.contains("overlay layer identified as xfs"), "{m}"),
        Err(e) => panic!("expected the inner layer to be named in the refusal, got {e}"),
        Ok(_) => panic!("xfs overlay layer must be refused"),
    }

    // 只有只读根、没有尾部 RW 层（bytes_used = 0）：认得却无法扩
    let mut no_layer = vec![0u8; LEN as usize];
    no_layer[0..4].copy_from_slice(b"hsqs");
    assert!(matches!(
        grow_target_at(&fs_fixture("ovl_none", no_layer), 3, 0, LEN),
        Err(FsError::UnsupportedFs(m)) if m.contains("without a trailing RW overlay layer")
    ));

    // 普通分区：可扩目标就是分区自己
    let mut plain = vec![0u8; 64 * 1024];
    plain[0x438..0x43A].copy_from_slice(&0xEF53u16.to_le_bytes());
    let t = target(grow_target_at(&fs_fixture("plain_ext", plain), 2, 0, 64 * 1024));
    assert_eq!(t.fstype, FsKind::Ext);
    assert!(matches!(t.scope, DeviceScope::Partition(2)));

    // 空区域与 LVM PV：类型原样报出，供调用方分辨（PV 有别的出路）；`unknown` 不区分
    // "空"与"认不出的类型"，故调用点的措辞不得宣称里面没有文件系统
    assert!(matches!(
        grow_target_at(&fs_fixture("plain_zero", vec![0u8; 64 * 1024]), 2, 0, 64 * 1024),
        Ok(Growable::NoFilesystem(None))
    ));
    let mut pv = vec![0u8; 64 * 1024];
    pv[512..520].copy_from_slice(b"LABELONE"); // PV label 在第 2 扇区，同 fsid 的识别口径
    pv[536..544].copy_from_slice(b"LVM2 001");
    assert!(matches!(
        grow_target_at(&fs_fixture("plain_pv", pv), 2, 0, 64 * 1024),
        Ok(Growable::NoFilesystem(Some(FsKind::Lvm2Pv)))
    ));

    // 页格式在本机激活不了的 swap（签名只落在 32K 候选位）不是"无信息"：identify 按
    // swapon 口径给 "unknown"，而探测认得它——扩容后有一条真实待办，故与空区域分道
    let mut swap32k = vec![0u8; 64 * 1024];
    swap32k[32768 - 10..32768].copy_from_slice(b"SWAPSPACE2");
    assert!(matches!(
        grow_target_at(&fs_fixture("plain_swap32k", swap32k), 2, 0, 64 * 1024),
        Ok(Growable::SwapUnactivatable)
    ));

    // 认得出却不接线的类型（exfat）：拒绝
    let mut exfat = vec![0u8; 64 * 1024];
    exfat[3..11].copy_from_slice(b"EXFAT   ");
    assert!(matches!(
        grow_target_at(&fs_fixture("plain_exfat", exfat), 2, 0, 64 * 1024),
        Err(FsError::UnsupportedFs(m)) if m.contains("cannot grow exfat")
    ));
}

/// 分区层的写盘前判定：`unknown` 与"认得却不接线"同样要由 `--no-fs` 显式担下，
/// 而 overlay 首启现场、PV、探测得出的 32K swap 都有下家，不得被这条规则误伤
#[test]
pub(crate) fn unidentified_region_needs_an_explicit_opt_out() {
    assert!(check_grow_step(Growable::OverlayPending, true).is_ok());
    assert!(check_grow_step(Growable::SwapUnactivatable, true).is_ok());
    assert!(check_grow_step(Growable::NoFilesystem(Some(FsKind::Lvm2Pv)), true).is_ok());
    match check_grow_step(Growable::NoFilesystem(None), true) {
        Err(FsError::UnsupportedFs(m)) => {
            assert!(m.contains("unknown") && m.contains("--no-fs"), "{m}")
        }
        Err(e) => panic!("the refusal must stay in the unsupported domain (refused/10), got {e}"),
        Ok(()) => panic!("an unidentified region must not pass the preflight"),
    }
}

pub(crate) fn fs_fixture(tag: &str, data: Vec<u8>) -> crate::dev::WriteSource {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("diskedit_fsops_{tag}_{}.img", std::process::id()));
    std::fs::write(&tmp, &data).unwrap();
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&tmp).unwrap();
    let size = data.len() as u64;
    crate::dev::WriteSource::unowned(FileSource {
        identity: crate::dev::TargetIdentity::resolve_image(&tmp),
        file: f,
        path: tmp,
        sector_size: 512,
        size,
        is_block: false,
        journal: None,
        fingerprint: Default::default(),
        loop_mapping: None,
    })
}

/// 分区字节范围：GPT 与 MBR 两种表来源；空槽位/越界编号/容器分区/无表均拒绝
#[test]
pub(crate) fn partition_byte_range_gpt_and_mbr() {
    // GPT：按解析时的扇区大小换算
    let mut src = fs_fixture("pbr_gpt", vec![0u8; 2 * 1024 * 1024]);
    crate::table::create_gpt(&mut src, 512, None).unwrap();
    crate::gpt_policy::add_entry(&mut src, 2048, 3000, "p", crate::table::LINUX_FS_TYPE_GUID).unwrap();
    assert_eq!(partition_byte_range(&src, 1).unwrap(), (2048 * 512, (3000 - 2048 + 1) * 512));
    assert!(partition_byte_range(&src, 2).is_err(), "empty slot must be rejected");
    assert!(partition_byte_range(&src, 99).is_err(), "out-of-range number must be rejected");
    drop(src);

    // MBR：容器分区（0x05/0x0F/0x85）无法做 FS 操作
    let mut src = fs_fixture("pbr_mbr", vec![0u8; 2 * 1024 * 1024]);
    crate::table::create_mbr(&mut src).unwrap();
    crate::table::add_mdos_entry(&mut src, 63, 200, 0x83).unwrap();
    crate::table::add_mdos_entry(&mut src, 201, 300, 0x83).unwrap();
    // 被测对象是"容器类型条目的区间判读"，类型字节直接写盘面：追加路径拒绝容器类型
    let mut lba0 = vec![0u8; 512];
    src.read_at(0, &mut lba0).unwrap();
    lba0[446 + 16 + 4] = 0x05;
    src.write_at(0, &lba0).unwrap();
    assert_eq!(partition_byte_range(&src, 1).unwrap(), (63 * 512, (200 - 63 + 1) * 512));
    let err = partition_byte_range(&src, 2).unwrap_err();
    assert!(err.to_string().contains("container"), "{err}");
    drop(src);

    // 无表
    let src = fs_fixture("pbr_none", vec![0u8; 2 * 1024 * 1024]);
    assert!(partition_byte_range(&src, 1).is_err());
}

// mountinfo 解析器仅 Linux 编译（与调用方同门控），故测试同样门控
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn mountinfo_fields_and_octal_escapes() {
    use super::{decode_octal, parse_mountinfo};
    // 固定字段 1..6，其后 optional fields 数量可变，以单独的 "-" 分界
    // （man proc_pid_mountinfo(5)）
    let e = parse_mountinfo("36 25 8:1 / /mnt/data rw,relatime shared:1 - ext4 /dev/sda1 rw")
        .expect("parse");
    assert_eq!(e.dev_no, (8, 1));
    assert_eq!(e.mount_point, "/mnt/data");
    assert_eq!(e.source, "/dev/sda1");
    // 挂载点/源路径的八进制转义须先解码才能与真实路径比对（空格 = \040）
    let e = parse_mountinfo("37 25 8:2 / /mnt/my\\040data rw - ext4 /dev/sda2 rw").expect("parse");
    assert_eq!(e.mount_point, "/mnt/my data");
    assert_eq!(decode_octal("/a\\011b"), "/a\tb");
    assert_eq!(decode_octal("/a\\xb"), "/a\\xb"); // 非八进制转义原样保留
    // 无 "-" 分界 / 字段不足 → 拒绝
    assert!(parse_mountinfo("36 25 8:1 / / rw").is_none());
    assert!(parse_mountinfo("").is_none());
}

/// 挂载点必须逐次唯一：只用 PID 命名时，崩溃残留的挂载点会让下一次挂载叠在同一个
/// 目录上，PID 复用还会直接撞上别人的残留。此处只断言纯函数部分，不必真挂载
#[test]
pub(crate) fn temp_mount_points_are_unique() {
    let a = super::mount_point();
    let b = super::mount_point();
    assert_ne!(a, b, "two calls in one process must not share a mount point");
    assert_eq!(a.parent(), b.parent(), "both live directly under the temp dir");
}

#[test]
pub(crate) fn erase_range_table_layout() {
    // 2 PiB 分区：原始表 10 行，其中 4 条 bcachefs 桶行取整后同址，被相邻重复项去重，
    // 无越界时产出 7 个区间
    let big = 2 * 1024 * 1024 * 1024 * 1024 * 1024u64; // 2 PiB
    let rs = erase_ranges(big, 512);
    assert_eq!(rs.len(), 7);
    assert_eq!(rs[0], (0, 512 * 1024));
    assert_eq!(rs[1], (64 * 1024 * 1024, 4096));
    // FastTrack：起点 = 末尾 -3087 扇区，长度 512B 恰整扇区
    assert_eq!(rs[4], (big - 3087 * 512, 512));
    // 尾部 768KiB 区间不越界
    let tail = rs.last().unwrap();
    assert_eq!(tail.1 % 512, 0);
    assert!(tail.0 + tail.1 <= big);
    // 8MiB 分区：64MiB/256GiB/1PiB 越界裁掉，头部+FT+bcachefs+尾部 = 4 区间
    let small = erase_ranges(8 * 1024 * 1024, 512);
    assert_eq!(small.len(), 4);
    assert_eq!(small[0], (0, 512 * 1024));
    assert_eq!(small[1], (8 * 1024 * 1024 - 3087 * 512, 512));
    // 尾部区间裁剪后长度 = 768K−(768K−512K) = 512K（末端到分区界）
    let tail = small.last().unwrap();
    assert_eq!(tail.0 + tail.1, 8 * 1024 * 1024);
    // 256GiB 分区：头部+64MiB+FT+bcachefs+尾部 = 5 区间
    let mid = erase_ranges(256 * 1024 * 1024 * 1024u64, 512);
    assert_eq!(mid.len(), 5);
    assert_eq!(mid[1], (64 * 1024 * 1024, 4096));
    // bcachefs 桶对齐：-1MiB 先按 rounding 下取整再落扇区界
    // （part_len−1MiB 非桶对齐时命中更低位）
    let odd = erase_ranges(8288 * 1024, 512);
    assert_eq!(odd[2], (7168 * 1024, 4096)); // floor(7288K, 128K) = 7168K
}

/// 卸载侧的判定：只有"跑完且退出码 0"算没有待办。非零退出与工具缺失都不能
/// 证明已卸掉——挂载中的目录 rmdir 必然失败，"用后即删"就此失效，故各留一条待办
#[test]
pub(crate) fn release_outcome_decides_whether_a_cleanup_todo_exists() {
    assert!(leak_from("/mnt/x", ReleaseOutcome::Released).is_none());

    let failed = leak_from("/mnt/x", ReleaseOutcome::Failed("target is busy".into())).unwrap();
    assert_eq!(failed.point, "/mnt/x");
    assert_eq!(failed.why, "target is busy", "the tool's own words are the reason");
    assert_eq!(failed.hint(), "umount /mnt/x");

    let missing = leak_from("/mnt/x", ReleaseOutcome::Unavailable("umount not found".into())).unwrap();
    assert_eq!(missing.why, "umount not found");
    assert_eq!(missing.point, "/mnt/x");
}

/// 核心结果与清理待办的合成：两个事实各归各位，不揉成一个结果——
/// 核心成功时待办随 `Ok` 浮出（卸载也成功则为 `None`）；核心失败时主错误原样返回，
/// 清理待办只降为一条告警（残留挂载点仍要让人看见，但不能改写主错误）
#[test]
pub(crate) fn core_failure_and_cleanup_failure_stay_separate() {
    let leak = || Some(MountLeak { point: "/mnt/x".into(), why: "target is busy".into() });

    let (out, warn) = settle_release(Ok(()), None);
    assert!(matches!(out, Ok(None)), "core ok + release ok has nothing left over");
    assert!(warn.is_none());

    let (out, warn) = settle_release(Ok(()), leak());
    assert!(matches!(out, Ok(Some(_))), "core ok carries the cleanup todo out through Ok");
    assert!(warn.is_none(), "a successful core op reports the todo, not a warning");

    let (out, warn) = settle_release(Err(FsError::unsupported("boom")), None);
    assert!(out.is_err(), "core failure stays the main error");
    assert!(warn.is_none());

    let (out, warn) = settle_release(Err(FsError::unsupported("boom")), leak());
    assert!(out.is_err(), "the cleanup todo must not rewrite the main error");
    assert!(warn.is_some(), "a leftover mount point is still surfaced when the core op failed");
}

#[test]
pub(crate) fn erase_ranges_boundaries() {
    const K: u64 = 1024;
    const M: u64 = 1024 * K;
    // part_len 恰为 rounding（bcachefs 128K 桶）整数倍：pos = part_len−1M 落桶界，无需取整
    // （FT 行起点算成负数后被裁剪丢弃，bcachefs 是第 2 个产出区间）
    let exact = erase_ranges(M + 128 * K, 512);
    assert_eq!(exact[1], (128 * K, 4 * K));
    // 多 1 字节：floor 后命中同一桶界
    let plus1 = erase_ranges(M + 128 * K + 1, 512);
    assert_eq!(plus1[1], (128 * K, 4 * K));
    // 1MiB 分区：头部 512K + 尾部 512K——尾部行（-512K, 768K）跨尾裁剪保留半段
    // （不整段跳过），bcachefs 四行起点被 floor 到 0、产出的 (0,4K) 与头部不同址故保留
    // （重复写零幂等，只去重相邻的完全相同项，不做区间合并）
    let tiny = erase_ranges(M, 512);
    assert!(tiny.contains(&(0, 512 * K)));
    assert!(tiny.contains(&(512 * K, 512 * K)));
    assert!(tiny.iter().all(|&(s, l)| s + l <= M && s < M));
    // part_len = 1536K：尾部区间（from_end=512K）起点 = 1M（256K 取整后恰对齐），与头部不重叠
    let tail_fit = erase_ranges(1536 * K, 512);
    assert_eq!(tail_fit.last().unwrap(), &(M, 512 * K));
    // 所有区间：整扇区、落在分区界内（不同 rounding 的桶对齐不保证产出有序，
    // 未排序——wipe_zero 逐区间写零与顺序无关）
    for len in [M, 768 * K + 1, M + 128 * K + 1, 8 * M + 1, 8288 * K] {
        for &(s, l) in erase_ranges(len, 512).iter() {
            assert!(s % 512 == 0 && l % 512 == 0 || s + l == len, "tail clip may end unaligned at part end");
            assert!(s + l <= len && s < len);
        }
    }
}

#[test]
pub(crate) fn e2fsck_exit_code_semantics() {
    // man e2fsck 按位或语义（bit1 = 改 root fs 须重启）：resize 判据 0/1 可继续，
    // 2/3/4 拒绝，更高位为基础设施失败；check 判据 0..=3 成功
    for ok in [0, 1] {
        assert!(check_e2fsck_for_resize(ok).is_ok(), "exit {ok} must pass");
    }
    for refuse in [2, 3, 4, 8, 16] {
        assert!(check_e2fsck_for_resize(refuse).is_err(), "exit {refuse} must fail");
    }
    for ok in [0, 1, 2, 3] {
        assert!(check_e2fsck_for_check(ok).is_ok(), "check exit {ok} must pass");
    }
    for refuse in [4, 8, 16] {
        assert!(check_e2fsck_for_check(refuse).is_err(), "check exit {refuse} must fail");
    }
}

#[test]
pub(crate) fn parses_tool_output_formats() {
    // resize2fs -P / dumpe2fs -h 实际输出格式样本
    let a = "Estimated minimum size of the filesystem: 12345\n";
    assert_eq!(parse_num_field(a, "Estimated minimum size of the filesystem:").unwrap(), 12345);
    let b = "meta-data=/dev/loop0\nBlock size:               4096\n";
    assert_eq!(parse_num_field(b, "Block size:").unwrap(), 4096);
    assert!(parse_num_field("nothing here", "Block size:").is_err());
}

#[test]
pub(crate) fn parses_shrink_read_back_output_formats() {
    // 三个回读命令的实际输出样本（本机实测：dumpe2fs 1.47.2 / ntfsresize v2022.10.3 /
    // btrfs-progs v6.17.1）
    let ext = "Filesystem volume name:   <none>\nBlock count:              8192\nBlock size:               4096\n";
    assert_eq!(parse_num_lead(ext, "Block count:").unwrap(), 8192);
    assert_eq!(parse_num_lead(ext, "Block size:").unwrap(), 4096);
    // ntfsresize --info 的尺寸行后面还跟着人读单位，取开头的数字
    let ntfs = "Cluster size       : 4096 bytes\nCurrent volume size: 33554944 bytes (34 MB)\n";
    assert_eq!(parse_num_lead(ntfs, "Current volume size:").unwrap(), 33_554_944);
    // dump-super -f 里还有 dev_item.total_bytes 与 backup_total_bytes，行首锚定只取主超级块那条
    let btrfs = "total_bytes             314572800\ndev_item.total_bytes    268435456\n             backup_total_bytes:     1073741824\n";
    assert_eq!(parse_num_lead(btrfs, "total_bytes").unwrap(), 314_572_800);
}

/// 缩容工具成功返回后的回读判定：正常（含向下取整）、超出请求值、读不出来三种结局
/// 各有各的出路，不能塌缩成"成功/失败"
#[test]
pub(crate) fn shrink_read_back_decides_between_within_over_and_unverifiable() {
    let hint = "dumpe2fs -h <partition device>";
    // 回读值等于请求值（ntfs 实测如此）：正常成功，成功行同时给两个数
    let v = classify_shrink(Ok(33_554_944), 33_554_944, "ntfsresize", hint).unwrap();
    assert!(matches!(v, ShrinkVerify::Within(33_554_944)));
    assert_eq!(
        v.ok_line(33_554_944).as_deref(),
        Some("fs shrunk to 33554944 bytes (requested 33554944)")
    );
    assert!(v.pending(3).is_none(), "落点在请求范围内没有待办");
    // 工具按自身粒度向下取整（ext 实测：请求 33,554,944 → 8192×4096 = 33,554,432）
    let v = classify_shrink(Ok(33_554_432), 33_554_944, "resize2fs", hint).unwrap();
    assert!(matches!(v, ShrinkVerify::Within(33_554_432)));
    assert_eq!(
        v.ok_line(33_554_944).as_deref(),
        Some("fs shrunk to 33554432 bytes (requested 33554944)")
    );
    // 落点超出请求值：工具行为不符合预期，分区不能按请求尺寸缩进去
    let e = match classify_shrink(Ok(67_108_864), 33_554_944, "resize2fs", hint) {
        Err(e) => e,
        Ok(_) => panic!("落点超出请求值必须报成失败"),
    };
    assert!(
        matches!(e, FsError::CommandFailed(ref m) if m.contains("67108864") && m.contains("33554944")),
        "超出请求值要报成工具行为不符合预期: {e}"
    );
    // 读不出来：不是缩容失败，而是一条"结果无法验证"的待办
    let v = classify_shrink(Err(std::io::Error::other("dumpe2fs: command not found")), 33_554_944, "resize2fs", hint)
        .unwrap();
    assert!(v.ok_line(33_554_944).is_none(), "回读不出来时没有成功行");
    let p = v.pending(3).expect("回读不出来要给出一条待办");
    assert_eq!(p.part, 3);
    assert_eq!(p.hint, hint);
    assert!(p.detail.contains("could not be read back"), "{:?}", p.detail);
}