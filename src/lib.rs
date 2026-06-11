//! editor-core — 最小可运行内核 (Step B1)
//!
//! 目标只有一个：**第一次真正 `cargo test` 跑绿**。把项目从「纸上架构」变成「真实代码」。
//!
//! 刻意压到最小，保证一次编译过：
//!   · 中性命名 `editor-core`（不属于 office，将来 Drafting 也能复用）。
//!   · 零外部依赖（连 serde 都不用——op payload 先用最朴素的 enum，把编译面降到最低）。
//!   · 只有一个 op：`SetMarkdown`，且它返回**自己的逆 op**（可逆性的最小证明）。
//!   · 不接 PatchEngine / assemble / depends_on / SourceMap —— 全部等这个绿了再增量加。
//!
//! 这是 B 方案的第一步：先要「一个真能跑的最小核」，再要「完整」。
//! 跑：把本文件放进 `src/lib.rs`，`cargo test`。

use std::collections::HashMap;

// ───────────────────────── identity ─────────────────────────

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct NodeId(pub String);

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct PatchId(pub String);

// ───────────────────────── offsets：char-safe 偏移 ─────────────────────────

/// 文本位置，**以 char 计**（不是 byte）。这是对 CJK/emoji 安全的前提：
/// 业务层只谈 char 位置，byte 偏移只在 `CharIndex` 这一层换算。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pos(pub usize);

/// 半开区间 `[start, end)`，两端都是 char 位置。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CharRange {
    pub start: Pos,
    pub end: Pos,
}
impl CharRange {
    /// 便捷构造：直接给 char 下标。
    pub fn chars(start: usize, end: usize) -> Self {
        CharRange { start: Pos(start), end: Pos(end) }
    }
}

/// char↔byte 转换层。**唯一**允许把「位置」落到 byte 偏移的地方；
/// 业务代码永远不裸 `s[a..b]`，必须走这里换算，才能对多字节字符安全。
pub struct CharIndex;
impl CharIndex {
    /// 一个串里有多少个 char。
    pub fn char_len(s: &str) -> usize {
        s.chars().count()
    }

    /// 把 char 位置换成 byte 偏移；允许等于「字符总数」（即末尾）。越界 → `None`。
    pub fn byte_offset(s: &str, char_pos: usize) -> Option<usize> {
        let mut count = 0;
        for (byte, _) in s.char_indices() {
            if count == char_pos {
                return Some(byte);
            }
            count += 1;
        }
        // 走到这里 count == 字符总数；只有正好落在末尾才合法。
        if count == char_pos {
            Some(s.len())
        } else {
            None
        }
    }

    /// 把 char 区间换成 `(byte_start, byte_end)`；任一端越界或 start > end → `None`。
    pub fn byte_range(s: &str, range: &CharRange) -> Option<(usize, usize)> {
        let start = Self::byte_offset(s, range.start.0)?;
        let end = Self::byte_offset(s, range.end.0)?;
        if start <= end {
            Some((start, end))
        } else {
            None
        }
    }
}

// ───────────────────────── 节点类型（typed，非 notation）─────────────────────────

/// 节点的块类型——**typed 真理**，不靠文本里的 `#` 记法判断（还原则①：notation ≠ model）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Paragraph,
    Heading { level: usize },
    /// 解析不进结构的不透明内容（任何 notation 都有，**中性概念**，不是 office 专有）。
    /// 「全部可见、部分结构化」的 typed 落地：节点 text 是它的人类可读描述，**只读**——
    /// renderer 不为它产 SourceMap 片段，`apply_fragment_edit` 因此天然拒绝（NoSpan）。
    /// 具体是什么（表格/图片/…）属于 adapter 层词汇，不进内核。
    Opaque,
}
impl Default for NodeKind {
    fn default() -> Self {
        NodeKind::Paragraph
    }
}

// ───────────────────────── 结构快照 ─────────────────────────

/// 一棵子树的无损快照：节点自身（含 kind）+ 递归全部后代。
/// `RemoveNode` 删带 children 的节点时，逆 op 用它把整棵子树原样还原。
#[derive(Clone, Debug, PartialEq)]
pub struct NodeSnapshot {
    pub id: NodeId,
    pub kind: NodeKind,
    pub text: String,
    pub children: Vec<NodeSnapshot>,
}

// ───────────────────────── op（先用朴素 enum，不引 serde_json）─────────────────────────

/// 最小 op 集合。将来扩展 op 时往这里加变体；payload 先用具体字段，避免 JSON 依赖。
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    SetMarkdown { md: String },
    /// 区间替换：把 `range`（char 计）内的文本换成 `text`。
    /// 逆 op 也是 `SetSpan`，但 range 会随插入文本的长度变化而不同（长度变了也能 undo）。
    SetSpan { range: CharRange, text: String },
    /// 设节点的块类型（typed）。逆 op 是 `SetKind { 旧 kind }`。
    SetKind { kind: NodeKind },

    /// 在 `parent` 的第 `index` 个孩子位插入一个新叶子 section（`id` + `text`）。
    /// 逆 op 是 `RemoveNode { id }`。
    InsertSection { parent: NodeId, index: usize, id: NodeId, text: String },
    /// 删除 `id` 及其整棵子树。逆 op 是 `CreateNode`，快照含 children，可无损还原。
    RemoveNode { id: NodeId },
    /// 把一棵子树快照重建到 `parent` 的第 `index` 个孩子位。逆 op 是 `RemoveNode`。
    CreateNode { parent: NodeId, index: usize, snapshot: NodeSnapshot },
}

// ───────────────────────── target ─────────────────────────

/// 先只支持单节点 target。Anchor/Region 等等后续增量加。
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Node(NodeId),
}
impl Target {
    pub fn primary_node(&self) -> Option<&NodeId> {
        match self {
            Target::Node(n) => Some(n),
        }
    }
}

// ───────────────────────── patch ─────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum PatchSource {
    Authored,
    Derived { from: PatchId },
    /// AI 产出的 patch。`reviewed=false` 时 **commit 直接拒绝**（原则 7：Human 直达，AI 过 review）——
    /// 信任不对称是数据级强制，不靠 UI 自觉。review 动作 = `PatchSet::approve_ai`。
    Ai { reviewed: bool },
}

#[derive(Clone, Debug)]
pub struct Patch {
    pub id: PatchId,
    pub source: PatchSource,
    pub target: Target,
    pub op: Op,
    /// 本 patch 依赖的其它 patch id（必须在同一 PatchSet 内）。拓扑序据此定序。
    pub depends_on: Vec<PatchId>,
}
impl Patch {
    pub fn authored(id: &str, target: Target, op: Op) -> Self {
        Patch {
            id: PatchId(id.to_string()),
            source: PatchSource::Authored,
            target,
            op,
            depends_on: Vec::new(),
        }
    }
    /// AI 提案 patch：出生即「未 review」，进不了 commit，必须先走 `PatchSet::approve_ai`。
    pub fn ai(id: &str, target: Target, op: Op) -> Self {
        Patch {
            id: PatchId(id.to_string()),
            source: PatchSource::Ai { reviewed: false },
            target,
            op,
            depends_on: Vec::new(),
        }
    }
    /// 链式设置依赖（id 字符串）。
    pub fn with_deps(mut self, deps: &[&str]) -> Self {
        self.depends_on = deps.iter().map(|d| PatchId(d.to_string())).collect();
        self
    }
}

// ───────────────────────── patchset + 拓扑序 ─────────────────────────

/// 一组要**原子**处理的 patch；组内按 `depends_on` 拓扑序执行（Step 5 才真正 apply）。
#[derive(Clone, Debug)]
pub struct PatchSet {
    pub patches: Vec<Patch>,
}

#[derive(Debug, PartialEq)]
pub enum TopoError {
    /// `depends_on` 指向组外不存在的 patch id。
    DanglingDependency { patch: PatchId, missing: PatchId },
    /// 组内出现重复的 patch id。
    DuplicateId(PatchId),
    /// 依赖成环，整组无法定序（携带仍未定序的 id，按原序）。
    Cycle(Vec<PatchId>),
}

impl PatchSet {
    pub fn new(patches: Vec<Patch>) -> Self {
        PatchSet { patches }
    }

    /// **review 动作**：把组内全部 AI patch 标记为已 review。调用它即宣告
    /// 「人已经看过这组 AI 改动并放行」——UI 层的职责是保证这句话为真（先展示对照再调它）。
    /// 没有它，含 AI patch 的组进不了 `commit`（原则 7 的机械强制）。
    pub fn approve_ai(mut self) -> Self {
        for p in &mut self.patches {
            if let PatchSource::Ai { reviewed } = &mut p.source {
                *reviewed = true;
            }
        }
        self
    }

