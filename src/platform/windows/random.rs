//! 宿主随机字节：`BCryptGenRandom` 配系统首选随机源。
//!
//! 新表的 GUID 与临时目录后缀都取自这里；失败时返回错误，不退回可预测的种子。

use std::ffi::c_void;
use std::io;

use crate::platform::{PlatformError, PlatformResult};

/// 让 `BCryptGenRandom` 使用系统首选随机源。
const USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
/// `NTSTATUS` 的成功值。
const STATUS_SUCCESS: i32 = 0;

#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptGenRandom(algorithm: *mut c_void, buffer: *mut u8, length: u32, flags: u32) -> i32;
}

pub fn bytes(len: usize) -> PlatformResult<Vec<u8>> {
    let mut buffer = vec![0u8; len];
    let length = u32::try_from(len)
        .map_err(|_| PlatformError::parse("the requested entropy length exceeds u32"))?;
    if length == 0 {
        return Ok(buffer);
    }
    // SAFETY: 缓冲区长度与传入的长度一致；算法句柄为空表示使用系统首选随机源。
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buffer.as_mut_ptr(),
            length,
            USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != STATUS_SUCCESS {
        return Err(PlatformError::Io {
            path: "BCryptGenRandom".to_owned(),
            message: io::Error::from_raw_os_error(status).to_string(),
        });
    }
    Ok(buffer)
}
