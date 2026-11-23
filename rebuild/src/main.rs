//! 进程入口：参数交给应用层，退出码只在此处转成 [`std::process::ExitCode`]。

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(diskedit::app::run(std::env::args_os()))
}