    /// 按 `depends_on` 做**稳定拓扑排序**：依赖在前、被依赖在后；互无约束者保持输入原序。
    /// 悬空依赖 → `DanglingDependency`；重复 id → `DuplicateId`；成环 → `Cycle`。
    /// 纯函数，不碰 model。
    pub fn topo_order(&self) -> Result<Vec<&Patch>, TopoError> {
        let n = self.patches.len();

        // id → index，并查重。
        let mut index_of: HashMap<&PatchId, usize> = HashMap::with_capacity(n);
        for (i, p) in self.patches.iter().enumerate() {
            if index_of.insert(&p.id, i).is_some() {
                return Err(TopoError::DuplicateId(p.id.clone()));
            }
        }

        // 闭合校验：每个 depends_on 必须落在组内。
        for p in &self.patches {
            for dep in &p.depends_on {
                if !index_of.contains_key(dep) {
                    return Err(TopoError::DanglingDependency {
                        patch: p.id.clone(),
                        missing: dep.clone(),
                    });
                }
            }
        }

        // 稳定拓扑：每轮在原序里挑「依赖都已发射」的最靠前者，发射后从头重扫。
        let mut emitted = vec![false; n];
        let mut order: Vec<&Patch> = Vec::with_capacity(n);
        while order.len() < n {
            let mut progressed = false;
            for i in 0..n {
                if emitted[i] {
                    continue;
                }
                let ready = self.patches[i]
                    .depends_on
                    .iter()
                    .all(|dep| emitted[index_of[dep]]);
                if ready {
                    emitted[i] = true;
                    order.push(&self.patches[i]);
                    progressed = true;
                    break; // 重新从头扫，保证每步取原序最靠前的就绪 patch
                }
            }
            if !progressed {
                // 还有剩余却无人就绪 → 成环；按原序收集未发射 id。
                let stuck = self
                    .patches
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !emitted[*i])
                    .map(|(_, p)| p.id.clone())
                    .collect();
                return Err(TopoError::Cycle(stuck));
            }
        }
        Ok(order)
    }
}

#[derive(Debug, PartialEq)]
pub enum PatchError {
    UnknownTarget(NodeId),
    /// SetSpan 的区间落在文本之外（含 start > end）。
    RangeOutOfBounds(CharRange),
    /// 插入/重建时 id 已存在。
    DuplicateNode(NodeId),
    /// 子节点下标越过父 children 长度（== len 是末尾追加，合法）。
    IndexOutOfBounds { parent: NodeId, index: usize, len: usize },
    /// 想删一个没有父的节点（root 或游离节点）。
    NotRemovable(NodeId),
}

// ───────────────────────── DocumentModel trait ─────────────────────────

pub trait DocumentModel {
    fn root_kind(&self) -> &'static str;
    fn get_text(&self, id: &NodeId) -> Option<&str>;
    /// 应用一个 op，返回**逆 op**（model 内部状态可逆的最小证明）。
    fn apply_op(&mut self, target: &Target, op: &Op) -> Result<Op, PatchError>;
    /// **纯函数**：只读「已 apply authored」的模型，产 derived patch（不改模型、不预测、不镜像 apply_op）。
    /// 传入 authored 仅用于把派生 patch 的 `depends_on` 链到触发它的 authored patch。
    fn derive(&self, authored: &[Patch]) -> Vec<Patch>;
}

// ───────────────────────── assemble 时序 ─────────────────────────

#[derive(Debug, PartialEq)]
pub enum AssembleError {
    Apply(PatchError),
    Topo(TopoError),
}

/// 组装时序：**apply authored 进缓冲 → derive → topo → 打包**，返回按拓扑序排好的 PatchSet。
/// Step 4 直接把 authored apply 到 model 当「缓冲」；真正的事务/回滚是 Step 5。
pub fn assemble<M: DocumentModel>(model: &mut M, authored: Vec<Patch>) -> Result<PatchSet, AssembleError> {
    // 1. apply authored，让 model 反映新状态（derive 要读它）。
    for p in &authored {
        model.apply_op(&p.target, &p.op).map_err(AssembleError::Apply)?;
    }
    // 2. derive 只读新状态，产派生 patch（depends_on 已链到 authored）。
    let derived = model.derive(&authored);
    // 3. authored + derived 合并，按 depends_on 拓扑定序。
    let mut all = authored;
    all.extend(derived);
    let set = PatchSet::new(all);
    let ordered: Vec<Patch> = set
        .topo_order()
        .map_err(AssembleError::Topo)?
        .into_iter()
        .cloned()
        .collect();
    // 4. 打包成按拓扑序排列的 PatchSet。
    Ok(PatchSet::new(ordered))
}

// ───────────────────────── commit + undo（事务）─────────────────────────

#[derive(Debug, PartialEq)]
pub enum CommitError {
    /// 拓扑定序失败（环/悬空依赖/重复 id）；整组 abort，model 未改动。
    Topo(TopoError),
    /// 拓扑序里某个 patch apply 失败；已 apply 的部分已逆序回滚，model 复原到提交前。
    Aborted { patch: PatchId, cause: PatchError },
    /// 组内含未 review 的 AI patch；整组 abort，model 一行未动（原则 7：AI 不可直达）。
    UnreviewedAi(PatchId),
}

/// 把已排好序的 patch 依次 apply：任一失败就把已 apply 的逆 op 逆序打回去（回滚整组），返回 Err。
/// 成功则返回各 patch 的逆 patch（**apply 顺序**）。
/// 依赖 apply_op 的「per-op 原子性」：单个 op 失败时不改 model，所以回滚起点是干净的。
fn apply_sequence<M: DocumentModel>(model: &mut M, ordered: &[Patch]) -> Result<Vec<Patch>, CommitError> {
    let mut inverses: Vec<Patch> = Vec::with_capacity(ordered.len());
    for p in ordered {
        match model.apply_op(&p.target, &p.op) {
            Ok(inv_op) => inverses.push(Patch {
                id: PatchId(format!("inv:{}", p.id.0)),
                source: PatchSource::Authored,
                target: p.target.clone(),
                op: inv_op,
                depends_on: Vec::new(),
            }),
            Err(cause) => {
                // 回滚：已 apply 的逆 op 逆序打回（model 正处在它们各自的「后置」状态，必然成功）。
                for inv in inverses.iter().rev() {
                    let _ = model.apply_op(&inv.target, &inv.op);
                }
                return Err(CommitError::Aborted { patch: p.id.clone(), cause });
            }
        }
    }
    Ok(inverses)
}

/// **原子提交**一个 PatchSet：按 depends_on 拓扑序 apply；任一失败回滚整组。
/// 成功返回**逆 PatchSet**（patch 已按 undo 顺序——即 apply 的逆序——排列）。
pub fn commit<M: DocumentModel>(model: &mut M, set: &PatchSet) -> Result<PatchSet, CommitError> {
    // 0. 信任闸口（原则 7）：未 review 的 AI patch → 整组拒绝，model 一行未动。
    //    Human/Derived patch 直达；AI patch 必须先经 `approve_ai`。
    if let Some(p) = set
        .patches
        .iter()
        .find(|p| matches!(p.source, PatchSource::Ai { reviewed: false }))
    {
        return Err(CommitError::UnreviewedAi(p.id.clone()));
    }
    // 1. 拓扑定序：环/悬空/重复 → 整组 abort，model 一行未动。
    let ordered: Vec<Patch> = set
        .topo_order()
        .map_err(CommitError::Topo)?
        .into_iter()
        .cloned()
        .collect();
    // 2. 顺序 apply + 失败回滚，拿到 apply 顺序的逆 patch。
    let mut inverses = apply_sequence(model, &ordered)?;
    // 3. 逆 PatchSet = 逆 patch 逆序（undo 按此顺序 apply 即逆序回滚）。
    inverses.reverse();
    Ok(PatchSet::new(inverses))
}

/// **回滚整组**：把 commit 返回的逆 PatchSet 按其存储顺序原子 apply。
pub fn undo<M: DocumentModel>(model: &mut M, inverse: &PatchSet) -> Result<(), CommitError> {
    apply_sequence(model, &inverse.patches)?;
    Ok(())
}

// ───────────────────────── 最小 prose model ─────────────────────────

/// 节点内部数据：自身文本 + **有序** children。
struct NodeData {
    kind: NodeKind,
    text: String,
    children: Vec<NodeId>,
}

/// 一棵有序节点树：`root` 是顶层容器，section 作为它（或更深节点）的有序孩子。
/// 够证明 setMarkdown/setSpan 的文本闭环 + insert/remove/create 的结构闭环。
pub struct ProseModel {
    nodes: HashMap<NodeId, NodeData>,
    root: NodeId,
    /// 单调递增的修订号；每次成功 apply_op +1。derive 只读，绝不动它。
    rev: u64,
    /// 可选的 TOC 节点 id；注册后 derive 才会派生「更新 TOC」patch。
    toc: Option<NodeId>,
}
impl ProseModel {
    pub fn new() -> Self {
        let root = NodeId("root".to_string());
        let mut nodes = HashMap::new();
        nodes.insert(root.clone(), NodeData { kind: NodeKind::Paragraph, text: String::new(), children: Vec::new() });
        ProseModel { nodes, root, rev: 0, toc: None }
    }

