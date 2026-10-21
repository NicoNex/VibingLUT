//! Filmstrip under the Grade canvas: the photos graded together, each shown
//! through the current grade, and a grid view of all of them, so a look can
//! be judged on skin, sky, foliage and night shots at once.

use gettextrs::gettext;
use relm4::adw::prelude::*;
use relm4::gtk::gdk;
use relm4::{ComponentSender, gtk};

use crate::{App, Msg, vbox};

/// Most photos the strip holds: each one is re-rendered on every grade change.
pub const MAX_ROLL: usize = 12;
/// Longest side of strip photos, kept decoded so a profile change only re-encodes them.
pub const ROLL_PX: u32 = 720;

struct Card {
    root: gtk::Overlay,
    button: gtk::Button,
    pic: gtk::Picture,
    grid_pic: gtk::Picture,
    grid_button: gtk::Button,
}

pub struct Filmstrip {
    /// Revealed once a photo is open.
    pub root: gtk::Revealer,
    /// The grid view, shown in place of the single photo.
    pub grid_root: gtk::ScrolledWindow,
    strip: gtk::Box,
    add: gtk::Button,
    grid: gtk::FlowBox,
    cards: Vec<Card>,
}

impl Filmstrip {
    pub fn new(sender: &ComponentSender<App>) -> Self {
        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text(gettext("Add photos to grade together"))
            .valign(gtk::Align::Center)
            .css_classes(["circular", "roll-add"])
            .margin_start(6)
            .margin_end(6)
            .build();
        add.update_property(&[gtk::accessible::Property::Label(&gettext("Add photos"))]);
        let s = sender.clone();
        add.connect_clicked(move |_| s.input(Msg::Open));
        let strip =
            gtk::Box::builder().spacing(8).margin_start(12).margin_end(12).margin_top(8).margin_bottom(8).build();
        strip.append(&add);
        let scroll = gtk::ScrolledWindow::builder()
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&strip)
            .css_classes(["filmstrip"])
            .build();
        let root = gtk::Revealer::builder()
            .child(&scroll)
            .reveal_child(false)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .build();

        let grid = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            // Pictures shrink to their cell, so the columns come from these bounds.
            .min_children_per_line(2)
            .max_children_per_line(3)
            .row_spacing(12)
            .column_spacing(12)
            .margin_start(24)
            .margin_end(24)
            .margin_top(60) // clear of the canvas tools
            .margin_bottom(24)
            .valign(gtk::Align::Start)
            .build();
        let grid_root = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&grid)
            .css_classes(["canvas"])
            .build();
        Self { root, grid_root, strip, add, grid, cards: Vec::new() }
    }

    /// Recreates the cards for photos named `names` (structure changed).
    pub fn rebuild(&mut self, names: &[String], active: Option<usize>, sender: &ComponentSender<App>) {
        for c in self.cards.drain(..) {
            self.strip.remove(&c.root);
        }
        self.grid.remove_all();
        for (i, name) in names.iter().enumerate() {
            let pic = gtk::Picture::builder()
                .content_fit(gtk::ContentFit::Cover)
                .width_request(132)
                .height_request(88)
                .build();
            let label = gtk::Label::builder()
                .label(name)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .max_width_chars(16)
                .css_classes(["caption"])
                .build();
            let body = vbox(4);
            body.append(&pic);
            body.append(&label);
            let button =
                gtk::Button::builder().child(&body).css_classes(["flat", "roll-card"]).tooltip_text(name).build();
            button.update_property(&[gtk::accessible::Property::Label(name)]);
            let s = sender.clone();
            button.connect_clicked(move |_| s.input(Msg::RollSelect(i)));
            // The remove button sits beside the card button, never inside it.
            let remove = gtk::Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text(gettext("Remove from the filmstrip"))
                .halign(gtk::Align::End)
                .valign(gtk::Align::Start)
                .margin_top(4)
                .margin_end(4)
                .css_classes(["circular", "osd", "roll-remove"])
                .build();
            remove.update_property(&[gtk::accessible::Property::Label(&gettext!("Remove {}", name))]);
            let s = sender.clone();
            remove.connect_clicked(move |_| s.input(Msg::RollRemove(i)));
            let card = gtk::Overlay::builder().child(&button).css_classes(["roll-item"]).build();
            card.add_overlay(&remove);
            self.strip.insert_child_after(&card, self.cards.last().map(|c| &c.root));

            let grid_pic = gtk::Picture::builder()
                .content_fit(gtk::ContentFit::Contain)
                .height_request(240)
                .hexpand(true)
                .can_shrink(true)
                .build();
            let caption = gtk::Label::builder()
                .label(name)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .css_classes(["caption", "dim-label"])
                .build();
            let cell = vbox(6);
            cell.append(&grid_pic);
            cell.append(&caption);
            let grid_button = gtk::Button::builder()
                .child(&cell)
                .css_classes(["flat", "grid-card"])
                .tooltip_text(gettext("Edit this photo"))
                .build();
            grid_button.update_property(&[gtk::accessible::Property::Label(name)]);
            let s = sender.clone();
            grid_button.connect_clicked(move |_| {
                s.input(Msg::RollSelect(i));
                s.input(Msg::Grid(false));
            });
            self.grid.append(&grid_button);
            self.cards.push(Card { root: card, button, pic, grid_pic, grid_button });
        }
        // The add button always comes last.
        self.strip.reorder_child_after(&self.add, self.cards.last().map(|c| &c.root));
        self.set_active(active);
        self.root.set_reveal_child(!names.is_empty());
    }

    pub fn set_active(&self, active: Option<usize>) {
        for (i, c) in self.cards.iter().enumerate() {
            for b in [&c.button, &c.grid_button] {
                if Some(i) == active { b.add_css_class("active") } else { b.remove_css_class("active") }
            }
        }
    }

    /// The graded rendition of photo `i`.
    pub fn set_frame(&self, i: usize, texture: &gdk::MemoryTexture) {
        if let Some(c) = self.cards.get(i) {
            c.pic.set_paintable(Some(texture));
            c.grid_pic.set_paintable(Some(texture));
        }
    }
}
