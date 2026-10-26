<!-- ZMY SELF-ADDED CODE START -->
# lightbuild

lightbuild 是基于 **Grok Build** 的独立 fork，遵循 **Apache License, Version 2.0**，为终端开发工作流提供 AI 编程助手。

项目保留全屏终端界面（TUI）、非交互式运行和 Agent Client Protocol（ACP）集成，支持连接本地模型或自行配置的模型 API 服务。lightbuild 并非 SpaceXAI 官方产品；上游代码的版权与第三方许可声明予以保留。

<!-- 删除上游品牌图片、截图与推广入口；本 README 使用纯文本介绍 lightbuild。 -->

## 功能

- **终端交互**：全屏 TUI、键盘快捷键、会话管理与主题配置。
- **编程工具**：读取和编辑文件、搜索代码、执行终端命令。
- **模型接入**：支持 OpenAI Chat Completions、OpenAI Responses 和 Anthropic Messages 协议，以及兼容这些协议的本地服务。
- **自动化**：通过 headless 模式在脚本中运行任务，或通过 ACP 与客户端集成。
- **扩展与权限**：支持 MCP、skills、plugins、hooks，以及工具权限和沙箱配置。

## 自定义更改

lightbuild 在上游基础上包含以下自定义更改：

1. **支持 `max` 思考档位**：支持通过 `--reasoning-effort max`（或 `--effort max`）选择最高思考档位，也可在交互界面使用 `/effort max`。实际可选档位与效果取决于模型及服务端支持，并非所有模型都支持 `max`。
2. **移除遥测上报**：在 fork 的能力策略中禁用遥测与错误上报，包括产品事件、Mixpanel、内置及外部 OpenTelemetry 导出和 Sentry 错误上报。此处指禁用远程上报，并不代表删除所有相关源码或本地调试日志，也不影响用户主动配置的模型 API 请求。

## 获取与构建

发布产物（如有）见 [lightbuild Releases](https://github.com/ZhimingYe/lightbuild/releases)。也可以直接从源码构建。

### 环境要求

- **Rust**：使用 [`rust-toolchain.toml`](rust-toolchain.toml) 固定的工具链，`rustup` 会在构建时按需安装。
- **DotSlash**：[`bin/`](bin/) 下的工具包装器（包括 `protoc`）需要它下载并运行对应工具。构建前确保 `dotslash` 位于 `PATH` 中。
- **Protocol Buffers 编译器**：默认通过 [`bin/protoc`](bin/protoc) 调用，也可通过 `PROTOC` 或 `PATH` 提供 `protoc`。

主要构建环境为 Linux 和 macOS；Windows 构建尚未在本仓库验证。

```sh
git clone https://github.com/ZhimingYe/lightbuild.git
cd lightbuild

cargo install dotslash
dotslash --help

# 构建并启动终端界面
cargo run -p xai-grok-pager-bin

# 构建 release 二进制
cargo build -p xai-grok-pager-bin --release
```

构建产物为 `target/release/xai-grok-pager`。项目名为 **lightbuild**，但源码中的 crate 名称、二进制名称和配置路径仍沿用上游命名；本文不会假定系统中已有名为 `lightbuild` 的命令。

## 配置模型

无需强制登录 Grok 账户即可进入界面，但运行模型任务前仍需配置可用的模型服务，以及该服务要求的凭据。

在 `~/.grok/config.toml` 中配置自定义模型，例如：

```toml
[models]
default = "my-model"

[model.my-model]
name = "My Model"
model = "your-model-id"
base_url = "https://api.example.com/v1"
api_backend = "chat_completions"
env_key = "MODEL_API_KEY"
```

将 `model` 和 `base_url` 替换为实际模型标识与服务地址，并在当前环境中设置 `MODEL_API_KEY`。不要把真实密钥提交到仓库。连接本地服务时，请使用该服务实际支持的协议与认证配置。

更多参数见[自定义模型指南](crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md)。

## 使用

以下示例从仓库根目录运行，使用前面的 release 构建产物：

```sh
# 交互式终端界面
./target/release/xai-grok-pager -m my-model

# 非交互式任务
./target/release/xai-grok-pager -m my-model -p "解释这个项目的目录结构"

# 查看命令行参数
./target/release/xai-grok-pager --help
```

Agent 可调用文件和终端工具。请在可信工作目录中使用，并按需要设置[权限与安全策略](crates/codegen/xai-grok-pager/docs/user-guide/22-permissions-and-safety.md)。

## 文档

完整指南位于 [`docs/user-guide/`](crates/codegen/xai-grok-pager/docs/user-guide/)，常用入口：

- [快捷键](crates/codegen/xai-grok-pager/docs/user-guide/03-keyboard-shortcuts.md)与[斜杠命令](crates/codegen/xai-grok-pager/docs/user-guide/04-slash-commands.md)
- [配置](crates/codegen/xai-grok-pager/docs/user-guide/05-configuration.md)与[自定义模型](crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md)
- [MCP 服务](crates/codegen/xai-grok-pager/docs/user-guide/07-mcp-servers.md)、[Skills](crates/codegen/xai-grok-pager/docs/user-guide/08-skills.md)与[Plugins](crates/codegen/xai-grok-pager/docs/user-guide/09-plugins.md)
- [Headless 模式](crates/codegen/xai-grok-pager/docs/user-guide/14-headless-mode.md)与[沙箱](crates/codegen/xai-grok-pager/docs/user-guide/18-sandbox.md)

部分指南继承自上游，仍使用 `grok` 命令及 Grok 品牌名称；其中涉及上游登录、托管服务或安装渠道的说明，不代表 lightbuild 提供或要求使用这些服务。

## 开发

主要代码位于 `crates/codegen/`：`xai-grok-pager-bin` 是二进制入口，`xai-grok-pager` 提供 TUI，`xai-grok-shell` 提供 Agent 运行时，`xai-grok-tools` 实现工具，`xai-grok-workspace` 处理工作区操作。

```sh
cargo check -p xai-grok-pager-bin
cargo test -p xai-grok-config
cargo clippy -p xai-grok-pager-bin
cargo fmt --all
```

请按需针对单个 crate 运行检查，避免不必要的全工作区构建。根目录的 `Cargo.toml` 是生成文件，依赖调整优先在各 crate 的清单中进行。fork 自定义修改须遵循 [`AGENTS.md`](AGENTS.md) 的成对注释标记约定。

## 来源与许可证

lightbuild fork 自 Grok Build，基于 **Apache License, Version 2.0** 继续开发与分发。上游版权声明为 **Copyright 2023–2026 SpaceXAI**；完整许可条款见 [`LICENSE`](LICENSE)。本项目的名称与维护不代表上游官方背书。

第三方依赖和 vendored 代码保留各自的许可证及归属声明，详见：

- [`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES)：依赖、主题及源码移植的声明。
- [`crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md`](crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md)：工具层移植代码的许可与修改声明。
- [`third_party/NOTICE`](third_party/NOTICE)：vendored Mermaid 相关组件的声明索引。
<!-- ZMY SELF-ADDED CODE END -->
