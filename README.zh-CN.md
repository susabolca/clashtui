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
- 包含 LLM Chat 页面和面向 LLM 的配置 spec。Chat 集成仍在开发中。

## 快速开始

构建：

```bash
cargo build --release
```

打开配置界面：

```bash
target/release/clashtui config
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
- `Runtime`：service、自启动、日志、mihomo core、controller、DNS。
- `Chat`：LLM 辅助配置预览，仍在开发中。
- `Exit`：保存、start、stop、reload、restart、默认值和退出动作。

常用按键：

- `Up` / `Down`：移动选择。
- `Enter`：打开、编辑或执行。
- `Esc`：返回；在根页面会进入 Exit 页面。
- `Tab` / `Left` / `Right`：在 section root 上切换页面。
- `F9`：确认后加载默认值。
- `F10`：确认后保存并重启。

## 配置文件

用户配置位于仓库外：

```text
~/.config/clashtui/config.yaml
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
