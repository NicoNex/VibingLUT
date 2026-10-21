//! VibingLUT GTK4/libadwaita front-end over `vibinglut-core`.

/// `gettext` with `{}` placeholders filled in order (translations may not reorder them).
macro_rules! gettext {
    ($msgid:literal, $($arg:expr),+ $(,)?) => {
        $crate::fill(&gettextrs::gettext($msgid), &[$($arg.to_string()),+])
    };
}

mod lab_page;
mod settings;
mod widgets;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use gettextrs::gettext;
use lab_page::{LabAdd, LabKind, LabLayer, LabUi, Layers};
use relm4::adw::prelude::*;
use relm4::gtk::{gdk, gio, glib};
use relm4::{ComponentParts, ComponentSender, RelmApp, SimpleComponent, adw, gtk};
use vibinglut_core::chart::test_chart;
use vibinglut_core::gpu::{self, CompareBase, Done, Job};
use vibinglut_core::grade::{BAND_HUE_RANGE, BAND_HUES, Curve, Grade, Params, Stage};
use vibinglut_core::lab::{Blend, Layer, Source, Stack, oklch_to_srgb};
use vibinglut_core::lut::{Format, Lut3d, LutEntry, MAX_LIBRARY};
use vibinglut_core::photo::{Decoded, Photo, RASTER_EXTS, RAW_EXTS};
use vibinglut_core::profiles::Profile;
use vibinglut_core::scopes::{HIST_H, HIST_W, Scopes, VEC_SIZE};
use widgets::{ColorWheel, CurveEditor, Slider};

const APP_ID: &str = "io.github.vibinglut.VibingLUT";

type Field = fn(&mut Params) -> &mut f32;

/// What an export writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    Grade,
    /// The neutral LUT (e.g. a HALD to process in another editor).
    Identity,
    Lab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Grade,
    Compare,
    Lab,
}

#[derive(Debug)]
enum Msg {
    Open,
    OpenPath(PathBuf),
    Loaded(u64, PathBuf, Result<Decoded, String>),
    /// A RAW re-encoded for a profile: (load generation, profile, pixels).
    Encoded(u64, Profile, Arc<Photo>),
    Set(Field, f64),
    Wheel(usize, f32, f32),
    WheelLuma(usize, f64),
    Band(usize),
    Hue(usize, f64),
    Curve(usize, Curve),
    ResetAll,
    Compare(bool),
    Split(f32),
    Export(Format, usize, What),
    ExportTo(Format, usize, What, PathBuf),
    ChooseFolder,
    Folder(PathBuf),
    GalleryLoaded(Vec<LutEntry>, Vec<String>, bool),
    Group(u32),
    Search(String),
    /// Rescans the LUT folder.
    Reload,
    /// A sidebar section switched off/on (`None` = the LUTs after the grade).
    Bypass(Option<Stage>, bool),
    /// Compare LUTs on the untouched photo instead of on top of the grade.
    CompareOriginal(bool),
    Preferences,
    Page(Page),
    LayerAdd(Layers, LabAdd),
    OpenInLab,
    LayerLoaded(Layers, PathBuf, Result<Lut3d, String>),
    LayerRemove(Layers, usize),
    LayerMove(Layers, usize, bool),
    LayerIntensity(Layers, usize, f64),
    LayerProfile(Layers, usize, Profile),
    LabInput(Profile),
    LabBlend(Blend),
    Chart(bool),
    ChartReady(Profile, Arc<Photo>),
    Profile(Profile),
    SelectLut(usize),
    Gpu(Done),
}

struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    grade_stack: gtk::Stack,
    grade_pic: gtk::Picture,
    info: gtk::Label,
    sidebar: Sidebar,
    gallery_stack: gtk::Stack,
    compare_stack: gtk::Stack,
    compare_pic: gtk::Picture,
    flow: gtk::FlowBox,
    folder_label: gtk::Label,
    groups: gtk::DropDown,
    prefs: adw::PreferencesDialog,
    prefs_row: adw::ActionRow,
    lab: LabUi,
    stack: adw::ViewStack,
    thumbs: Vec<gtk::Picture>,
    export: adw::SplitButton,
    chart_btn: gtk::ToggleButton,
}

