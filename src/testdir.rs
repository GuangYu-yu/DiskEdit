//! 测试用临时目录：默认在最后一个持有者析构时连同内容一起删除；
//! 设置 `DISKEDIT_KEEP_TEST_DIR=1` 时保留现场（路径打印到 stderr）便于事后查看。
//!
//! 名字由 `tempfile` 随机生成（`diskedit_<name>_<随机>`），同一用例重复运行、
//! 多个测试并行都不会撞名，也不再需要把 pid 拼进名字。

use std::path::{Path, PathBuf};

pub(crate) struct TestDir {
    /// `None`：按环境变量要求保留现场，不在析构时删除
    inner: Option<tempfile::TempDir>,
    path: PathBuf,
}

impl TestDir {
    pub(crate) fn new(name: &str) -> Self {
        let mut dir = tempfile::Builder::new()
            .prefix(&format!("diskedit_{name}_"))
            .tempdir()
            .unwrap_or_else(|e| panic!("cannot create the test directory for {name}: {e}"));
        let path = dir.path().to_path_buf();
        let keep = std::env::var_os("DISKEDIT_KEEP_TEST_DIR").is_some_and(|v| v == "1");
        if keep {
            // 只有显式要求保留现场时才关掉自动删除
            dir.disable_cleanup(true);
            eprintln!("DISKEDIT_KEEP_TEST_DIR=1: keeping {}", path.display());
        }
        Self { inner: (!keep).then_some(dir), path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// 目录内的子路径：被测对象可以把它写成一个尚不存在的名字
    pub(crate) fn join(&self, name: impl AsRef<Path>) -> PathBuf {
        self.path.join(name)
    }

    /// 需要确认清理成功时用（`TempDir::drop` 吞掉删除错误）：返回删除结果
    pub(crate) fn close(&mut self) -> std::io::Result<()> {
        match self.inner.take() {
            Some(dir) => {
                let path = dir.path().to_path_buf();
                let result = remove_all(&path);
                drop(dir);
                result
            }
            None => Ok(()),
        }
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if let Some(dir) = self.inner.take() {
            let path = dir.path().to_path_buf();
            best_effort_remove_all(&path);
            drop(dir);
        }
    }
}

/// 析构里清理失败无处上报，也不能 panic：这是全仓唯一允许丢弃删除结果的地方
#[allow(clippy::let_underscore_must_use)] // 有意忽略：Drop 没有返回通道
fn best_effort_remove_all(path: &Path) {
    let _ = remove_all(path);
}

/// 删除目录，`NotFound` 视为已删除。
///
/// 本机 Windows 上 `std::fs::remove_dir_all` 对含子目录的树首次调用返回 145
/// （目录非空），紧接着重试即可清空；句柄未关闭时重试也不会成功，错误原样返回。
/// `TempDir::drop` 吞掉这个错误，故删除由本函数完成并重试
fn remove_all(path: &Path) -> std::io::Result<()> {
    let mut last = Ok(());
    for _ in 0..3 {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => last = Err(e),
        }
    }
    last
}

impl std::ops::Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

pub(crate) fn test_dir(name: &str) -> TestDir {
    TestDir::new(name)
}

/// 把"文件放在这个目录里"的所有者与"对文件的句柄"绑成一个值。
///
/// 字段顺序是删除能否成功的前提：`value`（可能的文件句柄 / 映射）先声明，析构时先释放，
/// 目录随后才删。Windows 上句柄未关时目录删不掉，而 `TempDir::drop` 会吞掉这个错误。
pub(crate) struct TestSource<T> {
    value: T,
    dir: TestDir,
}

impl<T> TestSource<T> {
    pub(crate) fn new(dir: TestDir, value: T) -> Self {
        Self { value, dir }
    }

    /// 释放被测对象（关闭它持有的句柄 / 映射），目录继续由本值持有。
    /// 需要先放下句柄、稍后再按路径重新打开同一文件的用例用这个，不要用
    /// `drop(src)`——那会把目录一起删掉
    pub(crate) fn release(self) -> TestDir {
        let Self { value, dir } = self;
        drop(value);
        dir
    }
}

impl<T> std::ops::Deref for TestSource<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> std::ops::DerefMut for TestSource<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TempDir::drop` 吞掉删除错误，清理是否真的发生只能由 `close()` 回答。
    /// 树里放一个子目录：本机 Windows 上首次 `remove_dir_all` 会在这种情况下返回
    /// 145，重试才算把目录真的删掉
    #[test]
    fn a_fixture_directory_is_removed_once_no_handle_is_open() {
        let mut dir = test_dir("cleanup");
        let probe = dir.join("sub/probe.bin");
        std::fs::create_dir_all(probe.parent().unwrap()).unwrap();
        std::fs::write(&probe, b"x").unwrap();
        let path = dir.path().to_path_buf();
        assert!(path.exists());
        dir.close().expect("the fixture directory must be removable while no handle is open");
        assert!(!path.exists(), "close() must have removed the directory");
    }

    /// `release` 只放下被测对象，目录留给调用方；句柄关闭之后目录才删得掉
    #[test]
    fn release_closes_the_value_and_keeps_the_directory() {
        let dir = test_dir("cleanup_src");
        let probe = dir.join("probe.bin");
        let file = std::fs::OpenOptions::new().create(true).truncate(true).write(true).open(&probe).unwrap();
        let mut dir = TestSource::new(dir, file).release();
        assert!(probe.exists(), "releasing the value must not remove the directory");
        dir.close().expect("the file handle is gone, so the directory must be removable");
        assert!(!probe.exists());
    }
}
