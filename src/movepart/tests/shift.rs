#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败
use super::*;
use super::support::*;

/// 条目循环未走完（swap 重建失败就地停止）时扩容收尾必须跳过：grow_end_for 按全体
/// moves 取 min(new_first)−1，会把尚未搬移成员的旧位划进扩容区，提交即写出与未搬移
/// 分区重叠的表项。swap 条目只给 64 扇区（32KiB，低于 mkswap 的最小可建容量）：
/// Linux 上工具齐备时重建按容量拒绝；非 Linux 或工具缺失（mkswap 或镜像所需的
/// losetup）时以环境不满足（30）在写盘前停下（不建 ckpt）——分类走 outcome 的
/// 唯一映射点，与其它 require_linux / 缺工具路径同源。恢复段另起一份未动过的现场：
/// 已有 relocation ckpt 时不做能力拒绝，缺工具/重建失败只以 pending 收场并保留现场
#[test]
pub(crate) fn swap_rebuild_failure_skips_growth_finalization() {
    const SWAP: [u8; 16] = [0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F];
    // root 2048..6143 | home 6144..10239 | swap 10240..10303（64 扇区）
    let fixture = || plan_fixture(&[
        (1, [0x11; 16], 2048, 6143),
        (2, [0x22; 16], 6144, 10239),
        (3, SWAP, 10240, 10303),
    ]);

    // 新跑（无 ckpt）：工具不齐时必须在写盘前拒绝，不建立 pending 事务
    let (mut src, path) = fixture();
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let plan = make_plan(&mut src, 1).unwrap();
    assert!(plan.moves.iter().any(|m| m.is_swap), "fixture must carry a movable swap");
    // 平台闸先于工具探测：非 Linux 上即使两个工具名都在 PATH，也按环境不满足断言
    let tools_ready = cfg!(target_os = "linux")
        && crate::fsops::find_tool("mkswap").is_ok()
        && crate::fsops::find_tool("losetup").is_ok();
    if !tools_ready {
        let fresh = ap(&mut src, &plan, 1024 * 1024, true, &mut |_| {});
        assert_eq!(
            fresh.exit_code(),
            30,
            "a new swap relocation without the rebuild tools must stop before writing: {fresh:?}"
        );
        assert!(!ckpt_path.exists(), "a refusal must not create a checkpoint");
        let g = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!((g.entries[0].starting_lba, g.entries[0].ending_lba), (2048, 6143));
        assert_eq!((g.entries[2].starting_lba, g.entries[2].ending_lba), (10240, 10303));
    } else {
        // 工具在：swap 只有 64 扇区（32KiB，低于 mkswap 最小容量），重建仍以 pending 收场
        let fresh = ap(&mut src, &plan, 1024 * 1024, true, &mut |_| {});
        assert_eq!(fresh.exit_code(), 20, "the fresh run ends as pending: {fresh:?}");
    }
    drop(src);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&ckpt_path);

    // 恢复：未动过的现场 + 已有 relocation ckpt —— 不做写盘前的能力拒绝，
    // 缺工具/重建失败只能以 pending 收场，现场（ckpt）必须保留
    let (mut src, path) = fixture();
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    let plan = make_plan(&mut src, 1).unwrap();
    let ckpt = Checkpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: src.size, ss: plan.ss, grow_part: plan.grow_part,
        last_usable_lba: plan.last_usable_lba, moves: plan.moves.clone(), kind: plan.kind,
        cur_index: 0, chunks_done: 0, chunk_bytes: 1024 * 1024,
        fp: src.fingerprint, map: src.loop_mapping,
    };
    atomic_write_ckpt(&ckpt_path, &ckpt.serialize()).unwrap();
    let o = ap(&mut src, &plan, 1024 * 1024, true, &mut |_| {});
    match &o {
        crate::outcome::Outcome::Applied { pending, .. } => {
            assert_eq!(pending.len(), 1, "须报一条待办：{pending:?}");
            assert_eq!(pending[0].kind, crate::outcome::PendingKind::Swap);
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    assert!(!o.is_complete());
    assert_eq!(o.exit_code(), 20, "swap 未重建，不能报完全成功");
    // 扩容收尾被跳过：home 仍在旧位，root 终点未动，表无重叠
    let g = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!((g.entries[0].starting_lba, g.entries[0].ending_lba), (2048, 6143));
    assert_eq!((g.entries[1].starting_lba, g.entries[1].ending_lba), (6144, 10239));
    assert!(ckpt_path.exists(), "有待办时 checkpoint 必须保留");
    drop(src);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&ckpt_path);
}