struct App {
    grade: Grade,
    band: usize,
    compare: bool,
    split: f32,
    path: Option<PathBuf>,
    luts: Vec<LutEntry>,
    lut_paths: Arc<Vec<PathBuf>>,
    /// Sub-folder of each LUT, and the distinct ones offered by the filter
    /// (filter index 0 = all, i = group_names[i - 1]).
    group_names: Vec<String>,
    /// Gallery filters: sub-folder (None = all) and lower-cased search text.
    group: Option<String>,
    query: String,
    page: Page,
    lab_layers: Vec<LabLayer>,
    /// LUTs applied after the grade on the Grade page, baked into its export.
    looks: Vec<LabLayer>,
    /// Sections switched off with their eye toggle; they are left out of preview and export.
    bypassed: Vec<Stage>,
    looks_off: bool,
    compare_original: bool,
    /// What the Compare base was last built from: grade, looks (by identity), profile.
    base_key: Option<BaseKey>,
    /// Thumbnails need re-rendering (new source or library) next time Compare is shown.
    gallery_stale: bool,
    lab_input: Profile,
    lab_blend: Blend,
    profile: Profile,
    /// The open file, decoded once; `photo` is it encoded for a profile.
    decoded: Option<Decoded>,
    /// Shown source and the profile its pixels are encoded in; the test
    /// chart can be swapped in and out.
    photo: Option<(Arc<Photo>, Profile)>,
    chart: Option<(Arc<Photo>, Profile)>,
    show_chart: bool,
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
            self.toast(&gettext("GPU unavailable"));
        }
    }

    fn active(&self) -> Option<&(Arc<Photo>, Profile)> {
        if self.show_chart { self.chart.as_ref() } else { self.photo.as_ref() }
    }

    fn has_source(&self) -> bool {
        self.active().is_some()
    }

    /// Profile of the pixels currently shown (photo or chart).
    fn source_profile(&self) -> Profile {
        self.active().map_or(Profile::Standard, |s| s.1)
    }

    /// Sends the active source (photo or test chart) to the GPU and redraws.
    fn show_source(&mut self) {
        let Some((img, profile)) = self.active().cloned() else {
            // Nothing to show (e.g. chart turned off with no photo open).
            self.ui.export.set_sensitive(false);
            self.ui.info.set_label("");
            self.ui.grade_stack.set_visible_child_name("empty");
            return;
        };
        // The shader decodes the profile the pixels were actually encoded in.
        self.grade.params.set_input(profile);
        let name = match (&self.path, self.show_chart) {
            (Some(p), false) => p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            _ => gettext("Test chart"),
        };
        self.ui.info.set_label(&format!("{name} · {} · {}×{}", profile.name(), img.width, img.height));
        self.send(Job::SetPhoto(img));
        self.sent = None;
        self.ui.export.set_sensitive(true);
        self.ui.grade_stack.set_visible_child_name("image");
        self.gallery_stale = true;
        self.refresh();
    }

    /// The grade as previewed and exported: switched-off sections are neutral.
    fn graded(&self) -> Grade {
        let mut g = self.grade.clone();
        self.bypassed.iter().for_each(|&s| g.bypass(s));
        g
    }

    fn render(&mut self) {
        let req = Some((self.graded(), self.split_value()));
        if self.has_source() && self.sent != req {
            self.sent = req;
            let (grade, split) = self.sent.clone().expect("set above");
            self.send(Job::Render(grade, split));
        }
    }

    fn render_compare(&self) {
        if let (Some(i), true) = (self.selected, self.has_source()) {
            self.send(Job::ApplyLut(self.luts[i].path.clone(), self.split_value()));
        }
    }

    fn filter_gallery(&self) {
        for (i, l) in self.luts.iter().enumerate() {
            if let Some(c) = self.ui.flow.child_at_index(i as i32) {
                let in_group = self.group.as_ref().is_none_or(|g| *g == l.group);
                let found = self.query.is_empty()
                    || l.title.to_lowercase().contains(&self.query)
                    || l.group.to_lowercase().contains(&self.query);
                c.set_visible(in_group && found);
            }
        }
    }

    /// Hands the Grade page's result to Compare LUTs; true when it changed.
    fn sync_compare_base(&mut self) -> bool {
        let input = self.source_profile();
        let (grade, looks) = if self.compare_original {
            (self.grade.neutralized(), Vec::new())
        } else {
            (self.graded(), self.look_layers())
        };
        let luts = looks
            .iter()
            .filter_map(|l| match &l.source {
                Source::Lut(lut) => Some((lut.clone(), l.intensity, l.input)),
                _ => None,
            })
            .collect();
        let key = Some(BaseKey { grade: grade.clone(), luts, input });
        if self.base_key == key {
            return false;
        }
        self.base_key = key;
        // ponytail: an untouched grade keeps the exact direct path (no lattice round trip).
        let base = (!grade.is_neutral() || !looks.is_empty()).then_some(CompareBase { grade, looks, input });
        self.send(Job::SetCompareBase(base));
        true
    }

    fn render_gallery(&mut self) {
        if self.has_source() && !self.luts.is_empty() {
            self.gallery_stale = false;
            self.gallery_gen += 1;
            self.send(Job::Gallery(self.gallery_gen, self.lut_paths.clone()));
        }
    }

    /// Re-renders the view on screen after a split change: nothing else moved.
    fn redraw(&mut self) {
        match self.page {
            Page::Grade => self.render(),
            Page::Compare => self.render_compare(),
            Page::Lab => self.render_lab(),
        }
    }

    /// Renders only the view on screen; the others refresh when shown.
    fn refresh(&mut self) {
        match self.page {
            Page::Grade => self.render(),
            Page::Compare => {
                // Thumbnails are rendered only for this page, when their inputs changed.
                if self.sync_compare_base() || self.gallery_stale {
                    self.render_gallery();
                }
                self.render_compare()
            }
            Page::Lab => self.render_lab(),
        }
    }

    fn lab_job_layers(&self) -> Vec<Layer> {
        self.lab_layers.iter().map(|l| self.job_layer(l)).collect()
    }

    fn job_layer(&self, l: &LabLayer) -> Layer {
        let (source, input) = match &l.kind {
            LabKind::Lut(lut) => (Source::Lut(lut.clone()), l.input),
            LabKind::Grade => (Source::Grade(Box::new(self.graded())), self.source_profile()),
            LabKind::Convert(p) => (Source::Convert(*p), *p),
        };
        Layer { source, intensity: l.intensity, input }
    }

    fn lab_stack(&self, input: Profile) -> Stack {
        Stack { input, layers: self.lab_job_layers(), blend: self.lab_blend }
    }

    fn render_lab(&self) {
        if self.has_source() && !self.lab_layers.is_empty() {
            // The preview shows the stack on the current source, so its input is the source's profile.
            self.send(Job::Lab { stack: self.lab_stack(self.source_profile()), split: self.split_value() });
        }
    }

    /// Expresses the open file in the current profile: rasters are already
    /// a signal, RAWs are re-encoded (off the main thread) without re-decoding.
    fn encode_photo(&mut self, sender: &ComponentSender<Self>) {
        match &self.decoded {
            Some(Decoded::Raster(p)) => {
                self.photo = Some((p.clone(), self.profile));
                self.show_source();
            }
            Some(raw @ Decoded::Raw(_)) => {
                let (raw, s, generation, profile) =
                    (raw.clone(), sender.input_sender().clone(), self.load_gen, self.profile);
                std::thread::spawn(move || s.emit(Msg::Encoded(generation, profile, raw.encode(profile))));
            }
            None => {}
        }
    }

    fn layers_mut(&mut self, which: Layers) -> &mut Vec<LabLayer> {
        match which {
            Layers::Lab => &mut self.lab_layers,
            Layers::Look => &mut self.looks,
        }
    }

    fn push_layer(&mut self, which: Layers, kind: LabKind, input: Profile, sender: &ComponentSender<Self>) {
        self.layers_mut(which).push(LabLayer { kind, intensity: 1.0, input });
        self.layers_changed(which, true, sender);
    }

    fn push_lut(&mut self, which: Layers, lut: Arc<Lut3d>, sender: &ComponentSender<Self>) {
        // Vendors name the input in the file (e.g. "…_FLog_…"): preselect it.
        let input = Profile::guess_from_name(&lut.title);
        self.push_layer(which, LabKind::Lut(lut), input, sender);
    }

    fn layers_changed(&mut self, which: Layers, structure: bool, sender: &ComponentSender<Self>) {
        match which {
            Layers::Lab => {
                if !structure {
                    self.ui.lab.refresh_summary(&self.lab_layers, self.lab_blend, self.lab_input);
                }
                self.lab_changed(structure, sender);
            }
            Layers::Look => {
                if structure {
                    lab_page::layer_rows(
                        &self.ui.sidebar.looks,
                        &self.looks,
                        Layers::Look,
                        self.source_profile(),
                        sender,
                    );
                }
                self.send(Job::SetLook(self.look_layers()));
                self.sent = None;
                self.render();
            }
        }
    }

    fn look_layers(&self) -> Vec<Layer> {
        if self.looks_off {
            return Vec::new();
        }
        self.looks.iter().map(|l| self.job_layer(l)).collect()
    }

    fn lab_changed(&mut self, structure: bool, sender: &ComponentSender<Self>) {
        if structure {
            self.ui.lab.rebuild(&self.lab_layers, self.lab_blend, self.lab_input, self.source_profile(), sender);
        }
        self.render_lab();
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
    section_with(title, child, None)
}

/// Section with an eye toggle that switches it off (`which` = None: the LUTs after the grade).
fn bypassable(
    title: &str,
    child: &impl IsA<gtk::Widget>,
    which: Option<Stage>,
    sender: &ComponentSender<App>,
) -> gtk::Box {
    let eye = gtk::ToggleButton::builder()
        .icon_name("view-reveal-symbolic")
        .tooltip_text(gettext("Disable section"))
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular"])
        .build();
    let (snd, content) = (sender.clone(), child.clone().upcast::<gtk::Widget>());
    eye.connect_toggled(move |b| {
        let off = b.is_active();
        b.set_icon_name(if off { "view-conceal-symbolic" } else { "view-reveal-symbolic" });
        b.set_tooltip_text(Some(&if off { gettext("Enable section") } else { gettext("Disable section") }));
        if off {
            content.add_css_class("bypassed")
        } else {
            content.remove_css_class("bypassed")
        }
        snd.input(Msg::Bypass(which, off));
    });
    section_with(title, child, Some(&eye))
}

fn section_with(title: &str, child: &impl IsA<gtk::Widget>, eye: Option<&gtk::ToggleButton>) -> gtk::Box {
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
    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["card", "section"]).build();
    let (r, c) = (revealer.clone(), card.clone());
    button.connect_clicked(move |_| {
        let open = !r.reveals_child();
        r.set_reveal_child(open);
        // Symmetric padding once the body is gone, so the header sits centred.
        if open {
            c.remove_css_class("collapsed")
        } else {
            c.add_css_class("collapsed")
        }
        arrow.set_icon_name(Some(if open { "pan-down-symbolic" } else { "pan-end-symbolic" }));
    });
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    button.set_hexpand(true);
    top.append(&button);
    if let Some(eye) = eye {
        eye.set_margin_end(6);
        top.append(eye);
    }
    card.append(&top);
    card.append(&revealer);
    card
}

