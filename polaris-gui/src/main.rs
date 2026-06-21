//! polaris-gui — 原生 GUI（eframe/egui）。
//!
//! 能力：打开真实 docx（标题/段落/表格占位/真图）→ 编辑（失焦经 `apply_fragment_edit` 原子提交，
//! 撤销栈 = 逆 PatchSet）→ 按 `@paraId` 身份写回 → 版本历史（留底/回任意旧版）→
//! AI 改写提案（假 AI 真通道：AI patch 必须过 review 对照面板，core 层 commit 闸口强制）。
//! editor-core 保持中性；GUI 只调它的公开 API。逻辑可测，egui 视图层薄。
//!
//! 跑：`cargo run -p polaris-gui`

use eframe::egui;
use editor_core::{
    apply_fragment_edit, commit, render_html, DocumentModel, EditError, NodeId, NodeKind, Op, Patch,
    PatchSet, ProseModel, Target,
};
use polaris_docx::{history, import_blocks, DocxBackend, DocxBlock, DocxOp, OfficeCliBackend};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc};

/// 一个可显示/可编辑的块：节点 id + 类型 + 文本缓冲。
#[derive(Clone, Debug, PartialEq)]
struct ViewBlock {
    node: NodeId,
    kind: NodeKind,
    text: String,
}

/// 一条 AI 改动建议（旧文/新文对照 + 勾选状态）。
struct AiChange {
    node: NodeId,
    old: String,
    new: String,
    selected: bool,
}

/// 等待 review 的 AI 提案：**多条**改动，逐条勾选，接受的合成一个 PatchSet 原子提交
/// （= 一个撤销单位）。提案本身**不碰 model**；某条的段落被人改过 → 该条失效不可勾。
struct AiProposal {
    changes: Vec<AiChange>,
}

/// diff 的一段（相对 old→new）：相等 / 删除（只在 old）/ 插入（只在 new）。
#[derive(Clone, Debug, PartialEq)]
enum DiffPart {
    Equal(String),
    Delete(String),
    Insert(String),
}

/// 字符级 LCS diff（old → new），相邻同类 char 合并成串便于着色。**纯函数**。
/// CJK 无词边界，按 char 对齐最稳；段落级长度下 O(n·m) DP 足够快（不引 diff 依赖）。
fn char_diff(old: &str, new: &str) -> Vec<DiffPart> {
    let a: Vec<char> = old.chars().collect();
    let b: Vec<char> = new.chars().collect();
    let (n, m) = (a.len(), b.len());
    // dp[i][j] = LCS(a[i..], b[j..]) 的长度
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] { dp[i + 1][j + 1] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
        }
    }
    // 相邻同类合并：kind 0=Equal 1=Delete 2=Insert
    fn push(parts: &mut Vec<DiffPart>, kind: u8, ch: char) {
        match parts.last_mut() {
            Some(DiffPart::Equal(s)) if kind == 0 => s.push(ch),
            Some(DiffPart::Delete(s)) if kind == 1 => s.push(ch),
            Some(DiffPart::Insert(s)) if kind == 2 => s.push(ch),
            _ => parts.push(match kind {
                0 => DiffPart::Equal(ch.to_string()),
                1 => DiffPart::Delete(ch.to_string()),
                _ => DiffPart::Insert(ch.to_string()),
            }),
        }
    }
    // 回溯成编辑序列（差异时删优先于插，保证确定性）
    let mut parts = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            push(&mut parts, 0, a[i]);
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            push(&mut parts, 1, a[i]);
            i += 1;
        } else {
            push(&mut parts, 2, b[j]);
            j += 1;
        }
    }
    while i < n {
        push(&mut parts, 1, a[i]);
        i += 1;
    }
    while j < m {
        push(&mut parts, 2, b[j]);
        j += 1;
    }
    parts
}

/// 把一段 diff 渲染成内联着色文本：相等=弱灰、删除=红+删除线、插入=绿+加粗。
/// 一眼看出改了哪几个字（无空隙拼接，靠颜色而非位置区分）。
fn show_diff(ui: &mut egui::Ui, parts: &[DiffPart]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for part in parts {
            let rt = match part {
                DiffPart::Equal(s) => egui::RichText::new(s).weak(),
                DiffPart::Delete(s) => {
                    egui::RichText::new(s).color(egui::Color32::from_rgb(200, 60, 60)).strikethrough()
                }
                DiffPart::Insert(s) => egui::RichText::new(s).color(egui::Color32::from_rgb(40, 150, 70)).strong(),
            };
            ui.label(rt);
        }
    });
}

/// 一次 AI 请求的种类与快照（发起时的旧文随请求走：收货时用来判断段落是否已被人改过）。
enum AiRequest {
    /// 单段改写。
    Single { node: NodeId, old: String },
    /// 全文清理：请求时全部可编辑段的 (id, 旧文) 快照。
    CleanAll { olds: Vec<(NodeId, String)> },
}

/// 一次 AI 调用的结果（后台线程经 channel 送回 UI 线程；result = 模型**原始文本**，解析在 UI 线程做）。
struct AiReply {
    request: AiRequest,
    result: Result<String, String>,
}

/// 单段改写提示词。**纯函数**，便于单测锚定关键约束。
fn ai_rewrite_prompt(text: &str) -> String {
    format!(
        "你是文档润色助手。改写下面这段话，使其更通顺、专业、简洁；保持原意、保持中文、\
         保留专有名词与数字；只输出改写后的正文，不要任何解释、前后缀、引号或 markdown。\n\n原文：\n{text}"
    )
}

/// 全文清理提示词：段落带 id 列出，要求模型只回「需要改的段」的 JSON 数组。**纯函数**。
fn ai_clean_prompt(segments: &[(NodeId, String)]) -> String {
    let mut body = String::new();
    for (id, text) in segments {
        body.push_str(&format!("[{}] {}\n", id.0, text));
    }
    format!(
        "你是文档清理助手。下面是一篇文档的段落列表，每段开头方括号里是它的 id。\n\
         任务：找出需要清理的段落——删除明显的测试杂质（如插在正文里的无意义数字串 1111/2222/333333 等）、\
         修正明显的误植；其余内容**原样保留**，不要做风格性改写，不要动没有问题的段落。\n\
         只输出一个 JSON 数组，元素形如 {{\"id\": \"...\", \"new\": \"...\"}}，**只包含需要修改的段落**；\
         没有要改的就输出 []。不要任何解释、markdown 代码栅栏或其它文本。\n\n{body}"
    )
}

/// 解析「全文清理」的模型回复（JSON 数组 `[{id,new}]`）成改动列表。
/// 只认请求里存在的 id；new 与请求时旧文相同的丢弃。返回 (改动列表, 丢弃数)。**纯函数**。
fn parse_clean_reply(raw: &str, olds: &[(NodeId, String)]) -> Result<(Vec<AiChange>, usize), String> {
    let cleaned = parse_ai_reply(raw);
    let items: Vec<serde_json::Value> =
        serde_json::from_str(&cleaned).map_err(|e| format!("模型回复不是合法 JSON 数组: {e}"))?;
    let old_of: HashMap<&str, &str> = olds.iter().map(|(n, t)| (n.0.as_str(), t.as_str())).collect();
    let mut changes = Vec::new();
    let mut dropped = 0usize;
    for it in &items {
        let (Some(id), Some(new)) =
            (it.get("id").and_then(|v| v.as_str()), it.get("new").and_then(|v| v.as_str()))
        else {
            dropped += 1;
            continue;
        };
        match old_of.get(id) {
            Some(old) if *old != new => changes.push(AiChange {
                node: NodeId(id.to_string()),
                old: old.to_string(),
                new: new.to_string(),
                selected: true,
            }),
            _ => dropped += 1, // 不认识的 id / 无实质变化
        }
    }
    Ok((changes, dropped))
}

/// 清洗模型回复：去首尾空白、剥 markdown 代码栅栏、剥成对的首尾引号（模型偶尔套壳）。**纯函数**。
fn parse_ai_reply(reply: &str) -> String {
    let mut s = reply.trim();
    if s.starts_with("```") {
        s = s.trim_start_matches(|c| c != '\n').trim_start_matches('\n');
        if let Some(end) = s.rfind("```") {
            s = &s[..end];
        }
        s = s.trim();
    }
    for (a, b) in [('"', '"'), ('“', '”'), ('「', '」')] {
        if s.chars().count() >= 2 && s.starts_with(a) && s.ends_with(b) {
            s = &s[a.len_utf8()..s.len() - b.len_utf8()];
            s = s.trim();
        }
    }
    s.to_string()
}

