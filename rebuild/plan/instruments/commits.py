#!/usr/bin/env python3
"""提交信息守卫：每条提交的主题行都要能一眼看出范围。

规范（`plan/04-任务与验收.md` 与 `plan/05-静态自检.md` 的机器化）：

    <前缀>[ <范围>]: <做了什么>

- 前缀是 `S<数字>`、`T<数字>`，或 `plan`、`docs`、`ci`、`chore`、`build`、`test`、`refactor`；
- 冒号后必须有非空说明；
- 主题不得出现 `wip`、`tmp`、`fix`、`update`、`杂项`（括号包裹不算例外）。

基线是 `plan/footprint.base` 所在的提交；基线之后（不含基线）的每条非合并提交都检查。

用法：

    python plan/instruments/commits.py
    python plan/instruments/commits.py --base <sha>
    python plan/instruments/commits.py --selftest
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

BASE_FILE = "rebuild/plan/footprint.base"
SUBJECT_RE = re.compile(
    r"^(?:S\d+|T\d+)(?: [^:\n]{1,60})?: \S.*$"
    r"|^(?:plan|docs|ci|chore|build|test|refactor)(?: [^:\n]{1,60})?: \S.*$"
)
FORBIDDEN_RE = re.compile(r"\b(?:wip|tmp|fix|update)\b|杂项", re.IGNORECASE)


def run_git(root: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args], cwd=root, text=True, capture_output=True, check=False
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or f"git {' '.join(args)} 失败")
    return result.stdout


def repo_root() -> Path:
    here = Path(__file__).resolve().parent
    output = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        cwd=here,
        text=True,
        capture_output=True,
        check=True,
    ).stdout
    return Path(output.strip())


def base_commit(root: Path) -> str:
    return run_git(root, "log", "-1", "--format=%H", "--", BASE_FILE).strip()


def subject_of(root: Path, commit: str) -> str:
    return run_git(root, "log", "-1", "--format=%s", commit).strip()


def problems(subject: str) -> list[str]:
    found = []
    if not SUBJECT_RE.match(subject):
        found.append("主题不符合 `<前缀>[ <范围>]: <做了什么>`")
    forbidden = FORBIDDEN_RE.search(subject)
    if forbidden:
        found.append(f"主题含禁用词 `{forbidden.group(0)}`")
    return found


def check(base: str | None) -> int:
    root = repo_root()
    resolved = base or base_commit(root)
    if not resolved:
        print(f"未找到基线：{BASE_FILE} 还没有提交记录", file=sys.stderr)
        return 2
    commits = run_git(root, "rev-list", "--no-merges", f"{resolved}..HEAD").split()
    failures = []
    for commit in commits:
        subject = subject_of(root, commit)
        for problem in problems(subject):
            failures.append((commit[:12], subject, problem))
    if failures:
        print(f"提交信息：基线 {resolved[:12]}..HEAD 共 {len(commits)} 条")
        for commit, subject, problem in failures:
            print(f"  {commit}  {subject}")
            print(f"    {problem}")
        return 1
    print(f"提交信息：基线 {resolved[:12]}..HEAD 共 {len(commits)} 条，全部合规")
    return 0


def selftest() -> int:
    good = [
        "S2 new: 覆盖门、最小容量与保护 MBR",
        "S1 读侧纵切: info/inspect/records/check 重写",
        "T13 在线执行: 挂载点 resizefs 经平台反查",
        "plan: 补齐全集门与文档一致性",
        "chore: 移除仓库根 Cargo.lock",
    ]
    bad = [
        "wip",
        "update: 一些改动",
        "S2 fix: 修补",
        "S2 new",
        "fix: 杂项",
        "随便写点",
    ]
    failed = 0
    for subject in good:
        if problems(subject):
            print(f"selftest 失败：应当合规却报错 -> {subject}")
            failed += 1
    for subject in bad:
        if not problems(subject):
            print(f"selftest 失败：应当报错却放行 -> {subject}")
            failed += 1
    if failed:
        return 1
    print(f"commits selftest：{len(good) + len(bad)} 条通过")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="提交信息守卫")
    parser.add_argument("--base", help="覆盖基线提交")
    parser.add_argument("--selftest", action="store_true", help="只跑内部自检")
    args = parser.parse_args()
    if args.selftest:
        return selftest()
    try:
        return check(args.base)
    except RuntimeError as error:
        print(f"commits: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
