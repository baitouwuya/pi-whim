use super::*;
use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

fn push<T>(values: &Arc<Mutex<Vec<T>>>, value: T) {
    values.lock().expect("test mutex poisoned").push(value);
}

#[test]
fn signal_snapshots_listeners_and_defers_reentrant_emission() {
    let (signal, emitter) = Signal::<i32>::channel();
    let first_values = Arc::new(Mutex::new(Vec::new()));
    let second_values = Arc::new(Mutex::new(Vec::new()));
    let third_values = Arc::new(Mutex::new(Vec::new()));
    let second_subscription = Arc::new(Mutex::new(None::<Subscription>));
    let retained = Arc::new(Mutex::new(Vec::<Subscription>::new()));
    let connected = Arc::new(AtomicBool::new(false));

    let first = {
        let first_values = first_values.clone();
        let second_subscription = second_subscription.clone();
        let retained = retained.clone();
        let connected = connected.clone();
        let emitter = emitter.clone();
        let signal_for_callback = signal.clone();
        let third_values = third_values.clone();
        signal.subscribe(Observer::new(move |value| {
            push(&first_values, value);
            if value == 1 && !connected.swap(true, Ordering::AcqRel) {
                if let Some(subscription) = second_subscription
                    .lock()
                    .expect("test mutex poisoned")
                    .take()
                {
                    subscription.unsubscribe();
                }
                let third = signal_for_callback.subscribe(Observer::new({
                    let third_values = third_values.clone();
                    move |value| push(&third_values, value)
                }));
                retained.lock().expect("test mutex poisoned").push(third);
                emitter.emit(2);
            }
        }))
    };
    let second = {
        let second_values = second_values.clone();
        signal.subscribe(Observer::new(move |value| push(&second_values, value)))
    };
    *second_subscription.lock().expect("test mutex poisoned") = Some(second);

    emitter.emit(1);

    assert_eq!(
        *first_values.lock().expect("test mutex poisoned"),
        vec![1, 2]
    );
    assert_eq!(*second_values.lock().expect("test mutex poisoned"), vec![1]);
    assert_eq!(*third_values.lock().expect("test mutex poisoned"), vec![2]);
    assert_eq!(signal.listener_count(), 2);
    drop(first);
    retained.lock().expect("test mutex poisoned").clear();
}

#[test]
fn signal_restores_drain_state_after_observer_panic() {
    let (signal, emitter) = Signal::<i32>::channel();
    let panicking = signal.subscribe_fn(|_| panic!("expected observer panic"));

    let result = catch_unwind(AssertUnwindSafe(|| emitter.emit(1)));
    assert!(result.is_err());
    panicking.unsubscribe();

    let values = Arc::new(Mutex::new(Vec::new()));
    let _subscription = signal.subscribe_fn({
        let values = values.clone();
        move |value| push(&values, value)
    });
    assert!(emitter.emit(2));
    assert_eq!(*values.lock().expect("test mutex poisoned"), vec![2]);
}

#[test]
fn signal_terminal_events_are_single_shot_and_replayed_to_late_subscribers() {
    let (signal, emitter) = Signal::<i32, u8>::channel();
    let events = Arc::new(Mutex::new(Vec::<SignalEvent<i32, u8>>::new()));
    let _subscription = signal.subscribe_event({
        let events = events.clone();
        move |event| push(&events, event)
    });

    assert!(emitter.emit(1));
    assert!(emitter.error(7));
    assert!(!emitter.emit(2));
    assert!(!emitter.complete());
    assert_eq!(
        *events.lock().expect("test mutex poisoned"),
        vec![SignalEvent::Next(1), SignalEvent::Error(7)]
    );

    let late_events = Arc::new(Mutex::new(Vec::new()));
    let _late = signal.subscribe_event({
        let late_events = late_events.clone();
        move |event| push(&late_events, event)
    });
    assert_eq!(
        *late_events.lock().expect("test mutex poisoned"),
        vec![SignalEvent::Error(7)]
    );
}

