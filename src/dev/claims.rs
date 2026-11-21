//! 载体占据声明：`<state_root>/claims/<键摘要>` 记着"这个目标上次用哪个显式身份打开"。
//!
//! 声明的名字是用户给的断言，不是设备事实。身份不可得的块设备没法按名字反查设备，
//! 因此"换个名字会不会开出第二份现场"只能靠这份记录回答：记录键本身由名字派生，
//! 换名字得到的是另一个空目录，从那里看不见旧现场。
//!
//! 声明与它所保护的那份现场生命周期一致：现场还在，声明就必须被同一名字续上；
//! 现场已被 `recover --release` 收走，旧声明即为残留，下一次写命令用新名字覆盖它。
//! 读取与写入都在 [`ClaimFile`] 的锁内完成；锁由 `targetlock` 在整次写入期间持有，
//! 两个不同名字不可能同时通过准入。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::outcome::RunOutcome;

/// 一台载体上的一份占据声明。新建时按 `0600` 创建、不跟随符号链接；既有文件不再
/// 收紧权限，前提是 `claims` 目录仅本用户可写。空文件表示没有声明
pub(crate) struct ClaimFile {
    file: File,
}

impl ClaimFile {
    /// 打开并锁定声明文件。与锁文件同一套取向：文件不存在就创建，取不到锁即拒绝
    pub(crate) fn open_locked(path: &Path) -> Result<Self, RunOutcome> {
        let file = crate::targetlock::open_lock_file(path)?;
        let mut claim = Self { file };
        // 上一位持有者可能留下更长的名字：先读到末尾，写入时整体覆盖
        claim.read()?;
        Ok(claim)
    }

    /// 读当前声明；空文件即没有
    pub(crate) fn read(&mut self) -> Result<Option<String>, RunOutcome> {
        self.file.seek(SeekFrom::Start(0)).map_err(claim_io)?;
        let mut text = String::new();
        self.file.read_to_string(&mut text).map_err(claim_io)?;
        let name = text.trim_end_matches('\n');
        Ok((!name.is_empty()).then(|| name.to_string()))
    }

    /// 用新名字覆盖声明
    pub(crate) fn write(&mut self, name: &str) -> Result<(), RunOutcome> {
        self.file.set_len(0).map_err(claim_io)?;
        self.file.seek(SeekFrom::Start(0)).map_err(claim_io)?;
        self.file.write_all(name.as_bytes()).map_err(claim_io)?;
        self.file.write_all(b"\n").map_err(claim_io)?;
        self.file.sync_data().map_err(claim_io)
    }
}

fn claim_io(e: std::io::Error) -> RunOutcome {
    RunOutcome::refused(format!("cannot use the carrier claim of this target: {e}"))
}

/// 准入：声明里的名字与这次给的名字必须是同一个；记录已经不在时旧声明作废。
///
/// `record_exists` 回答"那个名字下的现场是否还在"。名字相同、现场还在，才允许续跑；
/// 名字不同而现场还在，一律拒绝——这正是"同一个名字才能续跑"的落点。
pub(crate) fn admit(
    claim: &mut ClaimFile,
    declared: Option<&str>,
    record_exists: impl Fn(&str) -> std::io::Result<bool>,
) -> Result<(), RunOutcome> {
    let Some(held) = claim.read()? else {
        if let Some(name) = declared {
            claim.write(name)?;
        }
        return Ok(());
    };
    // "现场还在"这个问题必须是三态：读不出目录不等于没有现场。把它折成 false 会
    // 顺手改写声明并在别的名字下准入——正好放行本该被拒绝的那一次运行
    let live = record_exists(&held).map_err(|e| {
        RunOutcome::no_mutation(format!(
            "cannot tell whether the record opened as --identity {held} is still present: {e} — \
             refusing to admit another scene over it"
        ))
    })?;
    if !live {
        if let Some(name) = declared {
            claim.write(name)?;
        }
        return Ok(());
    }
    match declared {
        Some(name) if name == held => Ok(()),
        Some(name) => Err(RunOutcome::refused(format!(
            "this target was last opened as --identity {held}, and that recovery record is still \
             present; refusing to open a second scene as --identity {name}. Pass --identity {held} \
             to resume it, or run `diskedit recover --release --identity {held}` to release it"
        ))),
        None => Err(RunOutcome::refused(format!(
            "this target has a recovery record opened as --identity {held}; pass --identity {held} \
             to resume it, or run `diskedit recover --release --identity {held}` to release it"
        ))),
    }
}

/// 测试用：不开锁的声明文件（临时目录里的普通文件）
#[cfg(test)]
pub(crate) fn test_claim(path: &Path) -> ClaimFile {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .expect("create the test claim file");
    ClaimFile { file }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn refusal(err: RunOutcome) -> String {
        match err {
            RunOutcome::Refused(message) => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("diskedit-claim-{name}-{}", std::process::id()));
        // 上一次运行可能留下同名目录；清掉再建，避免读到别人的声明
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("create the claim scratch directory");
        dir.join("claim")
    }

    #[test]
    fn an_empty_claim_admits_a_named_run_and_remembers_it() {
        let path = scratch("empty");
        let mut claim = test_claim(&path);
        admit(&mut claim, Some("export-a"), |_| Ok(false)).expect("an empty claim admits");
        let mut claim = test_claim(&path);
        assert_eq!(claim.read().unwrap().as_deref(), Some("export-a"));
    }

    #[test]
    fn the_same_name_resumes_while_its_record_is_present() {
        let path = scratch("same");
        let mut claim = test_claim(&path);
        claim.write("export-a").unwrap();
        admit(&mut claim, Some("export-a"), |name| Ok(name == "export-a")).expect("the same name resumes");
    }

    #[test]
    fn a_different_name_is_refused_while_the_record_is_present() {
        let path = scratch("different");
        let mut claim = test_claim(&path);
        claim.write("export-a").unwrap();
        let err = refusal(admit(&mut claim, Some("export-b"), |_| Ok(true)).expect_err("a different name is refused"));
        assert!(err.contains("export-a"), "{err}");
        assert!(err.contains("export-b"), "{err}");
    }

    #[test]
    fn a_missing_name_is_refused_while_the_record_is_present() {
        let path = scratch("missing");
        let mut claim = test_claim(&path);
        claim.write("export-a").unwrap();
        let err = refusal(admit(&mut claim, None, |_| Ok(true)).expect_err("a missing name is refused"));
        assert!(err.contains("--identity export-a"), "{err}");
    }

    #[test]
    fn a_stale_claim_yields_to_the_next_name() {
        let path = scratch("stale");
        let mut claim = test_claim(&path);
        claim.write("export-a").unwrap();
        admit(&mut claim, Some("export-b"), |_| Ok(false)).expect("a stale claim does not block");
        let mut claim = test_claim(&path);
        assert_eq!(claim.read().unwrap().as_deref(), Some("export-b"));
    }
}
