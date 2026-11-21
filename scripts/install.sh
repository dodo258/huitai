#!/bin/sh
# 安装会话台（huitai）的 `ai` 命令：
#   curl -fsSL https://github.com/dodo258/huitai/releases/latest/download/install.sh | sh
# 下载本机芯片对应的预编译程序，SHA256 对上才放进 ~/.local/bin（HUITAI_INSTALL_DIR 可改），不用 sudo。
# 已经装过再跑一遍就是升级；在后台跑着的对话不受影响。
set -eu

repo=${HUITAI_REPO:-dodo258/huitai}
dir=${HUITAI_INSTALL_DIR:-$HOME/.local/bin}

say() { printf '%s\n' "$*"; }
die() {
	printf 'huitai: %s\n' "$*" >&2
	exit 1
}

[ "$(uname -s)" = Darwin ] || die '只支持 macOS / macOS only'
case $(uname -m) in
	arm64 | aarch64) arch=aarch64 ;;
	x86_64) arch=x86_64 ;;
	*) die "不认识的芯片 / unsupported CPU: $(uname -m)" ;;
esac
command -v curl >/dev/null 2>&1 || die '需要 curl / curl is required'

name=ai-$arch-apple-darwin
base=https://github.com/$repo/releases/latest/download
mkdir -p "$dir" || die "建不了目录 / cannot create $dir"
# 在目标目录里新建只有自己能进的暂存目录（mktemp -d：随机名、0700、已存在就失败），
# 下载、校验、改名都在里面做：别人没法预先放链接或掉包，最后的 mv 在同一个磁盘上是原子改名。
tmp=$(mktemp -d "$dir/.ai-install.XXXXXX") || die "没法在 $dir 里建暂存目录 / cannot create a staging directory in $dir"
trap 'rm -rf "$tmp"' EXIT

say "下载 / downloading $name"
curl -fsSL --retry 2 --connect-timeout 15 -o "$tmp/$name" "$base/$name" || die '下载失败 / download failed'
curl -fsSL --retry 2 --connect-timeout 15 -o "$tmp/$name.sha256" "$base/$name.sha256" || die '下载校验和失败 / checksum download failed'
want=$(awk '{print $1; exit}' "$tmp/$name.sha256")
got=$(shasum -a 256 "$tmp/$name" | awk '{print $1}')
[ -n "$want" ] && [ "$want" = "$got" ] || die '校验和对不上，没有安装 / checksum mismatch, nothing installed'

chmod 755 "$tmp/$name"
# 校验过的就是改名过去的这个文件；换掉正在用的旧版也不会半截。
mv -f "$tmp/$name" "$dir/ai"

version=$("$dir/ai" --version 2>/dev/null || true)
say "已安装 / installed: $dir/ai ${version}"
case ":$PATH:" in
	*":$dir:"*) say '敲 ai 开始 / run: ai' ;;
	*)
		say "$dir 不在 PATH 里，把这行加进 ~/.zshrc 后重开终端："
		say "$dir is not on your PATH; add this line to ~/.zshrc and open a new terminal:"
		say "  export PATH=\"$dir:\$PATH\""
		;;
esac