/// Where a file dialog opens: the remembered folder for `key`, if it still exists.
fn start_in(key: &str) -> Option<gio::File> {
    settings::folder(key).map(gio::File::for_path)
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
.section.collapsed { padding-bottom: 4px; }
.section-header { padding: 6px 6px; margin-bottom: 2px; border-radius: 8px; }
.slider-row { margin: 2px 4px 0 4px; }
.slider-value { opacity: 0.8; }
.slider-reset { min-width: 24px; min-height: 24px; padding: 0; }
scale.track-temp trough { background-image: linear-gradient(to right, #4f86e8, #e8e8e8, #f2b84b); }
scale.track-tint trough { background-image: linear-gradient(to right, #4cbf5a, #e8e8e8, #d85bd0); }
scale.track-temp trough highlight, scale.track-tint trough highlight { background: none; }
.scope { background-color: rgba(0,0,0,0.55); border-radius: 8px; }
.band { min-width: 22px; min-height: 22px; padding: 0; border-radius: 99px; }
.band:checked { box-shadow: 0 0 0 2px var(--accent-color); }
.bypassed { opacity: 0.45; }
flowboxchild { padding: 0; border-radius: 12px; }
flowboxchild:selected { background: none; }
.thumb-card { padding: 6px; border-radius: 12px; }
.thumb-card picture { border-radius: 8px; }
flowboxchild:selected .thumb-card { background-color: color-mix(in srgb, var(--accent-bg-color) 35%, transparent); }
.lut-badge { margin: 5px; padding: 1px 6px; border-radius: 6px; font-size: 0.68em; font-weight: 800;
  letter-spacing: 0.04em; color: rgba(255,255,255,0.92); background-color: rgba(0,0,0,0.45); }
.lab-summary { padding: 8px 10px; border-radius: 8px; background-color: color-mix(in srgb, var(--accent-bg-color) 14%, transparent); }
.osd-info { padding: 4px 10px; border-radius: 99px; font-size: 0.9em; }
"#;

/// Selective-colour band names; their hues are `grade::BAND_HUES`.
fn band_names() -> [String; 8] {
    [
        gettext("Reds"),
        gettext("Oranges"),
        gettext("Yellows"),
        gettext("Greens"),
        gettext("Teals"),
        gettext("Blues"),
        gettext("Purples"),
        gettext("Magentas"),
    ]
}
/// CSS class prefixes of the band's hue / saturation / luminance tracks.
const BAND_TRACKS: [&str; 3] = ["hue", "sat", "lum"];

/// Swatches and slider tracks computed in OKLCh from the shader's own band
/// hues, so each track shows exactly what moving it does to that band.
fn band_css() -> String {
    let hex = |l: f32, c: f32, h: f32| {
        let [r, g, b] = oklch_to_srgb(l, c, h).map(|v| (v * 255.0).round() as u8);
        format!("#{r:02x}{g:02x}{b:02x}")
    };
    let mut css = String::new();
    for (i, &h) in BAND_HUES.iter().enumerate() {
        let hue: Vec<String> =
            [-1.0, -0.5, 0.0, 0.5, 1.0].iter().map(|t| hex(0.72, 0.13, h + t * BAND_HUE_RANGE)).collect();
        css += &format!(".band-{i} {{ background: {}; }}\n", hex(0.68, 0.15, h));
        css +=
            &format!("scale.hue-{i} trough {{ background-image: linear-gradient(to right, {}); }}\n", hue.join(", "));
        css += &format!(
            "scale.sat-{i} trough {{ background-image: linear-gradient(to right, {}, {}); }}\n",
            hex(0.72, 0.0, h),
            hex(0.72, 0.2, h)
        );
        css += &format!(
            "scale.lum-{i} trough {{ background-image: linear-gradient(to right, {}, {}); }}\n",
            hex(0.38, 0.1, h),
            hex(0.92, 0.1, h)
        );
    }
    css
}

struct Sidebar {
    scroll: gtk::ScrolledWindow,
    sliders: Vec<Slider>,
    wheels: Vec<(ColorWheel, Slider)>,
    hue: [Slider; 3],
    curves: CurveEditor,
    scopes: [gtk::Picture; 3],
    looks: gtk::ListBox,
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
    let scope_sel = toggles(&[
        ("hist", gettext("Histogram").as_str()),
        ("wave", gettext("Waveform").as_str()),
        ("vec", gettext("Vectorscope").as_str()),
    ]);
    let st = scope_stack.clone();
    scope_sel.connect_active_name_notify(move |g| {
        if let Some(n) = g.active_name() {
            st.set_visible_child_name(&n);
        }
    });
    let scopes_box = vbox(8);
    scopes_box.append(&scope_sel);
    scopes_box.append(&scope_stack);
    panel.append(&section(&gettext("Scopes"), &scopes_box));

    // Input profile: the signal the LUT will receive in camera.
    let profile_box = vbox(6);
    let profile_sel = lab_page::profile_dropdown(Profile::Standard);
    profile_sel.update_property(&[gtk::accessible::Property::Label(&gettext("Input profile"))]);
    let hint = gtk::Label::builder()
        .label(profile_hint(Profile::Standard))
        .css_classes(["caption", "dim-label"])
        .wrap(true)
        .xalign(0.0)
        .build();
    let (snd, h) = (sender.clone(), hint.clone());
    profile_sel.connect_selected_notify(move |d| {
        let profile = Profile::ALL[d.selected() as usize];
        h.set_label(&profile_hint(profile));
        snd.input(Msg::Profile(profile));
    });
    profile_box.append(&profile_sel);
    profile_box.append(&hint);
    panel.append(&section(&gettext("Input profile"), &profile_box));

    let mut sliders = Vec::new();
    let mut add = |bx: &gtk::Box, label: &str, min: f64, max: f64, digits: u32, track: Option<&str>, field: Field| {
        let s = Slider::new(label, min, max, 0.0, digits, track);
        let snd = sender.clone();
        s.connect(move |v| snd.input(Msg::Set(field, v)));
        bx.append(&s.root);
        sliders.push(s);
    };

    let wb = vbox(4);
    add(&wb, &gettext("Temperature"), -100.0, 100.0, 0, Some("track-temp"), |p| &mut p.temperature);
    add(&wb, &gettext("Tint"), -100.0, 100.0, 0, Some("track-tint"), |p| &mut p.tint);
    panel.append(&bypassable(&gettext("White balance"), &wb, Some(Stage::WhiteBalance), sender));

    let tone = vbox(4);
    add(&tone, &gettext("Exposure"), -3.0, 3.0, 2, None, |p| &mut p.exposure);
    add(&tone, &gettext("Contrast"), -100.0, 100.0, 0, None, |p| &mut p.contrast);
    add(&tone, &gettext("Highlights"), -100.0, 100.0, 0, None, |p| &mut p.highlights);
    add(&tone, &gettext("Shadows"), -100.0, 100.0, 0, None, |p| &mut p.shadows);
    add(&tone, &gettext("Whites"), -100.0, 100.0, 0, None, |p| &mut p.whites);
    add(&tone, &gettext("Blacks"), -100.0, 100.0, 0, None, |p| &mut p.blacks);
    panel.append(&bypassable(&gettext("Tone"), &tone, Some(Stage::Tone), sender));

    let color = vbox(4);
    add(&color, &gettext("Saturation"), -100.0, 100.0, 0, None, |p| &mut p.saturation);
    add(&color, &gettext("Vibrance"), -100.0, 100.0, 0, None, |p| &mut p.vibrance);
    add(&color, &gettext("Shadow saturation"), -100.0, 100.0, 0, None, |p| &mut p.sat_shadows);
    add(&color, &gettext("Highlight saturation"), -100.0, 100.0, 0, None, |p| &mut p.sat_highlights);
    panel.append(&bypassable(&gettext("Color"), &color, Some(Stage::Color), sender));

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
    panel.append(&bypassable(&gettext("Primary color wheels"), &wheels_row, Some(Stage::Wheels), sender));

    // HSL mixer (OKLCh bands)
    let hsl = vbox(6);
    let names = band_names();
    let band_name = gtk::Label::builder().label(names[0].as_str()).css_classes(["caption-heading"]).build();
    let dots = gtk::Box::builder().spacing(6).halign(gtk::Align::Center).build();
    let mut first: Option<gtk::ToggleButton> = None;
    for (i, name) in names.iter().enumerate() {
        let b = gtk::ToggleButton::builder()
            .tooltip_text(name.as_str())
            .css_classes(["band", &format!("band-{i}")])
            .build();
        b.update_property(&[gtk::accessible::Property::Label(name)]);
        match &first {
            None => {
                b.set_active(true);
                first = Some(b.clone());
            }
            Some(f) => b.set_group(Some(f)),
        }
        let (snd, bn, name) = (sender.clone(), band_name.clone(), name.clone());
        b.connect_toggled(move |b| {
            if b.is_active() {
                bn.set_label(&name);
                snd.input(Msg::Band(i));
            }
        });
        dots.append(&b);
    }
    hsl.append(&dots);
    hsl.append(&band_name);
    let hue: [Slider; 3] = std::array::from_fn(|k| {
        let label = [gettext("Hue"), gettext("Saturation"), gettext("Luminance")][k].clone();
        let s = Slider::new(&label, -100.0, 100.0, 0.0, 0, Some(&format!("{}-0", BAND_TRACKS[k])));
        let snd = sender.clone();
        s.connect(move |v| snd.input(Msg::Hue(k, v)));
        hsl.append(&s.root);
        s
    });
    panel.append(&bypassable(&gettext("Selective color"), &hsl, Some(Stage::Selective), sender));

    // Curves
    let snd = sender.clone();
    let curves = CurveEditor::new(move |ch, c| snd.input(Msg::Curve(ch, c)));
    let curve_sel = toggles(&[
        ("0", "Luma"),
        ("1", gettext("Red").as_str()),
        ("2", gettext("Green").as_str()),
        ("3", gettext("Blue").as_str()),
    ]);
    let ce = curves.clone();
    curve_sel.connect_active_name_notify(move |g| {
        if let Some(n) = g.active_name() {
            ce.set_active(n.parse().unwrap_or(0));
        }
    });
    let reset_curve =
        gtk::Button::builder().label(gettext("Reset curve")).css_classes(["flat"]).halign(gtk::Align::End).build();
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
            .label(gettext("Click to add a point, double-click or right-click to remove it"))
            .css_classes(["caption", "dim-label"])
            .wrap(true)
            .build(),
    );
    curve_box.append(&reset_curve);
    panel.append(&bypassable(&gettext("Curves"), &curve_box, Some(Stage::Curves), sender));

    // LUTs after the grade
    let look_box = vbox(8);
    let looks = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    let empty = lab_page::caption(&gettext("No LUTs: the grade is exported as is."));
    empty.set_margin_top(12);
    empty.set_margin_bottom(12);
    empty.set_margin_start(12);
    empty.set_margin_end(12);
    looks.set_placeholder(Some(&empty));
    look_box.append(&looks);
    let add_menu = gio::Menu::new();
    add_menu.append(Some(gettext("LUT from file…").as_str()), Some("win.look-add::file"));
    add_menu.append(Some(gettext("LUT selected in Compare").as_str()), Some("win.look-add::selected"));
    look_box.append(
        &gtk::MenuButton::builder()
            .label(gettext("Add LUT"))
            .menu_model(&add_menu)
            .halign(gtk::Align::Start)
            .css_classes(["pill"])
            .build(),
    );
    look_box.append(&lab_page::caption(&gettext(
        "Applied after the grade, top to bottom, and included in the exported LUT.",
    )));
    panel.append(&bypassable(&gettext("LUTs"), &look_box, None, sender));

    let reset = gtk::Button::builder()
        .label(gettext("Reset all"))
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
    Sidebar { scroll, sliders, wheels, hue, curves, scopes: scope_pics, looks }
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
    type Init = ();
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

    fn init(_: Self::Init, window: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        // Neutral dark surround is the colourist standard.
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::PreferDark);

        let snd = sender.clone();
        let jobs = gpu::spawn(move |d| snd.input(Msg::Gpu(d)));

        // ---------------- grade page
        let grade_pic = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        grade_pic.update_property(&[gtk::accessible::Property::Label(&gettext("Grade preview"))]);
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
            &empty_state(
                &sender,
                "image-x-generic-symbolic",
                &gettext("Open a photo"),
                &gettext("Drop a JPEG or camera RAW here, or choose a file.\nThe grade becomes a .cube LUT for the Standard profile."),
                &gettext("Open Photo…"),
                || Msg::Open,
            ),
            Some("empty"),
        );
        grade_stack.add_named(&loading, Some("loading"));
        grade_stack.add_named(&canvas, Some("image"));

        let sidebar = build_sidebar(&sender);
        let grade_split = adw::OverlaySplitView::builder()
            .sidebar_position(gtk::PackType::End)
            .min_sidebar_width(340.0)
            .max_sidebar_width(420.0)
            .sidebar_width_fraction(0.28)
            .content(&grade_stack)
            .sidebar(&sidebar.scroll)
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
            .label(gettext("No folder"))
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["heading"])
            .build();
        let folder_btn = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text(gettext("Choose LUT folder"))
            .css_classes(["flat"])
            .build();
        let snd = sender.clone();
        folder_btn.connect_clicked(move |_| snd.input(Msg::ChooseFolder));
        let folder_row =
            gtk::Box::builder().spacing(6).margin_start(12).margin_end(8).margin_top(8).margin_bottom(4).build();
        let to_lab = gtk::Button::builder()
            .icon_name("view-list-symbolic")
            .tooltip_text(gettext("Open the selected LUT in LUT Lab (to convert or combine it)"))
            .css_classes(["flat"])
            .build();
        let snd = sender.clone();
        to_lab.connect_clicked(move |_| snd.input(Msg::OpenInLab));
        folder_row.append(&folder_label);
        folder_row.append(&to_lab);
        let reload = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text(gettext("Reload folder"))
            .css_classes(["flat"])
            .build();
        let snd = sender.clone();
        reload.connect_clicked(move |_| snd.input(Msg::Reload));
        folder_row.append(&reload);
        let original = gtk::ToggleButton::builder()
            .icon_name("image-x-generic-symbolic")
            .tooltip_text(gettext("Compare on the original photo, without the Grade edits"))
            .css_classes(["flat"])
            .build();
        let snd = sender.clone();
        original.connect_toggled(move |b| snd.input(Msg::CompareOriginal(b.is_active())));
        folder_row.append(&original);
        folder_row.append(&folder_btn);
        let groups = gtk::DropDown::builder().visible(false).margin_start(8).margin_end(8).margin_bottom(4).build();
        groups.update_property(&[gtk::accessible::Property::Label(&gettext("Subfolder"))]);
        let snd = sender.clone();
        groups.connect_selected_notify(move |d| snd.input(Msg::Group(d.selected())));
        let search = gtk::SearchEntry::builder()
            .placeholder_text(gettext("Search LUTs"))
            .margin_start(8)
            .margin_end(8)
            .margin_bottom(6)
            .build();
        let snd = sender.clone();
        search.connect_search_changed(move |e| snd.input(Msg::Search(e.text().into())));
        let gallery_stack = gtk::Stack::new();
        gallery_stack.add_named(
            &adw::StatusPage::builder()
                .icon_name("view-grid-symbolic")
                .title(gettext("No LUTs"))
                .description(gettext("Choose a folder with .cube files"))
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
        gallery.append(&groups);
        gallery.append(&search);
        gallery.append(&gallery_stack);

        let compare_pic = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        compare_pic.update_property(&[gtk::accessible::Property::Label(&gettext("Selected LUT preview"))]);
        add_split_drag(&compare_pic, &sender);
        let compare_canvas = gtk::Overlay::builder().child(&compare_pic).css_classes(["canvas"]).build();
        let compare_stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
        compare_stack.add_named(
            &empty_state(
                &sender,
                "view-grid-symbolic",
                &gettext("Compare LUTs"),
                &gettext("Choose a folder of .cube LUTs: each one is applied to the current photo with tetrahedral interpolation on the GPU."),
                &gettext("Choose Folder…"),
                || Msg::ChooseFolder,
            ),
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
        stack.add_titled_with_icon(&compare_split, Some("compare"), &gettext("Compare LUTs"), "view-grid-symbolic");
        let lab = lab_page::build(&sender, &export_menu("lab-export", false));
        stack.add_titled_with_icon(&lab.root, Some("lab"), "LUT Lab", "view-list-symbolic");
        let snd = sender.clone();
        stack.connect_visible_child_name_notify(move |s| {
            let page = match s.visible_child_name().as_deref() {
                Some("compare") => Page::Compare,
                Some("lab") => Page::Lab,
                _ => Page::Grade,
            };
            snd.input(Msg::Page(page));
        });

        let header = adw::HeaderBar::new();
        let switcher = adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build();
        header.set_title_widget(Some(&switcher));

        let open = gtk::Button::builder()
            .icon_name("document-open-symbolic")
            .tooltip_text(gettext("Open Photo (Ctrl+O)"))
            .build();
        let snd = sender.clone();
        open.connect_clicked(move |_| snd.input(Msg::Open));
        header.pack_start(&open);
        let chart_btn = gtk::ToggleButton::builder()
            .icon_name("applications-science-symbolic")
            .tooltip_text(gettext("Test chart: shows what the LUT does to neutrals, hues and skin (T)"))
            .build();
        let snd = sender.clone();
        chart_btn.connect_toggled(move |b| snd.input(Msg::Chart(b.is_active())));
        header.pack_start(&chart_btn);

        let menu = gio::Menu::new();
        let bg_menu = gio::Menu::new();
        bg_menu.append(Some(gettext("Default").as_str()), Some("win.background::default"));
        bg_menu.append(Some(gettext("Black").as_str()), Some("win.background::black"));
        bg_menu.append(Some(gettext("White").as_str()), Some("win.background::white"));
        menu.append_section(Some(gettext("Preview background").as_str()), &bg_menu);
        let section_menu = gio::Menu::new();
        section_menu.append(Some(gettext("Preferences").as_str()), Some("win.preferences"));
        section_menu.append(Some(gettext("Keyboard Shortcuts").as_str()), Some("win.show-help-overlay"));
        section_menu.append(Some(gettext("About VibingLUT").as_str()), Some("win.about"));
        menu.append_section(None, &section_menu);
        header.pack_end(
            &gtk::MenuButton::builder()
                .icon_name("open-menu-symbolic")
                .menu_model(&menu)
                .primary(true)
                .tooltip_text(gettext("Main Menu"))
                .build(),
        );

        let export_menu = export_menu("export", true);
        let export = adw::SplitButton::builder()
            .label(gettext("Export LUT"))
            .menu_model(&export_menu)
            .css_classes(["suggested-action"])
            .tooltip_text(gettext("Export a 33³ .cube (Ctrl+E)"))
            .sensitive(false)
            .build();
        let snd = sender.clone();
        export.connect_clicked(move |_| snd.input(Msg::Export(Format::Cube, 33, What::Grade)));
        header.pack_end(&export);

        let compare_btn = gtk::ToggleButton::builder()
            .icon_name("view-dual-symbolic")
            .tooltip_text(gettext("Before / after (\\)"))
            .build();
        let snd = sender.clone();
        compare_btn.connect_toggled(move |b| snd.input(Msg::Compare(b.is_active())));
        header.pack_end(&compare_btn);

        let sidebar_btn = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-right-symbolic")
            .tooltip_text(gettext("Show panel"))
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
        bp.add_setter(&lab.root, "collapsed", Some(&true.to_value()));
        bp.add_setter(&sidebar_btn, "visible", Some(&true.to_value()));
        bp.add_setter(&header, "title-widget", Some(&None::<gtk::Widget>.to_value()));
        bp.add_setter(&switcher_bar, "reveal", Some(&true.to_value()));
        window.add_breakpoint(bp);

        // ---------------- actions & shortcuts
        for (name, what) in [("export", What::Grade), ("lab-export", What::Lab)] {
            let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
            let snd = sender.clone();
            action.connect_activate(move |_, v| {
                // Target "<kind>-<size>", e.g. "vlt-17", "identity-64".
                let Some(target) = v.and_then(|v| v.str().map(str::to_owned)) else { return };
                let Some((kind, size)) = target.split_once('-') else { return };
                let size = size.parse().unwrap_or(33);
                let (format, what) = match kind {
                    "vlt" => (Format::Vlt, what),
                    "hald" => (Format::Hald, what),
                    "identity" => (Format::Hald, What::Identity),
                    _ => (Format::Cube, what),
                };
                snd.input(Msg::Export(format, size, what));
            });
            window.add_action(&action);
        }
        for (name, which) in [("lab-add", Layers::Lab), ("look-add", Layers::Look)] {
            let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
            let snd = sender.clone();
            action.connect_activate(move |_, v| {
                let add = match v.and_then(|v| v.str().map(str::to_owned)).as_deref() {
                    Some("selected") => LabAdd::Selected,
                    Some("file") => LabAdd::File,
                    Some("grade") => LabAdd::Grade,
                    _ => LabAdd::Convert,
                };
                snd.input(Msg::LayerAdd(which, add));
            });
            window.add_action(&action);
        }
        let background =
            gio::SimpleAction::new_stateful("background", Some(glib::VariantTy::STRING), &"default".to_variant());
        let canvases = [canvas.clone(), compare_canvas.clone(), lab.canvas.clone()];
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
                .comments(gettext(
                    "Color grading from photos and 3D .cube LUT creation, GPU accelerated (Vulkan/Metal).",
                ))
                .license_type(gtk::License::Gpl30)
                .developer_name("Nicolò Santamaria")
                .build()
                .present(Some(&w));
        });
        window.add_action(&about);
        let prefs_action = gio::SimpleAction::new("preferences", None);
        let snd = sender.clone();
        prefs_action.connect_activate(move |_, _| snd.input(Msg::Preferences));
        window.add_action(&prefs_action);
        let prefs_btn = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text(gettext("Choose folder"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let snd = sender.clone();
        prefs_btn.connect_clicked(move |_| snd.input(Msg::ChooseFolder));
        let prefs_row = adw::ActionRow::builder()
            .title(gettext("LUT folder"))
            .subtitle(gettext("No folder"))
            .activatable_widget(&prefs_btn)
            .build();
        prefs_row.add_suffix(&prefs_btn);
        let prefs_group = adw::PreferencesGroup::builder()
            .title(gettext("LUT library"))
            .description(gettext("LUT files in this folder and its subfolders appear in Compare LUTs."))
            .build();
        prefs_group.add(&prefs_row);
        let prefs_page =
            adw::PreferencesPage::builder().title(gettext("General")).icon_name("preferences-system-symbolic").build();
        prefs_page.add(&prefs_group);
        let prefs = adw::PreferencesDialog::new();
        prefs.add(&prefs_page);
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
        let c = chart_btn.clone();
        shortcut("t", Box::new(move || c.set_active(!c.is_active())));
        let c = compare_btn.clone();
        shortcut("backslash", Box::new(move || c.set_active(!c.is_active())));
        let s = stack.clone();
        shortcut("<Alt>1", Box::new(move || s.set_visible_child_name("grade")));
        let s = stack.clone();
        shortcut("<Alt>2", Box::new(move || s.set_visible_child_name("compare")));
        let (s, e) = (stack.clone(), search.clone());
        shortcut(
            "<Control>f",
            Box::new(move || {
                s.set_visible_child_name("compare");
                e.grab_focus();
            }),
        );
        let w = window.clone();
        let snd = sender.clone();
        shortcut("<Control>comma", Box::new(move || snd.input(Msg::Preferences)));
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
            lut_paths: Arc::default(),
            group_names: Vec::new(),
            group: None,
            query: String::new(),
            page: Page::Grade,
            lab_layers: Vec::new(),
            looks: Vec::new(),
            bypassed: Vec::new(),
            looks_off: false,
            compare_original: false,
            base_key: None,
            gallery_stale: true,
            lab_input: Profile::Standard,
            lab_blend: Blend::Chain,
            profile: Profile::Standard,
            decoded: None,
            photo: None,
            chart: None,
            show_chart: false,
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
                sidebar,
                gallery_stack,
                compare_stack,
                compare_pic,
                flow,
                folder_label,
                groups,
                prefs,
                prefs_row,
                lab,
                stack: stack.clone(),
                thumbs: Vec::new(),
                export,
                chart_btn,
            },
        };
        OPEN.with(|o| o.set(sender.input_sender().clone()).ok());
        if let Some(dir) = settings::folder(settings::LUT_FOLDER) {
            sender.input(Msg::Folder(dir));
        }
        ComponentParts { model, widgets: () }
    }

    fn update(&mut self, msg: Msg, sender: ComponentSender<Self>) {
        match msg {
            Msg::Open => {
                // add_suffix matches case-insensitively.
                let pats: Vec<String> = RASTER_EXTS.iter().chain(RAW_EXTS).map(|s| s.to_string()).collect();
                let dialog = gtk::FileDialog::builder()
                    .title(gettext("Open Photo"))
                    .filters(&file_filters(&gettext("Photos and RAWs"), &pats))
                    .modal(true)
                    .build();
                dialog.set_initial_folder(start_in(settings::PHOTO_FOLDER).as_ref());
                let s = sender.clone();
                dialog.open(Some(&self.ui.window), gio::Cancellable::NONE, move |r| {
                    if let Some(p) = r.ok().and_then(|f| f.path()) {
                        s.input(Msg::OpenPath(p));
                    }
                });
            }
            Msg::OpenPath(path) => {
                // ponytail: a failed save only loses the dialog's start folder.
                if let Some(dir) = path.parent() {
                    let _ = settings::set_folder(settings::PHOTO_FOLDER, dir);
                }
                self.ui.grade_stack.set_visible_child_name("loading");
                self.load_gen += 1;
                let (s, generation) = (sender.input_sender().clone(), self.load_gen);
                std::thread::spawn(move || {
                    let r = Decoded::load(&path);
                    s.emit(Msg::Loaded(generation, path, r));
                });
            }
            Msg::Loaded(generation, ..) if generation != self.load_gen => {}
            Msg::Loaded(_, path, Ok(decoded)) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.ui.window.set_title(Some(&format!("{name} — VibingLUT")));
                self.decoded = Some(decoded);
                self.path = Some(path);
                // Opening a photo means you want to see it.
                if self.show_chart {
                    self.show_chart = false;
                    self.ui.chart_btn.set_active(false);
                }
                self.encode_photo(&sender);
            }
            Msg::Loaded(_, path, Err(e)) => {
                if !self.has_source() {
                    self.ui.grade_stack.set_visible_child_name("empty");
                } else {
                    self.ui.grade_stack.set_visible_child_name("image");
                }
                self.toast(&gettext!("Cannot open {}: {}", path.display(), e));
            }
            Msg::Encoded(generation, profile, photo) => {
                if generation == self.load_gen && profile == self.profile {
                    self.photo = Some((photo, profile));
                    self.show_source();
                }
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
                for (k, (s, v)) in
                    self.ui.sidebar.hue.iter().zip([p.hue_shift[b], p.hue_sat[b], p.hue_lum[b]]).enumerate()
                {
                    for i in 0..BAND_HUES.len() {
                        s.scale.remove_css_class(&format!("{}-{i}", BAND_TRACKS[k]));
                    }
                    s.scale.add_css_class(&format!("{}-{b}", BAND_TRACKS[k]));
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
                self.ui.sidebar.sliders.iter().chain(&self.ui.sidebar.hue).for_each(Slider::reset);
                for (w, l) in &self.ui.sidebar.wheels {
                    w.set(0.0, 0.0);
                    l.reset();
                }
                self.ui.sidebar.curves.reset_all();
                self.grade = self.grade.neutralized();
                self.render();
            }
            Msg::Compare(on) => {
                self.compare = on;
                self.redraw();
            }
            Msg::Split(x) => {
                if self.compare {
                    self.split = x;
                    self.redraw();
                }
            }
            // Ctrl+E fires even while the button is insensitive.
            Msg::Export(_, _, What::Grade) if !self.has_source() => {}
            Msg::Export(_, _, What::Lab) if self.lab_layers.is_empty() => {}
            Msg::Export(format, size, what) => {
                let stem = match (what, &self.path) {
                    (What::Identity, _) => format!("hald_identity_{size}"),
                    (What::Lab, _) => format!("lab_{}_{size}", self.lab_input.tag()),
                    (What::Grade, Some(p)) => {
                        let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        format!("{stem}_{}_{size}", self.profile.tag())
                    }
                    (What::Grade, None) => format!("chart_{}_{size}", self.profile.tag()),
                };
                let label = format!("{} {size}³", format.badge());
                let dialog = gtk::FileDialog::builder()
                    .title(gettext!("Export {}", label))
                    .initial_name(format!("{stem}.{}", format.ext()))
                    .filters(&file_filters(&label, &[format.ext().into()]))
                    .modal(true)
                    .build();
                let s = sender.clone();
                dialog.save(Some(&self.ui.window), gio::Cancellable::NONE, move |r| {
                    if let Some(p) = r.ok().and_then(|f| f.path()) {
                        s.input(Msg::ExportTo(format, size, what, p));
                    }
                });
            }
            Msg::ExportTo(format, size, what, path) => {
                let title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                match what {
                    What::Identity => {
                        // The neutral LUT needs no GPU: write it off the main thread.
                        let s = sender.input_sender().clone();
                        std::thread::spawn(move || {
                            let r = Lut3d::identity(size, format).save(&path).map(|_| path);
                            s.emit(Msg::Gpu(Done::Exported(r)));
                        });
                    }
                    What::Grade => self.send(Job::Export {
                        grade: self.graded(),
                        look: self.look_layers(),
                        size,
                        format,
                        title,
                        path,
                    }),
                    What::Lab => {
                        self.send(Job::ExportLab { stack: self.lab_stack(self.lab_input), size, format, title, path })
                    }
                }
            }
            Msg::ChooseFolder => {
                let dialog = gtk::FileDialog::builder().title(gettext("LUT folder")).modal(true).build();
                dialog.set_initial_folder(start_in(settings::LUT_FOLDER).as_ref());
                let s = sender.clone();
                dialog.select_folder(Some(&self.ui.window), gio::Cancellable::NONE, move |r| {
                    if let Some(p) = r.ok().and_then(|f| f.path()) {
                        s.input(Msg::Folder(p));
                    }
                });
            }
            Msg::Folder(dir) => {
                if let Err(e) = settings::set_folder(settings::LUT_FOLDER, &dir) {
                    self.toast(&gettext!("Cannot save the LUT folder: {}", e));
                }
                self.ui.prefs_row.set_subtitle(&dir.to_string_lossy());
                self.ui
                    .folder_label
                    .set_label(&dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let (luts, errors, truncated) = Lut3d::load_dir(&dir);
                    s.emit(Msg::GalleryLoaded(luts, errors, truncated));
                });
            }
            Msg::GalleryLoaded(luts, errors, truncated) => {
                if !errors.is_empty() {
                    self.toast(&gettext!("{} files skipped: {}", errors.len(), errors.join("; ")));
                }
                self.ui.flow.remove_all();
                self.ui.thumbs.clear();
                if truncated {
                    self.toast(&gettext!("Very large library: showing the first {} LUTs", MAX_LIBRARY));
                }
                for lut in &luts {
                    let pic = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).height_request(96).build();
                    let badge = gtk::Label::builder()
                        .label(lut.format.badge())
                        .halign(gtk::Align::End)
                        .valign(gtk::Align::Start)
                        .css_classes(["lut-badge"])
                        .build();
                    let framed = gtk::Overlay::builder().child(&pic).build();
                    framed.add_overlay(&badge);
                    let card = vbox(4);
                    card.add_css_class("thumb-card");
                    card.append(&framed);
                    card.append(
                        &gtk::Label::builder()
                            .label(&lut.title)
                            .ellipsize(gtk::pango::EllipsizeMode::End)
                            .css_classes(["caption"])
                            .build(),
                    );
                    let place = if lut.group.is_empty() { String::new() } else { format!("{} · ", lut.group) };
                    card.set_tooltip_text(Some(&format!(
                        "{place}{} · {} {}³",
                        lut.title,
                        lut.format.badge(),
                        lut.size
                    )));
                    self.ui.flow.append(&card);
                    self.ui.thumbs.push(pic);
                }
                self.ui.gallery_stack.set_visible_child_name(if luts.is_empty() { "empty" } else { "grid" });
                self.group = None;
                self.group_names = luts.iter().map(|l| l.group.clone()).collect();
                self.group_names.dedup(); // sorted by group in core
                let labels: Vec<String> = std::iter::once(gettext("All folders"))
                    .chain(
                        self.group_names.iter().map(|g| if g.is_empty() { gettext("Main folder") } else { g.clone() }),
                    )
                    .collect();
                let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
                self.ui.groups.set_model(Some(&gtk::StringList::new(&labels)));
                self.ui.groups.set_visible(self.group_names.iter().any(|g| !g.is_empty()));
                self.lut_paths = Arc::new(luts.iter().map(|l| l.path.clone()).collect());
                self.luts = luts;
                self.filter_gallery();
                self.selected = None;
                self.ui.compare_stack.set_visible_child_name("empty");
                self.gallery_stale = true;
                if matches!(self.page, Page::Compare) {
                    self.refresh();
                }
                if let Some(c) = self.ui.flow.child_at_index(0) {
                    self.ui.flow.select_child(&c);
                }
            }
            Msg::Group(sel) => {
                // Index 0 shows everything; otherwise only LUTs of that sub-folder.
                self.group = (sel as usize).checked_sub(1).and_then(|i| self.group_names.get(i)).cloned();
                self.filter_gallery();
            }
            Msg::Bypass(Some(stage), off) => {
                self.bypassed.retain(|&s| s != stage);
                if off {
                    self.bypassed.push(stage);
                }
                self.render();
            }
            Msg::Bypass(None, off) => {
                self.looks_off = off;
                self.layers_changed(Layers::Look, false, &sender);
            }
            Msg::CompareOriginal(on) => {
                self.compare_original = on;
                self.refresh();
            }
            Msg::Reload => match settings::folder(settings::LUT_FOLDER) {
                Some(dir) => sender.input(Msg::Folder(dir)),
                None => sender.input(Msg::ChooseFolder),
            },
            Msg::Search(text) => {
                self.query = text.to_lowercase();
                self.filter_gallery();
            }
            Msg::Preferences => self.ui.prefs.present(Some(&self.ui.window)),
            Msg::SelectLut(i) => {
                if i < self.luts.len() && self.selected != Some(i) {
                    self.selected = Some(i);
                    self.render_compare();
                }
            }
            Msg::Profile(profile) => {
                self.profile = profile;
                if self.show_chart {
                    sender.input(Msg::Chart(true));
                }
                self.encode_photo(&sender);
            }
            Msg::Chart(on) => {
                self.show_chart = on;
                // The chart is encoded per profile; (re)build it off the main thread.
                if !on || self.chart.as_ref().is_some_and(|c| c.1 == self.profile) {
                    self.show_source();
                } else {
                    let (s, profile) = (sender.input_sender().clone(), self.profile);
                    std::thread::spawn(move || s.emit(Msg::ChartReady(profile, Arc::new(test_chart(profile)))));
                }
            }
            Msg::ChartReady(profile, chart) => {
                if profile == self.profile {
                    self.chart = Some((chart, profile));
                    if self.show_chart {
                        self.show_source();
                    }
                }
            }
            Msg::LayerAdd(which, add) => match add {
                LabAdd::Selected => match self.selected.and_then(|i| self.luts.get(i)) {
                    Some(entry) => {
                        let (p, s) = (entry.path.clone(), sender.input_sender().clone());
                        std::thread::spawn(move || {
                            let lut = Lut3d::load(&p);
                            s.emit(Msg::LayerLoaded(which, p, lut));
                        });
                    }
                    None => self.toast(&gettext("Select a LUT in Compare LUTs first")),
                },
                LabAdd::File => {
                    let pats: Vec<String> = ["cube", "vlt", "png"].map(String::from).to_vec();
                    let dialog = gtk::FileDialog::builder()
                        .title(gettext("Add LUT"))
                        .filters(&file_filters(&gettext("LUTs (.cube, .vlt, HALD .png)"), &pats))
                        .modal(true)
                        .build();
                    dialog.set_initial_folder(start_in(settings::LUT_FOLDER).as_ref());
                    let s = sender.clone();
                    dialog.open(Some(&self.ui.window), gio::Cancellable::NONE, move |r| {
                        let Some(p) = r.ok().and_then(|f| f.path()) else { return };
                        let s = s.input_sender().clone();
                        std::thread::spawn(move || {
                            let lut = Lut3d::load(&p);
                            s.emit(Msg::LayerLoaded(which, p, lut));
                        });
                    });
                }
                LabAdd::Grade => self.push_layer(which, LabKind::Grade, Profile::Standard, &sender),
                LabAdd::Convert => {
                    self.push_layer(which, LabKind::Convert(Profile::Standard), Profile::Standard, &sender)
                }
            },
            Msg::OpenInLab => {
                if self.selected.is_some() {
                    self.ui.stack.set_visible_child_name("lab");
                }
                sender.input(Msg::LayerAdd(Layers::Lab, LabAdd::Selected));
            }
            Msg::LayerLoaded(which, _, Ok(lut)) => self.push_lut(which, Arc::new(lut), &sender),
            Msg::LayerLoaded(_, p, Err(e)) => self.toast(&gettext!("Cannot read {}: {}", p.display(), e)),
            Msg::LayerRemove(which, i) => {
                if i < self.layers_mut(which).len() {
                    self.layers_mut(which).remove(i);
                    self.layers_changed(which, true, &sender);
                }
            }
            Msg::LayerMove(which, i, up) => {
                let j = if up { i.checked_sub(1) } else { Some(i + 1) };
                if let Some(j) = j.filter(|&j| j < self.layers_mut(which).len()) {
                    self.layers_mut(which).swap(i, j);
                    self.layers_changed(which, true, &sender);
                }
            }
            Msg::LayerIntensity(which, i, v) => {
                if let Some(l) = self.layers_mut(which).get_mut(i) {
                    l.intensity = (v / 100.0) as f32;
                    self.layers_changed(which, false, &sender);
                }
            }
            Msg::LayerProfile(which, i, p) => {
                if let Some(l) = self.layers_mut(which).get_mut(i) {
                    match &mut l.kind {
                        LabKind::Convert(to) => *to = p,
                        _ => l.input = p,
                    }
                    self.layers_changed(which, false, &sender);
                }
            }
            // Only the summary depends on these; the rows stay as they are.
            Msg::LabInput(p) => {
                self.lab_input = p;
                self.ui.lab.refresh_summary(&self.lab_layers, self.lab_blend, self.lab_input);
            }
            Msg::LabBlend(b) => {
                self.lab_blend = b;
                self.ui.lab.refresh_summary(&self.lab_layers, self.lab_blend, self.lab_input);
                self.render_lab();
            }
            Msg::Page(page) => {
                self.page = page;
                self.refresh();
            }
            Msg::Gpu(done) => self.on_gpu(done),
        }
    }
}

