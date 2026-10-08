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

/// Exactly one node of `kind` must exist (mediators merge by name). A regression that split one mediator into
/// several nodes would still satisfy `>= 1`, so we pin the exact count here.
fn single_node(b: &common::Built, kind: &str) -> gt_domain::model::Node {
    let nodes = nodes_of_kind(b, kind);
    assert_eq!(
        nodes.len(),
        1,
        "expected exactly one {kind} node (mediators merge by name), got {}: {:?}",
        nodes.len(),
        nodes.iter().map(|n| &n.name).collect::<Vec<_>>()
    );
    nodes.into_iter().next().unwrap()
}

fn single_node_name(b: &common::Built, kind: &str) -> String {
    single_node(b, kind).name
}

/// The `Method` nodes at the far end of every `edge` in-edge on the (single) `kind` node. The engine links a
/// semantic edge directly from the owning `Method` (not from a call-site node), so the source *is* the method. This
/// pins that a semantic edge hangs off the *right* call site, not merely that *some* edge of that kind exists on the
/// mediator.
fn incoming_method_sources(b: &common::Built, kind: &str, edge: &str) -> Vec<String> {
    let node = single_node(b, kind);
    b.store
        .edges_of(node.id, EdgeDirection::Incoming)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.0 == edge)
        .filter_map(|e| b.store.get_node(e.from_id).ok().flatten())
        .filter(|m| m.kind.0 == "Method")
        .map(|m| m.name.clone())
        .collect()
}

/// The `Method` nodes targeted by every `edge` **out**-edge on the (single) `kind` node (used for `Schedule --Triggers--> method`).
fn outgoing_method_targets(b: &common::Built, kind: &str, edge: &str) -> Vec<String> {
    let node = single_node(b, kind);
    b.store
        .edges_of(node.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.0 == edge)
        .filter_map(|e| {
            b.store
                .get_node(e.to_id)
                .ok()
                .flatten()
                .filter(|m| m.kind.0 == "Method")
                .map(|m| m.name.clone())
        })
        .collect()
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

    // Each mediator must merge into exactly one node keyed by its name, and the name must be the literal the
    // annotation / call site carries (cache name, queue / topic name, event type, cron). The original `>= 1` check
    // would pass even if every annotation spawned its own node — which would silently break the publish / subscribe
    // loop that the consumer + producer edges are supposed to form on a single node.
    assert_eq!(
        single_node_name(&b, "Cache"),
        "userCache",
        "the three cache annotations must merge on the cache name"
    );
    assert_eq!(
        single_node_name(&b, "Queue"),
        "orders.queue",
        "Rabbit consumer + producer must merge on the queue name"
    );
    assert_eq!(
        single_node_name(&b, "Topic"),
        "audit.topic",
        "Kafka consumer + producer must merge on the topic name"
    );
    assert_eq!(
        single_node_name(&b, "Event"),
        "OrderPlacedEvent",
        "event publisher + subscriber must merge on the event type"
    );
    assert_eq!(
        single_node_name(&b, "Schedule"),
        "0 0 * * * *",
        "the @Scheduled cron literal must become the Schedule node name"
    );

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

    // Each semantic edge must hang off the *right* method, not merely exist on the mediator. The `has_incoming_edge`
    // checks above only look at the edge kind, so a regression that mis-attached a cache / event / queue edge to the
    // wrong call site would still pass. The engine links a semantic edge directly from the owning `Method`.
    let cache_readers = incoming_method_sources(&b, "Cache", "ReadsCache");
    assert!(
        cache_readers.iter().any(|m| m == "getUser"),
        "ReadsCache must come from @Cacheable on `getUser`, got: {cache_readers:?}"
    );
    let cache_writers = incoming_method_sources(&b, "Cache", "WritesCache");
    assert!(
        cache_writers.iter().any(|m| m == "save") && cache_writers.iter().any(|m| m == "clear"),
        "WritesCache must come from @CachePut `save` and @CacheEvict `clear`, got: {cache_writers:?}"
    );
    assert!(
        incoming_method_sources(&b, "Event", "ListensTo").iter().any(|m| m == "onOrderPlaced"),
        "Event ListensTo must come from @EventListener `onOrderPlaced`"
    );
    assert!(
        incoming_method_sources(&b, "Event", "Emits").iter().any(|m| m == "doSomething"),
        "Event Emits must come from `publishEvent` inside `doSomething`"
    );
    assert!(
        incoming_method_sources(&b, "Queue", "ListensTo").iter().any(|m| m == "onOrder"),
        "Queue ListensTo must come from @RabbitListener `onOrder`"
    );
    assert!(
        incoming_method_sources(&b, "Queue", "PublishesTo").iter().any(|m| m == "produce"),
        "Queue PublishesTo must come from `convertAndSend` inside `produce`"
    );
    assert!(
        incoming_method_sources(&b, "Topic", "ListensTo").iter().any(|m| m == "onAudit"),
        "Topic ListensTo must come from @KafkaListener `onAudit`"
    );
    assert!(
        incoming_method_sources(&b, "Topic", "PublishesTo")
            .iter()
            .any(|m| m == "produceKafka"),
        "Topic PublishesTo must come from `kafkaTemplate.send` inside `produceKafka`"
    );
    assert!(
        outgoing_method_targets(&b, "Schedule", "Triggers").iter().any(|m| m == "nightly"),
        "Schedule Triggers must target the @Scheduled `nightly` method"
    );

    // The Cache / Event / Queue rules declare `side` in their binding (so the node survives any side-filtered
    // perspective); the Schedule / Topic rules do not, so only these three are pinned here. A regression that
    // dropped the `side` prop from any of them would make the node disappear from a backend view.
    for kind in ["Cache", "Event", "Queue"] {
        let nodes = nodes_of_kind(&b, kind);
        assert!(!nodes.is_empty(), "the {kind} node must exist");
        for n in &nodes {
            assert_eq!(
                n.properties.get("side").and_then(|v| v.as_str()),
                Some("backend"),
                "the {kind} node {name} must carry side=backend, got properties={props}",
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
