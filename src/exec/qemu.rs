//! VM 载体：bundle 校验与 QEMU 启动参数。
//!
//! bundle 是一个目录：`manifest.json` 声明架构、可执行文件版本与每个制品的
//! SHA-256，`kernel` / `initramfs` 是启动所需的两件制品。校验在这里一次做完，
//! 任何不符都在启动之前拒绝——宿主的写入记录与目标几何不能被一个来路不明的
//! guest 接走。
//!
//! 启动器把目标句柄按 fd 交给 QEMU（`-add-fd` 加 `/dev/fdset/`，经 `-blockdev` 接盘），
//! 缓存固定为 `cache.direct=on`，串口走 stdio；guest 的协议帧因此直接落在宿主会话的
//! 管道上。QEMU 的退出码只说明虚拟机进程怎么结束的，步骤是否落盘由协议的 `result`
//! 与宿主探测回答。

#[cfg(target_os = "linux")]
use std::io::Read as _;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
#[cfg(target_os = "linux")]
use std::path::{Component, Path, PathBuf};

#[cfg(target_os = "linux")]
use serde_json::Value;
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};

#[cfg(target_os = "linux")]
use super::protocol;

use crate::outcome::RunOutcome;

/// 载体目录的环境变量名
pub(crate) const BUNDLE_ENV: &str = "DISKEDIT_VM_BUNDLE";

/// 是否配了载体目录。只看变量是否设置；目录内容由 [`Bundle::discover`] 在写盘前校验
fn configured() -> bool {
    std::env::var_os(BUNDLE_ENV).is_some()
}

/// 本平台能不能把目标交给载体。交棒走 fd 与 `/dev/fdset`，只有 Linux 有这条路径；
/// 其余平台上 [`Bundle::discover`] 恒返回 `None`，配了 bundle 也接不走作业
pub(crate) fn platform_can_carry() -> bool {
    cfg!(target_os = "linux")
}

/// 这次运行有没有可能落到载体：本平台能交棒，且配了 bundle。准入据此决定"先别拒绝"
pub(crate) fn carrier_possible() -> bool {
    platform_can_carry() && configured()
}

/// 清单元数据格式的版本
#[cfg(target_os = "linux")]
const SCHEMA: u64 = 1;

/// guest 内的目标设备节点。virtio-blk 只有这一块盘
#[cfg(target_os = "linux")]
pub(crate) const TARGET: &str = "/dev/vda";

/// 校验通过的载体 bundle
#[cfg(target_os = "linux")]
pub(crate) struct Bundle {
    kernel: PathBuf,
    initramfs: PathBuf,
    arch: String,
    /// 清单声明的 worker 映像摘要；构建方未声明时为 `None`
    pub(crate) worker_digest: Option<[u8; 32]>,
}

/// 非 Linux 宿主的载体端口：VM 启动路径依赖 fd 交棒与 `/dev/fdset`，这里不提供
#[cfg(not(target_os = "linux"))]
pub(crate) struct Bundle {
    pub(crate) worker_digest: Option<[u8; 32]>,
}

