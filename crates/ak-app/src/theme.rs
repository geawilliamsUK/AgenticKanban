//! The look of the app: a liquid-glass theme.
//!
//! Panels and cards are translucent layers with a hairline rim, floating over
//! a soft colour backdrop. Corners are generously rounded, controls are
//! capsules, and colours come from the iOS system palette. Each mode has a
//! light and a dark variant, and egui picks one from the system setting.
//!
//! Views take their colours and frames from here instead of writing literals,
//! so the whole look can be changed in one place.

use std::path::PathBuf;
use std::sync::Arc;

use egui::{
    pos2, vec2, Color32, ColorImage, Context, CornerRadius, FontData, FontDefinitions, FontFamily,
    FontId, Frame, InnerResponse, LayerId, Margin, Rect, Shadow, Stroke, TextStyle, TextureHandle,
    TextureOptions, Theme, Ui,
};

/// Corner radius of the floating panels: the sidebar and the ticket panel.
pub const PANEL_RADIUS: u8 = 26;
/// Corner radius of cards that sit on the backdrop.
pub const CARD_RADIUS: u8 = 18;
/// Corner radius of controls and notices. At the default control height this
/// is a capsule.
pub const CONTROL_RADIUS: u8 = 14;
/// Gap between a floating panel and the window edge.
pub const PANEL_GAP: i8 = 10;

/// The font family for headings: Segoe UI Semibold where Windows has it.
const SEMIBOLD: &str = "semibold";

/// A semantic colour. Views say what a colour means, not which colour it is,
/// so the light and dark variants stay consistent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Links, selection and primary actions.
    Accent,
    /// Information that is neither good nor bad.
    Teal,
    /// Running, approved, complete.
    Green,
    /// Waiting or scheduled.
    Yellow,
    /// Stopping, high priority, needs attention soon.
    Orange,
    /// Failed, denied, urgent.
    Red,
    /// Idle, inactive, de-emphasised.
    Gray,
}

/// The colours of one mode.
#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    /// Primary text.
    pub text: Color32,
    /// Labels, captions and other secondary text.
    pub text_secondary: Color32,
    /// The floating panels: sidebar and ticket panel.
    pub panel: Color32,
    /// Cards that sit on the backdrop.
    pub card: Color32,
    /// Cards under the pointer.
    pub card_hover: Color32,
    /// Sheets and popups: dialogs, menus and the toast.
    pub sheet: Color32,
    /// The hairline edge of glass.
    pub rim: Color32,
    /// Sunken fields and code blocks.
    pub inset: Color32,
    /// Resting fill of a button.
    pub control: Color32,
    /// Hovered buttons and source-list rows.
    pub control_hover: Color32,
    /// The fill of a selected source-list row.
    pub selection: Color32,
    /// The shadow under floating glass.
    pub shadow: Color32,
}

impl Palette {
    fn new(dark: bool) -> Self {
        if dark {
            Self {
                dark,
                text: rgba((245, 245, 247), 255),
                text_secondary: rgba((235, 235, 245), 153),
                panel: rgba((28, 30, 42), 150),
                card: rgba((255, 255, 255), 20),
                card_hover: rgba((255, 255, 255), 32),
                sheet: rgba((36, 38, 52), 248),
                rim: rgba((255, 255, 255), 38),
                inset: rgba((6, 8, 18), 190),
                control: rgba((255, 255, 255), 24),
                control_hover: rgba((255, 255, 255), 44),
                selection: rgba(tone_rgb(true, Tone::Accent), 78),
                shadow: rgba((0, 0, 0), 110),
            }
        } else {
            Self {
                dark,
                text: rgba((28, 28, 30), 255),
                text_secondary: rgba((60, 60, 67), 166),
                panel: rgba((255, 255, 255), 170),
                card: rgba((255, 255, 255), 150),
                card_hover: rgba((255, 255, 255), 200),
                sheet: rgba((250, 250, 252), 248),
                rim: rgba((255, 255, 255), 220),
                inset: rgba((120, 120, 128), 30),
                control: rgba((255, 255, 255), 190),
                control_hover: rgba((255, 255, 255), 240),
                selection: rgba(tone_rgb(false, Tone::Accent), 44),
                shadow: rgba((30, 40, 70), 46),
            }
        }
    }

    /// The solid colour of a tone, tuned for text and strokes in this mode.
    pub fn tone(&self, tone: Tone) -> Color32 {
        rgba(tone_rgb(self.dark, tone), 255)
    }

    /// A translucent wash of a tone, for chips and notices.
    pub fn tone_fill(&self, tone: Tone) -> Color32 {
        rgba(tone_rgb(self.dark, tone), if self.dark { 52 } else { 36 })
    }

