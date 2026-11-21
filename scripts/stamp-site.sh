#!/bin/sh
# 官网静态文件盖版本号：scripts/stamp-site.sh
# 把 site/ 里 HTML 引用的 style.css、site.js、img/*.png 改成带 `?v=<内容前 8 位 SHA256>` 的地址。
# 官网域名的浏览器缓存是 4 小时，文件一改地址就跟着变，访问过的人不会拿到旧样式、旧截图。
# 改了 site/ 里的样式、脚本或图片后跑一次再提交、部署；内容没变就什么也不改。
set -eu

site=$(CDPATH='' cd -- "$(dirname -- "$0")/../site" && pwd)
cd "$site"

for f in style.css site.js img/*.png; do
	# 先单独算哈希、核对是 8 位十六进制再改 HTML：读不到文件时宁可退出，也不把引用改坏。
	[ -r "$f" ] || {
		echo "读不到 $f" >&2
		exit 1
	}
	sum=$(shasum -a 256 "$f") || exit 1
	v=$(printf '%s' "$sum" | cut -c1-8)
	case $v in
	[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) ;;
	*)
		echo "$f 的哈希不对：$v" >&2
		exit 1
		;;
	esac
	# 文件名里的 . 在正则里转义；引用可能带 ../ 前缀或完整网址，已有的 ?v= 一起换掉。
	pat=$(printf '%s' "$f" | sed 's/\./\\./g')
	for html in index.html en/index.html; do
		sed -i '' -E "s#(${pat})(\?v=[0-9a-f]*)?\"#\1?v=${v}\"#g" "$html"
	done
done
grep -ho '[a-z/._-]*\(css\|js\|png\)?v=[0-9a-f]*' index.html en/index.html | sort -u
