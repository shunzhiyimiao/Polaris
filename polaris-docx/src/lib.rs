//! polaris-docx — office（docx）输入适配层。
//!
//! 严格在 `editor-core` 之外：内核保持中性（不塞 office 逻辑），docx 概念只在这里出现。
//! Step 7a：中性结构 `DocxBlock` + 可替换 `DocxBackend` trait + `FakeBackend` +
//! 「block → ProseModel」最小映射。**不碰子进程、不碰 OOXML、不引 serde。**
//! 真后端（OfficeCLI/Pandoc 子进程）是 Step 7b。

use editor_core::{DocumentModel, NodeId, NodeKind, Op, ProseModel, Target};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Command;

/// 文件级版本历史（写盘前快照、可回任意旧版）——对「写盘」effect 的补偿。
pub mod history;

/// 后端把一个 docx 拍平成的一串中性「块」。core/adapter 都不解析 OOXML，只消费它。
/// `para_id` 是该段在 docx 里的稳定身份（Word 的 w14:paraId，从后端 path 里带出）——
/// 写回时按它定位，**不按位置数**；表格等非段落元素没有它，paraId 缺失的文件（如 Pandoc 产物）为 None。
#[derive(Clone, Debug, PartialEq)]
pub enum DocxBlock {
    Heading { level: usize, text: String, para_id: Option<String> },
    Paragraph { text: String, para_id: Option<String> },
    /// 后端看得见、但还映射不进结构的东西（表格/图/复杂格式）：原样留痕，绝不丢弃。
    /// `para_id`：本体是段落的（如纯图段）带身份 → 可删可写回；体级非段落元素（表格）没有。
    /// `image`：真图字节（从 `view html` 的 data URI 解出）；拿不到 → None，显示降级为文字占位。
    /// `anchor`：体级元素的 `after:` 形式路径（如表格 `tbl[1]`），供写回时把新段锚到它之后。
    Unstructured { kind: String, raw: String, para_id: Option<String>, image: Option<Vec<u8>>, anchor: Option<String> },
}

impl DocxBlock {
    /// 该块的稳定段落身份。
    pub fn para_id(&self) -> Option<&str> {
        match self {
            DocxBlock::Heading { para_id, .. }
            | DocxBlock::Paragraph { para_id, .. }
            | DocxBlock::Unstructured { para_id, .. } => para_id.as_deref(),
        }
    }

    /// 该块作为「在它之后插入」锚点的 `after:` 形式路径：
    /// 段落/标题/图片段 → `p[@paraId=X]`（有身份才有）；表格等体级元素 → `tbl[N]`（来自 anchor）。
    /// 写回新增段时用它定位，使表格也能推进锚点（新段落落到表格**后**而非退到表格前）。
    pub fn body_anchor(&self) -> Option<String> {
        if let Some(pid) = self.para_id() {
            Some(format!("p[@paraId={pid}]"))
        } else if let DocxBlock::Unstructured { anchor, .. } = self {
            anchor.clone()
        } else {
            None
        }
    }
}

/// 可替换的 docx 后端：把一个 docx 路径转成一串中性块。
/// 真实现（子进程后端）是 Step 7b；这里先只有 trait + fake。
pub trait DocxBackend {
    fn read_blocks(&self, path: &str) -> Result<Vec<DocxBlock>, String>;
}

/// 离线/测试用假后端：忽略 path，回放预置 blocks。
pub struct FakeBackend {
    pub blocks: Vec<DocxBlock>,
}
impl FakeBackend {
    pub fn new(blocks: Vec<DocxBlock>) -> Self {
        FakeBackend { blocks }
    }
}
impl DocxBackend for FakeBackend {
    fn read_blocks(&self, _path: &str) -> Result<Vec<DocxBlock>, String> {
        Ok(self.blocks.clone())
    }
}

/// 一个 block 渲成节点纯文本。
fn block_to_text(block: &DocxBlock) -> String {
    match block {
        // heading 文本即纯标题（不带 `#`）；level 由 import_blocks 通过 SetKind 落进 typed kind。
        DocxBlock::Heading { text, .. } => text.clone(),
        DocxBlock::Paragraph { text, .. } => text.clone(),
        // 映射不进 → 文本就是原始描述；「这是不透明内容」由 typed `Opaque` kind 表达，不再做文本前缀 hack。
        DocxBlock::Unstructured { raw, .. } => raw.clone(),
    }
}

