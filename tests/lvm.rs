//! LVM2 PV 的准入：PV 上的扩缩走 PV 链，`--no-fs`/`--grow-lv` 只在该链上成立。
//!
//! 宿主没有 lvm2 工具时，非 `--no-fs` 的 PV 扩缩按缺工具拒绝（退出码 6），而不是
//! 假装扩了文件系统。夹具在分区 1 起点的扇区 1/2 写入 LVM2 PV 标签，让内容识别
//! 落到 `lvm2_pv`。

// 整个套件都要求能写目标的宿主：PV 扩缩必然写 PV 元数据与分区表。
#![cfg(target_os = "linux")]

mod fixtures;

const SECTOR: usize = 512;
const PART1_START: usize = 2048;

fn pv_image(name: &str) -> String {
    fixtures::prepare();
    let directory = "target/fixtures/lvm";
    std::fs::create_dir_all(directory).expect("LVM 夹具目录可建");
    let path = format!("{directory}/{name}");
    let source = std::path::Path::new(fixtures::BASE).join("gpt.img");
    std::fs::copy(&source, &path).expect("复制夹具");
    let mut bytes = std::fs::read(&path).expect("读取夹具");
    for offset in [0usize, 512] {
        let at = PART1_START * SECTOR + offset;
        bytes[at..at + 8].copy_from_slice(b"LABELONE");
        bytes[at + 24..at + 32].copy_from_slice(b"LVM2 001");
    }
    std::fs::write(&path, bytes).expect("写回夹具");
    path
}

fn run(args: &[&str]) -> (i32, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_diskedit"))
        .args(args)
        .output()
        .expect("运行 diskedit");
    (
        output.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

/// 以受控的 `PATH` 运行：空路径让全部外部程序都不可用，与宿主是否装了 lvm2 无关。
fn run_without_tools(args: &[&str]) -> (i32, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_diskedit"))
        .args(args)
        .env("PATH", "/nonexistent")
        .output()
        .expect("运行 diskedit");
    (
        output.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

// covers: R0385, R0386, R0387, R0679, R0680, R0681
#[test]
fn pv_grow_without_the_lvm_tools_refuses_instead_of_touching_the_fs() {
    let path = pv_image("grow.img");
    // 外部程序是否可用取决于宿主；用空 PATH 明确制造"lvm2 工具不可用"的现场，
    // 否则装了 lvm2 的宿主会走到另一条分支，让这条用例随环境时绿时红。
    let (code, text) =
        run_without_tools(&["resize", &format!("{path}:1"), "--end", "3000", "--yes"]);
    assert_eq!(code, 6, "缺 lvm2 工具应按不支持拒绝：{text}");
    assert!(
        text.contains("Pvs") || text.contains("pvresize"),
        "拒绝文案应点名缺少的工具：{text}"
    );
}

#[test]
fn no_fs_is_refused_on_a_pv() {
    let path = pv_image("no-fs.img");
    let (code, text) = run(&[
        "resize",
        &format!("{path}:1"),
        "--end",
        "4000",
        "--no-fs",
        "--yes",
    ]);
    assert_eq!(code, 3, "PV 上 --no-fs 应被拒绝：{text}");
    assert!(text.contains("--no-fs"), "{text}");
}

#[test]
fn grow_lv_on_a_non_pv_is_refused() {
    let path = pv_image("non-pv.img");
    // 分区 2 是零字节内容，不是 PV。
    let (code, text) = run(&["resize", &format!("{path}:2"), "grow", "--grow-lv", "--yes"]);
    assert_eq!(code, 3, "--grow-lv 只能用在 PV 上：{text}");
    assert!(text.contains("LVM2 PV"), "{text}");
}
