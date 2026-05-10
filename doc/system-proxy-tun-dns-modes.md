# System Proxy, TUN, and DNS Modes

调研日期：2026-05-10

本文记录 Clash Verge Rev 中 `System Proxy`、`TUN` 和 `DNS` 三类网络模式的实现边界、配置链路、平台差异和相互影响。结论基于当前仓库代码、`sysproxy-rs` 依赖实现，以及 mihomo 官方配置文档。

## 一句话结论

- `System Proxy` 是操作系统代理设置，不是 mihomo 的入站模式。它把系统 HTTP/SOCKS 或 PAC 代理指向本机 `mixed-port`，只覆盖尊重系统代理的应用。
- `TUN` 是 mihomo 的虚拟网卡入站。它在 IP 层接管流量，通常用于覆盖不读取系统代理的应用，并依赖管理员权限或 Clash Verge Service。
- `DNS` 是 mihomo 内置 DNS 模块及本项目的独立 `dns_config.yaml` 覆写机制。TUN 开启时还会自动补齐 DNS 以保证域名流量能进入 mihomo 分流。

## 模式边界

| 模式 | 主要层级 | 本项目开关 | 是否写入 mihomo YAML | 主要配置文件/位置 | 覆盖范围 |
| --- | --- | --- | --- | --- | --- |
| System Proxy | OS 应用层代理 | `verge.enable_system_proxy` | 否，只依赖 mihomo 已监听的代理端口 | `verge.yaml`、OS 代理设置 | 尊重系统代理的应用 |
| PAC System Proxy | OS 应用层自动代理 | `enable_system_proxy + proxy_auto_config` | 否，PAC 服务由 Clash Verge 内嵌 HTTP server 提供 | `verge.yaml`、`/commands/pac` | 尊重 PAC 的应用 |
| TUN | mihomo IP 层入站 | `verge.enable_tun_mode` | 是，写入最终 `tun.enable` | `clash-verge.yaml` 最终运行配置 | 多数 TCP/UDP 流量 |
| DNS Override | mihomo DNS 模块 | `verge.enable_dns_settings` | 是，替换/注入最终 `dns` 与 `hosts` | App Home 下 `dns_config.yaml` | mihomo DNS 查询与被 TUN 劫持的 DNS |

核心区别：`System Proxy` 是“把应用引到 mihomo 代理端口”，`TUN` 是“把系统路由引到 mihomo 虚拟网卡”，`DNS` 是“让 mihomo 掌握域名解析结果和域名分流上下文”。

## 配置生成顺序

最终运行配置由 `src-tauri/src/enhance/mod.rs` 生成，关键顺序如下：

1. 读取订阅 profile。
2. 应用全局/订阅级 Merge、Script、Rules、Proxies、Groups。
3. 合并 Clash Verge 默认 Clash 配置，包括端口、controller、基础 `tun`。
4. 应用内置增强脚本。
5. 清理无效 proxy group 引用。
6. 调用 `use_tun(config, enable_tun)`，按 `enable_tun_mode` 更新 `tun.enable`，必要时补 DNS。
7. 排序。
8. 如果 `enable_dns_settings` 为真，再应用 `dns_config.yaml`。因此 DNS 覆写优先级高于 TUN 自动补齐的 DNS。

这意味着：用户打开 TUN 时会先得到一份可工作的基础 DNS；如果用户同时打开 DNS 覆写，最终 DNS 以独立 DNS 设置为准。

## System Proxy

### 作用

System Proxy 只修改操作系统代理配置。mihomo 侧必须有可用的 HTTP/SOCKS/mixed 监听端口，本项目默认使用 `mixed-port`。mihomo 官方文档说明 `mixed-port` 同时支持 HTTP(S) 与 SOCKS5 客户端连接。

### 本项目状态字段

主要字段位于 `src-tauri/src/config/verge.rs`：

- `enable_system_proxy`：总开关。
- `proxy_auto_config`：是否使用 PAC 模式。
- `pac_file_content`：PAC 脚本文本。
- `proxy_host`：写入 OS 代理或 PAC URL 的主机，默认 `127.0.0.1`。
- `system_proxy_bypass`：用户自定义绕过列表。
- `use_default_bypass`：是否追加内置绕过列表。
- `enable_proxy_guard`、`proxy_guard_duration`：代理守卫。
- `enable_bypass_check`：前端绕过列表格式检查。

模板默认值：

