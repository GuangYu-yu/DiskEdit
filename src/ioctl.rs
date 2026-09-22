//! Linux 块层 ioctl 的集中定义：常量、裸结构体与调用点都在本模块，
//! 其余模块经包装函数使用。UAPI 编号凡 libc 未导出的自持（标注内核头出处）。
//!
//! 架构守卫：手写编号取 asm-generic 的 _IOC 位域编码（x86_64/aarch64/arm/riscv64
//! 共用）；MIPS/PowerPC/sparc 的 _IOC 位域布局不同、同一编号含义不同，不受支持，
//! 在编译期拒绝而非运行期静默发错请求

#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "arm", target_arch = "riscv64"))
))]
compile_error!(
    "hand-coded ioctl numbers (BLKGETSIZE64/BLKRRPART/BLKPG) use the asm-generic \
     _IOC encoding, which only matches x86_64/aarch64/arm/riscv64"
);

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

/// 设备容量（字节）。BLKGETSIZE64 = _IOR(0x12, 114, u64)（内核 include/uapi/linux/fs.h，
/// 内容 u64）。libc 未导出该常量（0.2.139/0.2.186/0.2.189 实测），取 UAPI 定义自持
pub(crate) fn blkgetsize64(f: &File) -> io::Result<u64> {
    const BLKGETSIZE64: u64 = 0x8008_1272;
    let mut v: u64 = 0;
    // SAFETY: f 有效打开的 fd；内核仅写入 &mut v（输出方向 _IOR），调用期间指针有效
    let r = unsafe { libc::ioctl(f.as_raw_fd() as libc::c_int, BLKGETSIZE64 as libc::Ioctl, &mut v as *mut u64) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(v) }
}

/// 逻辑扇区大小（字节）。BLKSSZGET = _IO(0x12, 104)（include/uapi/linux/fs.h），
/// getter 返回 u32；libc 按架构导出正确编码（generic 0x1268、mips/powerpc/sparc 0x20001268）
pub(crate) fn blksszget(f: &File) -> io::Result<u32> {
    let mut v: u32 = 0;
    // SAFETY: f 有效打开的 fd；内核仅写入 &mut v，调用期间指针有效
    let r = unsafe { libc::ioctl(f.as_raw_fd() as libc::c_int, libc::BLKSSZGET as libc::Ioctl, &mut v as *mut u32) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(v) }
}

/// 通知内核重读分区表。BLKRRPART = _IO(0x12, 95)（include/uapi/linux/fs.h），
/// 无用户参数、内核不写回内存。errno 由调用方决定呈现方式（本层不丢弃）
pub(crate) fn blkrrpart(f: &File) -> io::Result<()> {
    const BLKRRPART: u64 = 0x125F;
    // SAFETY: f 有效打开的 fd；无指针参数
    let r = unsafe { libc::ioctl(f.as_raw_fd() as libc::c_int, BLKRRPART as libc::Ioctl, 0u32) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// include/uapi/linux/blkpg.h 的 blkpg_partition（start/length 单位 = 字节，
/// pno 标识分区，devname/volname 内核忽略）
#[repr(C)]
struct BlkpgPartition {
    start: i64,
    length: i64,
    pno: i32,
    devname: [u8; 64],
    volname: [u8; 64],
}

/// include/uapi/linux/blkpg.h 的 blkpg_ioctl_arg
#[repr(C)]
struct BlkpgIoctlArg {
    op: i32,
    flags: i32,
    datalen: i32,
    data: *mut BlkpgPartition,
}

/// BLKPG_RESIZE_PARTITION：对整盘 fd 调用（对分区 fd 调用内核报 -EINVAL），
/// pno 定位分区，start 固定为现值（改动 start 属于搬移，内核 bdev_resize_partition 拒绝）。
/// BLKPG = _IO(0x12, 105)；UAPI 语义见 online 模块头注释（block/ioctl.c 的 errno 分类）
pub(crate) fn blkpg_resize_partition(f: &File, start_bytes: u64, new_len_bytes: u64, pno: u32) -> io::Result<()> {
    const BLKPG: u64 = 0x1269; // _IO(0x12,105)
    const BLKPG_RESIZE_PARTITION: i32 = 3;
    // UAPI 字段是 i64：上游算出来的 u64 若已回绕（> i64::MAX），静默 as 会变成负数，
    // 内核看到的区间与请求南辕北辙——这类值不如当场拒掉
    let Ok(start) = i64::try_from(start_bytes) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("start {start_bytes} overflows BLKPG's signed range")));
    };
    let Ok(length) = i64::try_from(new_len_bytes) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("length {new_len_bytes} overflows BLKPG's signed range")));
    };
    let mut part = BlkpgPartition {
        start,
        length,
        pno: pno as i32,
        devname: [0; 64],
        volname: [0; 64],
    };
    let arg = BlkpgIoctlArg {
        op: BLKPG_RESIZE_PARTITION,
        flags: 0,
        datalen: size_of::<BlkpgPartition>() as i32,
        data: &mut part,
    };
    // SAFETY: arg/part 均为合法 repr(C) 栈对象、调用期间指针有效；BLKPG 编号与手写
    // blkpg_ioctl_arg 布局匹配
    let r = unsafe { libc::ioctl(f.as_raw_fd() as libc::c_int, BLKPG as libc::Ioctl, &arg) };
    if r == 0 {
        return Ok(());
    }
    let code = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    // errno 语义（内核 block/ioctl.c）：EACCES = 缺 CAP_SYS_ADMIN；EINVAL = 对分区 fd
    // 调用 / pno ≤ 0 / range 非法或溢出 / 未按逻辑块对齐 / 超出盘容量；
    // EBUSY = 与相邻分区重叠（block/partitions/core.c）
    let hint = match code {
        libc::EACCES => "requires root (CAP_SYS_ADMIN)",
        libc::EBUSY => "kernel rejected resize: overlaps another partition",
        libc::EINVAL => "kernel rejected resize (invalid pno/range, misaligned, or beyond capacity)",
        _ => "kernel rejected resize",
    };
    Err(io::Error::other(format!(
        "BLKPG_RESIZE_PARTITION failed: {hint} (errno {code})"
    )))
}