impl Bundle {
    /// 环境变量指向的 bundle。未设置时 `Ok(None)`；设置但任何一项不符都是拒绝，
    /// 不静默退回原生执行——调用方已经判定宿主做不了这份作业
    #[cfg(target_os = "linux")]
    pub(crate) fn discover() -> Result<Option<Self>, RunOutcome> {
        let Some(dir) = std::env::var_os(BUNDLE_ENV) else {
            return Ok(None);
        };
        let root = PathBuf::from(dir);
        let manifest = std::fs::read(root.join("manifest.json")).map_err(|e| {
            RunOutcome::unsupported(format!(
                "{BUNDLE_ENV}={}: cannot read manifest.json: {e}",
                root.display()
            ))
        })?;
        let value: Value = serde_json::from_slice(&manifest).map_err(|e| {
            RunOutcome::unsupported(format!(
                "{BUNDLE_ENV}={}: manifest.json is not JSON: {e}",
                root.display()
            ))
        })?;
        let schema = value.get("schema").and_then(Value::as_u64);
        if schema != Some(SCHEMA) {
            return Err(RunOutcome::unsupported(format!(
                "{BUNDLE_ENV}={}: manifest schema is {schema:?}, this build reads {SCHEMA}",
                root.display()
            )));
        }
        let arch = text(&value, "arch", "manifest.arch")?;
        if arch != std::env::consts::ARCH {
            return Err(RunOutcome::unsupported(format!(
                "the VM bundle was built for {arch}, this process is {}",
                std::env::consts::ARCH
            )));
        }
        let version = text(&value, "binary_version", "manifest.binary_version")?;
        if version != protocol::BINARY_VERSION {
            return Err(RunOutcome::unsupported(format!(
                "the VM bundle carries worker {version}, this host speaks {}",
                protocol::BINARY_VERSION
            )));
        }
        let kernel = artifact(&root, &value, "kernel")?;
        let initramfs = artifact(&root, &value, "initramfs")?;
        let worker_digest = value
            .get("worker")
            .and_then(|w| w.get("sha256"))
            .and_then(Value::as_str)
            .map(|hex| {
                parse_sha256(hex).ok_or_else(|| {
                    RunOutcome::unsupported("manifest.worker.sha256 is not a SHA-256 hex string")
                })
            })
            .transpose()?;
        Ok(Some(Self { kernel, initramfs, arch: arch.to_string(), worker_digest }))
    }

    /// 非 Linux 宿主不提供 VM 载体：启动路径依赖 fd 交棒与 `/dev/fdset`
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn discover() -> Result<Option<Self>, RunOutcome> {
        Ok(None)
    }
}

/// 一件制品：清单里的相对路径与声明的摘要
#[cfg(target_os = "linux")]
fn artifact(root: &Path, manifest: &Value, name: &str) -> Result<PathBuf, RunOutcome> {
    let Some(entry) = manifest.get(name) else {
        return Err(RunOutcome::unsupported(format!("manifest has no `{name}` entry")));
    };
    let rel = text(entry, "path", &format!("manifest.{name}.path"))?;
    let path = safe_join(root, rel).ok_or_else(|| {
        RunOutcome::unsupported(format!(
            "manifest.{name}.path `{rel}` is not a plain relative path"
        ))
    })?;
    let declared = text(entry, "sha256", &format!("manifest.{name}.sha256"))?;
    let declared = parse_sha256(declared)
        .ok_or_else(|| RunOutcome::unsupported(format!("manifest.{name}.sha256 is not a SHA-256 hex string")))?;
    let actual = file_digest(&path).map_err(|e| {
        RunOutcome::unsupported(format!("cannot read the bundle's {name} at {}: {e}", path.display()))
    })?;
    if actual != declared {
        return Err(RunOutcome::unsupported(format!(
            "the bundle's {name} does not match manifest.{name}.sha256 (declared {}, read {})",
            hex(&declared),
            hex(&actual)
        )));
    }
    Ok(path)
}

#[cfg(target_os = "linux")]
fn text<'a>(value: &'a Value, key: &str, what: &str) -> Result<&'a str, RunOutcome> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RunOutcome::unsupported(format!("{what} is missing or not a string")))
}

/// 只接受落在 bundle 目录内的相对路径：绝对路径与任何 `.`/`..`/前缀都拒绝
#[cfg(target_os = "linux")]
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let path = Path::new(rel);
    if path.is_absolute() {
        return None;
    }
    let mut out = root.to_path_buf();
    for part in path.components() {
        match part {
            Component::Normal(part) => out.push(part),
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(target_os = "linux")]
fn parse_sha256(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[index] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

#[cfg(target_os = "linux")]
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(target_os = "linux")]
fn file_digest(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    // 分块读的搬运单元：1 MiB（与帧上限同一口径，取值来自 `support::MIB_BYTES`）
    let mut buffer = vec![0u8; crate::support::MIB_BYTES as usize];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

/// 架构 → QEMU 程序名。只列本工具认识的两种：virtio-blk 与串口在两者上都成立
#[cfg(target_os = "linux")]
fn qemu_program(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("qemu-system-x86_64"),
        "aarch64" => Some("qemu-system-aarch64"),
        _ => None,
    }
}

/// 启动一条 QEMU 会话。目标句柄经 fd 交给 QEMU：子进程继承同一份打开的文件，
/// 宿主不复制内容，也不在中途重开路径
/// virtio-blk 能承载的逻辑扇区大小：最小扇区与 4K。目标几何由 `SectorSize` 校验，
/// 这里只回答"这台机器能不能把它挂上去"
#[cfg(target_os = "linux")]
const VIRTIO_SECTOR_SIZES: [u64; 2] = [crate::domain::SectorSize::MIN_BYTES, 4096];

/// 载体能拿到的硬件加速。启动前问一次，机器类型、CPU 型号与提示都跟着它走：
/// 不让 QEMU 自己回退，否则它会安静地落到 TCG，慢十倍也没有人知道
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Acceleration {
    Kvm,
    Tcg,
}

#[cfg(target_os = "linux")]
impl Acceleration {
    fn as_str(self) -> &'static str {
        match self {
            Self::Kvm => "kvm",
            Self::Tcg => "tcg",
        }
    }

    /// guest 看到的 CPU 型号：有 KVM 就给宿主型号，否则退到 TCG 支持的上限型号。
    /// 不给这一项时 guest 只看到 QEMU 的缺省型号
    fn cpu_model(self) -> &'static str {
        match self {
            Self::Kvm => "host",
            Self::Tcg => "max",
        }
    }
}

