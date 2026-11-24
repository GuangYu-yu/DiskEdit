#!/usr/bin/env python3
"""基线普查器：把旧版（行为基线）的可观察面机械数出来，并核对新版没有丢。

只读旧版源码，不改任何文件。三件事：

  1. surface  解析命令声明表，得到 命令 → 形态 → 选项/判别项/目标后缀/确认 的全集。
  2. sites    枚举全部退出码构造点（拒绝、未改动、不支持、用法、内部、部分完成）。
  3. 核对     旧版变了必红；新版命令面与旧版有差必红；旧版每个拒绝点没人在新版认领必红。

用法（在仓库任意位置执行，路径用参数给）：

  python census.py freeze --old <旧版 src> --out <基线目录>
      首次或旧版变化后：写出 census.json 与 refusal-sites.tsv。

  python census.py check --old <旧版 src> --baseline <基线目录>
      旧版与冻结结果不一致即退出 1。

  python census.py surface --old <旧版 src> --new <新版导出的 command-surface.json>
      [--allow <允许差异清单>]  新旧命令面有差且不在清单内即退出 1。

  python census.py owners --baseline <基线目录> --map <refusal-map.tsv>
      旧版每个拒绝点在新版必须有认领人与用例，缺一即退出 1。

新版导出文件的格式与 census.json 的 "commands" 完全相同，由新版自己的契约用例生成。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

FIELDS = ("name", "group", "summary", "variants", "common_options", "details", "mode",
          "capabilities", "confirmation", "build")
OUTCOME_KINDS = "refused|no_mutation|unsupported|usage|internal|partial[a-z_]*"
SITE_RE = re.compile(r"\bRunOutcome::(" + OUTCOME_KINDS + r")\s*\(")
COVERS_RE = re.compile(r"covers:\s*([A-Z]\d+(?:\s*,\s*[A-Z]\d+)*)")
MAX_COVERS = 8
OPT_RE = re.compile(r'"(--[a-z][a-z0-9-]*)"')
SYNTAX_OPT_RE = re.compile(r"--[a-z][a-z0-9-]*")
FLAG_SEL_RE = re.compile(r'Selector::Flag\(\s*"(--[a-z0-9-]+)"\s*\)')
POS_SEL_RE = re.compile(r'Selector::Positional\s*\{\s*index:\s*(\d+)\s*,\s*literal:\s*"([^"]*)"')
STR_RE = re.compile(r'"((?:[^"\\]|\\.)*)"')
CHAR_RE = re.compile(r"'(?:[^'\\]|\\.|\\u\{[0-9a-fA-F]+\})'")


class ParseError(Exception):
    pass


# ---------- 词法辅助 ----------

def skip_string(text: str, i: int) -> int:
    """text[i] 是双引号（或 r#" 的引号），返回字符串结束后的下标。"""
    j = i - 1
    hashes = 0
    while j >= 0 and text[j] == "#":
        hashes += 1
        j -= 1
    closer = '"' + "#" * hashes
    k = text.find(closer, i + 1)
    if k < 0:
        raise ParseError("unterminated string")
    return k + len(closer)


def skip_comment(text: str, i: int) -> int:
    if text.startswith("//", i):
        j = text.find("\n", i)
        return len(text) if j < 0 else j
    j = text.find("*/", i + 2)
    if j < 0:
        raise ParseError("unterminated comment")
    return j + 2


def match_close(text: str, i: int, pair: str) -> int:
    """text[i] 是开括号，返回配对的闭括号下标。字符串、字符字面量、注释不参与计数。"""
    stack = []
    table = {"{": "}", "(": ")", "[": "]"}
    n = len(text)
    while i < n:
        c = text[i]
        if c == '"' or (c == "#" and i > 0 and text[i - 1] in "#\""):
            i = skip_string(text, i)
            continue
        if text.startswith("r#", i) or text.startswith("r\"", i):
            k = i + (2 if text[i + 1] == "#" else 1)
            i = skip_string(text, k)
            continue
        if c == "/" and i + 1 < n and text[i + 1] in "/*":
            i = skip_comment(text, i)
            continue
        if c == "'":
            m = CHAR_RE.match(text, i)
            if m:
                i = m.end()
                continue
        if c in table:
            stack.append(table[c])
        elif stack and c == stack[-1]:
            stack.pop()
            if not stack:
                return i
        i += 1
    raise ParseError(f"unbalanced {pair!r} at {i}")