/// 把一串 block 顺序映射成 ProseModel 的顶层节点（追加在 root 下，文档序）。
/// 每个 block → 一个 `InsertSection`（typed patch）；标题/不透明块再补 `SetKind`（typed，不靠文本记法）。
/// 映射不进的落成 `Opaque` 节点（可见、core 层只读），绝不丢弃、绝不 panic。返回新建节点的 id（文档序）。
pub fn import_blocks(model: &mut ProseModel, blocks: &[DocxBlock]) -> Result<Vec<NodeId>, String> {
    let root = model.root().clone();
    let base = model.children(&root).map(|c| c.len()).unwrap_or(0);
    let mut ids = Vec::with_capacity(blocks.len());
    for (i, block) in blocks.iter().enumerate() {
        let id = NodeId(format!("docx:{}", base + i));
        let text = block_to_text(block);
        model
            .apply_op(
                &Target::Node(root.clone()),
                &Op::InsertSection { parent: root.clone(), index: base + i, id: id.clone(), text },
            )
            .map_err(|e| format!("import block {i}: {e:?}"))?;
        let kind = match block {
            DocxBlock::Heading { level, .. } => Some(NodeKind::Heading { level: *level }),
            DocxBlock::Unstructured { .. } => Some(NodeKind::Opaque),
            DocxBlock::Paragraph { .. } => None, // 默认即段落
        };
        if let Some(kind) = kind {
            model
                .apply_op(&Target::Node(id.clone()), &Op::SetKind { kind })
                .map_err(|e| format!("import block {i} setkind: {e:?}"))?;
        }
        ids.push(id);
    }
    Ok(ids)
}

/// 端到端：用后端读 docx，再映射进 model。Step 7a 用 `FakeBackend`；
/// 7b 换真后端（OfficeCLI 子进程），**调用方一行不用改**。
pub fn import_docx<B: DocxBackend>(
    backend: &B,
    path: &str,
    model: &mut ProseModel,
) -> Result<Vec<NodeId>, String> {
    let blocks = backend.read_blocks(path)?;
    import_blocks(model, &blocks)
}

// ───────────────────────── 真后端：OfficeCLI 子进程 ─────────────────────────
//
// 通过子进程调用外部 `officecli`（iOfficeAI/OfficeCLI, Apache-2.0）；不解析 OOXML、不绑死它
// （DocxBackend 可替换）。两次 view：`text --json` 拿每段文本（文档序）、`outline --json`
// 拿「哪些段是标题及其级别」，按段号合并成 DocxBlock。出处/许可见 THIRD_PARTY.md。

/// 用外部 `officecli` 子进程读 docx。`program` 默认 `"officecli"`（可注入绝对路径/替身）。
pub struct OfficeCliBackend {
    pub program: String,
}
impl OfficeCliBackend {
    pub fn new() -> Self {
        OfficeCliBackend { program: "officecli".to_string() }
    }
    pub fn with_program(program: impl Into<String>) -> Self {
        OfficeCliBackend { program: program.into() }
    }