- `enable_system_proxy: false`
- `proxy_auto_config: false`
- `proxy_host: 127.0.0.1`
- `verge_mixed_port: 7897`
- `enable_proxy_guard: false`
- `use_default_bypass: true`

### 开关链路

前端开关：

- `src/hooks/use-system-proxy-state.ts`
- `src/components/shared/proxy-control-switches.tsx`
- `src/components/setting/mods/sysproxy-viewer.tsx`

后端链路：

- 前端调用 `patchVerge({ enable_system_proxy: target })`。
- `src-tauri/src/feat/config.rs` 识别 `enable_system_proxy` 后设置 `SYS_PROXY` 更新标记。
- `src-tauri/src/core/sysopt.rs::update_sysproxy()` 读取当前 Verge 配置并写入 OS。
- `src-tauri/src/cmd/network.rs::get_sys_proxy()` 查询 OS 实际代理状态，用于 UI 指示器。

### 普通系统代理模式

当 `enable_system_proxy=true` 且 `proxy_auto_config=false`：

- `Sysproxy.host = proxy_host`
- `Sysproxy.port = verge_mixed_port`，没有时回退到当前 Clash 配置 `mixed-port`
- `Sysproxy.bypass = 默认绕过 + 用户绕过`
- `Autoproxy.enable = false`

应用顺序有平台兼容处理：纯系统代理模式会先清 PAC，再启用固定代理，避免 Windows WinINET 中 PAC 清理覆盖固定代理标记。

### PAC 模式

当 `enable_system_proxy=true` 且 `proxy_auto_config=true`：

- 固定系统代理关闭。
- PAC 自动代理开启。
- PAC URL 是 `http://{proxy_host}:{singleton_port}/commands/pac`。
- 生产环境 `singleton_port` 为 `33331`，开发环境为 `11233`。
- 内嵌 server 在 `src-tauri/src/utils/server.rs` 提供 `/commands/pac`，会把 `%mixed-port%` 替换成当前 mixed port。

默认 PAC：

```javascript
function FindProxyForURL(url, host) {
  return "PROXY 127.0.0.1:%mixed-port%; SOCKS5 127.0.0.1:%mixed-port%; DIRECT;";
}
```

前端编辑 PAC 时还支持 `%proxy_host%` 和 `%mixed-port%` 模板替换。需要注意：PAC 只是系统自动代理脚本，仍然只覆盖读取系统 PAC 设置的应用。

### 代理守卫

`enable_proxy_guard=true` 时，`Sysopt` 会把守卫类型设置为当前 `Sysproxy` 或 `Autoproxy`，`GuardMonitor` 周期性读取 OS 实际配置；如果发现被其他软件改掉，就写回 Clash Verge 期望值。守卫只在 `enable_system_proxy=true` 时启动，关闭系统代理时会停止。

### 平台实现

`sysproxy-rs` 依赖版本在 `Cargo.lock` 中固定为 git commit `f0775f6f...`，README 说明支持 Windows、macOS、Linux。

- Windows：使用 WinINET `InternetSetOptionW` 设置 LAN 代理，同时枚举 RAS 拨号/VPN 连接并逐个应用；固定代理写 `ProxyServer/ProxyOverride`，PAC 写 `AutoConfigURL`。
- macOS：使用 `networksetup` 设置当前活动网络服务的 HTTP、HTTPS、SOCKS 代理、绕过域名和自动代理 URL；读取则通过 SystemConfiguration。
- Linux：GNOME 等桌面使用 `gsettings`/`dconf` 的 `org.gnome.system.proxy`；KDE 使用 `kreadconfig`/`kwriteconfig` 读写 `kioslaverc`，同时同步 gsettings。

### 默认绕过列表

本项目在 `src-tauri/src/core/sysopt.rs` 内置平台默认绕过：

- Windows：`localhost;127.*;192.168.*;10.*;172.16.*...172.31.*;<local>`
- Linux：`localhost,127.0.0.1,192.168.0.0/16,10.0.0.0/8,172.16.0.0/12,::1`
- macOS：`127.0.0.1,192.168.0.0/16,10.0.0.0/8,172.16.0.0/12,localhost,*.local,*.crashlytics.com,<local>`

前端校验规则也按平台区分：Windows 使用分号，Unix-like 使用逗号。

### 状态显示

UI 不是只看 `enable_system_proxy`，还会比对 OS 实际状态：