impl App {
    fn on_gpu(&mut self, done: Done) {
        match done {
            Done::Ready(name) => self.toast(&format!("GPU: {name}")),
            Done::Failed(e) => self.toast(&gettext!("GPU unavailable: {}", e)),
            Done::Graded { width, height, rgba, scopes: Scopes { histogram, waveform, vectorscope } } => {
                self.ui.grade_pic.set_paintable(Some(&texture(width, height, rgba)));
                let [h, w, v] = &self.ui.sidebar.scopes;
                h.set_paintable(Some(&texture(HIST_W as u32, HIST_H as u32, histogram)));
                w.set_paintable(Some(&texture(HIST_W as u32, HIST_H as u32, waveform)));
                v.set_paintable(Some(&texture(VEC_SIZE as u32, VEC_SIZE as u32, vectorscope)));
            }
            Done::Lab { width, height, rgba } => self.ui.lab.pic.set_paintable(Some(&texture(width, height, rgba))),
            Done::Compared { width, height, rgba } => {
                self.ui.compare_pic.set_paintable(Some(&texture(width, height, rgba)));
                self.ui.compare_stack.set_visible_child_name("image");
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
                self.toast(&gettext!("LUT saved: {}", p.file_name().unwrap_or_default().to_string_lossy()))
            }
            Done::Exported(Err(e)) => self.toast(&gettext!("Export failed: {}", e)),
            // Wired up with the filmstrip and Match UI.
            Done::RollFrame { .. } | Done::Matched(_) => {}
            Done::Unreadable(p, e) => {
                self.ui.compare_stack.set_visible_child_name("empty");
                self.toast(&gettext!("Cannot read {}: {}", p.display(), e));
            }
        }
    }
}

