//! Bridges pi-whim signals to the GPUI event pump.

use crate::pump::{self, Handler};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};
use gpui::{Context, Task, Window};
use pi_whim_signal::{Observer, Signal, SignalEvent, StateSignal, Subscription};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

/// A reliable signal-to-pump bridge which preserves every notification.
pub struct SignalBridge<T, E> {
    receiver: Receiver<SignalEvent<T, E>>,
    delivery: Arc<OrdinaryDelivery<T, E>>,
    _subscription: Subscription,
}

impl<T, E> SignalBridge<T, E>
where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    /// Subscribes to every event from `signal`.
    pub fn new(signal: &Signal<T, E>) -> Self {
        let (sender, receiver) = unbounded();
        let delivery = Arc::new(OrdinaryDelivery::new(sender));
        let callback_delivery = Arc::clone(&delivery);
        let subscription = signal.subscribe_event(move |event| {
            callback_delivery.forward(event);
        });
        Self {
            receiver,
            delivery,
            _subscription: subscription,
        }
    }
}

impl<T, E> Drop for SignalBridge<T, E> {
    fn drop(&mut self) {
        self.delivery.close();
        self._subscription.unsubscribe();
    }
}

impl<T, E> SignalBridge<T, E> {
    /// Clones the read-only receiver handle.
    ///
    /// Cloned receivers are competing consumers, not broadcast subscribers:
    /// each event is delivered to only one receiver clone.
    pub fn receiver(&self) -> Receiver<SignalEvent<T, E>> {
        self.receiver.clone()
    }

    /// Clones the read-only receiver handle.
    pub fn events(&self) -> Receiver<SignalEvent<T, E>> {
        self.receiver()
    }
}

impl<T, E> SignalBridge<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    /// Starts pumping signal events into a GPUI entity.
    pub fn spawn<V: 'static>(
        &self,
        window: &Window,
        cx: &mut Context<V>,
        handle: Handler<V, SignalEvent<T, E>>,
    ) -> Task<()> {
        pump::spawn(self.receiver(), window, cx, handle)
    }
}

/// A state-signal bridge which keeps only the newest pending value.
pub struct StateSignalBridge<T, E> {
    receiver: Receiver<SignalEvent<T, E>>,
    _subscription: Subscription,
    delivery: Arc<StateDelivery<T, E>>,
    shutdown: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl<T, E> StateSignalBridge<T, E>
where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    /// Subscribes synchronously, preserving the state signal's initial replay.
    ///
    /// Returns an error if the delivery worker cannot be started.
    pub fn new(signal: &StateSignal<T, E>) -> std::io::Result<Self> {
        let (sender, receiver) = bounded(1);
        let (wake_sender, wake_receiver) = bounded(1);
        let (shutdown_sender, shutdown_receiver) = bounded(1);
        let delivery = Arc::new(StateDelivery::new(wake_sender));
        let worker_delivery = Arc::clone(&delivery);
        let callback_sender = sender.clone();
        let worker = thread::Builder::new()
            .name("pi-whim-gpui-state-signal".to_owned())
            .spawn(move || {
                run_state_worker(worker_delivery, wake_receiver, shutdown_receiver, sender)
            })?;

        let callback_delivery = Arc::clone(&delivery);
        let subscription = signal.subscribe(Observer::from_event(move |event| {
            callback_delivery.push(event, &callback_sender);
        }));

        Ok(Self {
            receiver,
            _subscription: subscription,
            delivery,
            shutdown: shutdown_sender,
            worker: Some(worker),
        })
    }
}

impl<T, E> StateSignalBridge<T, E> {
    /// Clones the read-only receiver handle.
    ///
    /// Cloned receivers are competing consumers, not broadcast subscribers:
    /// each event is delivered to only one receiver clone.
    pub fn receiver(&self) -> Receiver<SignalEvent<T, E>> {
        self.receiver.clone()
    }

