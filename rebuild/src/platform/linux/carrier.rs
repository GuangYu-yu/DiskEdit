//! Linux 的虚拟机载体：把目标作为 virtio-blk 交给 QEMU。
//!
//! 目标以一份独立描述符进入虚拟机，宿主已持有的句柄在会话期间不再写目标。
//! 逻辑扇区尺寸必须是 512 或 4096；其他几何在启动 QEMU 之前拒绝，避免来宾
//! 看到与宿主不同的块尺寸。串口承载协议帧，标准错误留给诊断。

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::platform::{CarrierLaunch, CarrierProcess, PlatformError, PlatformResult};

/// 来宾看到的整盘设备。
pub const GUEST_TARGET: &str = "/dev/vda";

/// QEMU 程序名与机器类型；架构不支持时返回原因。
fn emulator(arch: &str) -> Option<(&'static str, &'static str, &'static str)> {
    match arch {
        "x86_64" => Some(("qemu-system-x86_64", "q35", "ttyS0")),
        "aarch64" => Some(("qemu-system-aarch64", "virt", "ttyAMA0")),
        _ => None,
    }
}

/// 在 `PATH` 中查找 QEMU。
fn locate(name: &str) -> PlatformResult<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(PlatformError::tools(
        name,
        "在 PATH 中找不到 QEMU；安装 QEMU 或提供可用的虚拟机载体",
    ))
}

/// 可用的加速器；没有 `/dev/kvm` 时退回纯软件模拟。
fn acceleration() -> (&'static str, &'static str) {
    if Path::new("/dev/kvm").exists() {
        ("kvm", "host")
    } else {
        ("tcg", "max")
    }
}

/// 组装 QEMU 参数。`fd` 是目标描述符，`accel`/`cpu` 由 [`acceleration`] 给出。
fn arguments(
    launch: &CarrierLaunch,
    program: &str,
    machine: &str,
    console: &str,
    fd: i32,
    accel: &str,
    cpu: &str,
) -> Vec<String> {
    let sector = launch.sector_size;
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
        "-blockdev".into(),
        "driver=file,filename=/dev/fdset/1,node-name=target-file,cache.direct=on,aio=threads"
            .into(),
        "-blockdev".into(),
        "driver=raw,file=target-file,node-name=target".into(),
        "-device".into(),
        format!(
            "virtio-blk-pci,drive=target,logical_block_size={sector},physical_block_size={sector}"
        ),
        "-object".into(),
        "rng-random,filename=/dev/urandom,id=rng0".into(),
        "-device".into(),
        "virtio-rng-pci,rng=rng0".into(),
        "-kernel".into(),
        launch.kernel.display().to_string(),
        "-initrd".into(),
        launch.initramfs.display().to_string(),
        "-append".into(),
        format!(
            "console={console},115200 quiet loglevel=3 diskedit.token={}",
            launch.token
        ),
        "-machine".into(),
        format!("{machine},accel={accel}"),
        "-cpu".into(),
        cpu.into(),
    ];
    if program == "qemu-system-x86_64" {
        args.push("-device".into());
        args.push("isa-debug-exit,iobase=0xf4,iosize=0x04".into());
    }
    args
}

/// 只接受 512 或 4096 字节逻辑扇区。
fn check_sector(sector: u64) -> PlatformResult<()> {
    if sector == 512 || sector == 4096 {
        Ok(())
    } else {
        Err(PlatformError::not_supported(format!(
            "目标的逻辑扇区尺寸 {sector} 不受虚拟机载体支持，只接受 512 或 4096"
        )))
    }
}

/// 启动一台载体；返回的进程持有目标描述符直到结束。
pub fn launch(launch: &CarrierLaunch) -> PlatformResult<Box<dyn CarrierProcess>> {
    let (program, machine, console) = emulator(&launch.arch).ok_or_else(|| {
        PlatformError::not_supported(format!("架构 {} 没有可用的虚拟机载体", launch.arch))
    })?;
    check_sector(launch.sector_size)?;
    let executable = locate(program)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&launch.target)
        .map_err(|error| PlatformError::io(&launch.target, error))?;
    let fd = file.as_raw_fd();
    let (accel, cpu) = acceleration();
    let args = arguments(launch, program, machine, console, fd, accel, cpu);

    let mut command = Command::new(&executable);
    command
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    // 目标描述符要穿过 exec：开启时带了 close-on-exec，这里清掉；同时让
    // QEMU 随宿主退出，避免留下无人通信的来宾。
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| PlatformError::tools(program, format!("启动 QEMU 失败：{error}")))?;
    let io = child_io(&mut child);
    Ok(Box::new(Process {
        child,
        io,
        target: Some(file),
    }))
}

fn child_io(child: &mut Child) -> Option<(Box<dyn Write + Send>, Box<dyn Read + Send>)> {
    let stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    Some((Box::new(stdin), Box::new(stdout)))
}

struct Process {
    child: Child,
    io: Option<(Box<dyn Write + Send>, Box<dyn Read + Send>)>,
    /// 保持目标描述符存活；QEMU 经继承的副本访问同一文件。
    target: Option<File>,
}

impl CarrierProcess for Process {
    fn take_io(&mut self) -> Option<(Box<dyn Write + Send>, Box<dyn Read + Send>)> {
        self.io.take()
    }

    fn terminate(&mut self) {
        drop(self.child.kill());
        drop(self.child.wait());
        drop(self.target.take());
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> CarrierLaunch {
        CarrierLaunch {
            target: PathBuf::from("/tmp/image"),
            sector_size: 512,
            disk_size: 1 << 20,
            kernel: PathBuf::from("/bundle/kernel"),
            initramfs: PathBuf::from("/bundle/initramfs"),
            arch: "x86_64".into(),
            token: "token".into(),
        }
    }

    fn program(args: &[String]) -> String {
        args.join(" ")
    }

    #[test]
    fn x86_arguments_attach_target_as_virtio_blk() {
        let args = arguments(
            &launch(),
            "qemu-system-x86_64",
            "q35",
            "ttyS0",
            7,
            "kvm",
            "host",
        );
        let text = program(&args);
        assert!(text.contains("-add-fd fd=7,set=1"));
        assert!(text.contains("cache.direct=on"));
        assert!(text.contains("logical_block_size=512,physical_block_size=512"));
        assert!(text.contains("isa-debug-exit"));
        assert!(text.contains("-machine q35,accel=kvm"));
        assert!(text.contains("-cpu host"));
        assert!(text.contains("diskedit.token=token"));
    }

    #[test]
    fn arm_arguments_use_virt_and_no_debug_exit() {
        let mut launch = launch();
        launch.arch = "aarch64".into();
        let args = arguments(
            &launch,
            "qemu-system-aarch64",
            "virt",
            "ttyAMA0",
            3,
            "tcg",
            "max",
        );
        let text = program(&args);
        assert!(text.contains("-machine virt,accel=tcg"));
        assert!(text.contains("-cpu max"));
        assert!(text.contains("console=ttyAMA0,115200"));
        assert!(!text.contains("isa-debug-exit"));
    }

    #[test]
    fn unknown_architecture_is_refused() {
        let mut launch = launch();
        launch.arch = "riscv64".into();
        assert!(emulator(&launch.arch).is_none());
    }

    #[test]
    fn unsupported_sector_size_is_refused_before_spawn() {
        assert!(check_sector(1024).is_err(), "非 512/4096 的扇区应被拒绝");
        assert!(check_sector(512).is_ok());
        assert!(check_sector(4096).is_ok());
    }
}
