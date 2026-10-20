//! VibingLUT GTK4/libadwaita front-end over `vibinglut-core`.

mod widgets;

use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

use relm4::adw::prelude::*;
use relm4::gtk::{gdk, gio, glib};
use relm4::{ComponentParts, ComponentSender, RelmApp, SimpleComponent, adw, gtk};
use vibinglut_core::gpu::{self, Done, Job, View};
use vibinglut_core::grade::{Curve, Grade, Params};
use vibinglut_core::lut::Lut3d;
use vibinglut_core::photo::{Photo, RAW_EXTS};
use vibinglut_core::scopes::{HIST_H, HIST_W, Scopes, VEC_SIZE};
use widgets::{ColorWheel, CurveEditor, Slider};

const APP_ID: &str = "io.github.vibinglut.VibingLUT";

type Field = fn(&mut Params) -> &mut f32;

#[derive(Debug)]
enum Msg {
    Open,
    OpenPath(PathBuf),
    Loaded(u64, PathBuf, Result<Photo, String>),
    Set(Field, f64),
    Wheel(usize, f32, f32),
    WheelLuma(usize, f64),
    Band(usize),
    Hue(usize, f64),
    Curve(usize, Curve),
    ResetAll,
    Compare(bool),
    Split(f32),
    Export(usize),
    ExportTo(usize, PathBuf),
    ChooseFolder,
    Folder(PathBuf),
    GalleryLoaded(Vec<Arc<Lut3d>>, Vec<String>),
    SelectLut(usize),
    Gpu(Done),
}

struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    grade_stack: gtk::Stack,
    grade_pic: gtk::Picture,
    info: gtk::Label,
    scopes: [gtk::Picture; 3],
    sliders: Vec<Slider>,
    wheels: Vec<(ColorWheel, Slider)>,
    hue: [Slider; 3],
    curves: CurveEditor,
    gallery_stack: gtk::Stack,
    compare_stack: gtk::Stack,
    compare_pic: gtk::Picture,
    flow: gtk::FlowBox,
    folder_label: gtk::Label,
    thumbs: Vec<gtk::Picture>,
    export: adw::SplitButton,
}

struct App {
    grade: Grade,
    band: usize,
    compare: bool,
    split: f32,
    path: Option<PathBuf>,
    luts: Vec<Arc<Lut3d>>,
    selected: Option<usize>,
    jobs: mpsc::Sender<Job>,
    /// Last preview request; identical requests (e.g. echoes of programmatic
    /// widget resets) are not re-rendered.
    sent: Option<(Grade, f32)>,
    /// Latest photo load / gallery pass; results from older ones are dropped.
    load_gen: u64,
    gallery_gen: u64,
    ui: Ui,
}

impl App {
    fn split_value(&self) -> f32 {
        if self.compare { self.split } else { 0.0 }
    }

    fn send(&self, job: Job) {
        if self.jobs.send(job).is_err() {
            self.toast("GPU non disponibile");
        }
    }

    fn render(&mut self) {
        let req = Some((self.grade.clone(), self.split_value()));
        if self.path.is_some() && self.sent != req {
            self.send(Job::Render(self.grade.clone(), self.split_value()));
            self.sent = req;
        }
    }

    fn render_compare(&self) {
        if let (Some(i), Some(_)) = (self.selected, &self.path) {
            self.send(Job::ApplyLut(self.luts[i].clone(), self.split_value()));
        }
    }

    fn render_gallery(&mut self) {
        if self.path.is_some() && !self.luts.is_empty() {
            self.gallery_gen += 1;
            self.send(Job::Gallery { generation: self.gallery_gen, luts: self.luts.clone() });
        }
    }

    fn toast(&self, text: &str) {
        self.ui.toasts.add_toast(adw::Toast::builder().title(text).timeout(4).build());
    }
}

fn texture(width: u32, height: u32, rgba: Vec<u8>) -> gdk::MemoryTexture {
    let bytes = glib::Bytes::from_owned(rgba);
    gdk::MemoryTexture::new(width as i32, height as i32, gdk::MemoryFormat::R8g8b8a8, &bytes, width as usize * 4)
}

/// Card with a collapsible header, like the sections of a grading panel.
fn section(title: &str, child: &impl IsA<gtk::Widget>) -> gtk::Box {
    let arrow = gtk::Image::from_icon_name("pan-down-symbolic");
    let head = gtk::Box::builder().spacing(6).build();
    head.append(&gtk::Label::builder().label(title).css_classes(["heading"]).xalign(0.0).hexpand(true).build());
    head.append(&arrow);
    let button = gtk::Button::builder().child(&head).css_classes(["flat", "section-header"]).build();
    button.update_property(&[gtk::accessible::Property::Label(title)]);
    let revealer = gtk::Revealer::builder()
        .child(child)
        .reveal_child(true)
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .build();
    let r = revealer.clone();
    button.connect_clicked(move |_| {
        let open = !r.reveals_child();
        r.set_reveal_child(open);
        arrow.set_icon_name(Some(if open { "pan-down-symbolic" } else { "pan-end-symbolic" }));
    });
    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["card", "section"]).build();
    card.append(&button);
    card.append(&revealer);
    card
}

