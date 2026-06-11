# CLAUDE.md — Polaris Office / editor-core

> 把本文件放在项目根目录。它既是给 Claude Code 的工作约束，也是项目章程的浓缩。
> 完整设计见 `ai-native-office-runtime-总纲.md`（设计蓝图，已验证，但本身不可编译）。

---

## 你（Claude Code）必须遵守的工作纪律

这个项目过去最大的毛病是「一次性造一大坨从没编译过的代码」。**绝不重蹈覆辙。**

### 铁律（违反任何一条都算失败）

1. **增量，单步。** 一次只实现**一个**最小的能力。永远不要一次生成多个特性、多个文件、几百行未验证的代码。
2. **每步必须 `cargo test` 绿才能进下一步。** 实现一小块 → 写测试 → `cargo build` → `cargo test` → 绿了才继续。报错就停下修，不往前堆。
3. **每步结束后停下，报告：改了什么 / `cargo test` 结果 / 下一步建议。等我确认再继续。** 不要自作主张连做多步。
4. **没有 `todo!()` / `unimplemented!()` / 占位骗编译。** 每一步交付的都是真能跑、真有测试覆盖的代码。如果某个特性还没到该做的步骤，就**不引入它的类型**，而不是留个 `todo!()`。
5. **优先让它编译过，而不是让它「设计完整」。** 宁可功能少而真能跑，不要功能全而编不过。
6. **修 bug 先拿真实输出定锚。** 凡是涉及外部工具（officecli）行为的改动，先用真实文件实测它的输出形状，再写代码；测试 fixture 用真实输出，不用想象的。
7. **改完真机验证。** GUI 可用 AccessKit 按控件名驱动（按钮可点、文本可读；键盘注入进不去 egui）。验证一律在沙箱副本上做（`HOME=/tmp/polaris-verify` 直跑二进制，**不要** `HOME=… cargo run`——会把 rustup 带偏），绝不碰用户原件。

### 为什么这么做

设计已经在总纲里想透了。现在缺的**不是更多设计，是让设计持续变成真能运行的代码**。每一步绿灯，都是把「纸上架构」变成「存在的东西」的一次兑现。

---

## 项目是什么（一句话）

**Polaris Office**：以 Typed Model 为唯一真理之源、以 typed Patch 为唯一修改协议的 AI-native 文档 runtime。
当前只做内核 crate `editor-core`（中性命名，将来另一个产品 Drafting 也复用，所以**不要往里塞任何 office 专有逻辑**）。

---

## 不可违反的设计原则（实现时必须遵守）

1. **Typed Model 是真理之源**，notation（markdown/latex/docx）只是输入输出记法，不是模型。
2. **一切修改都是对 model 的 typed Patch**，`apply_op` 必须返回**逆 op**（内部状态永远可逆）。
3. **Patch 分两类**：内部可逆 patch（有逆 op）vs 外部副作用 effect（不可逆，带补偿）——写盘 effect 的补偿 = 文件级版本历史（已落地）。
4. **derive() 是纯函数**：只读「已 apply authored」的模型，产 derived patch，**绝不预测、绝不镜像 apply_op 逻辑**。
5. **PatchSet 原子**：组内按 `depends_on` 拓扑序执行；环 → 整组 abort。
6. **偏移用语义类型**（`Pos`/`CharRange` 的 newtype，不是裸 usize），且对 CJK/emoji 安全（char↔byte 转换层，绝不裸 `s[a..b]`）。
7. **Human/AI 信任不对称**：Human patch 可直达，AI patch 过 review（Step 17 起以最小形式落地）。

衍生纪律（实践中确立，同样不可违反）：

- **「全部可见、部分结构化」**：映射不进的内容（表格/图片）存可见占位块，**消失即 bug**。
- **写回按身份定位，不按位置数**：docx 段落用 `@paraId`，加载时随 block 带出（`para_map`）；写不回的改动**跳过并大声报数**，绝不静默吞、绝不猜位置。
- **GUI 一切判断以 model 为准**（dirty 检查、pending_ops 都读 model），UI 缓冲只是影子。
- **历史只增不减、按内容去重**：留底前字节比对，已在历史的状态不重复留。

---

## 当前状态（Step 0–18 已完成 ✅，91 测试全绿）