#[test]
fn state_signal_sends_initial_value_and_scope_disconnects() {
    let state = StateSignal::<i32>::new(5);
    let values = Arc::new(Mutex::new(Vec::new()));
    let scope = SubscriptionScope::new();
    let subscription = state.subscribe_fn({
        let values = values.clone();
        move |value| push(&values, value)
    });
    scope.add(subscription);

    assert_eq!(scope.len(), 1);
    assert_eq!(state.get(), 5);
    assert!(state.set(7));
    assert_eq!(*values.lock().expect("test mutex poisoned"), vec![5, 7]);

    scope.unsubscribe_all();
    assert!(scope.is_empty());
    assert!(state.set(9));
    assert_eq!(*values.lock().expect("test mutex poisoned"), vec![5, 7]);
}

#[test]
fn state_signal_restores_replay_state_after_observer_panic() {
    let state = StateSignal::<i32>::new(0);
    let values = Arc::new(Mutex::new(Vec::new()));
    let _subscription = state.subscribe_fn({
        let values = values.clone();
        move |value| {
            if value == 1 {
                panic!("expected state observer panic");
            }
            push(&values, value);
        }
    });

    let result = catch_unwind(AssertUnwindSafe(|| state.set(1)));
    assert!(result.is_err());
    assert!(state.set(2));
    assert_eq!(*values.lock().expect("test mutex poisoned"), vec![0, 2]);
}

#[test]
fn typed_chains_are_stable_and_apply_failure_policies() {
    let gates = GateChain::<i32, &'static str, &'static str>::allow_on_error();
    let first = gates.register(|_| Err("ignored"));
    let second = gates.register(|value| {
        Ok(if *value == 3 {
            GateDecision::Allow
        } else {
            GateDecision::Deny("not-three")
        })
    });
    assert_eq!(gates.evaluate(&3), Ok(GateDecision::Allow));
    assert_eq!(gates.evaluate(&4), Ok(GateDecision::Deny("not-three")));
    drop(first);
    assert_eq!(gates.len(), 1);
    drop(second);
    assert!(gates.is_empty());

    let propagating = GateChain::<i32, &'static str, &'static str>::propagating();
    let _failed = propagating.register(|_| Err("failed"));
    assert_eq!(propagating.evaluate(&1), Err("failed"));

    let denying = GateChain::<i32, &'static str, &'static str>::deny_on_error("error-deny");
    let _denied = denying.register(|_| Err("failed"));
    assert_eq!(denying.evaluate(&1), Ok(GateDecision::Deny("error-deny")));

    let transforms = TransformChain::<i32, &'static str>::preserving();
    let first = transforms.register(|_| Err("ignored"));
    let _second = transforms.register(|value| Ok(value + 1));
    assert_eq!(transforms.apply(4), Ok(5));
    drop(first);
    assert_eq!(transforms.apply(4), Ok(5));

    let failing = TransformChain::<i32, &'static str>::propagating();
    let _failed = failing.register(|_| Err("failed"));
    assert_eq!(failing.apply(4), Err("failed"));
}

#[test]
fn test_scheduler_uses_virtual_deadlines_and_stable_order() {
    let scheduler = TestScheduler::new();
    let observed = Arc::new(Mutex::new(Vec::<(Duration, u8)>::new()));

    let cancelled = schedule(&scheduler, Duration::from_secs(3), || {
        panic!("cancelled task ran")
    });
    cancelled.cancel();
    let observed_for_two = observed.clone();
    let scheduler_for_two = scheduler.clone();
    schedule(&scheduler, Duration::from_secs(2), move || {
        push(&observed_for_two, (scheduler_for_two.now(), 2));
    });
    let observed_for_first = observed.clone();
    let scheduler_for_first = scheduler.clone();
    let scheduler_for_nested = scheduler.clone();
    let scheduler_for_nested_schedule = scheduler_for_nested.clone();
    schedule(&scheduler, Duration::from_secs(5), move || {
        push(&observed_for_first, (scheduler_for_first.now(), 1));
        let observed_for_nested = observed_for_first.clone();
        let scheduler_for_nested_callback = scheduler_for_nested.clone();
        schedule(
            &scheduler_for_nested_schedule,
            Duration::from_secs(1),
            move || {
                push(
                    &observed_for_nested,
                    (scheduler_for_nested_callback.now(), 3),
                );
            },
        );
    });
    let observed_for_second = observed.clone();
    let scheduler_for_second = scheduler.clone();
    schedule(&scheduler, Duration::from_secs(5), move || {
        push(&observed_for_second, (scheduler_for_second.now(), 4));
    });

    scheduler.advance_to(Duration::from_secs(5));
    assert_eq!(scheduler.now(), Duration::from_secs(5));
    assert_eq!(
        *observed.lock().expect("test mutex poisoned"),
        vec![
            (Duration::from_secs(2), 2),
            (Duration::from_secs(5), 1),
            (Duration::from_secs(5), 4),
        ]
    );

    scheduler.advance_to(Duration::from_secs(10));
    assert_eq!(scheduler.now(), Duration::from_secs(10));
    assert_eq!(
        *observed.lock().expect("test mutex poisoned"),
        vec![
            (Duration::from_secs(2), 2),
            (Duration::from_secs(5), 1),
            (Duration::from_secs(5), 4),
            (Duration::from_secs(6), 3),
        ]
    );
}

