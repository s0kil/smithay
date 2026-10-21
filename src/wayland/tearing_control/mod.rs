//! Implementation of wp_tearing_control protocol
//!
//! ### Example
//!
//! ```no_run
//! # extern crate wayland_server;
//! #
//! use wayland_server::{protocol::wl_surface::WlSurface, DisplayHandle};
//! use smithay::{
//!     wayland::compositor::{self, CompositorState, CompositorClientState, CompositorHandler},
//!     wayland::tearing_control::{TearingControlSurfaceCachedState, TearingControlState},
//! };
//!
//! pub struct State {
//!     compositor_state: CompositorState,
//! };
//! struct ClientState { compositor_state: CompositorClientState }
//! impl wayland_server::backend::ClientData for ClientState {}
//!
//! smithay::delegate_dispatch2!(State);
//!
//! impl CompositorHandler for State {
//!    fn compositor_state(&mut self) -> &mut CompositorState {
//!        &mut self.compositor_state
//!    }
//!
//!    fn client_compositor_state<'a>(&self, client: &'a wayland_server::Client) -> &'a CompositorClientState {
//!        &client.get_data::<ClientState>().unwrap().compositor_state
//!    }
//!
//!    fn commit(&mut self, surface: &WlSurface) {
//!        compositor::with_states(&surface, |states| {
//!            let mut guard = states.cached_state.get::<TearingControlSurfaceCachedState>();
//!            let current = guard.current();
//!            dbg!(current.presentation_hint());
//!        });
//!    }
//! }
//!
//! let mut display = wayland_server::Display::<State>::new().unwrap();
//!
//! let compositor_state = CompositorState::new::<State>(&display.handle());
//! TearingControlState::new::<State>(&display.handle());
//!
//! let state = State {
//!     compositor_state,
//! };
//! ```

use std::sync::{
    Mutex,
    atomic::{self, AtomicBool},
};

use wayland_protocols::wp::tearing_control::v1::server::{
    wp_tearing_control_manager_v1::WpTearingControlManagerV1, wp_tearing_control_v1,
};
use wayland_server::{
    DisplayHandle, GlobalDispatch, Resource, Weak, backend::GlobalId, protocol::wl_surface::WlSurface,
};

use super::compositor::Cacheable;

use crate::wayland::GlobalData;

mod dispatch;
#[cfg(test)]
mod tests;

/// Data associated with WlSurface
/// The presentation hint is double buffered with the surface commit.
///
/// ```no_run
/// use smithay::wayland::compositor;
/// use smithay::wayland::tearing_control::TearingControlSurfaceCachedState;
///
/// # let wl_surface = todo!();
/// compositor::with_states(&wl_surface, |states| {
///     let mut guard = states.cached_state.get::<TearingControlSurfaceCachedState>();
///     let current = guard.current();
///     dbg!(current.presentation_hint());
/// });
/// ```
#[derive(Debug, Clone, Copy)]
pub struct TearingControlSurfaceCachedState {
    presentation_hint: wp_tearing_control_v1::PresentationHint,
}

impl TearingControlSurfaceCachedState {
    /// This informs the compositor that the client allows the surface to be presented asynchronously.
    pub fn presentation_hint(&self) -> &wp_tearing_control_v1::PresentationHint {
        &self.presentation_hint
    }
}

impl Default for TearingControlSurfaceCachedState {
    fn default() -> Self {
        Self {
            presentation_hint: wp_tearing_control_v1::PresentationHint::Vsync,
        }
    }
}

impl Cacheable for TearingControlSurfaceCachedState {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        *self
    }

    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

#[derive(Debug)]
struct TearingControlSurfaceData {
    is_resource_attached: AtomicBool,
}

impl TearingControlSurfaceData {
    fn new() -> Self {
        Self {
            is_resource_attached: AtomicBool::new(false),
        }
    }

    fn set_is_resource_attached(&self, is_attached: bool) {
        self.is_resource_attached
            .store(is_attached, atomic::Ordering::Release)
    }

    fn is_resource_attached(&self) -> bool {
        self.is_resource_attached.load(atomic::Ordering::Acquire)
    }
}

/// User data of `WpTearingControlV1` object
#[derive(Debug)]
pub struct TearingControlUserData(Mutex<Weak<WlSurface>>);

impl TearingControlUserData {
    fn new(surface: WlSurface) -> Self {
        Self(Mutex::new(surface.downgrade()))
    }

    #[inline]
    fn wl_surface(&self) -> Option<WlSurface> {
        self.0.lock().unwrap().upgrade().ok()
    }
}

/// Delegate type for [WpTearingControlManagerV1] global.
#[derive(Debug)]
pub struct TearingControlState {
    global: GlobalId,
}

impl TearingControlState {
    /// Register new [WpTearingControlManagerV1] global
    pub fn new<D>(display: &DisplayHandle) -> TearingControlState
    where
        D: GlobalDispatch<WpTearingControlManagerV1, GlobalData> + 'static,
    {
        let global = display.create_global::<D, WpTearingControlManagerV1, _>(1, GlobalData);

        TearingControlState { global }
    }

    /// Returns the WpTearingControlManagerV1 global id
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }
}
