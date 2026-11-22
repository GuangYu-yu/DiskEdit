//! 平台闸口与外部工具执行（Linux 实现；非 Linux 为拒绝存根）。

use super::*;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;

pub fn require_linux() -> Result<(), FsError> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        Err(FsError::platform("this operation requires Linux (build/runtime platform)"))
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
        Err(FsError::platform("not linux"))
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
pub(crate) fn find_tool(tool: Tool) -> Result<PathBuf, FsError> {
    let name = tool.name();
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
pub(crate) fn require_tool(tool: Tool) -> Result<(), FsError> {
    find_tool(tool).map(|_| ())
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

/// 子进程命令的闸口媒介类型。
///
/// 内部持有 [`std::process::Command`]，但 `Command` 本身不出本模块：字段私有，
/// 没有 `Deref` / `as_command` / `AsRef<Command>`，也没有 `impl From<Command>`；
/// 模块外既拿不到 `&Command`，也无法自行 `Command::new` 绕过环境净化。构造只由本
/// 模块的两个闸口函数完成——[`tool_command`]（文件系统工具）与 [`carrier_command`]
/// （VM 载体）。对外只暴露调用点真正用到的动作：argv、环境追加、stdio 配置、
/// `spawn` / `output` / `status`，以及 Linux 上交棒 fd 所需的 `pre_exec`。
pub(crate) struct GateCommand {
    command: Command,
}

impl GateCommand {
    /// 追加一个命令行参数
    pub(crate) fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self {
        self.command.arg(arg);
        self
    }

    /// 追加一串命令行参数
    pub(crate) fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command.args(args);
        self
    }

    /// 向子进程环境追加一项变量。基线环境由闸口构造函数钉死；这里只能追加，
    /// 不能清空或改写已钉死的项
    pub(crate) fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.command.env(key, val);
        self
    }

    pub(crate) fn stdin<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.command.stdin(cfg);
        self
    }

    pub(crate) fn stdout<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.command.stdout(cfg);
        self
    }

    pub(crate) fn stderr<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.command.stderr(cfg);
        self
    }

    pub(crate) fn spawn(&mut self) -> io::Result<std::process::Child> {
        self.command.spawn()
    }

    pub(crate) fn output(&mut self) -> io::Result<std::process::Output> {
        self.command.output()
    }

    pub(crate) fn status(&mut self) -> io::Result<std::process::ExitStatus> {
        self.command.status()
    }
}

/// Linux 专属：fork 之后、exec 之前在子进程里跑钩子。载体用它清目标 fd 的
/// CLOEXEC 并置 PDEATHSIG。unsafe 口径与 `CommandExt::pre_exec` 相同：闭包只能做
/// async-signal-safe 的事，SAFETY 契约由调用点在自己的注释里承担
#[cfg(target_os = "linux")]
impl GateCommand {
    pub(crate) unsafe fn pre_exec<F>(&mut self, f: F) -> &mut Self
    where
        F: FnMut() -> io::Result<()> + Send + Sync + 'static,
    {
        // SAFETY: 契约由本函数的调用方承担（与 `CommandExt::pre_exec` 同一口径）：
        // 闭包只做 async-signal-safe 的事，失败以 errno 返回，子进程在 exec 前退出
        unsafe {
            self.command.pre_exec(f);
        }
        self
    }
}

