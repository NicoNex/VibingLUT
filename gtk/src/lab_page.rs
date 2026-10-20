//! LUT Lab page: a stack of LUTs, the current grade and profile conversions,
//! chained or mixed into one LUT (maths in `vibinglut_core::lab`).

use std::sync::Arc;

use gettextrs::gettext;
use relm4::adw::prelude::*;
use relm4::gtk::gio;
use relm4::{ComponentSender, adw, gtk};
use vibinglut_core::lab::Blend;
use vibinglut_core::lut::Lut3d;
use vibinglut_core::profiles::Profile;

use crate::widgets::Slider;
use crate::{App, Msg, vbox};

#[derive(Clone, Debug)]
pub enum LabKind {
    Lut(Arc<Lut3d>),
    Grade,
    Convert(Profile),
}

/// The two layer stacks: the LUT Lab's and the LUTs applied after the grade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layers {
    Lab,
    Look,
}

#[derive(Clone, Debug)]
pub struct LabLayer {
    pub kind: LabKind,
    pub intensity: f32,
    /// Signal the LUT expects (unused for grade and conversions).
    pub input: Profile,
}

#[derive(Debug, Clone, Copy)]
pub enum LabAdd {
    Selected,
    File,
    Grade,
    Convert,
}

/// Plain-words description of what the exported LUT does.
fn summary(layers: &[LabLayer], blend: Blend, input: Profile) -> String {
    if layers.is_empty() {
        return String::new();
    }
    let name = |l: &LabLayer| match &l.kind {
        LabKind::Lut(lut) => format!("«{}» ({:.0}%)", lut.title, l.intensity * 100.0),
        LabKind::Grade => format!("{} ({:.0}%)", gettext("current grade"), l.intensity * 100.0),
        LabKind::Convert(p) => gettext!("conversion to {}", p.name()),
    };
    let mut steps = vec![gettext!("Input: {}", input.name())];
    let mut at = input;
    match blend {
        Blend::Chain => {
            for l in layers {
                if let LabKind::Lut(_) = l.kind
                    && l.input != at
                {
                    steps.push(gettext!("converted to {}", l.input.name()));
                }
                steps.push(name(l));
                at = match l.kind {
                    LabKind::Convert(p) => p,
                    _ => Profile::Standard,
                };
            }
        }
        Blend::Mix => {
            let looks: Vec<String> =
                layers.iter().filter(|l| !matches!(l.kind, LabKind::Convert(_))).map(name).collect();
            steps.push(gettext!("mix of {}", looks.join(" + ")));
            at = Profile::Standard;
            for l in layers {
                if let LabKind::Convert(p) = l.kind {
                    steps.push(name(l));
                    at = p;
                }
            }
        }
    }
    let out = if at.is_log() { at.name().to_string() } else { "Rec.709".into() };
    steps.push(gettext!("Output: {}", out));
    steps.join("  →  ")
}

fn chain_hint() -> String {
    gettext("Chain: layers apply one after another; intensity fades each step.")
}

fn mix_hint() -> String {
    gettext(
        "Mix: every LUT gets the same input; outputs are averaged in OKLab weighted by intensity \
         (below 100% the rest is the neutral image). Conversions apply to the result.",
    )
}

pub struct LabUi {
    pub root: adw::OverlaySplitView,
    pub list: gtk::ListBox,
    pub stack: gtk::Stack,
    pub pic: gtk::Picture,
    pub canvas: gtk::Overlay,
    pub export: gtk::Box,
    pub summary: gtk::Label,
}

pub(crate) fn profile_dropdown(selected: Profile) -> gtk::DropDown {
    let names: Vec<&str> = Profile::ALL.iter().map(|p| p.name()).collect();
    let d = gtk::DropDown::from_strings(&names);
    d.set_selected(Profile::ALL.iter().position(|p| *p == selected).unwrap_or(0) as u32);
    d
}

fn caption(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).css_classes(["caption", "dim-label"]).wrap(true).xalign(0.0).build()
}

fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).css_classes(["heading"]).xalign(0.0).hexpand(true).build()
}