- 固定代理：`sysproxy.enable` 必须为真，且 `server` 等于 `{proxy_host}:{mixed-port}`。
- PAC：`autoproxy.enable` 必须为真，且 URL 等于当前 `/commands/pac`。

因此“配置开关打开”不等于“系统代理实际指向 Clash Verge”。这是托盘/首页指示器使用真实状态的原因。

### 退出清理

退出时 `src-tauri/src/feat/window.rs` 会在 `enable_system_proxy=true` 时调用 `reset_sysproxy()`，清除固定代理和 PAC，避免应用退出后系统代理残留。

## TUN

### 作用

TUN 是 mihomo 的虚拟网卡入站，流量在 IP 层进入 mihomo。mihomo 官方 TUN 文档列出顶层 `tun` 配置，普通用户应使用顶层 `tun`，高级用户才使用 `listeners` 下的 tun listener。

### 本项目状态字段

`enable_tun_mode` 存在 `verge.yaml` 中，表示用户是否打开 TUN。实际 TUN 参数存在 Clash 配置的 `tun` 节，默认由 `src-tauri/src/config/clash.rs` 和前端 TUN 设置共同维护。

默认基础配置：

```yaml
tun:
  enable: false
  stack: gvisor
  auto-route: true
  strict-route: false
  auto-detect-interface: true
  dns-hijack:
    - any:53
```

前端 TUN 设置默认值：

- `stack: gvisor`
- `device: macOS 为 utun1024，其他平台为 Mihomo`
- `auto-route: true`
- `auto-redirect: false`，仅 Linux 暴露
- `auto-detect-interface: true`
- `dns-hijack: [any:53]`
- `strict-route: false`
- `mtu: 1500`
- `route-exclude-address: []`

### 开关链路

前端开关：

- `src/components/shared/proxy-control-switches.tsx`
- `src/hooks/use-system-state.ts`
- `src/components/setting/mods/tun-viewer.tsx`

后端链路：

- 前端调用 `patchVerge({ enable_tun_mode: value })`。
- `src-tauri/src/feat/config.rs` 将其标记为 `CLASH_CONFIG | GROUP_SYS_TRAY`。
- `CoreManager::update_config_checked()` 使用重新生成的最终配置热更新 mihomo。
- 配置生成阶段 `src-tauri/src/enhance/tun.rs::use_tun()` 把 `tun.enable` 写为开关值。

### 权限与服务模式

TUN 需要能够创建/配置虚拟网卡和路由。项目判断条件是：

- 当前进程管理员权限，或
- Clash Verge Service 可用。

如果二者都不可用，初始化配置时会自动把 `enable_tun_mode` 改为 `false`；运行中 `useSystemState` 也会在服务/管理员不可用时自动关闭 TUN 并提示。

Windows 还有额外等待逻辑：如果 TUN 需要服务，启动 core 前会短暂等待服务 IPC 就绪，避免开启 TUN 时服务尚未可用。

### TUN 参数含义

按 mihomo 官方文档：

- `stack` 可为 `system`、`gvisor`、`mixed`；官方建议无问题时使用 `mixed`，默认 `gvisor`。本项目模板默认 `gvisor`，UI 可选 `mixed/system/gvisor`。
- `auto-route` 自动设置全局路由，把全局流量导入 TUN。
- `auto-redirect` 仅 Linux，自动配置 iptables/nftables 重定向 TCP，需要 `auto-route`。
- `auto-detect-interface` 自动选择出口接口，多出口网卡建议手动指定。
- `dns-hijack` 将匹配 DNS 连接导入 mihomo 内部 DNS 模块；未写协议时默认为 UDP。
- `strict-route` 在 `auto-route` 下执行更严格路由。Linux 上可减少泄漏；Windows 上会加防火墙规则阻止多宿主 DNS 行为造成 DNS 泄漏，但可能影响 VirtualBox 等软件。
- `route-exclude-address` 在 `auto-route` 时排除自定义网段，本项目提供可视化编辑和 CIDR 校验。
- `mtu` 影响极限速率，一般保持默认。

### TUN 与 DNS 的自动耦合

`use_tun(config, true)` 会读取最终配置中的 `dns`：

- 如果 `dns.enhanced-mode` 是 `fake-ip`，或没有设置 `enhanced-mode`，则补齐：
  - `dns.enable: true`
  - `dns.ipv6` 同步顶层 `ipv6`
  - 缺省时设置 `enhanced-mode: fake-ip`
  - 缺省时设置 `fake-ip-range: 198.18.0.1/16`
