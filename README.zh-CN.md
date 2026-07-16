<div align="center">

<img src="assets/app-icon.svg" alt="tty7" width="88" height="88" />

### tty7

**终端工作台：shell、会话、SSH、coding agent。**

<sub>纯 Rust · GPU 渲染基于 Zed 的 gpui · VT 内核来自 Alacritty</sub>

<br />

[![CI](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml/badge.svg)](https://github.com/l0ng-ai/tty7/actions/workflows/ci.yml)
[![Version](https://img.shields.io/github/v/tag/l0ng-ai/tty7?label=version&color=ff8a5c)](https://github.com/l0ng-ai/tty7/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Discord](https://img.shields.io/badge/Discord-%E5%8A%A0%E5%85%A5%E7%BE%A4%E7%BB%84-5865F2?logo=discord&logoColor=white)](https://discord.gg/s3dethqz2V)

<sub>[English](README.md) · 简体中文</sub>

</div>

## 为什么

- **快** —— 吞吐约为 Alacritty、Ghostty、Kitty 的 2 倍（[基准测试](#基准测试)）
- **会话常驻** —— 退应用、重启机器，shell 照样运行；无需 tmux
- **编辑器级输入** —— 补全、语法高亮、历史搜索内置；zsh、bash、fish、PowerShell 零配置
- **认识 agent** —— 识别 pane 里的 Claude Code 等：状态、通知、会话恢复

## 安装

三平台原生构建都在 [**Releases**](https://github.com/l0ng-ai/tty7/releases)：

| | | |
|---|---|---|
| **macOS** | `…-macos-arm64.dmg` · `…-x86_64.dmg` | 拖进「应用程序」 |
| **Windows** | `…-setup.exe` · 便携版 `….zip` | |
| **Linux** | `…-x86_64.AppImage` | `chmod +x` 直接跑，x11/wayland 依赖已打包 |

## 有什么

| | |
|---|---|
| **输入** | 历史影子建议 · 带说明的 Tab 补全 · 语法高亮 · 多行编辑 · 点击定位光标 · <kbd>⌃ R</kbd> 模糊历史搜索 |
| **窗口** | 标签页与分屏 · <kbd>⌘ P</kbd> 命令面板 · <kbd>⌘ F</kbd> 回滚搜索 · 8 套主题 · 输入法 |
| **Coding agent** | 按 pane 识别约 17 个 CLI agent：状态点、通知、分支 + diff、重启后续上会话 |
| **SSH** | 原生 russh 栈：profile 凭据进 keychain、SFTP 面板、端口转发、跳板机 |

每一行的细节见 [docs/features.zh-CN.md](docs/features.zh-CN.md)。快捷键：<kbd>⌘ ,</kbd>
打开设置，可查看、重绑全部键位，含 tmux 预设（[完整列表](docs/features.zh-CN.md#快捷键)）。

## 基准测试

同一台机器、同一天、统一 155×40 网格 —— Apple M1 Pro，macOS 26.3.1，
取五次运行的平均值（2026-07-04）：

| | **tty7** | Alacritty | Ghostty | Kitty |
|---|---:|---:|---:|---:|
| 纯文本 IO —— 11 MB `cat` <sub>（越低越好）</sub> | **95 ms** | 239 ms | 179 ms | 185 ms |
| [DOOM-fire](https://github.com/const-void/DOOM-fire-zig) 帧率 <sub>（越高越好）</sub> | **888 fps** | 485 fps | 552 fps | 617 fps |
| 冷启动内存 | 116 MB¹ | 105 MB | 128 MB | 130 MB |

<sub>¹ GUI 105 MB + 常驻守护进程 11 MB。</sub>

测试方法与一键复现脚本：[`scripts/bench/`](scripts/bench/README.md)。

---

<div align="center">
<sub>

基于 [gpui](https://github.com/zed-industries/zed) 与 [`alacritty_terminal`](https://github.com/zed-industries/alacritty) 构建 · [Apache-2.0](LICENSE) · [Discord](https://discord.gg/s3dethqz2V) · [更新日志](CHANGELOG.md)

</sub>
</div>
