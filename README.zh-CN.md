# clashtui

<p align="right">
<a href="./README.md">English</a> | 中文
</p>

`clashtui` 是一个面向
[MetaCubeX/mihomo](https://github.com/MetaCubeX/mihomo) 的终端 UI 和后台控制器。
它本身不是代理核心，而是负责管理 mihomo 配置、订阅、本地监听端口、运行时状态、系统代理、DNS，以及可选的 TUN。

<p align="center">
  <img src="./doc/screen.png" alt="clashtui 终端界面截图" width="900">
</p>

## 基本能力

- 运行一个 Global Proxy，本地默认监听 `127.0.0.1:7070`。
- 添加多个 Port Proxy，例如 `127.0.0.1:7071`、`127.0.0.1:7072` 等。
- 每个 Port Proxy 可以选择自己的订阅、模式和代理节点。
- 默认使用单 mihomo runtime：Global Proxy、DNS、TUN 和所有 Port Proxy listeners 会生成到同一个 mihomo 配置中。
- 支持订阅、代理组、节点选择、流量用量、到期时间和本地 profile 缓存。
- 支持系统代理、mihomo DNS，以及可选 TUN 模式。
- macOS/Linux 上提供 privileged service 模式用于 TUN；不安装 service 时，普通 Port Proxy 功能仍可正常使用。
- 支持自定义 mihomo 二进制，也可以在 clashtui 配置目录中下载和管理 MetaCubeX/mihomo。
- 包含原生 LLM Chat assistant，可解释运行时行为、检查日志/配置，并生成安全的 draft 配置 patch。
- LLM provider 预设维护在 `llm-providers.yaml` 中；API key 和自定义模型保留在本地，内置 provider 更新只会在用户手动执行 Runtime 更新动作时合并。

## 快速开始

构建：

```bash
cargo build --release
```

打开配置界面：

```bash
target/release/clashtui config
```

TUI 默认使用英语。需要简体中文 UI 和 assistant 回复时，可以使用 `--language zh-CN`。
专业术语会尽量保留英文，例如 LLM、Runtime、DNS、TUN、Provider、Model、Base URL、Port Proxy 和 mihomo：

```bash
target/release/clashtui --language zh-CN config
```

启动后台 runtime：

```bash
target/release/clashtui start
```

查看状态：

```bash
target/release/clashtui status
```

停止或重启：

```bash
target/release/clashtui stop
target/release/clashtui restart
```

开发时可以直接使用：

```bash
cargo run -- config
cargo run -- start
cargo run -- status
```

## 如何开发

环境要求：

- 安装 Rust toolchain 和 Cargo。
- 完整的 service/TUN 开发需要 macOS 或 Linux。普通配置界面、状态检查和用户态 runtime 流程不需要安装 privileged service。
- 准备一个 mihomo 二进制，可以在 TUI 中配置，也可以通过 `MIHOMO_CORE` 指定，或者让 clashtui 下载托管 core。

常用本地检查：

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test
```

开发时可以直接运行 debug build：

```bash
cargo run -- config
cargo run -- start --verbose
cargo run -- status --verbose
cargo run -- stop --verbose
```

使用本地 mihomo 二进制测试：

```bash
MIHOMO_CORE=/path/to/mihomo cargo run -- start --verbose
```

生成的配置、profile 缓存、日志和托管 core 会写到用户配置目录，不会写入仓库。需要干净环境时，可以删除或编辑 `~/.config/clashtui/config.yaml`。

service 和 TUN 相关功能需要从已构建的二进制安装 privileged service：

```bash
cargo build
target/debug/clashtui service-install --path target/debug/clashtui
target/debug/clashtui service-status
target/debug/clashtui service-uninstall
```

安装命令会使用 `sudo`，并把指定二进制复制到系统 service 位置；修改 service 侧代码后，需要重新构建并重新安装。

## 基本使用流程

1. 运行 `clashtui config`。
2. 在 `Subscription` 页面添加订阅。
3. 在 `Main` 页面配置 `Global Proxy`，或者添加 Port Proxy。
4. 进入 `Exit` 页面选择 `Save & Restart`。
5. 测试本地代理端口：

```bash
curl -x http://127.0.0.1:7070 -I https://www.gstatic.com/generate_204
curl -x http://127.0.0.1:7071 -I https://www.gstatic.com/generate_204
curl -x http://127.0.0.1:7072 -I https://www.gstatic.com/generate_204
```

## AI Assistant

`Chat` 页面是 clashtui 内置的 OpenAI-compatible assistant，不是外部 Claude
Code/Codex session。它使用 `Runtime` -> `LLM` 分区中配置的 provider、base
URL、model 和 API key。

assistant 可以做的事：

- 在 `Chat` 页面流式回答运行时解释、配置问题和故障排查。
- 通过 Runtime LLM 分区里的 `Test Assistant` 发送一个小的 `hello` 请求，并在 popup 中显示流式 response 或错误。
- 读取当前 draft config，并对 secret 做隐藏处理。
- 检查生成的 mihomo runtime 文件，例如 `mihomo-run.yaml` 和 `mihomo-active.yaml`。
- 读取有限行数的 clashtui/mihomo 日志尾部。
- 查询 mihomo controller，获取 version、config 和 proxy group 摘要。
- 直接或通过指定 proxy URL 执行有限的 HTTP probe。
- 只运行 allowlist 中的只读诊断命令，例如 `ping`、`dig`、`nslookup`、`ip`、`route`、`netstat`、`lsof`、`ps` 等。
- 生成经过验证的结构化 draft config patch。用户仍然需要在 Chat 中确认应用 patch，然后手动 Save 或 Save & Restart；assistant 不会自动保存、重启，也不会直接编辑生成的 runtime 文件。

assistant 内置了 clashtui 和 mihomo 的本地知识，包括 runtime 生成逻辑、配置语义、patch 规则、mihomo config spec、DNS、TUN、系统代理、订阅、Port Proxy、LLM provider 和故障排查说明。它主要基于本地知识、当前 runtime 状态、日志和显式 probe 工作；当前没有通用的远程网页搜索工具。

LLM 配置保存在本地：

- `Runtime` -> `LLM Provider` 选择内置或自定义 provider preset。
- `Runtime` -> `LLM Base URL` 和 `Runtime` -> `LLM Model` 可以覆盖所选 preset。
- `Runtime` -> `LLM API Key` 会把 key 保存到本地 `llm-providers.yaml`。
- Model ID 是普通字符串；输入自定义 model 后会追加到本地 provider catalog。
- `Runtime` -> `Update LLM Providers` 由用户手动执行，会把当前二进制内置的 provider catalog 合并到本地文件，同时保留 API key、自定义 model ID 和自定义 provider。

面向中国地区的 provider preset 会区分普通按量 API 和 coding plan endpoint。部分供应商的 coding plan 有独立 base URL、model、key 或额度池，例如 Kimi Platform/Kimi Code、Qwen DashScope/Qwen Coding Plan、Volcengine Ark/Ark Coding Plan、Baidu Qianfan/Qianfan Coding Plan、GLM normal/coding endpoint 都会作为不同 preset 处理。排查鉴权、额度或 model-not-found 问题时，需要同时检查 provider preset、base URL、model ID 和 API key 来源。

## 多 Port Proxy

当一个本地代理端口不够用时，Port Proxy 是 clashtui 的核心使用场景。每个服务暴露一个 HTTP、SOCKS5 或 mixed listener，并且可以使用不同订阅或不同节点。

配置形态示例：

```yaml
proxy_ports:
  services:
    - name: hk-mixed
      enabled: true
      kind: mixed
      listen: 127.0.0.1
      port: 7071
      subscription: work
      mode: global
      proxy: HK-01

    - name: jp-mixed
      enabled: true
      kind: mixed
      listen: 127.0.0.1
      port: 7072
      subscription: personal
      mode: global
      proxy: JP-01
```

在默认的 `service` 或 `single` backend 中，这些 Port Proxy 会作为 mihomo `listeners` 运行在同一个 mihomo 进程里。`multi` / `multi-process` 仅作为兼容旧架构的 backend 保留。

## Service 与 TUN

默认 backend 是：

```yaml
runtime_backend: service
```

安装 privileged service 后，service 模式会启动一个由 service/root 持有的 mihomo runtime。这个 mihomo 同时负责 TUN、DNS、Global Proxy 和所有 Port Proxy listeners。

安装 service：

```bash
target/release/clashtui service-install
```

查看状态或卸载：

```bash
target/release/clashtui service-status
target/release/clashtui service-uninstall
```

如果 service 未安装或不可达，`clashtui start` 会在本次运行中回退到普通用户模式的单 mihomo runtime。Global Proxy 和 Port Proxy 仍可工作，但 TUN 会被禁用，因为创建 TUN 设备和路由需要特权。

## Mihomo Core

默认配置是 `mihomo.core: auto`。启动时按以下顺序查找 core：

1. 配置中的 `core_path`。
2. `MIHOMO_CORE` 环境变量。
3. clashtui 配置目录中托管的 MetaCubeX/mihomo release。
4. 系统中已安装的 mihomo/verge-mihomo。

如果没有可用 core，clashtui 可以在启动时下载托管的 stable mihomo release。Runtime 页面也提供 core 来源选择和更新动作。

## TUI 说明

页面：

- `Main`：运行时摘要、Global Proxy、Port Proxy 列表、Add Port Proxy。
- `Subscription`：订阅列表、profile 缓存、流量、到期时间、刷新状态。
- `DNS`：mihomo DNS、nameserver、fallback、fake-IP 和策略配置。
- `Runtime`：service、自启动、日志、mihomo core、controller、LLM 设置，以及手动 LLM provider catalog 更新。
- `Chat`：LLM 辅助配置、运行时解释、问题排查和 draft patch 确认。
- `Exit`：保存、start、stop、reload、restart、默认值和退出动作。

常用按键：

- `Up` / `Down`：移动选择。
- `Enter`：打开、编辑或执行。
- `Esc`：返回；在根页面会进入 Exit 页面。
- `Tab` / `Left` / `Right`：在 section root 上切换页面。
- `F9`：确认后加载默认值。
- `F10`：确认后保存并重启。

语言：

- `--language en`：英语 UI 和 assistant 偏好。这是默认值。
- `--language zh-CN`：简体中文 UI 和 assistant 偏好，专业术语会在更清晰时保持英文。

## 配置文件

用户配置位于仓库外：

```text
~/.config/clashtui/config.yaml
~/.config/clashtui/llm-providers.yaml
~/.config/clashtui/profiles/
~/.config/clashtui/cores/
~/.config/clashtui/mihomo-run.yaml
~/.config/clashtui/mihomo-active.yaml
~/.config/clashtui/*.log
```

macOS 上配置目录位于：

```text
~/Library/Application Support/clashtui/
```

service/root 持有的 mihomo 状态会放在普通用户配置目录之外，避免 root-owned 文件污染用户 runtime 文件。
