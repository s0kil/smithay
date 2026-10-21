use std::{
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_compositor, wl_registry, wl_subcompositor, wl_subsurface, wl_surface},
};
use wayland_protocols::wp::tearing_control::v1::client::{
    wp_tearing_control_manager_v1, wp_tearing_control_v1,
};
use wayland_server::{Display, protocol::wl_surface::WlSurface};

use super::{TearingControlState, TearingControlSurfaceCachedState};
use crate::wayland::compositor::{self, CompositorClientState, CompositorHandler, CompositorState};

type Hint = wayland_protocols::wp::tearing_control::v1::server::wp_tearing_control_v1::PresentationHint;

struct Server {
    compositor: CompositorState,
    surfaces: Arc<Mutex<Vec<WlSurface>>>,
}
struct ClientData {
    compositor: CompositorClientState,
}
impl wayland_server::backend::ClientData for ClientData {}
crate::delegate_dispatch2!(Server);
impl CompositorHandler for Server {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }
    fn client_compositor_state<'a>(&self, client: &'a wayland_server::Client) -> &'a CompositorClientState {
        &client.get_data::<ClientData>().unwrap().compositor
    }
    fn new_surface(&mut self, surface: &WlSurface) {
        self.surfaces.lock().unwrap().push(surface.clone());
    }
    fn commit(&mut self, _: &WlSurface) {}
}

struct Client;
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_surface::WlSurface);
delegate_noop!(Client: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(Client: ignore wl_subsurface::WlSubsurface);
delegate_noop!(Client: ignore wp_tearing_control_manager_v1::WpTearingControlManagerV1);
delegate_noop!(Client: ignore wp_tearing_control_v1::WpTearingControlV1);

struct Harness {
    connection: Connection,
    queue: wayland_client::EventQueue<Client>,
    compositor: wl_compositor::WlCompositor,
    subcompositor: wl_subcompositor::WlSubcompositor,
    manager: wp_tearing_control_manager_v1::WpTearingControlManagerV1,
    surfaces: Arc<Mutex<Vec<WlSurface>>>,
    stop: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}
impl Harness {
    fn new() -> Self {
        let (server_socket, client_socket) = UnixStream::pair().unwrap();
        let surfaces = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let server_surfaces = surfaces.clone();
        let server_stop = stop.clone();
        let server = thread::spawn(move || {
            let mut display = Display::<Server>::new().unwrap();
            let mut handle = display.handle();
            let compositor = CompositorState::new::<Server>(&handle);
            TearingControlState::new::<Server>(&handle);
            handle
                .insert_client(
                    server_socket,
                    Arc::new(ClientData {
                        compositor: CompositorClientState::default(),
                    }),
                )
                .unwrap();
            let mut state = Server {
                compositor,
                surfaces: server_surfaces,
            };
            let deadline = Instant::now() + Duration::from_secs(15);
            while !server_stop.load(Ordering::Acquire) && Instant::now() < deadline {
                display.dispatch_clients(&mut state).unwrap();
                display.flush_clients().unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });
        let connection = Connection::from_socket(client_socket).unwrap();
        let (globals, queue) = registry_queue_init::<Client>(&connection).unwrap();
        let qh = queue.handle();
        let compositor = globals.bind(&qh, 1..=6, ()).unwrap();
        let subcompositor = globals.bind(&qh, 1..=1, ()).unwrap();
        let manager = globals.bind(&qh, 1..=1, ()).unwrap();
        Self {
            connection,
            queue,
            compositor,
            subcompositor,
            manager,
            surfaces,
            stop,
            server: Some(server),
        }
    }
    fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut Client).unwrap();
    }
    fn hint(&self, index: usize) -> Hint {
        let surface = self.surfaces.lock().unwrap()[index].clone();
        compositor::with_states(&surface, |state| {
            *state
                .cached_state
                .get::<TearingControlSurfaceCachedState>()
                .current()
                .presentation_hint()
        })
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let result = self.server.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

#[test]
fn hint_and_destroy_are_double_buffered_and_resource_can_be_recreated() {
    let mut h = Harness::new();
    let qh = h.queue.handle();
    let surface = h.compositor.create_surface(&qh, ());
    let control = h.manager.get_tearing_control(&surface, &qh, ());
    control.set_presentation_hint(wp_tearing_control_v1::PresentationHint::Async);
    h.roundtrip();
    assert_eq!(h.hint(0), Hint::Vsync);
    surface.commit();
    h.roundtrip();
    assert_eq!(h.hint(0), Hint::Async);
    control.destroy();
    h.roundtrip();
    assert_eq!(h.hint(0), Hint::Async);
    surface.commit();
    h.roundtrip();
    assert_eq!(h.hint(0), Hint::Vsync);
    let new_control = h.manager.get_tearing_control(&surface, &qh, ());
    new_control.set_presentation_hint(wp_tearing_control_v1::PresentationHint::Async);
    surface.commit();
    h.roundtrip();
    assert_eq!(h.hint(0), Hint::Async);
}

#[test]
fn duplicate_control_is_a_protocol_error() {
    let mut h = Harness::new();
    let qh = h.queue.handle();
    let surface = h.compositor.create_surface(&qh, ());
    let _one = h.manager.get_tearing_control(&surface, &qh, ());
    let _two = h.manager.get_tearing_control(&surface, &qh, ());
    assert!(h.queue.roundtrip(&mut Client).is_err());
    let error = h.connection.protocol_error().unwrap();
    assert_eq!(
        error.code,
        wp_tearing_control_manager_v1::Error::TearingControlExists as u32
    );
}

#[test]
fn requests_after_surface_destruction_are_inert() {
    let mut h = Harness::new();
    let qh = h.queue.handle();
    let surface = h.compositor.create_surface(&qh, ());
    let control = h.manager.get_tearing_control(&surface, &qh, ());
    surface.destroy();
    h.roundtrip();
    control.set_presentation_hint(wp_tearing_control_v1::PresentationHint::Async);
    control.destroy();
    h.roundtrip();
}

#[test]
fn synchronized_subsurface_hint_waits_for_parent_commit() {
    let mut h = Harness::new();
    let qh = h.queue.handle();
    let parent = h.compositor.create_surface(&qh, ());
    let child = h.compositor.create_surface(&qh, ());
    let _subsurface = h.subcompositor.get_subsurface(&child, &parent, &qh, ());
    let control = h.manager.get_tearing_control(&child, &qh, ());
    control.set_presentation_hint(wp_tearing_control_v1::PresentationHint::Async);
    child.commit();
    h.roundtrip();
    assert_eq!(h.hint(1), Hint::Vsync);
    parent.commit();
    h.roundtrip();
    assert_eq!(h.hint(1), Hint::Async);
}

#[test]
fn destroying_manager_does_not_reset_existing_surface_hint() {
    let mut h = Harness::new();
    let qh = h.queue.handle();
    let surface = h.compositor.create_surface(&qh, ());
    let control = h.manager.get_tearing_control(&surface, &qh, ());
    h.manager.destroy();
    control.set_presentation_hint(wp_tearing_control_v1::PresentationHint::Async);
    surface.commit();
    h.roundtrip();
    assert_eq!(h.hint(0), Hint::Async);
}
