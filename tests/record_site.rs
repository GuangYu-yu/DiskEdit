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

// covers: I17
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

    // covers: I17
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
