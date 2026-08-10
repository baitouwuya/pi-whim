use crate::{Observer, Signal};
use parking_lot::Mutex;
use std::sync::Arc;

impl<T, E> Signal<T, E> {
    /// Maps every value while preserving upstream error and completion semantics.
    pub fn map<U, F>(&self, mapper: F) -> Signal<U, E>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(T) -> U + Send + Sync + 'static,
    {
        let (output, _) = Signal::channel();
        let weak = output.downgrade();
        let mapper = Arc::new(mapper);
        let subscription = self.subscribe(Observer::with_callbacks(
            {
                let mapper = mapper.clone();
                let weak = weak.clone();
                move |value| {
                    weak.emit(mapper(value));
                }
            },
            {
                let weak = weak.clone();
                move |error| {
                    weak.error(error);
                }
            },
            move || {
                weak.complete();
            },
        ));
        output.keep_subscription(subscription);
        output
    }

    /// Emits only values accepted by `predicate`.
    pub fn filter<F>(&self, predicate: F) -> Signal<T, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(&T) -> bool + Send + Sync + 'static,
    {
        let (output, _) = Signal::channel();
        let weak = output.downgrade();
        let predicate = Arc::new(predicate);
        let subscription = self.subscribe(Observer::with_callbacks(
            {
                let predicate = predicate.clone();
                let weak = weak.clone();
                move |value| {
                    if predicate(&value) {
                        weak.emit(value);
                    }
                }
            },
            {
                let weak = weak.clone();
                move |error| {
                    weak.error(error);
                }
            },
            move || {
                weak.complete();
            },
        ));
        output.keep_subscription(subscription);
        output
    }

    /// Maps values to optional values, dropping `None` results.
    pub fn filter_map<U, F>(&self, mapper: F) -> Signal<U, E>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(T) -> Option<U> + Send + Sync + 'static,
    {
        let (output, _) = Signal::channel();
        let weak = output.downgrade();
        let mapper = Arc::new(mapper);
        let subscription = self.subscribe(Observer::with_callbacks(
            {
                let mapper = mapper.clone();
                let weak = weak.clone();
                move |value| {
                    if let Some(mapped) = mapper(value) {
                        weak.emit(mapped);
                    }
                }
            },
            {
                let weak = weak.clone();
                move |error| {
                    weak.error(error);
                }
            },
            move || {
                weak.complete();
            },
        ));
        output.keep_subscription(subscription);
        output
    }

    /// Accumulates values and emits each new accumulator value.
    pub fn scan<A, F>(&self, initial: A, accumulator: F) -> Signal<A, E>
    where
        T: Clone + Send + Sync + 'static,
        A: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(&mut A, T) + Send + Sync + 'static,
    {
        let (output, _) = Signal::channel();
        let weak = output.downgrade();
        let state = Arc::new(Mutex::new(initial));
        let accumulator = Arc::new(accumulator);
        let subscription = self.subscribe(Observer::with_callbacks(
            {
                let state = state.clone();
                let accumulator = accumulator.clone();
                let weak = weak.clone();
                move |value| {
                    let next = {
                        let mut state = state.lock();
                        accumulator(&mut state, value);
                        state.clone()
                    };
                    weak.emit(next);
                }
            },
            {
                let weak = weak.clone();
                move |error| {
                    weak.error(error);
                }
            },
            move || {
                weak.complete();
            },
        ));
        output.keep_subscription(subscription);
        output
    }
}