- 如果用户已经明确设置 `enhanced-mode: redir-host`，则不会强行改为 `fake-ip`。
- 关闭 TUN 时不会修改配置文件里的 DNS，只在 macOS 尝试恢复系统 DNS。

macOS 特殊处理：开启 TUN 且使用/补齐 fake-ip DNS 时，项目会异步先恢复公共 DNS，再把系统 DNS 设为 `114.114.114.114`；关闭 TUN 或退出时会恢复原 DNS。脚本位于 `scripts/set_dns.sh`、`scripts/unset_dns.sh`，打包时复制到资源目录。

### 退出清理

退出时如果 `enable_tun_mode=true`，项目会调用 mihomo API 临时 patch：

```json
{ "tun": { "enable": false } }
```

然后停止 core。这样做是为了让 mihomo 有机会清理虚拟网卡、路由和防火墙规则。

## DNS

### 作用

mihomo DNS 模块负责域名解析、Fake-IP/Redir-Host 增强解析、fallback 防污染、按域名策略选择 nameserver，以及为 TUN 的 DNS 劫持提供内部解析入口。

官方 DNS 文档说明：

- `dns.enable=false` 时使用系统 DNS。
- `listen` 支持 UDP/TCP 监听。
- `enhanced-mode` 可选 `fake-ip` 或 `redir-host`，默认 `redir-host`。
- `fake-ip-range` 是 fake-ip 地址池，TUN 默认 IPv4 地址也会参考该值。
- `respect-rules` 会让 DNS 连接遵守路由规则，但需要配置 `proxy-server-nameserver`，且不建议和 `prefer-h3` 一起使用。
- `proxy-server-nameserver` 专用于解析代理节点域名；留空时跟随 `nameserver-policy`、`nameserver`、`fallback`。
- `direct-nameserver` 专用于直连出口解析；留空时同样跟随通用配置。
- 配置 `fallback` 后，`fallback-filter` 默认启用，`geoip-code` 默认 CN。
- DNS server 类型支持 UDP、TCP、DoT、DoH、DoQ、`system`、`dhcp`、`rcode`。

### 本项目 DNS 覆写文件

项目启动时会创建 App Home 下的 `dns_config.yaml`，常量名为 `DNS_CONFIG`。默认内容包含：

```yaml
dns:
  enable: true
  listen: :53
  enhanced-mode: fake-ip
  fake-ip-range: 198.18.0.1/16
  fake-ip-filter-mode: blacklist
  prefer-h3: false
  respect-rules: false
  use-hosts: false
  use-system-hosts: false
  ipv6: true
  default-nameserver:
    - system
    - 223.6.6.6
    - 8.8.8.8
    - 2400:3200::1
    - 2001:4860:4860::8888
  nameserver:
    - 8.8.8.8
    - https://doh.pub/dns-query
    - https://dns.alidns.com/dns-query
  fallback: []
  proxy-server-nameserver:
    - https://doh.pub/dns-query
    - https://dns.alidns.com/dns-query
    - tls://223.5.5.5
  direct-nameserver: []
  direct-nameserver-follow-policy: false
hosts: {}
```

前端 `DnsViewer` 同时支持可视化表单和 YAML 编辑；保存时写入 `dns_config.yaml`，随后调用 core 校验。若当前运行配置中 DNS 已启用，会立即调用 `apply_dns_config(true)` 应用。

### DNS 开关链路

前端：

- `src/components/setting/setting-clash.tsx` 持有独立 `dnsSettingsEnabled` 状态。
- 切换时先写 `verge.enable_dns_settings`，再调用 `apply_dns_config(apply)`。
- 异常时回滚 `enable_dns_settings`。

后端：

- `save_dns_config` 保存独立 YAML。
- `validate_dns_config` 使用 core validator 校验文件。
- `apply_dns_config(true)` 读取 `dns_config.yaml` 并 patch 到 runtime。
- `apply_dns_config(false)` 触发重新生成配置，不再加载 DNS 覆写文件。
- 配置生成阶段 `apply_dns_settings()` 在 `enable_dns_settings=true` 时把 `hosts` 和 `dns` 注入最终配置。

### 覆写规则与当前风险点

`dns_config.yaml` 支持两种结构：

