# 第三方依赖与出处（总览）

## 外部工具（子进程调用，不打包、不分发，需用户自行安装）

- **OfficeCLI（`officecli`）** —— docx 读取与写回的后端。详细出处、许可（Apache-2.0）与可替换性说明见 [`polaris-docx/THIRD_PARTY.md`](polaris-docx/THIRD_PARTY.md)。
- **Claude Code CLI（`claude`）** —— AI 改写提案的模型调用（`claude -p`，子进程），`polaris-gui` 的真 AI 源。来源：Anthropic（https://claude.com/claude-code），使用本机已认证安装。

## Rust crate 依赖（见各 Cargo.toml，均为常见宽松许可：MIT / Apache-2.0）

- `serde_json` —— 解析 officecli 的 JSON 输出
- `base64` —— 解码 `view html` 内嵌的 data URI 图片
- `eframe` / `egui` / `egui_extras` / `image` —— 原生 GUI 与图片渲染
- `rfd` —— 系统文件选择对话框
- `chrono` —— 版本历史的本地时间显示