#[test]
fn thread_scheduler_runs_tasks_and_shuts_down_without_blocking() {
    let mut scheduler = ThreadScheduler::try_new().expect("thread scheduler should start in tests");
    let (sender, receiver) = mpsc::channel();
    schedule(&scheduler, Duration::from_millis(5), move || {
        sender.send(17).expect("receiver dropped");
    });
    assert_eq!(
        receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("scheduler task did not run"),
        17
    );
    scheduler.shutdown();
}

#[test]
fn thread_scheduler_survives_task_panic() {
    let mut scheduler = ThreadScheduler::try_new().expect("thread scheduler should start in tests");
    schedule(&scheduler, Duration::ZERO, || {
        panic!("expected scheduled task panic");
    });

    let (sender, receiver) = mpsc::channel();
    schedule(&scheduler, Duration::from_millis(5), move || {
        sender.send(23).expect("receiver dropped");
    });
    assert_eq!(
        receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("worker did not survive task panic"),
        23
    );
    scheduler.shutdown();
}

#[test]
fn delay_debounce_throttle_and_timeout_use_virtual_time() {
    let (source, emitter) = Signal::<i32>::channel();
    let scheduler = TestScheduler::new();
    let delayed = source.delay(Duration::from_secs(5), scheduler.clone());
    let delayed_events = Arc::new(Mutex::new(Vec::new()));
    let _delayed_subscription = delayed.subscribe_event({
        let delayed_events = delayed_events.clone();
        move |event| push(&delayed_events, event)
    });
    emitter.emit(1);
    scheduler.advance(Duration::from_secs(4));
    assert!(
        delayed_events
            .lock()
            .expect("test mutex poisoned")
            .is_empty()
    );
    scheduler.advance(Duration::from_secs(1));
    assert_eq!(
        *delayed_events.lock().expect("test mutex poisoned"),
        vec![SignalEvent::Next(1)]
    );
    emitter.complete();
    scheduler.advance(Duration::from_secs(5));
    assert_eq!(
        *delayed_events.lock().expect("test mutex poisoned"),
        vec![SignalEvent::Next(1), SignalEvent::Complete]
    );

    let (source, emitter) = Signal::<i32>::channel();
    let scheduler = TestScheduler::new();
    let debounced = source.debounce(Duration::from_secs(5), scheduler.clone());
    let debounced_values = Arc::new(Mutex::new(Vec::new()));
    let _subscription = debounced.subscribe_fn({
        let debounced_values = debounced_values.clone();
        move |value| push(&debounced_values, value)
    });
    emitter.emit(1);
    scheduler.advance(Duration::from_secs(4));
    emitter.emit(2);
    scheduler.advance(Duration::from_secs(4));
    assert!(
        debounced_values
            .lock()
            .expect("test mutex poisoned")
            .is_empty()
    );
    scheduler.advance(Duration::from_secs(1));
    assert_eq!(
        *debounced_values.lock().expect("test mutex poisoned"),
        vec![2]
    );

    let (source, emitter) = Signal::<i32>::channel();
    let scheduler = TestScheduler::new();
    let throttled = source.throttle_leading_trailing(Duration::from_secs(10), scheduler.clone());
    let throttled_values = Arc::new(Mutex::new(Vec::new()));
    let _subscription = throttled.subscribe_fn({
        let throttled_values = throttled_values.clone();
        move |value| push(&throttled_values, value)
    });
    emitter.emit(1);
    emitter.emit(2);
    emitter.emit(3);
    assert_eq!(
        *throttled_values.lock().expect("test mutex poisoned"),
        vec![1]
    );
    scheduler.advance(Duration::from_secs(10));
    assert_eq!(
        *throttled_values.lock().expect("test mutex poisoned"),
        vec![1, 3]
    );
    emitter.complete();

    let (source, emitter) = Signal::<i32>::channel();
    let scheduler = TestScheduler::new();
    let trailing = source.throttle(
        Duration::from_secs(10),
        scheduler.clone(),
        ThrottleOptions::trailing_only(),
    );
    let trailing_values = Arc::new(Mutex::new(Vec::new()));
    let _subscription = trailing.subscribe_fn({
        let trailing_values = trailing_values.clone();
        move |value| push(&trailing_values, value)
    });
    emitter.emit(1);
    emitter.emit(2);
    assert!(
        trailing_values
            .lock()
            .expect("test mutex poisoned")
            .is_empty()
    );
    scheduler.advance(Duration::from_secs(10));
    assert_eq!(
        *trailing_values.lock().expect("test mutex poisoned"),
        vec![2]
    );

    let (source, emitter) = Signal::<i32>::channel();
    let scheduler = TestScheduler::new();
    let timed = source.timeout(Duration::from_secs(5), scheduler.clone());
    let timed_events = Arc::new(Mutex::new(Vec::new()));
    let _subscription = timed.subscribe_event({
        let timed_events = timed_events.clone();
        move |event| push(&timed_events, event)
    });
    scheduler.advance(Duration::from_secs(5));
    assert!(matches!(
        timed_events.lock().expect("test mutex poisoned").as_slice(),
        [SignalEvent::Error(SignalError::Timeout(duration))] if *duration == Duration::from_secs(5)
    ));
    assert_eq!(source.listener_count(), 0);
    assert!(emitter.emit(9));
}