fn vbox(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Vertical, spacing)
}

/// Segmented control (libadwaita ≥ 1.7) returning the group.
fn toggles(names: &[(&str, &str)]) -> adw::ToggleGroup {
    let group = adw::ToggleGroup::builder().css_classes(["round"]).homogeneous(true).build();
    for (name, label) in names {
        group.add(adw::Toggle::builder().name(*name).label(*label).build());
    }
    group
}

fn file_filters(name: &str, patterns: &[String]) -> gio::ListStore {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(name));
    for p in patterns {
        filter.add_suffix(p);
    }
    let store = gio::ListStore::new::<gtk::FileFilter>();
    store.append(&filter);
    store
}

/// Fraction of the displayed image under widget x (Picture uses `contain`).
fn image_fraction(pic: &gtk::Picture, x: f64) -> f32 {
    let Some(p) = pic.paintable() else { return 0.5 };
    let (w, h) = (pic.width() as f64, pic.height() as f64);
    let aspect = p.intrinsic_aspect_ratio();
    let shown = if aspect > 0.0 { (h * aspect).min(w) } else { w };
    (((x - (w - shown) / 2.0) / shown).clamp(0.001, 0.999)) as f32
}

fn add_split_drag(pic: &gtk::Picture, sender: &ComponentSender<App>) {
    let drag = gtk::GestureDrag::new();
    let (p, s) = (pic.clone(), sender.clone());
    drag.connect_drag_begin(move |_, x, _| s.input(Msg::Split(image_fraction(&p, x))));
    let (p, s) = (pic.clone(), sender.clone());
    drag.connect_drag_update(move |g, dx, _| {
        if let Some((x, _)) = g.start_point() {
            s.input(Msg::Split(image_fraction(&p, x + dx)));
        }
    });
    pic.add_controller(drag);
}

