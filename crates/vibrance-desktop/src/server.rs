//! The MPRIS server thread: registration on the session bus, the `org.mpris.MediaPlayer2` and
//! `.Player` interfaces, and the loop that publishes `PlayerState` updates and `Seeked`.

use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use mpris_server::zbus::{self, fdo};
use mpris_server::{
    LoopStatus, Metadata, PlaybackRate, PlaybackStatus, PlayerInterface, RootInterface, Server,
    Signal, Time, TrackId, Volume,
};

use crate::control::{Core, Transport};
use crate::metadata::{
    build_metadata, can_play, changed_properties, from_mpris_loop, has_track, playback_status,
    to_mpris_loop, to_time,
};
use crate::{Command, Error, PlayerState};

/// How long `start` waits for the session bus before giving up.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// Messages from `Mpris` handles (and the interface itself) to the serve loop.
pub enum Msg {
    Update(PlayerState),
    Seeked(Duration),
}

pub struct Player<E> {
    core: Core<E>,
    /// For `Seeked` after an MPRIS seek. Weak, so the loop ends once every `Mpris` is dropped.
    loopback: async_channel::WeakSender<Msg>,
}

impl<E: Transport> Player<E> {
    fn seeked(&self, position: Option<Duration>) {
        if let Some(position) = position
            && let Some(tx) = self.loopback.upgrade()
        {
            let _ = tx.try_send(Msg::Seeked(position));
        }
    }
}

impl<E: Transport> RootInterface for Player<E> {
    async fn raise(&self) -> fdo::Result<()> {
        self.core.send(Command::Raise);
        Ok(())
    }

    async fn quit(&self) -> fdo::Result<()> {
        self.core.send(Command::Quit);
        Ok(())
    }

    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_fullscreen(&self, _fullscreen: bool) -> zbus::Result<()> {
        Ok(())
    }

    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn identity(&self) -> fdo::Result<String> {
        Ok("Vibrance".to_owned())
    }

    async fn desktop_entry(&self) -> fdo::Result<String> {
        Ok("vibrance".to_owned())
    }

    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

impl<E: Transport> PlayerInterface for Player<E> {
    async fn next(&self) -> fdo::Result<()> {
        self.core.send(Command::Next);
        Ok(())
    }

    async fn previous(&self) -> fdo::Result<()> {
        self.core.send(Command::Previous);
        Ok(())
    }

    async fn pause(&self) -> fdo::Result<()> {
        self.core.pause();
        Ok(())
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        self.core.play_pause();
        Ok(())
    }

    async fn stop(&self) -> fdo::Result<()> {
        self.core.stop();
        Ok(())
    }

    async fn play(&self) -> fdo::Result<()> {
        self.core.play();
        Ok(())
    }

    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        self.seeked(self.core.seek_by(offset.as_micros()));
        Ok(())
    }

    async fn set_position(&self, track_id: TrackId, position: Time) -> fdo::Result<()> {
        self.seeked(self.core.set_position(track_id.as_str(), position.as_micros()));
        Ok(())
    }

    async fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("Vibrance does not open URIs over MPRIS".to_owned()))
    }

    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        Ok(playback_status(self.core.state().status))
    }

    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(to_mpris_loop(self.core.state().loop_status))
    }

    async fn set_loop_status(&self, loop_status: LoopStatus) -> zbus::Result<()> {
        self.core.set_loop(from_mpris_loop(loop_status));
        Ok(())
    }

    async fn rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn set_rate(&self, _rate: PlaybackRate) -> zbus::Result<()> {
        Ok(())
    }

    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(self.core.state().shuffle)
    }

    async fn set_shuffle(&self, shuffle: bool) -> zbus::Result<()> {
        self.core.set_shuffle(shuffle);
        Ok(())
    }

    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(build_metadata(self.core.state().track.as_ref()))
    }

    async fn volume(&self) -> fdo::Result<Volume> {
        Ok(self.core.state().volume)
    }

    async fn set_volume(&self, volume: Volume) -> zbus::Result<()> {
        self.core.set_volume(volume);
        Ok(())
    }

    async fn position(&self) -> fdo::Result<Time> {
        Ok(to_time(self.core.position()))
    }

    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(self.core.state().can_next)
    }

    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(self.core.state().can_previous)
    }

    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(can_play(&self.core.state()))
    }

    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(has_track(&self.core.state()))
    }

    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(has_track(&self.core.state()))
    }

    async fn can_control(&self) -> fdo::Result<bool> {
        Ok(true)
    }
}