/// 续跑前的位置一致性校验（R2）：ckpt 是持久化输入，而续跑用的 plan 由 ckpt 直接重建，
/// 拿两者互比是自循环——唯一独立证据是盘上**当前**起点。此处把一条尚未开始搬移的
/// 条目挪到别处，模拟"表在作业中断后被第三方改过"：必须拒绝，而不是把别人的布局
/// 当成自己的续跑点继续搬
#[test]
pub(crate) fn resume_refuses_when_a_pending_partition_moved_on_disk() {
    const SWAP: [u8; 16] = [0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F];
    // root 2048..6143 | home 6144..10239 | swap 10240..12287
    let (mut src, path) = plan_fixture(&[
        (1, [0x11; 16], 2048, 6143),
        (2, [0x22; 16], 6144, 10239),
        (3, SWAP, 10240, 12287),
    ]);
    let chunk: u64 = 1024 * 1024;
    let plan = make_plan(&mut src, 1).unwrap();
    // 执行序：[swap, home]；home 的 first_lba 是 6144，目标位在 first_lba + delta
    assert!(plan.moves.len() >= 2, "the fixture must produce relocations");
    let home = plan.moves.iter().find(|m| m.part_num == 2).expect("home must be a movable");
    assert_ne!(home.delta_lba, 0, "home must need a real relocation for this test");
    // 预置"尚未开始搬移"的现场：cur_index=0 ⇒ home（moves 未达下标）必须仍在 first_lba
    let ckpt = Checkpoint {
        proof: crate::dev::LiveProof::Inode,
        disk_size: src.size, ss: plan.ss, grow_part: plan.grow_part,
        last_usable_lba: plan.last_usable_lba, moves: plan.moves.clone(), kind: plan.kind,
        cur_index: 0, chunks_done: 0, chunk_bytes: chunk,
        fp: src.fingerprint, map: src.loop_mapping,
    };
    atomic_write_ckpt(src.identity.checkpoint_path(), &ckpt.serialize()).unwrap();
    drop(src);

    // 篡改现场：把 home 挪到一个空闲位置（既非 first_lba、也非任何目标位），改主副本
    // 起点/终点并重算主数组与主头的 CRC。盘尾备份保持原样——解析仍有一份自洽副本
    let arr = 2 * 512usize; // 主 Entry Array 起点 LBA 2
    let e = arr + 128; // 条目 2（索引 1）
    let mut raw = std::fs::read(&path).unwrap();
    raw[e + 32..e + 40].copy_from_slice(&20000u64.to_le_bytes());
    raw[e + 40..e + 48].copy_from_slice(&24095u64.to_le_bytes());
    let arr_crc = table::crc32(&raw[arr..arr + 128 * 128]);
    raw[512 + 88..512 + 92].copy_from_slice(&arr_crc.to_le_bytes());
    raw[512 + 16..512 + 20].fill(0);
    let hdr_crc = table::crc32(&raw[512..512 + 92]);
    raw[512 + 16..512 + 20].copy_from_slice(&hdr_crc.to_le_bytes());
    std::fs::write(&path, &raw).unwrap();

    let src = plan_open(&path);
    let g = geom_of(&src);
    // 篡改确实落到几何上：home 现在停在 20000，而非 ckpt 记的 first_lba=6144
    assert_eq!(crate::gpt_policy::live_entry(&g, 2).unwrap().starting_lba, 20000);
    // 直呼事前判定，隔离出拒绝来源：必须是 R2 的位置校验，而不是更靠前的占用/场景/槽位判定
    match prepare_apply(&src, &g, &plan, chunk, true, &mut |_| {}) {
        Err(Fail::Refused(m)) => assert!(m.contains("not where the checkpoint says"), "{m}"),
        Err(other) => panic!("expected a position refusal, got {other:?}"),
        Ok(_) => panic!("expected a position refusal, but the resume was accepted"),
    }
    assert!(src.identity.checkpoint_path().exists(), "a refusal must not consume the checkpoint");
    drop(src);
    let _ = std::fs::remove_file(&path);
}

