//! 载体会话入口：`__worker` 是宿主与 guest 之间的协议入口，不属于命令行接口。

use super::diskedit;

/// 工作进程只能由宿主以带令牌的管道拉起。缺少令牌时拒绝启动，且不能退到
/// "把 stdin 当命令文本"的老式交互上
#[test]
fn worker_requires_token() {
    for token in [None, Some("")] {
        let mut cmd = diskedit();
        cmd.arg("__worker");
        match token {
            Some(value) => {
                cmd.env("DISKEDIT_WORKER_TOKEN", value);
            }
            None => {
                cmd.env_remove("DISKEDIT_WORKER_TOKEN");
            }
        }
        let out = cmd.output().unwrap();
        assert_eq!(out.status.code(), Some(3), "token {token:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("DISKEDIT_WORKER_TOKEN"), "token {token:?}: {stderr}");
    }
}

/// `__worker` 不出现在帮助与命令清单里：它没有用户可填的参数，
/// 列出来只会让人以为可以手动运行
#[test]
fn worker_not_in_help() {
    let mut inputs = vec![vec!["--help"], vec!["help"]];
    for name in ["resize", "mkfs", "recover"] {
        inputs.push(vec!["help", name]);
    }
    for args in inputs {
        let out = diskedit().args(&args).output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!text.contains("__worker"), "`diskedit {}` mentions __worker:\n{text}", args.join(" "));
    }
}
