<p align="center">
  <img src="docs/icon.png" alt="EchoBird" width="140" />
</p>

<h1 align="center">EchoBird</h1>

<p align="center"><strong>ChatGPT、Codex CLI、Claude Code 多账号切换</strong> · 多模型智能路由与故障自动切换 · AI 工具一键安装</p>

<p align="center">
  <a href="https://github.com/congci6/EchoBird/releases">
    <img src="https://img.shields.io/github/v/release/congci6/EchoBird?style=flat-square&color=D97757" alt="Release" />
  </a>
  <img src="https://img.shields.io/badge/%E5%B9%B3%E5%8F%B0-Windows%20%7C%20macOS%20%7C%20Linux-blue?style=flat-square" alt="平台" />
  <img src="https://img.shields.io/badge/%E6%8A%80%E6%9C%AF-Tauri%20%2B%20Rust-orange?style=flat-square" alt="Tauri + Rust" />
  <img src="https://img.shields.io/github/license/congci6/EchoBird?style=flat-square" alt="MIT 许可" />
</p>

<p align="center">
  <a href="https://github.com/congci6/EchoBird">仓库</a> ·
  <a href="https://github.com/congci6/EchoBird/releases/latest">下载</a>
</p>

---

## 这是什么

很多朋友让我帮他们安装 **Claude Code**、**OpenClaw**、**Hermes Agent**……不但每个人的系统都不一样,甚至有些人还抠门到不愿花钱买大模型,安装和解释起来都特别费劲。于是我开发了这个叫「EchoBird」的 Agent —— 灵感来自《赛博朋克 2077》里那位聪慧过人、总能帮主角搞定一切技术难题的天才女助理 **Songbird**…

<p align="center">
  <img src="docs/screenshots/deepseek-harness-demo.gif" alt="DeepSeek Harness 一键安装+切换模型 （演示）" width="820" />
  <br/>
  <sub><strong>DeepSeek Harness 一键安装+切换模型 （演示）</strong></sub>
</p>

## 亮点

EchoBird 提供 **4 大场景**,共享一个 **模型数据中枢** —— **一处配置,四处生效**。

### 4 大场景

- **安装与修复** —— 让 AI 帮你安装与修复主流 AI 工具(Claude Code、OpenClaw、Hermes Agent 等);本地与远程都支持
- **一键本地大模型** —— 内置 vLLM / SGLang / llama.cpp 三引擎,选好量化版本按下 START 就能跑
- **我的 AI 项目** —— 你自己 Vibe Coding 的应用或游戏,在 EchoBird 里统一接入与管理
- **应用管理** —— 所有跟 AI / Agent 有关的应用或游戏一键启动与管理

### 共享地基

- **模型中心** —— 统一的模型数据中枢(OpenAI / Anthropic / 本地 LLM / API Router);一处配置好,4 大场景立即生效;附带一键测速,使用前看清真实延迟

**跨平台** —— Windows、macOS、Linux(x64 + arm64)

## 多账号切换 —— ChatGPT、Codex CLI、Claude Code

在 EchoBird 的**应用管理**中保存多个账号、查看使用额度，并选择启动工具时使用的账号。

- **ChatGPT 桌面版与 Codex CLI** —— 通过浏览器登录添加 OpenAI 账号，选中已保存账号后启动。这两个工具共享本地 Codex 账号配置，切换会影响共用的登录状态。
- **Claude Code** —— 通过浏览器授权添加账号，选择账号后启动 Claude Code；可查看套餐、5 小时与 7 天剩余额度及重置倒计时（以服务商返回的数据为准）。
- **额度一目了然** —— 在同一面板查看剩余额度和重置时间、刷新用量、删除已保存账号。

**开始使用：**打开「应用管理」→ 选择 ChatGPT、Codex CLI 或 Claude Code → 添加并授权账号 → 选中账号 → 启动。账号选择在启动时生效；需要使用第三方 API 时，改选对应的模型即可。

## 多模型智能路由 —— 按优先级调用，故障自动切换

