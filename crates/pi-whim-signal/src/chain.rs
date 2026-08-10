use crate::Subscription;
use parking_lot::Mutex;
use std::convert::Infallible;
use std::sync::{Arc, Weak};

type Gate<T, E, D> = Arc<dyn Fn(&T) -> Result<GateDecision<D>, E> + Send + Sync>;
type Transform<T, E> = Arc<dyn Fn(T) -> Result<T, E> + Send + Sync>;

/// The typed result of a gate evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateDecision<D> {
    /// The value passed this gate.
    Allow,
    /// The value was denied and carries a typed reason.
    Deny(D),
}

/// Determines what a [`GateChain`] does when a gate returns an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateFailurePolicy<D = ()> {
    /// Stop evaluation and return the gate error.
    Propagate,
    /// Ignore the gate error and continue evaluating later gates.
    AllowOnError,
    /// Deny the value with this typed reason when a gate errors.
    DenyOnError(D),
}

struct GateRegistry<T, E, D> {
    next_id: u64,
    gates: Vec<(u64, Gate<T, E, D>)>,
}

/// A cloneable, thread-safe dynamic registry of typed gates.
///
/// Registrations are evaluated in stable registration order.  The returned
/// [`Subscription`] removes the gate when dropped.  Evaluations snapshot the
/// registry before invoking user code, so registration, disconnection, and gate
/// callbacks never contend on the registry lock.
pub struct GateChain<T, E = Infallible, D = ()> {
    registry: Arc<Mutex<GateRegistry<T, E, D>>>,
    failure_policy: GateFailurePolicy<D>,
}

impl<T, E, D> Clone for GateChain<T, E, D>
where
    D: Clone,
{
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            failure_policy: self.failure_policy.clone(),
        }
    }
}

impl<T, E, D> GateChain<T, E, D> {
    /// Creates an empty chain with an explicit failure policy.
    pub fn new(failure_policy: GateFailurePolicy<D>) -> Self {
        Self {
            registry: Arc::new(Mutex::new(GateRegistry {
                next_id: 0,
                gates: Vec::new(),
            })),
            failure_policy,
        }
    }

    /// Creates an empty chain which propagates gate failures.
    pub fn propagating() -> Self {
        Self::new(GateFailurePolicy::Propagate)
    }

    /// Creates an empty chain which allows a value when a gate errors.
    pub fn allow_on_error() -> Self {
        Self::new(GateFailurePolicy::AllowOnError)
    }

    /// Creates an empty chain which preserves the current decision on error.
    ///
    /// This is an alias for [`GateChain::allow_on_error`].
    pub fn preserving() -> Self {
        Self::allow_on_error()
    }

    /// Creates an empty chain which denies with a fixed typed reason on error.
    pub fn deny_on_error(reason: D) -> Self {
        Self::new(GateFailurePolicy::DenyOnError(reason))
    }

    /// Appends a gate and returns a subscription which disconnects it on Drop.
    pub fn register<F>(&self, gate: F) -> Subscription
    where
        T: Send + Sync + 'static,
        E: Send + Sync + 'static,
        D: Send + Sync + 'static,
        F: Fn(&T) -> Result<GateDecision<D>, E> + Send + Sync + 'static,
    {
        let gate: Gate<T, E, D> = Arc::new(gate);
        let (id, registry) = {
            let mut registry = self.registry.lock();
            let id = registry.next_id;
            registry.next_id = registry.next_id.wrapping_add(1);
            registry.gates.push((id, gate));
            (id, Arc::downgrade(&self.registry))
        };
        Subscription::new(move || disconnect_gate(&registry, id))
    }

    /// Returns the number of currently connected gates.
    pub fn len(&self) -> usize {
        self.registry.lock().gates.len()
    }

    /// Returns whether no gates are currently connected.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Evaluates a snapshot of gates in registration order.
    pub fn evaluate(&self, value: &T) -> Result<GateDecision<D>, E>
    where
        D: Clone,
    {
        let gates = self
            .registry
            .lock()
            .gates
            .iter()
            .map(|(_, gate)| gate.clone())
            .collect::<Vec<_>>();
        for gate in gates {
            match gate(value) {
                Ok(GateDecision::Allow) => {}
                Ok(GateDecision::Deny(reason)) => return Ok(GateDecision::Deny(reason)),
                Err(error) => match &self.failure_policy {
                    GateFailurePolicy::Propagate => return Err(error),
                    GateFailurePolicy::AllowOnError => {}
                    GateFailurePolicy::DenyOnError(reason) => {
                        return Ok(GateDecision::Deny(reason.clone()));
                    }
                },
            }
        }
        Ok(GateDecision::Allow)
    }