/// 文件系统工具的闸口构造函数：可执行路径按固定工具表解析（[`TOOL_PATH`]），
/// 环境按"只保留 PATH 与 LC_ALL=C"净化。
///
/// 返回的 [`GateCommand`] 把内部 `Command` 封在闸口模块内：模块外在类型层就拿不到
/// `Command`，也没有第二条自行构造的路径。环境净化因此不是"调用点应当遵守"的约定，
/// 而是构造点只有这一处的编译期事实。
///
/// 环境整体清空的原因：`LD_PRELOAD`、`IFS`、`MALLOC_*` 一类变量能改变工具行为乃至把
/// 代码注入子进程，而它们全由调用环境决定。PATH 必须保留——工具常再调自己的助手程序；
/// 但它必须是**上面那张固定的表**，不是调用者的 PATH，否则子进程的助手程序同样由调用者
/// 挑。LC_ALL 固定为 C，各工具的输出格式才可解析。调用点只负责 args 与 stdio
///
/// argv 里的设备路径由工具自己 `open`：本进程持有的句柄钉住自己的 ioctl，不覆盖
/// 子进程的路径解析。改传 `/proc/<pid>/fd/N` 也不行——lvm2 的 `pvcreate` / `pvresize`
/// 按名字在 sysfs 里找设备，对该路径报 `No device found`，因此不按工具分流，所有
/// 调用点用同一份真实设备路径
pub(crate) fn tool_command(tool: Tool) -> Result<GateCommand, FsError> {
    let mut command = Command::new(find_tool(tool)?);
    command.env_clear();
    let mut sealed = GateCommand { command };
    sealed.env("LC_ALL", "C").env("PATH", TOOL_PATH);
    Ok(sealed)
}

/// VM 载体（QEMU）的闸口构造函数。程序路径由调用点解析后传入；环境策略在此显式
/// 钉死，与文件系统工具同一口径：
///
/// - `env_clear()`：`LD_PRELOAD`、`MALLOC_*` 一类调用者可控变量不进子进程；
/// - `PATH` 固定为 [`TOOL_PATH`]，不用调用者的 PATH；
/// - 不保留 `LC_ALL`：QEMU 自己的输出不参与协议解析——宿主帧解码器读的是 guest 经
///   `-serial stdio` 送来的字节，QEMU 自身诊断只作噪声行转发，没有按 locale 解析的
///   文本，不需要固定 locale；
/// - 其余变量（`HOME`、`TMPDIR`、`QEMU_*` 等）本配置下载体都不读：`-no-user-config`
///   关掉用户配置，随机源走绝对路径 `/dev/urandom`，不在临时目录落盘。
#[cfg(target_os = "linux")]
pub(crate) fn carrier_command(program: PathBuf) -> GateCommand {
    let mut command = Command::new(program);
    command.env_clear();
    let mut sealed = GateCommand { command };
    sealed.env("PATH", TOOL_PATH);
    sealed
}

/// 工具与目标的写关系。工具是否改盘由工具与它收到的参数共同决定（同一支 `e2fsck`
/// 在 `-n` 下只读、在 `-fp` 下修复；`fsck.f2fs` 无参也可能写回全局元数据），
/// 编译期无从判定，故由调用点显式声明。声明成 [`ReadsOnly`](Self::ReadsOnly) 却
/// 实际写盘时，写事实不置位：失败会被报成"确定未改动"
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolAccess {
    /// 工具不写目标：只读开关、查询、`--dry-run` 一类
    ReadsOnly,
    /// 进程一旦启动就可能改过目标
    WritesTarget,
}

/// 只读工具：不写目标的查询与探测。与 [`run_write`] 之间不设默认入口——每个调用点
/// 必须写明是哪一类，写目标的工具从只读入口启动在这一层不可能发生
///
/// 参数用 `AsRef<OsStr>` 收：绝大多数调用点传 `&[&str]` 即可，卷标这类任意字节的参数
/// 靠它原样透传（见 os_bytes），不经任何编码转换
///
/// 先 `spawn` 再 `wait_with_output`：两者的失败不是一回事。spawn 失败 = 工具没跑起来
/// （`Io`）；等待/读取输出失败 = 工具已经启动，写没写目标无法断言，故记
/// `CommandFailed`（调用点的映射把它落成"可能已改变"）
pub(crate) fn run_read<S: AsRef<OsStr>>(tool: Tool, args: &[S]) -> Result<std::process::Output, FsError> {
    run_with(tool, args, ToolAccess::ReadsOnly)
}

/// 会改动目标的工具：进程一旦启动，盘上状态就不再可断言，故在 spawn 成功后置位
/// （见 `outcome::note_target_write`）。参数决定读写时（如 `fsck.f2fs` 的修复分支）
/// 用 [`run_with`] 按实际取值声明
pub(crate) fn run_write<S: AsRef<OsStr>>(tool: Tool, args: &[S]) -> Result<std::process::Output, FsError> {
    run_with(tool, args, ToolAccess::WritesTarget)
}