/// 能否用 KVM：`/dev/kvm` 存在且当前身份打得开才算。打得开与否就是同一件事的两种
/// 结果，不靠 QEMU 事后回答
#[cfg(target_os = "linux")]
fn acceleration() -> Acceleration {
    match std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm") {
        Ok(_) => Acceleration::Kvm,
        Err(_) => Acceleration::Tcg,
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn command(
    bundle: &Bundle,
    src: &crate::dev::WriteSource,
    token: &str,
) -> Result<std::process::Command, RunOutcome> {
    let program = qemu_program(&bundle.arch).ok_or_else(|| {
        RunOutcome::unsupported(format!(
            "no QEMU machine is wired for architecture {}",
            bundle.arch
        ))
    })?;
    if !VIRTIO_SECTOR_SIZES.contains(&src.sector_size) {
        return Err(RunOutcome::unsupported(format!(
            "virtio-blk cannot carry the target's {}-byte logical sectors",
            src.sector_size
        )));
    }
    let qemu = which(program).ok_or_else(|| {
        RunOutcome::unsupported(format!(
            "the VM bundle is present but {program} is not in PATH (package: qemu-system-{})",
            if bundle.arch == "x86_64" { "x86" } else { "arm" }
        ))
    })?;
    let fd = src.raw_fd();
    let accel = acceleration();
    if accel == Acceleration::Tcg {
        eprintln!(
            "note: /dev/kvm is not usable here — the carrier runs under TCG emulation, which is many \
             times slower than running the same job natively; give this process access to /dev/kvm, or \
             run the command as root so the job stays native"
        );
    }
    let mut command = std::process::Command::new(qemu);
    command.args(arguments(bundle, src.sector_size, fd, token, accel));
    // SAFETY: pre_exec 只做两个不分配内存的系统调用；失败以 errno 返回，
    // 子进程在 exec 之前退出，命令执行不会继续
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // 父进程消失时一并终止虚拟机：否则目标句柄被一个孤儿持有，
            // 比"会话中断"更难回收
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}

/// QEMU 的命令行。设备、总线与退出装置随架构取：x86 用 q35 与 isa-debug-exit，
/// aarch64 用 virt 与 PSCI 关机；加速器与 CPU 型号由调用方判定后传进来
#[cfg(target_os = "linux")]
fn arguments(
    bundle: &Bundle,
    sector_size: u64,
    fd: i32,
    token: &str,
    accel: Acceleration,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-nodefaults".into(),
        "-no-user-config".into(),
        "-m".into(),
        "1024".into(),
        "-smp".into(),
        "1".into(),
        "-display".into(),
        "none".into(),
        "-monitor".into(),
        "none".into(),
        "-serial".into(),
        "stdio".into(),
        "-no-reboot".into(),
        "-add-fd".into(),
        format!("fd={fd},set=1"),
        // 不用 `-drive`：它交棒 fdset 时走 `monitor_fdset_dup_fd_add`，要求 fd 已带
        // O_DIRECT，镜像目标上这一步 EPERM、guest 根本起不来。`-blockdev` 走块层自己的
        // 打开路径，同一个 fd 可用；`cache.direct=on` 与 `-drive` 的 `cache=none` 等价
        "-blockdev".into(),
        "driver=file,filename=/dev/fdset/1,node-name=target-file,cache.direct=on,aio=threads".into(),
        "-blockdev".into(),
        "driver=raw,file=target-file,node-name=target".into(),
        "-device".into(),
        format!(
            "virtio-blk-pci,drive=target,logical_block_size={sector_size},physical_block_size={sector_size}"
        ),
        "-object".into(),
        "rng-random,filename=/dev/urandom,id=rng0".into(),
        "-device".into(),
        "virtio-rng-pci,rng=rng0".into(),
        "-kernel".into(),
        bundle.kernel.display().to_string(),
        "-initrd".into(),
        bundle.initramfs.display().to_string(),
        "-append".into(),
        // 令牌只能走内核命令行：guest 侧启动脚本不在本仓库里，换通道（fw_cfg、附加盘）都
        // 需要 guest 配合才能读到。这**不扩大攻击面**——能读 `/proc/cmdline` 的 guest 进程
        // 同时也能读交给它的设备节点（`/dev/vd*`），令牌挡的不是它；令牌挡的是宿主机上
        // 拿到同一根管写权限的第三方，而它们看不到 guest 的 stdout（会话号经那里下发）。
        // 真正的会话边界由每帧回带的会话号承担（见 `exec/worker.rs`）
        format!("console=ttyS0,115200 quiet loglevel=3 diskedit.token={token}"),
    ];
    match bundle.arch.as_str() {
        "aarch64" => args.extend([
            "-machine".into(),
            format!("virt,accel={}", accel.as_str()),
        ]),
        // isa-debug-exit 是 x86 上唯一能带出"guest 主动结束"退出码的装置；
        // guest 用它区分正常关机与启动失败
        _ => args.extend([
            "-machine".into(),
            format!("q35,accel={}", accel.as_str()),
            "-device".into(),
            "isa-debug-exit,iobase=0xf4,iosize=0x04".into(),
        ]),
    }
    args.extend(["-cpu".into(), accel.cpu_model().into()]);
    args
}