EchoBird 的**智能路由（Smart Router）**用一个本地 API 汇总多个模型服务。把模型中心已配置的免费、付费或私有模型加入路由，拖动卡片设置优先级。模型限流、额度耗尽或临时不可用时，路由自动尝试下一个；冷却结束后，重新按优先级尝试。

- **最多接入 20 个模型** —— 复用已有模型配置，无需重复填写 API Key。
- **兼容工具共用一个入口** —— 支持 OpenAI Chat Completions 和 Anthropic Messages，可供 Claude Code 等兼容客户端使用。
- **优先级由你决定** —— 按你设定的顺序调用，不按任务内容或价格自动选模型。

**开始使用：**打开「智能路由」→ 从「我的模型」或免费模型目录添加模型 → 拖动排序 → 在「应用管理」中为兼容工具选择 **Auto Router**。工具通过本地 API 调用期间，请保持 EchoBird 运行。

智能路由切换的是已配置的模型 API，不会自动轮换已保存的登录账号。ChatGPT / Codex CLI 接入第三方模型需要 **Responses API**，当前智能路由尚未提供此接口；这两个工具请使用账号切换，或直接接入支持 Responses 的服务商。

在 Releases 页面查看全部版本。

## 多协议支持与协议测试

### 客户端协议与供应商协议是两回事

一个模型供应商**原生只说一种协议**，但客户端未必用那一种。EchoBird 的模型中心让你为每个模型指定**客户端协议**，与供应商的**原生协议**分开配置；两者不同时，请求经由本地桥接（默认 `127.0.0.1:53684`）转换后再发出。

支持四种协议，互相之间可双向转换（含流式与非流式）：

| 协议 | 接口 |
| --- | --- |
| OpenAI Chat Completions | `/v1/chat/completions` |
| OpenAI Responses | `/v1/responses` |
| Anthropic Messages | `/v1/messages` |
| Gemini `generateContent` | `/v1beta/models/{model}:generateContent` |

例如客户端要 Responses、供应商只提供 Chat Completions 时，桥接会把 Responses 请求转成 Chat 调用再把结果转回 Responses，客户端侧无需改动。

### 协议测试

在「模型中心」编辑模型配置时，可对**当前这一个模型**手动发起四协议测试，逐个协议实发请求并给出结论：

```
2/4  ▸ 显示详情
```

结果默认折叠，一行摘要显示通过数；展开后逐协议列出状态、延迟与错误信息。状态分为：

- **可用（Available）** —— 请求成功
- **不支持（Unsupported）** —— 供应商明确表示不实现该协议
- **鉴权失败（Auth）** —— API Key 无效或无权限
- **无响应（NoResponse）** —— 超时或网络不可达

报告与测试时的 Base URL、模型绑定。改动了其中任一项，旧结果立即失效——**不会拿上一次的结论误导这一次**。协议测试只在你点击时发出，绝不在保存模型或编辑字段时自动调用供应商。

### Responses 降级

部分供应商不支持 Responses 接口（常见表现为 5xx 且提示未实现）。提供两级开关，**默认均关闭**：

- **手动降级** —— 开启后该模型一律用 Chat Completions 走 Responses 入口
- **自动降级** —— 开启后，仅当供应商**明确表示不支持** Responses 时才切换，且只重试一次

自动降级的判定是严格的：超时、限流、鉴权失败、参数错误或普通服务故障**都不会**触发降级——这些情况重试没有意义，掩盖问题反而更糟。自动学习到的结果只保存在当前进程内，按 Base URL 隔离，重启后重新学习。

### 已知限制

- 部分模型配置缺少独立的 Gemini 供应商 URL / 原生协议字段，Gemini 原生接入需手工填写地址
- `claudecode`、`claudedesktop`、`aider` 的部分 Anthropic 分支尚未全部走中央路由
- `anthropic_proxy` 目前仍为字节透传，不做结构转换

## 支持的工具 —— 一键安装、一键切换模型

