//! The desktop entry and icon (SPEC 3.2). On Wayland the taskbar finds a window's icon
//! through its app ID, which must name an installed `.desktop` file; without one it shows a
//! generic icon.

use std::io;
use std::path::Path;

/// The window's app ID, and the name of the desktop entry and icon.
pub const APP_ID: &str = "io.github.logancrmp.vibrance";

const ICON: &str = include_str!("../../../assets/icons/vibrance.svg");

/// Install or refresh the desktop entry (launching `exe`) and icon under `data_home`
/// (`$XDG_DATA_HOME`). Returns whether anything changed.
pub fn install_launcher(data_home: &Path, exe: &Path) -> io::Result<bool> {
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=Vibrance\nGenericName=Music Player\nComment=Play your music library\n\
         Exec=\"{}\"\nIcon={APP_ID}\nTerminal=false\nCategories=AudioVideo;Audio;Player;\nStartupWMClass={APP_ID}\n",
        exe.display()
    );
    let desktop = write_if_changed(&data_home.join("applications").join(format!("{APP_ID}.desktop")), &entry)?;
    let icon = write_if_changed(&data_home.join("icons/hicolor/scalable/apps").join(format!("{APP_ID}.svg")), ICON)?;
    Ok(desktop || icon)
}

fn write_if_changed(path: &Path, contents: &str) -> io::Result<bool> {
    if std::fs::read_to_string(path).is_ok_and(|old| old == contents) {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write then rename, so a desktop watching the folder never reads half a file.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_once_and_points_at_the_binary() {
        let home = std::env::temp_dir().join(format!("vibrance-launcher-{}", std::process::id()));
        let exe = Path::new("/opt/vibrance dev/vibrance");
        assert!(install_launcher(&home, exe).unwrap());
        assert!(!install_launcher(&home, exe).unwrap(), "unchanged files are left alone");
        let entry = std::fs::read_to_string(home.join(format!("applications/{APP_ID}.desktop"))).unwrap();
        assert!(entry.contains("Exec=\"/opt/vibrance dev/vibrance\"\n"));
        assert!(entry.contains(&format!("Icon={APP_ID}\n")));
        assert!(home.join(format!("icons/hicolor/scalable/apps/{APP_ID}.svg")).exists());
        assert!(install_launcher(&home, Path::new("/elsewhere/vibrance")).unwrap(), "a moved binary updates Exec");
        std::fs::remove_dir_all(&home).unwrap();
    }
}
