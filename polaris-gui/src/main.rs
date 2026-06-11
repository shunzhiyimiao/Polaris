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

/// 一条等待 review 的 AI 改写提案（旧文/新文对照）。提案**不碰 model**；
/// 接受时按当下模型重建 patch（段落若已被人改过则作废）。
struct AiProposal {
    node: NodeId,
    old: String,
    new: String,
}

/// 一次 AI 改写调用的结果（后台线程经 channel 送回 UI 线程）。
struct AiReply {
    node: NodeId,
    /// 发起请求时的原文：收货时若段落已被人改过（请求在路上要数秒），提案过期作废。
    old: String,
    result: Result<String, String>,
}

/// 改写提示词。**纯函数**，便于单测锚定关键约束。
fn ai_rewrite_prompt(text: &str) -> String {
    format!(
        "你是文档润色助手。改写下面这段话，使其更通顺、专业、简洁；保持原意、保持中文、\
         保留专有名词与数字；只输出改写后的正文，不要任何解释、前后缀、引号或 markdown。\n\n原文：\n{text}"
    )
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
/// 改写是轻任务，用快模型；阻塞数秒，**调用方必须放后台线程**。
/// 实测：认证失败 exit=1 且错误走 stdout——所以失败信息把 stdout 也带上。
fn claude_rewrite(text: &str) -> Result<String, String> {
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
        .write_all(ai_rewrite_prompt(text).as_bytes())
        .map_err(|e| format!("写入 claude stdin 失败: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("等待 claude 失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("claude 调用失败: {} {}", stdout.trim(), stderr.trim()));
    }
    let cleaned = parse_ai_reply(&stdout);
    if cleaned.is_empty() {
        return Err("模型返回为空".to_string());
    }
    Ok(cleaned)
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
            let result = claude_rewrite(&old);
            let _ = tx.send(AiReply { node, old, result });
        });
    }

    /// 收 AI 结果：成功且有实质改动 → 进待 review 区；无改动/出错/段落已被人改过 → 只报状态。
    fn receive_ai_result(&mut self, reply: AiReply) {
        match reply.result {
            Err(e) => self.status = format!("AI 调用失败: {e}"),
            Ok(new) => {
                if self.model.get_text(&reply.node) != Some(reply.old.as_str()) {
                    self.status = format!("{} 在 AI 思考期间被改过，提案作废", reply.node.0);
                } else if new == reply.old {
                    self.status = format!("AI 对 {} 无改动建议", reply.node.0);
                } else {
                    self.status = format!("AI 提案待 review：{}（在对照面板接受/拒绝）", reply.node.0);
                    self.pending_ai = Some(AiProposal { node: reply.node, old: reply.old, new });
                }
            }
        }
    }

    /// 接受提案：构造 **AI 来源** patch → `approve_ai`（review 放行）→ 原子 commit → 入撤销栈。
    /// 提案期间段落被人改过 → 作废不提交（提案是针对旧文的）。
    fn accept_ai(&mut self) {
        let Some(p) = self.pending_ai.take() else { return };
        if self.model.get_text(&p.node) != Some(p.old.as_str()) {
            self.status = format!("{} 在提案后被改过，AI 提案作废", p.node.0);
            return;
        }
        let source_map = render_html(&self.model).source_map;
        let Some(span) = source_map.span_for(&p.node) else {
            self.status = format!("{} 不可编辑，AI 提案作废", p.node.0);
            return;
        };
        let set = PatchSet::new(vec![Patch::ai(
            "ai-rewrite",
            Target::Node(p.node.clone()),
            Op::SetSpan { range: span.range, text: p.new.clone() },
        )])
        .approve_ai(); // review 动作：用户刚在对照面板点了「接受」——没有这步 commit 会拒（core 强制）
        match commit(&mut self.model, &set) {
            Ok(inverse) => {
                self.undo_stack.push(inverse);
                self.rebuild_blocks();
                self.status = format!("已接受 AI 改写 {}（撤销可回）", p.node.0);
            }
            Err(e) => self.status = format!("AI 提案提交失败: {e:?}"),
        }
    }

    /// 拒绝提案：丢弃，model 一字未动。
    fn reject_ai(&mut self) {
        if let Some(p) = self.pending_ai.take() {
            self.status = format!("已拒绝 {} 的 AI 提案，模型未动", p.node.0);
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
        let after_of = |node: &NodeId| -> Option<String> {
            self.para_map.get(node).map(|pid| format!("p[@paraId={pid}]"))
        };

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

        // 3) 新增段 → add。锚点 = 前一个**有身份**的原段；无身份块（表格占位）不更新锚点，
        //    所以紧跟表格后新增的段会落到表格前——内容不丢，位置是已知妥协（见交付说明）。
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
                            // Opaque 只读：有真图字节给真图，没有降级为文字占位。
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

        // AI review 对照窗：有待审提案时浮出。决策收集后在面板外统一处理（借用纪律）。
        let mut ai_decision: Option<bool> = None;
        if let Some(p) = &self.pending_ai {
            egui::Window::new("AI 改写提案（需 review）")
                .collapsible(false)
                .default_width(420.0)
                .show(ctx, |ui| {
                    ui.label(format!("目标段落: {}", p.node.0));
                    ui.separator();
                    ui.label("旧文:");
                    ui.label(egui::RichText::new(p.old.as_str()).weak());
                    ui.separator();
                    ui.label("新文（AI 建议）:");
                    ui.label(egui::RichText::new(p.new.as_str()).strong());
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("✓ 接受").clicked() {
                            ai_decision = Some(true);
                        }
                        if ui.button("✗ 拒绝").clicked() {
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
            DocxBlock::Unstructured { kind: "table".to_string(), raw: "[Table: 1 rows]".to_string(), para_id: None, image: None }, // docx:1
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
    fn opaque_blocks_are_readonly_and_delete_gated() {
        let mut app = PolarisApp::new();
        let backend = FakeBackend::new(vec![
            DocxBlock::Paragraph { text: "前段".to_string(), para_id: Some("AA".to_string()) }, // docx:0
            DocxBlock::Unstructured { kind: "table".to_string(), raw: "[T]".to_string(), para_id: None, image: None }, // docx:1
            DocxBlock::Unstructured {
                kind: "image".to_string(),
                raw: "alt=\"图\"".to_string(),
                para_id: Some("IMG".to_string()),
                image: None,
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
            }, // docx:1
            DocxBlock::Unstructured {
                kind: "image".to_string(),
                raw: "alt=\"没抽到\"".to_string(),
                para_id: Some("MISS".to_string()),
                image: None, // 没字节 → 降级文字占位（不进 map，但块仍在）
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

    /// 测试小工具：装一篇单段文档。
    fn app_with_paragraph(text: &str) -> (PolarisApp, NodeId) {
        let mut app = PolarisApp::new();
        let backend = FakeBackend::new(vec![DocxBlock::Paragraph {
            text: text.to_string(),
            para_id: Some("AA".to_string()),
        }]);
        app.load_from_backend(&backend, "x.docx").unwrap();
        (app, NodeId("docx:0".to_string()))
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
        app.receive_ai_result(AiReply {
            node: node.clone(),
            old: "原始段落".to_string(),
            result: Ok("更专业的段落。".to_string()),
        });
        let p = app.pending_ai.as_ref().expect("应有待审提案");
        assert_eq!(p.old, "原始段落");
        assert_eq!(p.new, "更专业的段落。");
        assert_eq!(app.model.get_text(&node), Some("原始段落")); // 提案不碰 model

        app.accept_ai(); // = review 放行 + commit（与 Step 17 同一条通道）
        assert_eq!(app.model.get_text(&node), Some("更专业的段落。"));
        assert_eq!(app.blocks[0].text, "更专业的段落。");
        assert!(app.pending_ai.is_none());

        app.undo_last();
        assert_eq!(app.model.get_text(&node), Some("原始段落"));
    }

    #[test]
    fn ai_reply_reject_leaves_model_untouched() {
        let (mut app, node) = app_with_paragraph("改我呀");
        app.receive_ai_result(AiReply {
            node: node.clone(),
            old: "改我呀".to_string(),
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
            node: node.clone(),
            old: "原文".to_string(), // 请求发出时的旧文
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
            node: node.clone(),
            old: "原文".to_string(),
            result: Ok("AI 的改写。".to_string()),
        });
        app.commit_edit(&node, "人又改了一版。").unwrap();
        app.accept_ai();
        assert_eq!(app.model.get_text(&node), Some("人又改了一版。"));
        assert!(app.status.contains("作废"));
    }

    #[test]
    fn ai_reply_no_change_or_error_only_sets_status() {
        let (mut app, node) = app_with_paragraph("已经很好。");
        app.receive_ai_result(AiReply {
            node: node.clone(),
            old: "已经很好。".to_string(),
            result: Ok("已经很好。".to_string()), // 模型没改
        });
        assert!(app.pending_ai.is_none());
        assert!(app.status.contains("无改动建议"));

        app.receive_ai_result(AiReply {
            node: node.clone(),
            old: "已经很好。".to_string(),
            result: Err("Not logged in".to_string()),
        });
        assert!(app.pending_ai.is_none());
        assert!(app.status.contains("AI 调用失败"));
        assert_eq!(app.model.get_text(&node), Some("已经很好。")); // 全程一字未动
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
