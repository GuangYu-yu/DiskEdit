use super::*;
use super::support::*;

/// 隐形条目（type 字节非零但 size 为零）在解析侧按空槽处理：resize_mdos_entry
/// 不得把它当真实分区改写——那是凭空造出一条解析器看不见的分区，操作者无从核对。
/// 与 set_mdos_boot / set_mdos_hidden / set_mdos_type 同判据同文案
#[test]
pub(crate) fn resize_mdos_entry_refuses_invisible_type_only_entry() {
    let mut src = src_from_mbr("mdosinv", &[(0x83, 2048, 0)], 32768);
    let e = resize_mdos_entry(&mut src, 1, 1000)
        .expect_err("a type-only MBR entry must be refused by resize_mdos_entry");
    assert!(matches!(&e, Fail::Refused(m) if m.contains("invisible to the parser")), "{e:?}");
    // 正常条目（type≠0 且 size≠0）不受影响——缩容仍走既有路径
    let mut ok = src_from_mbr("mdosok", &[(0x83, 2048, 2048)], 32768);
    assert!(resize_mdos_entry(&mut ok, 1, 4096).is_ok(), "a real entry must still be resizable");
}

/// 越盘条目：raw 侧保留（info 可观察、可诊断），校验侧 parse_mbr 必须拒绝——
/// 写入侧有 end >= total 检查，所以坏表只能像真实世界那样来自别处：手写字节绕过
#[test]
pub(crate) fn msdos_entry_past_end_is_damaged_and_refused() {
    let mut src = src_from("past_end", vec![0u8; 300 * 512]);
    create_mbr(&mut src).unwrap();
    let mut lba0 = [0u8; 512];
    src.read_at(0, &mut lba0).unwrap();
    let rec = &mut lba0[446..446 + 16];
    rec[4] = 0x0C;
    rec[8..12].copy_from_slice(&1u32.to_le_bytes());
    rec[12..16].copy_from_slice(&10_000u32.to_le_bytes()); // 1 + 10_000 > 300 扇区
    src.write_at(0, &lba0).unwrap();

    let raw = parse_mbr_raw(&src).unwrap().expect("the entry must stay observable for diagnostics");
    assert!(matches!(raw.damage.first(), Some(MbrDamage::PastEnd { .. })), "{:?}", raw.damage);
    let e = parse_mbr(&src).expect_err("a past-end entry must refuse the write path");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
}

/// 起始 LBA 为 0 的条目：raw 侧保留可观察，校验侧 parse_mbr 必须拒绝。
/// 放行会让 partition_bytes 给出偏移 0 的区间，mkfs/check/set 从盘首擦写、覆盖 LBA0 的表
#[test]
pub(crate) fn msdos_entry_start_zero_is_damaged_and_refused() {
    let mut src = src_from("start_zero", vec![0u8; 300 * 512]);
    create_mbr(&mut src).unwrap();
    let mut lba0 = [0u8; 512];
    src.read_at(0, &mut lba0).unwrap();
    let rec = &mut lba0[446..446 + 16];
    rec[4] = 0x83;
    rec[8..12].copy_from_slice(&0u32.to_le_bytes()); // 起始 LBA 0
    rec[12..16].copy_from_slice(&100u32.to_le_bytes());
    src.write_at(0, &lba0).unwrap();

    let raw = parse_mbr_raw(&src).unwrap().expect("the entry must stay observable for diagnostics");
    assert!(matches!(raw.damage.first(), Some(MbrDamage::StartZero { .. })), "{:?}", raw.damage);
    let e = parse_mbr(&src).expect_err("a start-at-LBA-0 entry must refuse the write path");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
}

