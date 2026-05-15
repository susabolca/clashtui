# clashtui

<p align="right">
<a href="./README.en.md">English</a> | 中文
</p>

`clashtui` 是一个面向 [MetaCubeX/mihomo](https://github.com/MetaCubeX/mihomo)
的终端管理工具。它把订阅、节点选择、多个本地代理端口、DNS、TUN、系统代理和运行状态放在一个 TUI 里管理，减少手写 YAML、反复切换工具和手动排查日志的成本。

它适合已经在使用 mihomo / Clash.Meta，或者希望在服务器、开发机、桌面终端中用键盘完成代理配置的人。

<p align="center">
  <img src="./doc/screen.png" alt="clashtui 终端界面截图" width="900">
</p>

## 解决什么问题

直接使用 mihomo 时，常见问题不是“代理核心能不能跑”，而是日常维护容易分散：

- 订阅、proxy group、DNS、TUN、系统代理和本地端口分别散落在不同配置里。
- 一个本地代理端口不够用，不同程序可能需要走不同订阅、不同节点或不同模式。
- 修改配置后，不容易确认最终生成的 mihomo runtime 配置是否符合预期。
- 网络问题排查时，需要同时看日志、controller 状态、系统路由、DNS 和代理连通性。
- LLM provider、API key、base URL 和 model 信息经常需要在不同供应商之间切换。

`clashtui` 的目标是把这些操作集中到一个终端界面里：配置、启动、停止、检查状态、切换节点、查看运行信息，并在需要时让内置 AI assistant 协助解释和排查。

## 主要能力

- 管理 mihomo runtime，默认使用单进程分发 Global Proxy、DNS、TUN 和多个 Port Proxy。
- 添加和更新订阅，查看流量、到期时间、profile 缓存和刷新状态。
- 查看并切换 proxy group 和节点。
- 创建多个本地 Port Proxy，例如 `127.0.0.1:7071`、`127.0.0.1:7072`。
- 配置系统代理、mihomo DNS 和可选 TUN。
- 下载、选择或使用自定义 mihomo core。
- 使用内置 AI assistant 解释配置、读取日志、检查运行状态，并生成可确认的 draft config patch。
- 支持中文和英文界面，专业术语会尽量保留英文，例如 Runtime、DNS、TUN、Provider、Model、Base URL、Port Proxy 和 mihomo。

## 快速开始

构建 release 版本：

```bash
cargo build --release
```

打开配置界面：

```bash
target/release/clashtui config
```

使用中文界面：

```bash
target/release/clashtui -l zh config
```

也可以使用 `-l cn`。默认语言是英语：

```bash
target/release/clashtui -l en config
```

启动、查看状态、停止或重启 runtime：

```bash
target/release/clashtui start
target/release/clashtui status
target/release/clashtui stop
target/release/clashtui restart
```

## 基本使用流程

1. 运行 `clashtui config`。
2. 在 `Subscription` 页面添加订阅。
3. 在 `Main` 页面启用 `Global Proxy`，或者添加一个或多个 Port Proxy。
4. 进入 `Exit` 页面选择 `Save & Restart`。
5. 用本地代理端口测试连通性：

```bash
curl -x http://127.0.0.1:7070 -I https://www.gstatic.com/generate_204
```

如果你添加了 Port Proxy，也可以测试对应端口：

```bash
curl -x http://127.0.0.1:7071 -I https://www.gstatic.com/generate_204
curl -x http://127.0.0.1:7072 -I https://www.gstatic.com/generate_204
```

## 常见使用场景

### 一个默认代理端口

只需要给系统或浏览器提供一个本地代理时，可以使用默认的 `Global Proxy`。默认监听地址是：

```text
127.0.0.1:7070
```

### 多个程序使用不同节点

如果不同程序需要固定走不同节点，可以创建多个 Port Proxy。比如一个端口固定走香港节点，另一个端口固定走日本节点：

```text
127.0.0.1:7071 -> HK proxy
127.0.0.1:7072 -> JP proxy
```

这样不需要为每个程序单独维护一份 mihomo 配置。

### 需要 DNS 或 TUN

需要接管 DNS 或透明代理时，可以在 TUI 中配置 mihomo DNS 和 TUN。macOS/Linux 上完整 TUN 功能需要安装 privileged service；如果没有安装 service，普通 Global Proxy 和 Port Proxy 仍然可以使用。

## AI Assistant

`Chat` 页面是 clashtui 内置的 OpenAI-compatible assistant。它不是外部 Claude Code 或 Codex session，而是专门面向 clashtui 和 mihomo 使用场景设计的辅助功能。

它可以帮助你：

- 解释当前配置为什么这样生效。
- 检查 draft config、生成的 mihomo runtime 配置和日志。
- 查询 mihomo controller，了解 version、config、proxy group 和连接状态。
- 执行有限的只读诊断命令，例如 `ping`、`dig`、`nslookup`、`ip`、`route`、`netstat`、`lsof`、`ps`。
- 通过 HTTP probe 检查直连或代理连通性。
- 生成 draft config patch，并在你确认后应用到当前配置。

assistant 不会自动保存、重启或直接修改生成的 runtime 文件。配置变更仍然需要你在 TUI 中确认，然后手动 `Save` 或 `Save & Restart`。

当前 assistant 主要使用本地知识、当前 runtime 状态、日志和明确的 probe 结果工作，不提供通用远程网页搜索。

## LLM Provider

LLM 配置在 `Runtime` 页面里的 `LLM` 分区完成：

- 选择内置 provider preset，或填写自定义 provider。
- 配置 `Base URL`、`Model` 和 `API Key`。
- 使用 `Test Assistant` 发送一个简单请求，确认 provider 可用。
- 手动执行 `Update LLM Providers` 时，clashtui 会把当前版本内置的 provider 信息合并到本地配置。

API key、用户自定义 model 和自定义 provider 会保存在本地 `llm-providers.yaml` 中。更新内置 provider 时会做合并，不会简单覆盖本地文件。

面向中国地区的 provider preset 会区分普通 API endpoint 和 coding plan endpoint。部分供应商的 coding plan 可能有独立 base URL、model、key 或额度池，排查鉴权、额度和 `model-not-found` 问题时需要一起检查。

## TUI 页面

- `Main`：运行状态、Global Proxy、Port Proxy。
- `Subscription`：订阅、profile 缓存、流量和到期时间。
- `DNS`：mihomo DNS、nameserver、fallback、fake-IP 和策略配置。
- `Runtime`：service、日志、mihomo core、controller、LLM 设置和 provider 更新。
- `Chat`：AI assistant 对话、配置解释、问题排查和 draft patch 确认。
- `Exit`：保存、启动、停止、重启、恢复默认值和退出。

常用按键：

- `Up` / `Down`：移动选择。
- `Enter`：打开、编辑或执行。
- `Esc`：返回；在根页面会进入 Exit 页面。
- `Tab` / `Left` / `Right`：在 section root 上切换页面。
- `F9`：确认后加载默认值。
- `F10`：确认后保存并重启。

## 配置位置

Linux 上默认配置目录：

```text
~/.config/clashtui/
```

常见文件：

```text
config.yaml
llm-providers.yaml
profiles/
cores/
mihomo-run.yaml
mihomo-active.yaml
*.log
```

macOS 上默认配置目录：

```text
~/Library/Application Support/clashtui/
```

## 开发

普通开发只需要 Rust toolchain 和 Cargo：

```bash
cargo run -- config
cargo run -- start --verbose
cargo run -- status --verbose
cargo run -- stop --verbose
```

常用检查：

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test
```

如果要开发或测试 service/TUN 相关功能，需要 macOS 或 Linux，并从已构建的二进制安装 privileged service：

```bash
cargo build
target/debug/clashtui service-install --path target/debug/clashtui
target/debug/clashtui service-status
target/debug/clashtui service-uninstall
```