/// 真 AI 源：子进程调本机 `claude -p`（复用已认证的 Claude Code CLI，零新依赖——与 officecli 同模式）。
/// 提示词走 stdin，返回模型**原始文本**（解析交给调用方的纯函数）。
/// 阻塞数秒到一两分钟，**调用方必须放后台线程**。
/// 实测：认证失败 exit=1 且错误走 stdout——所以失败信息把 stdout 也带上。
fn claude_prompt(prompt: &str) -> Result<String, String> {
    let mut child = Command::new("claude")
        .args(["-p", "--model", "haiku"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 claude 失败（Claude Code CLI 装了吗？在 PATH 上吗？）: {e}"))?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(prompt.as_bytes())
        .map_err(|e| format!("写入 claude stdin 失败: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("等待 claude 失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("claude 调用失败: {} {}", stdout.trim(), stderr.trim()));
    }
    Ok(stdout.into_owned())
}

/// 文档序前序遍历，收集非空节点为块（与 renderer 同构，但产数据）。
fn document_blocks(model: &ProseModel) -> Vec<ViewBlock> {
    fn walk(model: &ProseModel, id: &NodeId, out: &mut Vec<ViewBlock>) {
        if let Some(text) = model.get_text(id) {
            if !text.is_empty() {
                out.push(ViewBlock {
                    node: id.clone(),
                    kind: model.node_kind(id).unwrap_or(NodeKind::Paragraph),
                    text: text.to_string(),
                });
            }
        }
        if let Some(children) = model.children(id) {
            for c in children {
                walk(model, c, out);
            }
        }
    }
    let mut out = Vec::new();
    let root = model.root().clone();
    walk(model, &root, &mut out);
    out
}

/// 内置样例文档（含中文标题/段落）。
fn sample_model() -> ProseModel {
    let mut m = ProseModel::new();
    let root = m.root().clone();
    let blocks: &[(&str, &str, Option<usize>)] = &[
        ("t", "Polaris 文档查看器", Some(1)),
        ("s1", "一、它是什么", Some(2)),
        ("p1", "以 Typed Model 为唯一真理之源的 AI-native 文档 runtime。", None),
        ("s2", "二、现在能干嘛", Some(2)),
        ("p2", "点开标题或正文就能改，失焦自动回写到 model；左上角可撤销。", None),
    ];
    for (i, (id, text, level)) in blocks.iter().enumerate() {
        m.apply_op(
            &Target::Node(root.clone()),
            &Op::InsertSection {
                parent: root.clone(),
                index: i,
                id: NodeId(id.to_string()),
                text: text.to_string(),
            },
        )
        .expect("sample insert");
        if let Some(level) = level {
            m.apply_op(
                &Target::Node(NodeId(id.to_string())),
                &Op::SetKind { kind: NodeKind::Heading { level: *level } },
            )
            .expect("sample setkind");
        }
    }
    m
}

struct PolarisApp {
    model: ProseModel,
    blocks: Vec<ViewBlock>,
    undo_stack: Vec<PatchSet>,
    original: HashMap<NodeId, String>,
    /// 节点 → docx 段落稳定身份（@paraId），加载时从 block 带出。写回按它定位，不按位置数。
    /// 不在表里的节点（表格占位、无 paraId 的文件）写不回——跳过并计数，绝不打错段。
    para_map: HashMap<NodeId, String>,
    /// 节点 → 真图字节（图片占位块）。Arc 让每帧渲染零拷贝；没有的图片块降级为文字占位。
    image_map: HashMap<NodeId, Arc<[u8]>>,
    /// 节点 → 「在它之后插入」的 `after:` 锚（段落 `p[@paraId=X]` / 表格 `tbl[N]`）。
    /// 新增段写回时遍历它推进锚点——表格也算数，故表格后的新段锚到表格后，不退到表格前。
    body_anchor: HashMap<NodeId, String>,
    loaded_path: Option<String>,
    status: String,
    doc_path: String,
    new_counter: usize,
    focused_once: bool,
    show_history: bool,
    /// 回版二次确认：有未保存改动时，第一次点某版本只警告，记在这里；再点同一版本才执行。
    confirm_restore: Option<PathBuf>,
    /// 待 review 的 AI 改写提案（原则 7：AI patch 必须过 review 才能 commit）。
    pending_ai: Option<AiProposal>,
    /// 在途的 AI 请求（后台线程跑 claude 子进程，~10s）。Some = 占线，一次一个。
    ai_rx: Option<mpsc::Receiver<AiReply>>,
}
impl PolarisApp {
    fn new() -> Self {
        let model = sample_model();
        let blocks = document_blocks(&model);
        let home = std::env::var("HOME").unwrap_or_default();
        // POLARIS_DOC 可指定启动时路径栏的默认文档（也方便沙箱验证：真 HOME + 沙箱文档）。
        let doc_path = std::env::var("POLARIS_DOC")
            .unwrap_or_else(|_| format!("{home}/Downloads/话术库.docx"));
        PolarisApp {
            model,
            blocks,
            undo_stack: Vec::new(),
            original: HashMap::new(),
            para_map: HashMap::new(),
            image_map: HashMap::new(),
            body_anchor: HashMap::new(),
            loaded_path: None,
            status: "就绪".to_string(),
            doc_path,
            new_counter: 0,
            focused_once: false,
            show_history: false,
            confirm_restore: None,
            pending_ai: None,
            ai_rx: None,
        }
    }

    /// 从 model 重建编辑缓冲（undo / 重载后调）。
    fn rebuild_blocks(&mut self) {
        self.blocks = document_blocks(&self.model);
    }

    /// 用某后端打开一个 docx：导入到新 model（覆盖当前文档），重建缓冲、清空撤销栈。
    /// 后端可注入——GUI 用 OfficeCLI 子进程，测试用 FakeBackend（不碰子进程）。
    fn load_from_backend<B: DocxBackend>(&mut self, backend: &B, path: &str) -> Result<(), String> {
        let blocks = backend.read_blocks(path)?;
        let mut model = ProseModel::new();
        let ids = import_blocks(&mut model, &blocks)?;
        // 节点 ↔ 段落身份对照表：写回靠它定位（表格等无身份的块自然不入表）。
        self.para_map = ids
            .iter()
            .zip(&blocks)
            .filter_map(|(id, b)| b.para_id().map(|p| (id.clone(), p.to_string())))
            .collect();
        // 节点 ↔ 真图字节：图片占位块的实际渲染数据（没有 → 渲染降级为文字占位）。
        self.image_map = ids
            .iter()
            .zip(&blocks)
            .filter_map(|(id, b)| match b {
                DocxBlock::Unstructured { image: Some(bytes), .. } => {
                    Some((id.clone(), Arc::from(bytes.as_slice())))
                }
                _ => None,
            })
            .collect();
        // 节点 ↔ after 锚（段落 + 表格都进）：新增段写回时据此推进锚点。
        self.body_anchor = ids
            .iter()
            .zip(&blocks)
            .filter_map(|(id, b)| b.body_anchor().map(|a| (id.clone(), a)))
            .collect();
        self.model = model;
        self.rebuild_blocks();
        self.undo_stack.clear();
        self.confirm_restore = None;
        self.loaded_path = Some(path.to_string());
        // 记下加载时各节点原文，保存时据此 diff + find/replace。
        self.original = self.blocks.iter().map(|b| (b.node.clone(), b.text.clone())).collect();
        Ok(())
    }

    /// 用 OfficeCLI 后端打开某路径并更新状态栏（文件选择器 / 路径栏都走这里）。
    fn open_path(&mut self, path: &str) {
        let backend = OfficeCliBackend::new();
        match self.load_from_backend(&backend, path) {
            Ok(()) => self.status = format!("已打开 {}（{} 块）", path, self.blocks.len()),
            Err(e) => self.status = format!("打开失败: {e}"),
        }
    }

    /// 删除某段：`RemoveNode` 原子提交（逆 op 含快照，可撤销）→ 重建视图。
    fn delete_block(&mut self, node: &NodeId) {
        let set = PatchSet::new(vec![Patch::authored(
            "del",
            Target::Node(node.clone()),
            Op::RemoveNode { id: node.clone() },
        )]);
        match commit(&mut self.model, &set) {
            Ok(inverse) => {
                self.undo_stack.push(inverse);
                self.rebuild_blocks();
                self.status = format!("已删除 {}（撤销可恢复）", node.0);
            }
            Err(e) => self.status = format!("删除失败: {e:?}"),
        }
    }

    /// 在 root 第 `index` 个孩子位插入新段落（`InsertSection` 原子提交，可撤销）→ 重建视图。
    fn insert_paragraph(&mut self, index: usize) {
        let root = self.model.root().clone();
        let id = NodeId(format!("new:{}", self.new_counter));
        self.new_counter += 1;
        let set = PatchSet::new(vec![Patch::authored(
            "add",
            Target::Node(root.clone()),
            Op::InsertSection { parent: root, index, id, text: "新段落".to_string() },
        )]);
        match commit(&mut self.model, &set) {
            Ok(inverse) => {
                self.undo_stack.push(inverse);
                self.rebuild_blocks();
                self.status = "已加段落（撤销可移除）".to_string();
            }
            Err(e) => self.status = format!("加段落失败: {e:?}"),
        }
    }

    /// 末尾追加一段。
    fn add_paragraph(&mut self) {
        let root = self.model.root().clone();
        let n = self.model.children(&root).map(|c| c.len()).unwrap_or(0);
        self.insert_paragraph(n);
    }

    /// 这个块可否删除：普通段/标题永远可删（纯 model 操作，可撤销）；
    /// Opaque 只有带身份的（图片段）可删——无身份的（表格）删了也写不回，UI 不给按钮。
    fn deletable(&self, b: &ViewBlock) -> bool {
        b.kind != NodeKind::Opaque || self.para_map.contains_key(&b.node)
    }

    /// 对某段发起 AI 改写：把原文交给后台线程跑真模型（claude 子进程，~10s），结果经 channel 回来。
    /// **不碰 model**；一次只允许一个在途请求。
    fn start_ai_request(&mut self, node: &NodeId) {
        if self.ai_rx.is_some() {
            self.status = "已有一个 AI 请求在路上，等它回来".to_string();
            return;
        }
        let Some(old) = self.model.get_text(node).map(|s| s.to_string()) else {
            self.status = format!("{} 不存在，无法发起 AI 提案", node.0);
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.ai_rx = Some(rx);
        self.status = format!("AI 改写中…（{}，约十秒）", node.0);
        let node = node.clone();
        std::thread::spawn(move || {
            let result = claude_prompt(&ai_rewrite_prompt(&old));
            let _ = tx.send(AiReply { request: AiRequest::Single { node, old }, result });
        });
    }

    /// 全文清理的目标段：全部可编辑块（Opaque 排除），超上限截断（先妥协，明示）。
    fn clean_targets(&self) -> (Vec<(NodeId, String)>, usize) {
        const MAX_SEGMENTS: usize = 100;
        let all: Vec<(NodeId, String)> = document_blocks(&self.model)
            .into_iter()
            .filter(|b| b.kind != NodeKind::Opaque)
            .map(|b| (b.node, b.text))
            .collect();
        let truncated = all.len().saturating_sub(MAX_SEGMENTS);
        (all.into_iter().take(MAX_SEGMENTS).collect(), truncated)
    }

    /// 「AI 清理全文」：全部可编辑段打包给真模型，要求只回需要改的段（JSON）。后台线程同单段。
    fn start_ai_clean_all(&mut self) {
        if self.ai_rx.is_some() {
            self.status = "已有一个 AI 请求在路上，等它回来".to_string();
            return;
        }
        let (olds, truncated) = self.clean_targets();
        if olds.is_empty() {
            self.status = "没有可清理的段落".to_string();
            return;
        }
        let warn = if truncated > 0 { format!("（超长截断，{truncated} 段未送审）") } else { String::new() };
        self.status = format!("AI 清理全文中…（{} 段，可能要一两分钟）{warn}", olds.len());
        let (tx, rx) = mpsc::channel();
        self.ai_rx = Some(rx);
        std::thread::spawn(move || {
            let result = claude_prompt(&ai_clean_prompt(&olds));
            let _ = tx.send(AiReply { request: AiRequest::CleanAll { olds }, result });
        });
    }

    /// 把一条改动并进待 review 提案（同节点替换旧条目），返回提案当前总条数。
    fn merge_change(&mut self, change: AiChange) -> usize {
        let proposal = self.pending_ai.get_or_insert_with(|| AiProposal { changes: Vec::new() });
        proposal.changes.retain(|c| c.node != change.node);
        proposal.changes.push(change);
        proposal.changes.len()
    }

    /// 收 AI 结果（解析全在这里的纯函数链上做）：有实质改动 → 并进待 review 提案；
    /// 无改动/出错/段落已被人改过 → 只报状态。
    fn receive_ai_result(&mut self, reply: AiReply) {
        let raw = match reply.result {
            Err(e) => {
                self.status = format!("AI 调用失败: {e}");
                return;
            }
            Ok(raw) => raw,
        };
        match reply.request {
            AiRequest::Single { node, old } => {
                let new = parse_ai_reply(&raw);
                if new.is_empty() {
                    self.status = "模型返回为空".to_string();
                } else if self.model.get_text(&node) != Some(old.as_str()) {
                    self.status = format!("{} 在 AI 思考期间被改过，该条作废", node.0);
                } else if new == old {
                    self.status = format!("AI 对 {} 无改动建议", node.0);
                } else {
                    let n = self.merge_change(AiChange { node, old, new, selected: true });
                    self.status = format!("AI 提案待 review：共 {n} 条（在对照面板勾选接受/拒绝）");
                }
            }
            AiRequest::CleanAll { olds } => match parse_clean_reply(&raw, &olds) {
                Err(e) => self.status = format!("AI 清理回复解析失败: {e}"),
                Ok((changes, dropped)) if changes.is_empty() => {
                    self.status = format!("AI 认为全文无需清理（丢弃无效项 {dropped} 条）");
                }
                Ok((changes, dropped)) => {
                    let mut n = 0;
                    for c in changes {
                        n = self.merge_change(c);
                    }
                    let warn = if dropped > 0 { format!("；丢弃无效项 {dropped} 条") } else { String::new() };
                    self.status = format!("AI 清理建议待 review：共 {n} 条{warn}");
                }
            },
        }
    }

    /// 某条提案是否仍有效：段落当前文本必须仍等于提案时的旧文（被人改过 → 失效）。
    fn change_valid(&self, c: &AiChange) -> bool {
        self.model.get_text(&c.node) == Some(c.old.as_str())
    }

    /// 接受勾选项：有效且勾选的改动合成**一个 AI 来源 PatchSet** → `approve_ai`（review 放行）→
    /// **一次原子 commit = 一个撤销单位**。失效/未勾的不进组；组空则不提交。
    fn accept_ai(&mut self) {
        let Some(p) = self.pending_ai.take() else { return };
        let total = p.changes.len();
        let source_map = render_html(&self.model).source_map;
        let mut patches = Vec::new();
        let mut stale = 0usize;
        for (i, c) in p.changes.iter().enumerate() {
            if !c.selected {
                continue;
            }
            if !self.change_valid(c) {
                stale += 1;
                continue;
            }
            let Some(span) = source_map.span_for(&c.node) else {
                stale += 1;
                continue;
            };
            patches.push(Patch::ai(
                &format!("ai-{i}"),
                Target::Node(c.node.clone()),
                Op::SetSpan { range: span.range, text: c.new.clone() },
            ));
        }
        if patches.is_empty() {
            self.status = format!("没有可提交的 AI 改动（共 {total} 条，失效 {stale} 条），未提交");
            return;
        }
        let n = patches.len();
        // review 动作：用户刚在对照面板勾选并点了「接受」——没有这步 commit 会拒（core 强制）
        let set = PatchSet::new(patches).approve_ai();
        match commit(&mut self.model, &set) {
            Ok(inverse) => {
                self.undo_stack.push(inverse);
                self.rebuild_blocks();
                let warn = if stale > 0 { format!("；{stale} 条已失效跳过") } else { String::new() };
                self.status = format!("已接受 {n} 条 AI 改动（一次撤销可整组回）{warn}");
            }
            Err(e) => self.status = format!("AI 提案提交失败: {e:?}"),
        }
    }

    /// 全部拒绝：整个提案丢弃，model 一字未动。
    fn reject_ai(&mut self) {
        if let Some(p) = self.pending_ai.take() {
            self.status = format!("已拒绝全部 {} 条 AI 提案，模型未动", p.changes.len());
        }
    }

    /// 在某段下方插入一段。
    fn add_paragraph_after(&mut self, node: &NodeId) {
        let root = self.model.root().clone();
        let children: Vec<NodeId> = self.model.children(&root).map(|c| c.to_vec()).unwrap_or_default();
        let index = children.iter().position(|n| n == node).map(|p| p + 1).unwrap_or(children.len());
        self.insert_paragraph(index);
    }

    /// 收集要写回 docx 的操作：改文本→set、删段→remove、新增→add（锚定前一个原段之后；
    /// 同锚点连续多段逆序插入以保序）。定位一律查 `para_map`（稳定 `@paraId` 身份）——
    /// **不按位置数**，表格等无身份块挡在中间也不会打错段。
    /// 返回 `(ops, skipped)`：skipped = 改了但写不回的块数（无 paraId），调用方必须呈现，不静默。
    fn pending_ops(&self) -> (Vec<DocxOp>, usize) {
        let path_of = |node: &NodeId| -> Option<String> {
            self.para_map.get(node).map(|pid| format!("/body/p[@paraId={pid}]"))
        };
        // after 锚查 body_anchor：段落 → p[@paraId]，表格 → tbl[N]，故表格也推进锚点。
        let after_of = |node: &NodeId| -> Option<String> { self.body_anchor.get(node).cloned() };

        // 以 **model** 为准（真理之源），不读 UI 缓冲——与 has_unsaved_changes 同一纪律。
        let blocks = document_blocks(&self.model);
        let mut ops = Vec::new();
        let mut skipped = 0usize;

        // 1) 改了文本的原段 → set（无身份 → 计入 skipped，绝不猜位置）
        for b in &blocks {
            if let Some(old) = self.original.get(&b.node) {
                if &b.text != old {
                    match path_of(&b.node) {
                        Some(path) => ops.push(DocxOp::Set { path, find: old.clone(), replace: b.text.clone() }),
                        None => skipped += 1,
                    }
                }
            }
        }

        // 2) 删掉的原段 → remove（paraId 稳定，顺序无所谓）
        let current: HashSet<&NodeId> = blocks.iter().map(|b| &b.node).collect();
        for node in self.original.keys() {
            if !current.contains(node) {
                match path_of(node) {
                    Some(path) => ops.push(DocxOp::Remove { path }),
                    None => skipped += 1,
                }
            }
        }

        // 3) 新增段 → add。锚点 = 前一个有 after 锚的原块（段落 p[@paraId] 或表格 tbl[N]）。
        //    表格也推进锚点，故紧跟表格后新增的段锚到表格**后**（tbl[N] 位置式：同批次若删了
        //    表格前的段会位移，属已知妥协——常见的「表格后加段」单独保存不受影响）。
        let mut anchor: Option<String> = None; // "p[@paraId=X]"；None=追加末尾
        let mut run: Vec<String> = Vec::new();
        for b in &blocks {
            if b.node.0.starts_with("new:") {
                if !b.text.is_empty() {
                    run.push(b.text.clone());
                }
            } else {
                for text in run.drain(..).rev() {
                    ops.push(DocxOp::Add { after: anchor.clone(), text });
                }
                if let Some(a) = after_of(&b.node) {
                    anchor = Some(a);
                }
            }
        }
        for text in run.drain(..).rev() {
            ops.push(DocxOp::Add { after: anchor.clone(), text });
        }

        (ops, skipped)
    }

    /// 保存：把改动**就地写回打开的那个 docx**。
    /// 注意别叫 `save`——会撞 `eframe::App::save`（状态持久化）。
    fn save_to_docx(&mut self) -> Result<String, String> {
        let file = self.loaded_path.clone().ok_or_else(|| "先「打开 docx」再保存".to_string())?;
        let (ops, skipped) = self.pending_ops();
        // 写不回的改动（表格占位块/无 paraId 的文件）大声报出来，绝不静默吞。
        let warn =
            if skipped > 0 { format!("；另有 {skipped} 处改动无法写回（未结构化块/缺 paraId），已跳过") } else { String::new() };
        if ops.is_empty() {
            return Ok(if skipped > 0 { format!("没有可写回的改动{warn}") } else { "无改动可保存".to_string() });
        }
        // 写盘是不可逆的外部 effect：落盘前先把当前文件快照进版本历史，任何旧状态都可回。
        // 按内容去重：当前状态已在历史（如刚回过版还没改）就不重复留。
        history::snapshot_if_new(Path::new(&file))?;
        let n = ops.len();
        let structural = ops.iter().any(|o| !matches!(o, DocxOp::Set { .. }));
        OfficeCliBackend::new().save_ops(&file, &ops)?;
        if structural {
            // 结构变了（增/删段）→ 重载，让节点 id 与新 docx 段落重新对齐（撤销历史会清空）。
            self.load_from_backend(&OfficeCliBackend::new(), &file)?;
        } else {
            // 仅文本改动 → 刷新原文快照即可（保住撤销历史、不重载）。
            self.original = self.blocks.iter().map(|b| (b.node.clone(), b.text.clone())).collect();
        }
        Ok(format!("已保存 {n} 处改动 → {file}（旧版已入历史）{warn}"))
    }

    /// 有没有「已进 model 但还没保存进 docx」的改动。
    /// 以 **model** 为准（真理之源），不读 UI 缓冲：当前 model 状态 vs 打开时快照，逐节点比对。
    fn has_unsaved_changes(&self) -> bool {
        let current: HashMap<NodeId, String> =
            document_blocks(&self.model).into_iter().map(|b| (b.node, b.text)).collect();
        current != self.original
    }

    /// 回版守门：有未保存改动时第一次点先拦下警告，对**同一个**版本再点一次才放行。
    fn approve_restore(&mut self, version: &Path) -> bool {
        if self.has_unsaved_changes() && self.confirm_restore.as_deref() != Some(version) {
            self.confirm_restore = Some(version.to_path_buf());
            self.status = "有未保存改动，回版会丢弃它们——再点一次该版本确认。".to_string();
            return false;
        }
        self.confirm_restore = None;
        true
    }

    /// 回到某历史版本：`history::restore` 覆盖前把未入历史的当前状态留底（已入历史则不重复），
    /// 然后从文件重载 model（节点与段落重新对齐，撤销栈清空）。返回是否留了底。
    fn restore_version<B: DocxBackend>(&mut self, backend: &B, version: &Path) -> Result<bool, String> {
        let file = self.loaded_path.clone().ok_or_else(|| "没有打开的文件".to_string())?;
        let snapped = history::restore(Path::new(&file), version)?;
        self.load_from_backend(backend, &file)?;
        Ok(snapped.is_some())
    }

    /// 把某节点缓冲文本回写 model：经 SourceMap 反查 → SetSpan → 原子提交，记录逆 op。
    /// 文本无变化则跳过（不产生空 commit / 多余 undo）。
    fn commit_edit(&mut self, node: &NodeId, new_text: &str) -> Result<(), EditError> {
        if self.model.get_text(node) == Some(new_text) {
            return Ok(());
        }
        let source_map = render_html(&self.model).source_map;
        let inverse = apply_fragment_edit(&mut self.model, &source_map, node, new_text)?;
        self.undo_stack.push(inverse);
        self.status = format!("已回写 {}（撤销栈 {}）", node.0, self.undo_stack.len());
        Ok(())
    }

    /// 撤销最近一次编辑：弹栈 → 应用逆 PatchSet → 重建缓冲。
    fn undo_last(&mut self) {
        if let Some(inverse) = self.undo_stack.pop() {
            let _ = editor_core::undo(&mut self.model, &inverse);
            self.rebuild_blocks();
            self.status = format!("已撤销（撤销栈 {}）", self.undo_stack.len());
        } else {
            self.status = "没有可撤销的编辑".to_string();
        }
    }
}

impl eframe::App for PolarisApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.focused_once {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.focused_once = true;
        }

        // 收在途的 AI 结果（后台线程 → channel）。没人动鼠标也要醒着收货：定时请求重绘。
        if let Some(rx) = &self.ai_rx {
            match rx.try_recv() {
                Ok(reply) => {
                    self.ai_rx = None;
                    self.receive_ai_result(reply);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(200));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.ai_rx = None;
                    self.status = "AI 线程异常退出".to_string();
                }
            }
        }

        let mut do_undo = false;
        let mut do_open: Option<String> = None;
        let mut do_save = false;
        let mut do_pick = false;
        let mut do_ai_clean = false;
        egui::TopBottomPanel::top("bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("📂 打开…").clicked() {
                    do_pick = true;
                }
                if ui.button("💾 保存").clicked() {
                    do_save = true;
                }
                if ui.button("↩ 撤销").clicked() {
                    do_undo = true;
                }
                if ui.button("🕘 历史").clicked() {
                    self.show_history = !self.show_history;
                }
                if ui.button("🤖 清理全文").clicked() {
                    do_ai_clean = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("路径:");
                ui.add(egui::TextEdit::singleline(&mut self.doc_path).desired_width(380.0));
                if ui.button("打开此路径").clicked() {
                    do_open = Some(self.doc_path.clone());
                }
            });
            ui.label(self.status.as_str());
        });

        // 版本历史侧栏（SidePanel 必须加在 CentralPanel 之前）。点「回此版」记下来，面板外统一处理。
        let mut clicked_version: Option<(PathBuf, u64)> = None;
        if self.show_history {
            egui::SidePanel::right("history").default_width(250.0).show(ctx, |ui| {
                ui.heading("版本历史");
                match &self.loaded_path {
                    None => {
                        ui.label("先打开一个 docx；保存时会自动留底。");
                    }
                    Some(file) => match history::list_versions(Path::new(file)) {
                        Ok(versions) if versions.is_empty() => {
                            ui.label("还没有历史版本：保存一次就有了。");
                        }
                        Ok(versions) => {
                            ui.label(format!("{} 个版本，新→旧。未入历史的当前状态会先留底。", versions.len()));
                            ui.separator();
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                for v in &versions {
                                    ui.horizontal(|ui| {
                                        if ui.button("回此版").clicked() {
                                            clicked_version = Some((v.path.clone(), v.millis));
                                        }
                                        ui.label(format_millis(v.millis));
                                    });
                                }
                            });
                        }
                        Err(e) => {
                            ui.label(format!("读历史失败: {e}"));
                        }
                    },
                }
            });
        }

        // 本帧内收集失焦的编辑（避免借用 self.blocks 时又借 self）。
        // 删除门先算好（循环里可变借用 blocks，不能再借 self）。
        let deletable: HashSet<NodeId> =
            self.blocks.iter().filter(|b| self.deletable(b)).map(|b| b.node.clone()).collect();
        let mut pending: Option<(NodeId, String)> = None;
        let mut pending_delete: Option<NodeId> = None;
        let mut pending_add_after: Option<NodeId> = None;
        let mut pending_ai_request: Option<NodeId> = None;
        let mut do_add = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                for block in &mut self.blocks {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            if deletable.contains(&block.node) && ui.button("x").clicked() {
                                pending_delete = Some(block.node.clone());
                            }
                            if ui.button("+").clicked() {
                                pending_add_after = Some(block.node.clone());
                            }
                            // AI 改写提案：只对可编辑块（Opaque 在 core 层就无 span，不给按钮）。
                            if block.kind != NodeKind::Opaque && ui.button("AI").clicked() {
                                pending_ai_request = Some(block.node.clone());
                            }
                        });
                        let resp = match block.kind {
                            NodeKind::Heading { level } => {
                                let size = match level {
                                    1 => 26.0,
                                    2 => 22.0,
                                    3 => 18.0,
                                    _ => 16.0,
                                };
                                ui.add(
                                    egui::TextEdit::singleline(&mut block.text)
                                        .font(egui::FontId::proportional(size))
                                        .desired_width(f32::INFINITY),
                                )
                            }
                            // Opaque 只读：有真图字节给真图，没有降级为文字占位（含图混排标记走这条）。
                            // 都不可输入、没有失焦提交路径（core 层也会拒：NoSpan）。
                            NodeKind::Opaque => match self.image_map.get(&block.node) {
                                Some(bytes) => ui.add(
                                    egui::Image::from_bytes(
                                        format!("bytes://{}", block.node.0),
                                        egui::load::Bytes::Shared(bytes.clone()),
                                    )
                                    // 行内默认 fit 会被行高（按钮列高度）压扁成缩略图；
                                    // 指定「宽度盒 + 高度不限」→ 等比放到正常宽度（实测验证）。
                                    .fit_to_exact_size(egui::vec2(
                                        ui.available_width().min(560.0),
                                        f32::INFINITY,
                                    )),
                                ),
                                None => ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(block.text.as_str()).weak().italics(),
                                    )
                                    .wrap(),
                                ),
                            },
                            NodeKind::Paragraph => ui.add(
                                egui::TextEdit::multiline(&mut block.text).desired_width(f32::INFINITY),
                            ),
                        };
                        if resp.lost_focus() {
                            pending = Some((block.node.clone(), block.text.clone()));
                        }
                    });
                    ui.add_space(6.0);
                }
                if ui.button("+ 末尾加段落").clicked() {
                    do_add = true;
                }
            });
        });

        // AI review 对照窗：有待审提案时浮出，逐条勾选。决策收集后在面板外统一处理（借用纪律）。
        let mut ai_decision: Option<bool> = None;
        // 失效标记先算好（闭包里 self.pending_ai 是可变借用，不能再调 &self 方法）。
        let change_validity: Vec<bool> = self
            .pending_ai
            .as_ref()
            .map(|p| {
                p.changes
                    .iter()
                    .map(|c| self.model.get_text(&c.node) == Some(c.old.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(p) = &mut self.pending_ai {
            // 右上锚定：让开左侧的 x/+/AI 操作列（真机验证发现默认位置会盖住按钮）。
            egui::Window::new("AI 改写提案（需 review）")
                .collapsible(false)
                .default_width(460.0)
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 64.0))
                .show(ctx, |ui| {
                    ui.label(format!("{} 条改动建议，勾选要接受的：", p.changes.len()));
                    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                        for (i, c) in p.changes.iter_mut().enumerate() {
                            ui.separator();
                            if change_validity.get(i).copied().unwrap_or(false) {
                                ui.checkbox(&mut c.selected, c.node.0.as_str());
                            } else {
                                c.selected = false;
                                ui.label(
                                    egui::RichText::new(format!("{}（已被人改过，失效）", c.node.0))
                                        .weak()
                                        .strikethrough(),
                                );
                            }
                            // 字符级 diff 着色：红删绿增，一眼看出改了哪几个字。
                            show_diff(ui, &char_diff(&c.old, &c.new));
                        }
                    });
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("✓ 接受勾选项").clicked() {
                            ai_decision = Some(true);
                        }
                        if ui.button("✗ 全部拒绝").clicked() {
                            ai_decision = Some(false);
                        }
                    });
                });
        }

        if let Some((node, text)) = pending {
            let _ = self.commit_edit(&node, &text);
        }
        if let Some(node) = pending_delete {
            self.delete_block(&node);
        }
        if let Some(node) = pending_ai_request {
            self.start_ai_request(&node);
        }
        if do_ai_clean {
            self.start_ai_clean_all();
        }
        match ai_decision {
            Some(true) => self.accept_ai(),
            Some(false) => self.reject_ai(),
            None => {}
        }
        if let Some(node) = pending_add_after {
            self.add_paragraph_after(&node);
        }
        if do_add {
            self.add_paragraph();
        }
        if do_undo {
            self.undo_last();
        }
        if let Some((version, millis)) = clicked_version {
            if self.approve_restore(&version) {
                self.status = match self.restore_version(&OfficeCliBackend::new(), &version) {
                    Ok(true) => format!("已回到 {} 的版本（回版前状态已留底）", format_millis(millis)),
                    Ok(false) => format!("已回到 {} 的版本（当前状态已在历史中，未重复留底）", format_millis(millis)),
                    Err(e) => format!("回版失败: {e}"),
                };
            }
        }
        if do_pick {
            // 弹系统选文件框（modal）；选中即载入。同步跑子进程，载入瞬间会短暂卡顿。
            if let Some(picked) = rfd::FileDialog::new().add_filter("Word 文档", &["docx"]).pick_file() {
                let p = picked.display().to_string();
                self.doc_path = p.clone();
                self.open_path(&p);
            } else {
                self.status = "已取消选择".to_string();
            }
        }
        if let Some(path) = do_open {
            self.open_path(&path);
        }
        if do_save {
            self.status = match self.save_to_docx() {
                Ok(s) => s,
                Err(e) => format!("保存失败: {e}"),
            };
        }
    }
}

