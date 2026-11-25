#!/usr/bin/env python3
"""提交外足迹守卫：重写只在 `rebuild/` 内落笔。

基线是 `plan/footprint.base` 所在的提交。基线之后：

- `git diff --name-only <base>..HEAD` 里的每个路径都必须在 `rebuild/` 下；
- 工作区（含未跟踪文件）里的每个路径同样必须在 `rebuild/` 下。

基线提交自身是裁决提交，豁免于本检查；这条豁免只发生一次，且由基线机制固定。

用法：

    python plan/instruments/footprint.py
    python plan/instruments/footprint.py --base <sha>     # 临时覆盖
    python plan/instruments/footprint.py --selftest
"""
from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

ALLOWED_ROOT = "rebuild"
BASE_FILE = "rebuild/plan/footprint.base"


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


def allowed(path: str) -> bool:
    return path == ALLOWED_ROOT or path.startswith(ALLOWED_ROOT + "/")


def committed_paths(root: Path, base: str) -> list[str]:
    output = run_git(root, "diff", "--name-only", "--no-renames", f"{base}..HEAD")
    return [line for line in output.splitlines() if line]


def worktree_paths(root: Path) -> list[str]:
    output = run_git(root, "status", "--porcelain=v1", "-z")
    entries = output.split("\0")
    paths: list[str] = []
    index = 0
    while index < len(entries):
        entry = entries[index]
        index += 1
        if not entry:
            continue
        status, path = entry[:2], entry[3:]
        paths.append(path)
        if status[0] in "RC":
            # 重命名/复制在 -z 输出里紧跟一条原路径；一并纳入检查。
            if index < len(entries) and entries[index]:
                paths.append(entries[index])
                index += 1
    return paths


def check(base: str | None) -> int:
    root = repo_root()
    resolved = base or base_commit(root)
    if not resolved:
        print(f"未找到基线：{BASE_FILE} 还没有提交记录", file=sys.stderr)
        return 2
    ancestor = subprocess.run(
        ["git", "merge-base", "--is-ancestor", resolved, "HEAD"], cwd=root, check=False
    )
    if ancestor.returncode != 0:
        print(f"基线 {resolved} 不是 HEAD 的祖先", file=sys.stderr)
        return 2

    violations: list[tuple[str, str]] = []
    for path in committed_paths(root, resolved):
        if not allowed(path):
            violations.append(("提交", path))
    for path in worktree_paths(root):
        if not allowed(path):
            violations.append(("工作区", path))

    if violations:
        print(f"提交外足迹：基线 {resolved[:12]}..HEAD")
        for where, path in violations:
            print(f"  越界（{where}）：{path}")
        print(f"只允许 {ALLOWED_ROOT}/ 下的路径")
        return 1

    count = len(committed_paths(root, resolved))
    print(f"提交外足迹：基线 {resolved[:12]}..HEAD 与工作区均未越界")
    print(f"提交内改动 {count} 条")
    return 0


def selftest() -> int:
    cases = [
        ("rebuild/src/app/mod.rs", True),
        ("rebuild", True),
        ("src/cmd/info.rs", False),
        ("Cargo.lock", False),
        ("rebuildish/file", False),
        ("README.md", False),
    ]
    failed = 0
    for path, expected in cases:
        if allowed(path) != expected:
            print(f"selftest 失败：{path} 期望 {expected}")
            failed += 1
    if failed:
        return 1
    print(f"footprint selftest：{len(cases)} 条通过")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="提交外足迹守卫")
    parser.add_argument("--base", help="覆盖基线提交")
    parser.add_argument("--selftest", action="store_true", help="只跑内部自检")
    args = parser.parse_args()
    if args.selftest:
        return selftest()
    try:
        return check(args.base)
    except RuntimeError as error:
        print(f"footprint: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
