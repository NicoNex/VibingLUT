//! VibingLUT GTK4/libadwaita front-end over `vibinglut-core`.

/// `gettext` with `{}` placeholders filled in order (translations may not reorder them).
macro_rules! gettext {
    ($msgid:literal, $($arg:expr),+ $(,)?) => {
        $crate::fill(&gettextrs::gettext($msgid), &[$($arg.to_string()),+])
    };
}

mod filmstrip;
mod lab_page;
mod presets_ui;
mod settings;
mod widgets;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use filmstrip::{Filmstrip, MAX_ROLL, ROLL_PX};
use gettextrs::gettext;
use lab_page::{LabAdd, LabKind, LabLayer, LabUi, Layers, caption};
use presets_ui::PresetsUi;
use relm4::adw::prelude::*;
use relm4::gtk::{gdk, gio, glib};
use relm4::{ComponentParts, ComponentSender, RelmApp, SimpleComponent, adw, gtk};
use vibinglut_core::chart::test_chart;
use vibinglut_core::gpu::{self, CompareBase, Done, Job};
use vibinglut_core::grade::{
    BAND_HUE_RANGE, BAND_HUES, Curve, Grade, HUE_HUE, HUE_SAT, LUM_SAT, LUMA, MASTER, MATCH_L, Params, Stage,
};
use vibinglut_core::lab::{Blend, Layer, Source, Stack, oklch_to_srgb};
use vibinglut_core::lut::{Format, Lut3d, LutEntry, MAX_LIBRARY};
use vibinglut_core::photo::{Decoded, Photo, RASTER_EXTS, RAW_EXTS, is_photo};
use vibinglut_core::preset::{self, Preset};
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
    CompareGraded(bool),
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
    Toast(String),
    /// Channel mixer: show output row / monochrome page, set a coefficient (delta from identity).
    MixerMono(bool),
    /// Channel mixer may change brightness (Preserve luminance off).
    MixerFree(bool),
    Mixer(usize, f64),
    /// Reference match: pick a file, it is decoded, then matched on the GPU.
    MatchChoose,
    MatchReference(PathBuf),
    /// The decoded reference, with its thumbnail as RGBA8.
    MatchLoaded(PathBuf, Result<(Arc<Photo>, Vec<u8>), String>),
    MatchClear,
    /// Preview aids: clipping stripes on the image, skin tone line on the vectorscope.
    Overlay(bool),
    SkinLine(bool),
    PresetsRefresh,
    PresetSave,
    PresetSaveAs(String),
    /// Writes a preset to the presets folder: `force` replaces one of the
    /// same name, `apply` loads it afterwards (imports).
    PresetWrite(Box<Preset>, bool, bool),
    PresetApply(PathBuf),
    PresetDelete(PathBuf),
    /// Puts back a deleted preset (its file and contents).
    PresetRestore(PathBuf, String),
    PresetImport,
    PresetExport,
    PresetExportTo(PathBuf),
    /// Undo of a preset load: the grade before it.
    RestoreGrade(Box<Grade>),
    /// Filmstrip: add photos, a photo decoded small (already encoded for
    /// `Profile`), re-encoded for a new profile, pick or drop one, grid view.
    RollAddPaths(Vec<PathBuf>),
    RollLoaded(PathBuf, Result<(Decoded, Profile, Arc<Photo>), String>),
    RollEncoded(Profile, Vec<(PathBuf, Arc<Photo>)>),
    /// The grade settled (debounced): render the filmstrip if this is the latest tick.
    RollTick(u64),
    RollSelect(usize),
    RollRemove(usize),
    Grid(bool),
    Gpu(Done),
}

/// A photo of the filmstrip: its file, a small decoded copy once read, and
/// that copy encoded for the profile it was last shown in.
struct RollItem {
    path: PathBuf,
    small: Option<Decoded>,
    encoded: Option<(Profile, Arc<Photo>)>,
}

/// Quiet time after the last grade change before the filmstrip re-renders.
const ROLL_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

/// The filmstrip copy of a decoded photo: small, and encoded for `profile`.
fn roll_copy(d: &Decoded, profile: Profile) -> (Decoded, Profile, Arc<Photo>) {
    let small = d.downscaled(ROLL_PX);
    let photo = small.encode(profile);
    (small, profile, photo)
}

