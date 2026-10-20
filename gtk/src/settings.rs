//! Tiny persisted preferences under `$XDG_CONFIG_HOME/vibinglut/`.
// ponytail: one file per value; switch to GSettings once there are many.

use std::path::{Path, PathBuf};

use relm4::gtk::glib;

fn file(name: &str) -> PathBuf {
    glib::user_config_dir().join("vibinglut").join(name)
}

pub fn lut_folder() -> Option<PathBuf> {
    let p = PathBuf::from(std::fs::read_to_string(file("lut-folder")).ok()?.trim());
    p.is_dir().then_some(p)
}

pub fn set_lut_folder(dir: &Path) -> std::io::Result<()> {
    let f = file("lut-folder");
    std::fs::create_dir_all(f.parent().expect("config file has a parent"))?;
    std::fs::write(f, dir.to_string_lossy().as_bytes())
}
