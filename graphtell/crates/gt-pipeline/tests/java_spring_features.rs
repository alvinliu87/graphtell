//! End-to-end self-check of Spring (Java) cache / event / queue / topic / schedule semantics.
//!
//! Deliberately uses a synthetic sample (no external Spring project needed): a minimal Spring Boot project is
//! placed in a temp directory (a pom.xml with spring-boot plus one .java carrying the various annotations), a full
//! graph build runs, and it is asserted that FKB really turns `@Cacheable` / `@Scheduled` / `@EventListener` /
//! `@RabbitListener` / `@KafkaListener` / `publishEvent` into the corresponding semantic nodes and edges.
//!
//! This also demonstrates "framework semantics for a second language (Java) can be completed by writing FKB only"
//! — no kernel change at all.

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
    nodes_of_kind(b, kind).len()
}

fn nodes_of_kind(b: &common::Built, kind: &str) -> Vec<gt_domain::model::Node> {
    b.store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(kind.to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query")
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
        panic!("the graph build of the synthetic Spring project should succeed");
    };

    // Nodes: at least one of each kind (an Event may split into several by send / receive method, but >= 1 suffices).
    for kind in ["Cache", "Schedule", "Event", "Queue", "Topic"] {
        assert!(
            count_kind(&b, kind) >= 1,
            "expected a {kind} semantic node, got: {:?}",
            ["Cache", "Schedule", "Event", "Queue", "Topic"]
                .iter()
                .map(|k| (*k, count_kind(&b, *k)))
                .collect::<Vec<_>>()
        );
    }

    // Edges: Cache has both ReadsCache (@Cacheable) and WritesCache (@CachePut/@CacheEvict).
    assert!(
        has_incoming_edge(&b, "Cache", "ReadsCache"),
        "Cache should have a ReadsCache in-edge"
    );
    assert!(
        has_incoming_edge(&b, "Cache", "WritesCache"),
        "Cache should have a WritesCache in-edge"
    );
    // Event has both ListensTo (@EventListener) and Emits (publishEvent).
    assert!(
        has_incoming_edge(&b, "Event", "ListensTo"),
        "Event should have a ListensTo in-edge"
    );
    assert!(
        has_incoming_edge(&b, "Event", "Emits"),
        "Event should have an Emits in-edge"
    );
    assert_eq!(
        count_kind(&b, "Event"),
        1,
        "the publisher and subscriber of the same event type should merge into 1 Event node"
    );
    // Queue / Topic consumer side = ListensTo.
    assert!(
        has_incoming_edge(&b, "Queue", "ListensTo"),
        "Queue should have a ListensTo in-edge"
    );
    assert!(
        has_incoming_edge(&b, "Topic", "ListensTo"),
        "Topic should have a ListensTo in-edge"
    );
    // Topic producer side (`kafkaTemplate.send`) -> PublishesTo: merges with the ListensTo above onto
    // the same "audit.topic" node, forming a self-consistent publish / subscribe loop (verifies narrowed receiver matching).
    assert!(
        has_incoming_edge(&b, "Topic", "PublishesTo"),
        "Topic should have a PublishesTo in-edge (the Kafka producer side)"
    );
    // Queue producer side (`convertAndSend`) -> PublishesTo: merges with the ListensTo above onto
    // the same "orders.queue" node, forming a self-consistent publish / subscribe loop (verifies parser argument capture).
    assert!(
        has_incoming_edge(&b, "Queue", "PublishesTo"),
        "Queue should have a PublishesTo in-edge (the message producer side)"
    );

    // Every out-of-process mediator this FKB builds must carry `side`. These rules historically declared none,
    // so their nodes arrived with no party evidence and disappeared from any side-filtered perspective; the side
    // is now inherited from `fkb/java/spring-boot.yaml`'s top-level `side: backend` (see the loader).
    for kind in ["Cache", "Event", "Queue"] {
        let nodes = nodes_of_kind(&b, kind);
        assert!(!nodes.is_empty(), "the {kind} node must exist");
        for n in &nodes {
            assert_eq!(
                n.properties.get("side").and_then(|v| v.as_str()),
                Some("backend"),
                "the {kind} node {name} must inherit side=backend (inherited from the increment's top-level declaration), got properties={props}",
                name = n.name,
                props = n.properties
            );
        }
    }

    // Schedule's Triggers is an out-edge (Schedule ——> method).
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
        "Schedule should have a Triggers out-edge"
    );
}
