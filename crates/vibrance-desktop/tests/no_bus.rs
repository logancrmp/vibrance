//! Without a reachable session bus, `Mpris::start` returns an error instead of panicking or
//! hanging. Its own test binary, because it changes the process environment.

use std::sync::Arc;
use std::time::{Duration, Instant};

use vibrance_audio::{Engine, Sink};
use vibrance_desktop::Mpris;

#[test]
fn start_fails_cleanly_without_a_session_bus() {
    // SAFETY: the only test in this binary, and no other thread reads the environment yet.
    unsafe {
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/vibrance-test-bus");
    }
    let engine = Arc::new(Engine::new(Sink::Null).unwrap());
    let t0 = Instant::now();
    let result = Mpris::start(engine);
    let err = result.err().expect("start must fail without a bus");
    println!("error: {err}");
    assert!(t0.elapsed() < Duration::from_secs(6));
}