/// A file's name for display.
fn file_label(path: &std::path::Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn stem_label(path: &std::path::Path) -> Option<String> {
    path.file_stem().map(|s| s.to_string_lossy().into_owned())
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
    presets: PresetsUi,
    filmstrip: Filmstrip,
    /// Single photo or the grid of the filmstrip.
    canvas_stack: gtk::Stack,
    grid_btn: gtk::ToggleButton,
    legend: gtk::Revealer,
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
    /// Compare LUTs on top of the Grade page's edits (off: on the untouched photo).
    compare_graded: bool,
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
    /// Clipping stripes on the Grade preview.
    overlay: bool,
    /// Filmstrip photos and the one being edited; `roll_gen` tags GPU frames.
    roll: Vec<RollItem>,
    active: Option<usize>,
    roll_gen: u64,
    /// Grade the filmstrip was last rendered with (None: render on the next
    /// change), and the latest debounce tick.
    roll_sent: Option<Grade>,
    roll_tick: u64,
    /// The match panel shows the reference file (false: matched by a preset, or no match).
    match_file: bool,
    /// Latest match request; results of older ones are dropped.
    match_gen: u64,
    /// The component's own input, for timers and worker threads.
    input: relm4::Sender<Msg>,
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
        let mut grade = self.graded();
        grade.params.overlay = self.overlay as u32;
        let req = Some((grade, self.split_value()));
        if self.has_source() && self.sent != req {
            self.sent = req;
            let (grade, split) = self.sent.clone().expect("set above");
            self.send(Job::Render(grade, split));
        }
        self.render_roll();
    }

    /// The filmstrip through the current grade (and look LUTs, applied by the
    /// worker). Overlay and split do not touch it, so only grade changes
    /// count, and only once a drag settles: the main preview never waits.
    fn render_roll(&mut self) {
        if self.roll.is_empty() || self.roll_sent.as_ref() == Some(&self.graded()) {
            return;
        }
        self.roll_tick += 1;
        let (s, tick) = (self.input.clone(), self.roll_tick);
        glib::timeout_add_local_once(ROLL_SETTLE, move || s.emit(Msg::RollTick(tick)));
    }

    /// Hands the filmstrip to the GPU; `layout` when photos were added or
    /// removed (frame indices changed). Not yet decoded: a black frame.
    fn upload_roll(&mut self, layout: bool) {
        if layout {
            self.roll_gen += 1;
        }
        let blank = Arc::new(Photo { width: 1, height: 1, pixels: vec![[0.0, 0.0, 0.0, 1.0]] });
        let photos =
            self.roll.iter().map(|it| it.encoded.as_ref().map_or_else(|| blank.clone(), |e| e.1.clone())).collect();
        self.send(Job::SetRoll(self.roll_gen, photos));
        self.roll_sent = None;
        self.render_roll();
    }

    /// Re-encodes, off the main thread, the strip photos shown in another profile.
    fn reencode_roll(&self) {
        let profile = self.profile;
        let stale: Vec<(PathBuf, Decoded)> = self
            .roll
            .iter()
            .filter(|it| it.encoded.as_ref().is_none_or(|e| e.0 != profile))
            .filter_map(|it| Some((it.path.clone(), it.small.clone()?)))
            .collect();
        if stale.is_empty() {
            return;
        }
        let s = self.input.clone();
        std::thread::spawn(move || {
            let encoded = stale.into_iter().map(|(p, small)| (p, small.encode(profile))).collect();
            s.emit(Msg::RollEncoded(profile, encoded));
        });
    }

    /// Takes photo `i` out of the strip; true if it was the one being edited.
    fn drop_from_roll(&mut self, i: usize, sender: &ComponentSender<Self>) -> bool {
        self.roll.remove(i);
        let was_active = self.active == Some(i);
        self.active = self.active.and_then(|a| if a > i { Some(a - 1) } else { (a < i).then_some(a) });
        self.roll_changed(sender);
        was_active
    }

    /// Shows the match panel as the grade has it.
    fn show_match_panel(&self) {
        let matched = self.grade.params.match_amount > 0.0;
        self.ui.sidebar.matching.stack.set_visible_child_name(if matched { "set" } else { "empty" });
    }

    /// Drops any in-flight match.
    fn cancel_match(&mut self) {
        self.match_gen += 1;
        self.show_match_panel();
    }

    /// Matches the photo on screen (as decoded, before any grading) to
    /// `reference`, from its small filmstrip copy when there is one.
    fn request_match(&mut self, reference: Arc<Photo>) {
        let small = self.active.and_then(|i| self.roll.get(i)).and_then(|it| it.encoded.clone());
        let source = match (small, self.active()) {
            (Some((profile, photo)), Some((_, shown))) if profile == *shown && !self.show_chart => photo,
            (_, Some((photo, _))) => photo.clone(),
            (_, None) => return self.toast(&no_photo_to_match()),
        };
        self.match_gen += 1;
        self.ui.sidebar.matching.stack.set_visible_child_name("busy");
        self.send(Job::Match { generation: self.match_gen, source, reference, input: self.grade.neutralized() });
    }

    /// Rebuilds the filmstrip cards after photos were added or removed.
    fn roll_changed(&mut self, sender: &ComponentSender<Self>) {
        let names: Vec<String> = self.roll.iter().map(|it| file_label(&it.path)).collect();
        self.ui.filmstrip.rebuild(&names, self.active, sender);
        self.ui.grid_btn.set_sensitive(self.roll.len() > 1);
        if self.roll.len() < 2 && self.ui.grid_btn.is_active() {
            self.ui.grid_btn.set_active(false);
        }
        self.upload_roll(true);
    }

    /// Opens filmstrip photo `i` for editing (full-size decode off the main thread).
    fn load_active(&mut self, i: usize, sender: &ComponentSender<Self>) {
        let Some(path) = self.roll.get(i).map(|it| it.path.clone()) else { return };
        self.active = Some(i);
        self.ui.filmstrip.set_active(self.active);
        // ponytail: a failed save only loses the dialog's start folder.
        if let Some(dir) = path.parent() {
            let _ = settings::set_folder(settings::PHOTO_FOLDER, dir);
        }
        self.ui.grade_stack.set_visible_child_name(if self.has_source() { "image" } else { "loading" });
        self.ui.info.set_label(&gettext("Loading…"));
        self.load_gen += 1;
        let (s, generation, profile) = (sender.input_sender().clone(), self.load_gen, self.profile);
        std::thread::spawn(move || {
            let r = Decoded::load(&path);
            // The strip copy comes from this decode (RollAddPaths skips it), even if this load is superseded.
            let small = r.as_ref().map(|d| roll_copy(d, profile)).map_err(String::clone);
            s.emit(Msg::RollLoaded(path.clone(), small));
            s.emit(Msg::Loaded(generation, path, r));
        });
    }

    /// Replaces the grade (preset, undo, reset) and shows it in every control.
    /// The input decoding stays: it belongs to the photo, not the grade.
    fn apply_grade(&mut self, mut grade: Grade) {
        (grade.params.input, grade.params.gamut) = (self.grade.params.input, self.grade.params.gamut);
        self.grade = grade;
        self.cancel_match();
        let p = self.grade.params;
        let sb = &self.ui.sidebar;
        // Each control echoes its value back as a message; the values already
        // match the grade, so the echoes change nothing.
        for (s, field) in &sb.sliders {
            s.adj.set_value(*field(&mut p.clone()) as f64);
        }
        for (i, (w, luma)) in sb.wheels.iter().enumerate() {
            let v = *wheel_mut(&mut p.clone(), i);
            w.set(v[0] as f64, v[1] as f64);
            luma.adj.set_value(v[2] as f64);
        }
        sb.curves.set_all(&self.grade.curves);
        sb.hue_curves.set_all(&self.grade.curves);
        sb.mixer.mono.set_active(p.mixer_mono > 0.5);
        sb.mixer.keep.set_active(p.mixer_free < 0.5);
        for (s, i, base) in &sb.mixer.sliders {
            s.adj.set_value(base + mixer_value(&p, *i) as f64);
        }
        sb.matching.amount.adj.set_value(p.match_amount as f64);
        if p.match_amount <= 0.0 {
            self.match_file = false;
        }
        if !self.match_file {
            sb.matching.pic.set_paintable(None::<&gdk::Paintable>);
            sb.matching.name.set_label(&gettext("Matched by a preset"));
        }
        self.show_band(self.band);
        self.sent = None;
        self.render();
    }

    /// Points the three selective-colour sliders at band `b`.
    fn show_band(&mut self, b: usize) {
        self.band = b;
        // The resulting Hue echoes carry unchanged values, so render() skips them.
        let p = self.grade.params;
        for (k, (s, v)) in self.ui.sidebar.hue.iter().zip([p.hue_shift[b], p.hue_sat[b], p.hue_lum[b]]).enumerate() {
            for i in 0..BAND_HUES.len() {
                s.scale.remove_css_class(&format!("{}-{i}", BAND_TRACKS[k]));
            }
            s.scale.add_css_class(&format!("{}-{b}", BAND_TRACKS[k]));
            s.adj.set_value(v as f64);
        }
    }

    fn presets(&self) -> Vec<(Preset, PathBuf)> {
        preset::list(&settings::presets_dir())
    }

    /// Toast with an Undo button that sends `undo()`.
    fn undo_toast(&self, title: &str, undo: impl Fn() -> Msg + 'static) {
        let toast = adw::Toast::builder().title(title).button_label(gettext("Undo")).timeout(6).build();
        let s = self.input.clone();
        toast.connect_button_clicked(move |_| s.emit(undo()));
        self.ui.toasts.add_toast(toast);
    }

    /// The grade as seen (switched-off sections stay off), as a preset.
    fn current_preset(&self, name: String) -> Preset {
        Preset { name, grade: self.graded().neutral_input() }
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
        let (grade, looks) = if self.compare_graded {
            (self.graded(), self.look_layers())
        } else {
            (self.grade.neutralized(), Vec::new())
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
                (self.sent, self.roll_sent) = (None, None);
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
    section_with(title, child, None, true)
}

/// Section with an eye toggle that switches it off (`which` = None: the LUTs
/// after the grade). Advanced sections start `open: false`, collapsed.
fn bypassable(
    title: &str,
    child: &impl IsA<gtk::Widget>,
    which: Option<Stage>,
    open: bool,
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
    section_with(title, child, Some(&eye), open)
}

fn section_with(title: &str, child: &impl IsA<gtk::Widget>, eye: Option<&gtk::ToggleButton>, open: bool) -> gtk::Box {
    let arrow = gtk::Image::from_icon_name(if open { "pan-down-symbolic" } else { "pan-end-symbolic" });
    let head = gtk::Box::builder().spacing(6).build();
    head.append(&gtk::Label::builder().label(title).css_classes(["heading"]).xalign(0.0).hexpand(true).build());
    head.append(&arrow);
    let button = gtk::Button::builder().child(&head).css_classes(["flat", "section-header"]).build();
    button.update_property(&[gtk::accessible::Property::Label(title)]);
    let revealer = gtk::Revealer::builder()
        .child(child)
        .reveal_child(open)
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .build();
    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["card", "section"]).build();
    if !open {
        card.add_css_class("collapsed");
    }
    let (r, c) = (revealer.clone(), card.clone());
    button.update_state(&[gtk::accessible::State::Expanded(Some(open))]);
    button.connect_clicked(move |b| {
        let open = !r.reveals_child();
        b.update_state(&[gtk::accessible::State::Expanded(Some(open))]);
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
scale.track-ch0 trough { background-image: linear-gradient(to right, #1b1b1b, #e5534b); }
scale.track-ch1 trough { background-image: linear-gradient(to right, #1b1b1b, #57c46a); }
scale.track-ch2 trough { background-image: linear-gradient(to right, #1b1b1b, #5b8def); }
scale.track-film-sat trough { background-image: linear-gradient(to right, #8a8580, #b34a3c, #5a1f2a); }
scale.track-film-density trough { background-image: linear-gradient(to right, #d9774f, #7a2c22); }
scale.track-ch0 trough highlight, scale.track-ch1 trough highlight, scale.track-ch2 trough highlight,
scale.track-film-sat trough highlight, scale.track-film-density trough highlight { background: none; }
.reference { border-radius: 8px; }
.canvas-tools { border-radius: 10px; }
.clip-swatch { min-width: 10px; min-height: 10px; border-radius: 3px; }
.clip-hi { background-color: #ff2929; }
.clip-lo { background-color: #2e6bff; }
.clip-gamut { background-color: #ffdb1a; }
.filmstrip { background-color: color-mix(in srgb, var(--view-bg-color) 55%, black);
  border-top: 1px solid color-mix(in srgb, var(--borders-color) 60%, transparent); }
.roll-card, .grid-card { padding: 4px; border-radius: 10px; }
.roll-card picture, .grid-card picture { border-radius: 6px; }
.roll-card.active, .grid-card.active { box-shadow: inset 0 0 0 2px var(--accent-color);
  background-color: color-mix(in srgb, var(--accent-bg-color) 18%, transparent); }
.roll-remove { min-width: 22px; min-height: 22px; padding: 0; opacity: 0; transition: opacity 150ms ease; }
.roll-item:hover .roll-remove, .roll-remove:focus-visible { opacity: 1; }
@media (prefers-contrast: more) {
  .roll-card.active, .grid-card.active { box-shadow: inset 0 0 0 3px currentColor; }
  .roll-remove { opacity: 1; }
}
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

/// Channel mixer controls: one page of three sliders per output channel,
/// and one for black & white, all views of `Params::mixer`.
struct MixerUi {
    rows: adw::ToggleGroup,
    pages: gtk::Stack,
    /// Output R, G, B rows, then the monochrome filter: (slider, `mixer_value` index, base).
    sliders: Vec<(Slider, usize, f64)>,
    mono: adw::SwitchRow,
    keep: adw::SwitchRow,
}

/// Reference match: empty state, or the reference with its strength.
struct MatchUi {
    stack: gtk::Stack,
    pic: gtk::Picture,
    name: gtk::Label,
    amount: Slider,
}

struct Sidebar {
    scroll: gtk::ScrolledWindow,
    sliders: Vec<(Slider, Field)>,
    wheels: Vec<(ColorWheel, Slider)>,
    hue: [Slider; 3],
    curves: CurveEditor,
    hue_curves: CurveEditor,
    mixer: MixerUi,
    matching: MatchUi,
    scopes: [gtk::Picture; 3],
    skin: gtk::DrawingArea,
    skin_btn: gtk::ToggleButton,
    looks: gtk::ListBox,
}

/// Curve editor with a channel selector and a reset button.
fn curve_panel(editor: &CurveEditor, names: &[(&str, &str)], hint: &str, sender: &ComponentSender<App>) -> gtk::Box {
    let sel = toggles(names);
    let ce = editor.clone();
    sel.connect_active_name_notify(move |g| {
        if let Some(ch) = g.active_name().and_then(|n| n.parse().ok()) {
            ce.set_active(ch);
        }
    });
    let reset =
        gtk::Button::builder().label(gettext("Reset curve")).css_classes(["flat"]).halign(gtk::Align::End).build();
    let (ce, snd) = (editor.clone(), sender.clone());
    reset.connect_clicked(move |_| {
        let (ch, c) = ce.reset_active();
        snd.input(Msg::Curve(ch, c));
    });
    let bx = vbox(8);
    bx.append(&sel);
    bx.append(&editor.area);
    bx.append(&caption(hint));
    bx.append(&reset);
    bx
}

fn build_mixer(sender: &ComponentSender<App>) -> (gtk::Box, MixerUi) {
    let names = [gettext("Red"), gettext("Green"), gettext("Blue")];
    let rows = toggles(&[("0", names[0].as_str()), ("1", names[1].as_str()), ("2", names[2].as_str())]);
    let pages = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vhomogeneous(false).build();
    let mut sliders = Vec::new();
    // Output rows: 100% of their own channel by default.
    for row in 0..3 {
        let page = vbox(4);
        for (col, name) in names.iter().enumerate() {
            let base = if row == col { 100.0 } else { 0.0 };
            let s = Slider::new(name, -200.0, 200.0, base, 0, Some(&format!("track-ch{col}")));
            let (snd, i) = (sender.clone(), row * 3 + col);
            s.connect(move |v| snd.input(Msg::Mixer(i, v - base)));
            page.append(&s.root);
            sliders.push((s, i, base));
        }
        pages.add_named(&page, Some(&row.to_string()));
    }
    // Black & white: luminance weights by default, plus a colour filter.
    let mono_page = vbox(4);
    for (col, w) in LUMA.into_iter().enumerate() {
        let base = (w as f64 * 1000.0).round() / 10.0;
        let s = Slider::new(&names[col], -200.0, 200.0, base, 0, Some(&format!("track-ch{col}")));
        let snd = sender.clone();
        s.connect(move |v| snd.input(Msg::Mixer(9 + col, v - base)));
        mono_page.append(&s.root);
        sliders.push((s, 9 + col, base));
    }
    mono_page.append(&caption(&gettext(
        "Like a colour filter on black & white film: more red brightens skin and darkens skies.",
    )));
    pages.add_named(&mono_page, Some("mono"));
    let p = pages.clone();
    rows.connect_active_name_notify(move |g| {
        if let Some(n) = g.active_name() {
            p.set_visible_child_name(&n);
        }
    });

    let mono = adw::SwitchRow::builder().title(gettext("Black & white")).use_markup(false).build();
    let keep = adw::SwitchRow::builder()
        .title(gettext("Preserve luminance"))
        .subtitle(gettext("The mix changes colour, not brightness"))
        .active(true)
        .build();
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    list.append(&mono);
    list.append(&keep);
    let snd = sender.clone();
    mono.connect_active_notify(move |m| snd.input(Msg::MixerMono(m.is_active())));
    let snd = sender.clone();
    keep.connect_active_notify(move |k| snd.input(Msg::MixerFree(!k.is_active())));

    let bx = vbox(8);
    bx.append(&list);
    bx.append(&rows);
    bx.append(&pages);
    bx.append(&caption(&gettext(
        "Each output channel is a mix of the scene's red, green and blue; 100% of its own channel is no change.",
    )));
    (bx, MixerUi { rows, pages, sliders, mono, keep })
}

fn build_match(sender: &ComponentSender<App>) -> MatchUi {
    let choose = |label: String| {
        let b = gtk::Button::builder().label(label).css_classes(["pill"]).halign(gtk::Align::Center).build();
        let snd = sender.clone();
        b.connect_clicked(move |_| snd.input(Msg::MatchChoose));
        b
    };
    let empty = vbox(10);
    empty.append(&caption(&gettext(
        "Make this photo look like a reference: a film still, a photo you love. Tones and colours are matched; refine the rest with the other tools.",
    )));
    empty.append(&choose(gettext("Choose Reference…")));

    let busy = adw::Spinner::builder().height_request(32).margin_top(12).margin_bottom(12).build();

    let pic = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .height_request(110)
        .css_classes(["reference"])
        .build();
    pic.update_property(&[gtk::accessible::Property::Label(&gettext("Reference photo"))]);
    let name =
        gtk::Label::builder().ellipsize(gtk::pango::EllipsizeMode::Middle).css_classes(["caption"]).xalign(0.0).build();
    let amount = Slider::new(&gettext("Strength"), 0.0, 100.0, 100.0, 0, None);
    let snd = sender.clone();
    amount.connect(move |v| snd.input(Msg::Set(|p| &mut p.match_amount, v)));
    let remove = gtk::Button::builder().label(gettext("Remove")).css_classes(["flat"]).build();
    let snd = sender.clone();
    remove.connect_clicked(move |_| snd.input(Msg::MatchClear));
    let actions = gtk::Box::builder().spacing(6).halign(gtk::Align::End).build();
    let change = choose(gettext("Change…"));
    change.remove_css_class("pill");
    change.add_css_class("flat");
    actions.append(&change);
    actions.append(&remove);
    let set = vbox(6);
    set.append(&pic);
    set.append(&name);
    set.append(&amount.root);
    set.append(&actions);

    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vhomogeneous(false).build();
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&busy, Some("busy"));
    stack.add_named(&set, Some("set"));
    MatchUi { stack, pic, name, amount }
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
    let skin = widgets::skin_line();
    skin.set_visible(false);
    let vec_layer = gtk::Overlay::builder().child(&scope_pics[2]).build();
    vec_layer.add_overlay(&skin);
    let scope_stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
    scope_stack.add_named(&scope_pics[0], Some("hist"));
    scope_stack.add_named(&scope_pics[1], Some("wave"));
    scope_stack.add_named(&vec_layer, Some("vec"));
    let scope_sel = toggles(&[
        ("hist", gettext("Histogram").as_str()),
        ("wave", gettext("Waveform").as_str()),
        ("vec", gettext("Vectorscope").as_str()),
    ]);
    scope_sel.set_hexpand(true);
    let skin_btn = gtk::ToggleButton::builder()
        .icon_name("avatar-default-symbolic")
        .tooltip_text(gettext("Skin tone line (K)"))
        .css_classes(["flat", "circular"])
        .valign(gtk::Align::Center)
        .visible(false)
        .build();
    skin_btn.update_property(&[gtk::accessible::Property::Label(&gettext("Skin tone line"))]);
    let snd = sender.clone();
    skin_btn.connect_toggled(move |b| snd.input(Msg::SkinLine(b.is_active())));
    let (st, sb) = (scope_stack.clone(), skin_btn.clone());
    scope_sel.connect_active_name_notify(move |g| {
        if let Some(n) = g.active_name() {
            st.set_visible_child_name(&n);
            sb.set_visible(n == "vec");
        }
    });
    let scope_head = gtk::Box::builder().spacing(6).build();
    scope_head.append(&scope_sel);
    scope_head.append(&skin_btn);
    let scopes_box = vbox(8);
    scopes_box.append(&scope_head);
    scopes_box.append(&scope_stack);
    panel.append(&section(&gettext("Scopes"), &scopes_box));

    // Input profile: the signal the LUT will receive in camera.
    let profile_box = vbox(6);
    let profile_sel = lab_page::profile_dropdown(Profile::Standard);
    profile_sel.update_property(&[gtk::accessible::Property::Label(&gettext("Input profile"))]);
    let hint = caption(&profile_hint(Profile::Standard));
    let (snd, h) = (sender.clone(), hint.clone());
    profile_sel.connect_selected_notify(move |d| {
        let profile = Profile::ALL[d.selected() as usize];
        h.set_label(&profile_hint(profile));
        snd.input(Msg::Profile(profile));
    });
    profile_box.append(&profile_sel);
    profile_box.append(&hint);
    panel.append(&section(&gettext("Input profile"), &profile_box));

    // Reference match comes first: the rest refines it.
    let matching = build_match(sender);
    panel.append(&bypassable(&gettext("Reference match"), &matching.stack, Some(Stage::Match), false, sender));

    let mut sliders = Vec::new();
    let mut add = |bx: &gtk::Box, label: &str, min: f64, max: f64, digits: u32, track: Option<&str>, field: Field| {
        let s = Slider::new(label, min, max, 0.0, digits, track);
        let snd = sender.clone();
        s.connect(move |v| snd.input(Msg::Set(field, v)));
        bx.append(&s.root);
        sliders.push((s, field));
    };

    let wb = vbox(4);
    add(&wb, &gettext("Temperature"), -100.0, 100.0, 0, Some("track-temp"), |p| &mut p.temperature);
    add(&wb, &gettext("Tint"), -100.0, 100.0, 0, Some("track-tint"), |p| &mut p.tint);
    panel.append(&bypassable(&gettext("White balance"), &wb, Some(Stage::WhiteBalance), true, sender));

    let tone = vbox(4);
    add(&tone, &gettext("Exposure"), -3.0, 3.0, 2, None, |p| &mut p.exposure);
    add(&tone, &gettext("Contrast"), -100.0, 100.0, 0, None, |p| &mut p.contrast);
    add(&tone, &gettext("Highlights"), -100.0, 100.0, 0, None, |p| &mut p.highlights);
    add(&tone, &gettext("Shadows"), -100.0, 100.0, 0, None, |p| &mut p.shadows);
    add(&tone, &gettext("Whites"), -100.0, 100.0, 0, None, |p| &mut p.whites);
    add(&tone, &gettext("Blacks"), -100.0, 100.0, 0, None, |p| &mut p.blacks);
    panel.append(&bypassable(&gettext("Tone"), &tone, Some(Stage::Tone), true, sender));

    let (mixer_box, mixer) = build_mixer(sender);
    panel.append(&bypassable(&gettext("Channel mixer"), &mixer_box, Some(Stage::Mixer), false, sender));

    let color = vbox(4);
    add(&color, &gettext("Saturation"), -100.0, 100.0, 0, None, |p| &mut p.saturation);
    add(&color, &gettext("Vibrance"), -100.0, 100.0, 0, None, |p| &mut p.vibrance);
    add(&color, &gettext("Shadow saturation"), -100.0, 100.0, 0, None, |p| &mut p.sat_shadows);
    add(&color, &gettext("Highlight saturation"), -100.0, 100.0, 0, None, |p| &mut p.sat_highlights);
    panel.append(&bypassable(&gettext("Color"), &color, Some(Stage::Color), true, sender));

    let film = vbox(4);
    add(&film, &gettext("Subtractive saturation"), -100.0, 100.0, 0, Some("track-film-sat"), |p| &mut p.film_sat);
    add(&film, &gettext("Color density"), 0.0, 100.0, 0, Some("track-film-density"), |p| &mut p.film_density);
    film.append(&caption(&gettext(
        "Film makes colour by adding dye, so richer colours are also deeper. Subtractive saturation saturates the way dye does; density deepens saturated colours. Greys are untouched.",
    )));
    panel.append(&bypassable(&gettext("Film color"), &film, Some(Stage::Film), true, sender));

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
    panel.append(&bypassable(&gettext("Selective color"), &hsl, Some(Stage::Selective), true, sender));

    let snd = sender.clone();
    let hue_curves = CurveEditor::new(HUE_HUE..LUM_SAT + 1, move |ch, c| snd.input(Msg::Curve(ch, c)));
    let hue_box = curve_panel(
        &hue_curves,
        &[
            (&HUE_HUE.to_string(), gettext("Hue vs Hue").as_str()),
            (&HUE_SAT.to_string(), gettext("Hue vs Sat").as_str()),
            (&LUM_SAT.to_string(), gettext("Lum vs Sat").as_str()),
        ],
        &gettext(
            "The middle line is no change. Pull a hue up or down to shift it, saturate or desaturate it; Lum vs Sat works by brightness.",
        ),
        sender,
    );
    panel.append(&bypassable(&gettext("Hue curves"), &hue_box, Some(Stage::HueCurves), false, sender));

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
    panel.append(&bypassable(&gettext("Primary color wheels"), &wheels_row, Some(Stage::Wheels), true, sender));

    // Curves
    let snd = sender.clone();
    let curves = CurveEditor::new(MASTER..HUE_HUE, move |ch, c| snd.input(Msg::Curve(ch, c)));
    let curve_box = curve_panel(
        &curves,
        &[
            ("0", "Luma"),
            ("1", gettext("Red").as_str()),
            ("2", gettext("Green").as_str()),
            ("3", gettext("Blue").as_str()),
        ],
        &gettext("Click to add a point, double-click or right-click to remove it"),
        sender,
    );
    panel.append(&bypassable(&gettext("Curves"), &curve_box, Some(Stage::Curves), true, sender));

    // LUTs after the grade
    let look_box = vbox(8);
    let looks = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    let empty = caption(&gettext("No LUTs: the grade is exported as is."));
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
    look_box.append(&caption(&gettext("Applied after the grade, top to bottom, and included in the exported LUT.")));
    panel.append(&bypassable(&gettext("LUTs"), &look_box, None, true, sender));

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
    Sidebar {
        scroll,
        sliders,
        wheels,
        hue,
        curves,
        hue_curves,
        mixer,
        matching,
        scopes: scope_pics,
        skin,
        skin_btn,
        looks,
    }
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
        // Preview aids, top right: clipping stripes and the filmstrip grid.
        let clip_btn = gtk::ToggleButton::builder()
            .icon_name("dialog-warning-symbolic")
            .tooltip_text(gettext("Show clipping (C)"))
            .build();
        clip_btn.update_property(&[gtk::accessible::Property::Label(&gettext("Show clipping"))]);
        let snd = sender.clone();
        clip_btn.connect_toggled(move |b| snd.input(Msg::Overlay(b.is_active())));
        let grid_btn = gtk::ToggleButton::builder()
            .icon_name("view-grid-symbolic")
            .tooltip_text(gettext("All photos in the filmstrip (G)"))
            .sensitive(false)
            .build();
        grid_btn.update_property(&[gtk::accessible::Property::Label(&gettext("Grid of all photos"))]);
        let snd = sender.clone();
        grid_btn.connect_toggled(move |b| snd.input(Msg::Grid(b.is_active())));
        let tools = gtk::Box::builder()
            .css_classes(["linked", "osd", "canvas-tools"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .margin_top(12)
            .margin_end(12)
            .build();
        tools.append(&clip_btn);
        tools.append(&grid_btn);
        // What the stripes mean, shown while they are.
        let legend_box = gtk::Box::builder().spacing(12).css_classes(["osd", "osd-info"]).build();
        for (class, label) in [
            ("clip-hi", gettext("Clipped highlights")),
            ("clip-lo", gettext("Crushed blacks")),
            ("clip-gamut", gettext("Out of gamut")),
        ] {
            let item = gtk::Box::builder().spacing(6).build();
            item.append(&gtk::Box::builder().css_classes(["clip-swatch", class]).valign(gtk::Align::Center).build());
            item.append(&gtk::Label::new(Some(&label)));
            legend_box.append(&item);
        }
        let legend = gtk::Revealer::builder()
            .child(&legend_box)
            .transition_type(gtk::RevealerTransitionType::Crossfade)
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .margin_end(16)
            .margin_bottom(16)
            .build();
        canvas.add_overlay(&legend);
        let filmstrip = Filmstrip::new(&sender);
        let canvas_stack =
            gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vexpand(true).build();
        canvas_stack.add_named(&canvas, Some("single"));
        canvas_stack.add_named(&filmstrip.grid_root, Some("grid"));
        let stage = gtk::Overlay::builder().child(&canvas_stack).build();
        stage.add_overlay(&tools);
        let image_page = vbox(0);
        image_page.append(&stage);
        image_page.append(&filmstrip.root);
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
                &gettext("Drop JPEGs or camera RAWs here, or choose files. Open several to grade them together.\nThe grade becomes a .cube LUT for the Standard profile."),
                &gettext("Open Photo…"),
                || Msg::Open,
            ),
            Some("empty"),
        );
        grade_stack.add_named(&loading, Some("loading"));
        grade_stack.add_named(&image_page, Some("image"));

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
        folder_row.append(&folder_btn);
        // What the candidates are applied to: the untouched photo, or the Grade page's result.
        let base_sel =
            toggles(&[("original", gettext("Original").as_str()), ("graded", gettext("With grade").as_str())]);
        base_sel.set_active_name(Some("original"));
        base_sel.set_tooltip_text(Some(&gettext("Apply the LUTs to the untouched photo or to it as edited in Grade")));
        base_sel.set_margin_start(8);
        base_sel.set_margin_end(8);
        base_sel.set_margin_bottom(6);
        let snd = sender.clone();
        base_sel.connect_active_name_notify(move |g| {
            snd.input(Msg::CompareGraded(g.active_name().as_deref() == Some("graded")))
        });
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
        gallery.append(&base_sel);
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
        let presets = presets_ui::build(&sender);
        header.pack_start(&presets.button);

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
            .use_markup(false)
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
        for (accel, b) in [("c", clip_btn.clone()), ("g", grid_btn.clone()), ("k", sidebar.skin_btn.clone())] {
            shortcut(accel, Box::new(move || b.set_active(b.is_sensitive() && !b.is_active())));
        }
        let snd = sender.clone();
        shortcut("<Control>s", Box::new(move || snd.input(Msg::PresetSave)));
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
            let Ok(list) = v.get::<gdk::FileList>() else { return false };
            let paths: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
            if paths.is_empty() {
                return false;
            }
            snd.input(Msg::RollAddPaths(paths));
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
            compare_graded: false,
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
            overlay: false,
            roll: Vec::new(),
            active: None,
            roll_gen: 0,
            roll_sent: None,
            roll_tick: 0,
            match_file: false,
            match_gen: 0,
            input: sender.input_sender().clone(),
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
                presets,
                filmstrip,
                canvas_stack,
                grid_btn,
                legend,
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
                let dialog = photo_dialog(&gettext("Open Photo"));
                let s = sender.clone();
                dialog.open_multiple(Some(&self.ui.window), gio::Cancellable::NONE, move |r| {
                    let Ok(files) = r else { return };
                    let paths: Vec<PathBuf> =
                        files.iter::<gio::File>().filter_map(|f| f.ok().and_then(|f| f.path())).collect();
                    if !paths.is_empty() {
                        s.input(Msg::RollAddPaths(paths));
                    }
                });
            }
            Msg::RollAddPaths(paths) => {
                // Already in the strip: just show it. New ones are appended, the
                // first of them is opened, and all are decoded small in one thread.
                let (photos, others): (Vec<PathBuf>, Vec<PathBuf>) = paths.into_iter().partition(|p| is_photo(p));
                if !others.is_empty() {
                    self.toast(&gettext!("{} files skipped: not JPEG, TIFF, PNG or camera RAW", others.len()));
                }
                let mut fresh = Vec::new();
                let mut show = None;
                for p in photos {
                    match self.roll.iter().position(|it| it.path == p) {
                        Some(i) => show = show.or(Some(i)),
                        None if self.roll.len() < MAX_ROLL => {
                            show = show.or(Some(self.roll.len()));
                            self.roll.push(RollItem { path: p.clone(), small: None, encoded: None });
                            fresh.push(p);
                        }
                        None => {
                            self.toast(&gettext!("The filmstrip holds up to {} photos", MAX_ROLL));
                            break;
                        }
                    }
                }
                let open = show.filter(|&i| Some(i) != self.active || !self.has_source());
                if !fresh.is_empty() {
                    self.roll_changed(&sender);
                    // The photo about to be opened gets its small copy from that decode.
                    let opening = open.map(|i| self.roll[i].path.clone());
                    fresh.retain(|p| Some(p) != opening.as_ref());
                    let (s, profile) = (sender.input_sender().clone(), self.profile);
                    std::thread::spawn(move || {
                        for p in fresh {
                            let small = Decoded::load(&p).map(|d| roll_copy(&d, profile));
                            s.emit(Msg::RollLoaded(p, small));
                        }
                    });
                }
                if let Some(i) = open {
                    self.load_active(i, &sender);
                }
            }
            Msg::RollLoaded(path, Ok((small, profile, photo))) => {
                if let Some(it) = self.roll.iter_mut().find(|it| it.path == path) {
                    (it.small, it.encoded) = (Some(small), Some((profile, photo)));
                    self.upload_roll(false);
                    // Decoded while the profile changed: show it in the new one.
                    if profile != self.profile {
                        self.reencode_roll();
                    }
                }
            }
            Msg::RollEncoded(profile, photos) => {
                for (path, photo) in photos {
                    if let Some(it) = self.roll.iter_mut().find(|it| it.path == path) {
                        it.encoded = Some((profile, photo));
                    }
                }
                self.upload_roll(false);
            }
            Msg::RollTick(tick) => {
                let grade = self.graded();
                if tick == self.roll_tick && self.roll_sent.as_ref() != Some(&grade) {
                    self.send(Job::RenderRoll(grade.clone()));
                    self.roll_sent = Some(grade);
                }
            }
            Msg::RollLoaded(path, Err(e)) => {
                // An unreadable photo leaves the strip instead of staying a black card.
                if let Some(i) = self.roll.iter().position(|it| it.path == path) {
                    self.drop_from_roll(i, &sender);
                }
                self.toast(&gettext!("Cannot open {}: {}", path.display(), e));
            }
            Msg::RollSelect(i) => {
                if Some(i) != self.active {
                    self.load_active(i, &sender);
                }
            }
            Msg::RollRemove(i) => {
                if i >= self.roll.len() {
                    return;
                }
                let was_active = self.drop_from_roll(i, &sender);
                if was_active && self.roll.is_empty() {
                    // Nothing left to grade: back to the empty state; a decode in flight is stale.
                    self.load_gen += 1;
                    self.cancel_match();
                    (self.decoded, self.photo, self.path) = (None, None, None);
                    self.ui.window.set_title(Some("VibingLUT"));
                    self.show_source();
                } else if was_active {
                    self.load_active(i.min(self.roll.len() - 1), &sender);
                }
            }
            Msg::Grid(on) => {
                self.ui.canvas_stack.set_visible_child_name(if on { "grid" } else { "single" });
                if self.ui.grid_btn.is_active() != on {
                    self.ui.grid_btn.set_active(on);
                }
            }
            Msg::Loaded(generation, ..) if generation != self.load_gen => {}
            Msg::Loaded(_, path, Ok(decoded)) => {
                self.ui.window.set_title(Some(&format!("{} — VibingLUT", file_label(&path))));
                self.decoded = Some(decoded);
                self.path = Some(path);
                // Opening a photo means you want to see it.
                if self.show_chart {
                    self.show_chart = false;
                    self.ui.chart_btn.set_active(false);
                }
                self.encode_photo(&sender);
            }
            // RollLoaded already reported the error and took the photo out of the
            // strip; the photo still on screen (if any) stays the one edited.
            Msg::Loaded(_, _, Err(_)) => {
                self.active = self.path.as_ref().and_then(|p| self.roll.iter().position(|it| it.path == *p));
                self.ui.filmstrip.set_active(self.active);
                self.show_source();
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
            Msg::Band(b) => self.show_band(b),
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
                let before = self.grade.clone();
                self.apply_grade(Grade::default());
                self.undo_toast(&gettext("Grade reset"), move || Msg::RestoreGrade(Box::new(before.clone())));
            }
            Msg::RestoreGrade(grade) => self.apply_grade(*grade),
            Msg::MixerMono(on) => {
                self.grade.params.mixer_mono = if on { 1.0 } else { 0.0 };
                let m = &self.ui.sidebar.mixer;
                m.rows.set_sensitive(!on);
                let page = m.rows.active_name().map(|n| n.to_string()).unwrap_or_else(|| "0".into());
                m.pages.set_visible_child_name(if on { "mono" } else { &page });
                self.render();
            }
            Msg::MixerFree(free) => {
                self.grade.params.mixer_free = if free { 1.0 } else { 0.0 };
                self.render();
            }
            Msg::Mixer(i, delta) => {
                *mixer_value_mut(&mut self.grade.params, i) = delta as f32;
                self.render();
            }
            Msg::MatchChoose => {
                if !self.has_source() {
                    return self.toast(&no_photo_to_match());
                }
                let dialog = photo_dialog(&gettext("Choose Reference"));
                let s = sender.clone();
                dialog.open(Some(&self.ui.window), gio::Cancellable::NONE, move |r| {
                    if let Some(p) = r.ok().and_then(|f| f.path()) {
                        s.input(Msg::MatchReference(p));
                    }
                });
            }
            Msg::MatchReference(path) => {
                self.ui.sidebar.matching.stack.set_visible_child_name("busy");
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    // References are display images (a still, a JPEG); RAWs are rendered neutrally.
                    let r = Decoded::load(&path).map(|d| {
                        let reference = d.downscaled(1024).encode(Profile::Standard);
                        let thumb = rgba8(&reference.pixels);
                        (reference, thumb)
                    });
                    s.emit(Msg::MatchLoaded(path, r));
                });
            }
            Msg::MatchLoaded(path, Ok((reference, thumb))) => {
                let m = &self.ui.sidebar.matching;
                m.pic.set_paintable(Some(&texture(reference.width, reference.height, thumb)));
                m.name.set_label(&file_label(&path));
                self.match_file = true;
                self.request_match(reference);
            }
            Msg::MatchLoaded(path, Err(e)) => {
                self.show_match_panel();
                self.toast(&gettext!("Cannot open {}: {}", path.display(), e));
            }
            Msg::MatchClear => {
                self.match_file = false;
                self.grade.bypass(Stage::Match);
                self.ui.sidebar.matching.amount.adj.set_value(0.0);
                self.cancel_match();
                self.render();
            }
            Msg::Overlay(on) => {
                self.overlay = on;
                self.ui.legend.set_reveal_child(on);
                self.render();
            }
            Msg::SkinLine(on) => self.ui.sidebar.skin.set_visible(on),
            Msg::PresetsRefresh => {
                let list: Vec<(String, PathBuf)> = self.presets().into_iter().map(|(p, path)| (p.name, path)).collect();
                self.ui.presets.fill(&list, &sender);
            }
            Msg::PresetSave => {
                let suggestion = self.path.as_deref().and_then(stem_label).unwrap_or_default();
                presets_ui::ask_name(&self.ui.window, &suggestion, &sender);
            }
            // What you see is what is saved: switched-off sections stay off.
            Msg::PresetSaveAs(name) => {
                sender.input(Msg::PresetWrite(Box::new(self.current_preset(name)), false, false))
            }
            Msg::PresetWrite(preset, force, apply) => {
                let path = settings::presets_dir().join(preset::file_name(&preset.name));
                let existing = self.presets();
                if !force && existing.iter().any(|(p, at)| *at == path || p.name == preset.name) {
                    let name = preset.name.clone();
                    let replace = move || Msg::PresetWrite(preset.clone(), true, apply);
                    return presets_ui::confirm_replace(&self.ui.window, &name, replace, &sender);
                }
                // A preset with the same name in another file is replaced too.
                for (p, at) in existing {
                    if p.name == preset.name && at != path {
                        let _ = std::fs::remove_file(at);
                    }
                }
                match preset.save(&path) {
                    Ok(()) if apply => sender.input(Msg::PresetApply(path)),
                    Ok(()) => self.toast(&gettext!("Preset “{}” saved", preset.name)),
                    Err(e) => self.toast(&gettext!("Cannot save the preset: {}", e)),
                }
            }
            Msg::PresetApply(path) => match Preset::load(&path) {
                Ok(p) => {
                    let before = self.grade.clone();
                    self.match_file = false;
                    self.apply_grade(p.grade);
                    self.undo_toast(&gettext!("Preset “{}” applied", p.name), move || {
                        Msg::RestoreGrade(Box::new(before.clone()))
                    });
                }
                Err(e) => self.toast(&gettext!("Cannot read the preset: {}", e)),
            },
            Msg::PresetDelete(path) => {
                // Deleting is immediate; the toast offers to put it back.
                let Ok(text) = std::fs::read_to_string(&path) else { return };
                if let Err(e) = std::fs::remove_file(&path) {
                    return self.toast(&gettext!("Cannot delete the preset: {}", e));
                }
                sender.input(Msg::PresetsRefresh);
                let name = Preset::from_json(&text).map(|p| p.name).unwrap_or_default();
                self.undo_toast(&gettext!("Preset “{}” deleted", name), move || {
                    Msg::PresetRestore(path.clone(), text.clone())
                });
            }
            Msg::PresetRestore(path, text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    self.toast(&gettext!("Cannot restore the preset: {}", e));
                }
                sender.input(Msg::PresetsRefresh);
            }
            Msg::PresetImport => {
                let dialog = preset_dialog(&gettext("Import Preset"));
                let s = sender.clone();
                dialog.open(Some(&self.ui.window), gio::Cancellable::NONE, move |r| {
                    let Some(p) = r.ok().and_then(|f| f.path()) else { return };
                    match Preset::load(&p) {
                        // Imported presets join the list under their own name, asking before replacing one.
                        Ok(preset) => s.input(Msg::PresetWrite(Box::new(preset), false, true)),
                        Err(e) => s.input(Msg::Toast(gettext!("Not a valid preset: {}", e))),
                    }
                });
            }
            Msg::PresetExport => {
                let dialog = preset_dialog(&gettext("Export Preset"));
                let stem = self.path.as_deref().and_then(stem_label).unwrap_or_else(|| "preset".into());
                dialog.set_initial_name(Some(&preset::file_name(&stem)));
                let s = sender.clone();
                dialog.save(Some(&self.ui.window), gio::Cancellable::NONE, move |r| {
                    if let Some(p) = r.ok().and_then(|f| f.path()) {
                        s.input(Msg::PresetExportTo(p));
                    }
                });
            }
            Msg::PresetExportTo(path) => match self.current_preset(stem_label(&path).unwrap_or_default()).save(&path) {
                Ok(()) => self.toast(&gettext!("Preset saved: {}", path.display())),
                Err(e) => self.toast(&gettext!("Cannot save the preset: {}", e)),
            },
            Msg::Toast(text) => self.toast(&text),
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
            Msg::CompareGraded(on) => {
                self.compare_graded = on;
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
                self.reencode_roll();
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
            Done::RollFrame { generation, index, width, height, rgba } => {
                if generation == self.roll_gen {
                    self.ui.filmstrip.set_frame(index, &texture(width, height, rgba));
                }
            }
            Done::Matched(generation, _) if generation != self.match_gen => {}
            Done::Matched(_, m) => {
                self.grade.params.match_ab = m.ab;
                self.grade.curves[MATCH_L] = m.lightness;
                self.grade.params.match_amount = 100.0;
                let ui = &self.ui.sidebar.matching;
                ui.amount.adj.set_value(100.0);
                ui.stack.set_visible_child_name("set");
                self.render();
            }
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

/// Mixer coefficient `i`: 0..9 the RGB rows, 9..12 the black & white filter.
fn mixer_value_mut(p: &mut Params, i: usize) -> &mut f32 {
    if i < 9 { &mut p.mixer[i] } else { &mut p.mono_mix[i - 9] }
}

fn mixer_value(p: &Params, i: usize) -> f32 {
    *mixer_value_mut(&mut p.clone(), i)
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
        (gettext("Show clipping"), "c"),
        (gettext("Skin tone line"), "k"),
        (gettext("Grid of all photos"), "g"),
        (gettext("Save preset"), "<Control>s"),
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
        let paths: Vec<PathBuf> = files.iter().filter_map(|f| f.path()).collect();
        if !paths.is_empty() {
            OPEN.with(|o| o.get().map(|s| s.emit(Msg::RollAddPaths(paths))));
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

/// File dialog for photos and RAWs, starting where the last photo was opened.
fn photo_dialog(title: &str) -> gtk::FileDialog {
    // add_suffix matches case-insensitively.
    let pats: Vec<String> = RASTER_EXTS.iter().chain(RAW_EXTS).map(|s| s.to_string()).collect();
    let dialog = gtk::FileDialog::builder()
        .title(title)
        .filters(&file_filters(&gettext("Photos and RAWs"), &pats))
        .modal(true)
        .build();
    dialog.set_initial_folder(start_in(settings::PHOTO_FOLDER).as_ref());
    dialog
}

fn preset_dialog(title: &str) -> gtk::FileDialog {
    let filters = file_filters(&gettext("VibingLUT presets"), &[preset::EXT.into()]);
    gtk::FileDialog::builder().title(title).filters(&filters).modal(true).build()
}

fn no_photo_to_match() -> String {
    gettext("Open a photo first: the match is computed on it")
}

/// Display-encoded float pixels as RGBA8, for thumbnails.
fn rgba8(pixels: &[[f32; 4]]) -> Vec<u8> {
    pixels.iter().flat_map(|p| p.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)).collect()
}