/// MBR 的 StartLBA/SizeInLBA 是 u32 字段：超出表示范围必须拒绝。
/// 少了这条检查，`start as u32` 会静默截断成一个指向别处的条目（落盘即损坏且不报错）
#[test]
pub(crate) fn add_mdos_entry_rejects_beyond_32bit_lba() {
    // 8KiB 让 4Kn 候选的 LBA1 探测（偏移 4096）有字节可读；下面把 size 改成 3TiB 只是
    // 为了让 u32 上限内的请求落在容器内，文件本身不必真有那么大
    let mut data = vec![0u8; 8192];
    data[510] = 0x55;
    data[511] = 0xAA;
    let mut src = src_from("mdos32", data);
    // 3 TiB 容器：扇区数超过 u32，但 LBA0 只需前 512 字节（真实文件无需那么大）
    src.size = 3 << 40;
    let over = u32::MAX as u64 + 1;
    let e = add_mdos_entry(&mut src, over, over + 2047, 0x83).unwrap_err();
    assert!(matches!(e, Fail::Refused(_)), "must be refused, not truncated: {e:?}");
    // 上限内的值仍应照常写入（新检查不得把合法调用一并拒掉）
    assert_eq!(add_mdos_entry(&mut src, 2048, 4095, 0x83).unwrap(), 1);
}

#[test]
pub(crate) fn msdos_hidden_toggle() {
    let data = vec![0u8; 300 * 512];
    let mut src = src_from("mhidden", data);
    create_mbr(&mut src).unwrap();
    add_mdos_entry(&mut src, 63, 200, 0x0B).unwrap();
    set_mdos_hidden(&mut src, 1, true).unwrap();
    assert_eq!(parse_mbr(&src).unwrap().unwrap()[0].os_type, 0x1B);
    set_mdos_hidden(&mut src, 1, false).unwrap();
    assert_eq!(parse_mbr(&src).unwrap().unwrap()[0].os_type, 0x0B);
    // 无 hidden 对应码的类型拒绝
    add_mdos_entry(&mut src, 210, 250, 0x83).unwrap();
    assert!(set_mdos_hidden(&mut src, 2, true).is_err());
}

#[test]
pub(crate) fn msdos_type_set_and_guards() {
    let data = vec![0u8; 300 * 512];
    let mut src = src_from("mtype", data);
    create_mbr(&mut src).unwrap();
    add_mdos_entry(&mut src, 63, 200, 0x83).unwrap();
    // 0x 前缀大小写在命令层；此处只验落盘与领域守卫
    set_mdos_type(&mut src, 1, 0x07).unwrap();
    assert_eq!(parse_mbr(&src).unwrap().unwrap()[0].os_type, 0x07);
    // 0x00 / 扩展容器类型双向拒绝，且不落盘
    for bad in [0x00u8, 0x05, 0x0F, 0x85] {
        let e = set_mdos_type(&mut src, 1, bad).unwrap_err();
        assert!(matches!(e, Fail::Refused(_)), "0x{bad:02X} must be refused: {e:?}");
    }
    assert_eq!(parse_mbr(&src).unwrap().unwrap()[0].os_type, 0x07);
    // 空槽拒绝
    assert!(set_mdos_type(&mut src, 2, 0x07).is_err());
    // 既有条目本身是容器类型：它的类型是结构性的，不接受改写。容器形状直接改字节造出
    // —— add_mdos_entry 那道闸不允许写入容器类型
    let mut lba0 = vec![0u8; 512];
    src.read_at(0, &mut lba0).unwrap();
    lba0[446 + 4] = 0x05;
    src.write_at(0, &lba0).unwrap();
    let e = set_mdos_type(&mut src, 1, 0x07).unwrap_err();
    assert!(matches!(e, Fail::Refused(_)), "existing extended container must be refused: {e:?}");
    assert_eq!(parse_mbr(&src).unwrap().unwrap()[0].os_type, 0x05);
    // 既有条目是 GPT 保护记录：类型字节承担结构语义，与 del_mdos_entry 同判据
    lba0[446 + 4] = 0xEE;
    src.write_at(0, &lba0).unwrap();
    let e = set_mdos_type(&mut src, 1, 0x07).unwrap_err();
    assert!(matches!(e, Fail::Refused(_)), "existing protective record must be refused: {e:?}");
    let mut raw = vec![0u8; 512];
    src.read_at(0, &mut raw).unwrap();
    assert_eq!(raw[446 + 4], 0xEE, "a refused write must leave the type byte unchanged");
}

