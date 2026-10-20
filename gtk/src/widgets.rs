//! Custom colourist controls: labelled sliders, tone-curve editor, colour wheels.

use std::cell::RefCell;
use std::f64::consts::TAU;
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::gtk::{cairo, gdk};
use vibinglut_core::grade::{Curve, identity_curve, sample_curve};

// ------------------------------------------------------------------ slider

/// Label + value + reset on top, scale below (Lightroom/RapidRAW layout).
#[derive(Clone)]
pub struct Slider {
    pub root: gtk::Box,
    pub adj: gtk::Adjustment,
    pub default: f64,
}

impl Slider {
    pub fn new(label: &str, min: f64, max: f64, default: f64, digits: u32, track: Option<&str>) -> Self {
        let adj = gtk::Adjustment::new(default, min, max, (max - min) / 400.0, (max - min) / 20.0, 0.0);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("slider-row");

        let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let name = gtk::Label::builder().label(label).xalign(0.0).hexpand(true).css_classes(["slider-label"]).build();
        let value = gtk::Label::builder().css_classes(["numeric", "slider-value"]).width_chars(6).xalign(1.0).build();
        let reset = gtk::Button::builder()
            .icon_name("edit-undo-symbolic")
            .css_classes(["flat", "circular", "slider-reset"])
            .tooltip_text("Ripristina")
            .valign(gtk::Align::Center)
            .build();
        reset.update_property(&[gtk::accessible::Property::Label(&format!("Ripristina {label}"))]);
        head.append(&name);
        head.append(&reset);
        head.append(&value);

        let scale = gtk::Scale::builder().adjustment(&adj).draw_value(false).has_origin(false).hexpand(true).build();
        scale.update_property(&[gtk::accessible::Property::Label(label)]);
        if let Some(class) = track {
            scale.add_css_class(class);
        }
        root.append(&head);
        root.append(&scale);

        let sync = {
            let (value, reset) = (value.clone(), reset.clone());
            move |a: &gtk::Adjustment| {
                value.set_label(&format!("{:.*}", digits as usize, a.value()));
                let changed = (a.value() - default).abs() > 1e-9;
                reset.set_opacity(if changed { 1.0 } else { 0.0 });
                reset.set_sensitive(changed);
            }
        };
        sync(&adj);
        adj.connect_value_changed(sync);

        let a = adj.clone();
        reset.connect_clicked(move |_| a.set_value(default));
        // Double-click the track to reset, like every grading app.
        let dbl = gtk::GestureClick::new();
        let a = adj.clone();
        dbl.connect_pressed(move |_, n, _, _| {
            if n == 2 {
                a.set_value(default);
            }
        });
        scale.add_controller(dbl);

        Self { root, adj, default }
    }

    pub fn connect(&self, f: impl Fn(f64) + 'static) {
        self.adj.connect_value_changed(move |a| f(a.value()));
    }

    pub fn reset(&self) {
        self.adj.set_value(self.default);
    }
}

// ------------------------------------------------------------ curve editor

const CHANNEL_RGB: [(f64, f64, f64); 4] =
    [(0.92, 0.92, 0.92), (0.94, 0.33, 0.31), (0.36, 0.82, 0.42), (0.36, 0.56, 0.98)];
const HIT_RADIUS: f64 = 12.0;
const PAD: f64 = 8.0;
const MIN_GAP: f64 = 0.01;

struct CurveState {
    curves: [Curve; 4],
    active: usize,
    drag: Option<(usize, f64, f64)>, // point index, start x, start y (curve space)
}

#[derive(Clone)]
pub struct CurveEditor {
    pub area: gtk::DrawingArea,
    state: Rc<RefCell<CurveState>>,
}

fn to_curve(area: &gtk::DrawingArea, x: f64, y: f64) -> (f64, f64) {
    let (w, h) = (area.width() as f64 - 2.0 * PAD, area.height() as f64 - 2.0 * PAD);
    (((x - PAD) / w).clamp(0.0, 1.0), (1.0 - (y - PAD) / h).clamp(0.0, 1.0))
}