#[cfg(target_os = "linux")]
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(program)).find(|p| p.is_file())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn bundle() -> Bundle {
        Bundle {
            kernel: PathBuf::from("/tmp/bundle/kernel"),
            initramfs: PathBuf::from("/tmp/bundle/initramfs"),
            arch: "x86_64".into(),
            worker_digest: None,
        }
    }

    /// bundle 的启动参数：缓存钉死、fd 交棒、退出装置齐备
    #[test]
    fn no_unsafe_cache_and_exit_device() {
        let args = arguments(&bundle(), 512, 3, "token", Acceleration::Kvm);
        let joined = args.join(" ");
        assert!(joined.contains("cache.direct=on"));
        assert!(!joined.contains("cache=unsafe"));
        assert!(joined.contains("isa-debug-exit"));
        assert!(joined.contains("filename=/dev/fdset/1"));
        assert!(joined.contains("fd=3,set=1"));
        assert!(joined.contains("virtio-rng-pci"));
        assert!(joined.contains("diskedit.token=token"));
    }

    /// 加速器与 CPU 型号成对出现，且不留 QEMU 自己的回退通道：
    /// `-cpu host` 只在 KVM 下成立，TCG 下是 `max`
    #[test]
    fn the_cpu_model_follows_the_detected_acceleration() {
        let kvm = arguments(&bundle(), 512, 3, "token", Acceleration::Kvm).join(" ");
        assert!(kvm.contains("accel=kvm"));
        assert!(kvm.contains("-cpu host"));
        assert!(!kvm.contains("kvm:tcg"), "the fallback chain belongs to the launcher, not to QEMU");

        let tcg = arguments(&bundle(), 512, 3, "token", Acceleration::Tcg).join(" ");
        assert!(tcg.contains("accel=tcg"));
        assert!(tcg.contains("-cpu max"));
        assert!(!tcg.contains("host"));
    }

    /// 架构不认识的 bundle 不启动
    #[test]
    fn an_unknown_architecture_has_no_machine() {
        assert!(qemu_program("riscv64").is_none());
    }
}
