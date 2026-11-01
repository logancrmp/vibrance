//! The engine handle: starts the threads, forwards commands, and answers status queries from
//! shared state without waiting on anything.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender};

use crate::output::{self, FILE_RATE, Output};
use crate::player::{Command, OutputControl, Player};
use crate::shared::{CoreStatus, Flush, Shared};
use crate::tap::TapBuffer;
use crate::wav::WavWriter;
use crate::{Error, Event, PlayState, Sink, Status};

pub(crate) struct Inner {
    commands: Sender<Command>,
    events: Receiver<Event>,
    shared: Arc<Shared>,
    tap: Arc<TapBuffer>,
    /// Slider position as f32 bits.
    slider: AtomicU32,
    muted: AtomicBool,
    decode: Option<JoinHandle<()>>,
    output: Option<Output>,
}

impl Inner {
    pub fn new(sink: Sink) -> Result<Self, Error> {
        let is_device = matches!(sink, Sink::Device);
        let (output, side) = match sink {
            Sink::Device => output::spawn_device()?,
            Sink::Wav { path, realtime } => {
                let wav = WavWriter::create(&path, FILE_RATE)?;
                output::spawn_file(Some(wav), realtime)?
            }
            Sink::Null => output::spawn_file(None, true)?,
        };
        let (commands, command_rx) = crossbeam_channel::unbounded();
        let (event_tx, events) = crossbeam_channel::unbounded();
        let player = Player::new(
            command_rx,
            event_tx,
            side.shared.clone(),
            side.samples,
            side.markers,
            side.notices,
            is_device.then(|| output.control.clone()),
        );
        let decode = match thread::Builder::new()
            .name("vibrance-decode".into())
            .spawn(move || player.run())
        {
            Ok(handle) => handle,
            Err(e) => {
                let _ = output.control.send(OutputControl::Shutdown);
                let _ = output.thread.join();
                return Err(e.into());
            }
        };
        Ok(Inner {
            commands,
            events,
            shared: side.shared,
            tap: side.tap,
            slider: AtomicU32::new(1.0f32.to_bits()),
            muted: AtomicBool::new(false),
            decode: Some(decode),
            output: Some(output),
        })
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    pub fn set_volume(&self, volume: f32) {
        let volume = if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 0.0 };
        self.slider.store(volume.to_bits(), Ordering::Relaxed);
        self.update_gain();
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
        self.update_gain();
    }

    fn update_gain(&self) {
        let slider = f32::from_bits(self.slider.load(Ordering::Relaxed));
        let gain = if self.muted.load(Ordering::Relaxed) { 0.0 } else { slider * slider * slider };
        self.shared.set_gain(gain);
    }

    pub fn events(&self) -> Receiver<Event> {
        self.events.clone()
    }

    pub fn tap(&self) -> Arc<TapBuffer> {
        self.tap.clone()
    }

    pub fn status(&self) -> Status {
        let shared = &self.shared;
        let state = shared.state();
        let core = CoreStatus::decode(shared.status.read());
        let flush = Flush::decode(shared.flush.read());
        // A flush the output has not picked up yet already decides what the user sees, unless
        // it only takes effect at a later point in the audio.
        if flush.generation > core.generation && flush.cut <= core.read_index {
            return Status {
                state,
                track: flush.seg.track,
                position: shared.frames_to_duration(flush.seg.pos0),
                duration: flush.seg.duration,
            };
        }
        let mut pos = core.pos_end;
        if shared.realtime && state == PlayState::Playing && !core.ended {
            // `pos_end` becomes audible at `end_due_ns`; until then the audible frame lags.
            let now = shared.now_ns();
            if now < core.end_due_ns {
                let behind = (core.end_due_ns - now) * u64::from(shared.rate) / 1_000_000_000;
                pos = pos.saturating_sub(behind).max(core.pos0);
            }
        }
        let mut position = shared.frames_to_duration(pos);
        if let Some(d) = core.duration {
            position = position.min(d);
        }
        Status {
            state: if core.ended { PlayState::Stopped } else { state },
            track: core.track,
            position,
            duration: core.duration,
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(handle) = self.decode.take() {
            let _ = handle.join();
        }
        if let Some(output) = self.output.take() {
            let _ = output.control.send(OutputControl::Shutdown);
            let _ = output.thread.join();
        }
    }
}
