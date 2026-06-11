//! 文件级版本历史：每次写盘前把原文件快照进同目录侧挂的 `<文件名>.versions/`，
//! 可回到任意旧版；回版/写盘前先留底当前状态，但**按内容去重**——
//! 已在历史里的状态不重复留（反复回版不灌水），任何曾落过盘的状态都能找回。
//!
//! 这是对「写盘」这个**外部不可逆 effect** 的补偿机制（总纲原则 3：内部走逆 op，
//! 文件层走快照）。纯 `std::fs`，不碰子进程、不碰 OOXML，对任意扩展名的文件都成立。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 一个历史版本：快照文件路径 + 拍摄时刻（epoch 毫秒，从文件名解出）。
#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    pub path: PathBuf,
    pub millis: u64,
}

/// 历史目录：`/a/b/c.docx` → `/a/b/c.docx.versions/`（同目录、Finder 可见、跟着文档走）。
pub fn history_dir(file: &Path) -> PathBuf {
    let mut name = file.file_name().unwrap_or_default().to_os_string();
    name.push(".versions");
    file.with_file_name(name)
}

/// 把 `file` 当前内容快照进历史目录，返回快照路径。
/// 文件名 = 13 位零填充 epoch 毫秒（字典序 = 时间序）+ 原扩展名；
/// 同名已存在则毫秒 +1 直到空位——**绝不覆盖已有版本**。
pub fn snapshot(file: &Path) -> Result<PathBuf, String> {
    let dir = history_dir(file);
    fs::create_dir_all(&dir).map_err(|e| format!("建历史目录 {} 失败: {e}", dir.display()))?;
    let ext = file
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let mut millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("取系统时间失败: {e}"))?
        .as_millis() as u64;
    let dest = loop {
        let candidate = dir.join(format!("{millis:013}{ext}"));
        if !candidate.exists() {
            break candidate;
        }
        millis += 1;
    };
    fs::copy(file, &dest).map_err(|e| format!("快照 {} 失败: {e}", file.display()))?;
    Ok(dest)
}

/// 列出 `file` 的全部历史版本，**从新到旧**。历史目录不存在 → 空列表（不是错）。
/// 文件名解析不出毫秒数的东西（用户手放的文件）一律跳过，不弄炸列表。
pub fn list_versions(file: &Path) -> Result<Vec<Version>, String> {
    let dir = history_dir(file);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let entries =
        fs::read_dir(&dir).map_err(|e| format!("读历史目录 {} 失败: {e}", dir.display()))?;
    let mut versions: Vec<Version> = entries
        .filter_map(|e| {
            let path = e.ok()?.path();
            if !path.is_file() {
                return None;
            }
            let millis: u64 = path.file_stem()?.to_str()?.parse().ok()?;
            Some(Version { path, millis })
        })
        .collect();
    versions.sort_by(|a, b| b.millis.cmp(&a.millis));
    Ok(versions)
}

/// 仅当 `file` 当前内容**不在历史里**（按字节比对）时才快照，返回快照路径；已有同内容版本 → `None`。
/// 历史因此是「磁盘上出现过的互异状态」的集合：什么都不丢，也不重复。
pub fn snapshot_if_new(file: &Path) -> Result<Option<PathBuf>, String> {
    let current = fs::read(file).map_err(|e| format!("读 {} 失败: {e}", file.display()))?;
    for v in list_versions(file)? {
        // 先比大小再比内容，省掉绝大多数整读。
        let same_len = fs::metadata(&v.path).map(|m| m.len() == current.len() as u64).unwrap_or(false);
        if same_len && fs::read(&v.path).map(|b| b == current).unwrap_or(false) {
            return Ok(None);
        }
    }
    snapshot(file).map(Some)
}

