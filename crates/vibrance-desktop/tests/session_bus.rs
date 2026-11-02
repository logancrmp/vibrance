//! MPRIS on the real session bus, driven with `playerctl` and `busctl`.
//!
//! Each test registers its own `org.mpris.MediaPlayer2.vibrance.test<pid>_<tag>` and only ever
//! addresses that exact name, so other players on the desktop are never touched. The engine
//! uses `Sink::Null`; nothing opens an audio device.

use std::collections::HashMap;
use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command as Process;
use std::sync::{Arc, Once, mpsc};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use mpris_server::zbus;
use mpris_server::zbus::export::futures_core::Stream;
use vibrance_audio::{Engine, PlayState, Sink, Track};
use vibrance_desktop::{Command, LoopStatus, Mpris, PlayerState, Status, TrackInfo};

const TIMEOUT: Duration = Duration::from_secs(5);
const TRACK_SECS: u64 = 30;

// ---------- fixtures ----------

fn fixture() -> PathBuf {
    static ONCE: Once = Once::new();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("vibrance-desktop");
    // Spaces, '#', '%' and unicode, to check `xesam:url` encoding end to end.
    let path = dir.join("Tône #1 (100%).wav");
    ONCE.call_once(|| {
        std::fs::create_dir_all(&dir).unwrap();
        let src = format!("sine=frequency=440:sample_rate=48000:duration={TRACK_SECS}");
        let status = Process::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error", "-nostdin", "-f", "lavfi", "-i", &src])
            .args(["-ac", "2", "-c:a", "pcm_s16le"])
            .arg(&path)
            .status()
            .expect("ffmpeg is required for this test");
        assert!(status.success());
    });
    path
}

fn info(path: &Path) -> TrackInfo {
    TrackInfo {
        id: 7,
        title: "Sine Wave".into(),
        artist: "Test Artist".into(),
        album: "Test Album".into(),
        album_artist: "Various".into(),
        track_no: Some(3),
        duration: Some(Duration::from_secs(TRACK_SECS)),
        path: path.to_path_buf(),
        art: Some(PathBuf::from("/tmp/cover art.png")),
    }
}

struct Session {
    engine: Arc<Engine>,
    mpris: Mpris,
    commands: async_channel::Receiver<Command>,
    /// `playerctl -p` name: the bus name without `org.mpris.MediaPlayer2.`.
    player: String,
}

fn session(tag: &str) -> Session {
    let engine = Arc::new(Engine::new(Sink::Null).unwrap());
    let player = format!("vibrance.test{}_{tag}", std::process::id());
    let (mpris, commands) = Mpris::start_with_suffix(Arc::clone(&engine), &player).unwrap();
    assert_eq!(mpris.bus_name(), format!("org.mpris.MediaPlayer2.{player}"));
    Session { engine, mpris, commands, player }
}