/// UUID 回退：旧起点已被上一次 mkswap 的新头覆盖时，从新起点取同一身份；
/// 旧起点仍有有效 UUID 时不得被新起点的值替代
#[test]
pub(crate) fn swap_identity_falls_back_to_the_new_location_only_without_an_old_uuid() {
    let ss = 512u64;
    let len_lba = 8u64;
    let old_start = 0u64;
    let new_start = 8u64;
    let mut data = vec![0u8; ((new_start + len_lba) * ss) as usize];
    let put = |d: &mut [u8], start_lba: u64, uuid: [u8; 16]| {
        let base = (start_lba * ss) as usize;
        d[base + 4096 - 10..base + 4096].copy_from_slice(b"SWAPSPACE2");
        d[base + 1036..base + 1052].copy_from_slice(&uuid);
    };
    // 旧起点无签名，新起点有新头 → 回退取新起点的 UUID
    put(&mut data, new_start, [0x7A; 16]);
    let src = crate::support::src_from("swapfb", &data);
    let path = src.path.clone();
    assert_eq!(swap_identity_preferring(&src, old_start, new_start, len_lba, ss).0, Some([0x7A; 16]));
    // 旧起点有有效头 → 以旧起点为准
    put(&mut data, old_start, [0x11; 16]);
    let src2 = crate::support::src_from("swapfb2", &data);
    assert_eq!(swap_identity_preferring(&src2, old_start, new_start, len_lba, ss).0, Some([0x11; 16]));
    drop(src2);
    drop(src);
    let _ = std::fs::remove_file(&path);
}

