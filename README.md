# clashtui

本地 TUI 学习及实验工具。

用于学习 rust + tui 的开发，可以做一些配置、启动和状态查看，尝试理解交互界面的设计。

## 构建

需要 Rust 工具链。

```bash
cargo build --release
```

生成文件通常在：

```bash
target/release/clashtui
```

也可以直接运行：

```bash
cargo run -- config
```

## 使用

一般顺序：

```bash
clashtui service-install
clashtui config
clashtui start
clashtui status
clashtui stop
```

大致含义：

- `service-install` 准备后台服务, 需管理员权限
- `config` 打开配置界面
- `start` 启动后台部分
- `status` 查看当前状态
- `stop` 停止后台部分

需要重新应用时：

```bash
clashtui restart
```

如需更多输出：

```bash
clashtui --verbose status
```

语言可简单指定：

```bash
clashtui -l zh config
clashtui -l en config
```

## 开发

常见 Rust 项目命令：

```bash
cargo check
cargo test
cargo fmt
cargo clippy
```

常见 Rust 目录：

- `src/` 源码
- `target/` 构建产物
- `Cargo.toml` 项目配置
- `Cargo.lock` 锁定依赖
- `~/.cargo/` 本地工具链和缓存

## 配置目录

可以通过环境变量指定：

```bash
CLASHTUI_CONFIG_DIR=/path/to/dir clashtui config
```

未指定时，在常用平台通常使用：

- Linux: `~/.config/clashtui`
- macOS: `~/Library/Application Support/clashtui`

里面会保存本地配置、运行记录和少量运行时文件。