/// resize_mdos_entry 是 pub 写入口：start + 新长度越出盘尾必须拒绝——
/// 与 add_mdos_entry 的自守同严格，不能指望每个调用方都先查过 free
#[test]
pub(crate) fn resize_mdos_entry_rejects_beyond_disk_end() {
    let data = vec![0u8; 300 * 512];
    let mut src = src_from("mrszend", data);
    create_mbr(&mut src).unwrap();
    add_mdos_entry(&mut src, 63, 200, 0x83).unwrap();
    // 63 + 300 > 300 盘尾：拒绝且不落盘
    let e = resize_mdos_entry(&mut src, 1, 300).unwrap_err();
    assert!(matches!(e, Fail::Refused(_)), "must be refused: {e:?}");
    // 仍在盘内的扩容照常写入（新检查不得把合法调用一并拒掉）
    resize_mdos_entry(&mut src, 1, 200).unwrap();
    assert_eq!(parse_mbr(&src).unwrap().unwrap()[0].size_lba, 200);
}

/// resize_mdos_entry 的邻接自守：盘上已存在邻区时，扩到邻区之上必须在写 LBA0 前拒绝
#[test]
pub(crate) fn resize_mdos_entry_rejects_neighbor_overlap() {
    let data = vec![0u8; 300 * 512];
    let mut src = src_from("mrszovl", data);
    create_mbr(&mut src).unwrap();
    add_mdos_entry(&mut src, 63, 162, 0x83).unwrap();
    add_mdos_entry(&mut src, 200, 249, 0x83).unwrap();
    // [63, 162] 扩到 [63, 199] 与分区 2 [200, 249] 相接：合法（区间贴邻不重叠）
    resize_mdos_entry(&mut src, 1, 137).unwrap();
    // 盖过分区 2 起点：拒绝且不落盘
    let e = resize_mdos_entry(&mut src, 1, 138).unwrap_err();
    assert!(matches!(e, Fail::Refused(_)), "must be refused: {e:?}");
    assert_eq!(parse_mbr(&src).unwrap().unwrap()[0].size_lba, 137);
}

/// parse_mbr_raw 的重叠损伤：重叠条目保留在 raw 视图（info 可观察），
/// 写路径经 parse_mbr 整体拒绝
#[test]
pub(crate) fn parse_mbr_reports_overlap_damage() {
    let data = vec![0u8; 300 * 512];
    let mut src = src_from("mbrovl", data);
    create_mbr(&mut src).unwrap();
    add_mdos_entry(&mut src, 63, 162, 0x83).unwrap();
    add_mdos_entry(&mut src, 200, 249, 0x83).unwrap();
    // 手工把分区 2 起点改进分区 1 的区间（add_mdos_entry 自己不会写出这种表）
    let mut lba0 = vec![0u8; 512];
    src.read_at(0, &mut lba0).unwrap();
    lba0[446 + 16 + 8..446 + 16 + 12].copy_from_slice(&100u32.to_le_bytes());
    src.write_at(0, &lba0).unwrap();
    let raw = parse_mbr_raw(&src).unwrap().unwrap();
    assert!(raw.damage.iter().any(|d| matches!(d, MbrDamage::Overlap { .. })), "{:?}", raw.damage);
    assert_eq!(raw.parts.len(), 2, "重叠条目必须保留在 raw 视图里");
    // 写路径拒绝
    assert!(parse_mbr(&src).is_err());
}