const CSS: &str = r#"
.canvas { background-color: color-mix(in srgb, var(--view-bg-color) 70%, black); }
.canvas picture { margin: 24px; }
.canvas.bg-black { background-color: #000; }
.canvas.bg-white { background-color: #fff; }
.sidebar-pane { background-color: var(--sidebar-bg-color); }
.section { padding: 4px 8px 10px 8px; }
.section-header { padding: 6px 6px; margin-bottom: 2px; border-radius: 8px; }
.slider-row { margin: 2px 4px 0 4px; }
.slider-label { font-size: 0.92em; }
.slider-value { font-size: 0.9em; opacity: 0.8; }
.slider-reset { min-width: 24px; min-height: 24px; padding: 0; }
scale.track-temp trough { background-image: linear-gradient(to right, #4f86e8, #e8e8e8, #f2b84b); }
scale.track-tint trough { background-image: linear-gradient(to right, #4cbf5a, #e8e8e8, #d85bd0); }
scale.track-hue trough { background-image: linear-gradient(to right, #e44, #ea4, #ee4, #4e4, #4ee, #44e, #e4e, #e44); }
scale.track-temp trough highlight, scale.track-tint trough highlight, scale.track-hue trough highlight { background: none; }
.scope { background-color: rgba(0,0,0,0.55); border-radius: 8px; }
.band { min-width: 22px; min-height: 22px; padding: 0; border-radius: 99px; }
.band:checked { box-shadow: 0 0 0 2px var(--accent-color); }
.thumb-card { padding: 6px; border-radius: 12px; }
.thumb-card picture { border-radius: 8px; }
flowboxchild:selected .thumb-card { background-color: color-mix(in srgb, var(--accent-bg-color) 30%, transparent); }
.osd-info { padding: 4px 10px; border-radius: 99px; font-size: 0.9em; }
"#;

const BANDS: [(&str, &str); 8] = [
    ("Rossi", "#e5484d"),
    ("Arancioni", "#f08c3a"),
    ("Gialli", "#e8c93a"),
    ("Verdi", "#46b35a"),
    ("Teal", "#2fb5b0"),
    ("Blu", "#3f7ae0"),
    ("Viola", "#8a5ad8"),
    ("Magenta", "#d655b4"),
];

struct Sidebar {
    scroll: gtk::ScrolledWindow,
    sliders: Vec<Slider>,
    wheels: Vec<(ColorWheel, Slider)>,
    hue: [Slider; 3],
    curves: CurveEditor,
    scopes: [gtk::Picture; 3],
}

fn build_sidebar(sender: &ComponentSender<App>) -> Sidebar {
    let panel = vbox(12);
    panel.set_margin_top(12);
    panel.set_margin_bottom(18);
    panel.set_margin_start(12);
    panel.set_margin_end(12);

    // Scopes
    let scope_pics: [gtk::Picture; 3] = std::array::from_fn(|_| {
        gtk::Picture::builder().content_fit(gtk::ContentFit::Fill).height_request(140).css_classes(["scope"]).build()
    });
    scope_pics[2].set_content_fit(gtk::ContentFit::Contain);
    let scope_stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
    for (pic, name) in scope_pics.iter().zip(["hist", "wave", "vec"]) {
        scope_stack.add_named(pic, Some(name));
    }
    let scope_sel = toggles(&[("hist", "Istogramma"), ("wave", "Waveform"), ("vec", "Vettorscopio")]);
    let st = scope_stack.clone();
    scope_sel.connect_active_name_notify(move |g| {
        if let Some(n) = g.active_name() {
            st.set_visible_child_name(&n);
        }
    });
    let scopes_box = vbox(8);
    scopes_box.append(&scope_sel);
    scopes_box.append(&scope_stack);
    panel.append(&section("Scope", &scopes_box));

    let mut sliders = Vec::new();
    let mut add = |bx: &gtk::Box, label: &str, min: f64, max: f64, digits: u32, track: Option<&str>, field: Field| {
        let s = Slider::new(label, min, max, 0.0, digits, track);
        let snd = sender.clone();
        s.connect(move |v| snd.input(Msg::Set(field, v)));
        bx.append(&s.root);
        sliders.push(s);
    };

    let wb = vbox(4);
    add(&wb, "Temperatura", -100.0, 100.0, 0, Some("track-temp"), |p| &mut p.temperature);
    add(&wb, "Tinta", -100.0, 100.0, 0, Some("track-tint"), |p| &mut p.tint);
    panel.append(&section("Bilanciamento del bianco", &wb));

    let tone = vbox(4);
    add(&tone, "Esposizione", -3.0, 3.0, 2, None, |p| &mut p.exposure);
    add(&tone, "Contrasto", -100.0, 100.0, 0, None, |p| &mut p.contrast);
    add(&tone, "Alte luci", -100.0, 100.0, 0, None, |p| &mut p.highlights);
    add(&tone, "Ombre", -100.0, 100.0, 0, None, |p| &mut p.shadows);
    add(&tone, "Bianchi", -100.0, 100.0, 0, None, |p| &mut p.whites);
    add(&tone, "Neri", -100.0, 100.0, 0, None, |p| &mut p.blacks);
    panel.append(&section("Tono", &tone));

    let color = vbox(4);
    add(&color, "Saturazione", -100.0, 100.0, 0, None, |p| &mut p.saturation);
    add(&color, "Vividezza", -100.0, 100.0, 0, None, |p| &mut p.vibrance);
    add(&color, "Saturazione ombre", -100.0, 100.0, 0, None, |p| &mut p.sat_shadows);
    add(&color, "Saturazione alte luci", -100.0, 100.0, 0, None, |p| &mut p.sat_highlights);
    panel.append(&section("Colore", &color));

    // Primaries: lift / gamma / gain / offset wheels with a master slider each.
    let wheels_row = gtk::Grid::builder().column_spacing(10).row_spacing(10).column_homogeneous(true).build();
    let mut wheels = Vec::new();
    for (i, name) in ["Lift", "Gamma", "Gain", "Offset"].into_iter().enumerate() {
        let col = vbox(4);
        col.append(&gtk::Label::builder().label(name).css_classes(["caption-heading"]).build());
        let snd = sender.clone();
        let wheel = ColorWheel::new(name, move |x, y| snd.input(Msg::Wheel(i, x, y)));
        col.append(&wheel.area);
        let luma = Slider::new("Luma", -100.0, 100.0, 0.0, 0, None);
        let snd = sender.clone();
        luma.connect(move |v| snd.input(Msg::WheelLuma(i, v)));
        col.append(&luma.root);
        wheels_row.attach(&col, (i % 2) as i32, (i / 2) as i32, 1, 1);
        wheels.push((wheel, luma));
    }
    panel.append(&section("Ruote colore primarie", &wheels_row));

    // HSL mixer (OKLCh bands)
    let hsl = vbox(6);
    let band_name = gtk::Label::builder().label(BANDS[0].0).css_classes(["caption-heading"]).build();
    let dots = gtk::Box::builder().spacing(6).halign(gtk::Align::Center).build();
    let provider = gtk::CssProvider::new();
    let mut dot_css = String::new();
    let mut first: Option<gtk::ToggleButton> = None;
    for (i, (name, hex)) in BANDS.iter().enumerate() {
        let b = gtk::ToggleButton::builder().tooltip_text(*name).css_classes(["band", &format!("band-{i}")]).build();
        b.update_property(&[gtk::accessible::Property::Label(name)]);
        dot_css.push_str(&format!(".band-{i} {{ background: {hex}; }}\n"));
        match &first {
            None => {
                b.set_active(true);
                first = Some(b.clone());
            }
            Some(f) => b.set_group(Some(f)),
        }
        let (snd, bn) = (sender.clone(), band_name.clone());
        b.connect_toggled(move |b| {
            if b.is_active() {
                bn.set_label(BANDS[i].0);
                snd.input(Msg::Band(i));
            }
        });
        dots.append(&b);
    }
    provider.load_from_string(&dot_css);
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    hsl.append(&dots);
    hsl.append(&band_name);
    let hue: [Slider; 3] = std::array::from_fn(|k| {
        let (label, track) = [("Tinta", Some("track-hue")), ("Saturazione", None), ("Luminanza", None)][k];
        let s = Slider::new(label, -100.0, 100.0, 0.0, 0, track);
        let snd = sender.clone();
        s.connect(move |v| snd.input(Msg::Hue(k, v)));
        hsl.append(&s.root);
        s
    });
    panel.append(&section("Colori selettivi", &hsl));

    // Curves
    let snd = sender.clone();
    let curves = CurveEditor::new(move |ch, c| snd.input(Msg::Curve(ch, c)));
    let curve_sel = toggles(&[("0", "Luma"), ("1", "Rosso"), ("2", "Verde"), ("3", "Blu")]);
    let ce = curves.clone();
    curve_sel.connect_active_name_notify(move |g| {
        if let Some(n) = g.active_name() {
            ce.set_active(n.parse().unwrap_or(0));
        }
    });
    let reset_curve =
        gtk::Button::builder().label("Ripristina curva").css_classes(["flat"]).halign(gtk::Align::End).build();
    let (ce, snd) = (curves.clone(), sender.clone());
    reset_curve.connect_clicked(move |_| {
        let (ch, c) = ce.reset_active();
        snd.input(Msg::Curve(ch, c));
    });
    let curve_box = vbox(8);
    curve_box.append(&curve_sel);
    curve_box.append(&curves.area);
    curve_box.append(
        &gtk::Label::builder()
            .label("Clic per aggiungere un punto, doppio clic o clic destro per rimuoverlo")
            .css_classes(["caption", "dim-label"])
            .wrap(true)
            .build(),
    );
    curve_box.append(&reset_curve);
    panel.append(&section("Curve", &curve_box));

    let reset = gtk::Button::builder()
        .label("Ripristina tutto")
        .css_classes(["pill"])
        .halign(gtk::Align::Center)
        .margin_top(6)
        .build();
    let snd = sender.clone();
    reset.connect_clicked(move |_| snd.input(Msg::ResetAll));
    panel.append(&reset);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&panel)
        .vexpand(true)
        .css_classes(["sidebar-pane"])
        .build();
    Sidebar { scroll, sliders, wheels, hue, curves, scopes: scope_pics }
}

fn empty_state(
    sender: &ComponentSender<App>,
    icon: &str,
    title: &str,
    desc: &str,
    button: &str,
    msg: fn() -> Msg,
) -> adw::StatusPage {
    let b = gtk::Button::builder()
        .label(button)
        .css_classes(["pill", "suggested-action"])
        .halign(gtk::Align::Center)
        .build();
    let s = sender.clone();
    b.connect_clicked(move |_| s.input(msg()));
    adw::StatusPage::builder().icon_name(icon).title(title).description(desc).child(&b).build()
}

impl SimpleComponent for App {
    type Init = Option<PathBuf>;
    type Input = Msg;
    type Output = ();
    type Root = adw::ApplicationWindow;
    type Widgets = ();

    fn init_root() -> Self::Root {
        adw::ApplicationWindow::builder()
            .default_width(1440)
            .default_height(900)
            .width_request(360)
            .height_request(300)
            .title("VibingLUT")
            .build()
    }

    fn init(init: Self::Init, window: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        // Neutral dark surround is the colourist standard.
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::PreferDark);
        let css = gtk::CssProvider::new();
        css.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &gtk::prelude::WidgetExt::display(&window),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let snd = sender.clone();
        let jobs = gpu::spawn(move |d| snd.input(Msg::Gpu(d)));

        // ---------------- grade page
        let grade_pic = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        grade_pic.update_property(&[gtk::accessible::Property::Label("Anteprima del grade")]);
        add_split_drag(&grade_pic, &sender);
        let info = gtk::Label::builder()
            .css_classes(["osd", "osd-info"])
            .halign(gtk::Align::Start)
            .valign(gtk::Align::End)
            .margin_start(16)
            .margin_bottom(16)
            .build();
        let canvas = gtk::Overlay::builder().child(&grade_pic).css_classes(["canvas"]).build();
        canvas.add_overlay(&info);
        let loading = adw::Spinner::builder()
            .width_request(48)
            .height_request(48)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        let grade_stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
        grade_stack.add_named(
            &empty_state(&sender, "image-x-generic-symbolic", "Apri una foto", "Trascina qui un JPEG o un RAW della fotocamera, oppure scegli un file.\nIl grade diventa un LUT .cube per il profilo Standard.", "Apri foto…", || Msg::Open),
            Some("empty"),
        );
        grade_stack.add_named(&loading, Some("loading"));
        grade_stack.add_named(&canvas, Some("image"));

        let Sidebar { scroll: sidebar, sliders, wheels, hue, curves, scopes } = build_sidebar(&sender);
        let grade_split = adw::OverlaySplitView::builder()
            .sidebar_position(gtk::PackType::End)
            .min_sidebar_width(340.0)
            .max_sidebar_width(420.0)
            .sidebar_width_fraction(0.28)
            .content(&grade_stack)
            .sidebar(&sidebar)
            .build();

        // ---------------- compare page
        let flow = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .min_children_per_line(2)
            .max_children_per_line(2)
            .homogeneous(true)
            .valign(gtk::Align::Start)
            .row_spacing(6)
            .column_spacing(6)
            .margin_start(8)
            .margin_end(8)
            .margin_bottom(12)
            .build();
        let snd = sender.clone();
        flow.connect_child_activated(move |_, c| snd.input(Msg::SelectLut(c.index() as usize)));
        let snd = sender.clone();
        flow.connect_selected_children_changed(move |f| {
            if let Some(c) = f.selected_children().first() {
                snd.input(Msg::SelectLut(c.index() as usize));
            }
        });
        let folder_label = gtk::Label::builder()
            .label("Nessuna cartella")
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["heading"])
            .build();
        let folder_btn = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text("Scegli cartella LUT")
            .css_classes(["flat"])
            .build();
        let snd = sender.clone();
        folder_btn.connect_clicked(move |_| snd.input(Msg::ChooseFolder));
        let folder_row =
            gtk::Box::builder().spacing(6).margin_start(12).margin_end(8).margin_top(8).margin_bottom(4).build();
        folder_row.append(&folder_label);
        folder_row.append(&folder_btn);
        let gallery_stack = gtk::Stack::new();
        gallery_stack.add_named(
            &adw::StatusPage::builder()
                .icon_name("view-grid-symbolic")
                .title("Nessun LUT")
                .description("Scegli una cartella con file .cube")
                .css_classes(["compact"])
                .build(),
            Some("empty"),
        );
        gallery_stack.add_named(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&flow)
                .vexpand(true)
                .build(),
            Some("grid"),
        );
        let gallery = vbox(0);
        gallery.add_css_class("sidebar-pane");
        gallery.append(&folder_row);
        gallery.append(&gallery_stack);

        let compare_pic = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        compare_pic.update_property(&[gtk::accessible::Property::Label("Anteprima del LUT selezionato")]);
        add_split_drag(&compare_pic, &sender);
        let compare_canvas = gtk::Overlay::builder().child(&compare_pic).css_classes(["canvas"]).build();
        let compare_stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
        compare_stack.add_named(
            &empty_state(&sender, "view-grid-symbolic", "Confronta LUT", "Scegli una cartella di LUT .cube: ognuno viene applicato alla foto corrente con interpolazione tetraedrica sulla GPU.", "Scegli cartella…", || Msg::ChooseFolder),
            Some("empty"),
        );
        compare_stack.add_named(&compare_canvas, Some("image"));
        let compare_split = adw::OverlaySplitView::builder()
            .sidebar_position(gtk::PackType::Start)
            .min_sidebar_width(300.0)
            .max_sidebar_width(380.0)
            .content(&compare_stack)
            .sidebar(&gallery)
            .build();

        // ---------------- view stack + header
        let stack = adw::ViewStack::new();
        stack.add_titled_with_icon(&grade_split, Some("grade"), "Grade", "color-select-symbolic");
        stack.add_titled_with_icon(&compare_split, Some("compare"), "Confronta LUT", "view-grid-symbolic");

        let header = adw::HeaderBar::new();
        let switcher = adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build();
        header.set_title_widget(Some(&switcher));

        let open =
            gtk::Button::builder().icon_name("document-open-symbolic").tooltip_text("Apri foto (Ctrl+O)").build();
        let snd = sender.clone();
        open.connect_clicked(move |_| snd.input(Msg::Open));
        header.pack_start(&open);

        let menu = gio::Menu::new();
        let bg_menu = gio::Menu::new();
        bg_menu.append(Some("Predefinito"), Some("win.background::default"));
        bg_menu.append(Some("Nero"), Some("win.background::black"));
        bg_menu.append(Some("Bianco"), Some("win.background::white"));
        menu.append_section(Some("Sfondo anteprima"), &bg_menu);
        let section_menu = gio::Menu::new();
        section_menu.append(Some("Scorciatoie da tastiera"), Some("win.show-help-overlay"));
        section_menu.append(Some("Informazioni su VibingLUT"), Some("win.about"));
        menu.append_section(None, &section_menu);
        header.pack_end(
            &gtk::MenuButton::builder()
                .icon_name("open-menu-symbolic")
                .menu_model(&menu)
                .primary(true)
                .tooltip_text("Menu principale")
                .build(),
        );

        let export_menu = gio::Menu::new();
        export_menu.append(Some("Lumix S9 · 33³ .cube"), Some("win.export(33)"));
        export_menu.append(Some("Post-produzione · 65³ .cube"), Some("win.export(65)"));
        export_menu.append(Some("Alta precisione · 129³ .cube"), Some("win.export(129)"));
        let export = adw::SplitButton::builder()
            .label("Esporta LUT")
            .menu_model(&export_menu)
            .css_classes(["suggested-action"])
            .tooltip_text("Esporta .cube 33³ per Lumix (Ctrl+E)")
            .sensitive(false)
            .build();
        let snd = sender.clone();
        export.connect_clicked(move |_| snd.input(Msg::Export(33)));
        header.pack_end(&export);

        let compare_btn =
            gtk::ToggleButton::builder().icon_name("view-dual-symbolic").tooltip_text("Prima / dopo (\\)").build();
        let snd = sender.clone();
        compare_btn.connect_toggled(move |b| snd.input(Msg::Compare(b.is_active())));
        header.pack_end(&compare_btn);

        let sidebar_btn = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-right-symbolic")
            .tooltip_text("Mostra pannello")
            .active(true)
            .visible(false)
            .build();
        grade_split.bind_property("show-sidebar", &sidebar_btn, "active").bidirectional().sync_create().build();
        header.pack_end(&sidebar_btn);

        let switcher_bar = adw::ViewSwitcherBar::builder().stack(&stack).build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&stack));
        toolbar.add_bottom_bar(&switcher_bar);
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&toolbar));
        window.set_content(Some(&toasts));

        // Narrow windows: overlay sidebars, switcher moves to the bottom bar.
        let bp = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 900sp").expect("breakpoint"));
        bp.add_setter(&grade_split, "collapsed", Some(&true.to_value()));
        bp.add_setter(&compare_split, "collapsed", Some(&true.to_value()));
        bp.add_setter(&sidebar_btn, "visible", Some(&true.to_value()));
        bp.add_setter(&header, "title-widget", Some(&None::<gtk::Widget>.to_value()));
        bp.add_setter(&switcher_bar, "reveal", Some(&true.to_value()));
        window.add_breakpoint(bp);

        // ---------------- actions & shortcuts
        let export_action = gio::SimpleAction::new("export", Some(glib::VariantTy::INT32));
        let snd = sender.clone();
        export_action.connect_activate(move |_, v| {
            snd.input(Msg::Export(v.and_then(|v| v.get::<i32>()).unwrap_or(33) as usize))
        });
        window.add_action(&export_action);
        let background =
            gio::SimpleAction::new_stateful("background", Some(glib::VariantTy::STRING), &"default".to_variant());
        let canvases = [canvas.clone(), compare_canvas.clone()];
        background.connect_change_state(move |a, v| {
            let Some(v) = v else { return };
            a.set_state(v);
            for c in &canvases {
                c.remove_css_class("bg-black");
                c.remove_css_class("bg-white");
                if let Some(name) = v.str().filter(|n| *n != "default") {
                    c.add_css_class(&format!("bg-{name}"));
                }
            }
        });
        window.add_action(&background);
        let about = gio::SimpleAction::new("about", None);
        let w = window.clone();
        about.connect_activate(move |_, _| {
            adw::AboutDialog::builder()
                .application_name("VibingLUT")
                .application_icon(APP_ID)
                .version(env!("CARGO_PKG_VERSION"))
                .comments("Color grading da foto e creazione di LUT 3D .cube, accelerato su GPU (Vulkan/Metal).")
                .license_type(gtk::License::Gpl30)
                .developer_name("Nicolò Santamaria")
                .build()
                .present(Some(&w));
        });
        window.add_action(&about);
        let help = gio::SimpleAction::new("show-help-overlay", None);
        let w = window.clone();
        help.connect_activate(move |_, _| shortcuts_dialog().present(Some(&w)));
        window.add_action(&help);

        let shortcuts = gtk::ShortcutController::new();
        shortcuts.set_scope(gtk::ShortcutScope::Managed);
        let shortcut = |accel: &str, f: Box<dyn Fn() + 'static>| {
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(accel),
                Some(gtk::CallbackAction::new(move |_, _| {
                    f();
                    glib::Propagation::Stop
                })),
            ));
        };
        let snd = sender.clone();
        shortcut("<Control>o", Box::new(move || snd.input(Msg::Open)));
        let e = export.clone();
        shortcut("<Control>e", Box::new(move || e.emit_clicked()));
        let c = compare_btn.clone();
        shortcut("backslash", Box::new(move || c.set_active(!c.is_active())));
        let s = stack.clone();
        shortcut("<Alt>1", Box::new(move || s.set_visible_child_name("grade")));
        let s = stack.clone();
        shortcut("<Alt>2", Box::new(move || s.set_visible_child_name("compare")));
        let w = window.clone();
        shortcut("<Control>question", Box::new(move || shortcuts_dialog().present(Some(&w))));
        window.add_controller(shortcuts);

        // Drag & drop a photo anywhere on the window.
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let snd = sender.clone();
        drop.connect_drop(move |_, v, _, _| {
            let Some(f) = v.get::<gdk::FileList>().ok().and_then(|l| l.files().into_iter().next()) else {
                return false;
            };
            let Some(path) = f.path() else { return false };
            snd.input(Msg::OpenPath(path));
            true
        });
        window.add_controller(drop);

        let model = App {
            grade: Grade::default(),
            band: 0,
            compare: false,
            split: 0.5,
            path: None,
            luts: Vec::new(),
            selected: None,
            jobs,
            sent: None,
            load_gen: 0,
            gallery_gen: 0,
            ui: Ui {
                window: window.clone(),
                toasts,
                grade_stack,
                grade_pic,
                info,
                scopes,
                sliders,
                wheels,
                hue,
                curves,
                gallery_stack,
                compare_stack,
                compare_pic,
                flow,
                folder_label,
                thumbs: Vec::new(),
                export,
            },
        };
        if let Some(p) = init {
            sender.input(Msg::OpenPath(p));
        }
        ComponentParts { model, widgets: () }
    }

    fn update(&mut self, msg: Msg, sender: ComponentSender<Self>) {
        match msg {
            Msg::Open => {
                // add_suffix matches case-insensitively.
                let pats: Vec<String> =
                    ["jpg", "jpeg", "png", "tif", "tiff"].iter().chain(RAW_EXTS).map(|s| s.to_string()).collect();
                let dialog = gtk::FileDialog::builder()
                    .title("Apri foto")
                    .filters(&file_filters("Foto e RAW", &pats))
                    .modal(true)
                    .build();
                let s = sender.clone();
                dialog.open(Some(&self.ui.window), gio::Cancellable::NONE, move |r| {
                    if let Some(p) = r.ok().and_then(|f| f.path()) {
                        s.input(Msg::OpenPath(p));
                    }
                });
            }
            Msg::OpenPath(path) => {
                self.ui.grade_stack.set_visible_child_name("loading");
                self.load_gen += 1;
                let (s, generation) = (sender.input_sender().clone(), self.load_gen);
                std::thread::spawn(move || {
                    let r = Photo::load(&path);
                    s.emit(Msg::Loaded(generation, path, r));
                });
            }
            Msg::Loaded(generation, ..) if generation != self.load_gen => {}
            Msg::Loaded(_, path, Ok(photo)) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.ui.window.set_title(Some(&format!("{name} — VibingLUT")));
                self.ui.info.set_label(&format!("{name} · {}×{} anteprima", photo.width, photo.height));
                self.send(Job::SetPhoto(photo));
                self.path = Some(path);
                self.sent = None;
                self.ui.export.set_sensitive(true);
                self.ui.grade_stack.set_visible_child_name("image");
                self.render();
                self.render_gallery();
                self.render_compare();
            }
            Msg::Loaded(_, path, Err(e)) => {
                self.ui.grade_stack.set_visible_child_name(if self.path.is_some() { "image" } else { "empty" });
                self.toast(&format!("Impossibile aprire {}: {e}", path.display()));
            }
            Msg::Set(field, v) => {
                *field(&mut self.grade.params) = v as f32;
                self.render();
            }
            Msg::Wheel(i, x, y) => {
                let w = wheel_mut(&mut self.grade.params, i);
                (w[0], w[1]) = (x, y);
                self.render();
            }
            Msg::WheelLuma(i, v) => {
                wheel_mut(&mut self.grade.params, i)[2] = v as f32;
                self.render();
            }
            Msg::Band(b) => {
                self.band = b;
                // The resulting Hue echoes carry unchanged values, so render() skips them.
                let p = self.grade.params;
                for (s, v) in self.ui.hue.iter().zip([p.hue_shift[b], p.hue_sat[b], p.hue_lum[b]]) {
                    s.adj.set_value(v as f64);
                }
            }
            Msg::Hue(k, v) => {
                let p = &mut self.grade.params;
                let band = match k {
                    0 => &mut p.hue_shift,
                    1 => &mut p.hue_sat,
                    _ => &mut p.hue_lum,
                };
                band[self.band] = v as f32;
                self.render();
            }
            Msg::Curve(ch, c) => {
                self.grade.curves[ch] = c;
                self.render();
            }
            Msg::ResetAll => {
                self.ui.sliders.iter().chain(&self.ui.hue).for_each(Slider::reset);
                for (w, l) in &self.ui.wheels {
                    w.set(0.0, 0.0);
                    l.reset();
                }
                self.ui.curves.reset_all();
                self.grade = Grade::default();
                self.render();
            }
            Msg::Compare(on) => {
                self.compare = on;
                self.render();
                self.render_compare();
            }
            Msg::Split(x) => {
                if self.compare {
                    self.split = x;
                    self.render();
                    self.render_compare();
                }
            }
            Msg::Export(size) => {
                let Some(path) = &self.path else { return };
                let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "grade".into());
                let dialog = gtk::FileDialog::builder()
                    .title(format!("Esporta LUT {size}³"))
                    .initial_name(format!("{stem}_{size}.cube"))
                    .filters(&file_filters("LUT .cube", &["cube".into()]))
                    .modal(true)
                    .build();
                let s = sender.clone();
                dialog.save(Some(&self.ui.window), gio::Cancellable::NONE, move |r| {
                    if let Some(p) = r.ok().and_then(|f| f.path()) {
                        s.input(Msg::ExportTo(size, p));
                    }
                });
            }
            Msg::ExportTo(size, path) => {
                let title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                self.send(Job::Export { grade: self.grade.clone(), size, title, path });
            }
            Msg::ChooseFolder => {
                let dialog = gtk::FileDialog::builder().title("Cartella LUT").modal(true).build();
                let s = sender.clone();
                dialog.select_folder(Some(&self.ui.window), gio::Cancellable::NONE, move |r| {
                    if let Some(p) = r.ok().and_then(|f| f.path()) {
                        s.input(Msg::Folder(p));
                    }
                });
            }
            Msg::Folder(dir) => {
                self.ui
                    .folder_label
                    .set_label(&dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let (luts, errors) = load_folder(&dir);
                    s.emit(Msg::GalleryLoaded(luts, errors));
                });
            }
            Msg::GalleryLoaded(luts, errors) => {
                if !errors.is_empty() {
                    self.toast(&format!("{} file ignorati: {}", errors.len(), errors.join("; ")));
                }
                while let Some(c) = self.ui.flow.first_child() {
                    self.ui.flow.remove(&c);
                }
                self.ui.thumbs.clear();
                for lut in &luts {
                    let pic = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).height_request(96).build();
                    let card = vbox(4);
                    card.add_css_class("thumb-card");
                    card.append(&pic);
                    card.append(
                        &gtk::Label::builder()
                            .label(&lut.title)
                            .ellipsize(gtk::pango::EllipsizeMode::End)
                            .css_classes(["caption"])
                            .build(),
                    );
                    card.set_tooltip_text(Some(&format!("{} · {}³", lut.title, lut.size)));
                    self.ui.flow.append(&card);
                    self.ui.thumbs.push(pic);
                }
                self.ui.gallery_stack.set_visible_child_name(if luts.is_empty() { "empty" } else { "grid" });
                self.luts = luts;
                self.selected = None;
                self.ui.compare_stack.set_visible_child_name("empty");
                self.render_gallery();
                if let Some(c) = self.ui.flow.child_at_index(0) {
                    self.ui.flow.select_child(&c);
                }
            }
            Msg::SelectLut(i) => {
                if i < self.luts.len() && self.selected != Some(i) {
                    self.selected = Some(i);
                    self.render_compare();
                }
            }
            Msg::Gpu(done) => self.on_gpu(done),
        }
    }
}