    /// Evaluates the chain and returns the value only when it is allowed.
    ///
    /// Use [`GateChain::evaluate`] when the typed denial reason is needed.
    pub fn allow<'a>(&self, value: &'a T) -> Result<Option<&'a T>, E>
    where
        D: Clone,
    {
        match self.evaluate(value)? {
            GateDecision::Allow => Ok(Some(value)),
            GateDecision::Deny(_) => Ok(None),
        }
    }
}

impl<T, E, D> Default for GateChain<T, E, D> {
    fn default() -> Self {
        Self::propagating()
    }
}

fn disconnect_gate<T, E, D>(registry: &Weak<Mutex<GateRegistry<T, E, D>>>, id: u64) {
    if let Some(registry) = registry.upgrade() {
        registry
            .lock()
            .gates
            .retain(|(registered_id, _)| *registered_id != id);
    }
}

/// Determines what a [`TransformChain`] does when a transform fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransformFailurePolicy {
    /// Stop evaluation and return the transform error.
    Propagate,
    /// Preserve the previous value and continue with later transforms.
    Preserve,
}

struct TransformRegistry<T, E> {
    next_id: u64,
    transforms: Vec<(u64, Transform<T, E>)>,
}

/// A cloneable, thread-safe dynamic registry of typed transformations.
///
/// Registrations are evaluated in stable registration order.  The returned
/// [`Subscription`] removes the transform when dropped, and evaluations invoke
/// a snapshot without holding the registry lock.
pub struct TransformChain<T, E = Infallible> {
    registry: Arc<Mutex<TransformRegistry<T, E>>>,
    failure_policy: TransformFailurePolicy,
}

impl<T, E> Clone for TransformChain<T, E> {
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            failure_policy: self.failure_policy,
        }
    }
}

impl<T, E> TransformChain<T, E> {
    /// Creates an empty chain with an explicit failure policy.
    pub fn new(failure_policy: TransformFailurePolicy) -> Self {
        Self {
            registry: Arc::new(Mutex::new(TransformRegistry {
                next_id: 0,
                transforms: Vec::new(),
            })),
            failure_policy,
        }
    }

    /// Creates an empty chain which propagates transform failures.
    pub fn propagating() -> Self {
        Self::new(TransformFailurePolicy::Propagate)
    }

    /// Creates an empty chain which preserves the previous value on failure.
    pub fn preserving() -> Self {
        Self::new(TransformFailurePolicy::Preserve)
    }

    /// Appends a transform and returns a subscription which disconnects it on Drop.
    pub fn register<F>(&self, transform: F) -> Subscription
    where
        T: Send + Sync + 'static,
        E: Send + Sync + 'static,
        F: Fn(T) -> Result<T, E> + Send + Sync + 'static,
    {
        let transform: Transform<T, E> = Arc::new(transform);
        let (id, registry) = {
            let mut registry = self.registry.lock();
            let id = registry.next_id;
            registry.next_id = registry.next_id.wrapping_add(1);
            registry.transforms.push((id, transform));
            (id, Arc::downgrade(&self.registry))
        };
        Subscription::new(move || disconnect_transform(&registry, id))
    }

    /// Returns the number of currently connected transforms.
    pub fn len(&self) -> usize {
        self.registry.lock().transforms.len()
    }

    /// Returns whether no transforms are currently connected.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Applies a snapshot of transforms in registration order.
    ///
    /// `T: Clone` is required because the preserve policy retains the previous
    /// value when a transform returns an error.
    pub fn apply(&self, mut value: T) -> Result<T, E>
    where
        T: Clone,
    {
        let transforms = self
            .registry
            .lock()
            .transforms
            .iter()
            .map(|(_, transform)| transform.clone())
            .collect::<Vec<_>>();
        for transform in transforms {
            let previous = value.clone();
            match transform(value) {
                Ok(next) => value = next,
                Err(error) if self.failure_policy == TransformFailurePolicy::Propagate => {
                    return Err(error);
                }
                Err(_) => value = previous,
            }
        }
        Ok(value)
    }
}

impl<T, E> Default for TransformChain<T, E> {
    fn default() -> Self {
        Self::propagating()
    }
}

fn disconnect_transform<T, E>(registry: &Weak<Mutex<TransformRegistry<T, E>>>, id: u64) {
    if let Some(registry) = registry.upgrade() {
        registry
            .lock()
            .transforms
            .retain(|(registered_id, _)| *registered_id != id);
    }
}