fn profile_hint(p: Profile) -> String {
    if p.is_log() {
        gettext!("{} LUT → Rec.709. RAWs are encoded into this log; a JPEG/TIFF must already be in log.", p.name())
    } else {
        gettext("Signal the LUT will be applied to (Photo Style Standard, Vivid…).")
    }
}

/// Menu of export formats for action `action` (`win.<action>::<kind>-<size>`).
fn export_menu(action: &str, with_identity: bool) -> gio::Menu {
    let item =
        |m: &gio::Menu, label: &str, target: &str| m.append(Some(label), Some(&format!("win.{action}::{target}")));
    let menu = gio::Menu::new();
    // .cube is the universal 3D LUT format; the sizes differ only in precision.
    let cube = gio::Menu::new();
    item(&cube, &gettext("33³ .cube · cameras and monitors"), "cube-33");
    item(&cube, &gettext("65³ .cube · grading and editing"), "cube-65");
    item(&cube, &gettext("129³ .cube · maximum precision"), "cube-129");
    menu.append_section(Some(".cube"), &cube);
    let other = gio::Menu::new();
    item(&other, &gettext("Panasonic VLT · 17³ .vlt"), "vlt-17");
    item(&other, &gettext("HALD CLUT · 64³ .png"), "hald-64");
    menu.append_section(Some(gettext("Other formats").as_str()), &other);
    if with_identity {
        let tools = gio::Menu::new();
        item(&tools, &gettext("HALD identity · .png"), "identity-64");
        menu.append_section(Some(gettext("Tools").as_str()), &tools);
    }
    menu
}