impl App {
    fn on_gpu(&mut self, done: Done) {
        match done {
            Done::Ready(name) => self.toast(&format!("GPU: {name}")),
            Done::Failed(e) => self.toast(&format!("GPU non disponibile: {e}")),
            Done::Frame { view, width, height, rgba, scopes } => {
                let tex = texture(width, height, rgba);
                match view {
                    View::Grade => self.ui.grade_pic.set_paintable(Some(&tex)),
                    View::Compare => {
                        self.ui.compare_pic.set_paintable(Some(&tex));
                        self.ui.compare_stack.set_visible_child_name("image");
                    }
                }
                if let Some(Scopes { histogram, waveform, vectorscope }) = scopes {
                    let [h, w, v] = &self.ui.scopes;
                    h.set_paintable(Some(&texture(HIST_W as u32, HIST_H as u32, histogram)));
                    w.set_paintable(Some(&texture(HIST_W as u32, HIST_H as u32, waveform)));
                    v.set_paintable(Some(&texture(VEC_SIZE as u32, VEC_SIZE as u32, vectorscope)));
                }
            }
            Done::Thumb { generation, index, width, height, rgba } => {
                if generation != self.gallery_gen {
                    return;
                }
                if let Some(p) = self.ui.thumbs.get(index) {
                    p.set_paintable(Some(&texture(width, height, rgba)));
                }
            }
            Done::Exported(Ok(p)) => {
                self.toast(&format!("LUT salvato: {}", p.file_name().unwrap_or_default().to_string_lossy()))
            }
            Done::Exported(Err(e)) => self.toast(&format!("Esportazione fallita: {e}")),
        }
    }
}