pub fn build(sender: &ComponentSender<App>, export_menu: &gio::Menu) -> LabUi {
    let panel = vbox(12);
    panel.set_margin_top(12);
    panel.set_margin_bottom(18);
    panel.set_margin_start(12);
    panel.set_margin_end(12);

    let intro = caption(&gettext(
        "To convert a LUT from one profile to another: 1) add the LUT, 2) check which profile it was \
         made for, 3) choose the signal you will use the result on, 4) export.",
    ));
    panel.append(&intro);

    // 1 · Layers
    let add_menu = gio::Menu::new();
    add_menu.append(Some(gettext("LUT from file…").as_str()), Some("win.lab-add::file"));
    add_menu.append(Some(gettext("LUT selected in Compare").as_str()), Some("win.lab-add::selected"));
    add_menu.append(Some(gettext("Current grade").as_str()), Some("win.lab-add::grade"));
    add_menu.append(Some(gettext("Profile conversion").as_str()), Some("win.lab-add::convert"));
    let add = gtk::MenuButton::builder()
        .icon_name("list-add-symbolic")
        .menu_model(&add_menu)
        .tooltip_text(gettext("Add layer"))
        .css_classes(["flat"])
        .build();
    let head = gtk::Box::builder().spacing(6).build();
    head.append(&heading(&gettext("1 · Layers")));
    head.append(&add);
    panel.append(&head);
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    let empty = vbox(8);
    empty.set_margin_top(12);
    empty.set_margin_bottom(12);
    empty.append(&caption(&gettext("No layers.")));
    let open = gtk::Button::builder()
        .label(gettext("Add a LUT…"))
        .css_classes(["pill", "suggested-action"])
        .halign(gtk::Align::Center)
        .action_name("win.lab-add")
        .action_target(&"file".to_variant())
        .build();
    empty.append(&open);
    list.set_placeholder(Some(&empty));
    panel.append(&list);

    // 2 · Result: the camera signal the combined LUT will receive.
    let input = profile_dropdown(Profile::Standard);
    input.update_property(&[gtk::accessible::Property::Label(&gettext("Signal the final LUT will be used on"))]);
    let snd = sender.clone();
    input.connect_selected_notify(move |d| snd.input(Msg::LabInput(Profile::ALL[d.selected() as usize])));
    let result = vbox(6);
    result.add_css_class("card");
    result.add_css_class("section");
    let rh = heading(&gettext("2 · I will use the final LUT on"));
    rh.set_margin_top(6);
    result.append(&rh);
    result.append(&input);
    result.append(&caption(
        "Il Photo Style / log della camera su cui caricherai il LUT esportato. Il segnale viene convertito \
         automaticamente nel profilo di ogni livello.",
    ));
    panel.append(&result);

    // How layers combine (only matters with two or more).
    let mode = vbox(6);
    mode.add_css_class("card");
    mode.add_css_class("section");
    let mh = heading(&gettext("Layer blending"));
    mh.set_margin_top(6);
    mode.append(&mh);
    let blend = crate::toggles(&[("chain", gettext("Chain").as_str()), ("mix", gettext("Mix").as_str())]);
    let hint = caption(&chain_hint());
    let (snd, h) = (sender.clone(), hint.clone());
    blend.connect_active_name_notify(move |g| {
        let mix = g.active_name().as_deref() == Some("mix");
        h.set_label(&if mix { mix_hint() } else { chain_hint() });
        snd.input(Msg::LabBlend(if mix { Blend::Mix } else { Blend::Chain }));
    });
    mode.append(&blend);
    mode.append(&hint);
    panel.append(&mode);

    // What the exported LUT will do, in words.
    let summary =
        gtk::Label::builder().css_classes(["caption", "lab-summary"]).wrap(true).xalign(0.0).selectable(true).build();
    panel.append(&summary);

    // 3 · Export
    panel.append(&heading(&gettext("3 · Export")));
    let row = gtk::Box::builder().spacing(6).homogeneous(true).build();
    for (label, size) in [("Lumix · 33³ .cube", 33), ("Post · 65³ .cube", 65)] {
        let b = gtk::Button::builder().label(label).css_classes(["suggested-action"]).build();
        let snd = sender.clone();
        b.connect_clicked(move |_| snd.input(Msg::Export(vibinglut_core::lut::Format::Cube, size, crate::What::Lab)));
        row.append(&b);
    }
    let more = gtk::MenuButton::builder()
        .label(gettext("More formats (VLT, HALD, 129³)…"))
        .menu_model(export_menu)
        .css_classes(["flat"])
        .build();
    let export = vbox(6);
    export.set_sensitive(false);
    export.append(&row);
    export.append(&more);
    panel.append(&export);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&panel)
        .vexpand(true)
        .css_classes(["sidebar-pane"])
        .build();

    let pic = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    pic.update_property(&[gtk::accessible::Property::Label(&gettext("Combined LUT preview"))]);
    crate::add_split_drag(&pic, sender);
    let canvas = gtk::Overlay::builder().child(&pic).css_classes(["canvas"]).build();
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
    stack.add_named(
        &adw::StatusPage::builder()
            .icon_name("view-list-symbolic")
            .title("LUT Lab")
            .description(gettext(
                "Combine LUTs, grades and profile conversions: chained or mixed, each with its own intensity.\n\
                     E.g. a Fujifilm LUT for F-Log becomes a LUT for V-Log or for the Standard profile.",
            ))
            .build(),
        Some("empty"),
    );
    stack.add_named(&canvas, Some("image"));

    let root = adw::OverlaySplitView::builder()
        .sidebar_position(gtk::PackType::Start)
        .min_sidebar_width(320.0)
        .max_sidebar_width(400.0)
        .content(&stack)
        .sidebar(&scroll)
        .build();
    LabUi { root, list, stack, pic, canvas, export, summary }
}

