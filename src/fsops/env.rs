//! 平台闸口与外部工具执行（Linux 实现；非 Linux 为拒绝存根）。

use super::*;

pub fn require_linux() -> Result<(), FsError> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        Err(FsError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "this operation requires Linux (build/runtime platform)",
        )))
    }
}

pub fn require_root() -> Result<(), FsError> {
    #[cfg(target_os = "linux")]
    {
        // geteuid = POSIX.1（man geteuid）实际 UID 判定，非有效权限位
        // SAFETY: geteuid 无参数、不访问内存，恒成功
        if unsafe { libc::geteuid() } != 0 {
            return Err(FsError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "root required (losetup/mkfs/resize need kernel privileges)",
            )));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(FsError::Io(io::Error::new(io::ErrorKind::Unsupported, "not linux")))
    }
}

/// 工具存在性守卫：执行前 fail-fast 确认工具存在且可执行
pub(crate) fn find_tool(name: &str) -> Result<PathBuf, FsError> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| FsError::missing("PATH unset — cannot locate required tools"))?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        // 需可执行：存在但无 x 位时继续向后找，避免错误推迟到 spawn 才以裸 EACCES 冒出
        if p.is_file() && is_executable(&p) {
            return Ok(p);
        }
    }
    Err(FsError::missing(format!("required tool not found in PATH: {name}")))
}

/// 工具存在性预检：调用点在**首次写盘之前**确认后置条件所需的工具齐备时用它，
/// 避免"表已提交、FS 步才发现缺工具"。当前唯一消费者是 Linux 在线路径
#[cfg(target_os = "linux")]
pub(crate) fn require_tool(name: &str) -> Result<(), FsError> {
    find_tool(name).map(|_| ())
}

/// 执行位判定：stat(2) 得到的 st_mode 中 S_IXUSR|S_IXGRP|S_IXOTH（0o111）任一置位即视为
/// 可执行（权限位语义见 chmod(2)）
#[cfg(unix)]
pub(crate) fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
pub(crate) fn is_executable(_p: &std::path::Path) -> bool {
    true
}

/// 执行外部工具。参数用 `AsRef<OsStr>` 收：绝大多数调用点传 `&[&str]` 即可，
/// 卷标这类任意字节的参数靠它原样透传（见 os_bytes），不经任何编码转换
///
/// 先 `spawn` 再 `wait_with_output`：两者的失败不是一回事。spawn 失败 = 工具没跑起来
/// （`Io`）；等待/读取输出失败 = 工具已经启动，写没写目标无法断言，故记
/// `CommandFailed`（调用点的映射把它落成"可能已改变"）
pub(crate) fn run<S: AsRef<OsStr>>(tool: &str, args: &[S]) -> Result<std::process::Output, FsError> {
    let path = find_tool(tool)?;
    // 输出需按格式解析（resize2fs -P、dumpe2fs -h 等），固定 LC_ALL=C 防本地化翻译破坏解析
    let child = Command::new(path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("LC_ALL", "C")
        .spawn()
        .map_err(FsError::from)?;
    child.wait_with_output().map_err(|e| {
        FsError::CommandFailed(format!(
            "{tool} was started but its exit status could not be read ({e}) — it may have changed the target"
        ))
    })
}

/// 任意字节 → 命令行参数。unix 下按原字节构造：execve 的 argv 本就是字节串，无编码校验。
/// 非 unix 平台没有这条调用路径（mkswap 只存在于 Linux），那里退化为有损解码以保持可编译
#[cfg(unix)]
pub(crate) fn os_bytes(b: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(b).to_os_string()
}

#[cfg(not(unix))]
pub(crate) fn os_bytes(b: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(b).into_owned())
}