**端到端回路已闭合**：打开真实 docx → 看见（标题/段落/表格占位/**真图**）→ 改（typed patch + 撤销；不透明内容 core 层只读）→ 按身份写回原文件 → 每次落盘自动留底、可回任意旧版。

- **editor-core**（41 测试）：typed op + 逆 op、`Pos`/`CharRange`/`CharIndex`、结构 op（insert/remove/create，快照含 children）、`SetKind`、`NodeKind::Opaque`（typed 不透明内容，**不产 SourceMap 片段 → core 层只读**）、`PatchSet`+`topo_order`、`derive`+`assemble`、`commit` 原子 + `undo`、`render_html`+`SourceMap`、`apply_fragment_edit`。
- **polaris-docx**（25 测试）：中性 `DocxBlock`（带 `para_id` 身份 + 可选 `image` 字节）+ 可替换 `DocxBackend` trait + `FakeBackend`；`OfficeCliBackend` 子进程（text/outline/annotated/html 四视图合并，不碰 OOXML）；标题按段落计数器匹配 outline；纯图段升级为 Opaque 占位且**按文档序配对真图字节**；`DocxOp`（set/remove/add）+ `batch` 写回；`history` 模块（快照/列表/回版，内容去重）。
- **polaris-gui**（15 测试）：eframe/egui + AccessKit；打开/编辑/保存/撤销/增删段落；版本历史面板（回任意旧版、脏状态二次确认）；写回查 `para_map`，跳过数大声上报；Opaque 块只读渲染（有身份的可删）、图片块真图渲染（`image_map`，无字节降级文字占位）。

### officecli 实测事实（修 bug 时定锚用，别凭想象）

- `text --json` 元素序列含体级 `tbl`（type=table）；段落 path 带 `@paraId`（无 paraId 的文件是位置式 `p[N]`）。
- `outline --json` 的 `line` = 第几个**段落**（1 起，表格不占号）。
- 纯图段在 `text` 里是空段落；图片只在 `annotated` 模式现身（`[Image: alt="…", 尺寸]`，alt 跨多行）。
- `html --json` 返回 `{"success":…, "data":"<html…>"}`，data 即 HTML 字符串；真图以 `data:image/png;base64,…` 内嵌（字节与 `word/media/` 原图一致），`<img>` 为文档序。
- `view` 会留常驻进程，之后 `batch` 会复用导致不落盘——**view 完必须 `close`**。
- `batch` 部分失败也 exit 0，要靠输出里 `Batch complete … 0 failed` 判断。

### claude CLI 实测事实（AI 源定锚用）

- `claude -p --model haiku`，提示词走 stdin；干净文本输出，延迟 **9–12s** → 必须后台线程 + `request_repaint_after` 轮询收货。
- 认证失败 **exit=1 且错误打在 stdout**（如 `Not logged in`）——失败信息要把 stdout 一起带回。
- 假 HOME 沙箱继承不了认证（Keychain 之外还依赖 HOME 上下文，symlink `.claude`/`.claude.json` 也不行）——验证用 **真 HOME + `POLARIS_DOC=<路径>`**（GUI 支持该 env 指定启动文档）。
- `cargo test` 不重建主二进制！改完代码必须 `cargo build -p polaris-gui` 再跑真机，否则验证的是旧版。

### egui 实测事实（GUI 改动时定锚用）

- `egui_extras` 的 `image` feature 只装 loader **不开解码格式**——必须直接依赖 `image` crate 并显式开 `png`/`jpeg`（feature 合并），否则图片永远停在加载态。
- `Image` 默认 fit 同时受可用宽**和高**约束：在 `horizontal` 行里会被按钮列行高压成缩略图——用 `fit_to_exact_size(vec2(宽, INFINITY))`。
- AccessKit 可按控件名点按钮/读文本（`text area`=多行、`text field`=单行、`static text`=Label），但**图片控件不上报**、键盘事件注入不进 egui；验证图片渲染用「相邻行距几何」或临时 stdout 探针。

### 已知薄点（backlog，按需捞）

- 紧跟表格后新增段落会落到表格**前**（锚点退化到前一个有身份的段）。
- 无 paraId 的文件（Pandoc/textutil 产物）：能看不能存（保存时大声报「无法写回」）。要支持得做位置路径写回。
- 图文混排段（文字+图同段）：文字可编辑，但图的存在看不出来。
- 旧重复版本不回收；版本文件名是毫秒数，Finder 里人读不出（GUI 面板里有本地时间）。

---

## 增量步骤阶梯（一步一停，每步必须绿）

> 按顺序做。**每完成一步，停下报告并等我确认。** 不要跳步，不要合并步骤。

- **Step 14 ✅｜`Opaque` 节点类型（core）**：`NodeKind` 加 `Opaque`（中性概念：任何 notation 都有解析不进的内容，不算 office 逻辑）。renderer 对 Opaque 产出不可编辑语义的标签；SetKind/快照/逆 op 全链路过测试。**不碰 polaris-docx/GUI。**
  *Stop gate：绿，core 内 Opaque 节点渲染与 roundtrip 测试通过。*

- **Step 15 ✅｜占位块 typed 化 + 只读 + 可删**：import 把 Unstructured 落成 `Opaque` 节点（文本=原始描述，不再用 `[未结构化:…]` 前缀 hack）；`Unstructured` 带 `para_id`（表格没有、图片段有）；GUI 对 Opaque 只读渲染（不可编辑），有身份的可删（删图片段真正生效），无身份的不给删按钮。`pending_ops` 的 skipped 因此只剩「无 paraId 文件」一种来源。
  *Stop gate：绿，真机验证：图片占位不可编辑、可删除且写回正确，表格占位不可编辑不可删。*

- **Step 16 ✅｜真图显示**：后端从 `view html` 抽 base64 PNG（实测已内嵌 `<img src="data:image/png;base64,…"`），按文档序与 annotated 的图片段配对（抽取/配对是纯函数，离线可测）；GUI 图片段从文本占位升级为真图渲染（引 `image` crate 解码 PNG，理由：egui 贴图需要解码）。
  *Stop gate：绿，真机：插入的图片在 GUI 里以真图显示，占位降级路径（抽不出图时）仍可见不丢。*

- **Step 17 ✅｜第一条 AI patch 回路（假 AI，通道全真）**：GUI 选中一个段落 → 「AI 改写」→ 产出 `SetSpan` patch（**来源标记为 AI**）→ review 面板（旧文/新文对照）→ 接受=commit（入撤销栈）/ 拒绝=丢弃。AI 源先用**假实现**（固定规则改写），重点是把「AI patch 必须过 review、Human patch 直达」的信任不对称通道造出来（原则 7 第一次落地）。**不引网络依赖。**
  *Stop gate：绿，真机验证：AI 改写必经 review，接受后可撤销，拒绝后模型无变化。*

- **Step 18 ✅｜真 AI 接入**：把假 AI 源换成真模型调用（方式到这步再定：HTTP API or 子进程，需要新依赖时说明理由）。review 通道一行不改——这是 Step 17 把接口切对的验收。
  *Stop gate：绿，真机：真模型提的 patch 走完 review→commit→undo 全程。*

> Step 18 之后再停下重新规划。**在那之前不要做：conflicts、Issue、Validation、完整 Capability 系统、MCP/对外 agent 接口、多模块（Spreadsheet/Slide/…）。**

---

## 明确不要做的事（现在）

- ❌ 不要一次实现整个 runtime / 整个 v0.4.2。
- ❌ **不要把 docx 导入成可编辑的完整 OOXML model**（黑洞）。现行路线就是对的：查看 + 按身份定向写回，没结构化的内容原样留在文件里不碰。
- ❌ 不要自己写 OOXML 解析器——docx 一律走 `DocxBackend`（officecli 子进程），且保持可替换（别和 OfficeCLI 深耦合）。
- ❌ 不要做 MCP/对外 agent 接口（应用内 AI patch 源按 Step 17/18 走，那不是对外接口）。
- ❌ 不要做 conflicts / Issue / Validation / 完整 Capability（等单文档 AI 回路跑通再说）。
- ❌ 不要做 Spreadsheet/Slide/Legal/Diagram 模块（先把 prose 这一条走通）。
- ❌ 不要引入用不到的依赖。需要时才引，并说明理由（先例：serde_json 为解析 officecli JSON、chrono 为版本列表显示本地时间）。

---

## 每步交付格式（你回复我时）

```
## Step N 完成
- 改动：<一句话>
- cargo build: <pass/fail>
- cargo test: <test result: ok. N passed / 报错原文>
- 真机验证: <做了什么、看到了什么>（涉及 GUI/docx 行为时必填）
- 下一步建议：<Step N+1 还是先修什么>
- 等你确认再继续。
```

---

## 开始

Step 0–18 已绿，**本阶段阶梯走完**。按章程：停下重新规划下一阶段（候选方向见 backlog 与总纲；不要自行开工）。
