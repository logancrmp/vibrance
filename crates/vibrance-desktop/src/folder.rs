//! "Show in Folder": `org.freedesktop.FileManager1.ShowItems`, which opens the
//! folder with the file selected, falling back to `xdg-open <folder>`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use mpris_server::zbus;

use crate::metadata::file_url;

const FILE_MANAGER: &str = "org.freedesktop.FileManager1";
const FILE_MANAGER_PATH: &str = "/org/freedesktop/FileManager1";
/// The bus daemon's own activation timeout; a file manager that starts slower has failed.
const CALL_TIMEOUT: Duration = Duration::from_secs(25);

pub fn show_in_folder(file: &Path) {
    let file = file.to_path_buf();
    let spawned = thread::Builder::new().name("vibrance-show-in-folder".to_owned()).spawn(move || {
        if let Err(e) = show_items(FILE_MANAGER, &file) {
            log::info!("FileManager1.ShowItems failed ({e}); opening the folder instead");
            open_folder(&fallback_dir(&file));
        }
    });
    if let Err(e) = spawned {
        log::warn!("cannot start Show in Folder: {e}");
    }
}

/// Arguments of `ShowItems(as uris, s startup_id)`.
pub fn show_items_args(file: &Path) -> (Vec<String>, String) {
    (vec![file_url(file)], String::new())
}

/// The folder `xdg-open` gets: the file's parent, as an absolute path.
pub fn fallback_dir(file: &Path) -> PathBuf {
    let file = std::path::absolute(file).unwrap_or_else(|_| file.to_path_buf());
    match file.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => file,
    }
}

fn show_items(destination: &str, file: &Path) -> zbus::Result<()> {
    let body = show_items_args(file);
    zbus::block_on(async {
        let conn = zbus::conn::Builder::session().method_timeout(CALL_TIMEOUT).build().await?;
        conn.call_method(Some(destination), FILE_MANAGER_PATH, Some(FILE_MANAGER), "ShowItems", &body)
            .await?;
        Ok(())
    })
}

fn open_folder(dir: &Path) {
    let child = Command::new("xdg-open")
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match child {
        // Reap it; this is a throwaway thread anyway.
        Ok(mut child) => match child.wait() {
            Ok(status) if !status.success() => log::warn!("xdg-open {} failed: {status}", dir.display()),
            Ok(_) => {}
            Err(e) => log::warn!("xdg-open {}: {e}", dir.display()),
        },
        Err(e) => log::warn!("cannot run xdg-open: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_items_gets_one_encoded_file_url_and_no_startup_id() {
        let (uris, startup) = show_items_args(Path::new("/music/My Band/01 #1 Hit (50%).mp3"));
        assert_eq!(uris, ["file:///music/My%20Band/01%20%231%20Hit%20%2850%25%29.mp3"]);
        assert_eq!(startup, "");
    }

    #[test]
    fn show_items_reports_a_missing_file_manager() {
        // Nothing owns or activates this name, so the call fails (which triggers the
        // xdg-open fallback) without opening any window. Also fails if there is no bus.
        let r = show_items("org.freedesktop.FileManager1.VibranceTestNobody", Path::new("/x.flac"));
        assert!(r.is_err());
    }

    #[test]
    fn fallback_opens_the_parent_folder() {
        assert_eq!(fallback_dir(Path::new("/music/Björk/x.flac")), Path::new("/music/Björk"));
        assert_eq!(fallback_dir(Path::new("/x.flac")), Path::new("/"));
        assert_eq!(fallback_dir(Path::new("/")), Path::new("/"));
        let rel = fallback_dir(Path::new("x.flac"));
        assert!(rel.is_absolute());
        assert_eq!(rel, std::env::current_dir().unwrap());
    }
}
