//! 目标独占所有权：把"我独占了这块目标"抽成一层，业务层只看到所有权，看不到底层机制。
//!
//! 两类目标的凭据来源不同，对外是同一个概念：
//!
//! - **镜像**：独立的锁文件 `<镜像路径>.diskedit.lock`，非阻塞独占锁
//!   （`std::fs::File::try_lock`：Unix 上是 flock，Windows 上是 LockFileEx）。锁文件
//!   **不随释放删除**——删它有竞态：另一个进程可能刚取到同一把锁，而删掉的正是它正持有的
//!   那个名字。判据是"锁取不取得到"，不是"文件在不在"，因此残留的空锁文件不构成任何阻挡
//! - **块设备**：不叠加第二把锁。内核的 `O_EXCL` claim 本身就是独占凭据（见
//!   [`crate::dev::FileSource::open`] 的块设备分支），本类型只**登记**"调用方已持有"，
//!   不自己再开一个 fd——同一进程二次以 `O_EXCL` 打开同一设备是否 EBUSY 取决于内核的
//!   holder 语义（holder 是每次 open 的 `struct file`），不值得把正确性押在那上面
//!
//! 锁是 **advisory** 的：它只在**本工具**的各次调用之间互斥，不阻止别的程序写同一块盘。
//! 这正是把它藏在类型后面的理由——业务代码不该依赖 flock / LockFileEx 的平台细节

use std::fs::{File, OpenOptions};

use crate::dev::TargetIdentity;
use crate::outcome::Fail;

/// 持有期间目标归本进程独占；析构即释放。
///
/// 块设备上它是空值见证——独占权在调用方那条 `O_EXCL` 打开的 fd 上（见模块注释），
/// 本类型只表达"这次操作是在持有独占权的前提下进行的"这个事实
pub(crate) struct TargetLock {
    /// 锁的载体。本字段**不被读取**，存在的意义就是被持有：句柄析构即释放锁
    #[allow(dead_code)]
    file: Option<File>,
}

impl TargetLock {
    /// 取目标的独占所有权。
    ///
    /// 调用方**必须已经**用 [`crate::dev::FileSource::open`] 打开过目标：块设备的独占权
    /// 就是那次 `O_EXCL` 打开产生的，本函数对块设备只做登记，不会替它去 claim。
    /// 镜像则在此时取锁文件上的非阻塞独占锁
    pub(crate) fn acquire(identity: &TargetIdentity) -> Result<Self, Fail> {
        let Some(path) = identity.lock_path() else {
            return Ok(TargetLock { file: None });
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // 锁文件的内容自始至终无关紧要，有意义的只是它上面的锁；截断它既无必要，
            // 也会在"别人正持锁"时多写一次盘
            .truncate(false)
            .open(path)
            .map_err(|e| Fail::infra(format!("cannot open the target lock {}: {e}", path.display())))?;
        match file.try_lock() {
            Ok(()) => Ok(TargetLock { file: Some(file) }),
            // 另一个进程正持有 ⇒ 环境条件，不是请求与现状不匹配：换参数没用，等它做完即可。
            // 与块设备那条 O_EXCL 打开失败的归类一致（都是 30）
            Err(std::fs::TryLockError::WouldBlock) => Err(Fail::infra(format!(
                "another diskedit is working on this target (the lock {} is held); \
                 this tool serializes writers per target — wait for it to finish and retry",
                path.display()
            ))),
            Err(std::fs::TryLockError::Error(e)) => Err(Fail::infra(format!(
                "cannot take the target lock {}: {e}",
                path.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::let_underscore_must_use)] // 清理临时文件有意忽略失败
    use super::*;
    use std::path::PathBuf;

    fn identity_for(path: &std::path::Path) -> TargetIdentity {
        TargetIdentity::resolve(path, false, 0)
    }

    fn temp_image(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("diskedit_lk_{tag}_{}", std::process::id()));
        std::fs::write(&p, b"x").unwrap();
        p
    }

    fn cleanup(img: &std::path::Path) {
        let _ = std::fs::remove_file(img);
        if let Some(l) = identity_for(img).lock_path() {
            let _ = std::fs::remove_file(l);
        }
    }

    /// 同一目标二次取锁必须失败——这是"一个目标一把锁"的最小证据。
    /// 释放后必须能再取：判据是**锁本身**，不是锁文件在不在，所以残留的空文件的
    /// 不构成阻挡（它一定会残留，因为删除它有竞态）
    #[test]
    fn second_lock_on_the_same_image_is_refused() {
        let img = temp_image("excl");
        let lock_path = identity_for(&img).lock_path().unwrap().to_path_buf();
        let _ = std::fs::remove_file(&lock_path);

        let first = TargetLock::acquire(&identity_for(&img)).expect("the first holder must succeed");
        assert!(lock_path.exists(), "the lock file must be created next to the image");
        assert!(
            TargetLock::acquire(&identity_for(&img)).is_err(),
            "a second holder must not get the same target"
        );

        drop(first);
        assert!(lock_path.exists(), "releasing must not delete the lock file");
        TargetLock::acquire(&identity_for(&img)).expect("a released lock must be re-acquirable");

        cleanup(&img);
    }

    /// 块设备没有锁文件：它的独占权是打开设备时的 O_EXCL，本类型只登记，
    /// 因此不能凭空造出一个落点来
    #[test]
    fn block_devices_have_no_lock_file() {
        let ident = TargetIdentity::resolve(std::path::Path::new("/dev/nonexistent"), true, 1024);
        assert!(ident.lock_path().is_none(), "a block device must not get a lock file");
    }
}