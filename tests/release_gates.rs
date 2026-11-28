//! 释放与撤销入口的闸门：记录读不出来、归属不符与未完成态都必须拒绝。
//!
//! 判定层用构造好的现场直接调用生产入口；命令行入口在 Linux 上以隔离的状态根
//! 运行二进制，核对退出码、文案与记录文件是否仍在。

use diskedit::app::decide::{Decision, Refusal, release, undo};
use diskedit::app::observe::records::RecordState;
use diskedit::app::observe::{Command, RecoverSnapshot, Request, TargetFacts, TargetSpec};
use diskedit::platform::{Occupancy, TargetKind};
use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::record::{Frame, FrameKind, Preimage, RecordBody, encode};

const TARGET: Fingerprint = Fingerprint::strong([0x42; 32]);
const OTHER: Fingerprint = Fingerprint::strong([0x99; 32]);
const MIB: u64 = 1024 * 1024;

fn snapshot(bytes: Option<Vec<u8>>, unavailable: Option<&str>) -> RecoverSnapshot {
    RecoverSnapshot {
        target: TargetFacts {
            display: "disk.img".to_owned(),
            path: "disk.img".into(),
            bytes: 4 * MIB,
            kind: TargetKind::File,
            sector_size: 512,
            identity: Some(TARGET),
        },
        record: RecordState {
            directory: Some("state/images/x".into()),
            directory_exists: true,
            file: Some("state/images/x/record".into()),
            bytes,
            unavailable: unavailable.map(str::to_owned),
        },
        occupancy: Occupancy::Idle,
        is_partition_node: false,
    }
}

fn request(command: Command) -> Request {
    Request::new(TargetSpec::new("disk.img", None), command).with_confirmation(true)
}

fn body(target: Fingerprint) -> RecordBody {
    RecordBody {
        target,
        legacy_generation: 0,
        plan_digest: [0x11; 32],
    }
}

fn preimage_frame(seq: u32, data: &[u8]) -> Frame {
    Frame {
        seq,
        kind: FrameKind::Preimage,
        completed: true,
        irreversible: false,
        intent: Vec::new(),
        regions: vec![Preimage {
            offset: 512,
            len: data.len() as u64,
            digest: [0x33; 16],
            data: data.to_vec(),
            after: Vec::new(),
        }],
    }
}

fn intent_frame(seq: u32) -> Frame {
    Frame {
        seq,
        kind: FrameKind::Intent,
        completed: false,
        irreversible: false,
        intent: b"write-table".to_vec(),
        regions: Vec::new(),
    }
}

fn undo_decision(snapshot: &RecoverSnapshot) -> Decision {
    undo::decide(snapshot, &request(Command::Undo))
}

fn release_decision(snapshot: &RecoverSnapshot, discard_replay: bool) -> Decision {
    release::decide(snapshot, &request(Command::Release { discard_replay }))
}

#[test]
fn undo_does_not_turn_an_unreadable_record_into_no_record() {
    let decision = undo_decision(&snapshot(
        None,
        Some("cannot read state/images/x/record: permission denied"),
    ));
    match decision {
        Decision::Refuse(Refusal::RecordUnreadable { path }) => {
            assert_eq!(path, "state/images/x/record");
        }
        other @ (Decision::Refuse(_) | Decision::Report { .. } | Decision::Execute { .. }) => {
            panic!("an unreadable record must be refused as unreadable, got {other:?}")
        }
        Decision::Unchanged { note } => {
            panic!("an unreadable record must not be reported as no record: {note}")
        }
    }
}

#[test]
fn release_does_not_turn_an_unreadable_record_into_nothing_to_release() {
    let decision = release_decision(
        &snapshot(
            None,
            Some("cannot read state/images/x/record: permission denied"),
        ),
        true,
    );
    let Decision::Refuse(Refusal::RecordUnreadable { path }) = decision else {
        panic!("an unreadable record must be refused as unreadable, got {decision:?}");
    };
    assert_eq!(path, "state/images/x/record");
}

#[test]
fn a_confirmed_absent_record_is_still_nothing_to_release() {
    let decision = release_decision(&snapshot(None, None), true);
    let Decision::Unchanged { note } = decision else {
        panic!("a confirmed absent record is not a release, got {decision:?}");
    };
    assert!(note.contains("nothing to release"), "{note}");
}

#[test]
fn undo_refuses_a_record_that_belongs_to_another_target() {
    let bytes = encode(&body(OTHER), &[intent_frame(0)]).expect("记录可编码");
    let decision = undo_decision(&snapshot(Some(bytes), None));
    assert!(
        matches!(decision, Decision::Refuse(_)),
        "a record for another target must be refused, got {decision:?}"
    );
}

#[test]
fn release_refuses_a_record_that_belongs_to_another_target() {
    let bytes = encode(&body(OTHER), &[intent_frame(0)]).expect("记录可编码");
    let decision = release_decision(&snapshot(Some(bytes), None), true);
    let Decision::Refuse(Refusal::RecordForeign { why }) = decision else {
        panic!("a record for another target must be refused, got {decision:?}");
    };
    assert!(why.contains("different target"), "{why}");
}

