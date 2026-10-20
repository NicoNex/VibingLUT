//! LUT Lab page: a stack of LUTs, the current grade and profile conversions,
//! chained or mixed into one LUT (maths in `vibinglut_core::lab`).

use std::sync::Arc;

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

#[derive(Clone, Debug)]
pub struct LabLayer {
    pub name: String,
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

pub struct LabUi {
    pub root: adw::OverlaySplitView,
    pub list: gtk::ListBox,
    pub stack: gtk::Stack,
    pub pic: gtk::Picture,
    pub canvas: gtk::Overlay,
    pub export: adw::SplitButton,
}

fn profile_dropdown(selected: Profile) -> gtk::DropDown {
    let names: Vec<&str> = Profile::ALL.iter().map(|p| p.name()).collect();
    let d = gtk::DropDown::from_strings(&names);
    d.set_selected(Profile::ALL.iter().position(|p| *p == selected).unwrap_or(0) as u32);
    d
}

fn caption(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).css_classes(["caption", "dim-label"]).wrap(true).xalign(0.0).build()
}

pub fn build(sender: &ComponentSender<App>, export_menu: &gio::Menu) -> LabUi {
    let panel = vbox(12);
    panel.set_margin_top(12);
    panel.set_margin_bottom(18);
    panel.set_margin_start(12);
    panel.set_margin_end(12);

    // Result: the camera signal the combined LUT will receive.
    let input = profile_dropdown(Profile::Standard);
    input.update_property(&[gtk::accessible::Property::Label("Profilo di ingresso del LUT finale")]);
    let snd = sender.clone();
    input.connect_selected_notify(move |d| snd.input(Msg::LabInput(Profile::ALL[d.selected() as usize])));
    let result = vbox(6);
    result.add_css_class("card");
    result.add_css_class("section");
    result.append(&gtk::Label::builder().label("Risultato").css_classes(["heading"]).xalign(0.0).margin_top(6).build());
    result.append(&input);
    result.append(&caption(
        "Segnale della camera su cui userai il LUT finale. Ogni livello riceve il segnale convertito nel suo profilo.",
    ));

    let blend = adw::ToggleGroup::builder().css_classes(["round"]).homogeneous(true).build();
    blend.add(adw::Toggle::builder().name("chain").label("Catena").build());
    blend.add(adw::Toggle::builder().name("mix").label("Miscela").build());
    let hint = caption("Catena: i livelli si applicano uno dopo l'altro; l'intensità sfuma ogni passo.");
    let (snd, h) = (sender.clone(), hint.clone());
    blend.connect_active_name_notify(move |g| {
        let mix = g.active_name().as_deref() == Some("mix");
        h.set_label(if mix {
            "Miscela: ogni LUT riceve lo stesso ingresso; le uscite si mediano in OKLab con l'intensità come peso."
        } else {
            "Catena: i livelli si applicano uno dopo l'altro; l'intensità sfuma ogni passo."
        });
        snd.input(Msg::LabBlend(if mix { Blend::Mix } else { Blend::Chain }));
    });
    result.append(&blend);
    result.append(&hint);
    panel.append(&result);

    // Layers
    let add_menu = gio::Menu::new();
    add_menu.append(Some("LUT selezionato in Confronta"), Some("win.lab-add::selected"));
    add_menu.append(Some("LUT da file…"), Some("win.lab-add::file"));
    add_menu.append(Some("Grade corrente"), Some("win.lab-add::grade"));
    add_menu.append(Some("Conversione di profilo"), Some("win.lab-add::convert"));
    let add = gtk::MenuButton::builder()
        .icon_name("list-add-symbolic")
        .menu_model(&add_menu)
        .tooltip_text("Aggiungi livello")
        .css_classes(["flat"])
        .build();
    let head = gtk::Box::builder().spacing(6).build();
    head.append(&gtk::Label::builder().label("Livelli").css_classes(["heading"]).xalign(0.0).hexpand(true).build());
    head.append(&add);
    panel.append(&head);
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    list.set_placeholder(Some(&caption("Nessun livello. Aggiungi un LUT, il grade corrente o una conversione.")));
    panel.append(&list);

    let export = adw::SplitButton::builder()
        .label("Esporta LUT combinato")
        .menu_model(export_menu)
        .css_classes(["suggested-action", "pill"])
        .halign(gtk::Align::Center)
        .margin_top(6)
        .sensitive(false)
        .build();
    let snd = sender.clone();
    export.connect_clicked(move |_| snd.input(Msg::Export(vibinglut_core::lut::Format::Cube, 33, crate::What::Lab)));
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
    pic.update_property(&[gtk::accessible::Property::Label("Anteprima del LUT combinato")]);
    crate::add_split_drag(&pic, sender);
    let canvas = gtk::Overlay::builder().child(&pic).css_classes(["canvas"]).build();
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
    stack.add_named(
        &adw::StatusPage::builder()
            .icon_name("view-list-symbolic")
            .title("LUT Lab")
            .description(
                "Combina LUT, grade e conversioni di profilo: in catena o in miscela, ognuno con la sua intensità.\n\
                 Es. un LUT Fujifilm per F-Log diventa un LUT per V-Log o per il profilo Standard.",
            )
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
    LabUi { root, list, stack, pic, canvas, export }
}

impl LabUi {
    /// Recreates the layer rows (structural changes are rare; sliders update in place).
    pub fn rebuild(&self, layers: &[LabLayer], source_profile: Profile, sender: &ComponentSender<App>) {
        self.list.remove_all();
        let last = layers.len().saturating_sub(1);
        for (i, l) in layers.iter().enumerate() {
            let row = vbox(6);
            row.set_margin_top(8);
            row.set_margin_bottom(10);
            row.set_margin_start(10);
            row.set_margin_end(6);

            let (kind, title) = match &l.kind {
                LabKind::Lut(lut) => (lut.format.badge(), l.name.as_str()),
                LabKind::Grade => ("GRADE", "Grade corrente"),
                LabKind::Convert(_) => ("CONV", "Conversione"),
            };
            let head = gtk::Box::builder().spacing(4).build();
            head.append(
                &gtk::Label::builder().label(kind).css_classes(["lut-badge"]).valign(gtk::Align::Center).build(),
            );
            head.append(
                &gtk::Label::builder()
                    .label(title)
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .xalign(0.0)
                    .hexpand(true)
                    .css_classes(["heading"])
                    .tooltip_text(&l.name)
                    .build(),
            );
            for (icon, tip, enabled, msg) in [
                ("go-up-symbolic", "Sposta su", i > 0, Msg::LabMove(i, true)),
                ("go-down-symbolic", "Sposta giù", i < last, Msg::LabMove(i, false)),
                ("user-trash-symbolic", "Rimuovi", true, Msg::LabRemove(i)),
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

            match &l.kind {
                LabKind::Lut(_) => {
                    let d = profile_dropdown(l.input);
                    d.set_tooltip_text(Some("Profilo per cui è stato creato il LUT"));
                    let snd = sender.clone();
                    d.connect_selected_notify(move |d| {
                        snd.input(Msg::LabProfile(i, Profile::ALL[d.selected() as usize]));
                    });
                    row.append(&caption("Ingresso del LUT"));
                    row.append(&d);
                }
                LabKind::Grade => row.append(&caption(&format!("Ingresso: {}", source_profile.name()))),
                LabKind::Convert(p) => {
                    let d = profile_dropdown(*p);
                    let snd = sender.clone();
                    d.connect_selected_notify(move |d| {
                        snd.input(Msg::LabProfile(i, Profile::ALL[d.selected() as usize]));
                    });
                    row.append(&caption("Converti il segnale in"));
                    row.append(&d);
                }
            }
            if !matches!(l.kind, LabKind::Convert(_)) {
                let s = Slider::new("Intensità", 0.0, 100.0, 100.0, 0, None);
                s.adj.set_value(l.intensity as f64 * 100.0);
                let snd = sender.clone();
                s.connect(move |v| snd.input(Msg::LabIntensity(i, v)));
                row.append(&s.root);
            }
            self.list.append(&row);
        }
        self.export.set_sensitive(!layers.is_empty());
        self.stack.set_visible_child_name(if layers.is_empty() { "empty" } else { "image" });
    }
}