#[test]
fn combining_operators_preserve_pairs_and_completion() {
    let (left, left_emitter) = Signal::<i32>::channel();
    let (right, right_emitter) = Signal::<i32>::channel();
    let merged = Signal::merge(vec![left.clone(), right.clone()]);
    let merged_events = Arc::new(Mutex::new(Vec::new()));
    let _merged_subscription = merged.subscribe_event({
        let merged_events = merged_events.clone();
        move |event| push(&merged_events, event)
    });
    left_emitter.emit(1);
    right_emitter.emit(2);
    left_emitter.complete();
    assert!(!merged.is_terminated());
    right_emitter.complete();
    assert_eq!(
        *merged_events.lock().expect("test mutex poisoned"),
        vec![
            SignalEvent::Next(1),
            SignalEvent::Next(2),
            SignalEvent::Complete,
        ]
    );

    let (left, left_emitter) = Signal::<i32>::channel();
    let (right, right_emitter) = Signal::<i32>::channel();
    let zipped = left.zip(&right);
    let zipped_values = Arc::new(Mutex::new(Vec::new()));
    let _subscription = zipped.subscribe_fn({
        let zipped_values = zipped_values.clone();
        move |value| push(&zipped_values, value)
    });
    left_emitter.emit(1);
    left_emitter.emit(2);
    right_emitter.emit(10);
    right_emitter.emit(20);
    assert_eq!(
        *zipped_values.lock().expect("test mutex poisoned"),
        vec![(1, 10), (2, 20)]
    );

    let (left, left_emitter) = Signal::<i32>::channel();
    let (right, right_emitter) = Signal::<i32>::channel();
    let combined = left.combine_latest(&right);
    let combined_values = Arc::new(Mutex::new(Vec::new()));
    let _subscription = combined.subscribe_fn({
        let combined_values = combined_values.clone();
        move |value| push(&combined_values, value)
    });
    left_emitter.emit(1);
    assert!(
        combined_values
            .lock()
            .expect("test mutex poisoned")
            .is_empty()
    );
    right_emitter.emit(10);
    right_emitter.emit(11);
    left_emitter.complete();
    assert!(!combined.is_terminated());
    right_emitter.complete();
    assert_eq!(
        *combined_values.lock().expect("test mutex poisoned"),
        vec![(1, 10), (1, 11)]
    );
}

