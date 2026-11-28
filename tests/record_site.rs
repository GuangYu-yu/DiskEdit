//! 记录位置：唯一入口与 no-follow 行为。
//!
//! 记录目录里存着写前原像，父目录或叶子被换成符号链接时，读、写、删、改名都必须
//! 拒绝，而不是顺着链接作用到别处。记录字节只经记录端口进出；通用状态文件入口
//! 已经删除，这里用扫描防止它被重新引入。

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_files(directory: &Path, out: &mut Vec<PathBuf>) {
    if !directory.is_dir() {
        return;
    }
    for entry in std::fs::read_dir(directory).expect("目录可读") {
        let path = entry.expect("目录项可读").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn code_only(text: &str) -> String {
    text.lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn records_never_go_through_generic_state_file_io() {
    const GENERIC: [&str; 6] = [
        "read_file(",
        "write_state_file(",
        "append_state_file(",
        "remove_state_file(",
        "sync_directory(",
        "list_files(",
    ];
    let mut files = Vec::new();
    rust_files(&repo_root().join("src/app"), &mut files);
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("源文件可读");
        let code = code_only(&text);
        for needle in GENERIC {
            if code.contains(needle) {
                found.push(format!("{}: {needle}", file.display()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "记录路径只能经记录端口；这些调用回到了通用状态文件入口：{found:#?}"
    );
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::PathBuf;

    use diskedit::platform::{Platform, RecordEntry, host};

    fn fixture(name: &str) -> PathBuf {
        let directory = PathBuf::from("target/fixtures/record-site").join(name);
        drop(std::fs::remove_dir_all(&directory));
        std::fs::create_dir_all(&directory).expect("夹具目录可建");
        directory
    }

    #[test]
    fn reading_separates_absent_contents_and_non_files() {
        let platform = host();
        let directory = fixture("read");
        let record = directory.join("record");

        assert_eq!(
            platform.read_record(&record).expect("读记录"),
            RecordEntry::Absent
        );

        platform.write_record(&record, b"abc").expect("写记录");
        assert_eq!(
            platform.read_record(&record).expect("读记录"),
            RecordEntry::Contents(b"abc".to_vec())
        );

        std::fs::remove_file(&record).expect("删记录");
        std::fs::create_dir(&record).expect("建同名目录");
        assert_eq!(
            platform.read_record(&record).expect("读记录"),
            RecordEntry::NotARecoveryFile,
            "目录不是恢复记录"
        );
        std::fs::remove_dir(&record).expect("删目录");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let target = directory.join("elsewhere");
            std::fs::write(&target, b"zzz").expect("写链接目标");
            symlink(&target, &record).expect("建链接");
            assert_eq!(
                platform.read_record(&record).expect("读记录"),
                RecordEntry::NotARecoveryFile,
                "符号链接不是恢复记录"
            );
        }
    }

    #[test]
    fn a_regular_record_file_is_read() {
        let platform = host();
        let directory = fixture("regular");
        let path = directory.join("record");
        std::fs::write(&path, b"payload").expect("记录可写");
        assert_eq!(
            platform.read_record(&path).expect("普通文件可读"),
            RecordEntry::Contents(b"payload".to_vec())
        );
    }

    #[test]
    fn a_missing_record_is_absent() {
        let platform = host();
        let directory = fixture("missing");
        assert_eq!(
            platform
                .read_record(&directory.join("record"))
                .expect("缺失可回答"),
            RecordEntry::Absent
        );
    }

    #[test]
    fn a_symbolic_link_at_the_last_component_is_not_a_record() {
        let platform = host();
        let directory = fixture("link-last");
        let target = directory.join("elsewhere");
        std::fs::write(&target, b"payload").expect("目标可写");
        let path = directory.join("record");
        std::os::unix::fs::symlink(&target, &path).expect("链接可建");
        assert_eq!(
            platform.read_record(&path).expect("链接可回答"),
            RecordEntry::NotARecoveryFile,
            "指向别处字节的链接不能被当成本目标的记录"
        );
    }

    #[test]
    fn a_symbolic_link_in_the_parent_chain_is_not_a_record() {
        let platform = host();
        let directory = fixture("link-parent");
        let real = directory.join("real");
        std::fs::create_dir_all(&real).expect("目录可建");
        std::fs::write(real.join("record"), b"payload").expect("记录可写");
        let anchor = directory.join("anchor");
        std::os::unix::fs::symlink(&real, &anchor).expect("链接可建");
        assert_eq!(
            platform
                .read_record(&anchor.join("record"))
                .expect("链接可回答"),
            RecordEntry::NotARecoveryFile,
            "父目录是链接时整条路径都不算记录"
        );
    }

    #[test]
    fn a_directory_at_the_record_path_is_not_a_record() {
        let platform = host();
        let directory = fixture("directory");
        let path = directory.join("record");
        std::fs::create_dir_all(&path).expect("目录可建");
        assert_eq!(
            platform.read_record(&path).expect("目录可回答"),
            RecordEntry::NotARecoveryFile
        );
    }

    #[test]
    fn a_fifo_at_the_record_path_is_not_a_record_and_does_not_block() {
        let platform = host();
        let directory = fixture("fifo");
        let path = directory.join("record");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("路径无 NUL");
        let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(made, 0, "FIFO 可建");
        assert_eq!(
            platform.read_record(&path).expect("FIFO 可回答"),
            RecordEntry::NotARecoveryFile,
            "FIFO 不算记录，也不能把读取挂住"
        );
    }

    /// 跑一次 `records list --root`，返回退出码与标准错误。
    fn list_root(root: &std::path::Path) -> (i32, String) {
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

    #[test]
    fn scanning_reports_a_linked_record_as_not_a_recovery_file() {
        let directory = fixture("scan-link");
        let key = directory.join("images").join("aa");
        std::fs::create_dir_all(&key).expect("目录可建");
        let target = directory.join("elsewhere");
        std::fs::write(&target, b"payload").expect("目标可写");
        std::os::unix::fs::symlink(&target, key.join("record")).expect("链接可建");
        let (code, stderr) = list_root(&directory);
        assert_eq!(code, 4, "链接不能被报告成完整扫描：{stderr}");
        assert!(stderr.contains("is not a recovery file"), "{stderr}");
    }

    #[test]
    fn scanning_reports_a_regular_record() {
        let directory = fixture("scan-regular");
        let key = directory.join("images").join("aa");
        std::fs::create_dir_all(&key).expect("目录可建");
        std::fs::write(key.join("record"), b"payload").expect("记录可写");
        let (code, stderr) = list_root(&directory);
        assert_eq!(code, 0, "普通记录必须能列出：{stderr}");
    }

    #[test]
    fn a_parent_swapped_for_a_link_is_refused_by_every_operation() {
        let platform = host();
        let base = fixture("swap");
        let real = base.join("real");
        let other = base.join("other");
        std::fs::create_dir_all(&real).expect("记录目录可建");
        std::fs::create_dir_all(&other).expect("替身目录可建");
        std::fs::write(real.join("record"), b"original").expect("原记录可写");
        std::fs::write(other.join("record"), b"decoy").expect("替身记录可写");

        // 检查之后、动作之前，父目录被换成指向别处的链接。
        std::fs::rename(&real, base.join("moved")).expect("移走真目录");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&other, &real).expect("父目录换成链接");

        let through_link = real.join("record");
        assert_eq!(
            platform.read_record(&through_link).expect("读记录"),
            RecordEntry::NotARecoveryFile,
            "父目录是链接时不能读记录"
        );
        assert!(
            platform.write_record(&through_link, b"evil").is_err(),
            "父目录是链接时不能写记录"
        );
        assert!(
            platform.remove_record(&through_link).is_err(),
            "父目录是链接时不能删记录"
        );
        assert!(
            platform
                .rename_record(&through_link, &base.join("renamed"))
                .is_err(),
            "父目录是链接时不能改名记录"
        );
        assert_eq!(
            std::fs::read(other.join("record")).expect("替身记录可读"),
            b"decoy",
            "被拒绝的动作不能改动链接目标"
        );
        assert_eq!(
            std::fs::read(base.join("moved").join("record")).expect("原记录可读"),
            b"original",
            "被拒绝的动作不能改动原记录"
        );
    }

    #[test]
    fn write_remove_and_rename_stay_inside_the_record_directory() {
        let platform = host();
        let directory = fixture("mutate");
        let first = directory.join("record");
        let second = directory.join("record.1.abandoned");

        platform.write_record(&first, b"one").expect("写记录");
        platform.rename_record(&first, &second).expect("改名记录");
        assert_eq!(
            platform.read_record(&second).expect("读记录"),
            RecordEntry::Contents(b"one".to_vec())
        );
        assert_eq!(
            platform.read_record(&first).expect("读记录"),
            RecordEntry::Absent
        );

        platform.remove_record(&second).expect("删记录");
        assert_eq!(
            platform.read_record(&second).expect("读记录"),
            RecordEntry::Absent
        );
        let leftovers: Vec<String> = std::fs::read_dir(&directory)
            .expect("列目录")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "不应留下临时文件：{leftovers:?}");
    }
}