/// 回到某个旧版：当前状态若未入历史则先留底（回版这个动作本身也能再回退），再用 `version` 覆盖 `file`。
/// 返回留底的快照路径；当前状态已在历史里（如连续回版）→ `None`，不重复留。
pub fn restore(file: &Path, version: &Path) -> Result<Option<PathBuf>, String> {
    if !version.is_file() {
        return Err(format!("版本不存在: {}", version.display()));
    }
    let snapped = snapshot_if_new(file)?;
    fs::copy(version, file).map_err(|e| format!("回版失败: {e}"))?;
    Ok(snapped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    /// 每个测试一个独立临时目录（进程 id + 计数器，互不相踩；残留靠系统 tmp 清理）。
    fn temp_doc(content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "polaris-history-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("doc.docx");
        fs::write(&file, content).unwrap();
        file
    }

    #[test]
    fn history_dir_is_sibling_of_file() {
        assert_eq!(
            history_dir(Path::new("/a/b/c.docx")),
            PathBuf::from("/a/b/c.docx.versions")
        );
    }

    #[test]
    fn snapshot_copies_current_content_and_keeps_original() {
        let f = temp_doc("V1");
        let snap = snapshot(&f).unwrap();
        assert!(snap.starts_with(history_dir(&f)));
        assert_eq!(fs::read_to_string(&snap).unwrap(), "V1");
        assert_eq!(fs::read_to_string(&f).unwrap(), "V1"); // 原文件没动
    }

    #[test]
    fn repeated_snapshots_never_overwrite() {
        let f = temp_doc("V1");
        let s1 = snapshot(&f).unwrap();
        fs::write(&f, "V2").unwrap();
        let s2 = snapshot(&f).unwrap(); // 大概率同毫秒 → 走 +1 找空位分支
        assert_ne!(s1, s2);
        assert_eq!(fs::read_to_string(&s1).unwrap(), "V1");
        assert_eq!(fs::read_to_string(&s2).unwrap(), "V2");
        assert_eq!(list_versions(&f).unwrap().len(), 2);
    }

    #[test]
    fn list_is_new_to_old_and_skips_foreign_files() {
        let f = temp_doc("V1");
        snapshot(&f).unwrap();
        fs::write(&f, "V2").unwrap();
        snapshot(&f).unwrap();
        fs::write(history_dir(&f).join("readme.txt"), "x").unwrap();
        let vs = list_versions(&f).unwrap();
        assert_eq!(vs.len(), 2); // readme.txt 被跳过
        assert!(vs[0].millis >= vs[1].millis);
        assert_eq!(fs::read_to_string(&vs[0].path).unwrap(), "V2"); // 最新在前
        assert_eq!(fs::read_to_string(&vs[1].path).unwrap(), "V1");
    }

    #[test]
    fn list_empty_without_history_dir() {
        let f = temp_doc("V1");
        assert!(list_versions(&f).unwrap().is_empty());
    }

    #[test]
    fn restore_brings_back_old_and_snapshots_pre_restore_state() {
        let f = temp_doc("V1");
        snapshot(&f).unwrap(); // 历史: [V1]
        fs::write(&f, "V2").unwrap();
        let oldest = list_versions(&f).unwrap().last().unwrap().path.clone();
        restore(&f, &oldest).unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "V1"); // 回到旧版
        let contents: Vec<String> = list_versions(&f)
            .unwrap()
            .iter()
            .map(|v| fs::read_to_string(&v.path).unwrap())
            .collect();
        assert!(contents.contains(&"V2".to_string())); // 回版前的 V2 也留了底
    }

    #[test]
    fn snapshot_if_new_skips_content_already_in_history() {
        let f = temp_doc("V1");
        assert!(snapshot_if_new(&f).unwrap().is_some()); // 新内容 → 留底
        assert!(snapshot_if_new(&f).unwrap().is_none()); // 同内容再来 → 跳过
        fs::write(&f, "V2").unwrap();
        assert!(snapshot_if_new(&f).unwrap().is_some()); // 又是新内容 → 留底
        assert_eq!(list_versions(&f).unwrap().len(), 2);
    }

    #[test]
    fn repeated_restore_does_not_grow_history() {
        let f = temp_doc("V1");
        snapshot(&f).unwrap(); // 历史: [V1]
        fs::write(&f, "V2").unwrap();
        let v1 = list_versions(&f).unwrap().last().unwrap().path.clone();

        // 第一次回版：当前 V2 未入历史 → 留底，历史 [V1, V2]
        assert!(restore(&f, &v1).unwrap().is_some());
        assert_eq!(fs::read_to_string(&f).unwrap(), "V1");
        assert_eq!(list_versions(&f).unwrap().len(), 2);

        // 连点同一版本：当前 V1 已在历史 → 不再留底，版本数不涨
        assert!(restore(&f, &v1).unwrap().is_none());
        assert!(restore(&f, &v1).unwrap().is_none());
        assert_eq!(list_versions(&f).unwrap().len(), 2);

        // 在两版之间来回跳：每跳当前状态都已在历史 → 也不涨
        let v2 = list_versions(&f)
            .unwrap()
            .into_iter()
            .find(|v| fs::read_to_string(&v.path).unwrap() == "V2")
            .unwrap()
            .path;
        assert!(restore(&f, &v2).unwrap().is_none());
        assert_eq!(fs::read_to_string(&f).unwrap(), "V2");
        assert!(restore(&f, &v1).unwrap().is_none());
        assert_eq!(fs::read_to_string(&f).unwrap(), "V1");
        assert_eq!(list_versions(&f).unwrap().len(), 2); // 始终只有两种状态
    }

    #[test]
    fn restore_missing_version_errors_not_panics() {
        let f = temp_doc("V1");
        let err = restore(&f, Path::new("/nonexistent/0000000000000.docx")).unwrap_err();
        assert!(err.contains("版本不存在"));
        assert_eq!(fs::read_to_string(&f).unwrap(), "V1"); // 文件没被动
    }
}