1. 带 `dns` 和可选 `hosts` 顶层键：

```yaml
dns:
  enable: true
  enhanced-mode: fake-ip
hosts:
  example.local: 127.0.0.1
```

2. 直接把文件整体作为 DNS 配置：

```yaml
enable: true
enhanced-mode: fake-ip
nameserver:
  - 8.8.8.8
```

当前 `apply_dns_config(true)` 的即时 patch 逻辑会把整个文件作为 `dns` 值传入 runtime；而生成最终配置的 `apply_dns_settings()` 会识别顶层 `dns`/`hosts`。因此长期一致性最好使用第 1 种结构，也就是默认 UI 保存结构。

这里有一个实现风险：默认 UI 保存的是第 1 种顶层 `dns`/`hosts` 结构，但 `apply_dns_config(true)` 目前没有先拆出顶层 `dns`，而是直接构造 `{ dns: <整个文件> }` patch 到 runtime。换言之，保存后立即应用的运行时 DNS 可能短暂变成 `dns.dns`/`dns.hosts` 形态；重新生成最终配置时才会按 `apply_dns_settings()` 回到正确结构。若要修复，应让 `apply_dns_config(true)` 复用 `apply_dns_settings()` 的拆分逻辑，或在读取文件后优先取顶层 `dns`，并把顶层 `hosts` patch 到 runtime 顶层。

### fake-ip 与 redir-host

- `fake-ip`：DNS 返回 `fake-ip-range` 中的虚拟 IP，mihomo 通过映射表恢复域名上下文，适合 TUN 和域名规则分流。需要维护 fake-ip 过滤列表，避免局域网、NTP、连通性探测等域名被映射为 fake IP。
- `redir-host`：DNS 返回真实 IP，域名上下文依赖嗅探或连接信息，兼容性通常更直观，但在透明代理/TUN 场景中不如 fake-ip 稳定掌控域名分流。

本项目默认 DNS 覆写和 TUN 自动补齐都偏向 `fake-ip`，但如果用户已经明确设置 `redir-host`，TUN 自动逻辑不会覆盖。

### hosts

mihomo `hosts` 支持通配域名、字符串/数组值，以及域名重定向。完整域名优先级高于通配域名。本项目 DNS 面板把 `hosts` 与 `dns` 放在同一个 `dns_config.yaml` 中，生成最终配置时会把 `hosts` 提升到 mihomo 顶层。

## 三者交互

### System Proxy 与 TUN

二者可以同时打开，但通常没必要：

- System Proxy 负责代理感知型应用。
- TUN 负责不读取系统代理的应用，并接管 IP 层流量。
- 同时打开时，应用可能先连本机 mixed-port，再由 mihomo 发出代理连接；这些连接是否再次被 TUN 捕获取决于 mihomo 的路由、回环、进程/接口排除和平台栈行为。一般用户启用 TUN 后不需要再开 System Proxy。

UI 文案也提示：TUN 模式接管整个系统流量，启用后不需要打开系统代理。

### TUN 与 DNS

TUN 的 `dns-hijack` 需要 mihomo DNS 能工作，否则 UDP/TCP 53 被接入 mihomo 后可能没有有效解析结果。项目因此在 TUN 开启时自动启用/补齐 DNS。

`dns-hijack: any:53` 未写协议时默认 UDP；mihomo 官方示例也展示了 `tcp://any:53`，如果要覆盖 TCP DNS，可在 TUN 设置里显式加入。

### DNS 覆写与 TUN 自动 DNS

最终配置顺序是先 TUN 自动补齐 DNS，再 DNS 覆写。因此：

- 只开 TUN：项目会保证基础 fake-ip DNS 存在。
- 同时开 TUN 和 DNS 覆写：以 `dns_config.yaml` 为准。
- 关闭 DNS 覆写：重新生成配置后回到订阅/合并/TUN 自动逻辑。

## 常见故障定位

### System Proxy 打开但无效

优先检查：

- OS 实际代理是否启用并指向 `{proxy_host}:{mixed-port}`。
- 当前应用是否尊重系统代理或 PAC。
- `mixed-port` 是否监听、端口是否被占用。
- PAC 模式下 `/commands/pac` 是否可访问。
- Windows 拨号/VPN 连接名称含非 ASCII 时，`sysproxy-rs` 注释提示可能无法正确设置，建议使用英文连接名。
- Linux 桌面环境是否支持当前实现：GNOME 类环境依赖 `gsettings/dconf`，KDE 依赖 `kreadconfig*/kwriteconfig*`。

