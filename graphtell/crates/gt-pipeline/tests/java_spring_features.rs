//! Spring（Java）cache / event / queue / topic / schedule 语义特征的端到端自检。
//!
//! 刻意用合成样本（无需外部 Spring 工程样本）：在临时目录放一个最小 Spring Boot
//! 工程（pom.xml 含 spring-boot + 一个带各类注解的 .java），跑完整建图，
//! 断言 FKB 真的把 `@Cacheable` / `@Scheduled` / `@EventListener` / `@RabbitListener`
//! / `@KafkaListener` / `publishEvent` 落成对应的语义节点与边。
//!
//! 这同时证明「只写 FKB 即可为第二语言（Java）补齐框架语义」—— 没有改任何内核。

use gt_domain::model::{NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

fn synthetic_spring_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-java-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/main/java/com/example")).expect("mkdir");

    std::fs::write(
        dir.join("pom.xml"),
        r#"<?xml version="1.0"?>
<project>
  <dependencies>
    <dependency>
      <groupId>org.springframework.boot</groupId>
      <artifactId>spring-boot-starter-cache</artifactId>
    </dependency>
    <dependency>
      <groupId>org.springframework.boot</groupId>
      <artifactId>spring-boot-starter-amqp</artifactId>
    </dependency>
  </dependencies>
</project>
"#,
    )
    .expect("write pom");

    std::fs::write(
        dir.join("src/main/java/com/example/DemoService.java"),
        r#"package com.example;

import org.springframework.cache.annotation.Cacheable;
import org.springframework.cache.annotation.CachePut;
import org.springframework.cache.annotation.CacheEvict;
import org.springframework.scheduling.annotation.Scheduled;
import org.springframework.context.event.EventListener;
import org.springframework.amqp.rabbit.annotation.RabbitListener;
import org.springframework.kafka.annotation.KafkaListener;
import org.springframework.context.ApplicationEventPublisher;

public class DemoService {
    private ApplicationEventPublisher publisher;
    private Object rabbitTemplate;
    private Object kafkaTemplate;

    @Cacheable("userCache")
    public User getUser(Long id) { return null; }

    @CachePut("userCache")
    public void save(User u) {}

    @CacheEvict("userCache")
    public void clear() {}

    @Scheduled(cron = "0 0 * * * *")
    public void nightly() {}

    @EventListener
    public void onOrderPlaced(OrderPlacedEvent e) {}

    @RabbitListener(queues = "orders.queue")
    public void onOrder(Message m) {}

    @KafkaListener(topics = "audit.topic")
    public void onAudit(Message m) {}

    public void doSomething() {
        publisher.publishEvent(new OrderPlacedEvent());
    }

    public void produce() {
        rabbitTemplate.convertAndSend("orders.queue", "payload");
    }

    public void produceKafka() {
        kafkaTemplate.send("audit.topic", "payload");
    }
}
"#,
    )
    .expect("write java");

    dir
}

fn count_kind(b: &common::Built, kind: &str) -> usize {
    b.store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(kind.to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query")
        .len()
}

fn has_incoming_edge(b: &common::Built, kind: &str, edge: &str) -> bool {
    let nodes = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(kind.to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query");
    nodes.iter().any(|n| {
        b.store
            .edges_of(n.id, EdgeDirection::Incoming)
            .expect("edges")
            .iter()
            .any(|e| e.kind.as_str() == edge)
    })
}

#[test]
fn spring_features_produce_semantic_nodes_and_edges() {
    let root = synthetic_spring_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("合成 Spring 工程建图应成功");
    };

    // 节点：每类至少 1 个（Event 会按收发方法拆成多个，但 ≥1 即可）。
    for kind in ["Cache", "Schedule", "Event", "Queue", "Topic"] {
        assert!(
            count_kind(&b, kind) >= 1,
            "应产出 {kind} 语义节点，实际：{:?}",
            ["Cache", "Schedule", "Event", "Queue", "Topic"]
                .iter()
                .map(|k| (*k, count_kind(&b, *k)))
                .collect::<Vec<_>>()
        );
    }

    // 边：Cache 同时有 ReadsCache（@Cacheable）与 WritesCache（@CachePut/@CacheEvict）。
    assert!(
        has_incoming_edge(&b, "Cache", "ReadsCache"),
        "Cache 应有 ReadsCache 入边"
    );
    assert!(
        has_incoming_edge(&b, "Cache", "WritesCache"),
        "Cache 应有 WritesCache 入边"
    );
    // Event 既有 ListensTo（@EventListener）也有 Emits（publishEvent）。
    assert!(
        has_incoming_edge(&b, "Event", "ListensTo"),
        "Event 应有 ListensTo 入边"
    );
    assert!(
        has_incoming_edge(&b, "Event", "Emits"),
        "Event 应有 Emits 入边"
    );
    // 事件类型级归并：`@EventListener` 处理 `OrderPlacedEvent` 与
    // `publishEvent(new OrderPlacedEvent())` 都指向同一事件类型，应归并到
    // **同一个** Event 节点（而非以收发方法名各建一个）。
    assert_eq!(
        count_kind(&b, "Event"),
        1,
        "同一事件类型的发布方与订阅方应归并到 1 个 Event 节点"
    );
    // Queue / Topic 消费端 = ListensTo。
    assert!(
        has_incoming_edge(&b, "Queue", "ListensTo"),
        "Queue 应有 ListensTo 入边"
    );
    assert!(
        has_incoming_edge(&b, "Topic", "ListensTo"),
        "Topic 应有 ListensTo 入边"
    );
    // Topic 生产端（`kafkaTemplate.send`）→ PublishesTo：与上面的 ListensTo 合并到
    // 同一个 "audit.topic" 节点，形成自洽的发布 / 订阅闭环（验证 receiver 收窄匹配）。
    assert!(
        has_incoming_edge(&b, "Topic", "PublishesTo"),
        "Topic 应有 PublishesTo 入边（Kafka 生产端）"
    );
    // Queue 生产端（`convertAndSend`）→ PublishesTo：与上面的 ListensTo 合并到
    // 同一个 "orders.queue" 节点，形成自洽的发布 / 订阅闭环（验证解析器实参捕获）。
    assert!(
        has_incoming_edge(&b, "Queue", "PublishesTo"),
        "Queue 应有 PublishesTo 入边（消息生产端）"
    );

    // Schedule 的 Triggers 是出边（Schedule ——> 方法）。
    let schedules = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind("Schedule".to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query");
    assert!(
        schedules.iter().any(|n| {
            b.store
                .edges_of(n.id, EdgeDirection::Outgoing)
                .expect("edges")
                .iter()
                .any(|e| e.kind.as_str() == "Triggers")
        }),
        "Schedule 应有 Triggers 出边"
    );
}