def split_top(body: str, seps: str = ",") -> list[str]:
    """按顶层分隔符切分，字符串/注释/嵌套括号内不切。"""
    parts, start, depth, i, n = [], 0, 0, 0, len(body)
    while i < n:
        c = body[i]
        if c == '"':
            i = skip_string(body, i)
            continue
        if c == "/" and i + 1 < n and body[i + 1] in "/*":
            i = skip_comment(body, i)
            continue
        if c == "'":
            m = CHAR_RE.match(body, i)
            if m:
                i = m.end()
                continue
        if c in "{([":
            depth += 1
        elif c in "})]":
            depth -= 1
        elif c in seps and depth == 0:
            parts.append(body[start:i])
            start = i + 1
        i += 1
    parts.append(body[start:])
    return [p.strip() for p in parts if p.strip()]


# ---------- 命令声明表 ----------

def parse_variant(body: str) -> dict:
    args = split_top(body)
    if len(args) != 6:
        raise ParseError(f"variant 应有 6 个参数，实得 {len(args)}：{args[0] if args else ''}")
    syntax = STR_RE.search(args[0])
    target = re.search(r"TargetSuffix::(\w+)", args[4])
    options = sorted(set(OPT_RE.findall(args[2])))
    mentioned = sorted(set(SYNTAX_OPT_RE.findall(args[0])) - {"--yes"})
    return {
        "syntax": syntax.group(1) if syntax else args[0],
        "options": options,
        "selectors": sorted([f"flag:{m}" for m in FLAG_SEL_RE.findall(args[3])]
                            + [f"pos:{i}={lit}" for i, lit in POS_SEL_RE.findall(args[3])]),
        "target": target.group(1) if target else None,
        "default": args[5].strip() == "true",
        "syntax_only_options": sorted(set(mentioned) - set(options)),
    }


def parse_command(body: str) -> dict:
    chunks = split_top(body)
    fields: dict[str, str] = {}
    for chunk in chunks:
        m = re.match(r"(\w+)\s*:", chunk)
        if m and m.group(1) in FIELDS:
            fields[m.group(1)] = chunk[m.end():].strip()
    missing = [f for f in ("name", "variants", "confirmation") if f not in fields]
    if missing:
        raise ParseError(f"CommandDef 缺字段 {missing}")
    name = STR_RE.search(fields["name"]).group(1)
    vtext = fields["variants"]
    arr = vtext.index("[")
    inner = vtext[arr + 1:match_close(vtext, arr, "[]")]
    variants, depth, i = [], 0, 0
    while i < len(inner):
        c = inner[i]
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
        elif depth == 0 and inner.startswith("variant(", i):
            close = match_close(inner, i + len("variant"), "()")
            variants.append(parse_variant(inner[i + len("variant("):close]))
            i = close
        i += 1
    conf = re.search(r"Confirmation::(\w+)", fields["confirmation"])
    return {
        "name": name,
        "confirmation": conf.group(1) if conf else None,
        "common_options": sorted(set(OPT_RE.findall(fields.get("common_options", "")))),
        "variants": variants,
    }


def parse_surface(text: str) -> list[dict]:
    m = re.search(r"const COMMANDS\s*:\s*&\[CommandDef\]\s*=\s*&\[", text)
    if not m:
        raise ParseError("找不到 const COMMANDS: &[CommandDef]")
    end = match_close(text, m.end() - 1, "[]")
    inner = text[m.end():end]
    commands, i = [], 0
    while True:
        k = inner.find("CommandDef {", i)
        if k < 0:
            break
        close = match_close(inner, k + len("CommandDef "), "{}")
        commands.append(parse_command(inner[k + len("CommandDef {"):close]))
        i = close
    names = [c["name"] for c in commands]
    if len(names) != len(set(names)):
        raise ParseError(f"命令名重复：{sorted({n for n in names if names.count(n) > 1})}")
    return commands


