//! 文件扫描适配器对真实 CRMEB 样本的测试。
//!
//! 验证两件事：① 子工程标记文件能被发现（composer.json / package.json）；
//! ② 依赖与资源目录被内置规则正确排除。

use std::path::{Path, PathBuf};

use gt_adapter_fs::WalkDirScanner;
use gt_domain::port::ScanRequest;
use gt_domain::port::FileScanner;

fn sample_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("GRAPHTELL_SAMPLE_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    // 从 `CARGO_MANIFEST_DIR` 向上逐层查找 `samples/**/CRMEB-master`：
    // 先试 `samples/CRMEB-master`，再遍历一层子目录（样本按技术栈分目录放置时也能命中）。
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        let samples = cur.join("samples");
        let direct = samples.join("CRMEB-master");
        if direct.is_dir() {
            return Some(direct.canonicalize().unwrap_or(direct));
        }
        let mut hits: Vec<PathBuf> = std::fs::read_dir(&samples)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().join("CRMEB-master"))
            .filter(|p| p.is_dir())
            .collect();
        hits.sort();
        if let Some(hit) = hits.into_iter().next() {
            return Some(hit.canonicalize().unwrap_or(hit));
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

#[test]
#[ignore = "需要未入库的 CRMEB 样本（体积过大，不随仓库分发）"]
fn finds_sub_project_markers() {
    let Some(root) = sample_root() else {
        panic!("CRMEB 样本缺失：该测试已标记 #[ignore]，用 --ignored 运行时需要设置 GRAPHTELL_SAMPLE_DIR 或放置 samples/**/CRMEB-master");
    };
    let scanner = WalkDirScanner::new(Vec::new());
    let markers = scanner
        .find_markers(Path::new(&root), &["composer.json", "package.json"], 4)
        .expect("扫描标记文件");
    let names: Vec<String> = markers
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "composer.json"),
        "应找到后端 composer.json"
    );
    assert!(
        names.iter().any(|n| n == "package.json"),
        "应找到前端 package.json"
    );
}

#[test]
#[ignore = "需要未入库的 CRMEB 样本（体积过大，不随仓库分发）"]
fn scan_excludes_vendor_and_assets() {
    let Some(root) = sample_root() else {
        panic!("CRMEB 样本缺失：该测试已标记 #[ignore]，用 --ignored 运行时需要设置 GRAPHTELL_SAMPLE_DIR 或放置 samples/**/CRMEB-master");
    };
    let scanner = WalkDirScanner::new(Vec::new());
    let files = scanner
        .scan(&ScanRequest {
            root: root.clone(),
            extra_excludes: Vec::new(),
            languages: Vec::new(), // 空 = 所有支持的语言
            language_extensions: Vec::new(), // 空 = 回退扫描器内置扩展名表
        })
        .expect("扫描");

    assert!(!files.is_empty(), "应扫到源文件");

    for forbidden in ["/vendor/", "/node_modules/", "/target/", "/.git/"] {
        let leaked = files
            .iter()
            .any(|f| f.path.to_string_lossy().contains(forbidden));
        assert!(!leaked, "扫描结果不应包含 {forbidden}");
    }
    // 静态资源扩展名应被排除
    let has_asset = files
        .iter()
        .any(|f| {
            let name = f.path.to_string_lossy().to_ascii_lowercase();
            name.ends_with(".png")
                || name.ends_with(".jpg")
                || name.ends_with(".woff2")
                || name.ends_with(".zip")
        });
    assert!(!has_asset, "静态资源不应进入待分析集合");

    // 业务源码必须在
    assert!(
        files
            .iter()
            .any(|f| f.relative == "crmeb/app/event.php"),
        "app/event.php 必须在扫描结果中"
    );
}