/// 同 run，但从 stdin 喂入脚本（sfdisk -N 的分区描述只走 stdin）
#[cfg(target_os = "linux")]
pub(crate) fn run_input(tool: &str, args: &[&str], input: &str) -> Result<std::process::Output, FsError> {
    let path = find_tool(tool)?;
    let mut child = Command::new(path)
        .args(args)
        .stdin(Stdio::piped())
        .env("LC_ALL", "C")
        .spawn()
        .map_err(FsError::from)?;
    use std::io::Write as _;
    let mut stdin = child.stdin.take().expect("stdin piped");
    if let Err(e) = stdin.write_all(input.as_bytes()) {
        // 写 stdin 失败时子进程可能还在等输入，显式终止防僵留。子进程**已经启动**：
        // 它可能读到一部分输入后动过盘，也可能什么都没做，无法断言——按"可能已改变"记
        best_effort_reap_child(&mut child);
        return Err(FsError::CommandFailed(format!(
            "{tool} was started but its input could not be delivered ({e}) — it may have changed the target"
        )));
    }
    drop(stdin); // 关闭 stdin 让 sfdisk 看到输入结束
    child.wait_with_output().map_err(|e| {
        FsError::CommandFailed(format!(
            "{tool} was started but its exit status could not be read ({e}) — it may have changed the target"
        ))
    })
}

/// 终止并回收子进程：主错误已由调用点上报，回收本身失败只影响资源占用。
/// 命名即契约——与其余 `best_effort_*` 同族（见 Cargo.toml 的 lints 注释），
/// 使"有意忽略"与"忘了处理"可被 grep 区分
#[cfg(target_os = "linux")]
#[allow(clippy::let_underscore_must_use)] // 有意忽略：主错误已上报，见上
pub(crate) fn best_effort_reap_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// e2fsck 退出码判定——resize 语境（man e2fsck EXIT CODE：各项按位或求和）。bit1（REBOOT）
/// 置位条件（e2fsprogs e2fsck/unix.c）：FS 被修改且 ctx->mount_flags & EXT2_MF_ISROOT——改了
/// root fs 需重启才能继续 resize，2/3 一律中断（未挂载镜像上通常不出现）。
/// 0/1 通过，4 = 有未修正错误即拒绝。
/// 2/3/4 归 `CommandFailed`：三者都说明工具**运行过**——2/3 自述改过文件系统、
/// 4 在 -p 下仍会自动修复过——"不能证明已写"不等于"已证明未写"，不足以支撑
/// `Io → Infra` 的"确定未写盘"承诺。`Failed` 的语义正是不对是否落盘作断言
pub(crate) fn check_e2fsck_for_resize(code: i32) -> Result<(), FsError> {
    match code {
        0 | 1 => Ok(()),
        2 | 3 => Err(FsError::CommandFailed(
            "e2fsck: root filesystem was modified, reboot required before resizing".into(),
        )),
        4 => Err(FsError::CommandFailed(
            "e2fsck: uncorrected errors (exit 4), refuse resize".into(),
        )),
        c => Err(FsError::CommandFailed(format!("e2fsck infrastructure failure (exit {c})"))),
    }
}

/// 前序工具（`e2fsck -fp` 一类修复步骤）**可能已经写盘**之后，后一步的启动失败不能再走
/// `Io`：`Io` 在命令层映射为 Infra，语义是"确定未写盘"（见 `outcome::From<FsError>`）。
/// 启动不起来只说明这一次 spawn 没跑成，反推不出盘没被前序步骤改过
pub(crate) fn tool_write_maybe_started(tool: &str, dev: &str, e: FsError) -> FsError {
    FsError::CommandFailed(format!(
        "{tool} could not be started on {dev}: {e} — a preceding repair step may already have written to it"
    ))
}

/// e2fsck 退出码判定——check 语境。检查命令的本职就是修复：0（干净）、1（已修复）
/// 是成功；2/3 = 修复完成 + REBOOT 位（该位来自内核的 root 挂载标记，只影响"能否
/// 立即继续 resize"，与修复成败无关），同为成功。仅 4（-fp 后仍有未修正错误）算
/// 检查失败；8/16 等为基础设施故障。不复用 resize 判据——那套拒绝 2/3 的理由在
/// check 上没有对应物，照搬会把"修复成功"报成 Failed(30)
pub(crate) fn check_e2fsck_for_check(code: i32) -> Result<(), FsError> {
    match code {
        0..=3 => Ok(()),
        4 => Err(FsError::CommandFailed(
            "e2fsck: uncorrected errors remain (exit 4)".into(),
        )),
        c => Err(FsError::CommandFailed(format!("e2fsck infrastructure failure (exit {c})"))),
    }
}