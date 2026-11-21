#!/bin/sh
# 发行构建：scripts/build-release.sh <输出目录>
# 产出 Apple 芯片和 Intel 两个可执行文件 ai-<arch>-apple-darwin、各自的 .sha256（`ai update` 按这两个名字下载和校验）、
# install.sh 和总的 SHA256SUMS。版本号取 Cargo.toml。只构建，不打 tag、不上传。
set -eu

if [ "$#" -ne 1 ]; then
	echo '用法：scripts/build-release.sh <输出目录>' >&2
	exit 2
fi
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
mkdir -p "$1"
out=$(CDPATH='' cd -- "$1" && pwd)
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)
[ -n "$version" ] || {
	echo 'Cargo.toml 里没找到版本号' >&2
	exit 1
}

cd "$root"
# 依赖库的源码路径会随报错位置编进二进制；换成中性路径，发行包里不带构建机的家目录。
cargo_home=${CARGO_HOME:-$HOME/.cargo}
RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$root=/huitai --remap-path-prefix=$HOME=/home"
export RUSTFLAGS
files=
for arch in aarch64 x86_64; do
	target=$arch-apple-darwin
	rustup target list --installed | grep -qx "$target" || rustup target add "$target"
	echo "构建 $target"
	cargo build --release --locked --target "$target"
	name=ai-$target
	cp "target/$target/release/ai" "$out/$name"
	chmod 755 "$out/$name"
	(cd "$out" && shasum -a 256 "$name" >"$name.sha256")
	files="$files $name $name.sha256"
done

cp "$root/scripts/install.sh" "$out/install.sh"
chmod 644 "$out/install.sh"
files="$files install.sh"

# shellcheck disable=SC2086 # 文件名是上面拼的固定字符，要按空格拆开
(cd "$out" && shasum -a 256 $files >SHA256SUMS)
echo "v$version 已产出到 ${out}："
(cd "$out" && cat SHA256SUMS)
