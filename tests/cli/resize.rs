use super::*;

/// 每扇区内容由扇区号与字节位共同决定：只搬首扇区、跨 chunk 错位或漏掉残段都会与期望不等
fn marker_range(first_lba: u64, sectors: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity((sectors * 512) as usize);
    for lba in first_lba..first_lba + sectors {
        let mut s = [0u8; 512];
        for (j, b) in s.iter_mut().enumerate() {
            *b = (lba as u8).wrapping_mul(37).wrapping_add(j as u8);
        }
        v.extend_from_slice(&s);
    }
    v
}

fn read_region(img: &std::path::Path, lba: u64, len: usize) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(img).unwrap();
    f.seek(SeekFrom::Start(lba * 512)).unwrap();
    let mut v = vec![0u8; len];
    f.read_exact(&mut v).unwrap();
    v
}

fn write_region(img: &std::path::Path, lba: u64, bytes: &[u8]) {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(img).unwrap();
    f.seek(SeekFrom::Start(lba * 512)).unwrap();
    f.write_all(bytes).unwrap();
}

/// 精确落位的 GPT 夹具：`add` 只按第一个可容纳空洞落位，这些用例需要指定坐标。
fn write_layout(img: &std::path::Path, parts: &[(u64, u64)]) {
    let mut cur = Cursor::new(vec![0u8; 16 * 1024 * 1024]);
    let mut gpt = gptman::GPT::new_from(&mut cur, 512, [0xCD; 16]).unwrap();
    for (i, (start, end)) in parts.iter().enumerate() {
        gpt[i as u32 + 1] = gptman::GPTPartitionEntry {
            partition_type_guid: [0x11; 16],
            unique_partition_guid: [0x12 + i as u8; 16],
            starting_lba: *start,
            ending_lba: *end,
            attribute_bits: 0,
            partition_name: "".into(),
        };
    }
    gpt.write_into(&mut cur).unwrap();
    std::fs::write(img, with_protective_mbr(cur.into_inner())).unwrap();
}

