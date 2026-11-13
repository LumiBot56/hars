//! Core runtime for homeassistant-rust.
//!
//! Mirrors the three pillars of Home Assistant's `core.py`:
//! - [`EventBus`]: typed pub/sub for everything that happens in the system.
//! - [`StateMachine`]: the current [`State`] of every entity.
//! - [`Hass`]: the handle that ties them together and is shared by integrations.
//!
//! Design rule: the hot path (state writes + event fan-out) must not allocate
//! more than necessary and must never block on I/O.

mod entity_id;
mod event;
mod state;

pub use entity_id::{EntityId, EntityIdError};
pub use event::{Event, EventBus, EventKind};
pub use state::{Attributes, State, StateMachine};

use std::sync::Arc;

/// Shared handle to the running instance. Cheap to clone.
#[derive(Clone, Default)]
pub struct Hass {
    inner: Arc<HassInner>,
}

#[derive(Default)]
struct HassInner {
    bus: EventBus,
    states: StateMachine,
}

impl Hass {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bus(&self) -> &EventBus {
        &self.inner.bus
    }

    pub fn states(&self) -> &StateMachine {
        &self.inner.states
    }

    /// Set an entity's state and fire `state_changed` if it actually changed.
    pub fn set_state(
        &self,
        entity_id: EntityId,
        state: impl Into<smol_str::SmolStr>,
        attributes: Attributes,
    ) -> Arc<State> {
        let (new, old) = self
            .inner
            .states
            .set(entity_id.clone(), state.into(), attributes);
        let changed = old.as_ref().is_none_or(|o| !Arc::ptr_eq(o, &new));
        if changed {
            self.inner
                .bus
                .fire(Event::state_changed(entity_id, old, Some(new.clone())));
        }
        new
    }
}