#[test]
fn flattening_operators_bound_and_switch_inner_lifetimes() {
    let (outer, outer_emitter) = Signal::<i32>::channel();
    let inner_emitters = Arc::new(Mutex::new(Vec::<SignalEmitter<i32>>::new()));
    let flat = outer.flat_map(2, {
        let inner_emitters = inner_emitters.clone();
        move |_| {
            let (inner, emitter) = Signal::channel();
            inner_emitters
                .lock()
                .expect("test mutex poisoned")
                .push(emitter);
            inner
        }
    });
    let flat_values = Arc::new(Mutex::new(Vec::new()));
    let _flat_subscription = flat.subscribe_fn({
        let flat_values = flat_values.clone();
        move |value| push(&flat_values, value)
    });
    outer_emitter.emit(1);
    outer_emitter.emit(2);
    let emitters = inner_emitters.lock().expect("test mutex poisoned").clone();
    emitters[0].emit(10);
    emitters[1].emit(20);
    emitters[0].complete();
    emitters[1].complete();
    outer_emitter.complete();
    assert_eq!(
        *flat_values.lock().expect("test mutex poisoned"),
        vec![10, 20]
    );
    assert!(flat.is_terminated());

    let (outer, outer_emitter) = Signal::<i32>::channel();
    let inner_emitters = Arc::new(Mutex::new(Vec::<SignalEmitter<i32>>::new()));
    let switched = outer.switch_map({
        let inner_emitters = inner_emitters.clone();
        move |_| {
            let (inner, emitter) = Signal::channel();
            inner_emitters
                .lock()
                .expect("test mutex poisoned")
                .push(emitter);
            inner
        }
    });
    let switched_values = Arc::new(Mutex::new(Vec::new()));
    let _switch_subscription = switched.subscribe_fn({
        let switched_values = switched_values.clone();
        move |value| push(&switched_values, value)
    });
    outer_emitter.emit(1);
    outer_emitter.emit(2);
    let emitters = inner_emitters.lock().expect("test mutex poisoned").clone();
    emitters[0].emit(10);
    emitters[1].emit(20);
    emitters[1].complete();
    outer_emitter.complete();
    assert_eq!(
        *switched_values.lock().expect("test mutex poisoned"),
        vec![20]
    );
    assert!(switched.is_terminated());
}

#[test]
fn buffers_apply_finite_policies_and_flush_on_terminal() {
    let (source, emitter) = Signal::<i32>::channel();
    let buffered = source.buffer_count(2, BufferOptions::new(8, OverflowPolicy::DropOldest));
    let events = Arc::new(Mutex::new(Vec::new()));
    let _subscription = buffered.subscribe_event({
        let events = events.clone();
        move |event| push(&events, event)
    });
    emitter.emit(1);
    emitter.emit(2);
    emitter.emit(3);
    emitter.complete();
    assert_eq!(
        *events.lock().expect("test mutex poisoned"),
        vec![
            SignalEvent::Next(vec![1, 2]),
            SignalEvent::Next(vec![3]),
            SignalEvent::Complete,
        ]
    );

    let (source, emitter) = Signal::<i32, u8>::channel();
    let buffered = source.buffer_count(3, BufferOptions::new(1, OverflowPolicy::Error(9)));
    let events = Arc::new(Mutex::new(Vec::new()));
    let _subscription = buffered.subscribe_event({
        let events = events.clone();
        move |event| push(&events, event)
    });
    emitter.emit(1);
    emitter.emit(2);
    assert_eq!(
        *events.lock().expect("test mutex poisoned"),
        vec![SignalEvent::Error(9)]
    );

    let (source, emitter) = Signal::<i32>::channel();
    let scheduler = TestScheduler::new();
    let buffered = source.buffer_time(
        Duration::from_secs(5),
        scheduler.clone(),
        BufferOptions::new(8, OverflowPolicy::DropOldest),
    );
    let values = Arc::new(Mutex::new(Vec::new()));
    let _subscription = buffered.subscribe_fn({
        let values = values.clone();
        move |value| push(&values, value)
    });
    emitter.emit(1);
    scheduler.advance(Duration::from_secs(5));
    assert_eq!(*values.lock().expect("test mutex poisoned"), vec![vec![1]]);
    emitter.emit(2);
    emitter.complete();
    assert_eq!(
        *values.lock().expect("test mutex poisoned"),
        vec![vec![1], vec![2]]
    );

    let (source, emitter) = Signal::<i32>::channel();
    let immediate = source.buffer_time(
        Duration::from_secs(1),
        ImmediateScheduler,
        BufferOptions::new(8, OverflowPolicy::DropOldest),
    );
    let values = Arc::new(Mutex::new(Vec::new()));
    let _subscription = immediate.subscribe_fn({
        let values = values.clone();
        move |value| push(&values, value)
    });
    emitter.emit(4);
    assert_eq!(*values.lock().expect("test mutex poisoned"), vec![vec![4]]);
}