fn wheel_mut(p: &mut Params, i: usize) -> &mut [f32; 4] {
    match i {
        0 => &mut p.lift,
        1 => &mut p.gamma,
        2 => &mut p.gain,
        _ => &mut p.offset,
    }
}

fn shortcuts_dialog() -> adw::ShortcutsDialog {
    let dialog = adw::ShortcutsDialog::new();
    let section = adw::ShortcutsSection::new(None);
    for (title, accel) in [
        (gettext("Open Photo"), "<Control>o"),
        (gettext("Export LUT"), "<Control>e"),
        (gettext("Before / after"), "backslash"),
        (gettext("Test chart"), "t"),
        (gettext("Grade view"), "<Alt>1"),
        (gettext("Compare LUTs view"), "<Alt>2"),
        (gettext("Search LUTs"), "<Control>f"),
        (gettext("Preferences"), "<Control>comma"),
        (gettext("Keyboard Shortcuts"), "<Control>question"),
    ] {
        section.add(adw::ShortcutsItem::new(&title, accel));
    }
    dialog.add(section);
    dialog
}

fn fill(template: &str, args: &[String]) -> String {
    let mut parts = template.split("{}");
    let mut out = parts.next().unwrap_or_default().to_string();
    for (part, arg) in parts.zip(args.iter().map(String::as_str).chain(std::iter::repeat(""))) {
        out += arg;
        out += part;
    }
    out
}

