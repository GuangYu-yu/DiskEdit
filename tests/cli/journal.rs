use super::*;

#[test]
fn journal_lifecycle_and_table_undo() {
    let dir = std::env::temp_dir().join(format!("diskedit_jl_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("j.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let journal = sidecar(&dir, "j.img", JOURNAL_SUFFIX);
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    // mkfs 工具从 PATH 上摘掉（只留 exe 所在目录）：类型认得但工具不在 ⇒ 分区建成、
    // mkfs 起不来——PARTIAL(20)，journal 保留且不落屏障，undo 可整表回滚
    let exe_dir = std::path::Path::new(exe).parent().unwrap().to_path_buf();
    let run_no_mkfs = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).env("PATH", &exe_dir).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let part_count = || -> usize {
        let out = Command::new(exe).args(["info", img_s]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v["partitions"].as_array().unwrap().len()
    };

    // 成功完成的破坏性命令 ⇒ journal 删除（撤销窗口关闭，不留残留）
    let (c, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    assert!(!journal.exists(), "journal must be dropped after a successful new");
    let (c, e) = run(&["add", img_s, "--start", "2048", "--end", "4095", "--name", "x"]);
    assert_eq!(c, 0, "{e}");
    assert!(!journal.exists(), "journal must be dropped after a successful add");
    let (c, e) = run(&["set", &format!("{img_s}:1"), "flag", "esp", "on"]);
    assert_eq!(c, 0, "{e}");
    assert!(!journal.exists(), "journal must be dropped after a successful set");

    // 只读命令不得触碰（也不得误删）journal
    let (c, _) = run(&["info", img_s]);
    assert_eq!(c, 0);
    assert!(!journal.exists(), "read-only commands must not create a journal");

    // 未接线的 FS 类型是请求本身的错误：事前拒绝，此刻什么都没写
    let before = part_count();
    let (c, e) = run(&["create", img_s, "--size", "1M", "--fs", "no-such-fs-type"]);
    assert_eq!(c, 10, "an unsupported fstype must be refused upfront: {e}");
    assert_eq!(part_count(), before, "the refusal must leave the table untouched");
    assert!(!journal.exists(), "the refusal must not open a journal");

    // 部分失败（mkfs 工具不存在）⇒ journal 保留，供 undo 撤销半成品
    let (c, e) = run_no_mkfs(&["create", img_s, "--size", "1M", "--fs", "ext4"]);
    assert_eq!(c, 20, "partition created but mkfs failed must be EXIT_PARTIAL: {e}");
    assert!(journal.exists(), "journal must be kept after a partial failure");
    assert_eq!(part_count(), before + 1, "partition should exist before undo");

    // undo 回滚表操作：分区消失，journal 清除
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "undo of a table-only journal must succeed: {e}");
    assert!(!journal.exists(), "journal must be removed after undo");
    assert_eq!(part_count(), before, "undone partition must be gone");

    // journal 尾部未完成（截断 / 未写完的记录头）⇒ 那是"未完成的事务"而非损坏：
    // 记录先于写入落盘，故那条记录对应的写入根本没发生，丢弃它安全；完整前缀照常回放
    let (c, e) = run_no_mkfs(&["create", img_s, "--size", "1M", "--fs", "ext4"]);
    assert_eq!(c, 20, "{e}");
    let jb = std::fs::read(&journal).unwrap();
    std::fs::write(&journal, &jb[..jb.len() - 2]).unwrap();
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "a truncated tail must be treated as an unfinished append: {e}");
    assert_eq!(part_count(), before, "the complete prefix must still be replayed");
    assert!(!journal.exists(), "journal must be removed after a successful undo");

    // 尾部垃圾 = 未写完的记录头，同样按未完成处理
    let (c, e) = run_no_mkfs(&["create", img_s, "--size", "1M", "--fs", "ext4"]);
    assert_eq!(c, 20, "{e}");
    let jb = std::fs::read(&journal).unwrap();
    let mut tail = jb.clone();
    tail.extend_from_slice(&[0xFF; 5]);
    std::fs::write(&journal, &tail).unwrap();
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "an unterminated trailing header is an unfinished append: {e}");
    assert_eq!(part_count(), before, "the complete prefix must still be replayed");

    // 中途损坏（第 1 条记录的数据）⇒ 整体拒绝，不碰镜像——"不做部分回放"针对的是这种情形
    let (c, e) = run_no_mkfs(&["create", img_s, "--size", "1M", "--fs", "ext4"]);
    assert_eq!(c, 20, "{e}");
    let jb = std::fs::read(&journal).unwrap();
    let mut mid = jb.clone();
    mid[V4_HEADER_LEN + REC_HDR + 1] ^= 0xFF; // 头之后是记录头 `[len][crc]`，再往后即第 1 条记录的数据
    std::fs::write(&journal, &mid).unwrap();
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_ne!(c, 0, "mid-file corruption must refuse undo: {e}");
    assert_eq!(part_count(), before + 1, "a refused undo must not touch the image");

    // 恢复完整 journal → undo 正常
    std::fs::write(&journal, &jb).unwrap();
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "intact journal must undo cleanly: {e}");
    assert_eq!(part_count(), before, "undone partition must be gone");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 目标在 journal 写下之后被外部缩短：v4 的头里记着创建时的目标指纹，而尺寸处处可读，
