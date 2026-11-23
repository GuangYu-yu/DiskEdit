//! 内核视图：分区表重读、BLKPG 兜底与 btrfs FSID。
//!
//! ioctl 编号与结构体布局只在本文件可见。失败按语义分类上抛：内核不认识该请求
//! 归"能力不支持"，权限不足归"权限不足"，其余如实携带。

use std::path::Path;

use crate::platform::{PlatformError, PlatformResult};

/// `BLKRRPART`：让内核重读分区表。
const BLKRRPART: libc::c_ulong = 0x125F;
/// `BLKPG`：增删单个分区。
const BLKPG: libc::c_ulong = 0x1269;
const BLKPG_ADD_PARTITION: i32 = 1;
const BLKPG_DEL_PARTITION: i32 = 2;

#[repr(C)]
struct BlkpgPartition {
    start: i64,
    length: i64,
    pno: i32,
    devname: [libc::c_char; 64],
    volname: [libc::c_char; 64],
}

#[repr(C)]
struct BlkpgIoctlArg {
    op: i32,
    flags: i32,
    datalen: i32,
    data: *mut libc::c_void,
}

/// 请内核重读分区表。
pub fn reread(path: &Path) -> PlatformResult<()> {
    let file = std::fs::File::open(path).map_err(|error| PlatformError::io(path, error))?;
    use std::os::fd::AsRawFd;
    // SAFETY: fd 在调用期间有效；BLKRRPART 不接受额外参数。
    let result = unsafe { libc::ioctl(file.as_raw_fd(), BLKRRPART) };
    if result == 0 {
        Ok(())
    } else {
        Err(classify_ioctl(path, std::io::Error::last_os_error()))
    }
}

/// 用 `BLKPG` 增加一个分区；`start` 与 `length` 以字节计。
pub fn add_partition(path: &Path, number: u32, start: u64, length: u64) -> PlatformResult<()> {
    let number = i32::try_from(number)
        .map_err(|_| PlatformError::parse(format!("分区号 {number} 超出 i32 范围")))?;
    let start = i64::try_from(start).map_err(|_| PlatformError::parse("分区起点超出 i64 范围"))?;
    let length =
        i64::try_from(length).map_err(|_| PlatformError::parse("分区长度超出 i64 范围"))?;
    let mut partition = BlkpgPartition {
        start,
        length,
        pno: number,
        devname: [0; 64],
        volname: [0; 64],
    };
    let mut arg = BlkpgIoctlArg {
        op: BLKPG_ADD_PARTITION,
        flags: 0,
        datalen: std::mem::size_of::<BlkpgPartition>() as i32,
        data: (&mut partition as *mut BlkpgPartition).cast(),
    };
    run_blkpg(path, &mut arg)
}

/// 用 `BLKPG` 删除一个分区。
pub fn delete_partition(path: &Path, number: u32) -> PlatformResult<()> {
    let number = i32::try_from(number)
        .map_err(|_| PlatformError::parse(format!("分区号 {number} 超出 i32 范围")))?;
    let mut partition = BlkpgPartition {
        start: 0,
        length: 0,
        pno: number,
        devname: [0; 64],
        volname: [0; 64],
    };
    let mut arg = BlkpgIoctlArg {
        op: BLKPG_DEL_PARTITION,
        flags: 0,
        datalen: std::mem::size_of::<BlkpgPartition>() as i32,
        data: (&mut partition as *mut BlkpgPartition).cast(),
    };
    run_blkpg(path, &mut arg)
}

fn run_blkpg(path: &Path, arg: &mut BlkpgIoctlArg) -> PlatformResult<()> {
    let file = std::fs::File::open(path).map_err(|error| PlatformError::io(path, error))?;
    use std::os::fd::AsRawFd;
    // SAFETY: arg 指向本栈帧内的结构体，调用期间有效；内核按 datalen 读取。
    let result = unsafe { libc::ioctl(file.as_raw_fd(), BLKPG, arg as *mut BlkpgIoctlArg) };
    if result == 0 {
        Ok(())
    } else {
        Err(classify_ioctl(path, std::io::Error::last_os_error()))
    }
}

/// 按字节区间批量补建分区；内核已持有同一分区视为成功。
///
/// 这是 `partx` 不可用时的兜底：只补建盘上已有分区，不删除、不重排。
pub fn sync_partitions(path: &Path, partitions: &[(u32, u64, u64)]) -> PlatformResult<()> {
    for &(number, start, length) in partitions {
        match add_partition(path, number, start, length) {
            Ok(()) | Err(PlatformError::Busy { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn classify_ioctl(path: &Path, error: std::io::Error) -> PlatformError {
    match error.raw_os_error() {
        Some(libc::ENOTTY) | Some(libc::EINVAL) => {
            PlatformError::unsupported(format!("{} 不接受该内核请求：{error}", path.display()))
        }
        Some(libc::EACCES) | Some(libc::EPERM) => {
            PlatformError::denied(format!("{} 的内核请求被拒绝：{error}", path.display()))
        }
        Some(libc::EBUSY) => PlatformError::busy(format!("{} 正被占用：{error}", path.display())),
        _ => PlatformError::io(path, error),
    }
}

/// 从 `btrfs filesystem show` 输出中取 FSID。
pub fn parse_fsid(text: &str) -> Option<[u8; 16]> {
    let raw = text
        .lines()
        .find_map(|line| line.split_once("fsid:").map(|(_, rest)| rest.trim()))?;
    parse_uuid(raw)
}

fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    let hex: String = text
        .chars()
        .take_while(|ch| ch.is_ascii_hexdigit() || *ch == '-')
        .collect();
    let digits: String = hex.chars().filter(|ch| *ch != '-').collect();
    if digits.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (index, chunk) in digits.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        bytes[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fsid_line() {
        let text = "Label: none  uuid: 1111\n\tTotal devices 1 FS bytes used 1.00MiB\n\tfsid:    9f7c1e2a-3b4d-4e5f-8a9b-0c1d2e3f4a5b\n";
        let fsid = parse_fsid(text).expect("可解析");
        assert_eq!(fsid[0], 0x9f);
        assert_eq!(fsid[15], 0x5b);
    }

    #[test]
    fn rejects_incomplete_uuid() {
        assert!(parse_fsid("fsid 1234").is_none());
        assert!(parse_uuid("gggggggg-gggg-gggg-gggg-gggggggggggg").is_none());
    }
}
