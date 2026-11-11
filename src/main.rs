//! diskedit — 磁盘编辑工具：镜像文件与块设备统一为按偏移读写的字节存储。
//! 命令面与退出码契约：0=完成（分区表写入经读回复核、内核视图已刷新；FS 步以外部工具
//! 的退出码为准；缩容另做尺寸回读，回读不出时按部分完成报 5）/1=内部错误/2=参数或命令
//! 语法错误/3=策略拒绝或前置条件不满足/4=操作失败且确认无持久化修改/5=部分完成或恢复
//! 待处理/6=平台或能力不支持。
//! 5 的两个成因正交：见 outcome::Applied 的 pending 与 kernel_sync 以及 RunOutcome::Partial；
//! 退出码只在 RunOutcome::exit_code 一处映射
//!
//! 结构：cli=命令 schema、解析与类型化请求；support=命令层共用支撑件；cmd/=<每命令一个模块>；
//! assembly=计划期装配（只读目标 → 表步骤装配）；ioctl=Linux 块层 ioctl 集中地；
//! 其余为领域层（table/movepart/online/fsid/fsops/gpt_policy/lvm/outcome/dev）。

mod assembly;
mod cli;
mod cmd;
mod dev;
mod docs;
mod domain;
mod execution;
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
mod platform;
mod planner;
mod recovery;
mod support;
mod table;
mod targetlock;
#[cfg(test)]
mod testdir;
#[cfg(test)]
mod testrng;

use std::process::ExitCode;

use crate::outcome::{EXIT_INTERNAL, EXIT_OK, EXIT_PARTIAL};

/// panic 走默认钩子（原样打印消息与 backtrace），结论交给同一个归责点：本次已写则
/// 落部分完成（5）并给出恢复指引；从未写过则是内部错误（1）
fn main() -> ExitCode {
    match std::panic::catch_unwind(run) {
        Ok(code) => ExitCode::from(code),
        Err(_) => {
            let outcome = crate::outcome::RunOutcome::internal("the process panicked")
                .reconcile_with_writes();
            if matches!(outcome, crate::outcome::RunOutcome::Partial { .. }) {
                outcome.report();
                ExitCode::from(EXIT_PARTIAL)
            } else {
                ExitCode::from(EXIT_INTERNAL)
            }
        }
    }
}

fn run() -> u8 {
    let (def, request) = cli::parse();
    let mut result = cmd::dispatch(def, &request);
    // 收尾步骤的失败不直接改码：只把"已完整应用"的结果降级成部分完成（5），
    // 降级与映射都落在 RunOutcome 上
    let mut cleanup_failure: Option<String> = None;
    // 资源释放待办（`Drop` 里发现的，例如 loop 未解绑）不能只留在 stderr：它同样是一条
    // 未完成的后置条件。只在完整成功时降级——失败路径的码已经表达了更严重的事实
    let release_leaks = crate::fsops::take_release_leaks();
    if !release_leaks.is_empty() && result.code() == EXIT_OK {
        eprintln!(
            "note: {} cleanup step(s) did not complete — this run left resources behind (see the warnings above)",
            release_leaks.len()
        );
        let cause = format!("{} cleanup step(s) did not complete", release_leaks.len());
        result = result.degrade_partial(cause.clone());
        cleanup_failure = Some(cause);
    }
    // 返回结果同样要归责：命令内部的错误出口已在各自打印前归责（bail_fail /
    // report_final），这里兜住直接带着旧结论回到 main 的路径，改判时补一次呈现
    let before = result.code();
    result = result.reconcile_with_writes();
    if result.code() != before
        && let cmd::CmdResult::RunOutcome(o) = &result
    {
        o.report();
    }
    let code = result.code();
    // 收尾失败发生在命令自报成功之后，命令自己没有打印过 5 的指引，这里补上
    if code == EXIT_PARTIAL
        && let Some(cause) = cleanup_failure
    {
        crate::outcome::RunOutcome::partial_postcondition(cause, Vec::new()).report();
    }
    code
}
