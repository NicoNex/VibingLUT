//! Tiny persisted preferences under `$XDG_CONFIG_HOME/vibinglut/`.
// ponytail: one file per value; switch to GSettings once there are many.

use std::path::{Path, PathBuf};

use relm4::gtk::glib;

/// The LUT library folder.
pub const LUT_FOLDER: &str = "lut-folder";
/// Folder of the last opened photo, where "Open Photo" starts.
pub const PHOTO_FOLDER: &str = "photo-folder";

fn file(name: &str) -> PathBuf {
    glib::user_config_dir().join("vibinglut").join(name)
}

pub fn folder(key: &str) -> Option<PathBuf> {
    let p = PathBuf::from(std::fs::read_to_string(file(key)).ok()?.trim());
    p.is_dir().then_some(p)
}

pub fn set_folder(key: &str, dir: &Path) -> std::io::Result<()> {
    let f = file(key);
    std::fs::create_dir_all(f.parent().expect("config file has a parent"))?;
    std::fs::write(f, dir.to_string_lossy().as_bytes())
}