# ---------- 退出码构造点 ----------

def is_test_path(path: Path) -> bool:
    parts = path.parts
    return path.name.endswith("tests.rs") or "tests" in parts or "benches" in parts


def mask_line(line: str) -> str:
    """把一行里的字符串内容与注释抹成等长空格，保留引号本身，使下标不变。

    这样字符串或注释里出现的 `RunOutcome::refused(` 不会被数成构造点。
    跨行的原始字符串不在处理范围内。
    """
    out, i, n = [], 0, len(line)
    while i < n:
        c = line[i]
        if c == '"' or line.startswith("r#", i) or line.startswith("r\"", i):
            try:
                end = skip_string(line, i if c == '"' else i + 1)
            except ParseError:
                end = n
            out.append(line[i:i + 1])
            out.append(" " * max(0, end - i - 2))
            out.append('"' if end - i >= 2 else "")
            i = end
            continue
        if line.startswith("//", i):
            break
        out.append(c)
        i += 1
    return "".join(out)


def scan_sites(src: Path) -> list[dict]:
    sites = []
    for path in sorted(p for p in src.rglob("*.rs")):
        rel = path.relative_to(src).as_posix()
        test_file = is_test_path(path)
        try:
            lines = path.read_text(encoding="utf-8").splitlines()
        except UnicodeDecodeError:
            lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        joined = "\n".join(lines)
        # 行内 #[cfg(test)] mod x { … } 的 brace 深度：进入该深度之后的构造点算测试代码
        test_depth = None
        offset, depth, in_test_mod = 0, 0, False
        mod_re = re.compile(r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")
        test_spans = []
        for m in mod_re.finditer(joined):
            try:
                test_spans.append((m.end() - 1, match_close(joined, m.end() - 1, "{}")))
            except ParseError:
                pass
        for lineno, line in enumerate(lines, 1):
            masked = mask_line(line)
            for m in SITE_RE.finditer(masked):
                pos = offset + m.start()
                inline_test = any(a <= pos <= b for a, b in test_spans)
                context = "\n".join(lines[lineno - 1:lineno + 2])[m.end() - m.start():]
                lit = STR_RE.search(context)
                sites.append({
                    "file": rel,
                    "line": lineno,
                    "kind": m.group(1),
                    "message": lit.group(1)[:160] if lit else "",
                    "in_test": test_file or inline_test,
                })
            offset += len(line) + 1
    return sites


def first_line(message: str) -> str:
    return re.sub(r"\s+", " ", message.replace('\\"', '"').replace("\\n", " ")).strip()


# ---------- 冻结与核对 ----------

def freeze(old: Path, out: Path) -> int:
    commands = parse_surface((old / "cli.rs").read_text(encoding="utf-8"))
    sites = [s for s in scan_sites(old) if not s["in_test"]]
    prod = sorted(sites, key=lambda s: (s["file"], s["line"], s["kind"]))
    for n, s in enumerate(prod, 1):
        s["id"] = f"R{n:04d}"
    by_kind: dict[str, int] = {}
    for s in prod:
        by_kind[s["kind"]] = by_kind.get(s["kind"], 0) + 1
    # 语法串里写了、白名单里却没有的选项：解析器与声明表对不上的信号，基线自身应为 0
    orphan = sorted({f"{c['name']}|{v['syntax']}|{o}" for c in commands for v in c["variants"]
                     if (o := next((o for o in v["syntax_only_options"]
                                    if o not in c["common_options"]), None))})
    digest = hashlib.sha256("\n".join(
        f"{s['file']}:{s['line']}:{s['kind']}:{first_line(s['message'])}" for s in prod
    ).encode("utf-8")).hexdigest()
    data = {
        "commands": commands,
        "counts": {
            "commands": len(commands),
            "variants": sum(len(c["variants"]) for c in commands),
            "sites_total": len(prod),
            "sites_by_kind": dict(sorted(by_kind.items())),
            "options_in_syntax_not_whitelisted": orphan,
        },
        "sites_sha256": digest,
    }
    out.mkdir(parents=True, exist_ok=True)
    (out / "census.json").write_text(json.dumps(data, ensure_ascii=False, indent=1) + "\n",
                                     encoding="utf-8")
    lines = ["id\tfile\tline\tkind\tmessage"]
    lines += [f"{s['id']}\t{s['file']}\t{s['line']}\t{s['kind']}\t{first_line(s['message'])}"
              for s in prod]
    (out / "refusal-sites.tsv").write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(json.dumps(data["counts"], ensure_ascii=False))
    print(f"写出 {out / 'census.json'}、{out / 'refusal-sites.tsv'}")
    return 0


def check(old: Path, baseline: Path) -> int:
    frozen = json.loads((baseline / "census.json").read_text(encoding="utf-8"))
    commands = parse_surface((old / "cli.rs").read_text(encoding="utf-8"))
    sites = sorted((s for s in scan_sites(old) if not s["in_test"]),
                   key=lambda s: (s["file"], s["line"], s["kind"]))
    digest = hashlib.sha256("\n".join(
        f"{s['file']}:{s['line']}:{s['kind']}:{first_line(s['message'])}" for s in sites
    ).encode("utf-8")).hexdigest()
    bad = 0
    if commands != frozen["commands"]:
        bad += 1
        print("命令面与冻结结果不一致")
        for a, b in zip(frozen["commands"], commands):
            if a != b:
                print(f"  旧冻结：{a['name']} 形态 {len(a['variants'])}；现为 {len(b['variants'])}")
    if digest != frozen["sites_sha256"]:
        bad += 1
        print(f"退出码构造点与冻结结果不一致：冻结 {frozen['counts']['sites_total']} 处，"
              f"现为 {len(sites)} 处")
    if len(commands) != len(frozen["commands"]):
        bad += 1
    print("旧版与冻结结果一致" if bad == 0 else "旧版已变化：重新 freeze，并逐条核对差异来源")
    return 1 if bad else 0


def diff_surface(old: list[dict], new: list[dict]) -> list[str]:
    out = []
    o = {c["name"]: c for c in old}
    n = {c["name"]: c for c in new}
    for name in sorted(set(o) - set(n)):
        out.append(f"surface|command-missing|{name}")
    for name in sorted(set(n) - set(o)):
        out.append(f"surface|command-extra|{name}")
    for name in sorted(set(o) & set(n)):
        a, b = o[name], n[name]
        if a["confirmation"] != b.get("confirmation"):
            out.append(f"surface|confirmation|{name}|{a['confirmation']}|{b.get('confirmation')}")
        if sorted(a["common_options"]) != sorted(b.get("common_options", [])):
            out.append(f"surface|common-options|{name}|{sorted(a['common_options'])}|"
                       f"{sorted(b.get('common_options', []))}")
        av = {v["syntax"]: v for v in a["variants"]}
        bv = {v["syntax"]: v for v in b.get("variants", [])}
        for s in sorted(set(av) - set(bv)):
            out.append(f"surface|variant-missing|{name}|{s}")
        for s in sorted(set(bv) - set(av)):
            out.append(f"surface|variant-extra|{name}|{s}")
        for s in sorted(set(av) & set(bv)):
            for key in ("options", "selectors", "target", "default"):
                x, y = _norm(av[s][key]), _norm(bv[s].get(key))
                if x != y:
                    out.append(f"surface|variant-{key}|{name}|{s}|{x}|{y}")
    return out


def _norm(value):
    """选项与判别项是列表（顺序无关）；目标后缀是字符串；默认形态是布尔。统一成可比形式。"""
    return sorted(value) if isinstance(value, list) else value


def surface(old: Path, new: Path, allow: Path | None) -> int:
    allowed: dict[str, str | None] = {}
    bad_allow: list[str] = []
    if allow:
        known_d = plan_ids(allow.parent.parent)["D"]
        for ln in allow.read_text(encoding="utf-8").splitlines():
            if not ln.strip() or ln.lstrip().startswith("#"):
                continue
            body, d = _allow_line(ln)
            allowed[body] = d
            if d and d not in known_d:
                bad_allow.append(f"{d}（{body}）")
    oldc = parse_surface((old / "cli.rs").read_text(encoding="utf-8"))
    newc = json.loads(new.read_text(encoding="utf-8"))
    newc = newc["commands"] if isinstance(newc, dict) and "commands" in newc else newc
    diffs = diff_surface(oldc, newc)
    unregistered = [d for d in diffs if d not in allowed]
    for d in diffs:
        print(("已登记  " if d in allowed else "未登记  ") + d)
    if bad_allow:
        print("允许清单引用了不存在的 D：" + "；".join(bad_allow))
    print(f"命令面差异 {len(diffs)} 条，其中未登记 {len(unregistered)} 条")
    return 1 if unregistered or bad_allow else 0


def _allow_line(line: str) -> tuple[str, str | None]:
    """允许清单一行的格式是「差集行原文 + 制表符 + D 编号」；没有 D 编号的行按未登记差异之外的备注处理。"""
    parts = line.rstrip("\n").split("\t")
    if len(parts) == 2 and re.fullmatch(r"D\d+", parts[1].strip()):
        return parts[0].strip(), parts[1].strip()
    return line.strip(), None


def owners(baseline: Path, refmap: Path, tests: Path | None = None, due: Path | None = None) -> int:
    frozen = json.loads((baseline / "census.json").read_text(encoding="utf-8"))
    due_ids: set[str] | None = None
    if due is not None:
        due_ids = {ln.split("\t")[0].strip() for ln in due.read_text(encoding="utf-8").splitlines()
                   if ln.strip() and not ln.lstrip().startswith("#")}
        if not due_ids:
            print(f"{due} 里没有任何 R 编号：空清单不能当作本轮无待认领项")
            return 1
    covers_r: set[str] = set()
    if tests is not None:
        for f in tests.rglob("*.rs"):
            try:
                text = f.read_text(encoding="utf-8")
            except OSError:
                continue
            for m in COVERS_RE.finditer(text):
                toks = re.findall(r"R\d+", m.group(1))
                if len(toks) <= MAX_COVERS:
                    covers_r |= set(toks)
    placeholders = {"待填", "待补", "无", "-", "TBD", "todo", "TODO"}
    rows: dict[str, list[list[str]]] = {}
    for ln in refmap.read_text(encoding="utf-8").splitlines()[1:]:
        cols = [c.strip() for c in ln.split("\t")]
        if cols and cols[0]:
            rows.setdefault(cols[0], []).append(cols)
    invalid = []
    checked = 0
    for s in (baseline / "refusal-sites.tsv").read_text(encoding="utf-8").splitlines()[1:]:
        rid = s.split("\t")[0]
        if due_ids is not None and rid not in due_ids:
            continue
        checked += 1
        got = rows.get(rid, [])
        ok = bool(got) and all(
            len(c) >= 4 and all(c[1:4]) and not (set(c[1:4]) & placeholders) for c in got)
        if ok and tests is not None and rid not in covers_r:
            ok = False
        if not ok:
            invalid.append(rid)
    total = frozen["counts"]["sites_total"]
    scope = f"本轮应认领 {checked} 处；" if due_ids is not None else ""
    print(f"旧版退出码构造点 {total} 处；{scope}未认领或无效 {len(invalid)} 处")
    if tests is not None:
        print(f"用例里标注 covers 的 R 行 {len(covers_r)} 处")
    for rid in invalid[:50]:
        print("  未认领或无效 " + rid)
    if len(invalid) > 50:
        print(f"  …其余 {len(invalid) - 50} 处略")
    return 1 if invalid else 0


def plan_ids(plan: Path) -> dict[str, set[str]]:
    """从计划文档的表格与标题取出 B/I/D 编号全集；文件缺失视为空集。"""
    ids: dict[str, set[str]] = {}
    for key, name, prefix in (("B", "03-行为基线.md", "B"), ("I", "01-目标与边界.md", "I")):
        path = plan / name
        ids[key] = set()
        if path.exists():
            ids[key] = set(re.findall(rf"^\|\s*({prefix}\d+)\s*\|",
                                      path.read_text(encoding="utf-8"), re.M))
    path = plan / "divergences.md"
    ids["D"] = set()
    if path.exists():
        text = path.read_text(encoding="utf-8")
        ids["D"] = set(re.findall(r"^\|\s*(D\d+)\s*\|", text, re.M))
        ids["D"] |= set(re.findall(r"^#+\s*(D\d+)\b", text, re.M))
    return ids


def covers(plan: Path, src: Path, tests: Path) -> int:
    """B/I/D 每条都必须有带 // covers: 的用例；缺一条即退出 1。"""
    want = plan_ids(plan)
    found: dict[str, set[str]] = {k: set() for k in want}
    scanned = 0
    for root in (src, tests):
        if not root.exists():
            continue
        for f in root.rglob("*.rs"):
            scanned += 1
            try:
                text = f.read_text(encoding="utf-8", errors="ignore")
            except OSError:
                continue
            for m in COVERS_RE.finditer(text):
                toks = re.findall(r"[A-Z]\d+", m.group(1))
                if len(toks) > MAX_COVERS:
                    print(f"  忽略过宽标注 {f}:{text.count(chr(10), 0, m.start()) + 1}：{len(toks)} 个编号")
                    continue
                for tok in toks:
                    found.setdefault(tok[0], set()).add(tok)
    bad = 0
    for k in ("B", "I", "D"):
        missing = sorted(want[k] - found[k], key=lambda x: int(x[1:]))
        print(f"{k}: 全集 {len(want[k])}，已覆盖 {len(want[k]) - len(missing)}，缺 {len(missing)}")
        for x in missing:
            print("  缺 " + x)
        bad += len(missing)
    print(f"扫描 {scanned} 个 Rust 文件：" + ("覆盖完备" if bad == 0 else f"缺 {bad} 条"))
    return 1 if bad else 0


def _cells(line: str) -> list[str]:
    s = line.strip()
    if s.startswith("|"):
        s = s[1:]
    if s.endswith("|"):
        s = s[:-1]
    cells: list[str] = []
    cur: list[str] = []
    esc = False
    for ch in s:
        if esc:
            cur.append(ch)
            esc = False
        elif ch == "\\":
            esc = True
            cur.append(ch)
        elif ch == "|":
            cells.append("".join(cur).strip())
            cur = []
        else:
            cur.append(ch)
    cells.append("".join(cur).strip())
    return cells


def docs(plan: Path) -> int:
    """文档一致性：表格列数、编号与措辞、README 重复条目；任一不一致即退出 1。"""
    problems: list[str] = []
    for path in sorted(plan.glob("*.md")):
        lines = path.read_text(encoding="utf-8").splitlines()
        fence = False
        i = 0
        while i < len(lines):
            ln = lines[i]
            if ln.lstrip().startswith("```"):
                fence = not fence
                i += 1
                continue
            if fence:
                i += 1
                continue
            if (ln.strip().startswith("|") and i + 1 < len(lines)
                    and re.match(r"^\s*\|[\s:|-]+\|\s*$", lines[i + 1])):
                head = _cells(ln)
                sep = _cells(lines[i + 1])
                if len(head) != len(sep):
                    problems.append(f"{path.name}:{i + 1} 表头 {len(head)} 列、分隔 {len(sep)} 列")
                j = i + 2
                while j < len(lines) and lines[j].strip().startswith("|"):
                    n = len(_cells(lines[j]))
                    if n != len(head):
                        problems.append(f"{path.name}:{j + 1} 行 {n} 列，表头 {len(head)} 列")
                    j += 1
                i = j
                continue
            i += 1
    p01 = plan / "01-目标与边界.md"
    n_i = len(plan_ids(plan)["I"])
    if p01.exists():
        text = p01.read_text(encoding="utf-8")
        if "十项不变量" in text and n_i != 10:
            problems.append(f"01 写“十项不变量”，实际 I 共 {n_i} 条")
    readme = plan / "README.md"
    if readme.exists():
        seen: dict[str, int] = {}
        for idx, ln in enumerate(readme.read_text(encoding="utf-8").splitlines(), 1):
            t = ln.strip()
            if t.startswith("- ") and len(t) > 12:
                if t in seen:
                    problems.append(f"README.md:{idx} 与第 {seen[t]} 行重复")
                else:
                    seen[t] = idx
    for p in problems:
        print("文档不一致 " + p)
    print("文档一致" if not problems else f"文档不一致 {len(problems)} 处")
    return 1 if problems else 0


def selftest() -> int:
    """解析器自检：注入已知缺陷，每一条都必须被发现；发现不了即退出 1。"""
    sample = '''
pub(crate) const COMMANDS: &[CommandDef] = &[
    CommandDef {
        name: "demo",
        summary: "s, with \\"quotes\\" and // comment, bracket ]",
        variants: &[
            variant("<TARGET> [--size N]", &[TARGET_ARG], &[], &[], TargetSuffix::Whole, true),
            variant("<TARGET>:N --grow", &[TARGET_ARG], &[opt_desc("--grow", "d")],
                &[Selector::Flag("--grow"), Selector::Positional { index: 1, literal: "flag" }],
                TargetSuffix::Partition, false),
        ],
        common_options: &[opt("--sector-size")],
        details: crate::cmd::demo::DETAILS,
        confirmation: Confirmation::Required,
        build: build_demo,
    },
];
'''
    c = parse_surface(sample)[0]
    ok = True

    def expect(cond: bool, what: str):
        nonlocal ok
        if not cond:
            ok = False
            print("自检失败：" + what)

    expect(c["name"] == "demo", "命令名")
    expect(c["confirmation"] == "Required", "确认门")
    expect(c["common_options"] == ["--sector-size"], "公共选项")
    expect(len(c["variants"]) == 2, "形态数")
    expect(c["variants"][0]["syntax"] == "<TARGET> [--size N]", "形态语法含注释与括号")
    expect(c["variants"][1]["options"] == ["--grow"], "带说明的选项")
    expect(c["variants"][1]["selectors"] == ["flag:--grow", "pos:1=flag"], "判别项")
    expect(c["variants"][1]["default"] is False and c["variants"][0]["default"] is True, "默认形态")
    expect(c["variants"][0]["syntax_only_options"] == ["--size"], "语法里提到但白名单没有的选项")

    # 新旧差集：少一条命令、少一个形态、多一个选项、确认门变了，共 4 类都必须报出
    new = json.loads(json.dumps([c]))
    new.append({"name": "extra", "confirmation": "None", "common_options": [], "variants": []})
    new[0]["variants"][1]["options"].append("--new-flag")
    new[0]["confirmation"] = "None"
    d = diff_surface([c], new)
    kinds = sorted({x.split("|")[1] for x in d})
    expect(kinds == ["command-extra", "confirmation", "variant-options"], f"差集种类 {kinds}")
    # 反方向：新版少一个形态，必须报 variant-missing
    less = json.loads(json.dumps([c]))
    less[0]["variants"].pop()
    d2 = diff_surface([c], less)
    expect([x.split("|")[1] for x in d2] == ["variant-missing"], f"新版缺形态 {d2}")

    # 构造点：测试文件与内联测试模块内的不算，生产代码内的必须全部数到
    tmp = Path(__file__).resolve().parent / ".selftest_src"
    try:
        (tmp / "sub").mkdir(parents=True, exist_ok=True)
        (tmp / "sub" / "tests.rs").write_text('fn t() { let _ = RunOutcome::refused("x"); }\n',
                                              encoding="utf-8")
        (tmp / "sub" / "a.rs").write_text(
            '#[cfg(test)]\nmod tests { fn t() { RunOutcome::refused("in test"); } }\n'
            'fn p() -> RunOutcome {\n    RunOutcome::refused(format!("bad {}", 1))\n}\n'
            'fn q() { let s = "RunOutcome::refused(\\"not a site\\")"; let _ = s; }\n',
            encoding="utf-8")
        sites = [s for s in scan_sites(tmp) if not s["in_test"]]
        expect(len(sites) == 1 and sites[0]["file"] == "sub/a.rs" and sites[0]["line"] == 4,
               f"构造点计数 {[ (s['file'], s['line']) for s in sites ]}")
        expect(sites[0]["message"] == "bad {}", "拒绝文案")
    finally:
        for p in sorted(tmp.rglob("*"), reverse=True):
            p.unlink() if p.is_file() else p.rmdir()
        tmp.rmdir()
    tmp2 = Path(__file__).resolve().parent / ".selftest_plan"
    try:
        tmp2.mkdir()
        (tmp2 / "01-目标与边界.md").write_text(
            "| I | x |\n| --- | --- |\n| I1 | a |\n| I2 | b |\n", encoding="utf-8")
        (tmp2 / "03-行为基线.md").write_text(
            "| B | x |\n| --- | --- |\n| B1 | a |\n", encoding="utf-8")
        (tmp2 / "divergences.md").write_text(
            "| D | x |\n| --- | --- |\n| D1 | a |\n", encoding="utf-8")
        (tmp2 / "README.md").write_text("- 唯一一条说明，长度足够\n", encoding="utf-8")
        tdir = tmp2 / "t"
        tdir.mkdir()
        (tdir / "a.rs").write_text("// covers: B1, I1\n", encoding="utf-8")
        expect(covers(tmp2, tdir, tdir) == 1, "覆盖缺项没有报红")
        (tdir / "b.rs").write_text("// covers: I2, D1\n", encoding="utf-8")
        expect(covers(tmp2, tdir, tdir) == 0, "覆盖齐全仍报红")
        expect(docs(tmp2) == 0, "一致文档被误报")
        (tmp2 / "02-bad.md").write_text(
            "| a | b |\n| --- | --- | --- |\n| 1 | 2 | 3 |\n", encoding="utf-8")
        expect(docs(tmp2) == 1, "表格列数不一致没有报红")
    finally:
        for p in sorted(tmp2.rglob("*"), reverse=True):
            p.unlink() if p.is_file() else p.rmdir()
        tmp2.rmdir()
    print("解析器自检通过" if ok else "解析器自检未通过")
    return 0 if ok else 1


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("freeze")
    p.add_argument("--old", required=True, type=Path)
    p.add_argument("--out", required=True, type=Path)
    p = sub.add_parser("check")
    p.add_argument("--old", required=True, type=Path)
    p.add_argument("--baseline", required=True, type=Path)
    p = sub.add_parser("surface")
    p.add_argument("--old", required=True, type=Path)
    p.add_argument("--new", required=True, type=Path)
    p.add_argument("--allow", type=Path)
    p = sub.add_parser("owners")
    p.add_argument("--baseline", required=True, type=Path)
    p.add_argument("--map", required=True, type=Path)
    p.add_argument("--tests", type=Path)
    p.add_argument("--due", type=Path)
    p = sub.add_parser("covers")
    p.add_argument("--plan", type=Path, default=Path("plan"))
    p.add_argument("--src", type=Path, default=Path("src"))
    p.add_argument("--tests", type=Path, default=Path("tests"))
    p = sub.add_parser("docs")
    p.add_argument("--plan", type=Path, default=Path("plan"))
    sub.add_parser("selftest")
    a = ap.parse_args()
    try:
        if a.cmd == "freeze":
            return freeze(a.old, a.out)
        if a.cmd == "check":
            return check(a.old, a.baseline)
        if a.cmd == "surface":
            return surface(a.old, a.new, a.allow)
        if a.cmd == "owners":
            return owners(a.baseline, a.map, a.tests, a.due)
        if a.cmd == "covers":
            return covers(a.plan, a.src, a.tests)
        if a.cmd == "docs":
            return docs(a.plan)
        return selftest()
    except (ParseError, OSError, json.JSONDecodeError, KeyError, IndexError) as e:
        print(f"普查器无法给出结论：{type(e).__name__}: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