impl CurveEditor {
    pub fn new(on_change: impl Fn(usize, Curve) + 'static) -> Self {
        let state = Rc::new(RefCell::new(CurveState {
            curves: std::array::from_fn(|_| identity_curve()),
            active: 0,
            drag: None,
        }));
        let area = gtk::DrawingArea::builder().content_height(260).hexpand(true).css_classes(["curve-area"]).build();
        area.update_property(&[gtk::accessible::Property::Label("Editor curve")]);

        let s = state.clone();
        area.set_draw_func(move |area, cr, w, h| draw_curves(area, cr, w as f64, h as f64, &s.borrow()));

        let on_change = Rc::new(on_change);
        let drag = gtk::GestureDrag::new();
        {
            let (s, a, f) = (state.clone(), area.clone(), on_change.clone());
            drag.connect_drag_begin(move |_, x, y| {
                let mut st = s.borrow_mut();
                let (w, h) = (a.width() as f64 - 2.0 * PAD, a.height() as f64 - 2.0 * PAD);
                let ch = st.active;
                let hit = st.curves[ch].iter().position(|&(px, py)| {
                    let (sx, sy) = (PAD + px as f64 * w, PAD + (1.0 - py as f64) * h);
                    (sx - x).hypot(sy - y) < HIT_RADIUS
                });
                let idx = match hit {
                    Some(i) => i,
                    None => {
                        let (cx, cy) = to_curve(&a, x, y);
                        let pts = &mut st.curves[ch];
                        let i = pts.iter().position(|p| p.0 as f64 > cx).unwrap_or(pts.len() - 1).max(1);
                        // Keep a minimum gap so neighbours never collapse onto one x.
                        if cx - (pts[i - 1].0 as f64) < MIN_GAP || (pts[i].0 as f64) - cx < MIN_GAP {
                            return;
                        }
                        pts.insert(i, (cx as f32, cy as f32));
                        let curve = pts.clone();
                        drop(st);
                        a.queue_draw();
                        f(ch, curve);
                        st = s.borrow_mut();
                        i
                    }
                };
                let p = st.curves[ch][idx];
                st.drag = Some((idx, p.0 as f64, p.1 as f64));
            });
        }
        {
            let (s, a, f) = (state.clone(), area.clone(), on_change.clone());
            drag.connect_drag_update(move |_, dx, dy| {
                let (ch, curve) = {
                    let mut st = s.borrow_mut();
                    let Some((i, x0, y0)) = st.drag else { return };
                    let (w, h) = (a.width() as f64 - 2.0 * PAD, a.height() as f64 - 2.0 * PAD);
                    let ch = st.active;
                    let pts = &mut st.curves[ch];
                    let last = pts.len() - 1;
                    // Endpoints slide vertically only; inner points stay between neighbours.
                    let x = if i == 0 || i == last {
                        pts[i].0 as f64
                    } else {
                        let (lo, hi) = (pts[i - 1].0 as f64 + MIN_GAP, pts[i + 1].0 as f64 - MIN_GAP);
                        if lo < hi { (x0 + dx / w).clamp(lo, hi) } else { (lo + hi) / 2.0 }
                    };
                    pts[i] = (x as f32, (y0 - dy / h).clamp(0.0, 1.0) as f32);
                    (ch, pts.clone())
                };
                a.queue_draw();
                f(ch, curve);
            });
        }
        {
            let s = state.clone();
            drag.connect_drag_end(move |_, _, _| s.borrow_mut().drag = None);
        }
        area.add_controller(drag);

        // Right-click or double-click removes an inner point.
        let click = gtk::GestureClick::builder().button(0).build();
        {
            let (s, a, f) = (state.clone(), area.clone(), on_change.clone());
            click.connect_pressed(move |g, n, x, y| {
                if g.current_button() != gdk::BUTTON_SECONDARY && n != 2 {
                    return;
                }
                let (ch, curve) = {
                    let mut st = s.borrow_mut();
                    let (w, h) = (a.width() as f64 - 2.0 * PAD, a.height() as f64 - 2.0 * PAD);
                    let ch = st.active;
                    let pts = &mut st.curves[ch];
                    let last = pts.len() - 1;
                    let Some(i) = (1..last).find(|&i| {
                        let (sx, sy) = (PAD + pts[i].0 as f64 * w, PAD + (1.0 - pts[i].1 as f64) * h);
                        (sx - x).hypot(sy - y) < HIT_RADIUS
                    }) else {
                        return;
                    };
                    pts.remove(i);
                    st.drag = None;
                    (ch, st.curves[ch].clone())
                };
                a.queue_draw();
                f(ch, curve);
            });
        }
        area.add_controller(click);

        Self { area, state }
    }

