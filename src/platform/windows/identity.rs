//! 镜像的文件身份：卷序列号与文件索引。
//!
//! 身份必须跨进程、跨次调用稳定，并且在同一路径被换成另一个文件时改变。Win32
//! 的 `BY_HANDLE_FILE_INFORMATION` 同时给出卷序列号与文件索引（卷内唯一的
//! 64 位编号），二者合起来满足这两条；路径、文件大小与时间戳都不参与。

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::io::AsRawHandle;

/// `FILETIME`：两个 32 位分量。
#[repr(C)]
struct FileTime {
    low: u32,
    high: u32,
}

/// `BY_HANDLE_FILE_INFORMATION`：字段顺序与 Win32 定义一致。
#[repr(C)]
struct ByHandleFileInformation {
    file_attributes: u32,
    creation_time: FileTime,
    last_access_time: FileTime,
    last_write_time: FileTime,
    volume_serial_number: u32,
    file_size_high: u32,
    file_size_low: u32,
    number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetFileInformationByHandle(
        handle: *mut c_void,
        information: *mut ByHandleFileInformation,
    ) -> i32;
}

/// 卷序列号与文件索引；调用失败时返回系统错误，不返回占位值。
pub fn file_id(file: &File) -> io::Result<(u32, u64)> {
    let mut information = ByHandleFileInformation {
        file_attributes: 0,
        creation_time: FileTime { low: 0, high: 0 },
        last_access_time: FileTime { low: 0, high: 0 },
        last_write_time: FileTime { low: 0, high: 0 },
        volume_serial_number: 0,
        file_size_high: 0,
        file_size_low: 0,
        number_of_links: 0,
        file_index_high: 0,
        file_index_low: 0,
    };
    // SAFETY: 句柄来自存活的文件；调用只写 `information`，其布局与大小按
    // Win32 的 `BY_HANDLE_FILE_INFORMATION` 声明。
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let index =
        (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low);
    Ok((information.volume_serial_number, index))
}
