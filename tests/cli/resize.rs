use super::*;

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

    run(&["new", img_s, "--yes"]);
    let (c, _, _) = run(&["add", img_s, "--start", "2048", "--end", "6143", "--name", "a"]);
    assert_eq!(c, 0);
    let (c, _, _) = run(&["add", img_s, "--start", "14336", "--end", "16383", "--name", "b"]);
    assert_eq!(c, 0);

    // 数据指纹：写入可辨识字节到分区 a 首尾
    let mut f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
    use std::io::{Read, Seek, SeekFrom, Write};
    f.seek(SeekFrom::Start(2048 * 512)).unwrap();
    f.write_all(&[0xAA; 512]).unwrap();
    f.seek(SeekFrom::Start(6143 * 512)).unwrap();
    f.write_all(&[0xBB; 512]).unwrap();
    drop(f);

    // 向已占用区移动必须被拒绝（本工具策略：不做隐式避让，须先挪开挡路的分区）
    // 12000..17407 对齐后 12288..16383，与 b（14336..16383）重叠
    let (c, _, stderr) = run(&["resize-part", &format!("{img_s}:1"), "--start", "12000", "--end", "17407"]);
    assert_eq!(c, 10, "overlapping move must refuse");
    assert!(stderr.contains("overlaps"), "{stderr}");

    // move：分区 a 右移 8100..12400 → 对齐 8192..12287（落点空闲；长度不变，故没有 FS 步骤）
    let (c, _, stderr) = run(&["resize-part", &format!("{img_s}:1"), "--start", "8100", "--end", "12400"]);
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

    // shrink：unknown FS 的分区拒绝缩容（无法先缩 FS，缩表项会写坏数据——fail-fast 不落盘）
    let (c, _, stderr) = run(&["resize-part", &format!("{img_s}:1"), "--start", "8192", "--end", "11000"]);
    assert_eq!(c, 10, "shrink of unknown-FS partition must refuse: {stderr}");
    assert!(stderr.contains("shrink"), "{stderr}");
    // 拒绝发生在任何写入前：表项保持 move 后的 8192..12287
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 8192);
    assert_eq!(gpt[1].ending_lba, 12287);
    drop(f);

    // copy：分区 b 复制到 20000 → 对齐 20480（b 长 2048 扇区 → 20480..22527）
    // --chunk-size 1（1MiB = 2048 扇区 = 整块单 chunk）：参数化 chunk 全程走通
    let (c, out, stderr) = run(&["copy", &format!("{img_s}:2"), "--start", "20000", "--name", "bcopy", "--chunk-size", "1"]);
    assert_eq!(c, 0, "copy must succeed: {out}{stderr}");
    // 非法 chunk 拒绝（0 越界）
    let (c, _, _) = run(&["copy", &format!("{img_s}:2"), "--start", "24000", "--chunk-size", "0"]);
    assert_eq!(c, 10, "invalid --chunk-size must refuse");

    // flag/name
    let (c, _, _) = run(&["set", &format!("{img_s}:2"), "flag", "esp", "on"]);
    assert_eq!(c, 0);
    let (c, _, _) = run(&["set", &format!("{img_s}:2"), "name", "renamed"]);
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
    assert_eq!(gpt[3].ending_lba, 22527);
    // PARTUUID：默认 copy 生成新的 v4 GUID，不与源重复
    let src_guid = gpt[2].unique_partition_guid;
    let copy_guid = gpt[3].unique_partition_guid;
    assert_ne!(copy_guid, src_guid, "copy must not reuse the source PARTUUID by default");
    assert_eq!(copy_guid[7] & 0xF0, 0x40, "new PARTUUID must carry version 4");
    assert_eq!(copy_guid[8] & 0xC0, 0x80, "new PARTUUID must use the RFC 4122 variant");
    drop(gpt);
    drop(f);

    // --clone 显式保留源 PARTUUID（复制身份，而不只是复制内容）
    let (c, out, stderr) = run(&["copy", &format!("{img_s}:2"), "--start", "24000", "--clone"]);
    assert_eq!(c, 0, "--clone copy must succeed: {out}{stderr}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[4].starting_lba, 24576);
    assert_eq!(gpt[4].unique_partition_guid, src_guid, "--clone must keep the source PARTUUID");

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
    let (c, e) = run(&["add", img_s, "--start", "2048", "--end", "4095", "--name", "a"]);
    assert_eq!(c, 0, "{e}");

    // --end 与 --grow-to-end 互斥
    let (c, _) = run(&["resize-part", &format!("{img_s}:1"), "--start", "2048", "--end", "6000", "--grow-to-end"]);
    assert_eq!(c, 10, "--end + --grow-to-end must refuse");

    // grow-to-end：分区里没有 FS，只改表要由 --no-fs 显式担下；不带它在写表之前就拒绝
    let (c, e) = run(&["resize-part", &format!("{img_s}:1"), "--start", "2048", "--grow-to-end"]);
    assert_eq!(c, 10, "an unidentifiable region must be refused without --no-fs: {e}");
    assert!(e.contains("--no-fs"), "{e}");

    // 扩到 last_usable_lba 16350
    let (c, e) = run(&["resize-part", &format!("{img_s}:1"), "--start", "2048", "--grow-to-end", "--no-fs"]);
    assert_eq!(c, 0, "{e}");
    let mut f = std::fs::File::open(&img).unwrap();
    let gpt = gptman::GPT::find_from(&mut f).unwrap();
    assert_eq!(gpt[1].starting_lba, 2048);
    assert_eq!(gpt[1].ending_lba, 16350);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_swap_recreate() {
    let dir = std::env::temp_dir().join(format!("diskedit_swr_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("s.img");
    // 16 MiB（32768 扇区，last_usable 32734）：root 2048..6143 + swap 6144..8191
    // apply 后：swap 尾部打包 30687..32734（零数据搬移），root 扩到 32734
    // swap 分区内预置 v1 swap 头（见下），使重建时 -U/-L 都有值可传
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
    // v1 swap 头（内核 include/linux/swap.h union swap_header）：magic 在首页尾，
    // sws_uuid @1036、sws_volume @1052。全零分区只会走"无 swap 头 → 随机 UUID"那条路，
    // mkswap 的 -U/-L 参数构造就永远进不了自动化
    const SWAP_UUID: [u8; 16] = [0xA1; 16];
    const SWAP_LABEL: [u8; 3] = [b'A', 0xFF, b'B']; // 含非法 UTF-8 字节：按字节往返的判据
    {
        let ps = 4096usize; // 探测候选里本机页优先，4096 先命中
        let base = 6144usize * 512; // swap 分区起始（LBA 6144）
        let raw = cur.get_mut();
        raw[base + ps - 10..base + ps].copy_from_slice(b"SWAPSPACE2");
        raw[base + 1036..base + 1052].copy_from_slice(&SWAP_UUID);
        raw[base + 1052..base + 1055].copy_from_slice(&SWAP_LABEL);
    }
    std::fs::write(&img, with_protective_mbr(cur.into_inner())).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();

    // --no-fs：#1 是 grow 目标且里面没有 FS，只改表由用户显式担下（否则事前拒绝）；
    // #2 的 swap 重建是**搬移**的一环（swap 头随位置走），与本开关无关，重建照旧
    let out = Command::new(exe).args(["apply", img_s, "--grow", "1", "--no-fs"]).output().unwrap();
    let code = out.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // swap 重建依赖 mkswap/losetup（仅 Linux）：工具可用 → 后置条件全满足（OK）；
    // 不可用 → 分区表已更新但 swap 未重建，属部分完成（PARTIAL），且必须给出补救命令。
    // 这里不再把"FS/swap 步骤失败"当作成功——那正是让脚本误判空间可用的根源
    #[cfg(target_os = "linux")]
    {
        // effective UID 取自 /proc/self/status 的 Uid: 行（proc(5)：四列依次为
        // real/effective/saved/filesystem，取第 2 列）；测试层不引入 libc 依赖
        let is_root = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).map(str::to_string))
            .and_then(|l| l.split_whitespace().nth(2).map(|f| f == "0"))
            .unwrap_or(false);
        if is_root {
            assert_eq!(code, 0, "apply must fully succeed as root: {stderr}");
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
            assert_eq!(code, 20, "non-root: swap step pending → PARTIAL: {stderr}");
            assert!(stderr.contains("swap rebuild") && stderr.contains("mkswap"), "remedy must be printed: {stderr}");
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(code, 20, "without mkswap the swap step is pending → PARTIAL: {stderr}");
        assert!(stderr.contains("swap rebuild") && stderr.contains("mkswap"), "remedy must be printed: {stderr}");
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
    } else {
        // swap 未重建 → 条目循环未走完，扩容收尾跳过：root 保持原终点（收尾按全体 moves
        // 取 min(new_first)−1，未走完时会把未搬移条目的旧位划进扩容区）；swap 条目已落位
        assert_eq!(gpt[1].ending_lba, 6143, "growth must not be committed while the swap rebuild is pending");
        assert_eq!(gpt[2].starting_lba, 30687, "swap entry already relocated");
        assert_eq!(gpt[2].ending_lba, 32734);
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

    let (c, _, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", img_s, "--start", "2048", "--end", "4095", "--name", "a"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", img_s, "--start", "4096", "--end", "6143", "--name", "b"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", img_s, "--start", "12288", "--end", "14335", "--name", "c"]);
    assert_eq!(c, 0, "{e}");

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

    // 无 --allow-move：右侧被 b 挡 → 拒绝
    let (c, _, stderr) = run(&["resize", &format!("{img_s}:1"), "+3M"]);
    assert_eq!(c, 10, "blocked resize without --allow-move must refuse");
    assert!(stderr.contains("--allow-move"), "{stderr}");

    // --allow-move 无 --yes：打印位移计划后拒绝
    let (c, out, stderr) = run(&["resize", &format!("{img_s}:1"), "+3M", "--allow-move"]);
    assert_eq!(c, 10, "plan printed but --yes missing must refuse");
    assert!(out.contains("move part 2"), "plan must be printed: {out}");
    assert!(out.contains("move part 3"), "plan must include the far blocker: {out}");
    assert!(stderr.contains("--yes"), "{stderr}");

    // +3M 相对当前 1M 大小 → 绝对 4M：b 让位到新末端之后，c 本就在扩容前线之外不动，
    // a 精确扩到 10239（2048+8192-1）。
    // a 里的指纹不是任何 FS 的签名（识别为 unknown），故扩分区表要显式 --no-fs
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "+3M", "--allow-move", "--yes", "--no-fs"]);
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
    let (c, _, stderr) = run(&["resize", &format!("{img_s}:1"), "+64M", "--allow-move", "--yes"]);
    assert_eq!(c, 10, "grow beyond usable range must refuse: {stderr}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// D5（C2）：resize SIZE 非扇区对齐时回显 note（实际生效的下取整值），且不改变落盘值
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
    assert_eq!(run(&["add", img_s, "--start", "2048", "--end", "4095", "--name", "root"]).0, 0);
    // 1 MiB + 1 字节非 512 整数倍：note 回显"实际生效 1 MiB"，请求本身合法（同尺寸
    // 重写）故退出 0——note 出现在越界判定之后，被拒绝的请求不会再带 note
    let (c, _, err) = run(&["resize", &format!("{img_s}:1"), "1048577"]);
    assert!(err.contains("note: size 1048577 is not a multiple of the 512-byte sector"), "{err}");
    assert!(err.contains("resizing to 1048576 bytes"), "{err}");
    assert_eq!(c, 0, "the non-aligned but in-range request must succeed: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}