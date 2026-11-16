//! User preferences, not playback history or per-file state.
use glib::{KeyFile, KeyFileFlags};
use std::{
    fs,
    path::{Path, PathBuf},
};
pub struct Preferences {
    path: PathBuf,
    data: KeyFile,
}
impl Preferences {
    pub fn path() -> PathBuf {
        glib::user_config_dir().join("kimbap/preferences.ini")
    }
    pub fn defaults(path: PathBuf) -> Self {
        Self {
            path,
            data: KeyFile::new(),
        }
    }
    pub fn load(path: PathBuf) -> Result<Self, String> {
        let data = KeyFile::new();
        match fs::read(&path) {
            Ok(bytes) => {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|e| format!("Preferences are not UTF-8: {e}"))?;
                data.load_from_data(text, KeyFileFlags::KEEP_COMMENTS)
                    .map_err(|e| format!("Invalid preferences: {e}"))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(format!("Cannot read preferences: {e}")),
        }
        let prefs = Self { path, data };
        prefs.muted()?;
        Ok(prefs)
    }
    pub fn muted(&self) -> Result<bool, String> {
        if !self.data.has_group("video") || !self.data.has_key("video", "muted").unwrap_or(false) {
            return Ok(true);
        }
        self.data
            .boolean("video", "muted")
            .map_err(|e| format!("Invalid video mute preference: {e}"))
    }
    pub fn set_muted(&self, muted: bool) -> Result<(), String> {
        self.data.set_boolean("video", "muted", muted);
        let parent = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create preferences directory: {e}"))?;
        // GLib writes a temporary sibling and atomically replaces the destination.
        glib::file_set_contents(&self.path, self.data.to_data().as_bytes())
            .map_err(|e| format!("Cannot save mute preference: {e}"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mute_survives_recreation() {
        let dir = std::env::temp_dir().join(format!("kimbap-prefs-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("preferences.ini");
        let p = Preferences::load(path.clone()).unwrap();
        assert!(p.muted().unwrap());
        p.set_muted(false).unwrap();
        assert!(!Preferences::load(path.clone()).unwrap().muted().unwrap());
        p.set_muted(true).unwrap();
        assert!(Preferences::load(path.clone()).unwrap().muted().unwrap());
        fs::write(&path, "[video]\nmuted=not-a-boolean\n").unwrap();
        assert!(Preferences::load(path).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