/// What `spawn` hands back to `Mpris::start`.
pub struct Started {
    pub messages: async_channel::Sender<Msg>,
    pub commands: async_channel::Receiver<Command>,
    pub bus_name: String,
}

/// Start the server thread and wait until it owns a bus name (the first free one of
/// `suffixes`) or fails.
pub fn spawn<E: Transport>(engine: Arc<E>, suffixes: Vec<String>) -> Result<Started, Error> {
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (msg_tx, msg_rx) = async_channel::unbounded();
    let loopback = msg_tx.downgrade();
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<String, Error>>(1);

    thread::Builder::new()
        .name("vibrance-mpris".to_owned())
        .spawn(move || {
            zbus::block_on(async move {
                let make = || Player {
                    core: Core::new(Arc::clone(&engine), cmd_tx.clone()),
                    loopback: loopback.clone(),
                };
                let server = match register(&suffixes, make).await {
                    Ok(server) => server,
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };
                // `start` stops listening when it times out; then nobody wants this server.
                if init_tx.send(Ok(server.bus_name().to_string())).is_err() {
                    return;
                }
                drop(init_tx);
                serve(server, msg_rx).await;
                log::debug!("mpris: server stopped");
            });
        })
        .map_err(|e| Error::Dbus(format!("cannot start the MPRIS thread: {e}")))?;

    match init_rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(bus_name)) => Ok(Started { messages: msg_tx, commands: cmd_rx, bus_name }),
        Ok(Err(e)) => Err(e),
        Err(RecvTimeoutError::Timeout) => {
            Err(Error::Dbus("timed out connecting to the session bus".to_owned()))
        }
        Err(RecvTimeoutError::Disconnected) => {
            Err(Error::Dbus("the MPRIS thread exited during startup".to_owned()))
        }
    }
}

/// Take the first bus name that is free. Another Vibrance instance may own the plain one.
async fn register<E: Transport>(
    suffixes: &[String],
    make: impl Fn() -> Player<E>,
) -> Result<Server<Player<E>>, Error> {
    let dbus_err = |e: &dyn std::fmt::Display| Error::Dbus(e.to_string());
    // zbus requests names with REPLACE_EXISTING | ALLOW_REPLACEMENT and mpris-server offers no
    // way to change that, so registering an owned name would silently take it from the other
    // instance, leaving its audio without media-key control. Skip owned names instead.
    let probe = zbus::Connection::session().await.map_err(|e| dbus_err(&e))?;
    let dbus = fdo::DBusProxy::new(&probe).await.map_err(|e| dbus_err(&e))?;
    let mut last = Error::Dbus("no bus name to register".to_owned());
    for suffix in suffixes {
        let name = format!("org.mpris.MediaPlayer2.{suffix}");
        let bus_name = name.as_str().try_into().map_err(|e: zbus::Error| dbus_err(&e))?;
        let taken = dbus.name_has_owner(bus_name).await.map_err(|e| dbus_err(&e))?;
        let result = if taken { Err(zbus::Error::NameTaken) } else { Server::new(suffix, make()).await };
        match result {
            Ok(server) => return Ok(server),
            Err(zbus::Error::NameTaken) => {
                log::info!("mpris: {name} is taken");
                last = Error::Dbus(format!("{name} is taken"));
            }
            Err(e) => return Err(dbus_err(&e)),
        }
    }
    Err(last)
}

/// Publish updates until every `Mpris` handle is dropped.
async fn serve<E: Transport>(server: Server<Player<E>>, messages: async_channel::Receiver<Msg>) {
    while let Ok(msg) = messages.recv().await {
        let result = match msg {
            Msg::Update(new) => {
                let changes = {
                    let mut state = server.imp().core.state();
                    let changes = changed_properties(&state, &new);
                    *state = new;
                    changes
                };
                if changes.is_empty() { Ok(()) } else { server.properties_changed(changes).await }
            }
            Msg::Seeked(position) => server.emit(Signal::Seeked { position: to_time(position) }).await,
        };
        if let Err(e) = result {
            log::warn!("mpris: cannot emit signal: {e}");
        }
    }
}
