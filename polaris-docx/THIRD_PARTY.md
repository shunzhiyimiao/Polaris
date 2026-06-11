# 第三方工具出处与许可

## OfficeCLI（外部子进程，可选后端）

`polaris-docx` 的 `OfficeCliBackend` 通过子进程调用外部命令 `officecli` 读取 docx。

- 项目：OfficeCLI — https://github.com/iOfficeAI/OfficeCLI
- 许可：Apache License 2.0
- 用法：作为**外部工具**调用（`officecli view <file> text|outline --json`）。**本仓库不打包、不分发 OfficeCLI 的源码或二进制**，需用户自行安装。
- 可替换：`DocxBackend` 是抽象接口，OfficeCLI 只是其中一个实现（还可换 Pandoc、textutil 等），不与之深度耦合。

> 因为我们不分发其代码/二进制，Apache-2.0 的再分发义务（随附 LICENSE/NOTICE）严格说并不触发；此处仍按项目章程注明出处与许可，以示尊重与可追溯。