/// epoch 毫秒 → 本地时间字符串（版本列表显示用）。
fn format_millis(millis: u64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(millis as i64) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => millis.to_string(),
    }
}

/// 加载系统 CJK 字体（都是 .ttc，egui 取 index 0），否则中文会显示为方块。
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "/System/Library/Fonts/Supplemental/Songti.ttc",
        "/System/Library/Fonts/Supplemental/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/Supplemental/STHeiti Light.ttc",
        "/System/Library/Fonts/PingFang.ttc",
    ];
    let Some(bytes) = CANDIDATES.iter().find_map(|p| std::fs::read(p).ok()) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("cjk".to_owned(), egui::FontData::from_owned(bytes).into());
    fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "cjk".to_owned());
    fonts.families.entry(egui::FontFamily::Monospace).or_default().push("cjk".to_owned());
    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Polaris")
            .with_inner_size([760.0, 580.0])
            .with_active(true),
        ..Default::default()
    };
    eframe::run_native(
        "Polaris",
        opts,
        Box::new(|cc| {
            install_cjk_font(&cc.egui_ctx);
            // 真图显示：注册 egui 官方图片 loader（PNG/JPEG 解码走 image crate）。
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(PolarisApp::new()))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_docx::{DocxBlock, FakeBackend};

    #[test]
    fn open_docx_via_fake_backend_loads_into_model() {
        let mut app = PolarisApp::new();
        let backend = FakeBackend::new(vec![
            DocxBlock::Heading { level: 1, text: "标题".to_string(), para_id: None },
            DocxBlock::Paragraph { text: "正文".to_string(), para_id: None },
        ]);
        app.load_from_backend(&backend, "ignored.docx").unwrap();
        // 样例被 docx 内容覆盖；撤销栈清空
        assert_eq!(app.blocks.len(), 2);
        assert_eq!(app.blocks[0].kind, NodeKind::Heading { level: 1 });
        assert_eq!(app.blocks[0].text, "标题");
        assert!(app.undo_stack.is_empty());
        // 打开后还能编辑，且改标题文本保持 heading（typed）
        let node = app.blocks[0].node.clone();
        app.commit_edit(&node, "改过的标题").unwrap();
        assert_eq!(app.model.node_kind(&node), Some(NodeKind::Heading { level: 1 }));
    }

    #[test]
    fn pending_ops_covers_set_remove_add() {
        let mut app = PolarisApp::new();
        let backend = FakeBackend::new(vec![
            DocxBlock::Heading { level: 1, text: "标题".to_string(), para_id: Some("AA".to_string()) }, // docx:0
            DocxBlock::Paragraph { text: "甲".to_string(), para_id: Some("BB".to_string()) },           // docx:1
            DocxBlock::Paragraph { text: "乙".to_string(), para_id: Some("CC".to_string()) },           // docx:2
        ]);
        app.load_from_backend(&backend, "x.docx").unwrap();
        assert!(app.pending_ops().0.is_empty()); // 刚加载，无改动

        app.commit_edit(&NodeId("docx:0".to_string()), "新标题").unwrap(); // 改文本（进 model）
        app.delete_block(&NodeId("docx:1".to_string())); // 删 docx:1
        app.add_paragraph(); // 末尾加段（锚定到最后一个原段 docx:2 = CC）

        let (ops, skipped) = app.pending_ops();
        assert_eq!(skipped, 0);
        assert!(ops.iter().any(|o| matches!(o, DocxOp::Set { path, replace, .. }
            if path == "/body/p[@paraId=AA]" && replace == "新标题")));
        assert!(ops.iter().any(|o| matches!(o, DocxOp::Remove { path } if path == "/body/p[@paraId=BB]")));
        assert!(ops.iter().any(|o| matches!(o, DocxOp::Add { after, text }
            if after.as_deref() == Some("p[@paraId=CC]") && text == "新段落")));
    }

    #[test]
    fn pending_ops_targets_by_identity_with_table_between() {
        // 回归：表格（无 paraId 的 Unstructured）夹在段落中间。
        // 旧实现按位置数（docx:i → para_ids[i]），表格后所有写回目标右移一位、全部打错段；
        // 新实现按身份查表，必须精确命中 CC。
        let mut app = PolarisApp::new();
        let backend = FakeBackend::new(vec![
            DocxBlock::Paragraph { text: "前段".to_string(), para_id: Some("AA".to_string()) }, // docx:0
            DocxBlock::Unstructured { kind: "table".to_string(), raw: "[Table: 1 rows]".to_string(), para_id: None, image: None, anchor: None }, // docx:1
            DocxBlock::Paragraph { text: "后段".to_string(), para_id: Some("CC".to_string()) }, // docx:2
        ]);
        app.load_from_backend(&backend, "x.docx").unwrap();

        app.commit_edit(&NodeId("docx:2".to_string()), "后段改").unwrap();
        let (ops, skipped) = app.pending_ops();
        assert_eq!(skipped, 0);
        assert_eq!(
            ops,
            vec![DocxOp::Set {
                path: "/body/p[@paraId=CC]".to_string(),
                find: "后段".to_string(),
                replace: "后段改".to_string(),
            }]
        );
    }

    #[test]
    fn new_paragraph_after_table_anchors_to_table_not_before_it() {
        // Step 22（①）：表格带 tbl[N] 锚，新段落紧跟其后 → 锚到 tbl[1]，不退到表格前的段。
        let mut app = PolarisApp::new();
        let backend = FakeBackend::new(vec![
            DocxBlock::Paragraph { text: "前段".to_string(), para_id: Some("AA".to_string()) }, // docx:0
            DocxBlock::Unstructured {
                kind: "table".to_string(),
                raw: "[Table]".to_string(),
                para_id: None,
                image: None,
                anchor: Some("tbl[1]".to_string()),
            }, // docx:1（表格，after 锚 = tbl[1]）
        ]);
        app.load_from_backend(&backend, "x.docx").unwrap();
        app.add_paragraph(); // 文档末尾（= 表格后）加新段

        let (ops, skipped) = app.pending_ops();
        assert_eq!(skipped, 0);
        assert_eq!(
            ops,
            vec![DocxOp::Add { after: Some("tbl[1]".to_string()), text: "新段落".to_string() }]
        );
    }

    #[test]
    fn new_paragraph_after_plain_paragraph_still_anchors_to_it() {
        // 无表格时不回归：新段落锚到前一个段落的 p[@paraId]。
        let mut app = PolarisApp::new();
        let backend = FakeBackend::new(vec![DocxBlock::Paragraph {
            text: "唯一段".to_string(),
            para_id: Some("AA".to_string()),
        }]);
        app.load_from_backend(&backend, "x.docx").unwrap();
        app.add_paragraph();
        let (ops, _) = app.pending_ops();
        assert_eq!(
            ops,
            vec![DocxOp::Add { after: Some("p[@paraId=AA]".to_string()), text: "新段落".to_string() }]
        );
    }

    #[test]
    fn opaque_blocks_are_readonly_and_delete_gated() {
        let mut app = PolarisApp::new();
        let backend = FakeBackend::new(vec![
            DocxBlock::Paragraph { text: "前段".to_string(), para_id: Some("AA".to_string()) }, // docx:0
            DocxBlock::Unstructured { kind: "table".to_string(), raw: "[T]".to_string(), para_id: None, image: None, anchor: None }, // docx:1
            DocxBlock::Unstructured {
                kind: "image".to_string(),
                raw: "alt=\"图\"".to_string(),
                para_id: Some("IMG".to_string()),
                image: None,
                anchor: None,
            }, // docx:2
        ]);
        app.load_from_backend(&backend, "x.docx").unwrap();

        // typed：占位块是 Opaque，文本=原始描述（不再有「[未结构化:…]」前缀 hack）
        assert_eq!(app.blocks[1].kind, NodeKind::Opaque);
        assert_eq!(app.blocks[1].text, "[T]");
        assert_eq!(app.blocks[2].kind, NodeKind::Opaque);

        // 只读是 core 层强制：试图改 → NoSpan 报错，模型一字未动
        assert!(app.commit_edit(&NodeId("docx:1".to_string()), "乱改").is_err());
        assert_eq!(app.model.get_text(&NodeId("docx:1".to_string())), Some("[T]"));

        // 删除门：普通段可删；表格（无身份）不可删；图片（有身份）可删
        let blocks = app.blocks.clone();
        assert!(app.deletable(&blocks[0]));
        assert!(!app.deletable(&blocks[1]));
        assert!(app.deletable(&blocks[2]));
    }

    #[test]
    fn deleting_image_opaque_writes_back_by_identity() {
        let mut app = PolarisApp::new();
        let backend = FakeBackend::new(vec![
            DocxBlock::Paragraph { text: "前段".to_string(), para_id: Some("AA".to_string()) },
            DocxBlock::Unstructured {
                kind: "image".to_string(),
                raw: "alt=\"图\"".to_string(),
                para_id: Some("IMG".to_string()),
                image: None,
                anchor: None,
            },
        ]);
        app.load_from_backend(&backend, "x.docx").unwrap();
        app.delete_block(&NodeId("docx:1".to_string()));
        let (ops, skipped) = app.pending_ops();
        assert_eq!(skipped, 0);
        assert_eq!(ops, vec![DocxOp::Remove { path: "/body/p[@paraId=IMG]".to_string() }]);
    }

    #[test]
    fn image_map_built_from_blocks_with_bytes_and_degrades_without() {
        let mut app = PolarisApp::new();
        let backend = FakeBackend::new(vec![
            DocxBlock::Paragraph { text: "前".to_string(), para_id: Some("AA".to_string()) }, // docx:0
            DocxBlock::Unstructured {
                kind: "image".to_string(),
                raw: "alt=\"图\"".to_string(),
                para_id: Some("IMG".to_string()),
                image: Some(b"PNGBYTES".to_vec()), // 有真图字节 → 进 image_map
                anchor: None,
            }, // docx:1
            DocxBlock::Unstructured {
                kind: "image".to_string(),
                raw: "alt=\"没抽到\"".to_string(),
                para_id: Some("MISS".to_string()),
                image: None, // 没字节 → 降级文字占位（不进 map，但块仍在）
                anchor: None,
            }, // docx:2
        ]);
        app.load_from_backend(&backend, "x.docx").unwrap();

        let img = NodeId("docx:1".to_string());
        let miss = NodeId("docx:2".to_string());
        assert_eq!(app.image_map.get(&img).map(|b| b.as_ref()), Some(b"PNGBYTES".as_ref()));
        assert!(!app.image_map.contains_key(&miss));
        // 降级块仍可见（blocks 里有、文本是占位描述）、仍可删（有身份）
        assert_eq!(app.blocks[2].text, "alt=\"没抽到\"");
        let blocks = app.blocks.clone();
        assert!(app.deletable(&blocks[1]));
        assert!(app.deletable(&blocks[2]));
        // 换文档后 image_map 重建，不残留
        app.load_from_backend(
            &FakeBackend::new(vec![DocxBlock::Paragraph { text: "x".to_string(), para_id: None }]),
            "y.docx",
        )
        .unwrap();
        assert!(app.image_map.is_empty());
    }

    // ── Step 17/18：AI patch 回路（真模型在后台线程，测试用伪造的 AiReply 驱动收货口，
    //    不碰 claude 子进程；review→commit→undo 通道与 Step 17 完全相同——「通道一行不改」的验收）──

    /// 测试小工具：装一篇多段文档，返回各段节点 id。
    fn app_with_paragraphs(texts: &[&str]) -> (PolarisApp, Vec<NodeId>) {
        let mut app = PolarisApp::new();
        let blocks = texts
            .iter()
            .enumerate()
            .map(|(i, t)| DocxBlock::Paragraph { text: t.to_string(), para_id: Some(format!("P{i}")) })
            .collect();
        app.load_from_backend(&FakeBackend::new(blocks), "x.docx").unwrap();
        let ids = (0..texts.len()).map(|i| NodeId(format!("docx:{i}"))).collect();
        (app, ids)
    }

    /// 测试小工具：装一篇单段文档。
    fn app_with_paragraph(text: &str) -> (PolarisApp, NodeId) {
        let (app, ids) = app_with_paragraphs(&[text]);
        (app, ids.into_iter().next().unwrap())
    }

    /// 测试小工具：伪造一条成功的单段 AI 回包。
    fn ok_reply(node: &NodeId, old: &str, new: &str) -> AiReply {
        AiReply {
            request: AiRequest::Single { node: node.clone(), old: old.to_string() },
            result: Ok(new.to_string()),
        }
    }

    // ── Step 21：字符级 diff（review 对照着色）──

    #[test]
    fn char_diff_pure_insert_and_delete() {
        assert_eq!(char_diff("ab", "abc"), vec![DiffPart::Equal("ab".into()), DiffPart::Insert("c".into())]);
        assert_eq!(char_diff("abc", "ab"), vec![DiffPart::Equal("ab".into()), DiffPart::Delete("c".into())]);
    }

    #[test]
    fn char_diff_middle_replace_merges_runs() {
        // 中段替换：X→Y，前后相等段保留并合并成串
        assert_eq!(
            char_diff("abXcd", "abYcd"),
            vec![
                DiffPart::Equal("ab".into()),
                DiffPart::Delete("X".into()),
                DiffPart::Insert("Y".into()),
                DiffPart::Equal("cd".into()),
            ]
        );
    }

    #[test]
    fn char_diff_cjk_tail_deletion() {
        // 真机同形：删尾部杂质 / 删多余称谓，相等前缀整段保留
        assert_eq!(
            char_diff("一、开场与破冰11113232", "一、开场与破冰"),
            vec![DiffPart::Equal("一、开场与破冰".into()), DiffPart::Delete("11113232".into())]
        );
        assert_eq!(
            char_diff("财兔的Ryan小汪老师", "财兔的Ryan"),
            vec![DiffPart::Equal("财兔的Ryan".into()), DiffPart::Delete("小汪老师".into())]
        );
    }

    #[test]
    fn char_diff_edges_empty_and_identical() {
        assert_eq!(char_diff("同", "同"), vec![DiffPart::Equal("同".into())]);
        assert_eq!(char_diff("", "新"), vec![DiffPart::Insert("新".into())]);
        assert_eq!(char_diff("旧", ""), vec![DiffPart::Delete("旧".into())]);
        assert!(char_diff("", "").is_empty());
        // 全不同 → 全删 + 全插（删优先）
        assert_eq!(char_diff("猫", "狗"), vec![DiffPart::Delete("猫".into()), DiffPart::Insert("狗".into())]);
    }

    #[test]
    fn char_diff_reconstructs_both_sides() {
        // 不变式：Equal+Delete 拼回 old；Equal+Insert 拼回 new
        for (old, new) in [("财兔的Ryan小汪老师", "财兔的Ryan。"), ("abXcd", "aYcdZ"), ("", "新增"), ("删除", "")] {
            let parts = char_diff(old, new);
            let recon_old: String = parts
                .iter()
                .filter_map(|p| match p {
                    DiffPart::Equal(s) | DiffPart::Delete(s) => Some(s.as_str()),
                    DiffPart::Insert(_) => None,
                })
                .collect();
            let recon_new: String = parts
                .iter()
                .filter_map(|p| match p {
                    DiffPart::Equal(s) | DiffPart::Insert(s) => Some(s.as_str()),
                    DiffPart::Delete(_) => None,
                })
                .collect();
            assert_eq!(recon_old, old, "old 重建失败: {old:?}→{new:?}");
            assert_eq!(recon_new, new, "new 重建失败: {old:?}→{new:?}");
        }
    }

    #[test]
    fn ai_prompt_carries_text_and_constraints() {
        let p = ai_rewrite_prompt("待改写的段落");
        assert!(p.contains("待改写的段落"));
        assert!(p.contains("只输出改写后的正文")); // 关键输出约束在场
        assert!(p.contains("保持原意"));
    }

    #[test]
    fn parse_ai_reply_strips_fences_quotes_whitespace() {
        assert_eq!(parse_ai_reply("  改写结果。\n"), "改写结果。");
        assert_eq!(parse_ai_reply("```\n改写结果。\n```"), "改写结果。");
        assert_eq!(parse_ai_reply("```text\n改写结果。\n```"), "改写结果。");
        assert_eq!(parse_ai_reply("“改写结果。”"), "改写结果。");
        assert_eq!(parse_ai_reply("「改写结果。」"), "改写结果。");
        assert_eq!(parse_ai_reply("内文有“引号”不剥。"), "内文有“引号”不剥。");
    }

    #[test]
    fn ai_reply_becomes_proposal_then_accept_commits_and_undoes() {
        let (mut app, node) = app_with_paragraph("原始段落");
        app.receive_ai_result(ok_reply(&node, "原始段落", "更专业的段落。"));
        let p = app.pending_ai.as_ref().expect("应有待审提案");
        assert_eq!(p.changes.len(), 1);
        assert_eq!(p.changes[0].old, "原始段落");
        assert_eq!(p.changes[0].new, "更专业的段落。");
        assert!(p.changes[0].selected); // 默认勾选
        assert_eq!(app.model.get_text(&node), Some("原始段落")); // 提案不碰 model

        app.accept_ai(); // = review 放行 + commit（与 Step 17 同一条通道）
        assert_eq!(app.model.get_text(&node), Some("更专业的段落。"));
        assert_eq!(app.blocks[0].text, "更专业的段落。");
        assert!(app.pending_ai.is_none());

        app.undo_last();
        assert_eq!(app.model.get_text(&node), Some("原始段落"));
    }

    #[test]
    fn multi_change_partial_accept_is_one_atomic_undo_unit() {
        let (mut app, ids) = app_with_paragraphs(&["甲", "乙", "丙"]);
        app.receive_ai_result(ok_reply(&ids[0], "甲", "甲改。"));
        app.receive_ai_result(ok_reply(&ids[1], "乙", "乙改。"));
        app.receive_ai_result(ok_reply(&ids[2], "丙", "丙改。"));
        let p = app.pending_ai.as_mut().expect("应有提案");
        assert_eq!(p.changes.len(), 3);
        p.changes[1].selected = false; // 取消勾选「乙」

        app.accept_ai();
        assert_eq!(app.model.get_text(&ids[0]), Some("甲改。"));
        assert_eq!(app.model.get_text(&ids[1]), Some("乙")); // 未勾 → 没动
        assert_eq!(app.model.get_text(&ids[2]), Some("丙改。"));
        assert_eq!(app.undo_stack.len(), 1); // 一组 = 一个撤销单位

        app.undo_last(); // 一次撤销整组回滚
        assert_eq!(app.model.get_text(&ids[0]), Some("甲"));
        assert_eq!(app.model.get_text(&ids[2]), Some("丙"));
    }

    #[test]
    fn same_node_new_reply_replaces_old_entry() {
        let (mut app, node) = app_with_paragraph("原文");
        app.receive_ai_result(ok_reply(&node, "原文", "第一版改写。"));
        app.receive_ai_result(ok_reply(&node, "原文", "第二版改写。"));
        let p = app.pending_ai.as_ref().unwrap();
        assert_eq!(p.changes.len(), 1); // 不堆积
        assert_eq!(p.changes[0].new, "第二版改写。");
    }

    #[test]
    fn stale_change_skipped_on_accept_while_valid_ones_commit() {
        let (mut app, ids) = app_with_paragraphs(&["甲", "乙"]);
        app.receive_ai_result(ok_reply(&ids[0], "甲", "甲改。"));
        app.receive_ai_result(ok_reply(&ids[1], "乙", "乙改。"));
        app.commit_edit(&ids[0], "人改了甲").unwrap(); // 「甲」那条失效

        app.accept_ai();
        assert_eq!(app.model.get_text(&ids[0]), Some("人改了甲")); // 失效条没覆盖人改
        assert_eq!(app.model.get_text(&ids[1]), Some("乙改。")); // 有效条正常提交
        assert!(app.status.contains("失效"));
    }

    #[test]
    fn ai_reply_reject_leaves_model_untouched() {
        let (mut app, node) = app_with_paragraph("改我呀");
        app.receive_ai_result(AiReply {
            request: AiRequest::Single { node: node.clone(), old: "改我呀".to_string() },
            result: Ok("改好了。".to_string()),
        });
        assert!(app.pending_ai.is_some());
        app.reject_ai();
        assert!(app.pending_ai.is_none());
        assert_eq!(app.model.get_text(&node), Some("改我呀"));
        assert!(app.undo_stack.is_empty());
    }

    #[test]
    fn ai_reply_voided_if_paragraph_changed_during_flight() {
        // AI 思考要数秒：在途期间人改了段落 → 收货时直接作废，不进待审区
        let (mut app, node) = app_with_paragraph("原文");
        app.commit_edit(&node, "人在 AI 思考时改了。").unwrap();
        app.receive_ai_result(AiReply {
            request: AiRequest::Single { node: node.clone(), old: "原文".to_string() }, // 请求发出时的旧文
            result: Ok("AI 的改写。".to_string()),
        });
        assert!(app.pending_ai.is_none());
        assert!(app.status.contains("作废"));
        assert_eq!(app.model.get_text(&node), Some("人在 AI 思考时改了。"));
    }

    #[test]
    fn ai_reply_voided_if_changed_after_proposal_before_accept() {
        // 提案已挂出，接受前人又改了 → accept 时作废（Step 17 既有防线，原样有效）
        let (mut app, node) = app_with_paragraph("原文");
        app.receive_ai_result(AiReply {
            request: AiRequest::Single { node: node.clone(), old: "原文".to_string() },
            result: Ok("AI 的改写。".to_string()),
        });
        app.commit_edit(&node, "人又改了一版。").unwrap();
        app.accept_ai();
        assert_eq!(app.model.get_text(&node), Some("人又改了一版。"));
        assert!(app.status.contains("失效")); // 失效条被跳过，未提交任何东西
        assert!(app.status.contains("未提交"));
    }

    #[test]
    fn ai_reply_no_change_or_error_only_sets_status() {
        let (mut app, node) = app_with_paragraph("已经很好。");
        app.receive_ai_result(AiReply {
            request: AiRequest::Single { node: node.clone(), old: "已经很好。".to_string() },
            result: Ok("已经很好。".to_string()), // 模型没改
        });
        assert!(app.pending_ai.is_none());
        assert!(app.status.contains("无改动建议"));

        app.receive_ai_result(AiReply {
            request: AiRequest::Single { node: node.clone(), old: "已经很好。".to_string() },
            result: Err("Not logged in".to_string()),
        });
        assert!(app.pending_ai.is_none());
        assert!(app.status.contains("AI 调用失败"));
        assert_eq!(app.model.get_text(&node), Some("已经很好。")); // 全程一字未动
    }

    // ── Step 20：全文 AI 清理（JSON 多段提案进 Step 19 通道）──

    #[test]
    fn ai_clean_prompt_lists_segments_with_ids_and_constraints() {
        let segs = vec![
            (NodeId("docx:0".to_string()), "标题1111".to_string()),
            (NodeId("docx:3".to_string()), "正文".to_string()),
        ];
        let p = ai_clean_prompt(&segs);
        assert!(p.contains("[docx:0] 标题1111"));
        assert!(p.contains("[docx:3] 正文"));
        assert!(p.contains("JSON 数组"));
        assert!(p.contains("只包含需要修改的段落"));
    }

    #[test]
    fn parse_clean_reply_filters_unknown_ids_and_no_ops() {
        let olds = vec![
            (NodeId("docx:0".to_string()), "标题1111".to_string()),
            (NodeId("docx:1".to_string()), "干净段".to_string()),
        ];
        // 一条有效；一条 id 不认识；一条 new==old（无实质变化）；一条缺字段
        let raw = "```json\n[\n  {\"id\": \"docx:0\", \"new\": \"标题\"},\n  {\"id\": \"docx:9\", \"new\": \"幻觉段\"},\n  {\"id\": \"docx:1\", \"new\": \"干净段\"},\n  {\"id\": \"docx:1\"}\n]\n```";
        let (changes, dropped) = parse_clean_reply(raw, &olds).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].node, NodeId("docx:0".to_string()));
        assert_eq!(changes[0].old, "标题1111");
        assert_eq!(changes[0].new, "标题");
        assert_eq!(dropped, 3);

        assert!(parse_clean_reply("这不是JSON", &olds).is_err());
        assert_eq!(parse_clean_reply("[]", &olds).unwrap().0.len(), 0);
    }

    #[test]
    fn clean_all_reply_merges_into_review_channel_and_commits_atomically() {
        let (mut app, ids) = app_with_paragraphs(&["标题1111", "干净段", "正文2222"]);
        let olds: Vec<(NodeId, String)> = vec![
            (ids[0].clone(), "标题1111".to_string()),
            (ids[1].clone(), "干净段".to_string()),
            (ids[2].clone(), "正文2222".to_string()),
        ];
        app.receive_ai_result(AiReply {
            request: AiRequest::CleanAll { olds },
            result: Ok(r#"[{"id":"docx:0","new":"标题"},{"id":"docx:2","new":"正文"}]"#.to_string()),
        });
        let p = app.pending_ai.as_ref().expect("应有提案");
        assert_eq!(p.changes.len(), 2); // 只有需要改的段
        assert!(app.status.contains("共 2 条"));

        app.accept_ai(); // 走 Step 19 同一条通道：一个 PatchSet 原子提交
        assert_eq!(app.model.get_text(&ids[0]), Some("标题"));
        assert_eq!(app.model.get_text(&ids[1]), Some("干净段")); // 没被建议的段不动
        assert_eq!(app.model.get_text(&ids[2]), Some("正文"));
        assert_eq!(app.undo_stack.len(), 1);
        app.undo_last(); // 一次撤销整组回滚
        assert_eq!(app.model.get_text(&ids[0]), Some("标题1111"));
        assert_eq!(app.model.get_text(&ids[2]), Some("正文2222"));
    }

    #[test]
    fn clean_all_bad_json_or_empty_only_sets_status() {
        let (mut app, ids) = app_with_paragraphs(&["甲"]);
        let olds = vec![(ids[0].clone(), "甲".to_string())];
        app.receive_ai_result(AiReply {
            request: AiRequest::CleanAll { olds: olds.clone() },
            result: Ok("模型抽风不回JSON".to_string()),
        });
        assert!(app.pending_ai.is_none());
        assert!(app.status.contains("解析失败"));

        app.receive_ai_result(AiReply {
            request: AiRequest::CleanAll { olds },
            result: Ok("[]".to_string()),
        });
        assert!(app.pending_ai.is_none());
        assert!(app.status.contains("无需清理"));
        assert_eq!(app.model.get_text(&ids[0]), Some("甲")); // 全程未动
    }

    #[test]
    fn clean_targets_excludes_opaque_and_caps_at_limit() {
        // Opaque 排除
        let mut app = PolarisApp::new();
        let backend = FakeBackend::new(vec![
            DocxBlock::Paragraph { text: "甲".to_string(), para_id: Some("AA".to_string()) },
            DocxBlock::Unstructured { kind: "table".to_string(), raw: "[T]".to_string(), para_id: None, image: None, anchor: None },
            DocxBlock::Paragraph { text: "乙".to_string(), para_id: Some("BB".to_string()) },
        ]);
        app.load_from_backend(&backend, "x.docx").unwrap();
        let (targets, truncated) = app.clean_targets();
        assert_eq!(targets.len(), 2);
        assert!(targets.iter().all(|(_, t)| t == "甲" || t == "乙"));
        assert_eq!(truncated, 0);

        // 超过 100 段截断并报数
        let many: Vec<DocxBlock> = (0..105)
            .map(|i| DocxBlock::Paragraph { text: format!("段{i}"), para_id: Some(format!("P{i}")) })
            .collect();
        app.load_from_backend(&FakeBackend::new(many), "y.docx").unwrap();
        let (targets, truncated) = app.clean_targets();
        assert_eq!(targets.len(), 100);
        assert_eq!(truncated, 5);
    }

    #[test]
    fn paraidless_file_edits_skip_loudly() {
        // 无 paraId 文件（Pandoc 产物）：能改 model，保存时大声跳过——skipped 如今唯一的来源
        let mut app = PolarisApp::new();
        let backend =
            FakeBackend::new(vec![DocxBlock::Paragraph { text: "x".to_string(), para_id: None }]);
        app.load_from_backend(&backend, "x.docx").unwrap();
        app.commit_edit(&NodeId("docx:0".to_string()), "y").unwrap();
        let (ops, skipped) = app.pending_ops();
        assert!(ops.is_empty());
        assert_eq!(skipped, 1);
    }

    #[test]
    fn add_and_delete_paragraph_with_undo() {
        let mut app = PolarisApp::new(); // 样例 5 块
        let n0 = app.blocks.len();

        app.add_paragraph();
        assert_eq!(app.blocks.len(), n0 + 1);
        assert_eq!(app.blocks.last().unwrap().text, "新段落");

        let added = app.blocks.last().unwrap().node.clone();
        app.delete_block(&added);
        assert_eq!(app.blocks.len(), n0);

        app.undo_last(); // 撤销删除 → 段落回来
        assert_eq!(app.blocks.len(), n0 + 1);
        app.undo_last(); // 撤销添加 → 段落消失
        assert_eq!(app.blocks.len(), n0);
    }

    #[test]
    fn document_blocks_flattens_in_order_with_kind() {
        let m = sample_model();
        let blocks = document_blocks(&m);
        assert_eq!(blocks.len(), 5);
        assert_eq!(blocks[0].kind, NodeKind::Heading { level: 1 });
        assert_eq!(blocks[0].text, "Polaris 文档查看器");
        assert_eq!(blocks[1].kind, NodeKind::Heading { level: 2 });
        assert_eq!(blocks[2].kind, NodeKind::Paragraph);
        assert_eq!(blocks[2].node, NodeId("p1".to_string()));
    }

    #[test]
    fn commit_edit_updates_model_and_undo_restores() {
        let mut app = PolarisApp::new();
        let node = NodeId("p1".to_string());
        let before = app.model.get_text(&node).map(|s| s.to_string());

        app.commit_edit(&node, "改写后的正文").unwrap();
        assert_eq!(app.model.get_text(&node), Some("改写后的正文"));
        assert_eq!(app.undo_stack.len(), 1);

        app.undo_last();
        assert_eq!(app.model.get_text(&node).map(|s| s.to_string()), before);
        assert!(app.undo_stack.is_empty());
    }

    #[test]
    fn commit_edit_noop_when_unchanged() {
        let mut app = PolarisApp::new();
        let node = NodeId("p1".to_string());
        let same = app.model.get_text(&node).unwrap().to_string();
        app.commit_edit(&node, &same).unwrap();
        assert!(app.undo_stack.is_empty()); // 无变化 → 不提交
    }

    #[test]
    fn edit_heading_keeps_kind() {
        let mut app = PolarisApp::new();
        let node = NodeId("t".to_string());
        app.commit_edit(&node, "新标题").unwrap();
        assert_eq!(app.model.get_text(&node), Some("新标题"));
        // 改文本不改 kind：仍是一级标题（typed 模型红利）
        assert_eq!(app.model.node_kind(&node), Some(NodeKind::Heading { level: 1 }));
    }

    // ── 版本历史 ──

    #[test]
    fn unsaved_changes_tracked_against_load_snapshot() {
        let mut app = PolarisApp::new();
        let backend = FakeBackend::new(vec![DocxBlock::Paragraph { text: "甲".to_string(), para_id: None }]);
        app.load_from_backend(&backend, "x.docx").unwrap();
        assert!(!app.has_unsaved_changes()); // 刚加载 = 干净
        app.commit_edit(&NodeId("docx:0".to_string()), "乙").unwrap();
        assert!(app.has_unsaved_changes()); // 进了 model、没存 docx
        app.undo_last();
        assert!(!app.has_unsaved_changes()); // 撤销回去 = 又干净
    }

    #[test]
    fn approve_restore_requires_second_click_when_dirty() {
        let mut app = PolarisApp::new();
        let backend = FakeBackend::new(vec![DocxBlock::Paragraph { text: "甲".to_string(), para_id: None }]);
        app.load_from_backend(&backend, "x.docx").unwrap();
        let v1 = Path::new("/tmp/0000000000001.docx");
        let v2 = Path::new("/tmp/0000000000002.docx");
        assert!(app.approve_restore(v1)); // 干净 → 直接放行

        app.commit_edit(&NodeId("docx:0".to_string()), "乙").unwrap();
        assert!(!app.approve_restore(v1)); // 脏 → 第一次拦下
        assert!(!app.approve_restore(v2)); // 换了目标版本 → 重新拦
        assert!(app.approve_restore(v2)); // 同一目标第二次 → 放行
    }

    #[test]
    fn restore_version_swaps_file_and_reloads_model() {
        // 真实临时文件演文件层历史，FakeBackend 演重载——不碰 officecli 子进程。
        let dir = std::env::temp_dir().join(format!("polaris-gui-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // 清掉上次跑剩的历史，避免脏状态
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("doc.docx");
        std::fs::write(&file, "V1").unwrap();
        let path = file.to_string_lossy().into_owned();

        let mut app = PolarisApp::new();
        let blocks = vec![DocxBlock::Paragraph { text: "旧".to_string(), para_id: None }];
        app.load_from_backend(&FakeBackend::new(blocks.clone()), &path).unwrap();

        history::snapshot(&file).unwrap(); // 留底 V1
        std::fs::write(&file, "V2").unwrap(); // 模拟后来保存成了 V2
        let v1 = history::list_versions(&file).unwrap().last().unwrap().path.clone();

        app.commit_edit(&NodeId("docx:0".to_string()), "未保存改动").unwrap(); // 弄脏
        app.restore_version(&FakeBackend::new(blocks), &v1).unwrap();

        assert_eq!(std::fs::read_to_string(&file).unwrap(), "V1"); // 文件回到旧版
        let kept: Vec<String> = history::list_versions(&file)
            .unwrap()
            .iter()
            .map(|v| std::fs::read_to_string(&v.path).unwrap())
            .collect();
        assert!(kept.contains(&"V2".to_string())); // 回版前的 V2 被留底，没丢
        assert_eq!(app.blocks[0].text, "旧"); // model 已重载
        assert!(app.undo_stack.is_empty());
        assert!(!app.has_unsaved_changes());
    }
}
