# AnyChat (Primary Development - Tauri)

这是 AnyChat 的主分支，采用 Tauri 架构，旨在提供高性能、低占用的多 AI 聊天聚合体验。

> 迁移说明
>
> `2026-03-21` 起，原 `electron` 主线已经拆分为独立项目 **AmberKeeper**：`https://github.com/JS-banana/amberkeeper`。本仓库不再承载 Electron 主线的新功能开发。
>
> `2026-10-17` 起，应用户要求在本仓库聚合记忆：AnyChat 会把你在已开启「记忆」的服务中浏览过的对话，交给本机运行的 Hy-Memory 提炼成长期记忆，不提供聊天记录的浏览、搜索或导出；完整聊天记录的采集、存档与管理仍由 AmberKeeper 承接。页面内容由宿主通过原生 evaluate 在隔离 world 中只读取回，不恢复已移除的 relay、`anychat://capture` 或 IPC 通道。

## 分支说明

- **main**: 稳定分支，用于保存当前主线代码。
- **dev**: 开发分支，用于日常集成与迭代。

## 当前开发重点

1. **多 Webview 聚合体验**: 优化服务切换、窗口管理和快捷键。
2. **核心能力维护**: 持续整理 Tauri 主线能力与交互体验。
3. **性能优化**: 保持极低的内存占用和安装包体积。
4. **记忆聚合**: 通过 Hy-Memory 把浏览过的对话提炼为记忆。

## 关键文件

- `src/App.tsx`: 前端入口与路由逻辑
- `src/stores/app-store.ts`: 全局状态管理
- `src/components/SettingsPage.tsx`: 设置页与关于页入口
- `src/components/WebViewContainer.tsx`: 核心 WebView 容器
- `src-tauri/src/lib.rs`: Rust 后端核心
- `src-tauri/src/memory/`: 记忆引擎（页面探针、Hy-Memory 进程、指纹账本）
- `src/components/MemoryPanel.tsx`: 设置页「记忆」标签

## 决策记录

- `2026-10-17` 记忆聚合。决策：Hy-Memory 是唯一的记忆存储，由用户自行安装 Python 与 `hy-memory==2.0.3`，不随包打包；AnyChat 不保存聊天原文，只在本机记录已发送消息的指纹。原因：用户要求在 AnyChat 内聚合记忆，同时不重建 AmberKeeper 的采集与存档能力。页面读取目前只在 Linux（WebKitGTK ≥ 2.40）实现，macOS 与 Windows 返回不支持。

## 相关文档

- [技术路线图](.sisyphus/plans/)
- [Gemini 研究报告](docs/research/2026-01-20 研究报告gemini.md)
- [任务跟踪记录](docs/plans/2026-01-28-anychat-triage.md)
- [遗留捕获链路清理计划](docs/plans/2026-03-24-remove-legacy-capture-plumbing.md)

---

_详细信息请参阅文档索引中的具体文档_