    /// 顶层容器节点 id。结构 op 通常以它为 parent。
    pub fn root(&self) -> &NodeId {
        &self.root
    }

    /// 当前修订号。
    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// 注册 TOC 节点（该节点应已存在）；derive 据此决定是否派生 TOC 更新。
    pub fn set_toc(&mut self, id: &str) {
        self.toc = Some(NodeId(id.to_string()));
    }

    /// 注册/更新一个顶层叶子节点（新建则挂到 root 下）。保留 Step 0/1 的用法。
    pub fn insert(&mut self, id: &str, text: &str) {
        let nid = NodeId(id.to_string());
        if self.nodes.contains_key(&nid) {
            self.nodes.get_mut(&nid).unwrap().text = text.to_string();
        } else {
            self.nodes
                .insert(nid.clone(), NodeData { kind: NodeKind::Paragraph, text: text.to_string(), children: Vec::new() });
            self.nodes.get_mut(&self.root).expect("root 必然存在").children.push(nid);
        }
    }

    /// 读某节点的有序 children（id 列表）。
    pub fn children(&self, id: &NodeId) -> Option<&[NodeId]> {
        self.nodes.get(id).map(|n| n.children.as_slice())
    }

    /// 读某节点的块类型（typed）。
    pub fn node_kind(&self, id: &NodeId) -> Option<NodeKind> {
        self.nodes.get(id).map(|n| n.kind)
    }

    /// 取某节点文本的可变借用（只给文本 op 用；和结构 op 的整表改动分开，避免借用打架）。
    fn text_slot_mut(&mut self, target: &Target) -> Result<&mut String, PatchError> {
        let id = target
            .primary_node()
            .cloned()
            .ok_or_else(|| PatchError::UnknownTarget(NodeId("<none>".to_string())))?;
        let node = self
            .nodes
            .get_mut(&id)
            .ok_or_else(|| PatchError::UnknownTarget(id.clone()))?;
        Ok(&mut node.text)
    }

    /// 找 `child` 的父节点及其在父 children 中的下标；root/游离节点 → None。
    fn find_parent(&self, child: &NodeId) -> Option<(NodeId, usize)> {
        for (pid, data) in &self.nodes {
            if let Some(idx) = data.children.iter().position(|c| c == child) {
                return Some((pid.clone(), idx));
            }
        }
        None
    }

    /// 递归把 `id` 为根的子树快照下来（含全部后代）。
    fn snapshot(&self, id: &NodeId) -> Option<NodeSnapshot> {
        let data = self.nodes.get(id)?;
        let children = data
            .children
            .iter()
            .map(|c| self.snapshot(c).expect("树不变量：child 必然存在"))
            .collect();
        Some(NodeSnapshot { id: id.clone(), kind: data.kind, text: data.text.clone(), children })
    }

    /// 递归把一棵快照树插回 nodes 表（不挂任何父）。
    fn restore_subtree(&mut self, snap: &NodeSnapshot) {
        let child_ids = snap.children.iter().map(|c| c.id.clone()).collect();
        self.nodes
            .insert(snap.id.clone(), NodeData { kind: snap.kind, text: snap.text.clone(), children: child_ids });
        for c in &snap.children {
            self.restore_subtree(c);
        }
    }

    /// 递归把 `id` 子树从 nodes 表删掉（不动父的 children）。
    fn remove_subtree(&mut self, id: &NodeId) {
        if let Some(data) = self.nodes.remove(id) {
            for c in &data.children {
                self.remove_subtree(c);
            }
        }
    }

    /// 文档序（从 root 前序遍历）收集所有 heading 的标题。
    fn collect_headings(&self, id: &NodeId, out: &mut Vec<String>) {
        if let Some(nd) = self.nodes.get(id) {
            if matches!(nd.kind, NodeKind::Heading { .. }) {
                // 标题即节点纯文本（不含任何 `#` 记法）。
                out.push(nd.text.clone());
            }
            for c in &nd.children {
                self.collect_headings(c, out);
            }
        }
    }

    /// 把当前所有 heading 标题拼成 TOC 文本（每行 `- 标题`）。
    fn build_toc(&self) -> String {
        let mut titles = Vec::new();
        self.collect_headings(&self.root, &mut titles);
        titles.iter().map(|t| format!("- {t}")).collect::<Vec<_>>().join("\n")
    }
}
impl Default for ProseModel {
    fn default() -> Self { Self::new() }
}

impl DocumentModel for ProseModel {
    fn root_kind(&self) -> &'static str {
        "prose"
    }

    fn get_text(&self, id: &NodeId) -> Option<&str> {
        self.nodes.get(id).map(|n| n.text.as_str())
    }

    fn apply_op(&mut self, target: &Target, op: &Op) -> Result<Op, PatchError> {
        let inverse = match op {
            Op::SetMarkdown { md } => {
                let slot = self.text_slot_mut(target)?;
                let old = std::mem::replace(slot, md.clone());
                // 逆 op：把文本设回旧值。
                Ok(Op::SetMarkdown { md: old })
            }
            Op::SetSpan { range, text } => {
                let slot = self.text_slot_mut(target)?;
                // 唯一的 byte 落点：走 CharIndex 换算，绝不裸用 char 位置当 byte。
                let (b_start, b_end) = CharIndex::byte_range(slot.as_str(), range)
                    .ok_or(PatchError::RangeOutOfBounds(*range))?;
                // b_start/b_end 已是 char 边界，这里切片对 CJK/emoji 安全。
                let removed = slot[b_start..b_end].to_string();
                let mut next = String::with_capacity(slot.len() - (b_end - b_start) + text.len());
                next.push_str(&slot[..b_start]);
                next.push_str(text);
                next.push_str(&slot[b_end..]);
                *slot = next;
                // 逆 op：把「新插入的那段」换回旧文本；range 跟着新长度走，所以长度变了也能精确还原。
                let inserted = CharIndex::char_len(text);
                let inv_range = CharRange { start: range.start, end: Pos(range.start.0 + inserted) };
                Ok(Op::SetSpan { range: inv_range, text: removed })
            }
            Op::SetKind { kind } => {
                let id = target
                    .primary_node()
                    .cloned()
                    .ok_or_else(|| PatchError::UnknownTarget(NodeId("<none>".to_string())))?;
                let node = self
                    .nodes
                    .get_mut(&id)
                    .ok_or_else(|| PatchError::UnknownTarget(id.clone()))?;
                // 逆 op：把 kind 设回旧值。
                let old = std::mem::replace(&mut node.kind, *kind);
                Ok(Op::SetKind { kind: old })
            }

            Op::InsertSection { parent, index, id, text } => {
                if !self.nodes.contains_key(parent) {
                    return Err(PatchError::UnknownTarget(parent.clone()));
                }
                if self.nodes.contains_key(id) {
                    return Err(PatchError::DuplicateNode(id.clone()));
                }
                let len = self.nodes.get(parent).unwrap().children.len();
                if *index > len {
                    return Err(PatchError::IndexOutOfBounds { parent: parent.clone(), index: *index, len });
                }
                self.nodes.insert(id.clone(), NodeData { kind: NodeKind::Paragraph, text: text.clone(), children: Vec::new() });
                self.nodes.get_mut(parent).unwrap().children.insert(*index, id.clone());
                // 逆 op：删掉刚插入的叶子。
                Ok(Op::RemoveNode { id: id.clone() })
            }

            Op::RemoveNode { id } => {
                if !self.nodes.contains_key(id) {
                    return Err(PatchError::UnknownTarget(id.clone()));
                }
                let (parent, index) = self
                    .find_parent(id)
                    .ok_or_else(|| PatchError::NotRemovable(id.clone()))?;
                // 先把整棵子树（含 children）快照下来，再删——逆 op 才能无损还原。
                let snapshot = self.snapshot(id).expect("已确认存在");
                self.nodes.get_mut(&parent).unwrap().children.remove(index);
                self.remove_subtree(id);
                // 逆 op：在原位 (parent, index) 用快照重建整棵子树。
                Ok(Op::CreateNode { parent, index, snapshot })
            }

            Op::CreateNode { parent, index, snapshot } => {
                if !self.nodes.contains_key(parent) {
                    return Err(PatchError::UnknownTarget(parent.clone()));
                }
                if self.nodes.contains_key(&snapshot.id) {
                    return Err(PatchError::DuplicateNode(snapshot.id.clone()));
                }
                let len = self.nodes.get(parent).unwrap().children.len();
                if *index > len {
                    return Err(PatchError::IndexOutOfBounds { parent: parent.clone(), index: *index, len });
                }
                self.restore_subtree(snapshot);
                self.nodes.get_mut(parent).unwrap().children.insert(*index, snapshot.id.clone());
                // 逆 op：删掉刚重建的子树。
                Ok(Op::RemoveNode { id: snapshot.id.clone() })
            }
        }?;
        // 走到这里说明 apply 成功（所有失败路径都已提前 return/?）。
        self.rev += 1;
        Ok(inverse)
    }

    fn derive(&self, authored: &[Patch]) -> Vec<Patch> {
        // 没注册 toc 节点（或它已不存在）就不派生。
        let toc_id = match &self.toc {
            Some(t) if self.nodes.contains_key(t) => t.clone(),
            _ => return Vec::new(),
        };

        // 找「改了 heading 的 authored patch」：其目标节点**当前**是 heading（只读结果，不预测）。
        let heading_deps: Vec<PatchId> = authored
            .iter()
            .filter(|p| {
                p.target
                    .primary_node()
                    .and_then(|n| self.nodes.get(n))
                    .map(|nd| matches!(nd.kind, NodeKind::Heading { .. }))
                    .unwrap_or(false)
            })
            .map(|p| p.id.clone())
            .collect();

        // 没有 heading 被改 → TOC 无需更新。
        if heading_deps.is_empty() {
            return Vec::new();
        }

        // 读「已 apply authored」后的当前 headings，生成 TOC 文本。
        let toc_text = self.build_toc();

        // 派生「更新 TOC」patch，depends_on 显式链到所有改了 heading 的 authored patch。
        vec![Patch {
            id: PatchId("derive:toc".to_string()),
            source: PatchSource::Derived { from: heading_deps[0].clone() },
            target: Target::Node(toc_id),
            op: Op::SetMarkdown { md: toc_text },
            depends_on: heading_deps,
        }]
    }
}