fn wheel_mut(p: &mut Params, i: usize) -> &mut [f32; 4] {
    match i {
        0 => &mut p.lift,
        1 => &mut p.gamma,
        2 => &mut p.gain,
        _ => &mut p.offset,
    }
}

fn load_folder(dir: &Path) -> (Vec<Arc<Lut3d>>, Vec<String>) {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("cube")))
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    let (mut luts, mut errors) = (Vec::new(), Vec::new());
    for p in paths {
        match Lut3d::load(&p) {
            Ok(l) => luts.push(Arc::new(l)),
            Err(e) => errors.push(format!("{}: {e}", p.file_name().unwrap_or_default().to_string_lossy())),
        }
    }
    (luts, errors)
}

fn shortcuts_dialog() -> adw::ShortcutsDialog {
    let dialog = adw::ShortcutsDialog::new();
    let section = adw::ShortcutsSection::new(None);
    for (title, accel) in [
        ("Apri foto", "<Control>o"),
        ("Esporta LUT", "<Control>e"),
        ("Prima / dopo", "backslash"),
        ("Vista Grade", "<Alt>1"),
        ("Vista Confronta LUT", "<Alt>2"),
        ("Scorciatoie da tastiera", "<Control>question"),
    ] {
        section.add(adw::ShortcutsItem::new(title, accel));
    }
    dialog.add(section);
    dialog
}

fn main() {
    let file = std::env::args().nth(1).map(PathBuf::from);
    RelmApp::new(APP_ID).with_args(Vec::new()).run::<App>(file);
}
