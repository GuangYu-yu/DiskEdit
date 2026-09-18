//! 命令实现：每条命令的参数消费、逻辑与帮助文本同处一个模块。

pub(crate) mod fs;
pub(crate) mod info;
pub(crate) mod layout;
pub(crate) mod plan;
pub(crate) mod resize;
pub(crate) mod undo;

use crate::args::{help_cmd, usage, Args};

/// help <主题> 的文本路由：别名（del/apply 等）与归属模块在此收敛
pub(crate) fn help_text(name: &str) -> Option<&'static str> {
    match name {
        "info" => Some(info::HELP),
        "resize" => Some(resize::HELP),
        "move" => Some(layout::HELP_MOVE),
        "copy" => Some(layout::HELP_COPY),
        "create" => Some(layout::HELP_CREATE),
        "delete" | "del" => Some(layout::HELP_DELETE),
        "set" => Some(fs::HELP_SET),
        "check" => Some(fs::HELP_CHECK),
        "mkfs" => Some(fs::HELP_MKFS),
        "resizefs" => Some(fs::HELP_RESIZEFS),
        "undo" => Some(undo::HELP),
        "new" => Some(layout::HELP_NEW),
        "add" => Some(layout::HELP_ADD),
        "resize-part" => Some(layout::HELP_RESIZE_PART),
        "plan" | "apply" => Some(plan::HELP),
        _ => None,
    }
}

/// 命令分派。usage/help_cmd 不返回（直接退出进程）
pub(crate) fn dispatch(cmd: &str, a: &Args) -> u8 {
    match cmd {
        "help" | "--help" | "-h" => match a.pos.first() {
            Some(t) => help_cmd(t),
            None => usage(),
        },
        "info" => info::cmd_info(a),
        "resize" => resize::cmd_resize(a),
        "move" => layout::cmd_move(a),
        "create" => layout::cmd_create(a),
        "set" => fs::cmd_set(a),
        "mkfs" => fs::cmd_mkfs(a),
        "resizefs" => fs::cmd_resizefs(a),
        "check" => fs::cmd_check(a),
        "undo" => undo::cmd_undo(a),
        "new" => layout::cmd_new(a),
        "add" => layout::cmd_add(a),
        "del" | "delete" => layout::cmd_del(a),
        "resize-part" => layout::cmd_resize_part(a),
        "copy" => layout::cmd_copy(a),
        "plan" | "apply" => plan::cmd_plan_apply(cmd, a),
        _ => usage(),
    }
}