/// 故"缩短"就是"尺寸不符"，回放因此在任何字节落地之前被拒（这条判据不区分平台）。没有尺寸
/// 证据的载体（真块设备节点：元数据长度恒 0）上改由逐条核对在"记录越过了新的 EOF"处露出。
/// 两条路都必须在第一个字节写回之前拦下——"越过 EOF 的写会把文件写长"正是这里要防的那条路
#[test]
fn undo_refuses_when_the_target_shrank_under_its_journal() {
    let dir = std::env::temp_dir().join(format!("diskedit_shr_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("s.img");
    let full = 8 * 1024 * 1024;
    std::fs::write(&img, vec![0u8; full]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let journal = sidecar(&dir, "s.img", JOURNAL_SUFFIX);
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    // mkfs 工具从 PATH 上摘掉：create 建成分区、mkfs 起不来 ⇒ PARTIAL，journal 留在盘上
    let exe_dir = std::path::Path::new(exe).parent().unwrap().to_path_buf();
    let run_no_mkfs = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).env("PATH", &exe_dir).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let set_len = |n: u64| {
        std::fs::OpenOptions::new().write(true).open(&img).unwrap().set_len(n).unwrap();
    };

    let (c, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, e) = run(&["add", img_s, "--start", "2048", "--end", "4095"]);
    assert_eq!(c, 0, "{e}");
    let (c, e) = run_no_mkfs(&["create", img_s, "--size", "1M", "--fs", "ext4"]);
    assert_eq!(c, 20, "partition created but mkfs failed must be EXIT_PARTIAL: {e}");
    assert!(journal.exists(), "a partial operation leaves its journal behind: {e}");

    // 缩到一半：头里记的 size 与现状不符，且 journal 里落在旧末端附近的记录（表写入）
    // 越过了新的 EOF——两道判据都指向"这不是当时那个目标"，都必须拒绝
    let half = 4 * 1024 * 1024;
    set_len(half);
    // 拒绝前的整盘内容：核对整批发生在第一个字节写回之前，所以"一个字都没写回"是
    // 这里可断言的事实（逐条边核对边回放的话，能读到的那几条已经先被回滚了）
    let before = std::fs::read(&img).unwrap();
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_ne!(c, 0, "undo must refuse a target that is no longer the one the journal was written for: {e}");
    // 尺寸处处可读，故这条判据不区分平台：镜像被缩短即在身份核对处被拦下
    assert!(e.contains("size changed"), "a shrink must be caught by the size field of the identity check: {e}");
    // 出路必须说清：先把目标恢复原状再重跑，或确认不再需要回滚时 abandon
    assert!(e.contains("restore the target to the state it was in"), "{e}");
    assert!(e.contains("diskedit abandon"), "{e}");
    assert_eq!(std::fs::read(&img).unwrap(), before, "a refused replay must not write back any entry");
    assert!(journal.exists(), "the journal must be kept for a retry against the target as it was");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 身份核对拦下"为别的目标写下的 journal"：v4 的头记着创建时目标的指纹与 loop 映射，
/// 与当前目标不符即拒绝回放。拒绝必须落在任何写回之前，且不动盘面、不删现场——放弃它
/// 是用户的决定（`abandon`），不是拒绝的副作用。
///
/// 这里改的是头里"经 loop 写入"这一位（magic 之后第 24 字节，即身份段里 dev/ino/size
/// 各 8 字节之后的那一个）：真实世界里它对应"现场是经 loop 设备写下的，而当前目标不是"。它与指纹同属
/// `dev::scene_mismatch` 的判据（两族恢复现场共用），且不依赖平台是否提供稳定 inode——
/// 纯指纹不符的用例在 `dev::tests` 里有另一份
#[test]
fn undo_refuses_a_journal_whose_recorded_scene_is_not_this_target() {
    let dir = std::env::temp_dir().join(format!("diskedit_foreign_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("f.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let journal = sidecar(&dir, "f.img", JOURNAL_SUFFIX);
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let exe_dir = std::path::Path::new(exe).parent().unwrap().to_path_buf();
    let run_no_mkfs = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).env("PATH", &exe_dir).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    let (c, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, e) = run(&["add", img_s, "--start", "2048", "--end", "4095"]);
    assert_eq!(c, 0, "{e}");
    // mkfs 工具从 PATH 上摘掉 ⇒ 建成分区但 PARTIAL，留下一份带记录的真 journal
    let (c, e) = run_no_mkfs(&["create", img_s, "--size", "1M", "--fs", "ext4"]);
    assert_eq!(c, 20, "{e}");
    assert!(journal.exists(), "a partial operation leaves its journal behind: {e}");

    let mut jb = std::fs::read(&journal).unwrap();
    assert!(jb.len() > V4_HEADER_LEN, "the journal must hold at least one record to reach the identity check");
    jb[5 + 24] = 1; // 记成"经 loop 写入"
    v4_resign_header(&mut jb);
    std::fs::write(&journal, &jb).unwrap();

    let before = std::fs::read(&img).unwrap();
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_ne!(c, 0, "a journal written for another scene must not be replayed: {e}");
    assert!(e.contains("this journal is not replayed here"), "{e}");
    assert!(e.contains("loop device"), "the refusal must name the field that differs: {e}");
    assert!(e.contains("restore the target to the state it was in"), "the refusal must point at the way out: {e}");
    assert!(e.contains("diskedit abandon"), "the refusal must point at the way out: {e}");
    assert_eq!(std::fs::read(&img).unwrap(), before, "a refused replay must not write back any entry");
    assert_eq!(std::fs::read(&journal).unwrap(), jb, "a refused replay must leave the journal as it was");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 不开 journal 的写盘命令（mkfs / resizefs / check 的修复）在目标上留有未收尾现场时必须
/// 拒绝：它们会作废那份 journal 所指的旧布局，而用户随后 undo 仍会把旧表字节回放上去，
/// 形成"表与盘上内容自相矛盾"的状态。另外四条同族不变量：
/// - 0 字节的空壳是"残骸"（零次写入），不是"journal 损坏"；
/// - 零记录的完整头同样描述"零次写入"：两者都不构成未收尾现场，否则一次在创建 journal
///   时掉电就会把目标永久锁死；
/// - 1..头长-1 字节的**半截头**不是残骸：它与"原 journal 被外部截断"不可区分，按现场处理；
/// - 旧格式（v3）同样按现场处理：版本只升不兼，拒绝不删现场，只有 `abandon` 能释放；
/// - undo 成功回滚后要一并释放该事务的 checkpoint（否则它描述一个已被回滚掉的世界，
///   让后续 resize 被判成 Divergent 而永久拒绝）
#[test]
fn pending_recovery_blocks_unjournaled_writers_and_empty_shell_is_not_corruption() {
    let dir = std::env::temp_dir().join(format!("diskedit_pr_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("p.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let journal = sidecar(&dir, "p.img", JOURNAL_SUFFIX);
    let ckpt = sidecar(&dir, "p.img", CHECKPOINT_SUFFIX);
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    // mkfs 工具从 PATH 上摘掉（只留 exe 所在目录）：类型认得但工具不在 ⇒ 分区建成、
    // mkfs 起不来——PARTIAL(20)，journal 保留且不落屏障，undo 可整表回滚
    let exe_dir = std::path::Path::new(exe).parent().unwrap().to_path_buf();
    let run_no_mkfs = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).env("PATH", &exe_dir).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    let (c, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, e) = run(&["add", img_s, "--start", "2048", "--end", "4095"]);
    assert_eq!(c, 0, "{e}");

    // 造一个真正的未收尾现场：create 建好了分区、随后的 mkfs 起不来（工具不在），
    // 事务把 journal 留在目标上等 undo 或续跑
    let (c, e) = run_no_mkfs(&["create", img_s, "--size", "1M", "--fs", "ext4"]);
    assert_eq!(c, 20, "partition created but mkfs failed must be EXIT_PARTIAL: {e}");
    assert!(journal.exists(), "a partial operation must leave its journal behind: {e}");

    // 未收尾现场 ⇒ 目标仍被那次事务占着：**任何**写命令都不能接手它，含开 journal 的 add。
    // 断言必须落在措辞上——30 也可由别的理由产生（工具链缺失、平台不支持），只看退出码
    // 会把"闸口没生效"误判成通过；而且每次都要确认那份 journal 没被顺手删掉：
    // "被拒绝的命令销毁了别人的 durable history"正是这个洞最恶劣的形态
    let p1 = format!("{img_s}:1");
    for argv in [
        vec!["mkfs", p1.as_str(), "ext4", "--yes"],
        vec!["check", p1.as_str()],
        vec!["resizefs", p1.as_str()],
        vec!["add", img_s, "--start", "6144", "--end", "8191"],
        // copy 声明"没有可续跑的东西"，但正因如此更不能接管别人的现场——
        // 它走同一个严格打开闸，被拒绝在几何计算之前
        vec!["copy", p1.as_str(), "--start", "6144"],
    ] {
        let (c, e) = run(&argv);
        assert_eq!(c, 30, "{argv:?} must be refused while another transaction owns the target: {e}");
        assert!(e.contains("owns this target"), "{argv:?} must be stopped by the gate: {e}");
        assert!(e.contains("undo"), "{argv:?}: the refusal must point at the way out: {e}");
        assert!(journal.exists(), "{argv:?} must not destroy the other transaction's history: {e}");
    }

    // 现场收拾干净（undo 成功）⇒ 目标重新可用（断言针对措辞，因为 30 也可能来自工具链缺失等别的拒绝）
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "undo must release the pending state: {e}");
    assert!(!journal.exists(), "undo must drop the journal: {e}");
    let (c, e) = run(&["check", &format!("{img_s}:1")]);
    assert!(
        !e.contains("owns this target"),
        "the gate must let it through once no recovery state remains: code={c} {e}"
    );

    // 0 字节空壳：按残骸处理（补写 magic 后照常工作），不报"journal 损坏"
    std::fs::write(&journal, b"").unwrap();
    let (c, e) = run(&["set", &format!("{img_s}:1"), "flag", "esp", "on"]);
    assert_eq!(c, 0, "an empty journal shell must not be reported as corruption: {e}");
    assert!(!journal.exists(), "a successful journaled command must drop the journal");

    // 只含头、零记录的 journal（`ensure` 写完头、第一条记录还没落盘就中断留下的）：它描述的
    // 是零次写入，既不该挡住 mkfs，也不该被报成未收尾现场
    std::fs::write(&journal, v4_header_only()).unwrap();
    let (c, e) = run(&["mkfs", &format!("{img_s}:1"), "ext4", "--yes"]);
    assert!(
        !e.contains("owns this target"),
        "a record-less journal header must not block other writers: code={c} {e}"
    );

    // 但**半截头**（1..头长-1 字节，这里取前 20 字节）属于"读不出来的现场"：它既可能是一次
    // 没写完的初始化，也可能是原 journal 被外部截断到这么短——两者在内容上不可区分，而后者是
    // 丢了记录的现场。故不按"零次写入"放行：写命令被挡（30）、undo 也拒绝、文件一字不动，
    // 只有 `abandon` 能释放（它是改名保留字节，不是覆盖）
    let torn = v4_header_only()[..20].to_vec();
    std::fs::write(&journal, &torn).unwrap();
    let (c, e) = run(&["mkfs", &format!("{img_s}:1"), "ext4", "--yes"]);
    assert_eq!(c, 30, "a torn header must stop other writers: {e}");
    assert!(e.contains("owns this target"), "{e}");
    assert!(e.contains("abandon"), "the refusal must point at the way out: {e}");
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 30, "a torn header must not be replayed: {e}");
    assert!(e.contains("abandon"), "the refusal must point at the way out: {e}");
    assert_eq!(std::fs::read(&journal).unwrap(), torn, "a torn header must be left byte-identical");
    let (c, e) = run(&["abandon", img_s, "--yes"]);
    assert_eq!(c, 0, "abandon must be able to release a torn header: {e}");
    assert!(!journal.exists(), "abandon must move it out of the active name: {e}");

    // 但**旧格式**的 journal（magic 的尾字节即格式版本）属于"读不出来的现场"：记录布局已经变了，
    // 一律不猜着回放，而是当作有事没做完——目标仍被它占着（30）。版本只升不兼，拒绝也不删现场：
    // 那份旧 journal 只能由用户显式 `abandon` 释放
    let v3 = b"DEJL\x03".to_vec();
    std::fs::write(&journal, &v3).unwrap();
    let (c, e) = run(&["mkfs", &format!("{img_s}:1"), "ext4", "--yes"]);
    assert_eq!(c, 30, "a journal of an older format must stop other writers: {e}");
    assert!(e.contains("abandon"), "the refusal must point at the way out: {e}");
    assert_eq!(std::fs::read(&journal).unwrap(), v3, "refusing an older format must leave it byte-identical");
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 30, "an older format must not be replayed either: {e}");
    assert!(e.contains("abandon"), "the refusal must point at the way out: {e}");
    assert_eq!(std::fs::read(&journal).unwrap(), v3, "undo must not touch a journal it cannot read");
    let (c, e) = run(&["abandon", img_s, "--yes"]);
    assert_eq!(c, 0, "abandon must be able to release an unreadable journal: {e}");
    assert!(!journal.exists(), "abandon must move it out of the active name: {e}");

    // undo 回滚成功 ⇒ 同一目标的 checkpoint 一并释放
    let (c, e) = run_no_mkfs(&["create", img_s, "--size", "1M", "--fs", "ext4"]);
    assert_eq!(c, 20, "partition created but mkfs failed must be EXIT_PARTIAL: {e}");
    std::fs::write(&ckpt, b"stale checkpoint bytes").unwrap();
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "undo of a table-only journal must succeed: {e}");
    assert!(!ckpt.exists(), "a completed rollback must release that transaction's checkpoint");

    let _ = std::fs::remove_dir_all(&dir);
}

/// undo 收掉零记录的空壳（头完整、身份可归属），但拒绝删一份身份对不上的壳：
/// "读出来是空的"不是删除授权。也验证空壳与未收尾 checkpoint 并存时不报"目标干净"
#[test]
fn undo_removes_an_empty_shell_only_when_its_identity_matches() {
    let dir = std::env::temp_dir().join(format!("diskedit_shell_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("sh.img");
    std::fs::write(&img, vec![0u8; 4 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let journal = sidecar(&dir, "sh.img", JOURNAL_SUFFIX);
    let ckpt = sidecar(&dir, "sh.img", CHECKPOINT_SUFFIX);
    let run = |args: &[&str]| -> (i32, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
    };

    // 全零身份段：没有可比字段，归属由"候选落点由身份派生"承担——清理
    std::fs::write(&journal, v4_header_only()).unwrap();
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 10, "an empty shell means there is nothing to undo: {e}");
    assert!(e.contains("empty journal shell"), "the conclusion must say what was removed: {e}");
    assert!(!journal.exists(), "the empty shell must be removed: {e}");

    // 空壳 + 未收尾 checkpoint：不能报"目标干净"，也一字不动
    std::fs::write(&journal, v4_header_only()).unwrap();
    std::fs::write(&ckpt, b"incomplete relocation").unwrap();
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 10, "{e}");
    assert!(e.contains("unfinished checkpoint"), "a leftover checkpoint must not be hidden: {e}");
    assert!(journal.exists(), "nothing may be removed while another job is unfinished: {e}");
    std::fs::remove_file(&ckpt).unwrap();

    // 身份段写着别的尺寸：与本目标不符，拒绝删除并原样保留
    let mut foreign = v4_header_only();
    foreign[5 + 8 + 8..5 + 8 + 8 + 8].copy_from_slice(&12345u64.to_le_bytes());
    v4_resign_header(&mut foreign);
    std::fs::write(&journal, &foreign).unwrap();
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 10, "an empty shell of another target must be refused: {e}");
    assert!(e.contains("not removed here"), "{e}");
    assert_eq!(std::fs::read(&journal).unwrap(), foreign, "a foreign shell must be left byte-identical");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 一个目标一把锁。正面：别的持有者还在时写命令必须拒绝，而不是与它并行改同一块盘。
/// 反面同样重要：**残留的锁文件不构成任何阻挡**——判据是"锁取不取得到"，不是"文件在不在"。
/// 锁文件一定会残留，因为释放时删除它有竞态（另一个进程可能刚取到同一把锁）
#[test]
fn target_lock_serializes_writers_and_a_stale_lock_file_is_harmless() {
    let dir = std::env::temp_dir().join(format!("diskedit_tl_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("t.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let lock = sidecar(&dir, "t.img", LOCK_SUFFIX);
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    let (c, _, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");

    // 另一个持有者占着锁（不同的进程/句柄）：写命令必须拒绝并说清成因
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)
        .unwrap();
    holder.try_lock().unwrap();
    let (c, _, e) = run(&["add", img_s, "--start", "2048", "--end", "4095"]);
    assert_eq!(c, 30, "a held target lock must stop another writer: {e}");
    assert!(e.contains("another diskedit"), "the refusal must name the cause: {e}");

    // 反面：只读命令**不取所有权**，因此不被别人持有的锁挡住——`info` / `plan` 恰恰
    // 可能被用来查看一块正被写入的盘。（写命令里"先只读看一眼再取锁"的阶段同理）
    let (c, _, e) = run(&["info", img_s]);
    assert_eq!(c, 0, "a read-only command must not be blocked by a held lock: {e}");
    drop(holder);

    // 锁已释放、文件仍在：必须照常工作
    assert!(lock.exists(), "the lock file persists by design");
    let (c, _, e) = run(&["add", img_s, "--start", "2048", "--end", "4095"]);
    assert_eq!(c, 0, "a released lock must not block anything: {e}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// abandon 的完整契约：不依赖记录可解析性（损坏的 journal 也必须能释放）、幂等、
/// 多份现场一起处理、盘上字节一个都不动、单文件改名为固定落点、以及"中途崩溃后重跑收敛"
#[test]
fn abandon_releases_recovery_state_idempotently_and_converges() {
    let dir = std::env::temp_dir().join(format!("diskedit_ab_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("a.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let journal = sidecar(&dir, "a.img", JOURNAL_SUFFIX);
    let ckpt = sidecar(&dir, "a.img", CHECKPOINT_SUFFIX);
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    // mkfs 工具从 PATH 上摘掉（只留 exe 所在目录）：类型认得但工具不在 ⇒ create
    // 建成分区、mkfs 记 PARTIAL——journal 保留，正是本测试要收拾的现场
    let exe_dir = std::path::Path::new(exe).parent().unwrap().to_path_buf();
    let run_no_mkfs = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(exe).args(args).env("PATH", &exe_dir).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    let (c, _, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    let (c, _, e) = run(&["add", img_s, "--start", "2048", "--end", "4095"]);
    assert_eq!(c, 0, "{e}");

    // 放弃是不可逆的：没有 --yes 一律拒绝
    let (c, _, _) = run(&["abandon", img_s]);
    assert_eq!(c, 10, "abandon must require --yes");

    // 没有现场 ⇒ 空跑即成功（目标已经是"没有 active transaction"）
    let (c, o, e) = run(&["abandon", img_s, "--yes"]);
    assert_eq!(c, 0, "abandoning a clean target must be a no-op: {e}");
    assert!(o.contains("nothing to abandon"), "the no-op must say so: {o}");

    let before = std::fs::read(&img).unwrap();

    // 造两份现场：一份**读不出来**的 journal（陌生内容）与一份 checkpoint。
    // 前者正是 abandon 与 undo 的分界——undo 会因无法解析而拒绝，abandon 必须照常释放
    std::fs::write(&journal, b"not a diskedit journal at all").unwrap();
    std::fs::write(&ckpt, b"stale checkpoint bytes").unwrap();
    // 顺带钉住"固定落点"这一条：上一次 abandon 的残骸就在那儿，这次必须能覆盖它
    let journal_abandoned = dir.join(format!("a.img{JOURNAL_SUFFIX}{ABANDONED_SUFFIX}"));
    std::fs::write(&journal_abandoned, b"left over from an earlier abandon").unwrap();

    let (c, o, e) = run(&["abandon", img_s, "--yes"]);
    assert_eq!(c, 0, "abandon must release an unreadable journal: {e}");
    assert!(e.contains("not readable as a journal"), "it must warn what is being given up: {e}");
    assert!(o.contains("abandoned 2 recovery record(s)"), "both records must be released: {o}");
    assert!(!journal.exists(), "the journal must leave its active name");
    assert!(!ckpt.exists(), "the checkpoint must leave its active name");
    assert!(journal_abandoned.exists(), "the journal must land on the fixed .abandoned name");
    assert!(dir.join(format!("a.img{CHECKPOINT_SUFFIX}{ABANDONED_SUFFIX}")).exists(), "same for the checkpoint");
    assert_eq!(std::fs::read(&img).unwrap(), before, "abandon must not touch a byte of the target");

    // 现场没了 ⇒ 目标重新可用（断言针对措辞；30 也可能来自工具链缺失等别的拒绝）
    let (_, _, e) = run(&["check", &format!("{img_s}:1")]);
    assert!(!e.contains("owns this target"), "the gate must be clear now: {e}");

    // 幂等：再来一次仍是空跑
    let (c, o, _) = run(&["abandon", img_s, "--yes"]);
    assert_eq!(c, 0, "a second abandon must be a no-op");
    assert!(o.contains("nothing to abandon"), "{o}");

    // 崩溃重跑收敛：模拟"转换到一半就崩"，剩余的那份由下一次运行补上
    let (c, _, e) = run_no_mkfs(&["create", img_s, "--size", "1M", "--fs", "ext4"]);
    assert_eq!(c, 20, "{e}");
    std::fs::write(&ckpt, b"stale checkpoint bytes").unwrap();
    std::fs::rename(&ckpt, dir.join(format!("a.img{CHECKPOINT_SUFFIX}{ABANDONED_SUFFIX}"))).unwrap(); // 已转换完的那一份
    let (c, o, e) = run(&["abandon", img_s, "--yes"]);
    assert_eq!(c, 0, "abandon must converge on the rest: {e}");
    assert!(o.contains("abandoned 1 recovery record(s)"), "only the leftover is still active: {o}");
    assert!(!journal.exists(), "the leftover journal must be released by the re-run");

    // 同名冲突之一：目标名已是**同一个 inode**（上次 link 成功、删原件那步没跑完的残局）
    // ⇒ 收敛即成功：删掉原文件，绝不覆盖那份副本
    let (c, _, e) = run_no_mkfs(&["create", img_s, "--size", "1M", "--fs", "ext4"]);
    assert_eq!(c, 20, "{e}");
    let j_abandoned = dir.join(format!("a.img{JOURNAL_SUFFIX}{ABANDONED_SUFFIX}"));
    let _ = std::fs::remove_file(&j_abandoned);
    std::fs::hard_link(&journal, &j_abandoned).unwrap();
    assert!(journal.exists() && j_abandoned.exists(), "the fixture is the leftover state");
    let (c, o, e) = run(&["abandon", img_s, "--yes"]);
    assert_eq!(c, 0, "a same-inode leftover must converge: {e}");
    assert!(o.contains("abandoned 1 recovery record(s)"), "{o}");
    assert!(!journal.exists(), "the active name must be dropped");
    assert!(j_abandoned.exists(), "the abandoned copy must survive");

    // 同名冲突之二：首选名是**另一个文件** ⇒ 顺延到 `.abandoned.2`：既不覆盖别人的
    // 记录，也不为了"名字冲突"把目标锁死（那会让第二次中断后再无出路）
    let (c, _, e) = run_no_mkfs(&["create", img_s, "--size", "1M", "--fs", "ext4"]);
    assert_eq!(c, 20, "{e}");
    let _ = std::fs::remove_file(&j_abandoned);
    std::fs::write(&j_abandoned, b"an unrelated artifact").unwrap();
    let (c, o, e) = run(&["abandon", img_s, "--yes"]);
    assert_eq!(c, 0, "a taken preferred name must fall back to a free one: {e}");
    assert!(o.contains("abandoned 1 recovery record(s)"), "{o}");
    assert!(!journal.exists(), "the active record must still be released");
    assert_eq!(
        std::fs::read(&j_abandoned).unwrap(),
        b"an unrelated artifact",
        "the existing artifact must be untouched"
    );
    assert!(
        dir.join(format!("a.img{JOURNAL_SUFFIX}{ABANDONED_SUFFIX}.2")).exists(),
        "the released record must land on the fallback name"
    );

    let _ = std::fs::remove_dir_all(&dir);
}