// ───────────────────────── renderer + source map（出口）─────────────────────────

/// 一段渲染片段对应回 model 的位置：哪个节点的哪段 char-range（**只到 char，不碰像素**）。
#[derive(Clone, Debug, PartialEq)]
pub struct SourceSpan {
    pub node: NodeId,
    pub range: CharRange,
}

/// 渲染片段 ↔ (node, CharRange) 的映射。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceMap {
    pub spans: Vec<SourceSpan>,
}
impl SourceMap {
    /// 反查：某节点对应的第一条片段。
    pub fn span_for(&self, node: &NodeId) -> Option<&SourceSpan> {
        self.spans.iter().find(|s| &s.node == node)
    }
}

/// 渲染结果：HTML 文本 + SourceMap。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rendered {
    pub html: String,
    pub source_map: SourceMap,
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;")
}

/// 把 `&ProseModel` 渲染成 HTML（**具体类型入参**，不做 trait object / unsafe downcast）。
/// 文档序前序遍历：heading（按 typed `NodeKind`）→ `<hN>`、其余非空节点 → `<p>`；
/// 每个文本片段产出一条 SourceMap（node + 该片段在节点**纯文本**里的 char-range，对 CJK/emoji 安全）。
pub fn render_html(model: &ProseModel) -> Rendered {
    let mut out = Rendered::default();
    let root = model.root().clone();
    render_node(model, &root, &mut out);
    out
}

fn render_node(model: &ProseModel, id: &NodeId, out: &mut Rendered) {
    if let Some(text) = model.get_text(id) {
        if !text.is_empty() {
            // 标签由 typed kind 决定，不再解析文本里的 `#`。
            let kind = model.node_kind(id);
            let tag = match kind {
                Some(NodeKind::Heading { level }) => format!("h{}", level.clamp(1, 6)),
                Some(NodeKind::Opaque) => "pre".to_string(),
                _ => "p".to_string(),
            };
            out.html.push_str(&format!(
                "<{tag} data-node=\"{}\">{}</{tag}>",
                escape_attr(&id.0),
                escape_text(text)
            ));
            // Opaque 只读：**不产 SourceMap 片段**，「看 → 改」回路对它天然断开
            // （apply_fragment_edit 反查不到 span → NoSpan），这是 core 层的只读强制。
            if kind != Some(NodeKind::Opaque) {
                let range = CharRange { start: Pos(0), end: Pos(CharIndex::char_len(text)) };
                out.source_map.spans.push(SourceSpan { node: id.clone(), range });
            }
        }
    }
    if let Some(children) = model.children(id) {
        for c in children {
            render_node(model, c, out);
        }
    }
}

// ───────────────────────── 编辑回写（看 → 改）─────────────────────────

#[derive(Debug, PartialEq)]
pub enum EditError {
    /// SourceMap 里没有这个节点的片段（点了不可编辑/不存在的东西）。
    NoSpan(NodeId),
    /// 提交失败（透传 commit 的错误）。
    Commit(CommitError),
}

/// 把对某渲染片段的「整段改写」回写成 model 修改：经 `SourceMap` 反查该节点片段的 char-range，
/// 构造 `SetSpan` 并**原子提交**，返回逆 `PatchSet`（可 `undo`）。
/// 这是「看 → 改」回路的 core 一半——UI 层拿到 `data-node` 与新文本后调它。
pub fn apply_fragment_edit(
    model: &mut ProseModel,
    source_map: &SourceMap,
    node: &NodeId,
    new_text: &str,
) -> Result<PatchSet, EditError> {
    let span = source_map.span_for(node).ok_or_else(|| EditError::NoSpan(node.clone()))?;
    let set = PatchSet::new(vec![Patch::authored(
        "edit",
        Target::Node(node.clone()),
        Op::SetSpan { range: span.range, text: new_text.to_string() },
    )]);
    commit(model, &set).map_err(EditError::Commit)
}