### TUN 开启失败

优先检查：

- 是否管理员运行，或 Clash Verge Service 是否可用。
- Windows 下服务 IPC 是否就绪。
- 防火墙是否阻止 mihomo/TUN 出站。mihomo 官方文档提到 system/mixed 栈在防火墙开启时可能需要放行内核/应用或 TUN 网卡。
- macOS 设备名是否以 `utun` 开头。
- Linux `auto-redirect` 是否只在 Linux 使用，并且 `auto-route` 已启用。
- `route-exclude-address` 是否为合法 CIDR。

### TUN 下 DNS 异常或泄漏

优先检查：

- `dns.enable` 是否为 true。
- `dns-hijack` 是否覆盖了实际 DNS 协议，例如是否需要加入 `tcp://any:53`。
- `enhanced-mode` 是否符合预期；TUN 场景通常优先 `fake-ip`。
- `fake-ip-filter` 是否包含局域网、NTP、系统连通性检测、设备发现等必须真实解析的域名。
- Windows 是否需要 `strict-route` 来减少多宿主 DNS 泄漏，同时注意它可能影响虚拟化/局域网软件。
- macOS TUN 开启时项目会改系统 DNS，退出/关闭时应恢复；异常退出后可检查系统 DNS 是否残留。

### DNS 覆写保存成功但运行不一致

优先检查：

- `verge.enable_dns_settings` 是否为 true。
- `dns_config.yaml` 是否使用默认的顶层 `dns`/`hosts` 结构。
- `validate_dns_config` 是否通过。
- 运行时配置查看器中的最终 `dns` 是否已被替换。
- 如果刚保存，当前运行配置中 `clash.dns.enable` 为 false 时，UI 保存不会立即 apply，需要打开 DNS 设置开关或重新生成配置。

## 建议

- 普通桌面代理优先使用 `System Proxy`，因为影响小、容易回滚。
- 需要覆盖游戏、命令行、Electron/Java、部分不读系统代理的软件时使用 `TUN`。
- 开启 `TUN` 后一般关闭 `System Proxy`，除非明确需要让代理感知型应用仍按系统代理进入 mixed-port。
- `DNS` 覆写建议保持默认顶层结构，并把节点域名解析放到 `proxy-server-nameserver`，避免 DNS 查询本身依赖尚未解析的代理节点。
- TUN + DNS 场景优先 `fake-ip`，对局域网、NTP、系统探测、设备发现域名加入 `fake-ip-filter`。
- Linux 仅在需要透明重定向 TCP 时开启 `auto-redirect`，且保持 `auto-route=true`。

## 代码入口索引

- System Proxy UI：`src/hooks/use-system-proxy-state.ts`、`src/components/setting/mods/sysproxy-viewer.tsx`
- System Proxy 后端：`src-tauri/src/core/sysopt.rs`、`src-tauri/src/cmd/network.rs`
- PAC server：`src-tauri/src/utils/server.rs`
- TUN UI：`src/components/setting/mods/tun-viewer.tsx`
- TUN 配置生成：`src-tauri/src/enhance/tun.rs`
- TUN 权限/服务状态：`src/hooks/use-system-state.ts`、`src-tauri/src/config/config.rs`、`src-tauri/src/core/service.rs`
- DNS UI：`src/components/setting/mods/dns-viewer.tsx`、`src/components/setting/setting-clash.tsx`
- DNS 后端命令：`src-tauri/src/cmd/clash.rs`
- DNS 默认初始化：`src-tauri/src/utils/init.rs`
- 最终配置生成：`src-tauri/src/enhance/mod.rs`
- 默认端口与文件名：`src-tauri/src/constants.rs`

## 外部资料

- mihomo DNS configuration: https://wiki.metacubex.one/en/config/dns/
- mihomo DNS type: https://wiki.metacubex.one/en/config/dns/type/
- mihomo hosts: https://wiki.metacubex.one/en/config/dns/hosts/
- mihomo Proxy Ports: https://wiki.metacubex.one/en/config/inbound/port/
- mihomo TUN: https://wiki.metacubex.one/en/config/inbound/tun/
- mihomo TUN listener: https://wiki.metacubex.one/en/config/inbound/listeners/tun/
- sysproxy-rs: https://github.com/clash-verge-rev/sysproxy-rs
