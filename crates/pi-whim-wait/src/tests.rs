use crate::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Barrier};
use std::time::Duration;

fn owner(value: &str) -> WaitOwnerId {
    WaitOwnerId::new(value).unwrap()
}

fn source_id(value: &str) -> WaitSourceId {
    WaitSourceId::new(value).unwrap()
}

fn descriptor(id: &str) -> WaitSourceDescriptor {
    WaitSourceDescriptor::new(source_id(id), ["kind", "owner", "value"], ["kind", "owner"]).unwrap()
}

fn matcher(fields: &[(&str, Value)]) -> WaitMatcher {
    WaitMatcher::new(
        fields
            .iter()
            .map(|(field, value)| ((*field).to_owned(), value.clone()))
            .collect(),
    )
    .unwrap()
}

fn clause(source: &str, fields: &[(&str, Value)]) -> WaitClause {
    WaitClause::new(
        WaitSourceSelection::source(source_id(source)),
        matcher(fields),
    )
    .unwrap()
}

#[test]
fn foreground_wait_does_not_lose_a_wakeup() {
    let hub = WaitHub::new().unwrap();
    let source = hub.register_source(descriptor("messages")).unwrap();
    let sequence = hub.current_sequence();
    let ready = Arc::new(Barrier::new(2));
    let thread_hub = hub.clone();
    let thread_ready = ready.clone();
    let waiter = std::thread::spawn(move || {
        thread_ready.wait();
        thread_hub.wait(
            WaitQuery::after(
                WaitSourceSelection::source(source_id("messages")),
                matcher(&[("kind", json!("received"))]),
                sequence,
            ),
            Duration::from_secs(1),
        )
    });
    ready.wait();
    let published = source
        .publish(json!({"kind": "received", "owner": "root", "value": 1}))
        .unwrap();
    assert_eq!(
        waiter.join().unwrap().unwrap(),
        WaitStatus::Matched { event: published }
    );
}

#[test]
fn retained_event_can_be_matched_from_an_explicit_sequence() {
    let hub = WaitHub::new().unwrap();
    let source = hub.register_source(descriptor("messages")).unwrap();
    let sequence = hub.current_sequence();
    let before_ms = crate::types::wall_clock_ms();
    let published = source
        .publish(json!({"kind": "received", "owner": "root"}))
        .unwrap();
    let after_ms = crate::types::wall_clock_ms();
    assert!(published.emitted_at_ms >= before_ms);
    assert!(published.emitted_at_ms <= after_ms);
    let matched = hub
        .wait(
            WaitQuery::after(
                WaitSourceSelection::source(source_id("messages")),
                matcher(&[("owner", json!("root"))]),
                sequence,
            ),
            Duration::ZERO,
        )
        .unwrap();
    assert_eq!(matched, WaitStatus::Matched { event: published });
}

#[test]
fn future_wait_ignores_existing_publication() {
    let hub = WaitHub::new().unwrap();
    let source = hub.register_source(descriptor("messages")).unwrap();
    source
        .publish(json!({"kind": "received", "owner": "root"}))
        .unwrap();
    let result = hub.wait(
        WaitQuery::future(
            WaitSourceSelection::source(source_id("messages")),
            matcher(&[("owner", json!("root"))]),
        ),
        Duration::from_millis(10),
    );
    assert_eq!(result, Ok(WaitStatus::TimedOut));
}

#[test]
fn foreground_timeout_is_bounded() {
    let hub = WaitHub::new().unwrap();
    let _source = hub.register_source(descriptor("messages")).unwrap();
    let result = hub.wait(
        WaitQuery::future(
            WaitSourceSelection::source(source_id("messages")),
            WaitMatcher::empty(),
        ),
        Duration::from_millis(10),
    );
    assert_eq!(result, Ok(WaitStatus::TimedOut));
}

