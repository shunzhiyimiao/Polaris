//! 端到端查看器示例：把一个真实 docx 拖进来，经 OfficeCLI 后端 → ProseModel → HTML 打印出来。
//!
//! 用法： cargo run -p polaris-docx --example view_docx -- <path/to.docx>
//! 依赖外部 `officecli` 在 PATH 上（见 polaris-docx/THIRD_PARTY.md）。

use editor_core::{render_html, ProseModel};
use polaris_docx::{import_docx, OfficeCliBackend};

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("用法: view_docx <path/to.docx>");
            std::process::exit(2);
        }
    };

    let mut model = ProseModel::new();
    let backend = OfficeCliBackend::new();
    match import_docx(&backend, &path, &mut model) {
        Ok(ids) => {
            let r = render_html(&model);
            eprintln!("// imported {} blocks from {}", ids.len(), path);
            eprintln!("// sourcemap spans: {}", r.source_map.spans.len());
            println!("{}", r.html);
        }
        Err(e) => {
            eprintln!("import failed: {e}");
            std::process::exit(1);
        }
    }
}
