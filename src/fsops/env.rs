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

/// 外部工具的搜索目录，固定，不取调用环境的 `PATH`。
///
/// 这些工具以 root 运行，而 `PATH` 由调用者决定（`sudo` 默认保留它，包装脚本与
/// 服务单元也能设）：按它解析等于让调用者决定"跑哪个二进制"，再把它透传给子进程，
/// 连工具自己调的助手程序也一起落在调用者的目录里。固定表是 `sudo` 的 secure_path
/// 口径——这些目录在正常发行版上只归 root 可写。解析（[`find_tool`]）与执行
/// （[`tool_command`]）用同一份表：两处各按一个来源找，就等于"检查的"与"跑的"可能不是同一个。
///
/// 工具不在 FHS 目录里时（NixOS 的 `/run/current-system/sw/bin`、装进 `/opt` 的
/// 发行版包），由打包者在**构建时**用 `DISKEDIT_TOOL_PATH` 换掉这张表。取编译期
/// 常量而不是运行时变量：让调用环境决定搜索目录，与直接读 `PATH` 是同一件事，
/// 只是换了个名字
const TOOL_PATH: &str = match option_env!("DISKEDIT_TOOL_PATH") {
    Some(p) => p,
    None => "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
};

/// 工具存在性守卫：执行前 fail-fast 确认工具存在且可执行
pub(crate) fn find_tool(name: &str) -> Result<PathBuf, FsError> {
    for dir in std::env::split_paths(std::ffi::OsStr::new(TOOL_PATH)) {
        let p = dir.join(name);
        // 需可执行：存在但无 x 位时继续向后找，避免错误推迟到 spawn 才以裸 EACCES 冒出
        if p.is_file() && is_executable(&p) {
            return Ok(p);
        }
    }
    Err(FsError::missing(format!("required tool not found in the system tool path ({TOOL_PATH}): {name}")))
}

/// 工具存在性预检：调用点在**首次写盘之前**确认后置条件所需的工具齐备时用它，
/// 避免"表已提交、FS 步才发现缺工具"。消费者是 Linux 在线路径与 PV 收尾链的写表前预检
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

/// 外部命令的唯一构造函数：可执行路径按固定工具表解析（[`TOOL_PATH`]），环境按
/// "只保留 PATH 与 LC_ALL=C"净化。
///
/// 环境整体清空的原因：`LD_PRELOAD`、`IFS`、`MALLOC_*` 一类变量能改变工具行为乃至把
/// 代码注入子进程，而它们全由调用环境决定。PATH 必须保留——工具常再调自己的助手程序；
/// 但它必须是**上面那张固定的表**，不是调用者的 PATH，否则子进程的助手程序同样由调用者
/// 挑。LC_ALL 固定为 C，各工具的输出格式才可解析。调用点只负责 args 与 stdio
pub(crate) fn tool_command(tool: &str) -> Result<Command, FsError> {
    let mut cmd = Command::new(find_tool(tool)?);
    cmd.env_clear().env("LC_ALL", "C").env("PATH", TOOL_PATH);
    Ok(cmd)
}

/// 执行外部工具。参数用 `AsRef<OsStr>` 收：绝大多数调用点传 `&[&str]` 即可，
/// 卷标这类任意字节的参数靠它原样透传（见 os_bytes），不经任何编码转换
///
/// 先 `spawn` 再 `wait_with_output`：两者的失败不是一回事。spawn 失败 = 工具没跑起来
/// （`Io`）；等待/读取输出失败 = 工具已经启动，写没写目标无法断言，故记
/// `CommandFailed`（调用点的映射把它落成"可能已改变"）
pub(crate) fn run<S: AsRef<OsStr>>(tool: &str, args: &[S]) -> Result<std::process::Output, FsError> {
    let child = tool_command(tool)?
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

/// 同 run，但从 stdin 喂入脚本（sfdisk -N 的分区描述只走 stdin）。
/// stdout/stderr 也必须接管道：调用方按 `Output` 消费它们，未接时 `wait_with_output`
/// 只会返回空 Vec，子进程的失败原因就再也拿不到
#[cfg(target_os = "linux")]
pub(crate) fn run_input(tool: &str, args: &[&str], input: &str) -> Result<std::process::Output, FsError> {
    let mut child = tool_command(tool)?
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

/// 前序步骤（`e2fsck -fp` 一类修复、临时挂载触发的日志重放等）**可能已经写盘**之后，
/// 后一步的启动失败不能再走 `Io`：`Io` 在命令层映射为 Infra，语义是"确定未写盘"
/// （见 `outcome::From<FsError>`）。启动不起来只说明这一次 spawn 没跑成，反推不出盘
/// 没被前序步骤改过。已启动过的失败（`CommandFailed`）原样返回——它本来就意味着
/// "可能已改变"
pub(crate) fn tool_write_maybe_started(tool: &str, dev: &str, e: FsError) -> FsError {
    match e {
        FsError::CommandFailed(_) => e,
        e => FsError::CommandFailed(format!(
            "{tool} could not be started on {dev}: {e} — an earlier step may already have written to it"
        )),
    }
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