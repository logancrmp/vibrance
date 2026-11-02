//! Background saving of UI state and settings, so the UI thread never writes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

enum Write {
    State(String, String),
    File(PathBuf, String),
    Flush(Sender<()>),
}

#[derive(Clone)]
pub struct Persist {
    tx: Sender<Write>,
}

impl Persist {
    pub fn spawn(database: PathBuf) -> Persist {
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::Builder::new()
            .name("vibrance-persist".into())
            .spawn(move || run(database, rx))
            .expect("spawn persist thread");
        Persist { tx }
    }

    /// Debounced key/value write into the library database.
    pub fn state(&self, key: &str, value: String) {
        let _ = self.tx.send(Write::State(key.to_owned(), value));
    }

    pub fn file(&self, path: PathBuf, contents: String) {
        let _ = self.tx.send(Write::File(path, contents));
    }

    /// Block until everything queued so far is on disk (used at exit).
    pub fn flush(&self) {
        let (done, wait) = crossbeam_channel::bounded(1);
        if self.tx.send(Write::Flush(done)).is_ok() {
            let _ = wait.recv_timeout(Duration::from_secs(2));
        }
    }
}

fn run(database: PathBuf, rx: Receiver<Write>) {
    let mut pending_state: HashMap<String, String> = HashMap::new();
    let mut pending_files: HashMap<PathBuf, String> = HashMap::new();
    let mut library = None;
    loop {
        let msg = if pending_state.is_empty() && pending_files.is_empty() {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            rx.recv_timeout(Duration::from_secs(1))
        };
        let mut flush_ack = None;
        match msg {
            Ok(Write::State(k, v)) => {
                pending_state.insert(k, v);
                continue;
            }
            Ok(Write::File(p, c)) => {
                pending_files.insert(p, c);
                continue;
            }
            Ok(Write::Flush(ack)) => flush_ack = Some(ack),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                write_all(&database, &mut library, &mut pending_state, &mut pending_files);
                return;
            }
        }
        write_all(&database, &mut library, &mut pending_state, &mut pending_files);
        if let Some(ack) = flush_ack {
            let _ = ack.send(());
        }
    }
}

fn write_all(
    database: &std::path::Path,
    library: &mut Option<vibrance_library::Library>,
    state: &mut HashMap<String, String>,
    files: &mut HashMap<PathBuf, String>,
) {
    if !state.is_empty() {
        if library.is_none() {
            match vibrance_library::Library::open(database) {
                Ok(l) => *library = Some(l),
                Err(err) => log::error!("persist: cannot open database: {err}"),
            }
        }
        if let Some(lib) = library {
            for (k, v) in state.drain() {
                if let Err(err) = lib.set_state(&k, &v) {
                    log::error!("persist: cannot save {k}: {err}");
                }
            }
        }
    }
    for (path, contents) in files.drain() {
        let tmp = path.with_extension("tmp");
        if let Err(err) = std::fs::write(&tmp, contents).and_then(|()| std::fs::rename(&tmp, &path)) {
            log::error!("persist: cannot write {}: {err}", path.display());
        }
    }
}
