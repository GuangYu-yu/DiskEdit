#!/usr/bin/env bash
# 命令面静态校验：仓库内 shell 与 rust 测试里出现的子命令与旗标必须落在二进制 help 的登记表内。
# run_all.sh 把本脚本排在最前：一次列出全部失效点，不必等某个脚本执行到那一行才炸。
#
# 抽取范围（只认同一次调用边界内的旗标，不扫注释与普通字符串）：
#   shell: 仓库内全部 *.sh（.git / target / tmp / node_modules 之外）中
#          $B/$BF/$BFK/$BFKO/$BIN 开头的调用，取调用词与其后到 | ; & > ) 为止的
#          同段参数里的全部 --flag；续行先拼接再判断。
#   rust:  tests/*.rs、tests/cli/*.rs 中 run(&[...]) / .args([...]) / .args(&[...])
#          的字面量数组：第一个字面量是子命令（"records" "list" 这类两词命令合并），
#          其余 --flag 逐条校验。
#
# 抽取不到的地方（静态分析边界，不是豁免）：
#   - 旗标经 "$@" 透传的包装函数（如 blockdev_4kn.sh 的 addk）看不到实际旗标；
#   - bash -c '...' 字符串里的调用不展开（其中的程序名是位置参数，不是 $B）。
#
# 例外只有两种，都写在出现处：
#   1. 外部工具白名单：$B 族变量被指向 dmsetup/sgdisk 等工具时不按本工具命令面判；
#      直接调用的外部工具本来就不在抽取集里（扫描只锚定 $B 族变量，不做整文件旗标扫描，
#      否则注释与 echo 文案里的 --xxx 会淹没结果）；
#   2. `cli-surface: refused`：该调用刻意传未登记的命令或旗标并断言退出 2。
#
# 判定只用二进制的 help：命令必须在 help 的命令清单里，旗标必须在该命令
# help <CMD> 的 options 里。src/cli.rs 只参与前提检查（二进制是不是当前源码的构建
# 产物），不参与旗标判定——源码写错时不能两边一起错。二进制不可执行时直接退 2，
# 不做源码兜底。
#
# 二进制可执行、但 help 的命令清单与 src/cli.rs 对不上时，以退出码 2 报"不是当前源码
# 的构建产物"：陈旧的 tmp/DiskEdit 会把每条调用都判成失效点，掩盖真实回归。
source "$(dirname "$0")/lib.sh"

EXTERNAL="dmsetup sgdisk losetup blockdev partx sfdisk mount umount unshare setpriv"

bad=0
fail_line() { # 文件:行号: 文案
    echo "$1: $2"
    bad=1
}

# src/cli.rs 的登记表：只用于确认二进制出自当前源码，不参与旗标判定
src_commands=$(awk '
    match($0, /name: "[a-z][a-z0-9 -]*"/) {
        print substr($0, RSTART+7, RLENGTH-8)
    }
' "$_repo_root/src/cli.rs" | sort -u)

if ! [ -x "$B" ] || ! "$B" help >/dev/null 2>&1; then
    echo "二进制不可执行：$B"
    echo "命令面校验只认二进制 help，不做源码兜底"
    echo "先构建，或用 DISKEDIT_BIN_DIR 指向本次构建的输出目录"
    exit 2
fi
mode=binary

# 命令清单：everyday / low-level 两段里以两个空格开头的条目；第二词是小写单词的
# 拼成完整命令名（records list / verify / scan）
commands=$("$B" help | awk '
    /^everyday:/ { inseg=1; next }
    /^low-level/ { inseg=1; next }
    /^(target:|options \(|exit status:)/ { inseg=0 }
    inseg && /^  [a-z]/ {
        name=$1
        if ($2 ~ /^[a-z]/) name=name " " $2
        print name
    }
' | sort -u)

# 前提：二进制是当前源码的构建产物。命令清单对不上时，逐旗标比对只会产出一屏
# 与源码无关的失效点，把真实回归埋掉——这种情形直接报错退出，不当作测试违例
if [ "$commands" != "$src_commands" ]; then
    echo "二进制命令面与 src/cli.rs 不一致：$B 不是当前源码的构建产物"
    echo "  源码独有：$(comm -23 <(printf '%s\n' "$src_commands") <(printf '%s\n' "$commands") | tr '\n' ' ')"
    echo "  二进制独有：$(comm -13 <(printf '%s\n' "$src_commands") <(printf '%s\n' "$commands") | tr '\n' ' ')"
    echo "重建二进制，或用 DISKEDIT_BIN_DIR 指向本次构建的输出目录"
    exit 2
fi

declare -A opts_cache
load_opts() { # 命令 -> opts_current（缓存必须在当前 shell 落盘，不能放进 $(...) 子进程）
    local c=$1
    if [ -z "${opts_cache[$c]+x}" ]; then
        opts_cache[$c]=" $("$B" help "$c" </dev/null 2>/dev/null | awk '
            /^    --[a-z]/ { print $1 }
        ' | tr '\n' ' ')"
    fi
    opts_current=${opts_cache[$c]}
}

echo "mode: $mode"

# 两词命令：给抽取器做 "records" + "list" → "records list" 的合并判据
KNOWN2=" $(printf '%s\n' "$commands" | awk 'NF>1 { printf "%s %s ", $1, $2 }')"

CMDS="|$(printf '%s\n' "$commands" | tr '\n' '|')"

cmd_known() {
    case "$CMDS" in *"|$1|"*) return 0 ;; esac
    return 1
}

flag_ok() { # 命令 旗标
    load_opts "$1"
    case "$opts_current " in
        *" $2 "*) return 0 ;;
    esac
    # 注册表之外的两个接受位：解析器里的 --help/--version 出口
    case "$2" in
        --help|--version) return 0 ;;
    esac
    return 1
}