    /// 跑 `officecli view <path> <mode> --json`，把 stdout 解析成 JSON。
    fn run_json(&self, path: &str, mode: &str) -> Result<Value, String> {
        let out = Command::new(&self.program)
            .args(["view", path, mode, "--json"])
            .output()
            .map_err(|e| format!("启动 {} 失败（officecli 装了吗？在 PATH 上吗？）: {e}", self.program))?;
        if !out.status.success() {
            return Err(format!(
                "officecli view {mode} 退出码非零: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        serde_json::from_slice(&out.stdout).map_err(|e| format!("officecli {mode} --json 解析失败: {e}"))
    }

    /// 把一组操作经 `officecli batch`（一次 open/save）写回 docx 文件（**就地改 `file`**）。
    /// set/remove/add 一并提交。空列表直接成功。
    pub fn save_ops(&self, file: &str, ops: &[DocxOp]) -> Result<(), String> {
        if ops.is_empty() {
            return Ok(());
        }
        // 关掉文件可能存在的常驻，确保 batch 走「干净的 open/save」，改动才会真正落盘。
        let _ = Command::new(&self.program).args(["close", file]).output();
        let commands = build_batch(ops);
        let out = Command::new(&self.program)
            .args(["batch", file, "--commands", &commands])
            .env("OFFICECLI_BATCH_ALLOW_STDIN_REDIRECT", "1")
            .output()
            .map_err(|e| format!("启动 {} 失败: {e}", self.program))?;
        if !out.status.success() {
            return Err(format!("officecli batch 退出码非零: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        // batch 即便部分失败也 exit 0，靠输出汇总行报；非「0 failed」即有未命中。
        let stdout = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = stdout.lines().find(|l| l.contains("Batch complete")) {
            if !line.contains("0 failed") {
                return Err(format!("部分改动未写回: {}", line.trim()));
            }
        }
        Ok(())
    }

}
impl Default for OfficeCliBackend {
    fn default() -> Self {
        Self::new()
    }
}
impl DocxBackend for OfficeCliBackend {
    fn read_blocks(&self, path: &str) -> Result<Vec<DocxBlock>, String> {
        let text = self.run_json(path, "text")?;
        let outline = self.run_json(path, "outline")?;
        // annotated 是 run 级标注，图片只在这里现身（text view 里纯图段=空段，会被显示层隐藏）。
        let annotated = self.run_json(path, "annotated")?;
        // html 内嵌真图字节（data URI）。拿不到不报错——显示降级为文字占位，内容不丢。
        let html = self.run_json(path, "html").ok();
        // view 会给文件留常驻进程；关掉它——否则之后对同文件 batch 会复用该常驻、save 不落盘。
        let _ = Command::new(&self.program).args(["close", path]).output();
        let blocks = parse_blocks(&text, &outline)?;
        let image_paras = parse_image_paras(
            annotated.pointer("/data/content").and_then(Value::as_str).unwrap_or(""),
        );
        let blocks = apply_image_paras(blocks, &image_paras);
        // html --json 形如 {"success":…, "data":"<html…>"}：data 即 HTML 字符串（实测）。
        let bytes = html
            .as_ref()
            .and_then(|h| h.pointer("/data").and_then(Value::as_str))
            .map(parse_html_images)
            .unwrap_or_default();
        Ok(attach_images(blocks, &pair_images(&image_paras, bytes)))
    }
}

/// 一条要写回 docx 的操作。
#[derive(Clone, Debug, PartialEq)]
pub enum DocxOp {
    /// 改某段文本：在 `path` 段内 find → replace。`path` 建议用稳定的 `/body/p[@paraId=X]`。
    Set { path: String, find: String, replace: String },
    /// 删某段（按路径，建议用 `@paraId`）。
    Remove { path: String },
    /// 加段落：`after`=Some(`"p[@paraId=X]"`) 插在该段后；None 追加到 `/body` 末尾。
    Add { after: Option<String>, text: String },
}

/// 把一组操作拼成 `officecli batch` 的 JSON 命令数组。**纯函数，可测。**
fn build_batch(ops: &[DocxOp]) -> String {
    let commands: Vec<Value> = ops
        .iter()
        .map(|op| match op {
            DocxOp::Set { path, find, replace } => serde_json::json!({
                "command": "set", "path": path, "props": { "find": find, "replace": replace },
            }),
            DocxOp::Remove { path } => serde_json::json!({ "command": "remove", "path": path }),
            DocxOp::Add { after, text } => {
                let mut cmd = serde_json::json!({
                    "command": "add", "parent": "/body", "type": "p", "props": { "text": text },
                });
                if let Some(a) = after {
                    cmd["after"] = Value::String(a.clone());
                }
                cmd
            }
        })
        .collect();
    Value::Array(commands).to_string()
}

/// 从 `"/body/p[@paraId=00100000]"` 形式的 path 抽出稳定身份；位置式 path（`p[3]`）或缺失 → None。
fn parse_para_id(path: &str) -> Option<String> {
    Some(path.split("@paraId=").nth(1)?.split(']').next()?.to_string())
}

/// 从 `annotated` view 的纯文本里找出**含图片的段**：(paraId, 图片标注摘要)，**文档序**。
/// 行形如 `[/body/p[@paraId=X]] [Image: alt="…", 15.2cm×18.8cm] ← Body Text`，
/// 且 alt 可能含换行（Word 自动生成的 alt 会跨多行）——所以从 `[Image:` 起跨行收集到配对的 `]`。
/// 文档序很关键：`view html` 里的 `<img>` 同为文档序，两者按序配对得到「段 ↔ 图字节」。
/// **纯函数**，便于离线单测。
fn parse_image_paras(annotated: &str) -> Vec<(String, String)> {
    let mut images: Vec<(String, String)> = Vec::new();
    let mut current_para: Option<String> = None;
    let mut collecting: Option<(String, String)> = None; // (paraId, 已收集的标注)
    for line in annotated.lines() {
        if let Some((para, mut label)) = collecting.take() {
            // alt 跨行：续收，直到出现收尾的 `]`
            let frag = line.split(']').next().unwrap_or(line);
            if !label.is_empty() && !frag.is_empty() {
                label.push(' ');
            }
            label.push_str(frag.trim());
            if line.contains(']') {
                images.push((para, truncate_chars(&label, 80)));
            } else {
                collecting = Some((para, label));
            }
            continue;
        }
        if line.starts_with('[') {
            if let Some(id) = parse_para_id(line) {
                current_para = Some(id);
            }
        }
        if let Some(start) = line.find("[Image:") {
            if let Some(para) = current_para.clone() {
                let rest = &line[start + "[Image:".len()..];
                match rest.find(']') {
                    Some(end) => {
                        images.push((para, truncate_chars(rest[..end].trim(), 80)));
                    }
                    None => collecting = Some((para, rest.trim().to_string())), // 跨行，续收
                }
            }
        }
    }
    images
}

/// 从 `view html`（data URI 内嵌）抽出全部图片字节，**文档序**。
/// 形如 `src="data:image/png;base64,…"`；解不开的跳过（不 panic），格式不限 png。**纯函数**。
fn parse_html_images(html: &str) -> Vec<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("data:image/") {
        rest = &rest[i..];
        let Some(marker) = rest.find(";base64,") else { break };
        let payload = &rest[marker + ";base64,".len()..];
        let end = payload.find(['"', '\'', ')']).unwrap_or(payload.len());
        let cleaned: String = payload[..end].chars().filter(|c| !c.is_whitespace()).collect();
        if let Ok(bytes) = STANDARD.decode(cleaned.as_bytes()) {
            out.push(bytes);
        }
        rest = &payload[end..];
    }
    out
}

/// 按文档序配对：第 i 个含图段 ↔ 第 i 个 `<img>` 字节。数量不齐时多余的忽略、缺的不配
/// （显示端降级为文字占位，不丢内容）。同段多图取第一张。**纯函数**。
fn pair_images(ordered: &[(String, String)], bytes: Vec<Vec<u8>>) -> HashMap<String, Vec<u8>> {
    let mut map = HashMap::new();
    for ((para, _), img) in ordered.iter().zip(bytes) {
        map.entry(para.clone()).or_insert(img);
    }
    map
}

/// 把配对好的图片字节装进对应的图片占位块（按 para_id 对号）。**纯函数**。
fn attach_images(blocks: Vec<DocxBlock>, images: &HashMap<String, Vec<u8>>) -> Vec<DocxBlock> {
    blocks
        .into_iter()
        .map(|b| match b {
            DocxBlock::Unstructured { kind, raw, para_id: Some(id), image: None, anchor }
                if images.contains_key(&id) =>
            {
                DocxBlock::Unstructured {
                    kind,
                    raw,
                    image: Some(images[&id].clone()),
                    para_id: Some(id),
                    anchor,
                }
            }
            other => other,
        })
        .collect()
}

/// 按 char 截断（CJK 安全），超长加省略号。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

/// 把**纯图段**（自身无文字、但含图片的段）升级为可见的 Unstructured 占位——否则它是空段，
/// 会被显示层隐藏，图片就「消失」了（违反「全部可见」）。带文字的图文混排段保持原样（文字仍可编辑）。
/// 真图字节由 `attach_images` 随后装入；这里先 None（拿不到图时它就是最终形态=文字占位）。
fn apply_image_paras(blocks: Vec<DocxBlock>, images: &[(String, String)]) -> Vec<DocxBlock> {
    let label_of: HashMap<&str, &str> = {
        let mut m = HashMap::new();
        for (id, label) in images {
            m.entry(id.as_str()).or_insert(label.as_str()); // 同段多图取第一条标注
        }
        m
    };
    blocks
        .into_iter()
        .map(|b| match &b {
            DocxBlock::Paragraph { text, para_id: Some(id) }
                if text.is_empty() && label_of.contains_key(id.as_str()) =>
            {
                // 本体是段落 → 身份保留：图片段因此可删、删了能按 paraId 写回（锚走 para_id，anchor None）。
                DocxBlock::Unstructured {
                    kind: "image".to_string(),
                    raw: label_of[id.as_str()].to_string(),
                    para_id: Some(id.clone()),
                    image: None,
                    anchor: None,
                }
            }
            _ => b,
        })
        .collect()
}

/// 合并 `text --json`（元素序列，文档序）与 `outline --json`（标题→级别）成 DocxBlock 序列。
/// outline 的 `line` = 该标题是**第几个段落**（1 起，表格等非段落元素不占号，实测语义）——
/// 所以这里对段落元素跑计数器匹配，**不解析 path 里的位置数字**（@paraId 形式的 path 没有位置数字）。
/// 段落的 `@paraId` 一并从 path 带出，作为写回时的稳定身份。
/// 非段落元素（表格/图/…）→ Unstructured，绝不丢弃。**纯函数**：吃两个 JSON，便于离线单测。
fn parse_blocks(text: &Value, outline: &Value) -> Result<Vec<DocxBlock>, String> {
    // 标题段号（第几个段落，1 起）→ 级别
    let mut heading_level: HashMap<u64, usize> = HashMap::new();
    if let Some(hs) = outline.pointer("/data/headings").and_then(Value::as_array) {
        for h in hs {
            if let (Some(line), Some(level)) =
                (h.get("line").and_then(Value::as_u64), h.get("level").and_then(Value::as_u64))
            {
                heading_level.insert(line, level as usize);
            }
        }
    }

    let elements = text
        .pointer("/data/elements")
        .and_then(Value::as_array)
        .ok_or_else(|| "text --json 缺少 data.elements 数组".to_string())?;

    let mut blocks = Vec::with_capacity(elements.len());
    let mut para_no: u64 = 0; // 已见段落计数（1 起），与 outline 的 line 同一坐标系
    for el in elements {
        let typ = el.get("type").and_then(Value::as_str).unwrap_or("");
        let txt = el.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        let path = el.get("path").and_then(Value::as_str).unwrap_or("");
        if typ == "paragraph" {
            para_no += 1;
            let para_id = parse_para_id(path);
            match heading_level.get(&para_no).copied() {
                Some(level) => blocks.push(DocxBlock::Heading { level, text: txt, para_id }),
                None => blocks.push(DocxBlock::Paragraph { text: txt, para_id }),
            }
        } else {
            // 段落以外的元素（表格/图/…）：原样存成不透明块，绝不丢弃。
            // anchor = 体相对路径（`/body/tbl[1]` → `tbl[1]`），供写回把新段锚到它之后。
            let anchor = path.strip_prefix("/body/").map(str::to_string);
            blocks.push(DocxBlock::Unstructured { kind: typ.to_string(), raw: txt, para_id: None, image: None, anchor });
        }
    }
    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_core::render_html;

    fn import(blocks: Vec<DocxBlock>) -> ProseModel {
        let mut m = ProseModel::new();
        let backend = FakeBackend::new(blocks);
        import_docx(&backend, "ignored.docx", &mut m).unwrap();
        m
    }

    #[test]
    fn maps_heading_and_paragraph_and_views() {
        let m = import(vec![
            DocxBlock::Heading { level: 1, text: "Intro".to_string(), para_id: None },
            DocxBlock::Paragraph { text: "Hello world".to_string(), para_id: None },
        ]);
        // 映射成 model 节点：标题是 typed heading + 纯文本（不再带 `#`）
        assert_eq!(m.get_text(&NodeId("docx:0".to_string())), Some("Intro"));
        assert_eq!(m.node_kind(&NodeId("docx:0".to_string())), Some(NodeKind::Heading { level: 1 }));
        assert_eq!(m.get_text(&NodeId("docx:1".to_string())), Some("Hello world"));
        assert_eq!(m.node_kind(&NodeId("docx:1".to_string())), Some(NodeKind::Paragraph));
        // Step 6 查看器：看见标题 + 正文（HTML 不变）
        let r = render_html(&m);
        assert_eq!(
            r.html,
            "<h1 data-node=\"docx:0\">Intro</h1><p data-node=\"docx:1\">Hello world</p>"
        );
    }

    #[test]
    fn unstructured_is_visible_not_dropped() {
        let m = import(vec![
            DocxBlock::Heading { level: 2, text: "Sec".to_string(), para_id: None },
            DocxBlock::Unstructured { kind: "table".to_string(), raw: "A | B".to_string(), para_id: None, image: None, anchor: None },
            DocxBlock::Paragraph { text: "after".to_string(), para_id: None },
        ]);
        // 映射不进 → typed Opaque 节点（不是文本前缀 hack），文本=原始描述
        assert_eq!(m.node_kind(&NodeId("docx:1".to_string())), Some(NodeKind::Opaque));
        let r = render_html(&m);
        // 可见（<pre>）；前后块都在，没丢、没 panic
        assert_eq!(
            r.html,
            "<h2 data-node=\"docx:0\">Sec</h2><pre data-node=\"docx:1\">A | B</pre><p data-node=\"docx:2\">after</p>"
        );
        // 只读：SourceMap 不含 Opaque 片段（2 条，不是 3 条）
        assert_eq!(r.source_map.spans.len(), 2);
    }

    #[test]
    fn import_appends_in_document_order() {
        let mut m = ProseModel::new();
        let ids1 = import_docx(
            &FakeBackend::new(vec![DocxBlock::Paragraph { text: "one".to_string(), para_id: None }]),
            "a.docx",
            &mut m,
        )
        .unwrap();
        let ids2 = import_docx(
            &FakeBackend::new(vec![DocxBlock::Paragraph { text: "two".to_string(), para_id: None }]),
            "b.docx",
            &mut m,
        )
        .unwrap();
        assert_eq!(ids1, vec![NodeId("docx:0".to_string())]);
        assert_eq!(ids2, vec![NodeId("docx:1".to_string())]);
        let r = render_html(&m);
        assert_eq!(r.html, "<p data-node=\"docx:0\">one</p><p data-node=\"docx:1\">two</p>");
    }

    #[test]
    fn cjk_heading_imports_and_renders() {
        let m = import(vec![DocxBlock::Heading { level: 1, text: "标题".to_string(), para_id: None }]);
        let r = render_html(&m);
        assert_eq!(r.html, "<h1 data-node=\"docx:0\">标题</h1>");
    }

    // ── Step 7b：OfficeCLI 后端的 JSON 合并解析（用 canned JSON，不碰子进程/真文件）──

    #[test]
    fn parse_para_id_extracts_identity() {
        assert_eq!(parse_para_id("/body/p[@paraId=00100000]"), Some("00100000".to_string()));
        assert_eq!(parse_para_id("/body/p[3]"), None); // 位置式 path（无 paraId 的文件）
        assert_eq!(parse_para_id("/body/tbl[1]"), None);
    }

    #[test]
    fn parse_blocks_merges_text_and_outline() {
        // 位置式 path（paraId 缺失的文件，如 Pandoc/textutil 产物）：标题靠段落计数器照样命中。
        let text = serde_json::json!({
            "success": true,
            "data": { "elements": [
                { "path": "/body/p[1]", "type": "paragraph", "text": "Title" },
                { "path": "/body/p[2]", "type": "paragraph", "text": "一、节" },
                { "path": "/body/p[3]", "type": "paragraph", "text": "body text" },
                { "path": "/body/p[4]", "type": "paragraph", "text": "" },
                { "path": "/body/tbl[1]", "type": "table", "text": "r1c1 r1c2" }
            ]}
        });
        let outline = serde_json::json!({
            "success": true,
            "data": { "headings": [
                { "line": 1, "text": "Title", "level": 1 },
                { "line": 2, "text": "一、节", "level": 2 }
            ]}
        });
        let blocks = parse_blocks(&text, &outline).unwrap();
        assert_eq!(
            blocks,
            vec![
                DocxBlock::Heading { level: 1, text: "Title".to_string(), para_id: None },
                DocxBlock::Heading { level: 2, text: "一、节".to_string(), para_id: None },
                DocxBlock::Paragraph { text: "body text".to_string(), para_id: None },
                DocxBlock::Paragraph { text: "".to_string(), para_id: None },
                DocxBlock::Unstructured { kind: "table".to_string(), raw: "r1c1 r1c2".to_string(), para_id: None, image: None, anchor: Some("tbl[1]".to_string()) },
            ]
        );
    }

    #[test]
    fn parse_blocks_para_id_paths_with_table_between() {
        // 真实 Word 形态（实测 officecli 输出）：path 带 @paraId、表格是体级 tbl 元素。
        // 关键断言：outline 的 line 只数段落（表格不占号）——表格后的标题必须仍被识别；
        // 每个段落的 paraId 被带出来作为写回身份。
        let text = serde_json::json!({"data":{"elements":[
            {"path":"/body/p[@paraId=AA]","type":"paragraph","text":"总标题"},
            {"path":"/body/p[@paraId=BB]","type":"paragraph","text":"前段"},
            {"path":"/body/tbl[1]","type":"table","text":"[Table: 1 rows]"},
            {"path":"/body/p[@paraId=CC]","type":"paragraph","text":"表格后的节标题"},
            {"path":"/body/p[@paraId=DD]","type":"paragraph","text":"后段"}
        ]}});
        let outline = serde_json::json!({"data":{"headings":[
            {"line":1,"level":1,"text":"总标题"},
            {"line":3,"level":2,"text":"表格后的节标题"}
        ]}});
        let blocks = parse_blocks(&text, &outline).unwrap();
        assert_eq!(
            blocks,
            vec![
                DocxBlock::Heading { level: 1, text: "总标题".to_string(), para_id: Some("AA".to_string()) },
                DocxBlock::Paragraph { text: "前段".to_string(), para_id: Some("BB".to_string()) },
                // 表格的 after 锚从 path 取：`/body/tbl[1]` → `tbl[1]`（Step 22 ①）
                DocxBlock::Unstructured { kind: "table".to_string(), raw: "[Table: 1 rows]".to_string(), para_id: None, image: None, anchor: Some("tbl[1]".to_string()) },
                DocxBlock::Heading { level: 2, text: "表格后的节标题".to_string(), para_id: Some("CC".to_string()) },
                DocxBlock::Paragraph { text: "后段".to_string(), para_id: Some("DD".to_string()) },
            ]
        );
        // body_anchor()：段落给 p[@paraId]、表格给 tbl[N]
        assert_eq!(blocks[1].body_anchor(), Some("p[@paraId=BB]".to_string()));
        assert_eq!(blocks[2].body_anchor(), Some("tbl[1]".to_string()));
    }

    #[test]
    fn parse_blocks_then_import_and_render() {
        // 端到端（canned JSON，不碰子进程）：合并 → import → Step 6 渲染成 viewer HTML
        let text = serde_json::json!({"data":{"elements":[
            {"path":"/body/p[@paraId=AA]","type":"paragraph","text":"标题"},
            {"path":"/body/p[@paraId=BB]","type":"paragraph","text":"正文"}
        ]}});
        let outline = serde_json::json!({"data":{"headings":[{"line":1,"level":1,"text":"标题"}]}});
        let blocks = parse_blocks(&text, &outline).unwrap();
        let mut m = ProseModel::new();
        import_blocks(&mut m, &blocks).unwrap();
        let r = render_html(&m);
        assert_eq!(r.html, "<h1 data-node=\"docx:0\">标题</h1><p data-node=\"docx:1\">正文</p>");
    }

    #[test]
    fn parse_blocks_missing_elements_errors_not_panics() {
        let bad = serde_json::json!({ "success": false });
        assert!(parse_blocks(&bad, &bad).is_err());
    }

    // ── 图片可见性（annotated view 解析；fixture 取自真实 officecli 输出）──

    #[test]
    fn parse_image_paras_handles_multiline_alt() {
        // 真实形状：alt 含换行，标注跨 3 行才闭合；另一段是单行图片标注。**结果按文档序。**
        let annotated = "[/body/p[@paraId=00100008]] 「333333」 ← First Paragraph | 宋体 12pt bold\n\
                         [/body/p[@paraId=5B163514]] [Image: alt=\"表格\n\
                         \n\
                         AI 生成的内容可能不正确。\", 15.2cm×18.8cm] ← Body Text\n\
                         [/body/p[@paraId=0010000A]] 「」 ← Normal | 宋体 12pt\n\
                         [/body/p[@paraId=AABBCC01]] [Image: alt=\"logo\", 2cm×2cm] ← Body Text\n";
        let images = parse_image_paras(annotated);
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].0, "5B163514"); // 文档序
        assert!(images[0].1.starts_with("alt=\"表格"));
        assert!(images[0].1.contains("15.2cm×18.8cm"));
        assert_eq!(images[1], ("AABBCC01".to_string(), "alt=\"logo\", 2cm×2cm".to_string()));
    }

    #[test]
    fn apply_image_paras_makes_pure_image_paragraph_visible() {
        let images = vec![("IMG1".to_string(), "alt=\"图\", 1cm×1cm".to_string())];
        let blocks = vec![
            DocxBlock::Paragraph { text: "有字".to_string(), para_id: Some("IMG1".to_string()) }, // 图文混排：不动
            DocxBlock::Paragraph { text: "".to_string(), para_id: Some("IMG1".to_string()) },     // 纯图段：升级
            DocxBlock::Paragraph { text: "".to_string(), para_id: Some("EMPTY".to_string()) },    // 普通空段：不动
            DocxBlock::Paragraph { text: "".to_string(), para_id: None },                          // 无身份空段：不动
        ];
        let out = apply_image_paras(blocks, &images);
        assert_eq!(out[0], DocxBlock::Paragraph { text: "有字".to_string(), para_id: Some("IMG1".to_string()) });
        assert_eq!(
            out[1],
            DocxBlock::Unstructured {
                kind: "image".to_string(),
                raw: "alt=\"图\", 1cm×1cm".to_string(),
                para_id: Some("IMG1".to_string()),
                image: None,
                anchor: None,
            }
        );
        assert_eq!(out[2], DocxBlock::Paragraph { text: "".to_string(), para_id: Some("EMPTY".to_string()) });
        assert_eq!(out[3], DocxBlock::Paragraph { text: "".to_string(), para_id: None });
    }

    #[test]
    fn pure_image_paragraph_renders_as_visible_placeholder() {
        // 端到端到查看器：纯图段 → Unstructured → 可见占位（不再消失）
        let images = vec![("X1".to_string(), "alt=\"产品图\"".to_string())];
        let blocks = apply_image_paras(
            vec![
                DocxBlock::Paragraph { text: "前".to_string(), para_id: Some("AA".to_string()) },
                DocxBlock::Paragraph { text: "".to_string(), para_id: Some("X1".to_string()) },
                DocxBlock::Paragraph { text: "后".to_string(), para_id: Some("BB".to_string()) },
            ],
            &images,
        );
        let mut m = ProseModel::new();
        import_blocks(&mut m, &blocks).unwrap();
        let r = render_html(&m);
        assert_eq!(
            r.html,
            "<p data-node=\"docx:0\">前</p><pre data-node=\"docx:1\">alt=\"产品图\"</pre><p data-node=\"docx:2\">后</p>"
        );
    }

    // ── Step 16：真图字节（html data URI 抽取 → 按文档序配对 → 装块）──

    #[test]
    fn parse_html_images_decodes_data_uris_in_order() {
        // "P1"/"P2" 的 base64 = UDE= / UDI=；中间夹一个外链 img（跳过）和一个坏 base64（跳过）
        let html = r#"<html><body>
            <img src="data:image/png;base64,UDE=" alt="第一张"/>
            <img src="https://example.com/x.png"/>
            <img src="data:image/jpeg;base64,@@bad@@"/>
            <p>正文</p>
            <img src='data:image/png;base64,UDI='/>
        </body></html>"#;
        let imgs = parse_html_images(html);
        assert_eq!(imgs, vec![b"P1".to_vec(), b"P2".to_vec()]);
    }

    #[test]
    fn pair_images_zips_by_document_order_and_tolerates_mismatch() {
        let ordered = vec![
            ("AA".to_string(), "x".to_string()),
            ("BB".to_string(), "y".to_string()),
            ("CC".to_string(), "z".to_string()),
        ];
        // 只抽到 2 张图 → CC 配不上（显示降级），不报错
        let map = pair_images(&ordered, vec![b"1".to_vec(), b"2".to_vec()]);
        assert_eq!(map.len(), 2);
        assert_eq!(map["AA"], b"1".to_vec());
        assert_eq!(map["BB"], b"2".to_vec());
        assert!(!map.contains_key("CC"));
    }

    #[test]
    fn attach_images_fills_matching_opaque_blocks_only() {
        let mut map = HashMap::new();
        map.insert("IMG".to_string(), b"PNGBYTES".to_vec());
        let blocks = vec![
            DocxBlock::Paragraph { text: "前".to_string(), para_id: Some("AA".to_string()) },
            DocxBlock::Unstructured {
                kind: "image".to_string(),
                raw: "alt".to_string(),
                para_id: Some("IMG".to_string()),
                image: None,
                anchor: None,
            },
            // 表格：无身份 → 不装
            DocxBlock::Unstructured { kind: "table".to_string(), raw: "[T]".to_string(), para_id: None, image: None, anchor: None },
            // 配不上的图片段 → 保持 None（降级为文字占位）
            DocxBlock::Unstructured {
                kind: "image".to_string(),
                raw: "alt2".to_string(),
                para_id: Some("MISSING".to_string()),
                image: None,
                anchor: None,
            },
        ];
        let out = attach_images(blocks, &map);
        assert!(matches!(&out[1], DocxBlock::Unstructured { image: Some(b), .. } if b == b"PNGBYTES"));
        assert!(matches!(&out[2], DocxBlock::Unstructured { image: None, .. }));
        assert!(matches!(&out[3], DocxBlock::Unstructured { image: None, .. }));
    }

    #[test]
    fn build_batch_emits_set_remove_add() {
        let ops = vec![
            DocxOp::Set { path: "/body/p[@paraId=AA]".to_string(), find: "旧".to_string(), replace: "新".to_string() },
            DocxOp::Remove { path: "/body/p[@paraId=BB]".to_string() },
            DocxOp::Add { after: Some("p[@paraId=AA]".to_string()), text: "插段".to_string() },
            DocxOp::Add { after: None, text: "尾段".to_string() },
        ];
        let json = build_batch(&ops);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 4);
        assert_eq!(v[0]["command"], "set");
        assert_eq!(v[0]["path"], "/body/p[@paraId=AA]");
        assert_eq!(v[0]["props"]["replace"], "新");
        assert_eq!(v[1]["command"], "remove");
        assert_eq!(v[1]["path"], "/body/p[@paraId=BB]");
        assert_eq!(v[2]["command"], "add");
        assert_eq!(v[2]["after"], "p[@paraId=AA]"); // 插在指定段后
        assert_eq!(v[2]["props"]["text"], "插段");
        assert!(v[3].get("after").is_none()); // None → 追加末尾，无 after 键
        assert_eq!(v[3]["props"]["text"], "尾段");
    }
}
