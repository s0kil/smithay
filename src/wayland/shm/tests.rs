use std::{
    os::{fd::AsFd, unix::net::UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_shm, wl_shm_pool},
};
use wayland_server::{Display, protocol::wl_buffer::WlBuffer};

use super::{ShmHandler, ShmState};
use crate::wayland::buffer::BufferHandler;

struct Server(ShmState);
crate::delegate_dispatch2!(Server);
impl ShmHandler for Server {
    fn shm_state(&self) -> &ShmState {
        &self.0
    }
}
impl BufferHandler for Server {
    fn buffer_destroyed(&mut self, _: &WlBuffer) {}
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
delegate_noop!(Client: ignore wl_shm::WlShm);
delegate_noop!(Client: ignore wl_shm_pool::WlShmPool);

fn invalid_resize_is_a_protocol_error(size: i32) {
    let (server_socket, client_socket) = UnixStream::pair().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let server = thread::spawn(move || {
        let mut display = Display::<Server>::new().unwrap();
        let mut handle = display.handle();
        let mut state = Server(ShmState::new::<Server>(&handle, []));
        handle.insert_client(server_socket, Arc::new(())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !server_stop.load(Ordering::Acquire) && Instant::now() < deadline {
            display.dispatch_clients(&mut state).unwrap();
            display.flush_clients().unwrap();
            thread::sleep(Duration::from_millis(1));
        }
    });

    let connection = Connection::from_socket(client_socket).unwrap();
    let (globals, mut queue) = registry_queue_init::<Client>(&connection).unwrap();
    let qh = queue.handle();
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=2, ()).unwrap();
    let file = tempfile::tempfile().unwrap();
    file.set_len(4096).unwrap();
    let pool = shm.create_pool(file.as_fd(), 4096, &qh, ());
    queue.roundtrip(&mut Client).unwrap();
    pool.resize(size);
    let result = queue.roundtrip(&mut Client);
    stop.store(true, Ordering::Release);

    // Invalid client input must disconnect that client, not panic the server.
    server.join().unwrap();
    assert!(result.is_err());
    let error = connection.protocol_error().expect("expected a protocol error");
    assert_eq!(error.code, wl_shm::Error::InvalidFd as u32);
}

#[test]
fn zero_size_resize_does_not_panic_server() {
    invalid_resize_is_a_protocol_error(0);
}

#[test]
fn negative_size_resize_does_not_reinterpret_size_as_unsigned() {
    invalid_resize_is_a_protocol_error(-1);
}
