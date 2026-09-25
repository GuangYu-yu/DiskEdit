//! 目标独占所有权：把"我独占了这块目标"抽成一层，业务层只看到所有权，看不到底层机制。
//!
//! 两类目标的凭据来源不同，对外是同一个概念：
//!
//! - **镜像**：独立的锁文件（镜像路径加 [`crate::dev::LOCK_SUFFIX`]），非阻塞独占锁
//!   （`std::fs::File::try_lock`：Unix 上是 flock，Windows 上是 LockFileEx）。锁文件
//!   **不随释放删除**——删它有竞态：另一个进程可能刚取到同一把锁，而删掉的正是它正持有的
//!   那个名字。判据是"锁取不取得到"，不是"文件在不在"，因此残留的空锁文件不构成任何阻挡
//! - **块设备**：锁文件放在 `state_dir()` 下，与 journal / checkpoint 同一身份键派生
//!   （见 [`crate::dev`]），因此三者的路径规则只有一套、没有第二个 lock 目录概念。
//!   在锁之外，块设备的写打开仍是 `O_EXCL`（见 [`crate::dev::FileSource::open`] 的
//!   块设备分支）——两道凭据互为补充：O_EXCL 挡住别的持 fd 写者，锁文件覆盖不经过
//!   O_EXCL 的写入路径（如 `abandon`）与在线路径。同一进程二次以 `O_EXCL` 打开同一
//!   设备是否 EBUSY 取决于内核的 holder 语义（holder 是每次 open 的 `struct file`），
//!   不值得把正确性押在那上面
//!
//! 锁是 **advisory** 的：它只在**本工具**的各次调用之间互斥，不阻止别的程序写同一块盘。
//! 这正是把它藏在类型后面的理由——业务代码不该依赖 flock / LockFileEx 的平台细节

use std::fs::{File, OpenOptions};

use crate::dev::TargetIdentity;
use crate::outcome::Fail;

/// 持有期间目标归本进程独占；析构即释放。
///
/// 独占权由锁文件承载（两类目标同一机制，见模块注释），本类型不区分载体细节
pub(crate) struct TargetLock {
    /// 锁的载体。本字段**不被读取**，存在的意义就是被持有：句柄析构即释放锁
    #[allow(dead_code)]
    file: File,
}

impl TargetLock {
    /// 取目标的独占所有权：在锁落点上取非阻塞独占锁。调用方随后**必须**按自己的
    /// 路径打开目标（块设备的写打开是 `O_EXCL`，见模块注释）。
    ///
    /// 只有两种结局：拿到锁，或返回错误。**没有"降级为不持锁"这条路径**——锁落点由
    /// 身份恒给出（见 [`crate::dev::TargetIdentity::lock_path`]），建立失败即拒绝
    pub(crate) fn acquire(identity: &TargetIdentity) -> Result<Self, Fail> {
        let path = identity.lock_path();
        // 落点目录可能尚不存在：块设备的锁在 state_dir 下，而 state_dir 来自配置，
        // 任何一次调用都可能是首次。尽力创建即可——真正的失败在下面打开时以完整路径暴露
        if let Some(dir) = path.parent() {
            crate::dev::best_effort_mkdir(dir);
        }
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
            Ok(()) => Ok(TargetLock { file }),
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
        TargetIdentity::resolve_image(path)
    }

    fn temp_image(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("diskedit_lk_{tag}_{}", std::process::id()));
        std::fs::write(&p, b"x").unwrap();
        p
    }

    fn cleanup(img: &std::path::Path) {
        let _ = std::fs::remove_file(img);
        let _ = std::fs::remove_file(identity_for(img).lock_path());
    }

    /// 同一目标二次取锁必须失败——这是"一个目标一把锁"的最小证据。
    /// 释放后必须能再取：判据是**锁本身**，不是锁文件在不在，所以残留的空文件的
    /// 不构成阻挡（它一定会残留，因为删除它有竞态）
    #[test]
    fn second_lock_on_the_same_image_is_refused() {
        let img = temp_image("excl");
        let lock_path = identity_for(&img).lock_path().to_path_buf();
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

    /// 每个身份都有锁落点，且两侧落在各自的命名空间：镜像在目标旁（与用户给的路径
    /// 一一对应），块设备在 `state_dir()` 下与 journal / checkpoint 同目录。
    /// 这条不变量是"不存在无锁路径"的基础——落点恒有，取不到就拒绝
    #[test]
    fn every_identity_has_a_lock_path() {
        let img = std::path::Path::new("/tmp/diskedit_lk_path.img");
        assert_eq!(
            identity_for(img).lock_path(),
            crate::dev::suffix_path(img, crate::dev::LOCK_SUFFIX)
        );

        // 块设备侧命名断言只在非 Linux 跑：Linux 上伪造设备名的拓扑解析不出（fail-closed），
        // 身份构造会拒绝；Linux 侧的对应证据是"拒绝且带设备名"（dev 模块）
        #[cfg(not(target_os = "linux"))]
        {
            let bd = TargetIdentity::resolve_block(std::path::Path::new("/dev/nonexistent")).unwrap();
            let lock = bd.lock_path();
            assert_eq!(
                lock.parent(),
                Some(crate::dev::state_dir().as_path()),
                "a block device's lock must live in state_dir next to its journal"
            );
            assert!(
                lock.file_name().is_some_and(|n| n.to_string_lossy().ends_with(crate::dev::LOCK_SUFFIX)),
                "the lock name must be recognisable: {}",
                lock.display()
            );
            // 同一 state_dir 内，两个**不同**设备的锁名必须不同——这才是"不碰撞"的判据
            // （与镜像路径比较是恒真的：两者父目录本就不同，证明不了任何事）
            let other = TargetIdentity::resolve_block(std::path::Path::new("/dev/other")).unwrap();
            assert_ne!(lock, other.lock_path(), "two devices must not share one lock name");
        }
    }
}