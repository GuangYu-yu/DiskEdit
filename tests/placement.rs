//! 执行放置的验收矩阵：宿主能力足够时在本地跑，不足且没有载体时在首个字节写入前
//! 以 6 退出并给出配方；配了载体目录就先校验清单，不把来路不明的 guest 当成出路。
//!
//! 文件系统工具的缺口只在非 root 下出现；root 环境把它当"宿主能做"跳过这一段。

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn euid() -> u32 {
    let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|values| values.split_whitespace().nth(1))
        .and_then(|uid| uid.parse().ok())
        .expect("effective uid is in /proc/self/status")
}

/// 记录仓库按 0700 建立：写命令会复核状态根与键目录，组/其他可读即拒
fn private_dir(dir: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .expect("create the state dir");
}

fn fixture(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("diskedit_placement_")
        .tempdir()
        .expect("a fixture directory");
    let path = dir.path().join(name);
    let mut bytes = vec![0u8; 8 * 1024 * 1024];
    // MBR：一个从 LBA 2048 开始的 2 MiB 分区，签名齐全
    let entry = &mut bytes[446..462];
    entry[4] = 0x83;
    entry[8..12].copy_from_slice(&2048u32.to_le_bytes());
    entry[12..16].copy_from_slice(&4096u32.to_le_bytes());
    bytes[510] = 0x55;
    bytes[511] = 0xAA;
    std::fs::write(&path, &bytes).unwrap();
    (dir, path)
}

fn run(state: &Path, args: &[String], envs: &[(&str, &Path)]) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_DiskEdit"));
    cmd.args(args).env("DISKEDIT_STATE_DIR", state);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("run diskedit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 纯表步骤没有外部工具与特权需求：恒为 Native，不因缺少载体而拒绝
#[test]
fn pure_table_plan_runs_native() {
    let dir = tempfile::Builder::new()
        .prefix("diskedit_placement_")
        .tempdir()
        .expect("a fixture directory");
    let image = dir.path().join("blank.img");
    std::fs::write(&image, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let state = dir.path().join("state");
    private_dir(&state);
    let args: Vec<String> = ["new", &image.to_string_lossy(), "--table", "mbr", "--yes"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (code, stdout, stderr) = run(&state, &args, &[]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    let signature = std::fs::read(&image).unwrap();
    assert_eq!(&signature[510..512], &[0x55, 0xAA], "the table write did not reach the image");
}

/// 文件系统步骤：非 root 且没有载体目录时，能力缺口在首次写盘之前以 6 交出，
/// 并给出两条出路；配了载体目录则改为先校验清单
#[test]
fn matrix() {
    if euid() == 0 {
        eprintln!("skipped: a root host can run the filesystem tools, so there is no placement gap");
        return;
    }
    let (dir, image) = fixture("disk.img");
    let state = dir.path().join("state");
    private_dir(&state);
    let target = format!("{}:1", image.display());
    let args: Vec<String> = ["mkfs", &target, "ext4", "--yes"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let before = std::fs::read(&image).unwrap();

    let (code, stdout, stderr) = run(&state, &args, &[]);
    assert_eq!(code, 6, "{stdout}{stderr}");
    assert!(stderr.contains("DISKEDIT_VM_BUNDLE"), "{stderr}");
    assert!(stderr.contains("install the missing tools"), "{stderr}");
    assert_eq!(std::fs::read(&image).unwrap(), before, "the refused job wrote the target");

    let bundle = dir.path().join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    let (code, stdout, stderr) = run(&state, &args, &[("DISKEDIT_VM_BUNDLE", &bundle)]);
    assert_eq!(code, 6, "{stdout}{stderr}");
    assert!(stderr.contains("cannot read manifest.json"), "{stderr}");
    assert!(
        !stderr.contains("install the missing tools"),
        "a configured bundle must be validated, not reported as absent: {stderr}"
    );
    assert_eq!(std::fs::read(&image).unwrap(), before, "the rejected bundle wrote the target");
}
