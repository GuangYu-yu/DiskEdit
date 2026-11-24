//! 控制台通道：来宾经串口跑协议时关掉行规程的回显与按行缓冲。
//!
//! 串口连接的终端默认处于规范模式：输入被回显回串口，宿主会把自己的步骤帧
//! 当成来宾回复。这里只清掉回显与规范模式，保留其余设置。

use crate::platform::{PlatformError, PlatformResult};

/// 把标准输入切到原始模式；不是终端时什么都不做。
pub fn raw_stdin() -> PlatformResult<()> {
    // SAFETY: isatty/tcgetattr/tcsetattr 只读写调用方提供的 termios 结构，标准
    // 输入在进程生命周期内保持打开。
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return Ok(());
    }
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut term) } != 0 {
        return Err(PlatformError::io(
            "/dev/stdin",
            std::io::Error::last_os_error(),
        ));
    }
    term.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHOE | libc::ECHOK | libc::ECHONL);
    term.c_cc[libc::VMIN] = 1;
    term.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term) } != 0 {
        return Err(PlatformError::io(
            "/dev/stdin",
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}