    /// Clones the read-only receiver handle.
    pub fn events(&self) -> Receiver<SignalEvent<T, E>> {
        self.receiver()
    }
}

impl<T, E> StateSignalBridge<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    /// Starts pumping state events into a GPUI entity.
    pub fn spawn<V: 'static>(
        &self,
        window: &Window,
        cx: &mut Context<V>,
        handle: Handler<V, SignalEvent<T, E>>,
    ) -> Task<()> {
        pump::spawn(self.receiver(), window, cx, handle)
    }
}

impl<T, E> Drop for StateSignalBridge<T, E> {
    fn drop(&mut self) {
        self._subscription.unsubscribe();
        self.delivery.close();
        let _ = self.shutdown.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct StateDelivery<T, E> {
    state: Mutex<PendingState<T, E>>,
    wake: Sender<()>,
}

struct PendingState<T, E> {
    replaying: bool,
    next: Option<T>,
    terminal: Option<SignalEvent<T, E>>,
    finished: bool,
    closed: bool,
}

impl<T, E> StateDelivery<T, E> {
    fn new(wake: Sender<()>) -> Self {
        Self {
            state: Mutex::new(PendingState {
                replaying: true,
                next: None,
                terminal: None,
                finished: false,
                closed: false,
            }),
            wake,
        }
    }

    fn push(&self, event: SignalEvent<T, E>, output: &Sender<SignalEvent<T, E>>) {
        let terminal = is_terminal(&event);
        let mut wake = false;
        {
            let mut state = lock(&self.state);
            if state.closed || state.finished {
                return;
            }
            if state.replaying {
                state.replaying = false;
                match output.try_send(event) {
                    Ok(()) => {
                        if terminal {
                            state.finished = true;
                            state.next = None;
                            state.terminal = None;
                            wake = true;
                        }
                    }
                    Err(TrySendError::Full(event)) => {
                        wake = store_event(&mut state, event);
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        state.closed = true;
                        state.finished = true;
                        state.next = None;
                        state.terminal = None;
                        wake = true;
                    }
                }
            } else {
                wake = store_event(&mut state, event);
            }
        }
        if wake {
            self.wake();
        }
    }

    fn replace_candidate(&self, candidate: &mut Option<SignalEvent<T, E>>) {
        let mut state = lock(&self.state);
        if state.closed || state.finished {
            *candidate = None;
            return;
        }
        if matches!(candidate.as_ref(), Some(SignalEvent::Next(_))) {
            if let Some(value) = state.next.take() {
                *candidate = Some(SignalEvent::Next(value));
            }
        } else if candidate.is_none() {
            *candidate = next_event(&mut state);
        }
    }

    fn finish(&self) {
        {
            let mut state = lock(&self.state);
            state.finished = true;
            state.next = None;
            state.terminal = None;
        }
        self.wake();
    }

    fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        state.finished = true;
        state.next = None;
        state.terminal = None;
    }

    fn should_stop(&self) -> bool {
        let state = lock(&self.state);
        state.closed || state.finished
    }

    fn wake(&self) {
        let _ = self.wake.try_send(());
    }
}

struct OrdinaryDelivery<T, E> {
    active: Mutex<bool>,
    sender: Sender<SignalEvent<T, E>>,
}

impl<T, E> OrdinaryDelivery<T, E> {
    fn new(sender: Sender<SignalEvent<T, E>>) -> Self {
        Self {
            active: Mutex::new(true),
            sender,
        }
    }

    fn forward(&self, event: SignalEvent<T, E>) {
        let active = lock(&self.active);
        if *active {
            let _ = self.sender.send(event);
        }
    }

