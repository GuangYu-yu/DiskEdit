//! 跨 crate 的不变量证据：从公开 API 再断言一次。
//!
//! 各模块自身已有单位用例；这里从内核 crate 之外复核一次，让这些不变量也
//! 出现在 `tests/` 下，而不是只贴在 core 内部。

use diskedit_core::evidence::Evidence;
use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::{ContentKind, FsKind, profile};
use diskedit_core::record::{
    Frame, FrameKind, RecordBody, RecoveryExpectation, RecoveryPlan, encode, recover,
};

#[test]
fn every_content_kind_has_exactly_one_catalog_row() {
    // 目录表按内容类别逐行给出能力档案：每个类别恰好一行。
    let rows: Vec<&str> = ContentKind::ALL.iter().map(|kind| kind.as_str()).collect();
    let mut unique = rows.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        rows.len(),
        "a content kind has more than one catalogue row"
    );

    // 行的全集必须恰好是各文件系统族加上非文件系统类别，既不缺也不多。
    let mut expected: Vec<&str> = FsKind::ALL
        .iter()
        .map(|kind| ContentKind::Filesystem(*kind).as_str())
        .chain(
            [
                ContentKind::Swap,
                ContentKind::Lvm2Pv,
                ContentKind::Luks,
                ContentKind::BitLocker,
                ContentKind::Unknown,
            ]
            .into_iter()
            .map(|kind| kind.as_str()),
        )
        .collect();
    expected.sort_unstable();
    expected.dedup();
    let mut rows = rows;
    rows.sort_unstable();
    assert_eq!(
        rows, expected,
        "the catalogue rows do not cover every content kind exactly once"
    );

    for kind in ContentKind::ALL {
        assert_eq!(ContentKind::from_name(kind.as_str()), Some(kind));
        // 每行都给出五列能力。
        let row = profile(kind);
        let _ = (row.grow, row.shrink, row.attrs, row.check, row.mkfs);
    }
}

#[test]
fn unknown_format_is_not_the_same_as_absence() {
    let unknown: Evidence<()> = Evidence::UnknownFormat {
        why: "content matches no known signature".to_owned(),
    };
    assert!(!unknown.is_absent());
    assert!(unknown.is_unknown_format());
    let absent: Evidence<()> = Evidence::Absent;
    assert!(absent.is_absent());
    assert!(!absent.is_unknown_format());
}

#[test]
fn every_record_prefix_has_one_deterministic_recovery_plan() {
    let target = Fingerprint::strong([0x42; 32]);
    let total_bytes = 4 * 1024 * 1024;
    let body = RecordBody {
        target,
        length_generation: diskedit_core::record::length_generation(total_bytes),
        plan_digest: [0x11; 32],
    };
    let frames = [
        Frame {
            seq: 0,
            kind: FrameKind::Preimage,
            completed: false,
            irreversible: false,
            intent: b"step".to_vec(),
            regions: Vec::new(),
        },
        Frame {
            seq: 1,
            kind: FrameKind::Committed,
            completed: true,
            irreversible: false,
            intent: b"step".to_vec(),
            regions: Vec::new(),
        },
    ];
    let bytes = encode(&body, &frames).expect("记录编码应成立");
    let expectation = RecoveryExpectation::new(target, total_bytes);
    for cut in 0..=bytes.len() {
        let first = recover(&bytes[..cut], expectation);
        let second = recover(&bytes[..cut], expectation);
        assert_eq!(first, second, "前缀 {cut} 的结论必须确定");
    }
    assert_eq!(recover(&bytes, expectation), RecoveryPlan::Committed);
}