#[test]
fn task_metadata_bounds_and_serde_are_validated() {
    let metadata = WaitTaskMetadata::new("job", "a task").unwrap();
    assert_eq!(metadata.target_kind(), "job");
    assert_eq!(metadata.target_summary(), "a task");
    assert_eq!(
        serde_json::to_value(&metadata).unwrap()["target_kind"],
        "job"
    );
    assert_eq!(
        serde_json::from_value::<WaitTaskMetadata>(serde_json::to_value(&metadata).unwrap())
            .unwrap(),
        metadata
    );

    assert!(WaitTaskMetadata::new("", "").is_err());
    assert!(WaitTaskMetadata::new("k".repeat(65), "").is_err());
    assert!(WaitTaskMetadata::new("k", "s".repeat(257)).is_err());
    assert!(WaitTaskMetadata::new("é".repeat(32), "").is_ok());
    assert!(WaitTaskMetadata::new("é".repeat(33), "").is_err());
    assert!(WaitTaskMetadata::new("kind", "s".repeat(256)).is_ok());
    assert!(WaitTaskMetadata::new("kind", "s".repeat(257)).is_err());

    assert!(
        serde_json::from_value::<WaitTaskMetadata>(json!({
            "target_kind": "",
            "target_summary": ""
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<WaitTaskMetadata>(json!({
            "target_kind": "kind",
            "target_summary": "s".repeat(257)
        }))
        .is_err()
    );
}

#[test]
fn background_metadata_replays_and_completes_unchanged() {
    let hub = WaitHub::new().unwrap();
    let source = hub.register_source(descriptor("messages")).unwrap();
    let metadata = WaitTaskMetadata::new("message", "awaiting receipt").unwrap();
    let (completion_tx, completion_rx) = std::sync::mpsc::channel();
    let _completion = hub
        .completion_signal()
        .subscribe_fn(move |snapshot| completion_tx.send(snapshot).unwrap());
    let task_id = hub
        .start_background_with_metadata(
            owner("root"),
            WaitQuery::future(
                WaitSourceSelection::source(source_id("messages")),
                matcher(&[("kind", json!("received"))]),
            ),
            Duration::from_secs(1),
            Some(metadata.clone()),
        )
        .unwrap();

    assert_eq!(
        hub.task_state_signal().get()[0].metadata,
        Some(metadata.clone())
    );
    let event = source
        .publish(json!({"kind": "received", "owner": "root"}))
        .unwrap();
    let completed = completion_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(completed.task_id, task_id);
    assert_eq!(completed.metadata, Some(metadata.clone()));
    assert_eq!(completed.status, WaitStatus::Matched { event });
    assert_eq!(hub.task_state_signal().get()[0].metadata, Some(metadata));
}

#[test]
fn background_match_updates_completion_and_state_signals() {
    let hub = WaitHub::new().unwrap();
    let source = hub.register_source(descriptor("messages")).unwrap();
    let (completion_tx, completion_rx) = std::sync::mpsc::channel();
    let _completion = hub
        .completion_signal()
        .subscribe_fn(move |snapshot| completion_tx.send(snapshot).unwrap());
    let state_signal = hub.task_state_signal();
    let task_id = hub
        .start_background(
            owner("root"),
            WaitQuery::future(
                WaitSourceSelection::source(source_id("messages")),
                matcher(&[("kind", json!("received"))]),
            ),
            Duration::from_secs(1),
        )
        .unwrap();
    let pending = state_signal.get();
    assert!(matches!(
        pending.as_slice(),
        [WaitTaskSnapshot {
            status: WaitStatus::Pending,
            ..
        }]
    ));
    assert!(pending[0].started_at_ms > 0);
    assert!(pending[0].deadline_at_ms >= pending[0].started_at_ms);
    assert_eq!(pending[0].metadata, None);
    assert_eq!(pending[0].completed_at_ms, None);
    let event = source
        .publish(json!({"kind": "received", "owner": "root"}))
        .unwrap();
    let completed = completion_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(completed.task_id, task_id);
    assert_eq!(completed.status, WaitStatus::Matched { event });
    assert_eq!(completed.metadata, None);
    assert!(completed.completed_at_ms.is_some());
    assert_eq!(hub.task_status(&owner("root"), task_id).unwrap(), completed);
    assert_eq!(state_signal.get(), vec![completed]);
}

#[test]
fn background_wait_handles_retained_publication_without_stale_task_state() {
    let hub = WaitHub::new().unwrap();
    let source = hub.register_source(descriptor("messages")).unwrap();
    let sequence = hub.current_sequence();
    let event = source
        .publish(json!({"kind": "received", "owner": "root"}))
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let _subscription = hub
        .completion_signal()
        .subscribe_fn(move |snapshot| tx.send(snapshot).unwrap());
    let task_id = hub
        .start_background(
            owner("root"),
            WaitQuery::after(
                WaitSourceSelection::source(source_id("messages")),
                matcher(&[("kind", json!("received"))]),
                sequence,
            ),
            Duration::from_secs(1),
        )
        .unwrap();
    let completed = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(completed.task_id, task_id);
    assert_eq!(completed.status, WaitStatus::Matched { event });
    assert_eq!(hub.task_state_signal().get(), vec![completed]);
}

#[test]
fn foreground_timer_elapsed_is_distinct_from_monitored_timeout() {
    let hub = WaitHub::new().unwrap();
    let _source = hub.register_source(descriptor("messages")).unwrap();
    assert_eq!(hub.wait_timer(Duration::ZERO), Ok(WaitStatus::Elapsed));
    assert_eq!(
        hub.wait(
            WaitQuery::future(
                WaitSourceSelection::source(source_id("messages")),
                WaitMatcher::empty(),
            ),
            Duration::ZERO,
        ),
        Ok(WaitStatus::TimedOut)
    );
}

#[test]
fn background_timer_appears_in_state_and_completes_as_elapsed() {
    let hub = WaitHub::new().unwrap();
    let owner = owner("root");
    let state_signal = hub.task_state_signal();
    let (tx, rx) = std::sync::mpsc::channel();
    let _subscription = hub
        .completion_signal()
        .subscribe_fn(move |snapshot| tx.send(snapshot).unwrap());
    let task_id = hub
        .start_background_timer(owner.clone(), Duration::from_millis(10))
        .unwrap();
    let pending = hub.task_status(&owner, task_id).unwrap();
    assert_eq!(pending.status, WaitStatus::Pending);
    assert!(pending.started_at_ms > 0);
    assert!(pending.deadline_at_ms >= pending.started_at_ms.saturating_add(10));
    assert_eq!(pending.metadata, None);
    assert_eq!(pending.completed_at_ms, None);
    assert_eq!(state_signal.get(), vec![pending]);

    let completed = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(completed.task_id, task_id);
    assert_eq!(completed.status, WaitStatus::Elapsed);
    assert_eq!(completed.metadata, None);
    assert!(completed.completed_at_ms.is_some());
    assert_eq!(hub.task_status(&owner, task_id).unwrap(), completed);
    assert_eq!(state_signal.get(), vec![completed]);
}

#[test]
fn background_timer_metadata_replays_and_completes() {
    let hub = WaitHub::new().unwrap();
    let metadata = WaitTaskMetadata::new("timer", "deadline").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let _subscription = hub
        .completion_signal()
        .subscribe_fn(move |snapshot| tx.send(snapshot).unwrap());
    let task_id = hub
        .start_background_timer_with_metadata(
            owner("root"),
            Duration::from_millis(10),
            Some(metadata.clone()),
        )
        .unwrap();
    assert_eq!(
        hub.task_state_signal().get()[0].metadata,
        Some(metadata.clone())
    );
    let completed = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(completed.task_id, task_id);
    assert_eq!(completed.metadata, Some(metadata));
    assert_eq!(completed.status, WaitStatus::Elapsed);
}

#[test]
fn background_timer_can_be_cancelled_by_its_owner() {
    let hub = WaitHub::new().unwrap();
    let owner = owner("root");
    let task_id = hub
        .start_background_timer(owner.clone(), Duration::from_secs(1))
        .unwrap();
    let cancelled = hub.cancel(&owner, task_id).unwrap();
    assert_eq!(cancelled.status, WaitStatus::Cancelled);
    assert!(cancelled.completed_at_ms.is_some());
}

#[test]
fn background_timeout_completes_automatically() {
    let hub = WaitHub::new().unwrap();
    let _source = hub.register_source(descriptor("messages")).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let _subscription = hub
        .completion_signal()
        .subscribe_fn(move |snapshot| tx.send(snapshot).unwrap());
    let task_id = hub
        .start_background(
            owner("root"),
            WaitQuery::future(
                WaitSourceSelection::source(source_id("messages")),
                WaitMatcher::empty(),
            ),
            Duration::from_millis(10),
        )
        .unwrap();
    let snapshot = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(snapshot.task_id, task_id);
    assert_eq!(snapshot.status, WaitStatus::TimedOut);
}

#[test]
fn cancellation_is_owner_scoped_and_emits_once() {
    let hub = WaitHub::new().unwrap();
    let _source = hub.register_source(descriptor("messages")).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let _subscription = hub
        .completion_signal()
        .subscribe_fn(move |snapshot| tx.send(snapshot).unwrap());
    let task_id = hub
        .start_background(
            owner("alpha"),
            WaitQuery::future(
                WaitSourceSelection::source(source_id("messages")),
                WaitMatcher::empty(),
            ),
            Duration::from_secs(1),
        )
        .unwrap();
    assert_eq!(
        hub.task_status(&owner("beta"), task_id),
        Err(WaitError::TaskNotFound)
    );
    assert_eq!(
        hub.cancel(&owner("beta"), task_id),
        Err(WaitError::TaskNotFound)
    );
    let cancelled = hub.cancel(&owner("alpha"), task_id).unwrap();
    assert_eq!(cancelled.status, WaitStatus::Cancelled);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), cancelled);
    assert_eq!(hub.cancel(&owner("alpha"), task_id).unwrap(), cancelled);
    assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
}

#[test]
fn privileged_cancellation_ignores_owner_for_pending_tasks() {
    let hub = WaitHub::new().unwrap();
    let _source = hub.register_source(descriptor("messages")).unwrap();
    let task_id = hub
        .start_background(
            owner("alpha"),
            WaitQuery::future(
                WaitSourceSelection::source(source_id("messages")),
                WaitMatcher::empty(),
            ),
            Duration::from_secs(1),
        )
        .unwrap();

    let cancelled = hub.cancel_any(task_id).unwrap();
    assert_eq!(cancelled.status, WaitStatus::Cancelled);
    assert_eq!(hub.cancel_any(task_id).unwrap(), cancelled);
    assert_eq!(
        hub.cancel_any(WaitTaskId::new()),
        Err(WaitError::TaskNotFound)
    );
}

#[test]
fn composite_query_matches_independent_source_and_matcher_clauses() {
    let hub = WaitHub::new().unwrap();
    let messages = hub.register_source(descriptor("messages")).unwrap();
    let processes = hub
        .register_source(
            WaitSourceDescriptor::new(source_id("processes"), ["status"], ["status"]).unwrap(),
        )
        .unwrap();
    let sequence = hub.current_sequence();
    messages
        .publish(json!({"kind": "received", "owner": "other"}))
        .unwrap();
    let expected = processes.publish(json!({"status": "completed"})).unwrap();
    let query = WaitQuery::any_of_after(
        [
            clause("messages", &[("owner", json!("root"))]),
            clause("processes", &[("status", json!("completed"))]),
        ],
        sequence,
    )
    .unwrap();
    assert_eq!(
        hub.wait(query, Duration::ZERO).unwrap(),
        WaitStatus::Matched { event: expected }
    );
}

#[test]
fn closed_composite_clause_does_not_finish_while_another_can_match() {
    let hub = WaitHub::new().unwrap();
    let messages = hub.register_source(descriptor("messages")).unwrap();
    let processes = hub
        .register_source(
            WaitSourceDescriptor::new(source_id("processes"), ["status"], ["status"]).unwrap(),
        )
        .unwrap();
    messages.close();
    let query = WaitQuery::any_of([
        clause("messages", &[("kind", json!("received"))]),
        clause("processes", &[("status", json!("completed"))]),
    ])
    .unwrap();
    let owner = owner("root");
    let (tx, rx) = std::sync::mpsc::channel();
    let _subscription = hub
        .completion_signal()
        .subscribe_fn(move |snapshot| tx.send(snapshot).unwrap());
    let task_id = hub
        .start_background(owner.clone(), query, Duration::from_secs(1))
        .unwrap();
    assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
    assert_eq!(
        hub.task_status(&owner, task_id).unwrap().status,
        WaitStatus::Pending
    );

    let expected = processes.publish(json!({"status": "completed"})).unwrap();
    let completed = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(completed.status, WaitStatus::Matched { event: expected });
}

#[test]
fn composite_query_chooses_lowest_matching_retained_sequence() {
    let hub = WaitHub::new().unwrap();
    let messages = hub.register_source(descriptor("messages")).unwrap();
    let processes = hub
        .register_source(
            WaitSourceDescriptor::new(source_id("processes"), ["status"], ["status"]).unwrap(),
        )
        .unwrap();
    let sequence = hub.current_sequence();
    let earliest = processes.publish(json!({"status": "completed"})).unwrap();
    messages
        .publish(json!({"kind": "received", "owner": "root"}))
        .unwrap();
    let query = WaitQuery::any_of_after(
        [
            clause("messages", &[("owner", json!("root"))]),
            clause("processes", &[("status", json!("completed"))]),
        ],
        sequence,
    )
    .unwrap();
    assert_eq!(
        hub.wait(query, Duration::ZERO).unwrap(),
        WaitStatus::Matched { event: earliest }
    );
}

#[test]
fn composite_query_bounds_and_serde_are_enforced() {
    assert_eq!(
        WaitQuery::any_of(Vec::<WaitClause>::new()),
        Err(WaitError::EmptyClauses)
    );
    let clauses = (0..=MAX_WAIT_CLAUSES)
        .map(|_| clause("messages", &[]))
        .collect::<Vec<_>>();
    assert_eq!(
        WaitQuery::any_of(clauses),
        Err(WaitError::TooManyClauses {
            max: MAX_WAIT_CLAUSES
        })
    );

    let query = WaitQuery::any_of_after(
        [
            clause("messages", &[("owner", json!("root"))]),
            clause("processes", &[("status", json!("completed"))]),
        ],
        7,
    )
    .unwrap();
    let encoded = serde_json::to_value(&query).unwrap();
    assert_eq!(encoded["clauses"].as_array().unwrap().len(), 2);
    assert_eq!(encoded["after_sequence"], 7);
    assert_eq!(serde_json::from_value::<WaitQuery>(encoded).unwrap(), query);
    assert!(
        serde_json::from_value::<WaitQuery>(json!({
            "clauses": [],
            "after_sequence": null
        }))
        .is_err()
    );
}

#[test]
fn each_composite_clause_is_validated_against_its_own_descriptor() {
    let hub = WaitHub::new().unwrap();
    let _messages = hub.register_source(descriptor("messages")).unwrap();
    let _processes = hub
        .register_source(
            WaitSourceDescriptor::new(source_id("processes"), ["status"], ["status"]).unwrap(),
        )
        .unwrap();
    let query = WaitQuery::any_of([
        clause("messages", &[("kind", json!("received"))]),
        clause("processes", &[("owner", json!("root"))]),
    ])
    .unwrap();
    assert_eq!(
        hub.wait(query, Duration::ZERO),
        Err(WaitError::UnknownMatcherField("owner".into()))
    );
}

#[test]
fn any_selection_matches_only_compatible_sources() {
    let hub = WaitHub::new().unwrap();
    let messages = hub.register_source(descriptor("messages")).unwrap();
    let processes = hub
        .register_source(
            WaitSourceDescriptor::new(source_id("processes"), ["status"], ["status"]).unwrap(),
        )
        .unwrap();
    let sequence = hub.current_sequence();
    messages
        .publish(json!({"kind": "received", "owner": "root"}))
        .unwrap();
    let process_event = processes.publish(json!({"status": "completed"})).unwrap();
    let matched = hub
        .wait(
            WaitQuery::after(
                WaitSourceSelection::Any,
                matcher(&[("status", json!("completed"))]),
                sequence,
            ),
            Duration::ZERO,
        )
        .unwrap();
    assert_eq!(
        matched,
        WaitStatus::Matched {
            event: process_event
        }
    );
}

#[test]
fn exact_scalar_matcher_does_not_coerce_values() {
    let hub = WaitHub::new().unwrap();
    let source = hub.register_source(descriptor("messages")).unwrap();
    let sequence = hub.current_sequence();
    source
        .publish(json!({"kind": "1", "owner": "root"}))
        .unwrap();
    assert_eq!(
        hub.wait(
            WaitQuery::after(
                WaitSourceSelection::source(source_id("messages")),
                matcher(&[("kind", json!(1))]),
                sequence,
            ),
            Duration::ZERO,
        ),
        Ok(WaitStatus::TimedOut)
    );
}

#[test]
fn matcher_and_publication_validation_reject_unsafe_shapes() {
    let descriptor_error =
        WaitSourceDescriptor::new(source_id("bad"), ["public"], ["private"]).unwrap_err();
    assert_eq!(
        descriptor_error,
        WaitError::MatcherFieldNotPublic("private".into())
    );
    let non_scalar = WaitMatcher::new(BTreeMap::from([("kind".into(), json!(["a"]))]));
    assert_eq!(non_scalar, Err(WaitError::NonScalarMatcher("kind".into())));

    let hub = WaitHub::new().unwrap();
    let source = hub.register_source(descriptor("messages")).unwrap();
    let unknown_matcher = hub.wait(
        WaitQuery::future(
            WaitSourceSelection::source(source_id("messages")),
            matcher(&[("value", json!(1))]),
        ),
        Duration::ZERO,
    );
    assert_eq!(
        unknown_matcher,
        Err(WaitError::UnknownMatcherField("value".into()))
    );
    assert_eq!(
        source.publish(json!({"secret": "no"})),
        Err(WaitError::UnknownPayloadField("secret".into()))
    );
    assert_eq!(
        source.publish(json!({"kind": ["received"]})),
        Err(WaitError::NonScalarPublishedMatcherField("kind".into()))
    );
    assert_eq!(
        source.publish(json!(["not", "an", "object"])),
        Err(WaitError::PayloadMustBeObject)
    );
}

#[test]
fn active_task_limits_are_enforced_per_owner_and_hub() {
    let hub = WaitHub::test_with_limits(8, 4, 1, 2).unwrap();
    let _source = hub.register_source(descriptor("messages")).unwrap();
    let query = || {
        WaitQuery::future(
            WaitSourceSelection::source(source_id("messages")),
            WaitMatcher::empty(),
        )
    };
    hub.start_background(owner("alpha"), query(), Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        hub.start_background(owner("alpha"), query(), Duration::from_secs(1)),
        Err(WaitError::OwnerTaskLimit)
    );
    hub.start_background(owner("beta"), query(), Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        hub.start_background(owner("gamma"), query(), Duration::from_secs(1)),
        Err(WaitError::HubTaskLimit)
    );
}

#[test]
fn completed_snapshots_are_bounded() {
    let hub = WaitHub::test_with_limits(8, 2, 4, 4).unwrap();
    let _source = hub.register_source(descriptor("messages")).unwrap();
    let owner = owner("root");
    let mut ids = Vec::new();
    for _ in 0..3 {
        let task_id = hub
            .start_background(
                owner.clone(),
                WaitQuery::future(
                    WaitSourceSelection::source(source_id("messages")),
                    WaitMatcher::empty(),
                ),
                Duration::from_secs(1),
            )
            .unwrap();
        hub.cancel(&owner, task_id).unwrap();
        ids.push(task_id);
    }
    assert_eq!(
        hub.task_status(&owner, ids[0]),
        Err(WaitError::TaskNotFound)
    );
    assert_eq!(hub.task_state_signal().get().len(), 2);
}

#[test]
fn retained_events_are_bounded() {
    let hub = WaitHub::test_with_limits(2, 4, 4, 4).unwrap();
    let source = hub.register_source(descriptor("messages")).unwrap();
    source
        .publish(json!({"kind": "first", "owner": "root"}))
        .unwrap();
    source
        .publish(json!({"kind": "second", "owner": "root"}))
        .unwrap();
    source
        .publish(json!({"kind": "third", "owner": "root"}))
        .unwrap();
    assert_eq!(
        hub.wait(
            WaitQuery::after(
                WaitSourceSelection::source(source_id("messages")),
                matcher(&[("kind", json!("first"))]),
                0,
            ),
            Duration::ZERO,
        ),
        Ok(WaitStatus::TimedOut)
    );
    let matched = hub
        .wait(
            WaitQuery::after(
                WaitSourceSelection::source(source_id("messages")),
                matcher(&[("kind", json!("second"))]),
                0,
            ),
            Duration::ZERO,
        )
        .unwrap();
    assert!(matches!(
        matched,
        WaitStatus::Matched {
            event: WaitEvent { sequence: 2, .. }
        }
    ));
}

#[test]
fn source_close_completes_waits_and_stale_handles_cannot_close_replacement() {
    let hub = WaitHub::new().unwrap();
    let source = hub.register_source(descriptor("messages")).unwrap();
    let stale = source.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let _subscription = hub
        .completion_signal()
        .subscribe_fn(move |snapshot| tx.send(snapshot).unwrap());
    let task_id = hub
        .start_background(
            owner("root"),
            WaitQuery::future(
                WaitSourceSelection::source(source_id("messages")),
                WaitMatcher::empty(),
            ),
            Duration::from_secs(1),
        )
        .unwrap();
    source.close();
    let closed = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(closed.task_id, task_id);
    assert_eq!(closed.status, WaitStatus::SourceClosed);

    let replacement = hub.register_source(descriptor("messages")).unwrap();
    drop(stale);
    assert!(replacement.publish(json!({"kind": "new"})).is_ok());
}

#[test]
fn any_matcher_requires_one_source_to_authorize_every_field() {
    let hub = WaitHub::new().unwrap();
    let _one = hub
        .register_source(WaitSourceDescriptor::new(source_id("one"), ["kind"], ["kind"]).unwrap())
        .unwrap();
    let _two = hub
        .register_source(WaitSourceDescriptor::new(source_id("two"), ["owner"], ["owner"]).unwrap())
        .unwrap();
    let result = hub.wait(
        WaitQuery::future(
            WaitSourceSelection::Any,
            matcher(&[("kind", json!("ready")), ("owner", json!("root"))]),
        ),
        Duration::ZERO,
    );
    assert!(matches!(result, Err(WaitError::UnknownMatcherField(_))));
}

#[test]
fn serde_cannot_bypass_identifier_or_matcher_validation() {
    assert!(serde_json::from_value::<WaitOwnerId>(json!("")).is_err());
    assert!(serde_json::from_value::<WaitMatcher>(json!({"kind": ["ready"]})).is_err());
    assert!(
        serde_json::from_value::<WaitSourceDescriptor>(json!({
            "source_id": "source",
            "public_fields": ["public"],
            "matcher_fields": ["private"]
        }))
        .is_err()
    );
}

#[test]
fn identifiers_and_empty_source_sets_are_rejected() {
    assert!(matches!(
        WaitOwnerId::new(""),
        Err(WaitError::InvalidIdentifier { kind: "owner", .. })
    ));
    let hub = WaitHub::new().unwrap();
    assert_eq!(
        hub.wait(
            WaitQuery::future(WaitSourceSelection::sources([]), WaitMatcher::empty()),
            Duration::ZERO
        ),
        Err(WaitError::EmptySourceSelection)
    );
}
