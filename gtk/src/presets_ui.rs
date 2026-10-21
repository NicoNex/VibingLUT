//! Presets popover in the header bar: apply, save, delete, import, export.
//! The files are `vibinglut_core::preset` JSON under the config directory.

use std::path::PathBuf;

use gettextrs::gettext;
use relm4::adw::prelude::*;
use relm4::{ComponentSender, adw, gtk};

use crate::{App, Msg, vbox};

pub struct PresetsUi {
    pub button: gtk::MenuButton,
    list: gtk::ListBox,
}

pub fn build(sender: &ComponentSender<App>) -> PresetsUi {
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    let empty = gtk::Label::builder()
        .label(gettext("No presets yet. Save the current grade to reuse it on other photos."))
        .wrap(true)
        .max_width_chars(28)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["dim-label"])
        .build();
    list.set_placeholder(Some(&empty));
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(360)
        .child(&list)
        .build();

    let save = gtk::Button::builder()
        .label(gettext("Save Current Grade…"))
        .tooltip_text(gettext("Save the grade as a preset (Ctrl+S)"))
        .css_classes(["suggested-action", "pill"])
        .build();
    let import = gtk::Button::builder().label(gettext("Import…")).css_classes(["flat"]).hexpand(true).build();
    let export = gtk::Button::builder().label(gettext("Export…")).css_classes(["flat"]).hexpand(true).build();
    let files = gtk::Box::builder().homogeneous(true).spacing(6).build();
    files.append(&import);
    files.append(&export);

    let body = vbox(10);
    body.set_margin_top(6);
    body.set_margin_bottom(6);
    body.set_margin_start(6);
    body.set_margin_end(6);
    body.set_width_request(300);
    body.append(&gtk::Label::builder().label(gettext("Presets")).css_classes(["heading"]).xalign(0.0).build());
    body.append(&scroll);
    body.append(&save);
    body.append(&files);
    let popover = gtk::Popover::builder().child(&body).build();

    let button = gtk::MenuButton::builder()
        .icon_name("starred-symbolic")
        .tooltip_text(gettext("Presets"))
        .popover(&popover)
        .build();
    // The list is read from disk each time it opens, so it never goes stale.
    let s = sender.clone();
    popover.connect_show(move |_| s.input(Msg::PresetsRefresh));
    for (b, msg) in
        [(save, (|| Msg::PresetSave) as fn() -> Msg), (import, || Msg::PresetImport), (export, || Msg::PresetExport)]
    {
        let (s, p) = (sender.clone(), popover.clone());
        b.connect_clicked(move |_| {
            p.popdown();
            s.input(msg());
        });
    }
    PresetsUi { button, list }
}

impl PresetsUi {
    /// One row per preset: activate to apply, trash button to delete.
    pub fn fill(&self, presets: &[(String, PathBuf)], sender: &ComponentSender<App>) {
        self.list.remove_all();
        for (name, path) in presets {
            let row = adw::ActionRow::builder().title(name).use_markup(false).activatable(true).build();
            let delete = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text(gettext("Delete preset"))
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular"])
                .build();
            delete.update_property(&[gtk::accessible::Property::Label(&gettext!("Delete {}", name))]);
            let (s, p) = (sender.clone(), path.clone());
            delete.connect_clicked(move |_| s.input(Msg::PresetDelete(p.clone())));
            row.add_suffix(&delete);
            let (s, p, button) = (sender.clone(), path.clone(), self.button.clone());
            row.connect_activated(move |_| {
                button.popdown();
                s.input(Msg::PresetApply(p.clone()));
            });
            self.list.append(&row);
        }
    }
}

/// Asks for a preset name, then sends `Msg::PresetSaveAs`.
pub fn ask_name(window: &adw::ApplicationWindow, suggestion: &str, sender: &ComponentSender<App>) {
    let entry = gtk::Entry::builder().text(suggestion).activates_default(true).build();
    entry.update_property(&[gtk::accessible::Property::Label(&gettext("Preset name"))]);
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Save Preset"))
        .body(gettext("The grade is saved with all its settings and can be applied to any photo."))
        .extra_child(&entry)
        .default_response("save")
        .close_response("cancel")
        .build();
    dialog.add_responses(&[("cancel", &gettext("Cancel")), ("save", &gettext("Save"))]);
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    // An empty name cannot be saved.
    dialog.set_response_enabled("save", !suggestion.trim().is_empty());
    let d = dialog.clone();
    entry.connect_changed(move |e| d.set_response_enabled("save", !e.text().trim().is_empty()));
    let s = sender.clone();
    dialog.connect_response(Some("save"), move |_, _| s.input(Msg::PresetSaveAs(entry.text().trim().to_string())));
    dialog.present(Some(window));
}

/// Asks whether to replace the preset `name`; on yes sends `replace()`.
pub fn confirm_replace(
    window: &adw::ApplicationWindow,
    name: &str,
    replace: impl Fn() -> Msg + 'static,
    sender: &ComponentSender<App>,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Replace Preset?"))
        .body(gettext!("A preset named “{}” already exists. Replacing it overwrites its settings.", name))
        .default_response("cancel")
        .close_response("cancel")
        .build();
    dialog.add_responses(&[("cancel", &gettext("Cancel")), ("replace", &gettext("Replace"))]);
    dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
    let s = sender.clone();
    dialog.connect_response(Some("replace"), move |_, _| s.input(replace()));
    dialog.present(Some(window));
}