#[test]
fn release_refuses_a_record_whose_layout_cannot_be_read() {
    // 头部不足一个标识：既不能确认是本实现的布局，也读不出目标身份。
    let truncated = encode(&body(TARGET), &[]).expect("记录可编码")[..8].to_vec();
    let decision = release_decision(&snapshot(Some(truncated), None), true);
    assert!(
        matches!(decision, Decision::Refuse(Refusal::RecordDamaged { .. })),
        "a record that cannot be placed must be refused, got {decision:?}"
    );
}

#[test]
fn release_refuses_an_empty_record_file() {
    let decision = release_decision(&snapshot(Some(Vec::new()), None), true);
    assert!(
        matches!(decision, Decision::Refuse(Refusal::RecordDamaged { .. })),
        "an empty record file is not an absent record, got {decision:?}"
    );
}

#[test]
fn release_honours_the_replayable_boundary() {
    let bytes = encode(
        &body(TARGET),
        &[intent_frame(0), preimage_frame(1, &[0xAA; 4])],
    )
    .expect("记录可编码");
    let decision = release_decision(&snapshot(Some(bytes.clone()), None), false);
    let Decision::Refuse(Refusal::RecoveryRefused { why }) = decision else {
        panic!("an unfinished record with replayable ranges must be refused, got {decision:?}");
    };
    assert!(why.contains("discard-replay"), "{why}");
    let decision = release_decision(&snapshot(Some(bytes), None), true);
    assert!(
        matches!(decision, Decision::Execute { .. }),
        "--discard-replay must allow the release, got {decision:?}"
    );
}

#[test]
fn release_requires_discard_when_the_ranges_cannot_be_enumerated() {
    // 帧序号断档：载荷前缀可读、身份已核对，但能回放的区间数无从枚举。
    let bytes = encode(&body(TARGET), &[preimage_frame(1, &[0xAA; 4])]).expect("记录可编码");
    let decision = release_decision(&snapshot(Some(bytes.clone()), None), false);
    let Decision::Refuse(Refusal::RecoveryRefused { why }) = decision else {
        panic!("an unenumerable record must not be released silently, got {decision:?}");
    };
    assert!(why.contains("discard-replay"), "{why}");
    let decision = release_decision(&snapshot(Some(bytes), None), true);
    assert!(
        matches!(decision, Decision::Execute { .. }),
        "--discard-replay must allow the release, got {decision:?}"
    );
}

#[test]
fn a_foreign_layout_is_released_only_as_an_explicit_abandon() {
    // 标识不是本实现的：布局可确认，其载荷不被读取，显式放弃仍可释放。
    let mut bytes = encode(&body(TARGET), &[intent_frame(0)]).expect("记录可编码");
    bytes[0] = b'Z';
    let decision = release_decision(&snapshot(Some(bytes), None), false);
    assert!(
        matches!(decision, Decision::Execute { .. }),
        "a confirmable foreign layout may be released, got {decision:?}"
    );
}

#[cfg(target_os = "linux")]
mod cli {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use diskedit_core::fingerprint::Fingerprint;
    use diskedit_core::record::{Frame, FrameKind, Preimage, RecordBody, encode};
    use sha2::{Digest, Sha256};

    use super::OTHER;

    fn workspace(name: &str) -> PathBuf {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/fixtures/release-gates")
            .join(name);
        drop(std::fs::remove_dir_all(&base));
        std::fs::create_dir_all(&base).expect("夹具目录可建");
        base
    }

    fn image(root: &Path) -> PathBuf {
        let path = root.join("disk.img");
        std::fs::write(&path, vec![0u8; 4 * 1024 * 1024]).expect("镜像可写");
        path
    }

    /// 与 Linux 后端同一算式：`file\0` + dev + ino + len 的 SHA-256。
    fn fingerprint(image: &Path) -> Fingerprint {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(image).expect("镜像可读");
        let mut hasher = Sha256::new();
        hasher.update(b"file\0");
        hasher.update(metadata.dev().to_le_bytes());
        hasher.update(metadata.ino().to_le_bytes());
        hasher.update(metadata.len().to_le_bytes());
        Fingerprint::strong(hasher.finalize().into())
    }

