#!/usr/bin/env python3
"""门禁包装器：替命令下结论，口径只有一条——没跑够数就是红。

解决的问题：测试在前置条件缺失时一条用例都不跑，仍然以退出码 0 结束，看起来是绿的。

子命令：

  run      给一条命令下结论。
           python gate.py run --name G6 --log plan/evidence/G6.log --kind test --min-passed 39 \
               --require-env DISKEDIT_BASELINE -- cargo test --test equivalence

  all      按 `plan/gates.tsv` 逐条执行门禁表里的命令，记录写进 --evidence 目录。
           python gate.py all --plan plan/gates.tsv --who model     # 模型跑的那部分
           python gate.py all --plan plan/gates.tsv --who human     # 需要镜像、特权、基线二进制的那部分
           python gate.py all --plan plan/gates.tsv --who all --only G3,G5

  status   列出已有记录；任何一份缺失或未通过即退出 1。
           python gate.py status --evidence plan/evidence --gates plan/gates.tsv

  selftest 用伪造输出验证它自己：每一条该红的都必须红。

门禁表格式（制表符分隔，7 列，# 开头为注释）：
  门禁名  谁跑(model|human)  类型(test|lint|build)  最少通过数  必需环境变量(逗号分隔)  必需文件(逗号分隔)  命令

判定（test）：退出码为 0；至少一行 `test result:`；通过数不少于最少通过数；失败数为 0；
被忽略数不超过预算（默认 0）；`NotApplicable` 出现次数不超过预算（默认 0）；
必需的环境变量非空、必需的文件存在。前置条件缺失时命令不会被执行。
判定（lint / build）：只看退出码与前置条件。

退出码：0 通过；1 未通过（原因逐条打印）；2 包装器自身无法下结论。
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import io
import json
import os
import re
import shlex
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

RESULT_RE = re.compile(
    r"test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out")
KINDS = ("test", "lint", "build")


def judge(kind: str, exit_code: int, output: str, min_passed: int,
          ignored_budget: int, na_budget: int) -> tuple[bool, list[str], dict]:
    reasons = []
    counts = {"result_lines": 0, "passed": 0, "failed": 0, "ignored": 0,
              "not_applicable": output.count("NotApplicable")}
    if exit_code != 0:
        reasons.append(f"命令退出码为 {exit_code}")
    if kind == "test":
        for m in RESULT_RE.finditer(output):
            counts["result_lines"] += 1
            counts["passed"] += int(m.group(2))
            counts["failed"] += int(m.group(3))
            counts["ignored"] += int(m.group(4))
        if counts["result_lines"] == 0:
            reasons.append("输出里没有任何一行 `test result:`，说明一个测试目标都没有执行")
        if counts["passed"] < min_passed:
            reasons.append(f"通过数 {counts['passed']} 少于要求的 {min_passed}")
        if counts["failed"]:
            reasons.append(f"失败数 {counts['failed']}")
        if counts["ignored"] > ignored_budget:
            reasons.append(f"被忽略的用例 {counts['ignored']} 个，超过预算 {ignored_budget}")
    if counts["not_applicable"] > na_budget:
        reasons.append(f"`NotApplicable` 出现 {counts['not_applicable']} 次，超过预算 {na_budget}")
    return not reasons, reasons, counts


def preconditions(envs: list[str], files: list[str]) -> list[str]:
    problems = []
    for name in envs:
        if not os.environ.get(name, "").strip():
            problems.append(f"前置条件缺失：环境变量 {name} 未设置或为空")
    for path in files:
        if not Path(path).exists():
            problems.append(f"前置条件缺失：文件不存在 {path}")
    return problems


def commit_id(cwd: Path) -> str:
    try:
        out = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=cwd,
                             capture_output=True, text=True, timeout=10)
        return out.stdout.strip() if out.returncode == 0 else "unknown"
    except (OSError, subprocess.SubprocessError):
        return "unknown"


def tree_fingerprint(root: Path) -> str:
    """工作树内容指纹：与提交号无关，检测记录之后是否有人改过受管文件。"""
    h = hashlib.sha256()
    for p in sorted(root.rglob("*")):
        rel = p.relative_to(root)
        if ".git" in rel.parts or "target" in rel.parts or "__pycache__" in rel.parts:
            continue
        if any(part.startswith(".") for part in rel.parts):
            continue
        if rel.parts[:2] == ("plan", "evidence"):
            continue
        if not p.is_file():
            continue
        h.update(str(rel).encode("utf-8"))
        try:
            if p.stat().st_size <= 2 * 1024 * 1024:
                h.update(p.read_bytes())
            else:
                h.update(str(p.stat().st_size).encode("utf-8"))
        except OSError:
            pass
    return h.hexdigest()[:16]


def run_gate(name: str, kind: str, cmd: list[str], log: Path, min_passed: int | None = 1,
             ignored_budget: int = 0, na_budget: int = 0, require_env: list[str] | None = None,
             require_file: list[str] | None = None, cwd: str | None = None) -> bool:
    log.parent.mkdir(parents=True, exist_ok=True)
    problems = preconditions(require_env or [], require_file or [])
    unpinned = kind == "test" and min_passed is None
    if unpinned:
        problems.append("最少通过数未登记：该门禁还没有登记数，等同未执行")
    output, code = "", None
    if not problems:
        try:
            proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True,
                                  errors="replace", env=os.environ.copy())
            output, code = proc.stdout + "\n" + proc.stderr, proc.returncode
        except OSError as e:
            problems.append(f"命令无法执行：{e}")
    ok, reasons, counts = judge(kind, code if code is not None else -1, output,
                                min_passed if min_passed is not None else 1, ignored_budget, na_budget)
    reasons = problems + reasons
    ok = ok and not problems
    log.write_text(output if output else "(命令未执行)\n", encoding="utf-8")
    record = {
        "gate": name, "kind": kind, "command": cmd, "exit_code": code,
        "commit": commit_id(Path(cwd) if cwd else Path.cwd()),
        "tree": tree_fingerprint(Path(cwd) if cwd else Path.cwd()),
        "at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "counts": counts, "ok": ok, "reasons": reasons, "log": str(log),
    }
    log.with_suffix(log.suffix + ".json").write_text(
        json.dumps(record, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    print(f"[{name}] {'通过' if ok else '未通过'}  通过 {counts['passed']} 失败 {counts['failed']} "
          f"忽略 {counts['ignored']} 不适用 {counts['not_applicable']}  记录 {log}.json")
    for r in reasons:
        print("  红因：" + r)
    return ok


def read_plan(path: Path) -> list[dict]:
    rows = []
    for n, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        cols = [c.strip() for c in line.split("\t")]
        if len(cols) != 7:
            raise ValueError(f"{path}:{n} 应有 7 列，实得 {len(cols)}")
        name, who, kind, min_passed, envs, files, command = cols
        if who not in ("model", "human"):
            raise ValueError(f"{path}:{n} 第二列只能是 model 或 human")
        if kind not in KINDS:
            raise ValueError(f"{path}:{n} 第三列只能是 {KINDS}")
        # 最少通过数没登记实数（写作“待钉”）时，这一行单独判红，不拖累其他门禁
        pinned = int(min_passed) if min_passed.isdigit() else None
        if pinned is not None and pinned < 1:
            raise ValueError(f"{path}:{n} 最少通过数不得小于 1")
        rows.append({
            "name": name, "who": who, "kind": kind, "min_passed": pinned,
            "require_env": [x for x in envs.split(",") if x],
            "require_file": [x for x in files.split(",") if x],
            "command": shlex.split(command),
        })
    names = [r["name"] for r in rows]
    if not rows:
        raise ValueError(f"{path} 里没有任何门禁")
    if len(names) != len(set(names)):
        raise ValueError(f"{path} 门禁名重复：{sorted({n for n in names if names.count(n) > 1})}")
    return rows


def run_all(a: argparse.Namespace) -> int:
    try:
        rows = read_plan(Path(a.plan))
    except (OSError, ValueError) as e:
        print(f"门禁表无法读取：{e}", file=sys.stderr)
        return 2
    only = {x.strip() for x in a.only.split(",")} if a.only else None
    exclude = {x.strip() for x in a.exclude.split(",")} if getattr(a, "exclude", None) else set()
    chosen = [r for r in rows if (a.who == "all" or r["who"] == a.who)
              and (not only or r["name"] in only) and r["name"] not in exclude]
    if not chosen:
        print("门禁表里没有符合条件的行", file=sys.stderr)
        return 2
    results = [(r["name"], run_gate(r["name"], r["kind"], r["command"],
                                    Path(a.evidence) / f"{r['name']}.log", r["min_passed"],
                                    a.ignored_budget, a.na_budget,
                                    r["require_env"], r["require_file"], a.cwd)) for r in chosen]
    bad = [n for n, ok in results if not ok]
    print(f"共 {len(results)} 项，未通过 {len(bad)} 项" + (f"：{', '.join(bad)}" if bad else ""))
    return 1 if bad else 0


def status(a: argparse.Namespace) -> int:
    evidence = Path(a.evidence)
    records = sorted(evidence.glob("*.log.json")) if evidence.is_dir() else []
    if not records:
        print(f"{evidence} 下没有任何门禁记录")
        return 1
    bad = []
    current = tree_fingerprint(Path.cwd())
    for path in records:
        try:
            rec = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            bad.append(path.stem)
            print(f"[{path.stem}] 记录损坏")
            continue
        if not rec.get("ok"):
            bad.append(rec.get("gate", path.stem))
        if rec.get("tree") != current:
            bad.append(rec.get("gate", path.stem))
            print(f"[{rec.get('gate', path.stem)}] 记录的内容指纹 {rec.get('tree')} 与当前工作树 {current} 不一致")
        c = rec.get("counts", {})
        print(f"[{rec.get('gate', path.stem)}] {'通过' if rec.get('ok') else '未通过'}  "
              f"通过 {c.get('passed', 0)} 失败 {c.get('failed', 0)} 忽略 {c.get('ignored', 0)} "
              f"不适用 {c.get('not_applicable', 0)}  提交 {rec.get('commit')}  时间 {rec.get('at')}")
    missing = [x.strip() for x in a.expect.split(",") if x.strip()] if a.expect else []
    if getattr(a, "gates", None):
        missing += [r["name"] for r in read_plan(a.gates)]
    have = {json.loads(p.read_text(encoding="utf-8")).get("gate") for p in records}
    lacking = [m for m in missing if m not in have]
    for m in lacking:
        print(f"[{m}] 没有记录，等同未执行")
    if lacking or bad:
        print(f"未通过 {len(bad)} 项，缺记录 {len(lacking)} 项：不得交付")
        return 1
    print("全部有记录且通过")
    return 0


GOOD = ("   Compiling diskedit v0.1.0\n"
        "running 40 tests\n........................................\n"
        "test result: ok. 40 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n")


def selftest() -> int:
    """每一条伪造输出都对应一种真实发生过或必然会发生的事故；判错任何一条即退出 1。"""
    ok_all = True

    def case(name: str, kind: str, code: int, out: str, want_ok: bool, min_passed: int = 1) -> None:
        nonlocal ok_all
        got, reasons, _ = judge(kind, code, out, min_passed, 0, 0)
        if got != want_ok:
            ok_all = False
            print(f"自检失败：{name} 应为{'通过' if want_ok else '未通过'}，实为{'通过' if got else '未通过'}")
        elif not want_ok and not reasons:
            ok_all = False
            print(f"自检失败：{name} 判了未通过但没有给出原因")

    case("正常绿", "test", 0, GOOD, True, 39)
    case("通过数不足", "test", 0, GOOD, False, 41)
    case("一条用例都没跑却退出 0（实际发生过的事故）", "test", 0,
         "running 0 tests\n\nserver-requests\n", False)
    case("全部被忽略", "test", 0,
         "test result: ok. 0 passed; 0 failed; 40 ignored; 0 measured; 0 filtered out\n", False)
    case("有失败", "test", 101, GOOD.replace("0 failed", "2 failed") + "error: test failed", False)
    case("退出码 0 但文本写着 NotApplicable", "test", 0, GOOD + "NotApplicable { why: no root }\n", False)
    case("文本写着 ok 但退出码非零", "test", 101, GOOD, False)
    case("lint 正常", "lint", 0, "", True)
    case("lint 有告警", "lint", 1, "warning: unused variable\n", False)

    saved = os.environ.get("GATE_SELFTEST_VAR")
    try:
        os.environ.pop("GATE_SELFTEST_VAR", None)
        p = preconditions(["GATE_SELFTEST_VAR"], [__file__])
        if len(p) != 1 or "GATE_SELFTEST_VAR" not in p[0]:
            ok_all = False
            print(f"自检失败：缺失的环境变量没有被报出 {p}")
        p = preconditions([], [str(Path(__file__).with_name("no-such-file.bin"))])
        if len(p) != 1 or "no-such-file.bin" not in p[0]:
            ok_all = False
            print(f"自检失败：缺失的文件没有被报出 {p}")
        os.environ["GATE_SELFTEST_VAR"] = "x"
        if preconditions(["GATE_SELFTEST_VAR"], [__file__]):
            ok_all = False
            print("自检失败：前置条件齐备却报了缺失")
    finally:
        if saved is None:
            os.environ.pop("GATE_SELFTEST_VAR", None)
        else:
            os.environ["GATE_SELFTEST_VAR"] = saved

    # 门禁表：列数、角色、类型、最少通过数、重名，每一类错误都必须被拒绝
    tmp = Path(__file__).resolve().parent / ".selftest_plan.tsv"
    here = shlex.quote(sys.executable)
    # 样例表必须是单行：把多行输出转义后交给 print，还原出同样的文本
    good_esc = GOOD.replace("\\", "\\\\").replace("\n", "\\n").replace("'", "\\'")
    body = ("# 注释行\n"
            f"G-a\tmodel\ttest\t1\t\t\t{here} -c \"print('{good_esc}')\"\n"
            f"G-b\tmodel\tlint\t1\t\t\t{here} -c \"print('clean')\"\n"
            f"G-c\thuman\ttest\t5\tDISKEDIT_BASELINE\t\t{here} -c \"print('{good_esc}')\"\n")
    try:
        tmp.write_text(body, encoding="utf-8")
        rows = read_plan(tmp)
        if [r["name"] for r in rows] != ["G-a", "G-b", "G-c"] or rows[2]["who"] != "human":
            ok_all = False
            print(f"自检失败：门禁表解析结果不对 {rows}")
        for bad_body, why in (
            ("G\tmodel\ttest\t0\t\t\tx", "最少通过数为 0"),
            ("G\trobot\ttest\t1\t\t\tx", "角色写错"),
            ("G\tmodel\tother\t1\t\t\tx", "类型写错"),
            ("G\tmodel\ttest\t1\t\t\tx\ty", "多一列"),
            ("G\tmodel\ttest\t1\t\t\t", "少一列"),
            ("G\tmodel\ttest\t1\t\t\tx\nG\thuman\tlint\t1\t\t\tx", "门禁重名"),
            ("# 只有注释", "空表"),
        ):
            try:
                read_plan_write(tmp, bad_body)
                ok_all = False
                print(f"自检失败：{why} 的门禁表没有被拒绝")
            except ValueError:
                pass
        unpinned_rows = read_plan_write(tmp, "G-u\tmodel\ttest\t待钉\t\t\tx\nG-v\tmodel\tlint\t1\t\t\tx")
        if unpinned_rows[0]["min_passed"] is not None or unpinned_rows[1]["min_passed"] != 1:
            ok_all = False
            print("自检失败：待钉的门禁应当没有登记数，且不影响其他行")
        ns_u = argparse.Namespace(plan=str(tmp), evidence=str(tmp.with_suffix(".evidence")), who="model",
                                  only="G-u", cwd=None, ignored_budget=0, na_budget=0)
        with contextlib.redirect_stdout(io.StringIO()):
            if run_all(ns_u) != 1:
                ok_all = False
                print("自检失败：没有登记数的测试门禁应当判红")
        rec_u = json.loads((tmp.with_suffix(".evidence") / "G-u.log.json").read_text(encoding="utf-8"))
        if not any("未登记" in r for r in rec_u["reasons"]):
            ok_all = False
            print("自检失败：没有登记数的门禁红因里没有写明原因")
        tmp.write_text(body, encoding="utf-8")
        os.environ.pop("DISKEDIT_BASELINE", None)
        ns = argparse.Namespace(plan=str(tmp), evidence=str(tmp.with_suffix(".evidence")), who="human",
                                only=None, cwd=None, ignored_budget=0, na_budget=0)
        with contextlib.redirect_stdout(io.StringIO()):
            run_all(ns)
        # 人跑的那一项缺前置条件，必须未通过；记录必须写出
        rec = json.loads((tmp.with_suffix(".evidence") / "G-c.log.json").read_text(encoding="utf-8"))
        if rec["ok"] or not any("DISKEDIT_BASELINE" in r for r in rec["reasons"]):
            ok_all = False
            print("自检失败：按表执行时前置条件缺失没有变红")
        ns.who = "model"
        with contextlib.redirect_stdout(io.StringIO()):
            all_model = run_all(ns)
        if all_model != 0:
            ok_all = False
            print("自检失败：按表执行两项都应通过")
    except (OSError, ValueError, KeyError) as e:
        ok_all = False
        print(f"自检失败：按表执行时异常 {type(e).__name__}: {e}")
    finally:
        for p in sorted(tmp.parent.glob(".selftest_plan*"), reverse=True):
            if p.is_dir():
                for q in sorted(p.rglob("*"), reverse=True):
                    q.unlink() if q.is_file() else q.rmdir()
                p.rmdir()
            else:
                p.unlink()
    print("门禁包装器自检通过" if ok_all else "门禁包装器自检未通过")
    return 0 if ok_all else 1


def read_plan_write(tmp: Path, body: str) -> list[dict]:
    tmp.write_text(body, encoding="utf-8")
    return read_plan(tmp)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("run")
    p.add_argument("--name", required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--kind", choices=KINDS, default="test")
    p.add_argument("--min-passed", type=int, default=1)
    p.add_argument("--ignored-budget", type=int, default=0)
    p.add_argument("--na-budget", type=int, default=0)
    p.add_argument("--require-env", action="append", default=[])
    p.add_argument("--require-file", action="append", default=[])
    p.add_argument("--cwd")
    p.add_argument("cmd", nargs=argparse.REMAINDER)
    p = sub.add_parser("all")
    p.add_argument("--plan", required=True)
    p.add_argument("--evidence", default="plan/evidence")
    p.add_argument("--who", choices=("model", "human", "all"), default="model")
    p.add_argument("--only")
    p.add_argument("--exclude")
    p.add_argument("--cwd")
    p.add_argument("--ignored-budget", type=int, default=0)
    p.add_argument("--na-budget", type=int, default=0)
    p = sub.add_parser("status")
    p.add_argument("--evidence", default="plan/evidence")
    p.add_argument("--expect", help="必须有记录的门禁名，逗号分隔")
    p.add_argument("--gates", type=Path, help="按门禁表核对每条都有记录")
    sub.add_parser("selftest")
    a = ap.parse_args()
    if a.cmd == "selftest":
        return selftest()
    if a.cmd == "status":
        return status(a)
    if a.cmd == "all":
        return run_all(a)
    cmd = getattr(a, "cmd", None)
    if not cmd:
        print("缺少要执行的命令", file=sys.stderr)
        return 2
    cmd = cmd[1:] if cmd[:1] == ["--"] else cmd
    if a.min_passed < 1:
        print("--min-passed 不得小于 1：没有用例通过的测试不算通过", file=sys.stderr)
        return 2
    return 0 if run_gate(a.name, a.kind, cmd, Path(a.log), a.min_passed, a.ignored_budget,
                         a.na_budget, a.require_env, a.require_file, a.cwd) else 1


if __name__ == "__main__":
    sys.exit(main())