EchoBird 内置了各工具的安装脚本,**并直接写入每个工具的原生配置文件**,
所以你既能在一个地方装好,更关键的是能**一键切换模型**。在模型中心配好一处
provider,任意支持的工具都能指向它;不用手改 TOML / JSON,不用每个 CLI 重新
登录。这正是大部分「切换模型」开源仓要你自己折腾的部分。

### 一键安装 + 一键切换模型

以下工具**同时**支持安装与切换模型 —— 这是 EchoBird 的核心:

**编程 CLI** —— Claude Code · Codex CLI(OpenAI) · Grok Build(xAI) ·
Kimi CLI(月之暗面) · Qwen Code · Aider · OpenCode · MiMo Code(小米) · Kilo Code ·
ZCode(Z.AI) · OpenClaw · Pi · Vibe-Trading

**桌面应用** —— Claude Desktop(第三方 profile) · ChatGPT 桌面版 ·
Kimi 桌面端 · OpenCode Desktop · OpenScience · WorkBuddy(腾讯 CodeBuddy 办公版)

> 在 GitHub 上搜「给 Grok Build 切换模型」「给 Kimi CLI / 桌面端切换模型」?
> 这两个在这里都是一等公民 —— 在模型中心选好模型,按下切换,EchoBird
> 就帮你重写 `~/.grok/config.toml` 或 `~/.kimi-code/config.toml`。

### 一键安装与启动

这些工具由 EchoBird 检测、安装、管理,但模型切换由应用自身负责
(厂商锁定或无模型配置):

Hermes Desktop · Claude Science · Cursor · VS Code ·
Gemini Desktop · Coffee CLI

## 界面截图

### 模型中心 —— 模型数据中枢,一处配置,四处生效

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/screenshots/model-cn-dark.png">
  <img alt="模型中心" src="docs/screenshots/model-cn-light.png" width="100%">
</picture>

### 应用管理 —— 所有 AI / Agent 应用一键启动与管理

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/screenshots/app-cn-dark.png">
  <img alt="应用管理" src="docs/screenshots/app-cn-light.png" width="100%">
</picture>

### 本地大模型 —— 在自己机器上跑

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/screenshots/localllm-cn-dark.png">
  <img alt="本地大模型" src="docs/screenshots/localllm-cn-light.png" width="100%">
</picture>

### 安装与修复 —— 用对话搞定部署和排障

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/screenshots/agent-cn-dark.png">
  <img alt="安装与修复" src="docs/screenshots/agent-cn-light.png" width="100%">
</picture>

### 我的 AI 生涯 —— 仪表盘

<p align="center">
  <img src="https://github.com/user-attachments/assets/162f0428-a44d-4e83-9e10-c6b580ef0120" alt="EchoBird —— 我的 AI 生涯仪表盘" width="820" />
</p>

## 安装

### 下载安装包

最新版本 → <https://github.com/congci6/EchoBird/releases/latest>

或使用 `gh` 命令行直接下载：

```sh
gh release download --repo congci6/EchoBird --pattern "*Windows_x64.msi"
```

| 平台                        | 安装包                                 |
| --------------------------- | -------------------------------------- |
| Windows x64                 | `EchoBird_<ver>_Windows_x64-setup.exe` |
| macOS(Apple Silicon)        | `EchoBird_<ver>_macOS_arm64.dmg`       |
| Linux x64 · Debian/Ubuntu   | `EchoBird_<ver>_Linux_x64.deb`         |
| Linux arm64 · Debian/Ubuntu | `EchoBird_<ver>_Linux_arm64.deb`       |
| Linux x64 · Fedora/RHEL     | `EchoBird_<ver>_Linux_x64.rpm`         |
| Linux arm64 · Fedora/RHEL   | `EchoBird_<ver>_Linux_arm64.rpm`       |

---

<p align="center">
  Made with 💚 by EchoBird Team<br>
  <sub>⭐ <a href="https://github.com/congci6/EchoBird">在 GitHub 上点个 Star</a></sub>
</p>