/// 保护 MBR（UEFI 2.10 §5.2.3）：形状与 SizeInLBA 覆盖范围分层判定
#[test]
pub(crate) fn protective_mbr_shape_and_size() {
    let mut src = src_from_gpt("pmbr", 512); // 100 扇区 → SizeInLBA = 99
    assert!(pmbr_shape_valid(&src).unwrap());
    assert_eq!(pmbr_size_state(&src).unwrap(), PmbrSize::Normal);
    let good = {
        let mut b = [0u8; 512];
        src.read_at(0, &mut b).unwrap();
        b
    };
    // 每条用例：在有效布局上只改一处字节
    let patch = |src: &mut FileSource, f: &dyn Fn(&mut [u8; 512])| {
        let mut b = good;
        f(&mut b);
        src.write_at(0, &b).unwrap();
    };
    // 形状：签名丢失 / 类型非 0xEE / StartingLBA != 1 / 其余三条记录非零
    patch(&mut src, &|b| b[510] = 0);
    assert!(!pmbr_shape_valid(&src).unwrap());
    patch(&mut src, &|b| b[446 + 4] = 0x83);
    assert!(!pmbr_shape_valid(&src).unwrap());
    patch(&mut src, &|b| b[446 + 8..446 + 12].copy_from_slice(&2u32.to_le_bytes()));
    assert!(!pmbr_shape_valid(&src).unwrap());
    patch(&mut src, &|b| b[446 + 16 + 4] = 0x0B);
    assert!(!pmbr_shape_valid(&src).unwrap());

    // 尺寸：形状成立与否不受 SizeInLBA 影响，尺寸单独分类
    patch(&mut src, &|_| {});
    assert!(pmbr_shape_valid(&src).unwrap());
    // 小于容器（99 → 50）= 扩容后的 stale，形状仍有效
    patch(&mut src, &|b| b[446 + 12..446 + 16].copy_from_slice(&50u32.to_le_bytes()));
    assert!(pmbr_shape_valid(&src).unwrap());
    assert_eq!(
        pmbr_size_state(&src).unwrap(),
        PmbrSize::NeedsRepair { cause: PmbrIssue::Stale }
    );
    // 等于容器 = Normal；大于容器且非已知口径 = 不一致（拒绝自动修复）；0 亦视为 stale
    patch(&mut src, &|b| b[446 + 12..446 + 16].copy_from_slice(&99u32.to_le_bytes()));
    assert_eq!(pmbr_size_state(&src).unwrap(), PmbrSize::Normal);
    patch(&mut src, &|b| b[446 + 12..446 + 16].copy_from_slice(&200u32.to_le_bytes()));
    assert_eq!(pmbr_size_state(&src).unwrap(), PmbrSize::Inconsistent);
    patch(&mut src, &|b| b[446 + 12..446 + 16].copy_from_slice(&0u32.to_le_bytes()));
    assert_eq!(
        pmbr_size_state(&src).unwrap(),
        PmbrSize::NeedsRepair { cause: PmbrIssue::Stale }
    );

    // 大盘（4 TiB，32 位表示不下）：按规范饱和写 0xFFFFFFFF → Normal
    let mut big = src_from("pmbr_big", vec![0u8; 8192]);
    big.size = 4 * 1024 * 1024 * 1024 * 1024u64;
    {
        let mut b = [0u8; 512];
        big.read_at(0, &mut b).unwrap();
        b[446 + 4] = 0xEE;
        b[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
        b[446 + 12..446 + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        b[510] = 0x55;
        b[511] = 0xAA;
        big.write_at(0, &b).unwrap();
    }
    assert!(pmbr_shape_valid(&big).unwrap());
    assert_eq!(pmbr_size_state(&big).unwrap(), PmbrSize::Normal);

    // 4Kn 容器上的 512 字节口径值：UEFI 2.10 §5.2.3 的 SizeInLBA 以 logical block 计，
    // 512 口径是别的工具的历史写法 → 判为"非规范但可修复"，不得并进 Normal
    // （并入的后果：info 声称它合法，且 classify_repair 不排修复动作，兼容值永久停留）
    let mut k4 = src_from_gpt("pmbr_4kn", 4096); // 100 块 × 4096 = 409600 字节
    k4.sector_size = 4096; // 规范值 = 100 − 1 = 99；512 口径值 = 409600/512 − 1 = 799
    let set_size = |src: &mut FileSource, v: u32| {
        let mut b = vec![0u8; src.sector_size as usize];
        src.read_at(0, &mut b).unwrap();
        b[446 + 4] = 0xEE;
        b[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
        b[446 + 12..446 + 16].copy_from_slice(&v.to_le_bytes());
        b[510] = 0x55;
        b[511] = 0xAA;
        src.write_at(0, &b).unwrap();
    };
    set_size(&mut k4, 799);
    assert!(pmbr_shape_valid(&k4).unwrap());
    assert_eq!(
        pmbr_size_state(&k4).unwrap(),
        PmbrSize::NeedsRepair { cause: PmbrIssue::Compat512 }
    );
    // 策略层必须把它排进修复动作，否则写入路径不会规范化
    let last = k4.size / 4096 - 1;
    let g4 = load_gpt(&k4).unwrap().unwrap();
    assert_eq!(
        crate::gpt_policy::classify_repair(&g4, last).unwrap(),
        crate::gpt_policy::RepairAction::RepairProtectiveMbr
    );
    // 规范化点：写入路径重写一次即收敛
    ensure_protective_mbr(&mut k4).unwrap();
    assert_eq!(pmbr_size_state(&k4).unwrap(), PmbrSize::Normal);
    // 大于规范值且非已知口径（如 800）→ 可能是更大盘的截断副本，仍拒绝自动修复
    set_size(&mut k4, 800);
    assert_eq!(pmbr_size_state(&k4).unwrap(), PmbrSize::Inconsistent);
    let g4 = load_gpt(&k4).unwrap().unwrap();
    assert!(crate::gpt_policy::classify_repair(&g4, last).is_err());
}

/// 盘型由 LBA1 签名判定（UEFI 2.10 §5.3），保护 MBR 布局只定修复分类：
/// 布局破损的 GPT 盘绝不能按 msdos 解析。0xEE 槽若被当成真分区输出，
/// del/flag 会清除或改写它，恢复 GPT 所需的保护记录就此毁掉
#[test]
pub(crate) fn damaged_protective_mbr_never_parses_as_msdos() {
    let mut src = src_from_gpt("pmbdmg", 512);
    let mut good = [0u8; 512];
    src.read_at(0, &mut good).unwrap();

    // 破法一：槽位 1 仍是 0xEE 但 StartingLBA 被改坏——签名在 ⇒ 盘型 GPT，不判 msdos
    let mut b = good;
    b[446 + 8..446 + 12].copy_from_slice(&2u32.to_le_bytes());
    src.write_at(0, &b).unwrap();
    assert!(!pmbr_shape_valid(&src).unwrap());
    assert!(parse_mbr(&src).unwrap().is_none(), "0xEE 槽不是分区，签名在 ⇒ 不判 msdos");
    assert_eq!(table_label(&src).unwrap(), TableLabel::GptDamaged);

    // 破法二：槽位 1 类型被改写成真实分区类型——LBA1 仍有 CRC 自洽的主头，
    // 盘型仍是 GPT；按 msdos 解析会让写入只改 MBR 视图、GPT 视图不动
    let mut b = good;
    b[446 + 4] = 0x0B;
    src.write_at(0, &b).unwrap();
    assert_eq!(table_label(&src).unwrap(), TableLabel::GptDamaged);
    assert!(load_gpt(&src).unwrap().is_none(), "保护布局不成立 ⇒ load_gpt 不给表");

    // 破法三：签名抹掉（GPT 全灭）+ 槽位 1 起点被改坏——无签名无从判 GPT，
    // 0xEE 槽已排除，等价于零分区的 msdos 盘
    let mut b = good;
    b[446 + 8..446 + 12].copy_from_slice(&2u32.to_le_bytes());
    src.write_at(0, &b).unwrap();
    src.write_at(512, &vec![0u8; 512]).unwrap();
    assert!(!pmbr_shape_valid(&src).unwrap());
    assert!(parse_mbr(&src).unwrap().unwrap().is_empty());
    assert_eq!(table_label(&src).unwrap(), TableLabel::Mbr);

    // hybrid 布局：0xEE + 真实槽位 → 只输出真实槽位（内核同口径按 msdos 管）
    let mut h = src_from("hybrid", vec![0u8; 300 * 512]);
    create_mbr(&mut h).unwrap();
    add_mdos_entry(&mut h, 63, 200, 0x0B).unwrap();
    add_mdos_entry(&mut h, 210, 250, 0x83).unwrap(); // 第二分区进槽位 2
    let mut lba0 = [0u8; 512];
    h.read_at(0, &mut lba0).unwrap();
    lba0[446 + 4] = 0xEE;
    h.write_at(0, &lba0).unwrap();
    // msdos 盘上 LBA1 的残留 GPT 头：槽位是真实分区 → 照常按 msdos 管
    let mut sig = vec![0u8; 512];
    sig[..8].copy_from_slice(GPT_SIGNATURE);
    h.write_at(512, &sig).unwrap();
    let m = parse_mbr(&h).unwrap().unwrap();
    assert_eq!(m.len(), 1, "0xEE 槽不输出");
    assert_eq!(m[0].num, 2);
    assert_eq!(table_label(&h).unwrap(), TableLabel::Mbr);
}

/// msdos hidden 的幂等：已是目标态时重复同一意图必须成功且不改字节。第二次起被
/// 当"无对应类型"拒绝是这类"读当前值再决定"的改写的典型回归
#[test]
pub(crate) fn mdos_hidden_is_idempotent() {
    let mut src = src_from("hidden_idem", vec![0u8; 300 * 512]);
    create_mbr(&mut src).unwrap();
    add_mdos_entry(&mut src, 63, 200, 0x0C).unwrap(); // FAT32 LBA（可见侧）
    let ty = |src: &FileSource| {
        let mut lba0 = [0u8; 512];
        src.read_at(0, &mut lba0).unwrap();
        lba0[446 + 4]
    };

    set_mdos_hidden(&mut src, 1, true).unwrap();
    assert_eq!(ty(&src), 0x1C);
    set_mdos_hidden(&mut src, 1, true).unwrap();
    assert_eq!(ty(&src), 0x1C, "a repeated hide must be a no-op, not a refusal");

    set_mdos_hidden(&mut src, 1, false).unwrap();
    assert_eq!(ty(&src), 0x0C);
    set_mdos_hidden(&mut src, 1, false).unwrap();
    assert_eq!(ty(&src), 0x0C, "a repeated unhide must be a no-op, not a refusal");
}

/// 非配对类型两个方向都拒绝：幂等短路只认配对表内的值，不得把"未知类型"当成
/// "已在目标态"
#[test]
pub(crate) fn mdos_hidden_rejects_unpaired_type() {
    let mut src = src_from("hidden_unpaired", vec![0u8; 300 * 512]);
    create_mbr(&mut src).unwrap();
    add_mdos_entry(&mut src, 63, 200, 0x83).unwrap(); // Linux data：无 hidden 对应码
    let e = set_mdos_hidden(&mut src, 1, true).expect_err("0x83 must be refused");
    assert!(matches!(&e, Fail::Refused(m) if m.contains("no hidden counterpart")), "{e:?}");
    assert!(set_mdos_hidden(&mut src, 1, false).is_err(), "0x83 must be refused in both directions");
}