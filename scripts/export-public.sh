#!/bin/sh
# 导出公开版：scripts/export-public.sh <输出目录>
# 从 HEAD 取文件（工作区的改动不算），去掉只给维护者自己看的记录（下面 PRIVATE 列表），
# 再按本机禁用词表（~/.agentq/private/privacy-patterns.json）逐字节扫所有文件和文件名。
# 任何一步不过就退出非 0。不建 git、不联网。输出目录必须不存在或是空的，且不能在仓库里。
set -eu

PRIVATE='PROGRESS.md DECISIONS.md LESSONS.md docs/BACKLOG.md docs/TEST-MATRIX.md docs/specs'
# 公开仓库自己的名字可以出现（README、安装脚本、ai update 要用）；别处单独出现账号名照样算命中。
REPO=dodo258/huitai

if [ "$#" -ne 1 ]; then
	echo '用法：scripts/export-public.sh <输出目录>' >&2
	exit 2
fi
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
patterns=$HOME/.agentq/private/privacy-patterns.json
[ -f "$patterns" ] || {
	echo "找不到禁用词表：${patterns}" >&2
	exit 2
}

mkdir -p "$1"
out=$(CDPATH='' cd -- "$1" && pwd)
[ -z "$(ls -A "$out")" ] || {
	echo "输出目录不是空的：${out}" >&2
	exit 2
}
case $out/ in
	"$root"/*)
		echo "输出目录不能在仓库里：${out}" >&2
		exit 2
		;;
esac

git -C "$root" archive --format=tar HEAD | tar -x -C "$out"
for p in $PRIVATE; do
	rm -rf "${out:?}/$p"
done

# 大文件不进公开仓库。
big=$(find "$out" -type f -size +20M)
[ -z "$big" ] || {
	echo "有超过 20MB 的文件：$big" >&2
	exit 1
}

python3 -I - "$out" "$patterns" "$REPO" <<'EOF'
import json
import os
import sys

out, patterns_path, repo = sys.argv[1], sys.argv[2], sys.argv[3].encode()
with open(patterns_path, encoding="utf-8") as f:
    rules = [
        (p["type"], p["literal"].encode(), bool(p.get("case_sensitive", True)))
        for p in json.load(f)["patterns"]
    ]
if not rules:
    sys.exit("禁用词表是空的")


def hits(data: bytes) -> set[str]:
    data = data.replace(repo, b"TARGET-REPO")
    low = data.lower()
    return {kind for kind, lit, cs in rules if (lit in data if cs else lit.lower() in low)}


found = 0
for base, dirs, files in os.walk(out):
    for name in dirs + files:
        path = os.path.join(base, name)
        rel = os.path.relpath(path, out)
        kinds = hits(rel.encode())
        if name in files and not os.path.islink(path):
            with open(path, "rb") as f:
                kinds |= hits(f.read())
        for kind in sorted(kinds):
            # 只写路径和类别，不写命中的内容。
            print(f"{rel}: {kind}", file=sys.stderr)
            found += 1
if found:
    sys.exit(f"禁用词表命中 {found} 处")
EOF

echo "公开候选：${out}"
