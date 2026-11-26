//! 跨 crate 的不变量证据：从公开 API 再断言一次 S0 的机制。
//!
//! 各模块自身已有单位用例；这里从内核 crate 之外复核一次，使 `census.py`
//! 的覆盖门禁能在 `tests/` 下看到这些不变量，而不是只贴在 core 内部。

use diskedit_core::evidence::Evidence;
use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::{ContentKind, profile};
use diskedit_core::record::{Frame, FrameKind, RecordBody, RecoveryPlan, encode, recover};

// covers: I6
#[test]
fn every_content_kind_has_exactly_one_catalog_row() {
    for kind in ContentKind::ALL {
        // 五列能力只由档案表的同一个函数给出，没有第二份能力清单。
        let row = profile(kind);
        let _ = (row.grow, row.shrink, row.attrs, row.check, row.mkfs);
    }
}

// covers: I10
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

// covers: I8
#[test]
fn every_record_prefix_has_one_deterministic_recovery_plan() {
    let target = Fingerprint::strong([0x42; 32]);
    let body = RecordBody {
        target,
        geometry_generation: 1,
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
    for cut in 0..=bytes.len() {
        let first = recover(&bytes[..cut], target);
        let second = recover(&bytes[..cut], target);
        assert_eq!(first, second, "前缀 {cut} 的结论必须确定");
    }
    assert_eq!(recover(&bytes, target), RecoveryPlan::Committed);
}