check_invocation() { # 文件 行号 命令 旗标...
    local file=$1 nr=$2 cmd=$3; shift 3
    case " $EXTERNAL " in *" $cmd "*) return 0 ;; esac
    if [ "$cmd" = "help" ] || [ "$cmd" = "--version" ]; then
        [ $# -eq 0 ] && return 0
        fail_line "$file:$nr" "\`$cmd\` 不接受旗标：$(printf '%s ' "$@")"
        return 0
    fi
    if ! cmd_known "$cmd"; then
        fail_line "$file:$nr" "命令 \`$cmd\` 不在命令面内"
        return 0
    fi
    local f
    for f in "$@"; do
        flag_ok "$cmd" "$f" || fail_line "$file:$nr" "旗标 \`$f\` 不在 \`$cmd\` 的命令面内"
    done
}

# ---- shell：拼接续行 → 去引号外注释 → 逐处 $B/$BF... 取调用词与该段旗标 ----
check_shell() {
    local file=$1
    awk -v KNOWN2="$KNOWN2" '
    function strip(s,   i,c,out,sq,dq,esc) {
        sq=0; dq=0; out=""; esc=0
        for (i=1; i<=length(s); i++) {
            c=substr(s,i,1)
            if (esc) { out=out c; esc=0; continue }
            if (c=="\\") { out=out c; esc=1; continue }
            if (c=="\047" && !dq) { sq=!sq; out=out c; continue }
            if (c=="\"" && !sq) { dq=!dq; out=out c; continue }
            if (c=="#" && !sq && !dq && (i==1 || substr(s,i-1,1) ~ /[ \t]/)) break
            out=out c
        }
        return out
    }
    # 同一次调用到 | ; & > ) 为止（引号内的分隔符不算）
    function cut_seg(s,   i,c,out,sq,dq,esc) {
        sq=0; dq=0; out=""; esc=0
        for (i=1; i<=length(s); i++) {
            c=substr(s,i,1)
            if (esc) { out=out c; esc=0; continue }
            if (c=="\\") { out=out c; esc=1; continue }
            if (c=="\047" && !dq) { sq=!sq; out=out c; continue }
            if (c=="\"" && !sq) { dq=!dq; out=out c; continue }
            if (!sq && !dq && index("|;&>)", c)) break
            out=out c
        }
        return out
    }
    function emit(s, nr, raw,   rest,pre,post,p2,cmd,tail,nextw,seg,flags,flag,before) {
        if (raw ~ /cli-surface: refused/) return
        rest=s
        while (match(rest, /\$(BFKO|BFK|BIN|BF|B)/)) {
            pre=substr(rest,1,RSTART-1)
            post=substr(rest,RSTART+RLENGTH)
            rest=post
            if (substr(post,1,1) ~ /[A-Za-z0-9_]/) continue
            # 测试运算符后面的 $B 不是调用： [ ! -x "$B" ] / [ -f "$B" ]
            p2=pre; sub(/"$/, "", p2)
            if (p2 ~ /(^|[ \t])-[A-Za-z][ \t]+$/) continue
            sub(/^[ \t]*"?[ \t]*/, "", post)
            if (post ~ /^(\$|\()/) continue
            cmd=post; sub(/[^A-Za-z0-9_-].*$/, "", cmd)
            if (cmd=="") continue
            tail=post; sub(/^[A-Za-z0-9_-]+[ \t]+/, "", tail)
            nextw=tail; sub(/[^A-Za-z0-9_-].*$/, "", nextw)
            if (nextw!="" && index(KNOWN2, " " cmd " " nextw " ")) cmd=cmd " " nextw
            seg=cut_seg(post)
            flags=""
            while (match(seg, /--[a-z][a-z0-9-]*/)) {
                flag=substr(seg,RSTART,RLENGTH)
                before=substr(seg,1,RSTART-1)
                seg=substr(seg,RSTART+RLENGTH)
                if (before!="" && substr(before,length(before),1) !~ /[ \t=(,]/) continue
                if (index(flags " ", " " flag " ")) continue
                flags=flags " " flag
            }
            print nr "\t" cmd "\t" flags
        }
    }
    BEGIN { buf=""; raw=""; start=0 }
    {
        if (buf=="") { start=NR; raw="" }
        line=strip($0)
        raw=raw " " $0
        if (line ~ /\\[ \t]*$/) { sub(/\\[ \t]*$/, "", line); buf=buf line " "; next }
        emit(buf line, start, raw)
        buf=""
    }
    END { if (buf!="") emit(buf, start, raw) }
    ' "$file"
}

# ---- rust：run(&[...]) / .args([...]) / .args(&[...]) 的字面量数组 ----
# Command::new(...).args(...) 的链式调用是外部工具或变量传参的启动，不参与判定
check_rust() {
    local file=$1
    awk -v KNOWN2="$KNOWN2" '
    function row(s, nr,   n,lit,cmd,flags) {
        n=0; cmd="";
        while (match(s, /"[^"]*"/)) {
            lit=substr(s, RSTART+1, RLENGTH-2)
            s=substr(s, RSTART+RLENGTH)
            n++
            if (n==1) {
                if (lit ~ /^-/) return
                cmd=lit
                continue
            }
            if (n==2 && index(KNOWN2, " " cmd " " lit " ")) { cmd=cmd " " lit; continue }
            if (lit ~ /^--[a-z]/) {
                sub(/=.*$/, "", lit)
                if (!index(flags " ", " " lit " ")) flags=flags " " lit
            }
        }
        if (cmd=="") return
        print nr "\t" cmd "\t" flags
    }
    BEGIN { prev1=""; prev2="" }
    {
        started=0
        if (!in_call) {
            if (match($0, /(run|args)\(&?\[/)) {
                pre=substr($0, 1, RSTART-1)
                if (pre !~ /Command::new\(/ && prev1 !~ /Command::new\(/ && prev2 !~ /Command::new\(/) {
                    start=NR; depth=1; pos=1; refused=0
                    text=substr($0, RSTART+RLENGTH)
                    in_call=1; started=1
                }
            }
        }
        if (in_call && !started) text=text " " $0
        if (in_call) {
            if ($0 ~ /cli-surface: refused/) refused=1
            done=0
            for (i=pos; i<=length(text); i++) {
                c=substr(text, i, 1)
                if (c=="[") depth++
                else if (c=="]") { depth--; if (depth==0) { cut=i; done=1; break } }
            }
            pos=length(text)+1
            if (done) {
                if (!refused) row(substr(text, 1, cut-1), start)
                in_call=0; text=""
            }
        }
        prev2=prev1; prev1=$0
    }
    ' "$file"
}

# 仓库内全部 shell 入口：prune 掉构建产物与第三方源码树
mapfile -t sh_files < <(find "$_repo_root" \
    \( -name .git -o -name target -o -name tmp -o -name node_modules \) -prune -o \
    -name '*.sh' -print | sort)
if [ ${#sh_files[@]} -eq 0 ]; then
    echo "未找到 shell 脚本：$_repo_root"
    exit 2
fi

for f in "${sh_files[@]}"; do
    rel=${f#"$_repo_root"/}
    while IFS=$'\t' read -r nr cmd flags; do
        [ -z "$cmd" ] && continue
        check_invocation "$rel" "$nr" "$cmd" $flags
    done < <(check_shell "$f")
done

for f in "$_repo_root"/tests/*.rs "$_repo_root"/tests/cli/*.rs; do
    [ -f "$f" ] || continue
    while IFS=$'\t' read -r nr cmd flags; do
        [ -z "$cmd" ] && continue
        check_invocation "$(basename "$f")" "$nr" "$cmd" $flags
    done < <(check_rust "$f")
done

if [ "$bad" -ne 0 ]; then
    echo "命令面校验失败"
    exit 1
fi
echo "命令面校验通过（$mode）"
