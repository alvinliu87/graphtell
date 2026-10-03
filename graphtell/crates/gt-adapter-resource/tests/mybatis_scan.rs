//! The MyBatis mapper adapter in isolation: no graph, no pipeline — it reads files and returns facts.

use std::path::PathBuf;

use gt_adapter_fs::StdFileSystem;
use gt_adapter_resource::MyBatisMapperAdapter;
use gt_domain::model::{FactValue, Language, ProjectId, SubProject, SubProjectId};
use gt_domain::port::{PseudoCall, ResourceAdapter, ResourceFact};

fn write(dir: &std::path::Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

fn sub(root: PathBuf) -> SubProject {
    SubProject {
        id: SubProjectId(1),
        project_id: ProjectId(1),
        name: "app".into(),
        root_path: root,
        language: Language(Language::JAVA.to_string()),
        role: "backend".into(),
        detected_by: "pom.xml".into(),
        frameworks: vec!["mybatis".into()],
        facts: serde_json::Value::Null,
    }
}

fn scan(dir: &std::path::Path) -> Vec<PseudoCall> {
    let adapter = MyBatisMapperAdapter::default();
    assert_eq!(adapter.id(), "mybatis", "the adapter serves this knowledge id");
    adapter
        .scan(&sub(dir.to_path_buf()), dir, &StdFileSystem::new())
        .expect("scan should succeed")
        .into_iter()
        .map(|fact| match fact {
            ResourceFact::PseudoCall(call) => call,
        })
        .collect()
}

#[test]
fn each_statement_yields_one_pseudo_call_per_table() {
    let dir = std::env::temp_dir().join(format!("gt-mapper-scan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(
        &dir,
        "src/main/resources/mapper/OrderMapper.xml",
        r#"<mapper namespace="demo.OrderMapper">
  <select id="selectByUser">
    select * from eb_order where user_id = #{userId}
  </select>
  <update id="updateStatus">
    update eb_order set status = 1 where id = #{id}
  </update>
</mapper>
"#,
    );

    let calls = scan(&dir);
    assert_eq!(calls.len(), 2, "one pseudo call per statement: {calls:?}");

    let select = &calls[0];
    assert_eq!(select.owner_fqn, "demo.OrderMapper.selectByUser");
    assert_eq!(select.owner_class.as_deref(), Some("demo.OrderMapper"));
    assert_eq!(select.callee, "mybatis::select");
    assert_eq!(select.receiver.as_deref(), Some("mybatis"));
    assert_eq!(select.method.as_deref(), Some("select"));
    assert!(
        matches!(&select.args[0], FactValue::String(t) if t == "eb_order"),
        "the table name is argument 0: {:?}",
        select.args
    );
    assert_eq!(
        select.file, "src/main/resources/mapper/OrderMapper.xml",
        "the display path stays project-relative"
    );
    assert_eq!(
        select.props["mapper"], "src/main/resources/mapper/OrderMapper.xml",
        "evidence keeps the mapper file"
    );
    assert_eq!(select.span.start_line, 2, "the statement's own line");
    assert!(select.confidence < 1.0, "a synthesised fact is not a parser fact");

    assert_eq!(calls[1].callee, "mybatis::update");
    assert_eq!(calls[1].owner_fqn, "demo.OrderMapper.updateStatus");
}

#[test]
fn files_without_mapper_root_are_ignored() {
    let dir = std::env::temp_dir().join(format!("gt-mapper-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write(
        &dir,
        "src/main/resources/mapper/NotAMapper.xml",
        r#"<beans><bean id="selectAll" class="demo.OrderMapper"/></beans>"#,
    );
    write(&dir, "notes.txt", "<mapper namespace=\"demo.OrderMapper\">");

    assert!(scan(&dir).is_empty(), "only `<mapper`-rooted XMLs count");
}