impl LabUi {
    pub fn refresh_summary(&self, layers: &[LabLayer], blend: Blend, result_input: Profile) {
        self.summary.set_label(&summary(layers, blend, result_input));
    }

    /// Recreates the layer rows (structural changes are rare; sliders update in place).
    pub fn rebuild(
        &self,
        layers: &[LabLayer],
        blend: Blend,
        result_input: Profile,
        source_profile: Profile,
        sender: &ComponentSender<App>,
    ) {
        self.refresh_summary(layers, blend, result_input);
        layer_rows(&self.list, layers, Layers::Lab, source_profile, sender);
        self.export.set_sensitive(!layers.is_empty());
        self.stack.set_visible_child_name(if layers.is_empty() { "empty" } else { "image" });
    }
}

/// Rows for a layer list: reorder/remove buttons, the LUT's source profile, intensity.
pub fn layer_rows(
    list: &gtk::ListBox,
    layers: &[LabLayer],
    which: Layers,
    source_profile: Profile,
    sender: &ComponentSender<App>,
) {
    list.remove_all();
    let last = layers.len().saturating_sub(1);
    for (i, l) in layers.iter().enumerate() {
        let row = vbox(6);
        row.set_margin_top(8);
        row.set_margin_bottom(10);
        row.set_margin_start(10);
        row.set_margin_end(6);

        let (kind, title) = match &l.kind {
            LabKind::Lut(lut) => (lut.format.badge(), lut.title.clone()),
            LabKind::Grade => ("GRADE", gettext("Current grade")),
            LabKind::Convert(_) => ("CONV", gettext("Conversion")),
        };
        let head = gtk::Box::builder().spacing(4).build();
        head.append(&gtk::Label::builder().label(kind).css_classes(["lut-badge"]).valign(gtk::Align::Center).build());
        head.append(
            &gtk::Label::builder()
                .label(title.as_str())
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .xalign(0.0)
                .hexpand(true)
                .css_classes(["heading"])
                .tooltip_text(title.as_str())
                .build(),
        );
        for (icon, tip, enabled, msg) in [
            ("go-up-symbolic", gettext("Move up"), i > 0, Msg::LayerMove(which, i, true)),
            ("go-down-symbolic", gettext("Move down"), i < last, Msg::LayerMove(which, i, false)),
            ("user-trash-symbolic", gettext("Remove"), true, Msg::LayerRemove(which, i)),
        ] {
            let b = gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tip)
                .sensitive(enabled)
                .css_classes(["flat", "circular"])
                .build();
            let (snd, msg) = (sender.clone(), std::cell::Cell::new(Some(msg)));
            b.connect_clicked(move |_| {
                if let Some(m) = msg.take() {
                    snd.input(m);
                }
            });
            head.append(&b);
        }
        row.append(&head);

        let profile_row = match &l.kind {
            LabKind::Lut(_) => Some((gettext("This LUT was made for:"), l.input)),
            LabKind::Convert(p) => Some((gettext("Convert the signal to"), *p)),
            LabKind::Grade => None,
        };
        match profile_row {
            Some((label, profile)) => {
                let d = profile_dropdown(profile);
                let snd = sender.clone();
                d.connect_selected_notify(move |d| {
                    snd.input(Msg::LayerProfile(which, i, Profile::ALL[d.selected() as usize]))
                });
                row.append(&caption(&label));
                row.append(&d);
            }
            None => row.append(&caption(&gettext!("Input: {}", source_profile.name()))),
        }
        if !matches!(l.kind, LabKind::Convert(_)) {
            let s = Slider::new(&gettext("Intensity"), 0.0, 100.0, 100.0, 0, None);
            s.adj.set_value(l.intensity as f64 * 100.0);
            let snd = sender.clone();
            s.connect(move |v| snd.input(Msg::LayerIntensity(which, i, v)));
            row.append(&s.root);
        }
        list.append(&row);
    }
}