    /// The edge of a notice tinted with a tone.
    pub fn tone_rim(&self, tone: Tone) -> Color32 {
        rgba(tone_rgb(self.dark, tone), if self.dark { 120 } else { 110 })
    }
}

/// The palette for the mode a style is drawn in.
pub fn palette(style: &egui::Style) -> Palette {
    Palette::new(style.visuals.dark_mode)
}

/// The solid colour of a tone in the mode a style is drawn in.
pub fn tone(style: &egui::Style, tone: Tone) -> Color32 {
    palette(style).tone(tone)
}

fn rgba(rgb: (u8, u8, u8), alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(rgb.0, rgb.1, rgb.2, alpha)
}

/// The iOS system colours, in dark and light variants. The light variants are
/// deepened where a tone is used for text on a light surface.
fn tone_rgb(dark: bool, tone: Tone) -> (u8, u8, u8) {
    let (on_dark, on_light) = match tone {
        Tone::Accent => ((10, 132, 255), (0, 122, 255)),
        Tone::Teal => ((100, 210, 255), (50, 173, 230)),
        Tone::Green => ((48, 209, 88), (36, 138, 61)),
        Tone::Yellow => ((255, 214, 10), (183, 128, 0)),
        Tone::Orange => ((255, 159, 10), (201, 104, 0)),
        Tone::Red => ((255, 69, 58), (215, 0, 21)),
        Tone::Gray => ((152, 152, 157), (110, 110, 115)),
    };
    if dark {
        on_dark
    } else {
        on_light
    }
}

/// Installs the fonts and the style of both modes. Call once, at startup.
pub fn install(ctx: &Context) {
    install_fonts(ctx);
    ctx.set_style_of(Theme::Dark, style(true));
    ctx.set_style_of(Theme::Light, style(false));
}

/// Segoe UI, the system face of Windows, when it is installed. egui's bundled
/// face stays behind it as the fallback for anything Segoe does not cover.
fn install_fonts(ctx: &Context) {
    let mut fonts = FontDefinitions::default();
    if let Some(bytes) = system_font("segoeui.ttf") {
        fonts
            .font_data
            .insert("segoe-ui".into(), Arc::new(FontData::from_owned(bytes)));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "segoe-ui".into());
    }
    let mut semibold = Vec::new();
    if let Some(bytes) = system_font("seguisb.ttf") {
        fonts.font_data.insert(
            "segoe-ui-semibold".into(),
            Arc::new(FontData::from_owned(bytes)),
        );
        semibold.push("segoe-ui-semibold".to_owned());
    }
    semibold.extend(
        fonts
            .families
            .get(&FontFamily::Proportional)
            .cloned()
            .unwrap_or_default(),
    );
    fonts
        .families
        .insert(FontFamily::Name(SEMIBOLD.into()), semibold);
    ctx.set_fonts(fonts);
}

fn system_font(file: &str) -> Option<Vec<u8>> {
    let windir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    std::fs::read(windir.join("Fonts").join(file)).ok()
}