    fn record_dir(state: &Path, image: &Path) -> PathBuf {
        let name: String = fingerprint(image).bytes()[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        state.join("diskedit").join("images").join(name)
    }

    fn body(target: Fingerprint) -> RecordBody {
        RecordBody {
            target,
            legacy_generation: 0,
            plan_digest: [0x11; 32],
        }
    }

    fn preimage_frame(seq: u32, data: &[u8]) -> Frame {
        Frame {
            seq,
            kind: FrameKind::Preimage,
            completed: true,
            irreversible: false,
            intent: Vec::new(),
            regions: vec![Preimage {
                offset: 512,
                len: data.len() as u64,
                digest: [0x33; 16],
                data: data.to_vec(),
                after: Vec::new(),
            }],
        }
    }

    fn intent_frame(seq: u32) -> Frame {
        Frame {
            seq,
            kind: FrameKind::Intent,
            completed: false,
            irreversible: false,
            intent: b"write-table".to_vec(),
            regions: Vec::new(),
        }
    }

    fn run(state: &Path, args: &[&str]) -> (i32, String, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_diskedit"))
            .args(args)
            .env("XDG_STATE_HOME", state)
            .output()
            .expect("命令可运行");
        (
            output.status.code().expect("子进程应正常退出"),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn abandoned(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(directory)
            .expect("记录目录可读")
            .map(|entry| {
                entry
                    .expect("目录项可读")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.ends_with(".abandoned"))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn undo_refuses_a_record_for_another_target() {
        let root = workspace("undo-foreign");
        let image = image(&root);
        let state = root.join("state");
        let directory = record_dir(&state, &image);
        std::fs::create_dir_all(&directory).expect("记录目录可建");
        let record = directory.join("record");
        let bytes = encode(&body(OTHER), &[intent_frame(0)]).expect("记录可编码");
        std::fs::write(&record, &bytes).expect("记录可写");

        let target = image.to_str().expect("路径可转字符串");
        let (code, _stdout, stderr) = run(&state, &["undo", target, "--yes"]);
        assert_eq!(code, 3, "归属不符的记录必须拒绝：{stderr}");
        assert!(stderr.contains("different"), "{stderr}");
        assert_eq!(
            std::fs::read(&record).expect("记录仍在"),
            bytes,
            "拒绝时不得改动记录"
        );
    }

    #[test]
    fn undo_and_release_refuse_an_unreadable_record_location() {
        let root = workspace("unreadable-location");
        let image = image(&root);
        let state = root.join("state");
        let directory = record_dir(&state, &image);
        // 记录位置被目录占住：能定位，但读不出字节。
        std::fs::create_dir_all(directory.join("record")).expect("占位目录可建");
        let target = image.to_str().expect("路径可转字符串");

        let (code, stdout, stderr) = run(&state, &["undo", target, "--yes"]);
        assert_eq!(code, 3, "不可读的记录必须拒绝撤销：{stderr}");
        assert!(stderr.contains("cannot be decoded"), "{stderr}");
        assert!(!stderr.contains("no recovery record"), "{stderr}");
        assert!(!stdout.contains("nothing to undo"), "{stdout}");

        let (code, stdout, stderr) = run(&state, &["release", target, "--yes"]);
        assert_eq!(code, 3, "不可读的记录必须拒绝释放：{stderr}");
        assert!(stderr.contains("cannot be decoded"), "{stderr}");
        assert!(!stderr.contains("nothing to release"), "{stderr}");
        assert!(!stdout.contains("nothing to release"), "{stdout}");
        assert!(abandoned(&directory).is_empty(), "拒绝时不得归档记录");
    }

    #[test]
    fn release_refuses_a_record_that_cannot_be_placed() {
        let root = workspace("truncated-record");
        let image = image(&root);
        let state = root.join("state");
        let directory = record_dir(&state, &image);
        std::fs::create_dir_all(&directory).expect("记录目录可建");
        let valid = encode(&body(fingerprint(&image)), &[]).expect("记录可编码");
        std::fs::write(directory.join("record"), &valid[..8]).expect("截断记录可写");
        let target = image.to_str().expect("路径可转字符串");

        let (code, _stdout, stderr) = run(&state, &["release", target, "--yes"]);
        assert_eq!(code, 3, "读不出布局的记录必须拒绝释放：{stderr}");
        assert!(stderr.contains("cannot be read"), "{stderr}");
        assert!(abandoned(&directory).is_empty(), "拒绝时不得归档记录");
    }

    #[test]
    fn release_honours_the_replayable_boundary() {
        let root = workspace("replayable-boundary");
        let image = image(&root);
        let state = root.join("state");
        let directory = record_dir(&state, &image);
        std::fs::create_dir_all(&directory).expect("记录目录可建");
        let record = directory.join("record");
        let bytes = encode(
            &body(fingerprint(&image)),
            &[intent_frame(0), preimage_frame(1, &[0xAA; 4])],
        )
        .expect("记录可编码");
        std::fs::write(&record, &bytes).expect("记录可写");
        let target = image.to_str().expect("路径可转字符串");

        let (code, _stdout, stderr) = run(&state, &["release", target, "--yes"]);
        assert_eq!(code, 3, "还有可回放区间时必须拒绝：{stderr}");
        assert!(stderr.contains("discard-replay"), "{stderr}");
        assert_eq!(
            std::fs::read(&record).expect("记录仍在"),
            bytes,
            "拒绝时不得改动记录"
        );

        let (code, _stdout, stderr) =
            run(&state, &["release", target, "--yes", "--discard-replay"]);
        assert_eq!(code, 0, "显式放弃后应能释放：{stderr}");
        assert!(!record.exists(), "释放后活动记录应消失");
        assert_eq!(
            abandoned(&directory),
            vec!["record.1.abandoned".to_owned()],
            "释放应把记录移入隔离区"
        );
    }
}