    pub fn set_active(&self, ch: usize) {
        self.state.borrow_mut().active = ch;
        self.area.queue_draw();
    }

    /// Resets the active channel and returns its new (identity) curve.
    pub fn reset_active(&self) -> (usize, Curve) {
        let mut st = self.state.borrow_mut();
        let ch = st.active;
        st.curves[ch] = identity_curve();
        self.area.queue_draw();
        (ch, st.curves[ch].clone())
    }

    pub fn reset_all(&self) {
        self.state.borrow_mut().curves = std::array::from_fn(|_| identity_curve());
        self.area.queue_draw();
    }
}

fn draw_curves(area: &gtk::DrawingArea, cr: &cairo::Context, w: f64, h: f64, st: &CurveState) {
    let fg = area.color();
    let (iw, ih) = (w - 2.0 * PAD, h - 2.0 * PAD);
    cr.set_line_width(1.0);
    cr.set_source_rgba(fg.red() as f64, fg.green() as f64, fg.blue() as f64, 0.10);
    for i in 1..4 {
        let t = i as f64 / 4.0;
        cr.move_to(PAD + t * iw, PAD);
        cr.line_to(PAD + t * iw, PAD + ih);
        cr.move_to(PAD, PAD + t * ih);
        cr.line_to(PAD + iw, PAD + t * ih);
    }
    cr.move_to(PAD, PAD + ih);
    cr.line_to(PAD + iw, PAD);
    let _ = cr.stroke();

    let mut samples = vec![0.0f32; 256];
    // Inactive channels faintly behind, the active one on top.
    let order = (0..4).filter(|&c| c != st.active).chain(std::iter::once(st.active));
    for ch in order {
        let pts = &st.curves[ch];
        let active = ch == st.active;
        if !active && pts == &identity_curve() {
            continue;
        }
        sample_curve(pts, &mut samples);
        let (r, g, b) = CHANNEL_RGB[ch];
        cr.set_source_rgba(r, g, b, if active { 1.0 } else { 0.35 });
        cr.set_line_width(if active { 2.0 } else { 1.25 });
        for (i, v) in samples.iter().enumerate() {
            let (x, y) = (PAD + i as f64 / 255.0 * iw, PAD + (1.0 - *v as f64) * ih);
            if i == 0 { cr.move_to(x, y) } else { cr.line_to(x, y) }
        }
        let _ = cr.stroke();
        if active {
            for &(px, py) in pts {
                cr.arc(PAD + px as f64 * iw, PAD + (1.0 - py as f64) * ih, 5.0, 0.0, TAU);
                cr.set_source_rgb(r, g, b);
                let _ = cr.fill_preserve();
                cr.set_source_rgba(0.0, 0.0, 0.0, 0.6);
                cr.set_line_width(1.5);
                let _ = cr.stroke();
            }
        }
    }
}

// ------------------------------------------------------------- colour wheel

/// Hue/chroma disc. Position (x, y) is in the unit disc, y up; the hue at
/// angle `atan2(y, x)` matches `wheel()` in the shader.
#[derive(Clone)]
pub struct ColorWheel {
    pub area: gtk::DrawingArea,
    pos: Rc<RefCell<(f64, f64)>>,
    on_change: Rc<dyn Fn(f32, f32)>,
}

fn hsv(h: f64, s: f64, v: f64) -> (f64, f64, f64) {
    let f = |n: f64| {
        let k = (n + h * 6.0) % 6.0;
        v - v * s * (k.min(4.0 - k).clamp(0.0, 1.0))
    };
    (f(5.0), f(3.0), f(1.0))
}

