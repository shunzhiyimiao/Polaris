# Polaris

以 **Typed Model** 为唯一真理之源、以 **typed Patch** 为唯一修改协议的 AI-native 文档 runtime。

当前已闭合的端到端回路：

> 打开真实 docx（标题/段落/表格占位/真图）→ 在 typed model 上编辑（patch / 逆 op / 原子提交 / 撤销）→ 按 `@paraId` 身份写回原文件 → 每次落盘自动留底、可回任意旧版 → AI 改写提案必须过 review（core 层 commit 闸口机械强制，Human patch 直达）。

## 结构

| crate | 职责 |
|---|---|
| `editor-core`（根） | 中性内核：typed model / Patch / 逆 op / PatchSet 拓扑序 / 原子 commit / undo / renderer + SourceMap。不含任何 office 专有逻辑 |
| `polaris-docx` | docx 适配层：OfficeCLI 子进程后端（不解析 OOXML）、段落身份（`@paraId`）携带、定向写回 ops、文件级版本历史 |
| `polaris-gui` | eframe/egui 原生 GUI：查看 / 编辑 / 保存 / 撤销 / 版本历史 / AI 改写 review |

## 跑起来

前置：

- [OfficeCLI](https://github.com/iOfficeAI/OfficeCLI)：`officecli` 在 PATH 上——docx 读写后端
- （可选）[Claude Code CLI](https://claude.com/claude-code)：`claude` 已登录——AI 改写功能

```bash
cargo test --workspace        # 91 个测试，不碰任何子进程
cargo build -p polaris-gui
POLARIS_DOC=/path/to/some.docx ./target/debug/polaris-gui   # 或 cargo run -p polaris-gui
```

## 开发章程

设计原则、增量纪律、实测事实与当前步骤阶梯见 [CLAUDE.md](CLAUDE.md)；第三方出处见 [THIRD_PARTY.md](THIRD_PARTY.md)。
