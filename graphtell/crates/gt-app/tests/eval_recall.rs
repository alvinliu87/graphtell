//! 召回评测语料批量跑分。
//!
//! 把 `tests/eval/*.jsonl` 里的「查询 → 期望命中 → 最低质量档」当成回归语料，
//! **对真实 CRMEB 样本**跑一遍召回并校验。用途：
//!
//! 1. 锁定 6 类真实开发场景的召回基线（监听器浮出 / 业务方法命中 / 质量不崩）；
//! 2. 将来改召回算法、接 bge 语义路径后，可批量对比「质量档 + 命中节点」是否回归。
//!
//! **依赖体积过大的真实样本（不入库），整组标了 `#[ignore]`**：默认 `cargo test`
//! 不会执行，需先设置 `GRAPHTELL_SAMPLE_DIR` 指向 CRMEB 根，再
//! `cargo test -- --ignored eval_recall`。无样本时优雅跳过（不是失败）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use gt_app::{AppConfig, Container};
use gt_application::{PipelineService, ProjectService, RecallQuality, RecallQuery, RecallService};
use gt_domain::model::NewProject;
use gt_domain::port::{NoopObserver, Persistence, SystemClock};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../")
}

/// 在 `dir/samples` 或 `GRAPHTELL_SAMPLE_DIR` 下定位 CRMEB 样本根。
fn find_sample() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("GRAPHTELL_SAMPLE_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    let samples = workspace_root().join("samples");
    let direct = samples.join("CRMEB-master");
    if direct.is_dir() {
        return Some(direct);
    }
    let mut hits: Vec<PathBuf> = std::fs::read_dir(&samples)
        .ok()?
        .flatten()
        .map(|e| e.path().join("CRMEB-master"))
        .filter(|p| p.is_dir())
        .collect();
    hits.sort();
    hits.into_iter().next()
}

struct Built {
    container: Container,
    project_id: gt_domain::model::ProjectId,
}

/// 跑一次完整建图并缓存（同一测试二进制内只跑一遍）。
fn built() -> Option<Arc<Built>> {
    static CACHE: OnceLock<Option<Arc<Built>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let sample = find_sample()?;
            let data_dir =
                std::env::temp_dir().join(format!("graphtell-evaltest-{}", std::process::id()));
            std::fs::create_dir_all(&data_dir).ok()?;

            let config = AppConfig {
                data_dir,
                fkb_dir: Some(workspace_root().join("fkb")),
                views_dir: Some(workspace_root().join("views")),
                rules_dir: Some(workspace_root().join("rules")),
                bind: "127.0.0.1".into(),
                port: 0,
                ui_dir: None,
                };
            let container = Container::new(config).expect("容器装配不应失败");

            let projects =
                ProjectService::new(container.store.clone() as Arc<dyn Persistence>, Arc::new(SystemClock));
            let pipeline = PipelineService::new(
                container.store.clone() as Arc<dyn Persistence>,
                Arc::clone(&container.deps),
                Arc::clone(&container.rules) as Arc<dyn gt_domain::port::RuleProvider>,
            );

            let project = projects
                .create(NewProject {
                    name: "CRMEB".into(),
                    root_path: sample,
                    description: None,
                    config: None,
                })
                .expect("创建工程不应失败");

            pipeline
                .run(project.id, &NoopObserver)
                .expect("对 CRMEB 样本建图不应失败");

            Some(Arc::new(Built {
                container,
                project_id: project.id,
            }))
        })
        .clone()
}

fn skip() -> &'static str {
    "跳过：未找到 CRMEB 样本（可用 GRAPHTELL_SAMPLE_DIR 指定）"
}

/// 把质量档字符串解析成可比较的序数。
fn quality_ord(q: &RecallQuality) -> u8 {
    match q {
        RecallQuality::Low => 0,
        RecallQuality::Medium => 1,
        RecallQuality::High => 2,
    }
}

fn threshold_ord(s: &str) -> u8 {
    match s {
        "low" => 0,
        "high" => 2,
        _ => 1, // 默认 medium
    }
}

/// 跑 `tests/eval/recall_scenarios.jsonl` 整份语料，逐条校验「期望命中 + 最低质量档」。
#[test]
#[ignore = "需要 CRMEB-master 样本：设置 GRAPHTELL_SAMPLE_DIR 后运行 `cargo test -- --ignored` 才会执行"]
fn eval_recall_scenarios() {
    let Some(b) = built() else {
        eprintln!("{}", skip());
        return;
    };

    let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/eval/recall_scenarios.jsonl");
    let text = match std::fs::read_to_string(&corpus) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("跳过：语料文件 {corpus:?} 读取失败：{e}");
            return;
        }
    };

    let svc = RecallService::new(
        b.container.store.clone() as Arc<dyn Persistence>,
        b.container.filesystem(),
        b.container.scanner(),
    );

    let mut failures: Vec<String> = Vec::new();
    let mut ran = 0usize;

    for (i, line) in text.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("line {} 语料 JSON 解析失败：{e}", i + 1));
                continue;
            }
        };
        let id = v["id"].as_str().unwrap_or("?");
        let query = match v["query"].as_str() {
            Some(q) => q.to_string(),
            None => {
                failures.push(format!("[{id}] 缺少 query 字段"));
                continue;
            }
        };
        let expect: Vec<String> = v["expect_contains"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let min_q = v["expect_quality_at_least"].as_str().unwrap_or("medium");
        let note = v["note"].as_str().unwrap_or("");

        let result = match svc.recall(
            b.project_id,
            &RecallQuery {
                query: query.clone(),
                limit: 20,
                hops: 2,
                kinds: Vec::new(),
                with_snippets: false,
                include_body: false,
            },
        ) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("[{id}] recall 失败：{e}"));
                continue;
            }
        };
        ran += 1;

        // 期望命中：名字或 fqn 子串命中即算（fqn 含命名空间与类名，匹配更稳）。
        let mut missing = Vec::new();
        for exp in &expect {
            let hit = result
                .hits
                .iter()
                .any(|h| h.name.contains(exp) || h.fqn.as_deref().map_or(false, |f| f.contains(exp)));
            if !hit {
                missing.push(exp.clone());
            }
        }

        // 质量档：result.quality 序数必须 >= 期望阈值。
        let quality_ok = quality_ord(&result.quality) >= threshold_ord(min_q);

        // 可视化的单行摘要（跑 `--ignored` 时直接看到每条质量与缺失项）。
        let top: Vec<&str> = result
            .hits
            .iter()
            .take(4)
            .map(|h| h.name.as_str())
            .collect();
        eprintln!(
            "[{id}] quality={:?} top={:?} missing={:?} expect={:?}",
            result.quality, top, missing, expect
        );
        if !note.is_empty() {
            eprintln!("      note: {note}");
        }

        if !missing.is_empty() {
            failures.push(format!(
                "[{id}] query=`{query}` 期望命中未出现：{:?}（实际 top：{:?}）",
                missing, top
            ));
        }
        if !quality_ok {
            failures.push(format!(
                "[{id}] query=`{query}` 质量档 {:?} 低于期望 `{}`",
                result.quality, min_q
            ));
        }
    }

    if ran == 0 {
        eprintln!("warn: 语料未包含任何可执行的查询行");
    }
    if !failures.is_empty() {
        panic!(
            "eval 语料有 {} 条未通过（共执行 {ran} 条）：\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
