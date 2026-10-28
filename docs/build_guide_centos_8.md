<!-- ZMY SELF-ADDED CODE START -->
# 面向 CentOS 8 的 Linux x86_64 musl 构建指南

本文记录 lightbuild 的一次实际构建经验：在 **Ubuntu 22.04 x86_64** 上交叉构建静态 musl 二进制，供 CentOS 8 一类旧发行版使用。不是在 CentOS 8 上安装 Rust 并原地编译的指南，也不适用于 ARM64。

## 1. 已验证范围

- 源码：[`0f2eaaf8f62eb00b37cf0c940b3a6328cd5c3031`](https://github.com/ZhimingYe/lightbuild/commit/0f2eaaf8f62eb00b37cf0c940b3a6328cd5c3031)。
- 产物：[v1.0.45+zhimingye.4](https://github.com/ZhimingYe/lightbuild/releases/tag/v1.0.45%2Bzhimingye.4)。
- Rust：仓库固定的 `1.94.0`；Ubuntu musl 工具链：`1.2.2-4`。
- 目标：`x86_64-unknown-linux-musl`，保留默认功能，不使用 `target-cpu=native`。
- 最终 ELF：静态、非 PIE、类型为 `EXEC`；没有 `PT_INTERP`、`DT_NEEDED` 或 GLIBC 符号依赖。运行机器不需要安装 musl。
- 已在构建宿主机和断网的 **CentOS Stream 8 容器**中通过 `--version` / `--help`。

**兼容性边界：**容器共享宿主机的 `Linux 6.8.0-1061-aws` 内核，不是原生 CentOS 8 旧内核测试；也没有验证 CentOS Linux 8 GA、完整交互界面、真实模型 API 或完整测试套件。静态链接解决的是用户态动态库依赖，不保证所有旧内核系统调用、沙箱能力或外部工具都可用。Agent 调用的 Git、Shell 等仍依赖目标机器环境。

## 2. 准备环境与固定源码

需要 Rust/rustup、Git、C 编译工具、musl 工具链、DotSlash，以及打包检查用的 `binutils`、`file`、`tar`、`sha256sum`。Docker 仅用于容器验证；`gh` 仅用于发布。

以下是 Ubuntu 上的准备示例，不是历史安装命令的逐字记录；在有权限的构建机器上执行：

```bash
sudo apt-get update
sudo apt-get install -y build-essential musl-tools linux-libc-dev binutils file pkg-config

git clone https://github.com/ZhimingYe/lightbuild.git
cd lightbuild

# 复现本次发布；新版本应替换为已经确认的完整提交 SHA。
git checkout --detach 0f2eaaf8f62eb00b37cf0c940b3a6328cd5c3031
git status --short

# 在仓库内运行，以使用 rust-toolchain.toml 固定的工具链。
rustup target add x86_64-unknown-linux-musl
rustc --version
command -v musl-gcc

# 仅在缺少 DotSlash 时安装；确保 ~/.cargo/bin 在 PATH 中。
command -v dotslash || cargo install dotslash --locked
dotslash --help
```

`bin/` 中的工具包装器（包括 `protoc`）依赖 DotSlash。首次构建会下载 Rust 依赖及工具，因此**构建阶段需要网络**；后面的断网检查针对运行阶段。可使用已有可信缓存，但不要把下载失败误当成源码编译错误。

推荐使用独立、干净的 checkout，避免把其他任务的修改编入产物。可通过 `CARGO_TARGET_DIR` 复用编译缓存，但不要让多个构建同时写同一缓存目录。不要顺手提交 Cargo 自动整理的锁文件；本次发布没有修改源码或依赖版本。

## 3. 成功的构建配方

下面保留本次成功构建的编译参数，只将机器私有路径改为当前 checkout 下的路径。各节 shell 命令按顺序在同一个 Bash 会话中执行。

```bash
set -euo pipefail

export RELEASE_VERSION='1.0.45+zhimingye.4'
export SOURCE_SHA="$(git rev-parse HEAD)"
export CARGO_TARGET_DIR="$PWD/target"

GROK_VERSION="$RELEASE_VERSION" \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS='-C force-unwind-tables=yes -C relocation-model=static -C link-arg=-Wl,-z,relro,-z,now,-z,noexecstack -C link-arg=-static' \
CC_x86_64_unknown_linux_musl=musl-gcc \
CFLAGS_x86_64_unknown_linux_musl='-Du_int8_t=uint8_t -Du_int16_t=uint16_t -Du_int64_t=uint64_t -idirafter /usr/include -idirafter /usr/include/x86_64-linux-gnu' \
cargo build --locked -p xai-grok-pager-bin --release \
  --target x86_64-unknown-linux-musl
```

产物：`$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/xai-grok-pager`。

关键选择：

- `GROK_VERSION` 是项目已有的发布版本注入机制，版本库和入口二进制都读取它；本次注入 `.4`，没有更改源码清单里的 `.3`。检查 `--version`，不能仅凭 Cargo 包版本判断发布版本。
- `--locked` 固定依赖解析；不要为绕过错误而无意升级依赖。
- 使用 `--release`，不是未经本次验证的 `release-dist` profile；不关闭默认功能、jemalloc 或沙箱支持。
- 目标专属 rustflags 会覆盖该目标的配置，因此必须保留 `.cargo/config.toml` 已有的 unwind、RELRO、立即绑定和不可执行栈参数，而不是只设置 `-static`。
- 最终使用 `relocation-model=static` **配合** `-static`，得到静态非 PIE。不要把“musl target”或“加了静态标志”当成最终 ELF 一定静态且能启动的证据。
- `/usr/include/x86_64-linux-gnu` 是本次 Ubuntu x86_64 布局，不应原样套用到其他架构、发行版或工具链。

## 4. 实际踩坑与处理

### 找不到 musl 编译器

观察到：

```text
failed to find tool "x86_64-linux-musl-gcc": No such file or directory
```

Rust 的 musl target 组件不等于完整的 C 工具链。安装 `musl-tools`，再用目标专属 `CC` 和 linker 变量明确指定 `musl-gcc`。

### sqlite-vec 使用 musl 未声明的整数别名

观察到 `u_int8_t`、`u_int16_t`、`u_int64_t` 未知，以及 `uint8_t` 类型冲突。

单独给 C 文件加 `-include sys/types.h` 可以通过该小探针，但把它放进通用 CFLAGS 后也影响汇编输入，产生了：

```text
Error: no such instruction: `typedef unsigned long size_t'
```

最终改为目标专属宏映射：`-Du_int8_t=uint8_t -Du_int16_t=uint16_t -Du_int64_t=uint64_t`，不再向汇编强制注入 C 头文件。该方案是当前依赖和工具链组合的实测兼容办法，不是建议全局修改系统头文件。

### Linux 内核头文件探针失败

AWS-LC 的特性探针曾报告：

```text
fatal error: linux/random.h: No such file or directory
```

追加 `-idirafter /usr/include -idirafter /usr/include/x86_64-linux-gnu` 后，单独探针通过。使用 `-idirafter` 是为了让 musl 自己的头文件优先，再寻找宿主机提供的 Linux 头文件；不要直接用优先级更高的 `-I/usr/include` 混入 glibc 头文件。

这里观察到的是探针失败，不能据此断言它本身一定导致整个构建中止。更换工具链后应重新验证，不能把这一组宿主头路径当成通用 sysroot。

### 编译成功但二进制仍依赖加载器，甚至启动崩溃

本次普通 Cargo 构建产物是带 `/lib/ld-musl-x86_64.so.1` 解释器的动态 PIE，CLI 冒烟检查发生段错误。仅添加 `+crt-static` 的探针仍保留该解释器；最小 Rust 探针仅加 `-static` 虽然变成静态，却在 musl `_start_c`（`dlstart.c:67`）崩溃。

最终同时使用 `-C relocation-model=static -C link-arg=-static`，最小探针和完整程序均能启动。以上是本次 Ubuntu musl 工具链上的观测，不是对所有 musl/static PIE 工具链的结论。非 PIE 的选择也意味着不要宣称产物具备 PIE 地址随机化能力。

## 5. 打包与校验

保持 Cargo 缓存中的原始产物不变，仅剥离发布副本。压缩包根目录放置 `grok`，沿用已有命令名，而不是假定构建会生成 `lightbuild` 命令。

```bash
export RELEASE_DIR="$(dirname "$PWD")/lightbuild-release-$RELEASE_VERSION"
export PAYLOAD="$RELEASE_DIR/payload"
export ARCHIVE="lightbuild-${RELEASE_VERSION/+/-}-linux-x86_64-musl.tar.gz"
mkdir -p "$PAYLOAD"

install -m 0755 \
  "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/xai-grok-pager" \
  "$PAYLOAD/grok"
strip --strip-all "$PAYLOAD/grok"
cp LICENSE THIRD-PARTY-NOTICES "$PAYLOAD/"

(cd "$PAYLOAD" && sha256sum grok LICENSE THIRD-PARTY-NOTICES > SHA256SUMS)
(cd "$PAYLOAD" && sha256sum -c SHA256SUMS)
tar -czf "$RELEASE_DIR/$ARCHIVE" -C "$PAYLOAD" \
  grok LICENSE THIRD-PARTY-NOTICES SHA256SUMS
(cd "$RELEASE_DIR" && sha256sum "$ARCHIVE" > SHA256SUMS)
(cd "$RELEASE_DIR" && sha256sum -c SHA256SUMS)
```

发布目录应为本次版本专用目录，勿混入上次包中的文件。外层 `SHA256SUMS` 校验下载的压缩包，包内的同名文件校验解压后的二进制及许可证文件；两者不要混用。

## 6. 检查实际发布副本，而不只是编译日志

```bash
file "$PAYLOAD/grok"
readelf -hW "$PAYLOAD/grok"
readelf -lW "$PAYLOAD/grok"
readelf -dW "$PAYLOAD/grok"
readelf --version-info "$PAYLOAD/grok"
"$PAYLOAD/grok" --version
"$PAYLOAD/grok" --help >/dev/null
```

验收标准：x86-64 静态 ELF、类型为 `EXEC`、程序头无 `INTERP`、动态段无 `NEEDED`、版本信息无 GLIBC 依赖；`--version` 与预期版本及提交匹配。历史产物输出为 `grok 1.0.45+zhimingye.4 (0f2eaaf8f62e)`。

还应将压缩包解压到独立目录，执行包内 `sha256sum -c SHA256SUMS` 及 `./grok --version`，确保实际分发内容正确。

### CentOS 8 用户态冒烟测试

```bash
docker pull quay.io/centos/centos:stream8
docker run --network none --rm \
  --mount "type=bind,src=$PAYLOAD,dst=/release,readonly" \
  --entrypoint /release/grok quay.io/centos/centos:stream8 --version
docker run --network none --rm \
  --mount "type=bind,src=$PAYLOAD,dst=/release,readonly" \
  --entrypoint /release/grok quay.io/centos/centos:stream8 --help >/dev/null
```

本次 Docker Hub `centos:8` 拉取遇到 HTTP 429，故采用 Quay 的 CentOS Stream 8 镜像。这个替代必须明确披露，不能把结果写成 CentOS Linux 8 GA 或原生旧内核验证。镜像标签可变，后续验证宜记录镜像 digest、`/etc/os-release` 和 `uname -r`。

若需要确认原生 CentOS 8 兼容性，应再在实际旧内核机器或虚拟机上验证启动、交互、模型调用和沙箱/工具执行；换一个容器镜像无法替代内核验证。

## 7. 发布检查清单

1. 固定源码提交；确认发布 tag 未被其他版本占用，不覆盖已有产物。
2. 本地构建并验证，不新增 push/PR CI，不消耗 GitHub Actions 构建额度。
3. 上传压缩包和外层 `SHA256SUMS`；Release 指向实际构建提交。
4. Release 说明写清楚版本注入、静态 ELF 检查结果、实际测试的发行版和内核边界。
5. 发布后检查资产上传状态和服务端 SHA-256，确认与本地一致。

有发布授权时，可使用以下模板；**复现历史构建时不要再次执行发布，也不要覆盖本指南记录的既有 tag**：

```bash
# release-notes.md 由维护者填写本次变更与验证结果。
gh release create "v$RELEASE_VERSION" \
  "$RELEASE_DIR/$ARCHIVE" "$RELEASE_DIR/SHA256SUMS" \
  --repo ZhimingYe/lightbuild --target "$SOURCE_SHA" \
  --title "lightbuild $RELEASE_VERSION — Linux x86_64 musl" \
  --notes-file release-notes.md --latest
```

最重要的经验：**编译通过、目标名含 musl、容器能启动，是三个不同层面的证据。发布前分别检查 ELF 依赖、实际包内容和目标环境启动，并如实记录未验证范围。**
<!-- ZMY SELF-ADDED CODE END -->
