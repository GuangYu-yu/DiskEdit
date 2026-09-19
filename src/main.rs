//! diskedit — 磁盘编辑工具：镜像文件与块设备统一为按偏移读写的字节存储。
//! 命令面与退出码契约：0=完成（经读回复核，且内核视图已刷新）/10=拒绝执行（未写盘，
//! 成因在请求与现状不匹配）/20=部分完成（后置步骤未做，或表已写但内核分区视图过期）/
//! 30=基础设施失败（盘内容/环境故障且未写盘，或写盘后失败）。
//! 20 的两个成因正交：见 outcome::Applied 的 pending 与 kernel_sync；
//! 30 的两个成因也正交：见 outcome::{Outcome::Infra, Outcome::Failed}
//!
//! 结构：args=命令行解析与旗标契约；support=命令层共用支撑件；cmd/=<每命令一个模块>；
//! ioctl=Linux 块层 ioctl 集中地；其余为领域层（table/movepart/online/fsid/fsops/
//! gpt_policy/lvm/outcome/dev）。

mod args;
mod cmd;
mod dev;
mod fsid;
mod fsops;
mod gpt_policy;
#[cfg(target_os = "linux")]
mod ioctl;
#[cfg(target_os = "linux")]
mod lvm;
mod outcome;
#[cfg(target_os = "linux")]
mod online;
mod movepart;
mod support;
mod table;

use std::process::ExitCode;

use crate::args::{parse_args, refuse_unconsumed_flags};
use crate::support::{drop_journal, EXIT_OK};

fn main() -> ExitCode {
    let (cmd, a) = parse_args();
    refuse_unconsumed_flags(&cmd, &a);
    let code = cmd::dispatch(&cmd, &a);
    // 成功完成 ⇒ 撤销窗口已关闭，删除 undo journal（失败/中断时保留，供续传或回滚）。
    // 仅限命令表里标为开 journal 的命令：不开窗口的命令成功时若也删，会把先前失败
    // 操作留下的 journal 误清掉。删除动作须容忍文件不存在——块设备的在线路径会在别处写盘
    if code == EXIT_OK && cmd::opens_undo_journal(&cmd) {
        drop_journal(&a);
    }
    ExitCode::from(code)
}