#[test]
fn resilience_and_conflation_have_bounded_terminal_behaviour() {
    let (source, source_emitter) = Signal::<i32, u8>::channel();
    let (fallback, fallback_emitter) = Signal::<i32, u8>::channel();
    let caught = source.catch({
        let fallback = fallback.clone();
        move |_| fallback.clone()
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let _subscription = caught.subscribe_event({
        let events = events.clone();
        move |event| push(&events, event)
    });
    source_emitter.error(1);
    fallback_emitter.emit(8);
    fallback_emitter.complete();
    assert_eq!(
        *events.lock().expect("test mutex poisoned"),
        vec![SignalEvent::Next(8), SignalEvent::Complete]
    );

    let (source, emitter) = Signal::<i32, u8>::channel();
    let scheduler = TestScheduler::new();
    let retried = source.retry(
        RetryPolicy::new(2, RetryBackoff::Immediate),
        scheduler.clone(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let _subscription = retried.subscribe_event({
        let events = events.clone();
        move |event| push(&events, event)
    });
    emitter.error(3);
    scheduler.run_until_idle();
    assert_eq!(
        *events.lock().expect("test mutex poisoned"),
        vec![SignalEvent::Error(3)]
    );

    let (source, emitter) = Signal::<i32>::channel();
    let conflated = source.conflate_latest();
    let values = Arc::new(Mutex::new(Vec::new()));
    let _subscription = conflated.subscribe_fn({
        let values = values.clone();
        move |value| push(&values, value)
    });
    emitter.emit(1);
    emitter.emit(2);
    emitter.emit(3);
    emitter.complete();
    assert_eq!(*values.lock().expect("test mutex poisoned"), vec![1, 2, 3]);
}

#[test]
fn once_take_until_and_distinct_have_expected_boundaries() {
    let (source, emitter) = Signal::<i32>::channel();
    let once = source.once();
    let values = Arc::new(Mutex::new(Vec::new()));
    let _subscription = once.subscribe_fn({
        let values = values.clone();
        move |value| push(&values, value)
    });
    emitter.emit(1);
    emitter.emit(2);
    assert_eq!(*values.lock().expect("test mutex poisoned"), vec![1]);
    assert!(once.is_terminated());

    let (source, emitter) = Signal::<i32>::channel();
    let (notifier, notifier_emitter) = Signal::<()>::channel();
    let taken = source.take_until(&notifier);
    let values = Arc::new(Mutex::new(Vec::new()));
    let _subscription = taken.subscribe_fn({
        let values = values.clone();
        move |value| push(&values, value)
    });
    emitter.emit(1);
    notifier_emitter.emit(());
    emitter.emit(2);
    assert_eq!(*values.lock().expect("test mutex poisoned"), vec![1]);
    assert!(taken.is_terminated());

    let (source, emitter) = Signal::<i32>::channel();
    let distinct = source.distinct_until_changed();
    let values = Arc::new(Mutex::new(Vec::new()));
    let _subscription = distinct.subscribe_fn({
        let values = values.clone();
        move |value| push(&values, value)
    });
    for value in [1, 1, 2, 2, 1] {
        emitter.emit(value);
    }
    assert_eq!(*values.lock().expect("test mutex poisoned"), vec![1, 2, 1]);
}

#[test]
fn immediate_scheduler_executes_once_without_periodic_recursion() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_for_task = observed.clone();
    let task = schedule(&ImmediateScheduler, Duration::from_secs(1), move || {
        push(&observed_for_task, 1);
    });
    assert!(!task.is_cancelled());
    assert_eq!(*observed.lock().expect("test mutex poisoned"), vec![1]);
}

#[test]
fn public_event_types_remain_typed_without_json() {
    let event: SignalEvent<i32, &'static str> = SignalEvent::Next(3);
    assert_eq!(event, SignalEvent::Next(3));
    let error = SignalError::<Infallible>::BufferOverflow;
    assert_eq!(error, SignalError::BufferOverflow);
    let _ = thread::available_parallelism();
}