/// swap 身份读取的字节保真：sws_volume 是 16 字节定长字段，mkswap -L 存入时不校验编码，
/// 解成 String 必改写非法序列（Latin-1 会把 0xFF 变成 U+00FF 的两个字节），
/// 于是重建 swap 时 -L 写回的值与盘上原值不同
#[test]
pub(crate) fn swap_identity_label_is_raw_bytes() {
    let ss = 512u64;
    let ps = 4096usize;
    let mut data = vec![0u8; 4 * ps];
    data[ps - 10..ps].copy_from_slice(b"SWAPSPACE2");
    data[1036..1052].copy_from_slice(&[0x5A; 16]);
    data[1052..1068].copy_from_slice(&[b'A', 0xFF, b'B', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let src = crate::support::src_from("swaplabel", &data);
    let path = src.path.clone();
    let (uuid, label) = read_swap_identity(&src, 0, data.len() as u64 / ss, ss);
    assert_eq!(uuid, Some([0x5A; 16]));
    assert_eq!(label.as_deref(), Some(&[b'A', 0xFF, b'B'][..]));
    drop(src);
    let _ = std::fs::remove_file(&path);
}

/// 32K 页格式的 swap 在本机（4K 页）认不出类型：identify 走 swapon 口径给出 "unknown"，
/// 扩容收尾因此整段跳过 FS 步骤。但它确实是"分区已扩、swap 未重建"这一未完成的后置条件，
/// 必须报 Pending（20）而不是又一次静默成功。MBR 的 resize 与 apply 两条路径已有同一判据，
/// 这条守住 resize_part 那条
#[test]
pub(crate) fn unactivatable_swap_growth_is_pending_not_silent_success() {
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    drop(src);
    // 只在 32K 候选位置放 swap 签名：swapon 口径（候选集不含 32K）看不到它 → identify 给 unknown
    let base = 2048 * 512;
    let mut raw = std::fs::read(&p).unwrap();
    raw[base + 32768 - 10..base + 32768].copy_from_slice(b"SWAPSPACE2");
    std::fs::write(&p, &raw).unwrap();

    let mut src = plan_open(&p);
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    assert_eq!(crate::fsid::identify(&src, base as u64, 4096 * 512).unwrap(), None);
    assert!(crate::fsid::unactivatable_swap(&src, base as u64, 4096 * 512).unwrap());

    // 起点不变、向右扩
    let o = rsize(&mut src,1, 2048, 8191, 1024 * 1024, false, &mut |_| {});
    match &o {
        crate::outcome::Outcome::Applied { pending, .. } => {
            assert_eq!(pending.len(), 1, "须报一条待办：{pending:?}");
            assert_eq!(pending[0].kind, crate::outcome::PendingKind::Swap);
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    assert!(!o.is_complete());
    assert_eq!(o.exit_code(), 20, "分区已扩而 swap 未重建，不能报完全成功");

    drop(src);
    let _ = std::fs::remove_file(&ckpt_path);
    let _ = std::fs::remove_file(&p);
}

/// FS 步留下待办时 checkpoint 保留，重跑同一请求落到"已提交、补 FS 步"分支：
/// 重跑既不能静默完成（Fresh 下旧区间=新区间 ⇒ Leave ⇒ 报 0 而 swap 仍未重建），
/// 也不得再次搬动数据。夹具带搬移：chunk 循环的逐 chunk save 是这条路径的落盘来源
#[test]
pub(crate) fn swap_pending_keeps_checkpoint_and_rerun_retries_the_fs_step() {
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 10239)]);
    drop(src);
    // 32K 候选位置放 swap 签名：identify 认不出它；搬移把它一并拷到新区间的
    // 同一相对偏移，收尾的 swap 探测在新区间探得到 → 记成待办
    let base = 2048 * 512;
    let mut raw = std::fs::read(&p).unwrap();
    raw[base + 32768 - 10..base + 32768].copy_from_slice(b"SWAPSPACE2");
    std::fs::write(&p, &raw).unwrap();

    let mut src = plan_open(&p);
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();
    // 旧区间哨兵：正向搬移不与目标区重叠，旧区字节在整轮作业里都不该被改写
    let marker = base as u64 + 4096;
    let sentinel = [0xA5u8; 512];
    src.write_at(marker, &sentinel).unwrap();

    // move+grow：起点 2048 → 12288，区间 8192 LBA → 12288 LBA，
    // 拷贝量取 min = 4 MiB ⇒ 4 个 chunk
    let o1 = rsize(&mut src, 1, 12288, 24575, 1024 * 1024, false, &mut |_| {});
    assert_eq!(o1.exit_code(), 20, "{o1:?}");
    assert!(ckpt_path.exists(), "FS 步留下待办时 checkpoint 必须保留——重跑只能靠它落到续传分支");

    let o2 = rsize(&mut src, 1, 12288, 24575, 1024 * 1024, false, &mut |_| {});
    match &o2 {
        crate::outcome::Outcome::Applied { pending, .. } => {
            assert_eq!(pending.len(), 1, "重跑须再次报出 swap 待办：{pending:?}");
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    assert!(!o2.is_complete(), "重跑不得把未完成的 FS 步报成完成");
    assert_eq!(o2.exit_code(), 20);
    assert!(ckpt_path.exists(), "重跑仍留待办时 checkpoint 继续保留");

    let mut after = [0u8; 512];
    src.read_at(marker, &mut after).unwrap();
    assert_eq!(after, sentinel, "续传只重试 FS 步，不得再次搬动数据");
    let g2 = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!((g2.entries[0].starting_lba, g2.entries[0].ending_lba), (12288, 24575));

    drop(src);
    let _ = std::fs::remove_file(&ckpt_path);
    let _ = std::fs::remove_file(&p);
}

/// 纯扩容（不搬移）FS 步留下待办时 checkpoint 同样保留、重跑补试 FS 步：
/// 无搬移就没有 chunk 循环的 save 落点，开工时的这一次落盘是重跑落到
/// "已提交、补 FS 步"分支的唯一凭据——缺了它重跑按 Fresh 算出
/// 旧区间 == 新区间 ⇒ Leave ⇒ 报 0，待办从此无人认领
#[test]
pub(crate) fn pure_growth_fs_pending_keeps_checkpoint_and_rerun_retries() {
    let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    drop(src);
    // 32K 候选位置放 swap 签名：identify 认不出它，FS 步按"不可激活的 swap"记待办
    let base = 2048 * 512;
    let mut raw = std::fs::read(&p).unwrap();
    raw[base + 32768 - 10..base + 32768].copy_from_slice(b"SWAPSPACE2");
    std::fs::write(&p, &raw).unwrap();

    let mut src = plan_open(&p);
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();

    let o1 = rsize(&mut src, 1, 2048, 8191, 1024 * 1024, false, &mut |_| {});
    assert_eq!(o1.exit_code(), 20, "{o1:?}");
    assert!(ckpt_path.exists(), "纯扩容的 FS 待办同样要留 ckpt——重跑只能靠它落到补试分支");

    let o2 = rsize(&mut src, 1, 2048, 8191, 1024 * 1024, false, &mut |_| {});
    match &o2 {
        crate::outcome::Outcome::Applied { pending, .. } => {
            assert_eq!(pending.len(), 1, "重跑须再次报出 swap 待办：{pending:?}");
        }
        other => panic!("expected Applied, got {other:?}"),
    }
    assert!(!o2.is_complete(), "重跑不得把未完成的 FS 步报成完成");
    assert_eq!(o2.exit_code(), 20);

    drop(src);
    let _ = std::fs::remove_file(&ckpt_path);
    let _ = std::fs::remove_file(&p);
}

/// `dev::scene_mismatch` 的判据：**先比身份种类**（整盘 ID / 文件 inode / 无身份），同一种类
/// 内逐字段比，各字段只在**两侧都有证据**时参与（零值 = 该平台或该载体给不出这个字段，
/// 不等于"匹配"）。旧格式（`Legacy`）只写了 inode 与尺寸：当前目标同样是 inode 身份时照旧
/// 可比，是整盘身份或什么都给不出时拒绝；`Unavailable`（录制时就取不到身份）一律拒绝。
/// 该判据由 checkpoint 与 journal 两族现场共用，故在这里随 checkpoint 一侧一起测
#[test]
pub(crate) fn scene_mismatch_guards_foreign_scenes() {
    use crate::dev::{IdentityProof, LiveProof};
    let inode = IdentityProof::Live(LiveProof::Inode);
    let device_id = IdentityProof::Live(LiveProof::DeviceId);
    let unavailable = IdentityProof::Live(LiveProof::Unavailable);
    let legacy = IdentityProof::Legacy;
    let fp = |dev: u64, ino: u64, size: u64| crate::dev::TargetFingerprint { dev, ino, size, id: 0 };
    let fp_id = |size: u64, id: u64| crate::dev::TargetFingerprint { dev: 0, ino: 0, size, id };
    let scene = |fp: crate::dev::TargetFingerprint, map: Option<(u64, u64)>| {
        let mut s = crate::support::src_from("scene", &[]);
        s.fingerprint = fp;
        s.loop_mapping = map;
        s
    };
    let block = |fp: crate::dev::TargetFingerprint| {
        let mut s = scene(fp, None);
        s.is_block = true;
        s
    };
    let check = |src: &crate::dev::WriteSource,
                 rec_fp: crate::dev::TargetFingerprint,
                 rec_map: Option<(u64, u64)>,
                 proof: crate::dev::IdentityProof| checkpoint_foreign(src, &rec_fp, &rec_map, proof);

    // 镜像：inode 不符 → 拦
    let s = scene(fp(7, 42, 1024), None);
    assert!(check(&s, fp(7, 43, 1024), None, inode).unwrap().contains("fingerprint"));
    // 三者一致且双方都无映射 → 放行
    let s = scene(fp(7, 42, 1024), None);
    assert!(check(&s, fp(7, 42, 1024), None, inode).is_none());
    // 尺寸不符 → 拦（`Metadata::len()` 处处可读，故非 Unix 平台也走这条）
    let s = scene(fp(0, 0, 1024), None);
    assert!(check(&s, fp(0, 0, 2048), None, inode).unwrap().contains("size changed"));
    // 尺寸一致、inode 无证据 → 放行
    let s = scene(fp(0, 0, 1024), None);
    assert!(check(&s, fp(0, 0, 1024), None, inode).is_none());
    // 映射值不符 → 拦
    let s = scene(fp(7, 42, 1024), Some((0, 0)));
    assert!(check(&s, fp(7, 42, 1024), Some((4096, 0)), inode).unwrap().contains("loop mapping"));
    // 映射有无之别 → 拦：loop 写入的现场不能改从裸镜像续跑，反之亦然
    let s = scene(fp(7, 42, 1024), None);
    assert!(check(&s, fp(7, 42, 1024), Some((0, 0)), inode).unwrap().contains("loop device"));
    // 记录侧一个可比字段都没有：不构成证据，放行
    let s = scene(fp(0, 0, 0), None);
    assert!(check(&s, fp(7, 42, 1024), None, inode).is_none());
    // 一侧有尺寸证据、另一侧没有：不把"给不出"读成"匹配"，也不读成"不符"
    let s = scene(fp(0, 0, 0), None);
    assert!(check(&s, fp(0, 0, 1024), None, inode).is_none());
    let s = scene(fp(0, 0, 1024), None);
    assert!(check(&s, fp(0, 0, 0), None, inode).is_none());

    // 旧格式（v5）镜像现场：inode 与尺寸照旧可比
    let s = scene(fp(7, 42, 1024), None);
    assert!(check(&s, fp(7, 42, 1024), None, legacy).is_none());
    assert!(check(&s, fp(7, 43, 1024), None, legacy).unwrap().contains("fingerprint"));
    // 旧格式现场遇上现在只能给出整盘身份的目标：没有可比的身份 → 拦
    let s = block(fp_id(1 << 30, 0x1122));
    assert!(check(&s, fp(7, 42, 1024), None, legacy).unwrap().contains("before device identities"));
    // 旧格式现场遇上什么都不给出的块设备：同样拦
    let s = block(fp(0, 0, 0));
    assert!(check(&s, fp(0, 0, 0), None, legacy).unwrap().contains("before device identities"));

    // 整盘身份：同容量、同 ID 放行
    let s = block(fp_id(1 << 30, 0x1122));
    assert!(check(&s, fp_id(1 << 30, 0x1122), None, device_id).is_none());
    // 同容量、ID 不同（同名换盘）⇒ 拦
    assert!(check(&s, fp_id(1 << 30, 0x3344), None, device_id).unwrap().contains("device identity changed"));
    // 同 ID、容量不同 ⇒ 拦
    assert!(check(&s, fp_id(2 << 30, 0x1122), None, device_id).unwrap().contains("size changed"));
    // 记录侧有 ID、当前侧读不出 ⇒ 无法确认，拦
    let s = block(fp_id(1 << 30, 0));
    assert!(check(&s, fp_id(1 << 30, 0x3344), None, device_id).unwrap().contains("not readable"));
    // 记录侧有 ID、当前侧成了 backing 文件身份 ⇒ 载体变了，拦
    let s = block(fp(7, 42, 1 << 30));
    assert!(check(&s, fp_id(1 << 30, 0x3344), None, device_id).unwrap().contains("file inode identity"));

    // 录制时就没有身份（v6 的 Unavailable）：同容量也不放行，尺寸不能替代身份
    let s = block(fp_id(1 << 30, 0x1122));
    assert!(check(&s, fp_id(1 << 30, 0), None, unavailable).unwrap().contains("without any device identity"));
    let s = scene(fp(7, 42, 1024), None);
    assert!(check(&s, fp(7, 42, 1024), None, unavailable).unwrap().contains("without any device identity"));

    // loop：身份就是 backing 文件的 inode（dev / ino），相符即可续跑
    let s = block(fp(7, 42, 1024));
    assert!(check(&s, fp(7, 42, 1024), None, inode).is_none());
    // backing 换成另一个文件（同路径换了内容）⇒ 拦
    assert!(check(&s, fp(7, 43, 1024), None, inode).unwrap().contains("fingerprint"));
    // 旧格式的 loop 现场同样带 inode 证据，照旧可比
    assert!(check(&s, fp(7, 42, 1024), None, legacy).is_none());
    // 记录侧是 backing 文件、当前侧成了整盘身份 ⇒ 载体变了，拦
    let s = block(fp_id(1 << 30, 0x1122));
    assert!(check(&s, fp(7, 42, 1024), None, inode).unwrap().contains("whole-disk identity instead"));

    // "一个可比字段都没有"这一档是报出身份保障范围的那一档（undo 的 note）：全零指纹
    assert!(!crate::dev::TargetFingerprint::default().has_any_evidence());
    assert!(fp(0, 0, 1024).has_any_evidence() && fp(7, 0, 0).has_any_evidence() && fp_id(1, 1).has_any_evidence());
    let _ = std::fs::remove_file(&s.path);
}