impl Session {
    fn playerctl(&self, args: &[&str]) -> String {
        let out = Process::new("playerctl").arg("-p").arg(&self.player).args(args).output().unwrap();
        assert!(out.status.success(), "playerctl {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn busctl(&self, args: &[&str]) -> String {
        let out = Process::new("busctl")
            .arg("--user")
            .args(&args[..1])
            .arg(self.mpris.bus_name())
            .arg("/org/mpris/MediaPlayer2")
            .args(&args[1..])
            .output()
            .unwrap();
        assert!(out.status.success(), "busctl {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// Exactly these commands arrive (and nothing else, within a short grace period).
    fn expect(&self, want: &[Command]) {
        let mut got = Vec::new();
        let deadline = Instant::now() + TIMEOUT;
        while got.len() < want.len() && Instant::now() < deadline {
            match self.commands.try_recv() {
                Ok(c) => got.push(c),
                Err(_) => sleep(Duration::from_millis(5)),
            }
        }
        sleep(Duration::from_millis(50));
        got.extend(std::iter::from_fn(|| self.commands.try_recv().ok()));
        assert_eq!(got, want);
    }

    fn wait_state(&self, state: PlayState) {
        let deadline = Instant::now() + TIMEOUT;
        while self.engine.status().state != state {
            assert!(Instant::now() < deadline, "engine never reached {state:?}");
            sleep(Duration::from_millis(5));
        }
    }

    /// What the UI would do: publish a state and wait until D-Bus clients see it.
    fn publish(&self, state: PlayerState) {
        let want = match state.status {
            Status::Playing => "Playing",
            Status::Paused => "Paused",
            Status::Stopped => "Stopped",
        };
        self.mpris.update(state);
        let deadline = Instant::now() + TIMEOUT;
        while self.playerctl(&["status"]) != want {
            assert!(Instant::now() < deadline, "status never became {want}");
            sleep(Duration::from_millis(10));
        }
    }
}

fn secs(s: &str) -> f64 {
    s.parse().unwrap_or_else(|_| panic!("not a number: {s:?}"))
}

// ---------- signal watcher ----------

#[derive(Debug, PartialEq)]
enum Sig {
    Seeked(i64),
    /// Interface and the sorted names of the changed properties.
    Changed(String, Vec<String>),
}

/// Records `Seeked` and `PropertiesChanged` from the server's unique name.
fn watch(bus_name: &str) -> mpsc::Receiver<Sig> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let name = bus_name.to_owned();
    thread::spawn(move || {
        zbus::block_on(async move {
            let conn = zbus::Connection::session().await.unwrap();
            let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
            let owner = dbus.get_name_owner(name.as_str().try_into().unwrap()).await.unwrap();
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(owner.as_str())
                .path("/org/mpris/MediaPlayer2")
                .build()
                .unwrap();
            let mut stream = zbus::MessageStream::for_match_rule(rule, &conn, None).await.unwrap();
            ready_tx.send(()).unwrap();
            while let Some(Ok(msg)) = poll_fn(|cx| Pin::new(&mut stream).poll_next(cx)).await {
                let header = msg.header();
                let body = msg.body();
                let sig = match header.member().map(|m| m.as_str()) {
                    Some("Seeked") => Sig::Seeked(body.deserialize::<(i64,)>().unwrap().0),
                    Some("PropertiesChanged") => {
                        let (iface, changed, _): (String, HashMap<String, zbus::Variant<'_>>, Vec<String>) =
                            body.deserialize().unwrap();
                        let mut keys: Vec<_> = changed.into_keys().collect();
                        keys.sort();
                        Sig::Changed(iface, keys)
                    }
                    _ => continue,
                };
                if tx.send(sig).is_err() {
                    break;
                }
            }
        });
    });
    ready_rx.recv_timeout(TIMEOUT).unwrap();
    rx
}

fn next_sig(rx: &mpsc::Receiver<Sig>) -> Sig {
    rx.recv_timeout(TIMEOUT).expect("signal")
}

fn no_sig(rx: &mpsc::Receiver<Sig>) {
    if let Ok(sig) = rx.recv_timeout(Duration::from_millis(300)) {
        panic!("unexpected signal {sig:?}");
    }
}

fn player_changed(keys: &[&str]) -> Sig {
    Sig::Changed("org.mpris.MediaPlayer2.Player".into(), keys.iter().map(|k| k.to_string()).collect())
}

fn name_has_owner(name: &str) -> bool {
    let name = name.to_owned();
    zbus::block_on(async move {
        let conn = zbus::Connection::session().await.unwrap();
        let dbus = zbus::fdo::DBusProxy::new(&conn).await.unwrap();
        dbus.name_has_owner(name.as_str().try_into().unwrap()).await.unwrap()
    })
}

// ---------- tests ----------

#[test]
fn root_interface_properties_and_methods() {
    let s = session("root");
    let props = s.busctl(&[
        "get-property",
        "org.mpris.MediaPlayer2",
        "Identity",
        "DesktopEntry",
        "CanQuit",
        "CanRaise",
        "HasTrackList",
        "SupportedUriSchemes",
        "SupportedMimeTypes",
    ]);
    let lines: Vec<_> = props.lines().collect();
    assert_eq!(lines, ["s \"Vibrance\"", "s \"vibrance\"", "b true", "b true", "b false", "as 0", "as 0"]);

    s.busctl(&["call", "org.mpris.MediaPlayer2", "Raise"]);
    s.expect(&[Command::Raise]);
    s.busctl(&["call", "org.mpris.MediaPlayer2", "Quit"]);
    s.expect(&[Command::Quit]);
}

#[test]
fn playerctl_drives_engine_and_ui() {
    let s = session("ctl");
    let path = fixture();
    let signals = watch(s.mpris.bus_name());

    // Nothing published yet: CanPlay is false, so playerctl itself refuses play-pause.
    assert_eq!(s.playerctl(&["status"]), "Stopped");
    assert_eq!(s.busctl(&["get-property", "org.mpris.MediaPlayer2.Player", "CanPlay"]), "b false");

    // The UI has a queue but no current track.
    let queued = PlayerState { volume: 0.8, can_next: true, ..PlayerState::default() };
    s.mpris.update(queued.clone());
    assert_eq!(next_sig(&signals), player_changed(&["CanGoNext", "CanPlay", "Volume"]));

    // Stopped engine: play-pause asks the UI to start the queue.
    s.playerctl(&["play-pause"]);
    s.expect(&[Command::Play]);
    assert_eq!(s.engine.status().state, PlayState::Stopped);

    // The UI starts a track and publishes it.
    s.engine.play(Track::new(1, path.clone()), Duration::ZERO);
    s.wait_state(PlayState::Playing);
    let playing = PlayerState {
        status: Status::Playing,
        track: Some(info(&path)),
        volume: 0.8,
        shuffle: false,
        loop_status: LoopStatus::None,
        can_next: true,
        can_previous: true,
    };
    s.publish(playing.clone());
    assert_eq!(
        next_sig(&signals),
        player_changed(&["CanGoPrevious", "CanPause", "CanSeek", "Metadata", "PlaybackStatus"])
    );

    let meta = s.playerctl(&[
        "metadata",
        "--format",
        "{{mpris:trackid}}|{{xesam:title}}|{{xesam:artist}}|{{xesam:album}}|{{xesam:albumArtist}}|\
         {{xesam:trackNumber}}|{{mpris:length}}|{{mpris:artUrl}}|{{xesam:url}}",
    ]);
    let want_url = vibrance_desktop::file_url(&path);
    assert!(want_url.ends_with("/T%C3%B4ne%20%231%20%28100%25%29.wav"), "{want_url}");
    assert_eq!(
        meta,
        format!(
            "'/org/vibrance/track/7'|Sine Wave|Test Artist|Test Album|Various|3|30000000|\
             file:///tmp/cover%20art.png|{want_url}"
        )
    );
    println!("playerctl metadata:\n{}", s.playerctl(&["metadata"]));

    // Pause straight on the engine. The test never reads `commands` in between, like a hung
    // UI; the engine has already paused when playerctl returns.
    s.playerctl(&["play-pause"]);
    assert_eq!(s.engine.status().state, PlayState::Paused);
    s.expect(&[Command::Changed]);
    // The published status is the UI's business; it still says Playing.
    assert_eq!(s.playerctl(&["status"]), "Playing");

    // Position is read live from the engine.
    let engine_pos = s.engine.status().position.as_secs_f64();
    let pos = secs(&s.playerctl(&["position"]));
    assert!((pos - engine_pos).abs() < 0.05, "playerctl {pos} vs engine {engine_pos}");

    // SetPosition (absolute) and Seek (relative), both clamped, each with Seeked.
    s.playerctl(&["position", "12"]);
    let at = s.engine.status().position.as_secs_f64();
    assert!((at - 12.0).abs() < 0.05, "engine at {at}");
    assert!((secs(&s.playerctl(&["position"])) - 12.0).abs() < 0.05);
    s.expect(&[Command::Changed]);
    assert_eq!(next_sig(&signals), Sig::Seeked(12_000_000));

    s.playerctl(&["position", "5-"]);
    let at = s.engine.status().position.as_secs_f64();
    assert!((at - 7.0).abs() < 0.05, "engine at {at}");
    s.expect(&[Command::Changed]);
    assert_eq!(next_sig(&signals), Sig::Seeked(7_000_000));

    s.playerctl(&["position", "100-"]);
    assert!(s.engine.status().position.as_secs_f64() < 0.05);
    s.expect(&[Command::Changed]);
    assert_eq!(next_sig(&signals), Sig::Seeked(0));

    // Resume, then stop, both directly.
    s.playerctl(&["play-pause"]);
    assert_eq!(s.engine.status().state, PlayState::Playing);
    s.expect(&[Command::Changed]);
    s.playerctl(&["stop"]);
    assert_eq!(s.engine.status().state, PlayState::Stopped);
    s.expect(&[Command::Changed]);

    // Play while stopped goes to the UI; Pause and Stop while stopped do nothing.
    s.playerctl(&["play"]);
    s.expect(&[Command::Play]);
    s.playerctl(&["pause"]);
    s.playerctl(&["stop"]);
    s.expect(&[]);
    assert_eq!(s.engine.status().state, PlayState::Stopped);

    // Queue and settings go to the UI.
    s.playerctl(&["next"]);
    s.playerctl(&["previous"]);
    s.expect(&[Command::Next, Command::Previous]);
    s.playerctl(&["volume", "0.5"]);
    s.expect(&[Command::SetVolume(0.5)]);
    // zbus echoes the requested value, not the stale one.
    assert_eq!(next_sig(&signals), player_changed(&["Volume"]));
    assert_eq!(s.playerctl(&["volume"]), "0.500000");
    s.playerctl(&["shuffle", "On"]);
    s.expect(&[Command::SetShuffle(true)]);
    assert_eq!(next_sig(&signals), player_changed(&["Shuffle"]));
    s.playerctl(&["loop", "Track"]);
    s.expect(&[Command::SetLoop(LoopStatus::Track)]);
    assert_eq!(next_sig(&signals), player_changed(&["LoopStatus"]));

    // The UI confirms those settings: nothing left to signal.
    let confirmed =
        PlayerState { volume: 0.5, shuffle: true, loop_status: LoopStatus::Track, ..playing.clone() };
    s.mpris.update(confirmed.clone());
    no_sig(&signals);

    // Only what changed is signalled.
    s.mpris.update(PlayerState { status: Status::Paused, ..confirmed.clone() });
    assert_eq!(next_sig(&signals), player_changed(&["PlaybackStatus"]));
    s.mpris.update(PlayerState { status: Status::Paused, can_previous: false, ..confirmed.clone() });
    assert_eq!(next_sig(&signals), player_changed(&["CanGoPrevious"]));

    // `seeked` from the UI.
    s.mpris.seeked(Duration::from_millis(4_500));
    assert_eq!(next_sig(&signals), Sig::Seeked(4_500_000));

    // SetPosition for another track id is ignored.
    s.engine.play(Track::new(1, path.clone()), Duration::ZERO);
    s.wait_state(PlayState::Playing);
    let out = s.busctl(&[
        "call",
        "org.mpris.MediaPlayer2.Player",
        "SetPosition",
        "ox",
        "/org/vibrance/track/8",
        "20000000",
    ]);
    assert_eq!(out, "");
    assert!(s.engine.status().position.as_secs_f64() < 5.0);
    s.expect(&[]);
    no_sig(&signals);

    // Dropping the last handle releases the name.
    let name = s.mpris.bus_name().to_owned();
    assert!(name_has_owner(&name));
    drop(s);
    let deadline = Instant::now() + TIMEOUT;
    while name_has_owner(&name) {
        assert!(Instant::now() < deadline, "{name} still owned after drop");
        sleep(Duration::from_millis(20));
    }
}

#[test]
fn second_server_with_the_same_name_fails_cleanly() {
    let s = session("dup");
    let engine = Arc::new(Engine::new(Sink::Null).unwrap());
    let err = Mpris::start_with_suffix(engine, &s.player).err().expect("name is taken");
    assert!(err.to_string().contains("taken"), "{err}");
    // The first one still works.
    assert_eq!(s.playerctl(&["status"]), "Stopped");
}