impl ColorWheel {
    pub fn new(name: &str, on_change: impl Fn(f32, f32) + 'static) -> Self {
        let area = gtk::DrawingArea::builder()
            .content_width(96)
            .content_height(96)
            .halign(gtk::Align::Center)
            .css_classes(["color-wheel"])
            .build();
        area.update_property(&[gtk::accessible::Property::Label(name)]);
        let pos = Rc::new(RefCell::new((0.0, 0.0)));
        let p = pos.clone();
        area.set_draw_func(move |_, cr, w, h| draw_wheel(cr, w as f64, h as f64, *p.borrow()));
        let this = Self { area: area.clone(), pos, on_change: Rc::new(on_change) };

        let drag = gtk::GestureDrag::new();
        let start = Rc::new(RefCell::new((0.0, 0.0)));
        {
            let (t, st) = (this.clone(), start.clone());
            drag.connect_drag_begin(move |_, x, y| {
                *st.borrow_mut() = (x, y);
                t.set_from_widget(x, y);
            });
        }
        {
            let (t, st) = (this.clone(), start.clone());
            drag.connect_drag_update(move |_, dx, dy| {
                let (x, y) = *st.borrow();
                t.set_from_widget(x + dx, y + dy);
            });
        }
        area.add_controller(drag);
        let dbl = gtk::GestureClick::new();
        let t = this.clone();
        dbl.connect_pressed(move |_, n, _, _| {
            if n == 2 {
                t.set(0.0, 0.0);
            }
        });
        area.add_controller(dbl);
        this
    }

    fn set_from_widget(&self, x: f64, y: f64) {
        let r = self.area.width().min(self.area.height()) as f64 / 2.0 - 4.0;
        let (cx, cy) = (self.area.width() as f64 / 2.0, self.area.height() as f64 / 2.0);
        let (mut ux, mut uy) = ((x - cx) / r, (cy - y) / r);
        let len = ux.hypot(uy);
        if len > 1.0 {
            (ux, uy) = (ux / len, uy / len);
        }
        self.set(ux, uy);
    }

    pub fn set(&self, x: f64, y: f64) {
        *self.pos.borrow_mut() = (x, y);
        self.area.queue_draw();
        (self.on_change)(x as f32, y as f32);
    }
}

fn draw_wheel(cr: &cairo::Context, w: f64, h: f64, (px, py): (f64, f64)) {
    let r = w.min(h) / 2.0 - 4.0;
    let (cx, cy) = (w / 2.0, h / 2.0);
    const SEGMENTS: usize = 120;
    for i in 0..SEGMENTS {
        let a0 = i as f64 / SEGMENTS as f64 * TAU;
        let a1 = (i + 1) as f64 / SEGMENTS as f64 * TAU + 0.01;
        let (r_, g, b) = hsv(a0 / TAU, 0.75, 0.85);
        cr.set_source_rgb(r_, g, b);
        cr.move_to(cx, cy);
        // Screen y points down, so negate angles to keep hue counter-clockwise.
        cr.arc_negative(cx, cy, r, -a0, -a1);
        cr.close_path();
        let _ = cr.fill();
    }
    let grad = cairo::RadialGradient::new(cx, cy, 0.0, cx, cy, r);
    grad.add_color_stop_rgba(0.0, 0.5, 0.5, 0.5, 1.0);
    grad.add_color_stop_rgba(1.0, 0.5, 0.5, 0.5, 0.0);
    let _ = cr.set_source(&grad);
    cr.arc(cx, cy, r, 0.0, TAU);
    let _ = cr.fill();

    cr.set_source_rgba(0.0, 0.0, 0.0, 0.35);
    cr.set_line_width(1.0);
    cr.arc(cx, cy, r, 0.0, TAU);
    let _ = cr.stroke();

    let (x, y) = (cx + px * r, cy - py * r);
    cr.arc(x, y, 6.0, 0.0, TAU);
    cr.set_source_rgb(1.0, 1.0, 1.0);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.7);
    cr.set_line_width(1.5);
    let _ = cr.stroke();
}