/// The style of one mode: glass visuals, rounded widgets, and roomier spacing.
fn style(dark: bool) -> egui::Style {
    let p = Palette::new(dark);
    let accent = p.tone(Tone::Accent);
    let mut style = egui::Style {
        visuals: if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        },
        ..Default::default()
    };

    let v = &mut style.visuals;
    v.dark_mode = dark;
    // The backdrop shows through every panel that does not draw its own frame.
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = p.sheet;
    v.window_stroke = Stroke::new(1.0_f32, p.rim);
    v.window_corner_radius = CornerRadius::same(22);
    v.menu_corner_radius = CornerRadius::same(CONTROL_RADIUS);
    v.window_shadow = Shadow {
        offset: [0, 12],
        blur: 40,
        spread: 0,
        color: p.shadow,
    };
    v.popup_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: p.shadow,
    };
    v.faint_bg_color = p.card;
    v.extreme_bg_color = p.inset;
    v.code_bg_color = p.inset;
    v.weak_text_color = Some(p.text_secondary);
    v.hyperlink_color = accent;
    v.warn_fg_color = p.tone(Tone::Orange);
    v.error_fg_color = p.tone(Tone::Red);
    v.selection.bg_fill = p.selection;
    // egui also draws selected text in this colour. On a tinted row, the deep
    // accent is hard to read in dark mode, so the stroke lightens there.
    let selected_ink = if dark {
        rgba((130, 190, 255), 255)
    } else {
        accent
    };
    v.selection.stroke = Stroke::new(1.5_f32, selected_ink);

    let rim = Stroke::new(1.0_f32, p.rim);
    let text = Stroke::new(1.0_f32, p.text);
    let radius = CornerRadius::same(CONTROL_RADIUS);
    let w = &mut v.widgets;
    // Resting, hovered and pressed states. A hovered control swells a pixel,
    // which reads as the glass lifting under the pointer.
    w.noninteractive.bg_fill = Color32::TRANSPARENT;
    w.noninteractive.weak_bg_fill = p.card;
    w.noninteractive.bg_stroke = rim;
    w.noninteractive.fg_stroke = text;
    w.noninteractive.corner_radius = CornerRadius::same(CARD_RADIUS);
    w.inactive.bg_fill = p.control;
    w.inactive.weak_bg_fill = p.control;
    w.inactive.bg_stroke = rim;
    w.inactive.fg_stroke = text;
    w.inactive.corner_radius = radius;
    w.hovered.bg_fill = p.control_hover;
    w.hovered.weak_bg_fill = p.control_hover;
    w.hovered.bg_stroke = rim;
    w.hovered.fg_stroke = text;
    w.hovered.corner_radius = radius;
    w.hovered.expansion = 1.0;
    w.active.bg_fill = p.selection;
    w.active.weak_bg_fill = p.selection;
    w.active.bg_stroke = Stroke::new(1.0_f32, accent);
    w.active.fg_stroke = text;
    w.active.corner_radius = radius;
    w.open = w.hovered;

    let s = &mut style.spacing;
    s.item_spacing = vec2(8.0, 8.0);
    s.button_padding = vec2(12.0, 5.0);
    s.interact_size = vec2(40.0, 28.0);
    s.window_margin = Margin::same(16);
    s.menu_margin = Margin::same(8);

    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(22.0, FontFamily::Name(SEMIBOLD.into())),
        ),
        (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(14.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(12.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(13.0, FontFamily::Monospace),
        ),
    ]
    .into();

    style
}

/// The colour field behind the glass: a few large, soft blobs on a base colour.
/// It is generated once as a small texture that the GPU stretches over the
/// window, and the stretching softens it further.
pub struct Backdrop {
    dark: TextureHandle,
    light: TextureHandle,
}

impl Backdrop {
    pub fn new(ctx: &Context) -> Self {
        Self {
            dark: ctx.load_texture(
                "backdrop-dark",
                backdrop_image(true),
                TextureOptions::LINEAR,
            ),
            light: ctx.load_texture(
                "backdrop-light",
                backdrop_image(false),
                TextureOptions::LINEAR,
            ),
        }
    }