/// 启动外部工具的唯一入口。两个包装函数与它的差别只有访问方式：写事实的置位点
/// 因此只有这一处
pub(crate) fn run_with<S: AsRef<OsStr>>(
    tool: Tool,
    args: &[S],
    access: ToolAccess,
) -> Result<std::process::Output, FsError> {
    run_impl(tool, args, access == ToolAccess::WritesTarget)
}

fn run_impl<S: AsRef<OsStr>>(tool: Tool, args: &[S], writes: bool) -> Result<std::process::Output, FsError> {
    let child = tool_command(tool)?
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(FsError::from)?;
    if writes {
        crate::outcome::note_target_write();
    }
    child.wait_with_output().map_err(|e| {
        FsError::CommandFailed(format!(
            "{} was started but its exit status could not be read ({e}) — it may have changed the target",
            tool.name()
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

/// 从 stdin 喂脚本且会改动目标的工具（sfdisk）：spawn 成功后置位。
/// stdout/stderr 也必须接管道：调用方按 `Output` 消费它们，未接时 `wait_with_output`
/// 只会返回空 Vec，子进程的失败原因就再也拿不到
#[cfg(target_os = "linux")]
pub(crate) fn run_write_input(tool: Tool, args: &[&str], input: &str) -> Result<std::process::Output, FsError> {
    run_input_impl(tool, args, input, true)
}

#[cfg(target_os = "linux")]
fn run_input_impl(
    tool: Tool,
    args: &[&str],
    input: &str,
    writes: bool,
) -> Result<std::process::Output, FsError> {
    let mut child = tool_command(tool)?
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(FsError::from)?;
    if writes {
        crate::outcome::note_target_write();
    }
    use std::io::Write as _;
    // 句柄在上游 builder 里以 `.stdin(Stdio::piped())` 接出：按 `Child` 的构造约定，
    // piped 的 stdin 在返回的子进程句柄上必为 `Some`；spawn 成功到 take 之间没有别处
    // 动过这个局部句柄，因此这里取空不在可达路径上
    let mut stdin = child.stdin.take().expect("stdin was piped when the child was spawned");
    if let Err(e) = stdin.write_all(input.as_bytes()) {
        // 写 stdin 失败时子进程可能还在等输入，显式终止防僵留。子进程**已经启动**：
        // 它可能读到一部分输入后动过盘，也可能什么都没做，无法断言——按"可能已改变"记
        best_effort_reap_child(&mut child);
        return Err(FsError::CommandFailed(format!(
            "{} was started but its input could not be delivered ({e}) — it may have changed the target",
            tool.name()
        )));
    }
    drop(stdin); // 关闭 stdin 让 sfdisk 看到输入结束
    child.wait_with_output().map_err(|e| {
        FsError::CommandFailed(format!(
            "{} was started but its exit status could not be read ({e}) — it may have changed the target",
            tool.name()
        ))
    })
}

/// 终止并回收子进程：主错误已由调用点上报，回收本身失败只影响资源占用。
/// 命名即契约——与其余 `best_effort_*` 同族（见 Cargo.toml 的 lints 注释），
/// 使"有意忽略"与"忘了处理"可被 grep 区分
#[cfg(target_os = "linux")]
// audit-allow: A-9
#[allow(clippy::let_underscore_must_use)] // 有意忽略：主错误已上报
pub(crate) fn best_effort_reap_child(child: &mut std::process::Child) {
    // audit-allow: D-1
    let _ = child.kill();
    // audit-allow: D-2
    let _ = child.wait();
}

/// e2fsck 退出码判定——resize 语境（man e2fsck EXIT CODE：各项按位或求和）。
/// 位表：1 = 文件系统错误已修正，2 = 系统需重启，4 = 仍有未修正错误，8 = 运行错误，
/// 16 = 用法或语法错误，32 = 用户取消，128 = 共享库错误；组合码是这些位的和
/// （5 = 1|4，6 = 2|4，12 = 4|8）。bit1（REBOOT）置位条件（e2fsprogs
/// e2fsck/unix.c）：FS 被修改且 ctx->mount_flags & EXT2_MF_ISROOT——改了 root fs
/// 需重启才能继续 resize，2/3 一律中断（未挂载镜像上通常不出现）。
/// 0/1 通过，4 = 有未修正错误即拒绝；其余组合码逐位报出，不并成一句"基础设施故障"。
/// 2/3/4 与其余非零码归 `CommandFailed`：它们都说明工具**运行过**——2/3 自述改过
/// 文件系统、4 在 -p 下仍会自动修复过——"不能证明已写"不等于"已证明未写"，不足以
/// 支撑 `Io → NoMutation` 的"确定未写盘"承诺。`CommandFailed` 的语义正是不对
/// 是否落盘作断言
pub(crate) fn check_e2fsck_for_resize(code: i32) -> Result<(), FsError> {
    match code {
        0 | 1 => Ok(()),
        2 | 3 => Err(FsError::CommandFailed(
            "e2fsck: root filesystem was modified, reboot required before resizing".into(),
        )),
        4 => Err(FsError::CommandFailed(
            "e2fsck: uncorrected errors (exit 4), refuse resize".into(),
        )),
        c => Err(FsError::CommandFailed(format!(
            "e2fsck exit {c} ({}) — refuse resize",
            describe_e2fsck_bits(c)
        ))),
    }
}

/// 把组合退出码展开成它置位的每一项（位表见 [`check_e2fsck_for_resize`]）；
/// 表外的码没有可报的位，如实说明
fn describe_e2fsck_bits(code: i32) -> String {
    const BITS: [(i32, &str); 7] = [
        (1, "filesystem errors corrected"),
        (2, "system should be rebooted"),
        (4, "uncorrected errors remain"),
        (8, "operational error"),
        (16, "usage or syntax error"),
        (32, "cancelled by user"),
        (128, "shared-library error"),
    ];
    let named: Vec<&str> = BITS
        .iter()
        .filter(|(bit, _)| code & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if named.is_empty() {
        "no bit of the e2fsck exit-code table is set".to_string()
    } else {
        named.join(", ")
    }
}

/// e2fsck 退出状态判定——resize 语境。退出码与"被信号杀死"是两件事：`code()` 为 None 时
/// 用 [`describe_exit`] 出文，不拿一个占位码冒充退出码
pub(crate) fn check_e2fsck_status_for_resize(st: &std::process::ExitStatus) -> Result<(), FsError> {
    match st.code() {
        Some(code) => check_e2fsck_for_resize(code),
        None => Err(FsError::CommandFailed(format!("e2fsck {}, refuse resize", describe_exit(st)))),
    }
}

/// 前序步骤（`e2fsck -fp` 一类修复、临时挂载触发的日志重放等）**可能已经写盘**之后，
/// 后一步的启动失败不能再走 `Io`：`Io` 在命令层映射为 NoMutation，语义是"确定未写盘"
/// （见 `outcome::From<FsError>`）。启动不起来只说明这一次 spawn 没跑成，反推不出盘
/// 没被前序步骤改过。已启动过的失败（`CommandFailed`）原样返回——它本来就意味着
/// "可能已改变"
pub(crate) fn tool_write_maybe_started(tool: Tool, dev: &str, e: FsError) -> FsError {
    match e {
        FsError::CommandFailed(_) => e,
        e => FsError::CommandFailed(format!(
            "{} could not be started on {dev}: {e} — an earlier step may already have written to it",
            tool.name()
        )),
    }
}

/// e2fsck 退出码判定——check 语境。check 以 `-n` 运行（man e2fsck(8)：只读打开、
/// 所有提问答 `no`），不会写盘，因此 0 是唯一成功值；其余退出码都表示这次检查
/// 发现了问题或工具本身故障（工具退出非零即检查失败）
pub(crate) fn check_e2fsck_for_check(code: i32) -> Result<(), FsError> {
    match code {
        0 => Ok(()),
        c => Err(FsError::CommandFailed(format!("e2fsck reported errors (exit {c})"))),
    }
}