#[test]
fn resize_part_move_copy_flag_name() {
    let dir = std::env::temp_dir().join(format!("diskedit_rp_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("rp.img");
    // 16 MiB 镜像（32768 扇区），坐标全部取 1MiB 对齐值
    let data = vec![0u8; 16 * 1024 * 1024];
    std::fs::write(&img, &data).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    write_layout(&img, &[(2048, 6143), (14336, 18431)]);

    // 数据指纹：写入可辨识字节到分区 a 首尾
    let mut f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
    use std::io::{Read, Seek, SeekFrom, Write};
    f.seek(SeekFrom::Start(2048 * 512)).unwrap();
    f.write_all(&[0xAA; 512]).unwrap();
    f.seek(SeekFrom::Start(6143 * 512)).unwrap();
    f.write_all(&[0xBB; 512]).unwrap();
    drop(f);
    // b 的整区载荷：4096 个扇区各不相同，覆盖两次 chunk
    let b_payload = marker_range(14336, 4096);
    write_region(&img, 14336, &b_payload);

    // 向已占用区移动必须被拒绝（本工具策略：不做隐式避让，须先挪开挡路的分区）
    // 12000..17407 对齐后 12288..16383，与 b（14336..18431）重叠
    let img_before = std::fs::read(&img).unwrap();
    let entries_before = dir_entries(&dir);
    let (c, _, stderr) = run(&["move", &format!("{img_s}:1"), "--start", "12000", "--align", "mib", "--chunk", "4", "--yes"]);
    assert_eq!(c, 3, "overlapping move must refuse");
    assert!(stderr.contains("overlap"), "{stderr}");
    // 拒绝承诺未写盘：整镜像逐字节不变（表项未变不等于盘面未变）
    assert_eq!(std::fs::read(&img).unwrap(), img_before, "a refusal must leave the image byte-identical");
    assert_eq!(dir_entries(&dir), entries_before, "a refusal must not add sidecars");

    // move：分区 a 右移 8100..12400 → 对齐 8192..12287（落点空闲；长度不变，故没有 FS 步骤）
    let (c, _, stderr) = run(&["move", &format!("{img_s}:1"), "--start", "8100", "--align", "mib", "--chunk", "4", "--yes"]);
    assert_eq!(c, 0, "move must succeed: {stderr}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 8192);
    assert_eq!(gpt[1].ending_lba, 12287);
    // 指纹随数据一起搬移：新首尾应含 0xAA/0xBB
    let mut buf = [0u8; 512];
    f.seek(SeekFrom::Start(8192 * 512)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xAA), "head fingerprint must move with partition");
    f.seek(SeekFrom::Start(12287 * 512)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xBB), "tail fingerprint must move with partition");
    drop(f);

    // shrink：unknown FS 的分区拒绝缩容（无法先缩 FS，缩表项会写坏数据——fail-fast 不落盘）。
    // --yes 让请求越过确认门，命中这里要测的 FS 缩容守卫；不带它先被门拒成 3
    let img_before = std::fs::read(&img).unwrap();
    let entries_before = dir_entries(&dir);
    let (c, _, stderr) = run(&["resize-part", &format!("{img_s}:1"), "--end", "11000", "--yes"]);
    assert_eq!(c, 6, "shrink of unknown-FS partition must refuse: {stderr}");
    assert!(stderr.contains("shrink"), "{stderr}");
    // 拒绝发生在任何写入前：表项保持 move 后的 8192..12287，盘面亦逐字节不变
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 8192);
    assert_eq!(gpt[1].ending_lba, 12287);
    drop(f);
    assert_eq!(std::fs::read(&img).unwrap(), img_before, "a refusal must leave the image byte-identical");
    assert_eq!(dir_entries(&dir), entries_before, "a refusal must not add sidecars");

    // copy：分区 b 复制到 20000 → 对齐 20480（b 长 4096 扇区 → 20480..24575）
    // --chunk 1（1MiB = 2048 扇区）：源 2 MiB 跨两个 chunk，边界与残段都走一遍
    let (c, out, stderr) = run(&["copy", &format!("{img_s}:2"), "--start", "20000", "--align", "mib", "--name", "bcopy", "--chunk", "1", "--yes"]);
    assert_eq!(c, 0, "copy must succeed: {out}{stderr}");
    // 目标整区逐字节等于源；源必须保持原样
    assert_eq!(read_region(&img, 20480, b_payload.len()), b_payload, "copy target must equal the source byte-for-byte");
    assert_eq!(read_region(&img, 14336, b_payload.len()), b_payload, "copy must leave the source untouched");
    // 非法 chunk 拒绝（0 越界）
    let img_before = std::fs::read(&img).unwrap();
    let entries_before = dir_entries(&dir);
    let (c, _, _) = run(&["copy", &format!("{img_s}:2"), "--start", "24000", "--chunk", "0", "--yes"]);
    assert_eq!(c, 2, "invalid --chunk must refuse as a usage error");
    assert_eq!(std::fs::read(&img).unwrap(), img_before, "a refusal must leave the image byte-identical");
    assert_eq!(dir_entries(&dir), entries_before, "a refusal must not add sidecars");

    let (c, _, _) = run(&["set", &format!("{img_s}:2"), "flag", "esp", "on", "--yes"]);
    assert_eq!(c, 0);
    let (c, _, _) = run(&["set", &format!("{img_s}:2"), "name", "renamed", "--yes"]);
    assert_eq!(c, 0);

    // 终验：info + gptman
    let (c, out, _) = run(&["info", img_s]);
    assert_eq!(c, 0);
    assert!(out.contains("\"name\":\"renamed\""), "{out}");
    assert!(out.contains("\"name\":\"bcopy\""), "{out}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 8192);
    assert_eq!(gpt[1].ending_lba, 12287);
    // esp 标志 = 类型 GUID 换成 EFI System Partition GUID（属性位 bit60 是 Microsoft
    // read-only，与 ESP 无关）；磁盘字节序 = 前 3 字段小端
    assert_eq!(
        gpt[2].partition_type_guid,
        [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B],
        "esp flag must switch type GUID to EFI System Partition"
    );
    assert_eq!(gpt[3].starting_lba, 20480);
    assert_eq!(gpt[3].ending_lba, 24575);
    // PARTUUID：默认 copy 生成新的 v4 GUID，不与源重复
    let src_guid = gpt[2].unique_partition_guid;
    let copy_guid = gpt[3].unique_partition_guid;
    assert_ne!(copy_guid, src_guid, "copy must not reuse the source PARTUUID by default");
    assert_eq!(copy_guid[7] & 0xF0, 0x40, "new PARTUUID must carry version 4");
    assert_eq!(copy_guid[8] & 0xC0, 0x80, "new PARTUUID must use the RFC 4122 variant");
    drop(gpt);
    drop(f);

    // --keep-partuuid 显式保留源 PARTUUID（复制身份，而不只是复制内容）
    let (c, out, stderr) = run(&["copy", &format!("{img_s}:2"), "--start", "24000", "--align", "mib", "--keep-partuuid", "--chunk", "4", "--yes"]);
    assert_eq!(c, 0, "--keep-partuuid copy must succeed: {out}{stderr}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[4].starting_lba, 24576);
    assert_eq!(gpt[4].ending_lba, 28671);
    assert_eq!(gpt[4].unique_partition_guid, src_guid, "--keep-partuuid must keep the source PARTUUID");
    drop(gpt);
    drop(f);
    assert_eq!(read_region(&img, 24576, b_payload.len()), b_payload, "the kept-GUID copy must still copy every byte");
    assert_eq!(read_region(&img, 14336, b_payload.len()), b_payload, "the source must remain untouched after the second copy");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn grow_to_end() {
    let dir = std::env::temp_dir().join(format!("diskedit_gte_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("g.img");
    // 8 MiB（16384 扇区）：last_usable = 16350
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    let (c, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, e) = run(&["add", img_s, "1M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--name", "a", "--yes"]);
    assert_eq!(c, 0, "{e}");

    // --end 与 --grow-to-end 互斥
    let (c, _) = run(&["resize-part", &format!("{img_s}:1"), "--end", "6000", "--grow-to-end", "--yes"]);
    assert_eq!(c, 2, "--end + --grow-to-end must refuse as a usage error");

    // grow-to-end：分区里没有 FS，缺少 FS 步骤时在写表之前就拒绝；出路必须点名
    // 真正能只改分区表的命令，而不是让用户给 resize-part 加一个它不接受的开关
    let (c, e) = run(&["resize-part", &format!("{img_s}:1"), "--grow-to-end", "--yes"]);
    assert_eq!(c, 6, "an unidentifiable region must be refused: {e}");
    assert!(e.contains("resize --no-fs"), "the hint must name the command that can change the partition alone: {e}");
    assert!(!e.contains("pass --no-fs"), "the hint must not tell the user to pass --no-fs to this command: {e}");

    // resize-part 不登记 --no-fs：收到即用法错误，不进入任何写入
    let (c, e) = run(&["resize-part", &format!("{img_s}:1"), "--grow-to-end", "--no-fs", "--yes"]); // cli-surface: refused
    assert_eq!(c, 2, "--no-fs is outside the resize-part surface: {e}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// swap 分区内预置的 v1 头身份（内核 include/linux/swap.h union swap_header）：
/// sws_uuid @1036、sws_volume @1052。全零分区只会走"无 swap 头 → 随机 UUID"那条路，
/// mkswap 的 -U/-L 参数构造就永远进不了自动化
const SWAP_UUID: [u8; 16] = [0xA1; 16];
const SWAP_LABEL: [u8; 3] = [b'A', 0xFF, b'B']; // 含非法 UTF-8 字节：按字节往返的判据

/// 16 MiB（32768 扇区，last_usable 32734）：root 2048..6143 + swap 6144..8191。
/// resize grow 后 swap 尾部打包 30687..32734（零数据搬移），root 扩到 32734
fn write_swap_fixture(img: &std::path::Path) {
    let ss = 512u64;
    let data = vec![0u8; 16 * 1024 * 1024];
    let mut cur = Cursor::new(data);
    let mut gpt = gptman::GPT::new_from(&mut cur, ss, [0xCD; 16]).unwrap();
    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: [0x11; 16],
        unique_partition_guid: [0x12; 16],
        starting_lba: 2048,
        ending_lba: 6143,
        attribute_bits: 0,
        partition_name: "root".into(),
    };
    gpt[2] = gptman::GPTPartitionEntry {
        partition_type_guid: [
            0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F,
        ],
        unique_partition_guid: [0x42; 16],
        starting_lba: 6144,
        ending_lba: 8191,
        attribute_bits: 0,
        partition_name: "swap".into(),
    };
    gpt.write_into(&mut cur).unwrap();
    {
        let ps = 4096usize; // 探测候选里本机页优先，4096 先命中
        let base = 6144usize * 512; // swap 分区起始（LBA 6144）
        let raw = cur.get_mut();
        raw[base + ps - 10..base + ps].copy_from_slice(b"SWAPSPACE2");
        // 1 MiB 分区按 4096 页 = 256 页；mkswap 写 version = 1、last_page = 页数 − 1
        raw[base + 1024..base + 1028].copy_from_slice(&1u32.to_le_bytes());
        raw[base + 1028..base + 1032].copy_from_slice(&255u32.to_le_bytes());
        raw[base + 1036..base + 1052].copy_from_slice(&SWAP_UUID);
        raw[base + 1052..base + 1055].copy_from_slice(&SWAP_LABEL);
    }
    std::fs::write(img, with_protective_mbr(cur.into_inner())).unwrap();
}

#[test]
fn resize_grow_swap_recreate() {
    let dir = std::env::temp_dir().join(format!("diskedit_swr_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("s.img");
    write_swap_fixture(&img);
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();

    // --no-fs：#1 是 grow 目标且里面没有 FS，只改表由用户显式担下（否则事前拒绝）；
    // #2 的 swap 重建是**搬移**的一环（swap 头随位置走），与本开关无关，重建照旧
    let out = Command::new(exe)
        .args(["resize", &format!("{img_s}:1"), "grow", "--no-fs", "--allow-move", "--chunk", "1", "--yes"])
        .output()
        .unwrap();
    let code = out.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // swap 重建依赖 mkswap/losetup（仅 Linux）：工具可用 → 后置条件全满足（OK）；
    // 不可用 → 分区表已更新但 swap 未重建，属部分完成（PARTIAL），且必须给出补救命令。
    #[cfg(target_os = "linux")]
    {
        let is_root = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).map(str::to_string))
            .and_then(|l| l.split_whitespace().nth(2).map(|f| f == "0"))
            .unwrap_or(false);
        if is_root {
            assert_eq!(code, 0, "resize grow must fully succeed as root: {stderr}");
            // 重建后的 swap 落在新位置（LBA 30687）：-U/-L 传下去的值必须原样出现在新 swap 头里
            use std::io::{Read, Seek, SeekFrom};
            let mut f = std::fs::File::open(&img).unwrap();
            let mut uuid = [0u8; 16];
            let mut label = [0u8; 16];
            f.seek(SeekFrom::Start(30687 * 512 + 1036)).unwrap();
            f.read_exact(&mut uuid).unwrap();
            f.seek(SeekFrom::Start(30687 * 512 + 1052)).unwrap();
            f.read_exact(&mut label).unwrap();
            assert_eq!(uuid, SWAP_UUID, "mkswap -U must restore the swap UUID");
            assert_eq!(&label[..SWAP_LABEL.len()], &SWAP_LABEL[..], "mkswap -L must carry the label byte-for-byte");
            assert!(label[SWAP_LABEL.len()..].iter().all(|&b| b == 0), "the rest of the label field must be NUL padding");
        } else {
            // CI runner 等非 root 环境：mkswap 无法执行 → 表已更新、swap 待重建
            assert_eq!(code, 5, "non-root: swap step pending → PARTIAL: {stderr}");
            assert!(stderr.contains("swap rebuild") && stderr.contains("mkswap"), "remedy must be printed: {stderr}");
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // 新操作（无 ckpt）：非 Linux 平台必须在写盘前停下，不能先写表再以 pending 收场。
        // 能力门先于工具探测，故这里断言缺失原语文案而非 mkswap；分类走 outcome 的唯一映射点
        // （环境不满足 = 6）
        assert_eq!(code, 6, "a new swap relocation on a non-Linux host must stop before writing: {stderr}");
        assert!(
            stderr.contains("needs the"),
            "the refusal must name the missing primitive: {stderr}"
        );
    }

    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 2048, "root start unchanged");
    assert_eq!(gpt[2].unique_partition_guid, [0x42; 16], "PARTUUID must be preserved");
    if code == 0 {
        // swap 重建成功 → 条目循环走完，扩容收尾照常：root 扩到 swap 新区域之前
        //（30687-1=30686），不得重叠
        assert_eq!(gpt[1].ending_lba, 30686, "root must end before relocated swap");
        assert_eq!(gpt[2].starting_lba, 30687, "swap packed at tail");
        assert_eq!(gpt[2].ending_lba, 32734);
    } else if code == 5 {
        // 已写盘、swap 未重建 → 条目循环未走完，扩容收尾跳过：root 保持原终点（收尾按全体 moves
        // 取 min(new_first)−1，未走完时会把未搬移条目的旧位划进扩容区）；swap 条目已落位
        assert_eq!(gpt[1].ending_lba, 6143, "growth must not be committed while the swap rebuild is pending");
        assert_eq!(gpt[2].starting_lba, 30687, "swap entry already relocated");
        assert_eq!(gpt[2].ending_lba, 32734);
    } else {
        // 写盘前停下（30=环境不满足）：表与条目都不得变化
        assert_eq!(gpt[1].ending_lba, 6143, "a refusal must not grow the target");
        assert_eq!(gpt[2].starting_lba, 6144, "a refusal must not relocate swap");
        assert_eq!(gpt[2].ending_lba, 8191);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// 新跑搬移 swap：非 Linux 在写盘前以能力缺口（6）停下且不留下恢复状态；
/// Linux 上重建工具可执行则 swap 重建成功（0）并提交扩容收尾，不可执行
/// （非 root、mkswap 无法操作用 loop 映射）则表已更新、swap 待重建（5）
#[test]
fn new_swap_relocation_outcome_follows_platform_and_privilege() {
    let dir = std::env::temp_dir().join(format!("diskedit_swgate_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("s.img");
    write_swap_fixture(&img);
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();

    let out = Command::new(exe)
        .args(["resize", &format!("{img_s}:1"), "grow", "--no-fs", "--allow-move", "--chunk", "1", "--yes"])
        .output()
        .unwrap();
    let code = out.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&out.stderr);

    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(code, 6, "a non-Linux host must stop before writing: {stderr}");
        assert!(
            stderr.contains("needs the"),
            "the refusal must name the missing primitive: {stderr}"
        );
        assert!(
            std::fs::read_dir(record_dir(&img)).map(|mut d| d.next().is_none()).unwrap_or(true),
            "a refusal must not leave recovery state behind"
        );
        let mut f = std::fs::File::open(&img).unwrap();
        let gpt = gptman::GPT::find_from(&mut f).unwrap();
        assert_eq!((gpt[1].starting_lba, gpt[1].ending_lba), (2048, 6143), "root unchanged");
        assert_eq!((gpt[2].starting_lba, gpt[2].ending_lba), (6144, 8191), "swap unchanged");
    }

    #[cfg(target_os = "linux")]
    {
        let is_root = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).map(str::to_string))
            .and_then(|l| l.split_whitespace().nth(2).map(|f| f == "0"))
            .unwrap_or(false);
        let mut f = std::fs::File::open(&img).unwrap();
        let gpt = gptman::GPT::find_from(&mut f).unwrap();
        assert_eq!((gpt[2].starting_lba, gpt[2].ending_lba), (30687, 32734), "swap entry relocated to the tail");
        if is_root {
            assert_eq!(code, 0, "root: the swap rebuild must succeed: {stderr}");
            assert_eq!(gpt[1].ending_lba, 30686, "root grows up to the relocated swap");
        } else {
            assert_eq!(code, 5, "non-root: the swap step is pending → PARTIAL: {stderr}");
            assert!(stderr.contains("swap rebuild") && stderr.contains("mkswap"), "remedy must be printed: {stderr}");
            assert_eq!(gpt[1].ending_lba, 6143, "growth must not be committed while the swap rebuild is pending");
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// 最小位移扩容（resize SIZE + --allow-move）：挡路分区按需让位，
/// 目标精确落在请求的新末端（不吞并目标与挡路者之间的间隙）
#[test]
fn resize_shift_relocates_blockers() {
    let dir = std::env::temp_dir().join(format!("diskedit_shift_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("sh.img");
    // 16 MiB（32768 扇区，last_usable 32734），坐标全部 1MiB 对齐
    std::fs::write(&img, vec![0u8; 16 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };

    write_layout(&img, &[(2048, 4095), (4096, 6143), (12288, 14335)]);

    // 数据指纹：b 首尾、c 首部各写可辨识字节
    {
        let mut f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
        use std::io::{Seek, SeekFrom, Write};
        f.seek(SeekFrom::Start(4096 * 512)).unwrap();
        f.write_all(&[0xAA; 512]).unwrap();
        f.seek(SeekFrom::Start(6143 * 512)).unwrap();
        f.write_all(&[0xBB; 512]).unwrap();
        f.seek(SeekFrom::Start(12288 * 512)).unwrap();
        f.write_all(&[0xCC; 512]).unwrap();
    }

    // 无 --allow-move：右侧被 b 挡 → 拒绝。--yes 只让确认门让路，被测的是搬移授权
    let img_before = std::fs::read(&img).unwrap();
    let entries_before = dir_entries(&dir);
    let (c, _, stderr) = run(&["resize", &format!("{img_s}:1"), "+3M", "--yes"]);
    assert_eq!(c, 3, "blocked resize without --allow-move must refuse");
    assert!(stderr.contains("--allow-move"), "{stderr}");
    assert_eq!(std::fs::read(&img).unwrap(), img_before, "a refusal must leave the image byte-identical");
    assert_eq!(dir_entries(&dir), entries_before, "a refusal must not add sidecars");

    // --allow-move 无 --yes：确认门在解析期拒绝，不打印计划、不动盘
    let img_before = std::fs::read(&img).unwrap();
    let entries_before = dir_entries(&dir);
    let (c, _, stderr) = run(&["resize", &format!("{img_s}:1"), "+3M", "--allow-move", "--chunk", "1"]);
    assert_eq!(c, 2, "missing --yes must be a parse-time refusal");
    assert!(stderr.contains("--yes"), "{stderr}");
    assert_eq!(std::fs::read(&img).unwrap(), img_before, "a refusal must leave the image byte-identical");
    assert_eq!(dir_entries(&dir), entries_before, "a refusal must not add sidecars");

    // +3M 相对当前 1M 大小 → 绝对 4M：b 让位到新末端之后，c 本就在扩容前线之外不动，
    // a 精确扩到 10239（2048+8192-1）。
    // a 里的指纹不是任何 FS 的签名（识别为 unknown），故扩分区表要显式 --no-fs
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "+3M", "--allow-move", "--chunk", "1", "--yes", "--no-fs"]);
    assert_eq!(c, 0, "shift resize must succeed: {e}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 2048);
    assert_eq!(gpt[1].ending_lba, 10239, "target must land exactly at the requested end");
    assert_eq!(gpt[2].starting_lba, 10240, "adjacent blocker shifts by the growth delta");
    assert_eq!(gpt[2].ending_lba, 12287);
    assert_eq!(gpt[3].starting_lba, 12288, "far blocker already beyond the growth front — not relocated");
    assert_eq!(gpt[3].ending_lba, 14335);
    // 指纹随数据搬移
    let mut buf = [0u8; 512];
    use std::io::{Read, Seek, SeekFrom};
    f.seek(SeekFrom::Start(10240 * 512)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xAA), "b head fingerprint must survive");
    f.seek(SeekFrom::Start(12287 * 512)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xBB), "b tail fingerprint must survive");
    f.seek(SeekFrom::Start(12288 * 512)).unwrap();
    f.read_exact(&mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0xCC), "c head fingerprint must survive");
    drop(f);

    // 让位后的空闲不足：d 挡在末端且无尾部空间 → 明确拒绝
    let img_before = std::fs::read(&img).unwrap();
    let entries_before = dir_entries(&dir);
    let (c, _, stderr) = run(&["resize", &format!("{img_s}:1"), "+64M", "--allow-move", "--chunk", "1", "--yes"]);
    assert_eq!(c, 3, "grow beyond usable range must refuse: {stderr}");
    assert_eq!(std::fs::read(&img).unwrap(), img_before, "a refusal must leave the image byte-identical");
    assert_eq!(dir_entries(&dir), entries_before, "a refusal must not add sidecars");

    let _ = std::fs::remove_dir_all(&dir);
}

/// resize SIZE 非扇区对齐时回显 note（实际生效的下取整值），且不改变落盘值
#[test]
fn resize_non_aligned_size_emits_note() {
    let dir = std::env::temp_dir().join(format!("diskedit_na_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("blank.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1),
         String::from_utf8_lossy(&out.stdout).into_owned(),
         String::from_utf8_lossy(&out.stderr).into_owned())
    };
    assert_eq!(run(&["new", img_s, "--yes"]).0, 0);
    assert_eq!(run(&["add", img_s, "1M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--name", "root", "--yes"]).0, 0);
    // 1 MiB + 1 字节非 512 整数倍：note 回显"实际生效 1 MiB"，请求本身合法（同尺寸
    // 重写）故退出 0——note 出现在越界判定之后，被拒绝的请求不会再带 note
    let (c, _, err) = run(&["resize", &format!("{img_s}:1"), "1048577", "--yes"]);
    assert!(err.contains("note: size 1048577 is not a multiple of the 512-byte sector"), "{err}");
    assert!(err.contains("resizing to 1048576 bytes"), "{err}");
    assert_eq!(c, 0, "the non-aligned but in-range request must succeed: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}