    fn close(&self) {
        *lock(&self.active) = false;
    }
}

fn store_event<T, E>(state: &mut PendingState<T, E>, event: SignalEvent<T, E>) -> bool {
    match event {
        SignalEvent::Next(value) if state.terminal.is_none() => {
            state.next = Some(value);
            true
        }
        SignalEvent::Error(error) if state.terminal.is_none() => {
            state.terminal = Some(SignalEvent::Error(error));
            true
        }
        SignalEvent::Complete if state.terminal.is_none() => {
            state.terminal = Some(SignalEvent::Complete);
            true
        }
        SignalEvent::Next(_) | SignalEvent::Error(_) | SignalEvent::Complete => false,
    }
}

fn next_event<T, E>(state: &mut PendingState<T, E>) -> Option<SignalEvent<T, E>> {
    state
        .next
        .take()
        .map(SignalEvent::Next)
        .or_else(|| state.terminal.take())
}

fn is_terminal<T, E>(event: &SignalEvent<T, E>) -> bool {
    matches!(event, SignalEvent::Error(_) | SignalEvent::Complete)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn run_state_worker<T, E>(
    delivery: Arc<StateDelivery<T, E>>,
    wake: Receiver<()>,
    shutdown: Receiver<()>,
    output: Sender<SignalEvent<T, E>>,
) where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    let mut candidate = None;
    loop {
        delivery.replace_candidate(&mut candidate);
        if candidate.is_none() {
            if delivery.should_stop() {
                return;
            }
            crossbeam_channel::select_biased! {
                recv(shutdown) -> _ => return,
                recv(wake) -> result => {
                    if result.is_err() {
                        return;
                    }
                },
            }
            continue;
        }

        let Some(event) = candidate.as_ref().cloned() else {
            continue;
        };
        let terminal = is_terminal(&event);
        match select_state_delivery(delivery.as_ref(), &wake, &shutdown, &output, event) {
            StateDeliveryAction::Stop => return,
            StateDeliveryAction::Wake => continue,
            StateDeliveryAction::Sent => {
                candidate = None;
                if terminal {
                    delivery.finish();
                    return;
                }
            }
        }
    }
}

enum StateDeliveryAction {
    Stop,
    Wake,
    Sent,
}

fn select_state_delivery<T, E>(
    delivery: &StateDelivery<T, E>,
    wake: &Receiver<()>,
    shutdown: &Receiver<()>,
    output: &Sender<SignalEvent<T, E>>,
    event: SignalEvent<T, E>,
) -> StateDeliveryAction
where
    T: Send + 'static,
    E: Send + 'static,
{
    crossbeam_channel::select_biased! {
        recv(shutdown) -> _ => StateDeliveryAction::Stop,
        send(output, event) -> result => {
            if result.is_err() {
                delivery.close();
                StateDeliveryAction::Stop
            } else {
                StateDeliveryAction::Sent
            }
        },
        recv(wake) -> result => {
            if result.is_err() {
                StateDeliveryAction::Stop
            } else {
                StateDeliveryAction::Wake
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::TryRecvError;

    #[test]
    fn signal_bridge_preserves_next_error_and_complete_order() {
        let (signal, emitter) = Signal::<i32, &'static str>::channel();
        let bridge = SignalBridge::new(&signal);
        let receiver = bridge.receiver();

        assert!(emitter.emit(1));
        assert!(emitter.emit(2));
        assert!(emitter.error("failed"));

        assert_eq!(receiver.recv(), Ok(SignalEvent::Next(1)));
        assert_eq!(receiver.recv(), Ok(SignalEvent::Next(2)));
        assert_eq!(receiver.recv(), Ok(SignalEvent::Error("failed")));

        let (signal, emitter) = Signal::<i32, &'static str>::channel();
        let bridge = SignalBridge::new(&signal);
        let receiver = bridge.receiver();

        assert!(emitter.emit(3));
        assert!(emitter.complete());
        assert_eq!(receiver.recv(), Ok(SignalEvent::Next(3)));
        assert_eq!(receiver.recv(), Ok(SignalEvent::Complete));
    }

    #[test]
    fn dropping_signal_bridge_stops_forwarding() {
        let (signal, emitter) = Signal::<i32, &'static str>::channel();
        let bridge = SignalBridge::new(&signal);
        let receiver = bridge.receiver();
        drop(bridge);

        assert!(emitter.emit(1));
        assert!(matches!(
            receiver.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn state_bridge_captures_initial_replay() {
        let state = StateSignal::<i32, &'static str>::new(7);
        let bridge = StateSignalBridge::new(&state).expect("state bridge worker must start");

        assert_eq!(bridge.receiver().recv(), Ok(SignalEvent::Next(7)));
    }

    #[test]
    fn state_bridge_conflates_unconsumed_updates_to_latest() {
        let state = StateSignal::<i32, &'static str>::new(0);
        let bridge = StateSignalBridge::new(&state).expect("state bridge worker must start");
        let receiver = bridge.receiver();

        for value in 1..=100 {
            assert!(state.set(value));
        }

        assert_eq!(receiver.recv(), Ok(SignalEvent::Next(0)));
        assert_eq!(receiver.recv(), Ok(SignalEvent::Next(100)));
    }

    #[test]
    fn state_bridge_retains_terminal_after_latest_value() {
        let state = StateSignal::<i32, &'static str>::new(0);
        let bridge = StateSignalBridge::new(&state).expect("state bridge worker must start");
        let receiver = bridge.receiver();

        for value in 1..=100 {
            assert!(state.set(value));
        }
        assert!(state.complete());

        assert_eq!(receiver.recv(), Ok(SignalEvent::Next(0)));
        assert_eq!(receiver.recv(), Ok(SignalEvent::Next(100)));
        assert_eq!(receiver.recv(), Ok(SignalEvent::Complete));
    }

    #[test]
    fn state_bridge_retains_error_after_latest_value() {
        let state = StateSignal::<i32, &'static str>::new(0);
        let bridge = StateSignalBridge::new(&state).expect("state bridge worker must start");
        let receiver = bridge.receiver();

        for value in 1..=100 {
            assert!(state.set(value));
        }
        assert!(state.error("failed"));

        assert_eq!(receiver.recv(), Ok(SignalEvent::Next(0)));
        assert_eq!(receiver.recv(), Ok(SignalEvent::Next(100)));
        assert_eq!(receiver.recv(), Ok(SignalEvent::Error("failed")));
    }

    #[test]
    fn receiver_clones_share_signal_bridge_delivery() {
        let (signal, emitter) = Signal::<i32, &'static str>::channel();
        let bridge = SignalBridge::new(&signal);
        let first = bridge.receiver();
        let second = bridge.events();

        assert!(emitter.emit(3));
        assert_eq!(first.recv(), Ok(SignalEvent::Next(3)));
        assert!(emitter.emit(4));
        assert_eq!(second.recv(), Ok(SignalEvent::Next(4)));
    }

    #[test]
    fn state_worker_prefers_latest_delivery_over_ready_wake() {
        let (wake_sender, wake) = bounded(1);
        let (_shutdown_sender, shutdown) = bounded(1);
        let (output_sender, output) = bounded(1);
        let delivery = StateDelivery::<i32, &'static str>::new(wake_sender);

        assert!(output_sender.send(SignalEvent::Next(-1)).is_ok());
        delivery.push(SignalEvent::Next(1), &output_sender);
        delivery.push(SignalEvent::Next(100), &output_sender);
        assert_eq!(output.recv(), Ok(SignalEvent::Next(-1)));

        let mut candidate = None;
        delivery.replace_candidate(&mut candidate);
        assert_eq!(candidate, Some(SignalEvent::Next(100)));
        let Some(event) = candidate else {
            return;
        };

        let action = select_state_delivery(&delivery, &wake, &shutdown, &output_sender, event);

        assert!(matches!(action, StateDeliveryAction::Sent));
        assert_eq!(output.recv(), Ok(SignalEvent::Next(100)));
    }
}