/// Installed: `<prefix>/bin/vibinglut` → `<prefix>/share/locale`; dev: `make` puts catalogs in `target/locale`.
fn locale_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.join("share/locale")))
        .filter(|d| d.join("it/LC_MESSAGES/vibinglut.mo").exists())
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/locale")))
}

/// What the Compare base is built from; look LUTs compare by identity, not content.
struct BaseKey {
    grade: Grade,
    luts: Vec<(Arc<Lut3d>, f32, Profile)>,
    input: Profile,
}

impl PartialEq for BaseKey {
    fn eq(&self, o: &Self) -> bool {
        let same = |a: &(Arc<Lut3d>, f32, Profile), b: &(Arc<Lut3d>, f32, Profile)| {
            Arc::ptr_eq(&a.0, &b.0) && a.1 == b.1 && a.2 == b.2
        };
        self.grade == o.grade
            && self.input == o.input
            && self.luts.len() == o.luts.len()
            && self.luts.iter().zip(&o.luts).all(|(a, b)| same(a, b))
    }
}

thread_local! {
    /// The window's input, for files opened through the application (see `main`).
    static OPEN: std::cell::OnceCell<relm4::Sender<Msg>> = const { std::cell::OnceCell::new() };
}

fn main() {
    // gtk_init calls setlocale; a missing catalog just leaves the English source strings.
    let _ = gettextrs::bindtextdomain("vibinglut", locale_dir());
    let _ = gettextrs::bind_textdomain_codeset("vibinglut", "UTF-8");
    let _ = gettextrs::textdomain("vibinglut");
    // HANDLES_OPEN: files from the command line or a file manager ("Open With")
    // arrive here, also when VibingLUT is already running.
    let app = adw::Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::HANDLES_OPEN).build();
    app.connect_open(|app, files, _| {
        if let Some(p) = files.first().and_then(|f| f.path()) {
            OPEN.with(|o| o.get().map(|s| s.emit(Msg::OpenPath(p))));
        }
        app.activate();
    });
    // When VibingLUT is already running this process only forwards the files,
    // and RelmApp::run then blocks on one last main-loop iteration: keep waking
    // the loop until we turn out to be the primary instance (startup).
    static STARTED: AtomicBool = AtomicBool::new(false);
    app.connect_startup(|_| {
        STARTED.store(true, Ordering::Relaxed);
        relm4::set_global_css(&format!("{CSS}{}", band_css()));
    });
    std::thread::spawn(|| {
        while !STARTED.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(100));
            glib::MainContext::default().wakeup();
        }
    });
    RelmApp::from_app(app).with_args(std::env::args().collect()).run::<App>(());
}