// ───────────────────────── 测试 ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_markdown_changes_text() {
        let mut m = ProseModel::new();
        m.insert("p1", "hello");
        let inv = m
            .apply_op(
                &Target::Node(NodeId("p1".to_string())),
                &Op::SetMarkdown { md: "world".to_string() },
            )
            .unwrap();
        assert_eq!(m.get_text(&NodeId("p1".to_string())), Some("world"));
        // 逆 op 带回了旧文本
        assert_eq!(inv, Op::SetMarkdown { md: "hello".to_string() });
    }

    #[test]
    fn inverse_roundtrips() {
        let mut m = ProseModel::new();
        m.insert("p1", "original");
        let id = NodeId("p1".to_string());

        let inv = m
            .apply_op(&Target::Node(id.clone()), &Op::SetMarkdown { md: "edited".to_string() })
            .unwrap();
        assert_eq!(m.get_text(&id), Some("edited"));

        // 应用逆 op → 回到原文
        m.apply_op(&Target::Node(id.clone()), &inv).unwrap();
        assert_eq!(m.get_text(&id), Some("original"));
    }

    #[test]
    fn unknown_target_errors() {
        let mut m = ProseModel::new();
        let r = m.apply_op(
            &Target::Node(NodeId("ghost".to_string())),
            &Op::SetMarkdown { md: "x".to_string() },
        );
        assert_eq!(r, Err(PatchError::UnknownTarget(NodeId("ghost".to_string()))));
    }

    // ── Step 1：SetSpan / char-safe ──

    #[test]
    fn set_span_cjk_length_change_roundtrips() {
        let mut m = ProseModel::new();
        m.insert("p1", "你好世界"); // 4 char / 12 byte
        let id = NodeId("p1".to_string());

        // 把 [1,3) = "好世" 换成 "啊"（2 char → 1 char，长度变了）
        let inv = m
            .apply_op(
                &Target::Node(id.clone()),
                &Op::SetSpan { range: CharRange::chars(1, 3), text: "啊".to_string() },
            )
            .unwrap();
        assert_eq!(m.get_text(&id), Some("你啊界"));
        // 逆 op 的 range 跟着新长度走：[1,2)，并带回旧文本 "好世"
        assert_eq!(
            inv,
            Op::SetSpan { range: CharRange::chars(1, 2), text: "好世".to_string() }
        );

        // 应用逆 op → 精确还原
        m.apply_op(&Target::Node(id.clone()), &inv).unwrap();
        assert_eq!(m.get_text(&id), Some("你好世界"));
    }

    #[test]
    fn set_span_emoji_does_not_panic() {
        let mut m = ProseModel::new();
        m.insert("p1", "a😀b"); // '😀' 占 4 byte，裸 s[1..2] 会切坏 → panic
        let id = NodeId("p1".to_string());

        // 把 [1,2) = "😀" 换成 "🎉🎊"（1 char → 2 char）
        let inv = m
            .apply_op(
                &Target::Node(id.clone()),
                &Op::SetSpan { range: CharRange::chars(1, 2), text: "🎉🎊".to_string() },
            )
            .unwrap();
        assert_eq!(m.get_text(&id), Some("a🎉🎊b"));

        // roundtrip 回到带 emoji 的原文
        m.apply_op(&Target::Node(id.clone()), &inv).unwrap();
        assert_eq!(m.get_text(&id), Some("a😀b"));
    }

    #[test]
    fn set_span_insert_at_end() {
        // 空区间 [len,len) = 纯插入，不删任何东西。
        let mut m = ProseModel::new();
        m.insert("p1", "你好"); // 2 char
        let id = NodeId("p1".to_string());

        let inv = m
            .apply_op(
                &Target::Node(id.clone()),
                &Op::SetSpan { range: CharRange::chars(2, 2), text: "世界".to_string() },
            )
            .unwrap();
        assert_eq!(m.get_text(&id), Some("你好世界"));
        // 逆 op 是删掉刚插入的 [2,4)
        assert_eq!(
            inv,
            Op::SetSpan { range: CharRange::chars(2, 4), text: String::new() }
        );

        m.apply_op(&Target::Node(id.clone()), &inv).unwrap();
        assert_eq!(m.get_text(&id), Some("你好"));
    }

    #[test]
    fn set_span_out_of_bounds_errors() {
        let mut m = ProseModel::new();
        m.insert("p1", "hi"); // 2 char
        let id = NodeId("p1".to_string());

        let bad = CharRange::chars(1, 5); // end 越界
        let r = m.apply_op(&Target::Node(id), &Op::SetSpan { range: bad, text: "x".to_string() });
        assert_eq!(r, Err(PatchError::RangeOutOfBounds(bad)));
    }

    // ── Step 2：结构 op（有序 + 父子 + 无损快照）──

    fn nid(s: &str) -> NodeId {
        NodeId(s.to_string())
    }
    fn child_ids(m: &ProseModel, id: &NodeId) -> Vec<String> {
        m.children(id).unwrap().iter().map(|n| n.0.clone()).collect()
    }

    #[test]
    fn insert_section_inverse_is_remove() {
        let mut m = ProseModel::new();
        let root = m.root().clone();

        let inv = m
            .apply_op(
                &Target::Node(root.clone()),
                &Op::InsertSection { parent: root.clone(), index: 0, id: nid("s1"), text: "S1".to_string() },
            )
            .unwrap();

        assert_eq!(m.get_text(&nid("s1")), Some("S1"));
        assert_eq!(child_ids(&m, &root), vec!["s1"]);
        // 逆 op 就是删掉它
        assert_eq!(inv, Op::RemoveNode { id: nid("s1") });

        // 应用逆 op → section 消失，回到空文档
        m.apply_op(&Target::Node(root.clone()), &inv).unwrap();
        assert_eq!(m.get_text(&nid("s1")), None);
        assert!(child_ids(&m, &root).is_empty());
    }

    #[test]
    fn insert_section_keeps_order() {
        let mut m = ProseModel::new();
        let root = m.root().clone();
        let ins = |i: usize, id: &str| Op::InsertSection {
            parent: NodeId("root".to_string()),
            index: i,
            id: NodeId(id.to_string()),
            text: id.to_uppercase(),
        };

        m.apply_op(&Target::Node(root.clone()), &ins(0, "s1")).unwrap(); // [s1]
        m.apply_op(&Target::Node(root.clone()), &ins(1, "s3")).unwrap(); // [s1, s3]
        m.apply_op(&Target::Node(root.clone()), &ins(1, "s2")).unwrap(); // [s1, s2, s3]

        assert_eq!(child_ids(&m, &root), vec!["s1", "s2", "s3"]);
    }

    #[test]
    fn remove_node_restores_children_losslessly() {
        let mut m = ProseModel::new();
        let root = m.root().clone();

        // 建一棵带子结构的 section：sec1 → [a, b]
        m.apply_op(
            &Target::Node(root.clone()),
            &Op::InsertSection { parent: root.clone(), index: 0, id: nid("sec1"), text: "S1".to_string() },
        )
        .unwrap();
        m.apply_op(
            &Target::Node(nid("sec1")),
            &Op::InsertSection { parent: nid("sec1"), index: 0, id: nid("a"), text: "A".to_string() },
        )
        .unwrap();
        m.apply_op(
            &Target::Node(nid("sec1")),
            &Op::InsertSection { parent: nid("sec1"), index: 1, id: nid("b"), text: "B".to_string() },
        )
        .unwrap();
        assert_eq!(child_ids(&m, &nid("sec1")), vec!["a", "b"]);

        // 删 sec1：逆 op 必须是含整棵子树快照的 CreateNode
        let inv = m
            .apply_op(&Target::Node(nid("sec1")), &Op::RemoveNode { id: nid("sec1") })
            .unwrap();
        let expected = Op::CreateNode {
            parent: root.clone(),
            index: 0,
            snapshot: NodeSnapshot {
                id: nid("sec1"),
                kind: NodeKind::Paragraph,
                text: "S1".to_string(),
                children: vec![
                    NodeSnapshot { id: nid("a"), kind: NodeKind::Paragraph, text: "A".to_string(), children: vec![] },
                    NodeSnapshot { id: nid("b"), kind: NodeKind::Paragraph, text: "B".to_string(), children: vec![] },
                ],
            },
        };
        assert_eq!(inv, expected);

        // 删干净了：子树节点全没，root 也空
        assert_eq!(m.get_text(&nid("sec1")), None);
        assert_eq!(m.get_text(&nid("a")), None);
        assert_eq!(m.get_text(&nid("b")), None);
        assert!(child_ids(&m, &root).is_empty());

        // 应用逆 op → 整棵子树（含 children）无损还原
        m.apply_op(&Target::Node(root.clone()), &inv).unwrap();
        assert_eq!(m.get_text(&nid("sec1")), Some("S1"));
        assert_eq!(m.get_text(&nid("a")), Some("A"));
        assert_eq!(m.get_text(&nid("b")), Some("B"));
        assert_eq!(child_ids(&m, &root), vec!["sec1"]);
        assert_eq!(child_ids(&m, &nid("sec1")), vec!["a", "b"]);
    }

    #[test]
    fn structural_errors_do_not_panic() {
        let mut m = ProseModel::new();
        let root = m.root().clone();

        // 删 root：没有父 → NotRemovable
        assert_eq!(
            m.apply_op(&Target::Node(root.clone()), &Op::RemoveNode { id: root.clone() }),
            Err(PatchError::NotRemovable(root.clone()))
        );
        // 删不存在的节点 → UnknownTarget
        assert_eq!(
            m.apply_op(&Target::Node(nid("ghost")), &Op::RemoveNode { id: nid("ghost") }),
            Err(PatchError::UnknownTarget(nid("ghost")))
        );
        // 往不存在的父插入 → UnknownTarget
        assert_eq!(
            m.apply_op(
                &Target::Node(nid("ghost")),
                &Op::InsertSection { parent: nid("ghost"), index: 0, id: nid("x"), text: String::new() }
            ),
            Err(PatchError::UnknownTarget(nid("ghost")))
        );
        // index 越界 → IndexOutOfBounds（root 当前 0 个孩子）
        assert_eq!(
            m.apply_op(
                &Target::Node(root.clone()),
                &Op::InsertSection { parent: root.clone(), index: 5, id: nid("x"), text: String::new() }
            ),
            Err(PatchError::IndexOutOfBounds { parent: root.clone(), index: 5, len: 0 })
        );
    }

    #[test]
    fn insert_duplicate_id_errors() {
        let mut m = ProseModel::new();
        let root = m.root().clone();
        m.apply_op(
            &Target::Node(root.clone()),
            &Op::InsertSection { parent: root.clone(), index: 0, id: nid("dup"), text: String::new() },
        )
        .unwrap();
        assert_eq!(
            m.apply_op(
                &Target::Node(root.clone()),
                &Op::InsertSection { parent: root.clone(), index: 0, id: nid("dup"), text: String::new() }
            ),
            Err(PatchError::DuplicateNode(nid("dup")))
        );
    }

    // ── Step 3：PatchSet + topo_order（纯排序逻辑）──

    fn patch(id: &str, deps: &[&str]) -> Patch {
        Patch::authored(id, Target::Node(nid("n")), Op::SetMarkdown { md: String::new() }).with_deps(deps)
    }
    fn ids(order: &[&Patch]) -> Vec<String> {
        order.iter().map(|q| q.id.0.clone()).collect()
    }

    #[test]
    fn topo_orders_dependency_chain() {
        // 输入乱序：c 依赖 b，b 依赖 a
        let set = PatchSet::new(vec![patch("c", &["b"]), patch("a", &[]), patch("b", &["a"])]);
        assert_eq!(ids(&set.topo_order().unwrap()), vec!["a", "b", "c"]);
    }

    #[test]
    fn topo_preserves_order_without_deps() {
        let set = PatchSet::new(vec![patch("x", &[]), patch("y", &[]), patch("z", &[])]);
        assert_eq!(ids(&set.topo_order().unwrap()), vec!["x", "y", "z"]);
    }

    #[test]
    fn topo_moves_dependent_after_dependency() {
        // 输入 [d(依赖 a), a] → 必须把 d 排到 a 之后
        let set = PatchSet::new(vec![patch("d", &["a"]), patch("a", &[])]);
        assert_eq!(ids(&set.topo_order().unwrap()), vec!["a", "d"]);
    }

    #[test]
    fn topo_is_stable_with_partial_deps() {
        // a,b,c 无依赖保持原序；d 仅依赖 b，仍排到末尾（每步取原序最靠前的就绪者）
        let set = PatchSet::new(vec![patch("a", &[]), patch("b", &[]), patch("c", &[]), patch("d", &["b"])]);
        assert_eq!(ids(&set.topo_order().unwrap()), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn topo_detects_cycle() {
        let set = PatchSet::new(vec![patch("a", &["b"]), patch("b", &["a"])]);
        assert_eq!(
            set.topo_order().unwrap_err(),
            TopoError::Cycle(vec![PatchId("a".to_string()), PatchId("b".to_string())])
        );
    }

    #[test]
    fn topo_rejects_dangling_dependency() {
        let set = PatchSet::new(vec![patch("a", &["ghost"])]);
        assert_eq!(
            set.topo_order().unwrap_err(),
            TopoError::DanglingDependency {
                patch: PatchId("a".to_string()),
                missing: PatchId("ghost".to_string()),
            }
        );
    }

    #[test]
    fn topo_rejects_duplicate_id() {
        let set = PatchSet::new(vec![patch("a", &[]), patch("a", &[])]);
        assert_eq!(set.topo_order().unwrap_err(), TopoError::DuplicateId(PatchId("a".to_string())));
    }

    // ── Step 4：derive 纯函数 + assemble 时序 ──

    /// 建一个带 h1 / toc 节点、已注册 toc 的模型。
    fn model_with_toc() -> ProseModel {
        let mut m = ProseModel::new();
        m.insert("h1", "");
        m.insert("toc", "");
        m.set_toc("toc");
        m
    }

    #[test]
    fn derive_does_not_mutate_model() {
        let mut m = model_with_toc();
        // 把 h1 设成 typed heading 并改标题（apply 会顶 rev）
        m.apply_op(&Target::Node(nid("h1")), &Op::SetKind { kind: NodeKind::Heading { level: 1 } }).unwrap();
        m.apply_op(&Target::Node(nid("h1")), &Op::SetMarkdown { md: "A".to_string() }).unwrap();

        let authored =
            vec![Patch::authored("p1", Target::Node(nid("h1")), Op::SetMarkdown { md: "A".to_string() })];
        let before = m.rev();
        let _ = m.derive(&authored);
        // derive 只读 → rev 不变
        assert_eq!(m.rev(), before);
    }

    #[test]
    fn derive_reads_post_apply_state_and_links_deps() {
        let mut m = model_with_toc();
        // h1 是 typed heading（setup，不在 authored 里）
        m.apply_op(&Target::Node(nid("h1")), &Op::SetKind { kind: NodeKind::Heading { level: 1 } }).unwrap();
        let authored = vec![Patch::authored(
            "p1",
            Target::Node(nid("h1")),
            Op::SetMarkdown { md: "New Section".to_string() },
        )];
        // 时序：先 apply authored，再 derive
        m.apply_op(&authored[0].target, &authored[0].op).unwrap();

        let derived = m.derive(&authored);
        assert_eq!(derived.len(), 1);
        // derive 读到的是 apply 后的新章节标题
        match &derived[0].op {
            Op::SetMarkdown { md } => assert_eq!(md.as_str(), "- New Section"),
            other => panic!("unexpected derived op: {other:?}"),
        }
        // depends_on 链：派生的 TOC patch 依赖改 heading 的 authored patch
        assert_eq!(derived[0].depends_on, vec![PatchId("p1".to_string())]);
        // source 标记 Derived 且 from 链到 p1
        match &derived[0].source {
            PatchSource::Derived { from } => assert_eq!(from, &PatchId("p1".to_string())),
            other => panic!("expected Derived, got {other:?}"),
        }
    }

    #[test]
    fn derive_links_all_changed_headings_in_order() {
        let mut m = ProseModel::new();
        m.insert("h1", "");
        m.insert("h2", "");
        m.insert("toc", "");
        m.set_toc("toc");
        m.apply_op(&Target::Node(nid("h1")), &Op::SetKind { kind: NodeKind::Heading { level: 1 } }).unwrap();
        m.apply_op(&Target::Node(nid("h2")), &Op::SetKind { kind: NodeKind::Heading { level: 1 } }).unwrap();

        let authored = vec![
            Patch::authored("p1", Target::Node(nid("h1")), Op::SetMarkdown { md: "One".to_string() }),
            Patch::authored("p2", Target::Node(nid("h2")), Op::SetMarkdown { md: "Two".to_string() }),
        ];
        for p in &authored {
            m.apply_op(&p.target, &p.op).unwrap();
        }
        let derived = m.derive(&authored);
        assert_eq!(derived.len(), 1);
        // 文档序 TOC 覆盖两个 heading
        match &derived[0].op {
            Op::SetMarkdown { md } => assert_eq!(md.as_str(), "- One\n- Two"),
            other => panic!("unexpected: {other:?}"),
        }
        // depends_on 链覆盖两个 authored patch（按原序）
        assert_eq!(derived[0].depends_on, vec![PatchId("p1".to_string()), PatchId("p2".to_string())]);
    }

    #[test]
    fn assemble_applies_authored_then_derives_in_topo_order() {
        let mut m = model_with_toc();
        // h1 是 typed heading（setup）
        m.apply_op(&Target::Node(nid("h1")), &Op::SetKind { kind: NodeKind::Heading { level: 1 } }).unwrap();
        let authored = vec![Patch::authored(
            "p_h1",
            Target::Node(nid("h1")),
            Op::SetMarkdown { md: "Intro".to_string() },
        )];

        let set = assemble(&mut m, authored).unwrap();

        // authored 已 apply 到 model（缓冲），文本干净无 `#`
        assert_eq!(m.get_text(&nid("h1")), Some("Intro"));
        // 打包结果：authored 在前、派生 TOC 在后（它 depends_on p_h1）
        assert_eq!(set.patches.len(), 2);
        assert_eq!(set.patches[0].id, PatchId("p_h1".to_string()));
        let toc = &set.patches[1];
        assert_eq!(toc.target, Target::Node(nid("toc")));
        assert_eq!(toc.depends_on, vec![PatchId("p_h1".to_string())]);
        match &toc.op {
            Op::SetMarkdown { md } => assert_eq!(md.as_str(), "- Intro"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn derive_empty_without_toc_or_heading_change() {
        // 1) 没注册 toc → 空（即便 h1 是 heading 且被改）
        let mut m = ProseModel::new();
        m.insert("h1", "A");
        m.apply_op(&Target::Node(nid("h1")), &Op::SetKind { kind: NodeKind::Heading { level: 1 } }).unwrap();
        let authored =
            vec![Patch::authored("p1", Target::Node(nid("h1")), Op::SetMarkdown { md: "A".to_string() })];
        assert!(m.derive(&authored).is_empty());

        // 2) 注册了 toc、文档里也有 heading，但 authored 改的是普通段落 → 不更新 TOC
        let mut m2 = model_with_toc();
        m2.apply_op(&Target::Node(nid("h1")), &Op::SetKind { kind: NodeKind::Heading { level: 1 } }).unwrap();
        m2.insert("para", "plain");
        let authored2 = vec![Patch::authored(
            "p2",
            Target::Node(nid("para")),
            Op::SetMarkdown { md: "still plain".to_string() },
        )];
        m2.apply_op(&authored2[0].target, &authored2[0].op).unwrap();
        assert!(m2.derive(&authored2).is_empty());
    }

    // ── Step 5：commit + undo（事务）──

    #[test]
    fn commit_applies_all_atomically() {
        let mut m = ProseModel::new();
        m.insert("a", "A0");
        m.insert("b", "B0");

        // p2 依赖 p1 → 拓扑序 [p1, p2]
        let set = PatchSet::new(vec![
            Patch::authored("p1", Target::Node(nid("a")), Op::SetMarkdown { md: "A1".to_string() }),
            Patch::authored("p2", Target::Node(nid("b")), Op::SetMarkdown { md: "B1".to_string() })
                .with_deps(&["p1"]),
        ]);
        let inverse = commit(&mut m, &set).unwrap();

        // 全部 apply
        assert_eq!(m.get_text(&nid("a")), Some("A1"));
        assert_eq!(m.get_text(&nid("b")), Some("B1"));

        // 逆 set 按 undo 顺序（apply 逆序）：[inv(p2)→b=B0, inv(p1)→a=A0]
        assert_eq!(inverse.patches.len(), 2);
        assert_eq!(inverse.patches[0].target, Target::Node(nid("b")));
        assert_eq!(inverse.patches[1].target, Target::Node(nid("a")));
        match &inverse.patches[0].op {
            Op::SetMarkdown { md } => assert_eq!(md.as_str(), "B0"),
            other => panic!("unexpected: {other:?}"),
        }
        match &inverse.patches[1].op {
            Op::SetMarkdown { md } => assert_eq!(md.as_str(), "A0"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn commit_rolls_back_on_mid_failure() {
        let mut m = ProseModel::new();
        m.insert("a", "A0"); // 没有 "ghost" 节点

        let set = PatchSet::new(vec![
            Patch::authored("p1", Target::Node(nid("a")), Op::SetMarkdown { md: "A1".to_string() }),
            Patch::authored("p2", Target::Node(nid("ghost")), Op::SetMarkdown { md: "X".to_string() })
                .with_deps(&["p1"]),
        ]);
        let err = commit(&mut m, &set).unwrap_err();

        // p2 失败 → 整组回滚：p1 对 a 的改动被撤销
        assert_eq!(m.get_text(&nid("a")), Some("A0"));
        assert_eq!(
            err,
            CommitError::Aborted {
                patch: PatchId("p2".to_string()),
                cause: PatchError::UnknownTarget(nid("ghost")),
            }
        );
    }

    #[test]
    fn undo_restores_whole_group() {
        let mut m = ProseModel::new();
        m.insert("a", "A0");
        m.insert("b", "B0");

        let set = PatchSet::new(vec![
            Patch::authored("p1", Target::Node(nid("a")), Op::SetMarkdown { md: "A1".to_string() }),
            Patch::authored("p2", Target::Node(nid("b")), Op::SetMarkdown { md: "B1".to_string() })
                .with_deps(&["p1"]),
        ]);
        let inverse = commit(&mut m, &set).unwrap();
        assert_eq!(m.get_text(&nid("a")), Some("A1"));
        assert_eq!(m.get_text(&nid("b")), Some("B1"));

        undo(&mut m, &inverse).unwrap();
        // 整组回滚到提交前
        assert_eq!(m.get_text(&nid("a")), Some("A0"));
        assert_eq!(m.get_text(&nid("b")), Some("B0"));
    }

    #[test]
    fn commit_aborts_on_cycle_without_touching_model() {
        let mut m = ProseModel::new();
        m.insert("a", "A0");
        let set = PatchSet::new(vec![
            Patch::authored("p1", Target::Node(nid("a")), Op::SetMarkdown { md: "X".to_string() })
                .with_deps(&["p2"]),
            Patch::authored("p2", Target::Node(nid("a")), Op::SetMarkdown { md: "Y".to_string() })
                .with_deps(&["p1"]),
        ]);
        let err = commit(&mut m, &set).unwrap_err();
        assert!(matches!(err, CommitError::Topo(TopoError::Cycle(_))));
        // 拓扑失败发生在任何 apply 之前 → model 完全没动
        assert_eq!(m.get_text(&nid("a")), Some("A0"));
        assert_eq!(m.rev(), 0);
    }

    #[test]
    fn commit_and_undo_structural_op() {
        let mut m = ProseModel::new();
        let root = m.root().clone();
        let set = PatchSet::new(vec![Patch::authored(
            "p1",
            Target::Node(root.clone()),
            Op::InsertSection { parent: root.clone(), index: 0, id: nid("s1"), text: "S1".to_string() },
        )]);
        let inverse = commit(&mut m, &set).unwrap();
        assert_eq!(m.get_text(&nid("s1")), Some("S1"));
        assert_eq!(child_ids(&m, &root), vec!["s1"]);

        undo(&mut m, &inverse).unwrap();
        // 结构改动被整组撤销
        assert_eq!(m.get_text(&nid("s1")), None);
        assert!(child_ids(&m, &root).is_empty());
    }

    // ── Step 6：Renderer + SourceMap（出口，不碰 docx）──

    /// 用 SourceSpan 的 range 把节点纯文本切回来（验证反查正确）。
    fn slice_chars(text: &str, range: CharRange) -> String {
        let (b0, b1) = CharIndex::byte_range(text, &range).unwrap();
        text[b0..b1].to_string()
    }

    #[test]
    fn renders_heading_and_paragraph_with_sourcemap() {
        let mut m = ProseModel::new();
        let root = m.root().clone();
        m.apply_op(
            &Target::Node(root.clone()),
            &Op::InsertSection { parent: root.clone(), index: 0, id: nid("h"), text: "Title".to_string() },
        )
        .unwrap();
        m.apply_op(&Target::Node(nid("h")), &Op::SetKind { kind: NodeKind::Heading { level: 1 } }).unwrap();
        m.apply_op(
            &Target::Node(root.clone()),
            &Op::InsertSection { parent: root.clone(), index: 1, id: nid("p"), text: "Hello".to_string() },
        )
        .unwrap();

        let r = render_html(&m);
        assert_eq!(r.html, "<h1 data-node=\"h\">Title</h1><p data-node=\"p\">Hello</p>");

        // 两条片段，能反查回 node + range；标题不再含 `#`，range 即整段
        assert_eq!(r.source_map.spans.len(), 2);
        let sh = r.source_map.span_for(&nid("h")).unwrap();
        assert_eq!(sh.range, CharRange::chars(0, 5)); // "Title"
        let sp = r.source_map.span_for(&nid("p")).unwrap();
        assert_eq!(sp.range, CharRange::chars(0, 5));
    }

    #[test]
    fn cjk_node_range_is_correct_and_reverse_maps() {
        let mut m = ProseModel::new();
        let root = m.root().clone();
        m.apply_op(
            &Target::Node(root.clone()),
            &Op::InsertSection { parent: root.clone(), index: 0, id: nid("h"), text: "标题".to_string() },
        )
        .unwrap();
        m.apply_op(&Target::Node(nid("h")), &Op::SetKind { kind: NodeKind::Heading { level: 1 } }).unwrap();
        m.apply_op(
            &Target::Node(root.clone()),
            &Op::InsertSection { parent: root.clone(), index: 1, id: nid("p"), text: "你好世界".to_string() },
        )
        .unwrap();

        let r = render_html(&m);
        assert_eq!(r.html, "<h1 data-node=\"h\">标题</h1><p data-node=\"p\">你好世界</p>");

        // 含中文的 range 正确，且 range 切回纯文本 == 渲染出的片段
        let sh = r.source_map.span_for(&nid("h")).unwrap();
        assert_eq!(sh.range, CharRange::chars(0, 2));
        assert_eq!(slice_chars(m.get_text(&nid("h")).unwrap(), sh.range), "标题");
        let sp = r.source_map.span_for(&nid("p")).unwrap();
        assert_eq!(sp.range, CharRange::chars(0, 4));
        assert_eq!(slice_chars(m.get_text(&nid("p")).unwrap(), sp.range), "你好世界");
    }

    #[test]
    fn renders_nested_in_document_order_and_escapes() {
        let mut m = ProseModel::new();
        let root = m.root().clone();
        m.apply_op(
            &Target::Node(root.clone()),
            &Op::InsertSection { parent: root.clone(), index: 0, id: nid("sec"), text: "Sec".to_string() },
        )
        .unwrap();
        m.apply_op(&Target::Node(nid("sec")), &Op::SetKind { kind: NodeKind::Heading { level: 2 } }).unwrap();
        m.apply_op(
            &Target::Node(nid("sec")),
            &Op::InsertSection { parent: nid("sec"), index: 0, id: nid("kid"), text: "a < b & c".to_string() },
        )
        .unwrap();

        let r = render_html(&m);
        // 文档序：sec 的 heading 先、其子 kid 段落后；level=2 → h2；特殊字符转义
        assert_eq!(r.html, "<h2 data-node=\"sec\">Sec</h2><p data-node=\"kid\">a &lt; b &amp; c</p>");
        assert_eq!(r.source_map.spans.len(), 2);
        assert_eq!(r.source_map.spans[0].node, nid("sec"));
        assert_eq!(r.source_map.spans[1].node, nid("kid"));
        // 转义不影响 range：range 是纯文本坐标
        assert_eq!(r.source_map.spans[1].range, CharRange::chars(0, 9));
        assert_eq!(slice_chars(m.get_text(&nid("kid")).unwrap(), r.source_map.spans[1].range), "a < b & c");
    }

    // ── Step 8a/8b：typed NodeKind / SetKind（renderer/derive/import 已改用 kind）──

    #[test]
    fn hash_in_text_is_literal_not_a_heading() {
        // 收债验证：`#` 现在是普通字符。不设 kind 的节点即便文本以 `#` 开头也渲成 <p>，`#` 原样显示。
        let mut m = ProseModel::new();
        let root = m.root().clone();
        m.apply_op(
            &Target::Node(root.clone()),
            &Op::InsertSection { parent: root.clone(), index: 0, id: nid("p"), text: "# not a heading".to_string() },
        )
        .unwrap();
        let r = render_html(&m);
        assert_eq!(r.html, "<p data-node=\"p\"># not a heading</p>");
    }

    #[test]
    fn set_kind_changes_and_inverse_roundtrips() {
        let mut m = ProseModel::new();
        m.insert("p1", "Intro");
        let id = nid("p1");
        assert_eq!(m.node_kind(&id), Some(NodeKind::Paragraph)); // 默认段落

        let inv = m
            .apply_op(&Target::Node(id.clone()), &Op::SetKind { kind: NodeKind::Heading { level: 2 } })
            .unwrap();
        assert_eq!(m.node_kind(&id), Some(NodeKind::Heading { level: 2 }));
        assert_eq!(inv, Op::SetKind { kind: NodeKind::Paragraph }); // 逆 op 带回旧 kind

        m.apply_op(&Target::Node(id.clone()), &inv).unwrap();
        assert_eq!(m.node_kind(&id), Some(NodeKind::Paragraph));
    }

    #[test]
    fn remove_restore_preserves_kind() {
        let mut m = ProseModel::new();
        let root = m.root().clone();
        m.apply_op(
            &Target::Node(root.clone()),
            &Op::InsertSection { parent: root.clone(), index: 0, id: nid("h"), text: "Title".to_string() },
        )
        .unwrap();
        m.apply_op(&Target::Node(nid("h")), &Op::SetKind { kind: NodeKind::Heading { level: 1 } }).unwrap();
        assert_eq!(m.node_kind(&nid("h")), Some(NodeKind::Heading { level: 1 }));

        // 删除 → 逆 op（含快照）重建，kind 必须无损还原
        let inv = m.apply_op(&Target::Node(nid("h")), &Op::RemoveNode { id: nid("h") }).unwrap();
        assert_eq!(m.node_kind(&nid("h")), None);
        m.apply_op(&Target::Node(root.clone()), &inv).unwrap();
        assert_eq!(m.node_kind(&nid("h")), Some(NodeKind::Heading { level: 1 }));
    }

    // ── Step 9：SourceMap 编辑回写（看 → 改 → 再看 → 撤销）──

    #[test]
    fn edit_writeback_roundtrip_and_undo() {
        let mut m = ProseModel::new();
        let root = m.root().clone();
        m.apply_op(
            &Target::Node(root.clone()),
            &Op::InsertSection { parent: root.clone(), index: 0, id: nid("p1"), text: "Hello".to_string() },
        )
        .unwrap();
        let r = render_html(&m);
        assert_eq!(r.html, "<p data-node=\"p1\">Hello</p>");

        // 经 SourceMap 反查把 p1 整段改写成 "World!"
        let inv = apply_fragment_edit(&mut m, &r.source_map, &nid("p1"), "World!").unwrap();
        assert_eq!(m.get_text(&nid("p1")), Some("World!"));
        // 再渲染 → 看见新内容
        assert_eq!(render_html(&m).html, "<p data-node=\"p1\">World!</p>");
        // undo → 回到 "Hello"
        undo(&mut m, &inv).unwrap();
        assert_eq!(m.get_text(&nid("p1")), Some("Hello"));
    }

    #[test]
    fn edit_writeback_cjk_heading_keeps_kind() {
        let mut m = ProseModel::new();
        let root = m.root().clone();
        m.apply_op(
            &Target::Node(root.clone()),
            &Op::InsertSection { parent: root.clone(), index: 0, id: nid("h"), text: "标题".to_string() },
        )
        .unwrap();
        m.apply_op(&Target::Node(nid("h")), &Op::SetKind { kind: NodeKind::Heading { level: 1 } }).unwrap();
        let r = render_html(&m);

        // 改 heading 文本（CJK）→ 仍是 heading（SetSpan 只动文本，不动 typed kind）
        apply_fragment_edit(&mut m, &r.source_map, &nid("h"), "新标题啊").unwrap();
        assert_eq!(m.get_text(&nid("h")), Some("新标题啊"));
        assert_eq!(m.node_kind(&nid("h")), Some(NodeKind::Heading { level: 1 }));
        assert_eq!(render_html(&m).html, "<h1 data-node=\"h\">新标题啊</h1>");
    }

    #[test]
    fn edit_writeback_unknown_node_errors() {
        let mut m = ProseModel::new();
        let r = render_html(&m); // 空文档 → 空 SourceMap
        assert_eq!(
            apply_fragment_edit(&mut m, &r.source_map, &nid("ghost"), "x").unwrap_err(),
            EditError::NoSpan(nid("ghost"))
        );
    }

    // ── Step 14：Opaque 节点（typed 不透明内容，只读）──

    #[test]
    fn set_kind_opaque_roundtrips() {
        let mut m = ProseModel::new();
        m.insert("o", "表格描述");
        let inv = m.apply_op(&Target::Node(nid("o")), &Op::SetKind { kind: NodeKind::Opaque }).unwrap();
        assert_eq!(m.node_kind(&nid("o")), Some(NodeKind::Opaque));
        // 逆 op 带回旧 kind
        assert_eq!(inv, Op::SetKind { kind: NodeKind::Paragraph });
        m.apply_op(&Target::Node(nid("o")), &inv).unwrap();
        assert_eq!(m.node_kind(&nid("o")), Some(NodeKind::Paragraph));
    }

    #[test]
    fn opaque_renders_visible_but_not_editable() {
        let mut m = ProseModel::new();
        m.insert("p1", "正文");
        m.insert("o1", "[Table: 1 rows]");
        m.apply_op(&Target::Node(nid("o1")), &Op::SetKind { kind: NodeKind::Opaque }).unwrap();

        let r = render_html(&m);
        // 可见：渲染成 <pre>（含中文也安全）
        assert_eq!(
            r.html,
            "<p data-node=\"p1\">正文</p><pre data-node=\"o1\">[Table: 1 rows]</pre>"
        );
        // 只读：SourceMap 里没有 o1 的片段（正文有），「看 → 改」回路对它断开
        assert!(r.source_map.span_for(&nid("p1")).is_some());
        assert!(r.source_map.span_for(&nid("o1")).is_none());
        assert_eq!(
            apply_fragment_edit(&mut m, &r.source_map, &nid("o1"), "妄图改它").unwrap_err(),
            EditError::NoSpan(nid("o1"))
        );
        // 模型一个字没动
        assert_eq!(m.get_text(&nid("o1")), Some("[Table: 1 rows]"));
    }

    // ── Step 17：AI patch 信任闸口（原则 7：Human 直达，AI 过 review）──

    #[test]
    fn unreviewed_ai_patch_is_rejected_atomically() {
        let mut m = ProseModel::new();
        m.insert("p1", "原文");
        let set = PatchSet::new(vec![Patch::ai(
            "ai1",
            Target::Node(nid("p1")),
            Op::SetMarkdown { md: "AI 改写".to_string() },
        )]);
        // 未 review → commit 拒绝，model 一字未动
        assert_eq!(commit(&mut m, &set).unwrap_err(), CommitError::UnreviewedAi(PatchId("ai1".to_string())));
        assert_eq!(m.get_text(&nid("p1")), Some("原文"));
        // Human patch 同一条路直达（不对称的另一半）
        let human = PatchSet::new(vec![Patch::authored(
            "h1",
            Target::Node(nid("p1")),
            Op::SetMarkdown { md: "人改的".to_string() },
        )]);
        assert!(commit(&mut m, &human).is_ok());
        assert_eq!(m.get_text(&nid("p1")), Some("人改的"));
    }

    #[test]
    fn approved_ai_patch_commits_and_undo_restores() {
        let mut m = ProseModel::new();
        m.insert("p1", "原文");
        let set = PatchSet::new(vec![Patch::ai(
            "ai1",
            Target::Node(nid("p1")),
            Op::SetMarkdown { md: "AI 改写".to_string() },
        )])
        .approve_ai(); // 显式 review 动作
        let inverse = commit(&mut m, &set).unwrap();
        assert_eq!(m.get_text(&nid("p1")), Some("AI 改写"));
        // 撤销是人的动作：逆 PatchSet 是 Authored，不需要再 review
        undo(&mut m, &inverse).unwrap();
        assert_eq!(m.get_text(&nid("p1")), Some("原文"));
    }

    #[test]
    fn mixed_set_with_unreviewed_ai_rejects_whole_group() {
        let mut m = ProseModel::new();
        m.insert("p1", "甲");
        m.insert("p2", "乙");
        let set = PatchSet::new(vec![
            Patch::authored("h1", Target::Node(nid("p1")), Op::SetMarkdown { md: "人改".to_string() }),
            Patch::ai("ai1", Target::Node(nid("p2")), Op::SetMarkdown { md: "AI改".to_string() }),
        ]);
        // 原子性：human 那条也不能溜进去
        assert!(matches!(commit(&mut m, &set), Err(CommitError::UnreviewedAi(_))));
        assert_eq!(m.get_text(&nid("p1")), Some("甲"));
        assert_eq!(m.get_text(&nid("p2")), Some("乙"));
    }

    #[test]
    fn remove_restore_preserves_opaque_kind() {
        let mut m = ProseModel::new();
        let root = m.root().clone();
        m.apply_op(
            &Target::Node(root.clone()),
            &Op::InsertSection { parent: root.clone(), index: 0, id: nid("o"), text: "图片占位".to_string() },
        )
        .unwrap();
        m.apply_op(&Target::Node(nid("o")), &Op::SetKind { kind: NodeKind::Opaque }).unwrap();

        // 删除 → 逆 op（CreateNode 快照）→ 还原后 kind 仍是 Opaque（快照含 kind 的验收）
        let inv = m.apply_op(&Target::Node(nid("o")), &Op::RemoveNode { id: nid("o") }).unwrap();
        assert_eq!(m.node_kind(&nid("o")), None);
        m.apply_op(&Target::Node(root), &inv).unwrap();
        assert_eq!(m.node_kind(&nid("o")), Some(NodeKind::Opaque));
        assert_eq!(m.get_text(&nid("o")), Some("图片占位"));
    }
}
