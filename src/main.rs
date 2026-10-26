//! diskedit — 磁盘编辑工具：镜像文件与块设备统一为按偏移读写的字节存储。
//! 命令面与退出码契约：0=完成（分区表写入经读回复核、内核视图已刷新；FS 步以外部工具
//! 的退出码为准；缩容另做尺寸回读，回读不出时报 20）/10=拒绝执行（未写盘，成因在请求与现状不匹配）/
//! 20=部分完成（后置步骤未做，或表已写但内核分区视图过期）/
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
mod geometry;
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
mod targetlock;
mod transaction;

use std::process::ExitCode;

use crate::args::{parse_args, refuse_unconsumed_flags};
use crate::support::drop_journal;

fn main() -> ExitCode {
    let (cmd, a) = parse_args();
    refuse_unconsumed_flags(&cmd, &a);
    let result = cmd::dispatch(&cmd, &a);
    // 布局已完成（Applied）且没有已知待办 ⇒ 撤销窗口可以关闭，删除 undo journal。
    // 判据不能看退出码：20 同时覆盖"待办非空"（保留 journal 供 undo 撤销表写入）与
    // "仅内核视图过期"（事务已完成，必须关闭——否则下一次写命令会被自己残留的
    // journal 挡成 busy）。仅限命令表里标为开 journal 的命令：不开窗口的命令成功时
    // 若也删，会把先前失败操作留下的 journal 误清掉。删除动作须容忍文件不存在；
    // 块设备的在线路径（sfdisk、FS 工具）用自己的 fd 写盘、不经过本工具的 journal，
    // 那些写入本就不在回放范围内
    let mut code = result.code();
    if cmd::opens_undo_journal(&cmd) && result.closes_journal() && !drop_journal() {
        // 该删的恢复文件没删掉：事务的收尾没有完成，不能按完整成功退出
        code = crate::outcome::EXIT_PARTIAL;
    }
    // 资源释放待办（`Drop` 里发现的，例如 loop 未解绑）不能只留在 stderr：它同样是一条
    // 未完成的后置条件。只在完整成功时降级——失败路径的码已经表达了更严重的事实
    let release_leaks = crate::fsops::take_release_leaks();
    if !release_leaks.is_empty() && code == crate::outcome::EXIT_OK {
        eprintln!(
            "note: {} cleanup step(s) did not complete — this run left resources behind (see the warnings above)",
            release_leaks.len()
        );
        code = crate::outcome::EXIT_PARTIAL;
    }
    ExitCode::from(code)
}
