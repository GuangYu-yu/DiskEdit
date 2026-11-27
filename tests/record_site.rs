//! 记录位置的读取：只接受普通文件，任何一级符号链接都不跟随。
//!
//! 文档承诺的是"探测给出 absent / decodable / damaged / unrecognized /
//! not a recovery file / unreadable"六种回答；这里锁定"不是恢复文件"这一种在
//! Linux 宿主上的判定：最后一级链接、父目录链接、目录、FIFO 都不算记录。

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};

use diskedit::platform::{Platform, RecordEntry, host};

fn case(name: &str) -> PathBuf {
    let directory = PathBuf::from("target/fixtures/record-site").join(name);
    drop(std::fs::remove_dir_all(&directory));
    std::fs::create_dir_all(&directory).expect("夹具目录可建");
    // 绝对路径：符号链接的目标必须真的可解析，否则用例证明的会是"悬空链接"。
    std::fs::canonicalize(&directory).expect("夹具目录可解析")
}

// covers: R0651
#[test]
fn a_regular_record_file_is_read() {
    let directory = case("regular");
    let path = directory.join("record");
    std::fs::write(&path, b"payload").expect("记录可写");
    assert_eq!(
        host().read_record(&path).expect("普通文件可读"),
        RecordEntry::Contents(b"payload".to_vec())
    );
}

// covers: R0651
#[test]
fn a_missing_record_is_absent() {
    let directory = case("missing");
    assert_eq!(
        host()
            .read_record(&directory.join("record"))
            .expect("缺失可回答"),
        RecordEntry::Absent
    );
}

// covers: R0651
#[test]
fn a_symbolic_link_at_the_last_component_is_not_a_record() {
    let directory = case("link-last");
    let target = directory.join("elsewhere");
    std::fs::write(&target, b"payload").expect("目标可写");
    let path = directory.join("record");
    std::os::unix::fs::symlink(&target, &path).expect("链接可建");
    assert_eq!(
        host().read_record(&path).expect("链接可回答"),
        RecordEntry::NotARecoveryFile,
        "指向别处字节的链接不能被当成本目标的记录"
    );
}

// covers: R0651
#[test]
fn a_symbolic_link_in_the_parent_chain_is_not_a_record() {
    let directory = case("link-parent");
    let real = directory.join("real");
    std::fs::create_dir_all(&real).expect("目录可建");
    std::fs::write(real.join("record"), b"payload").expect("记录可写");
    let anchor = directory.join("anchor");
    std::os::unix::fs::symlink(&real, &anchor).expect("链接可建");
    assert_eq!(
        host()
            .read_record(&anchor.join("record"))
            .expect("链接可回答"),
        RecordEntry::NotARecoveryFile,
        "父目录是链接时整条路径都不算记录"
    );
}

// covers: R0651
#[test]
fn a_directory_at_the_record_path_is_not_a_record() {
    let directory = case("directory");
    let path = directory.join("record");
    std::fs::create_dir_all(&path).expect("目录可建");
    assert_eq!(
        host().read_record(&path).expect("目录可回答"),
        RecordEntry::NotARecoveryFile
    );
}

// covers: R0651
#[test]
fn a_fifo_at_the_record_path_is_not_a_record_and_does_not_block() {
    let directory = case("fifo");
    let path = directory.join("record");
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("路径无 NUL");
    let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(made, 0, "FIFO 可建");
    assert_eq!(
        host().read_record(&path).expect("FIFO 可回答"),
        RecordEntry::NotARecoveryFile,
        "FIFO 不算记录，也不能把读取挂住"
    );
}

/// 跑一次 `records list --root`，返回退出码与标准错误。
fn list_root(root: &Path) -> (i32, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_diskedit"))
        .args(["records", "list", "--root"])
        .arg(root)
        .output()
        .expect("命令可运行");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

// covers: I15
#[test]
fn scanning_reports_a_linked_record_as_not_a_recovery_file() {
    let directory = case("scan-link");
    let key = directory.join("images").join("aa");
    std::fs::create_dir_all(&key).expect("目录可建");
    let target = directory.join("elsewhere");
    std::fs::write(&target, b"payload").expect("目标可写");
    std::os::unix::fs::symlink(&target, key.join("record")).expect("链接可建");
    let (code, stderr) = list_root(&directory);
    assert_eq!(code, 4, "链接不能被报告成完整扫描：{stderr}");
    assert!(stderr.contains("is not a recovery file"), "{stderr}");
}

// covers: I15
#[test]
fn scanning_reports_a_regular_record() {
    let directory = case("scan-regular");
    let key = directory.join("images").join("aa");
    std::fs::create_dir_all(&key).expect("目录可建");
    std::fs::write(key.join("record"), b"payload").expect("记录可写");
    let (code, stderr) = list_root(&directory);
    assert_eq!(code, 0, "普通记录必须能列出：{stderr}");
}