    /// Paints the backdrop over the whole window. Call it first in each frame,
    /// so that everything else draws over it.
    pub fn paint(&self, ctx: &Context) {
        let texture = if ctx.style().visuals.dark_mode {
            &self.dark
        } else {
            &self.light
        };
        ctx.layer_painter(LayerId::background()).image(
            texture.id(),
            ctx.screen_rect(),
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

/// A coloured region of the backdrop. Positions and radii are in units of the
/// window height, so a blob is round on screen.
struct Blob {
    at: (f32, f32),
    radius: f32,
    color: (f32, f32, f32),
    strength: f32,
}

const DARK_BASE: (f32, f32, f32) = (9.0, 11.0, 22.0);
const DARK_BLOBS: [Blob; 5] = [
    Blob {
        at: (0.10, 0.15),
        radius: 0.55,
        color: (94.0, 92.0, 230.0),
        strength: 0.85,
    },
    Blob {
        at: (0.95, 0.05),
        radius: 0.45,
        color: (10.0, 132.0, 255.0),
        strength: 0.60,
    },
    Blob {
        at: (0.80, 0.95),
        radius: 0.60,
        color: (48.0, 176.0, 199.0),
        strength: 0.50,
    },
    Blob {
        at: (0.02, 1.00),
        radius: 0.45,
        color: (255.0, 55.0, 95.0),
        strength: 0.40,
    },
    Blob {
        at: (0.55, 0.45),
        radius: 0.35,
        color: (191.0, 90.0, 242.0),
        strength: 0.20,
    },
];

const LIGHT_BASE: (f32, f32, f32) = (236.0, 241.0, 250.0);
const LIGHT_BLOBS: [Blob; 5] = [
    Blob {
        at: (0.12, 0.18),
        radius: 0.55,
        color: (150.0, 200.0, 255.0),
        strength: 0.85,
    },
    Blob {
        at: (0.92, 0.12),
        radius: 0.45,
        color: (255.0, 190.0, 220.0),
        strength: 0.70,
    },
    Blob {
        at: (0.80, 0.92),
        radius: 0.60,
        color: (190.0, 245.0, 218.0),
        strength: 0.75,
    },
    Blob {
        at: (0.02, 0.98),
        radius: 0.45,
        color: (255.0, 226.0, 170.0),
        strength: 0.60,
    },
    Blob {
        at: (0.55, 0.50),
        radius: 0.35,
        color: (210.0, 196.0, 255.0),
        strength: 0.35,
    },
];

fn backdrop_image(dark: bool) -> ColorImage {
    const WIDTH: usize = 192;
    const HEIGHT: usize = 120;
    // The texture is stretched over a window about 1.6 times wider than tall.
    const ASPECT: f32 = 1.6;

    let (base, blobs) = if dark {
        (DARK_BASE, &DARK_BLOBS[..])
    } else {
        (LIGHT_BASE, &LIGHT_BLOBS[..])
    };
    let mut rgba = Vec::with_capacity(WIDTH * HEIGHT * 4);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let u = x as f32 / (WIDTH - 1) as f32;
            let v = y as f32 / (HEIGHT - 1) as f32;
            let mut c = [base.0, base.1, base.2];
            for blob in blobs {
                let dx = (u - blob.at.0) * ASPECT;
                let dy = v - blob.at.1;
                let falloff = (-(dx * dx + dy * dy) / (2.0 * blob.radius * blob.radius)).exp();
                let w = blob.strength * falloff;
                let col = [blob.color.0, blob.color.1, blob.color.2];
                for i in 0..3 {
                    c[i] += (col[i] - c[i]) * w;
                }
            }
            rgba.extend([c[0] as u8, c[1] as u8, c[2] as u8, 255]);
        }
    }
    ColorImage::from_rgba_unmultiplied([WIDTH, HEIGHT], &rgba)
}

/// A card on the backdrop: board columns, list items, sections of a panel.
pub fn card(style: &egui::Style) -> Frame {
    let p = palette(style);
    Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0_f32, p.rim))
        .corner_radius(CARD_RADIUS)
        .inner_margin(Margin::same(12))
        .shadow(Shadow {
            offset: [0, 4],
            blur: 18,
            spread: 0,
            color: p.shadow,
        })
}

/// A floating panel, inset from the window edge: the sidebar and the ticket
/// panel.
pub fn panel(style: &egui::Style) -> Frame {
    let p = palette(style);
    Frame::new()
        .fill(p.panel)
        .stroke(Stroke::new(1.0_f32, p.rim))
        .corner_radius(PANEL_RADIUS)
        .inner_margin(Margin::symmetric(14, 14))
        .outer_margin(Margin::same(PANEL_GAP))
        .shadow(Shadow {
            offset: [0, 10],
            blur: 32,
            spread: 0,
            color: p.shadow,
        })
}

/// A notice washed with a tone: approvals, warnings, notes.
pub fn tinted(style: &egui::Style, tone: Tone) -> Frame {
    let p = palette(style);
    Frame::new()
        .fill(p.tone_fill(tone))
        .stroke(Stroke::new(1.0_f32, p.tone_rim(tone)))
        .corner_radius(CONTROL_RADIUS)
        .inner_margin(Margin::same(10))
}

/// Shows `add_contents` inside `frame` and adds the glass highlight on top.
/// Use it for every surface that a frame from this module draws.
pub fn glass<R>(
    ui: &mut Ui,
    frame: Frame,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<R> {
    let radius = frame.corner_radius.nw;
    let margin = frame.outer_margin;
    let out = frame.show(ui, add_contents);
    // The response covers the outer margin too; the glass does not.
    let r = out.response.rect;
    let rect = Rect::from_min_max(
        pos2(r.left() + margin.left as f32, r.top() + margin.top as f32),
        pos2(
            r.right() - margin.right as f32,
            r.bottom() - margin.bottom as f32,
        ),
    );
    highlight_top_edge(ui, rect, radius);
    out
}

/// A bright line along the top edge of dark glass, where light catches it.
/// Light glass is already white, so it gets no highlight.
fn highlight_top_edge(ui: &Ui, rect: Rect, radius: u8) {
    if !ui.visuals().dark_mode {
        return;
    }
    let y = rect.top() + 0.5;
    // Capsules ask for a radius larger than the surface is wide.
    let r = (radius as f32).min(rect.width() * 0.5);
    ui.painter().line_segment(
        [pos2(rect.left() + r, y), pos2(rect.right() - r, y)],
        Stroke::new(1.0_f32, Color32::from_white_alpha(70)),
    );
}
