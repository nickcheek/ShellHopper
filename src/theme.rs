//! Colors, global egui styling, and the small custom-painted widgets the app uses.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Rect, Response, RichText, Sense,
    Stroke, StrokeKind, TextStyle, Vec2, Widget, WidgetInfo, WidgetType,
    collapsing_header::CollapsingState, pos2, vec2,
};

pub const TERMINAL: Color32 = Color32::from_rgb(13, 14, 18);
pub const SIDEBAR: Color32 = Color32::from_rgb(20, 21, 27);
pub const SURFACE: Color32 = Color32::from_rgb(30, 32, 41);
pub const SURFACE_HOVER: Color32 = Color32::from_rgb(42, 44, 57);
pub const BORDER: Color32 = Color32::from_rgb(40, 42, 54);
pub const TEXT: Color32 = Color32::from_rgb(226, 228, 238);
pub const MUTED: Color32 = Color32::from_rgb(139, 143, 159);
pub const FAINT: Color32 = Color32::from_rgb(98, 102, 120);
pub const ACCENT: Color32 = Color32::from_rgb(57, 103, 145);
pub const RED: Color32 = Color32::from_rgb(240, 104, 104);

pub const TERM_FONT_SIZE: f32 = 13.0;
pub const TERM_LINE_HEIGHT: f32 = 17.0;
/// Height of the strip that shares the window's top edge with the traffic lights.
pub const TITLEBAR_HEIGHT: f32 = 40.0;

pub fn apply(ctx: &egui::Context) {
    install_system_fonts(ctx);

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = TERMINAL;
    visuals.window_fill = SIDEBAR;
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(8);
    visuals.extreme_bg_color = TERMINAL;
    visuals.text_edit_bg_color = Some(TERMINAL);
    visuals.selection.bg_fill = ACCENT.gamma_multiply(0.45);
    visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(129, 174, 214));
    visuals.hyperlink_color = ACCENT;
    visuals.indent_has_left_vline = false;

    let radius = CornerRadius::same(6);
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    widgets.noninteractive.corner_radius = radius;
    widgets.inactive.bg_fill = SURFACE;
    widgets.inactive.weak_bg_fill = SURFACE;
    widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    widgets.inactive.corner_radius = radius;
    for state in [&mut widgets.hovered, &mut widgets.open] {
        state.bg_fill = SURFACE_HOVER;
        state.weak_bg_fill = SURFACE_HOVER;
        state.bg_stroke = Stroke::new(1.0, FAINT);
        state.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        state.corner_radius = radius;
        state.expansion = 0.0;
    }
    widgets.active.bg_fill = ACCENT;
    widgets.active.weak_bg_fill = ACCENT;
    widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    widgets.active.corner_radius = radius;
    widgets.active.expansion = 0.0;
    ctx.set_visuals(visuals);

    ctx.global_style_mut(|style| {
        for (text_style, size) in [
            (TextStyle::Small, 11.5),
            (TextStyle::Body, 13.0),
            (TextStyle::Button, 13.0),
            (TextStyle::Heading, 16.0),
        ] {
            style
                .text_styles
                .insert(text_style, FontId::proportional(size));
        }
        style
            .text_styles
            .insert(TextStyle::Monospace, FontId::monospace(TERM_FONT_SIZE));
        style.spacing.item_spacing = vec2(8.0, 8.0);
        style.spacing.button_padding = vec2(10.0, 5.0);
        style.spacing.interact_size.y = 26.0;
        style.spacing.window_margin = egui::Margin::same(16);
    });
}

/// Prefer the macOS system fonts; egui's bundled fonts stay as fallbacks.
fn install_system_fonts(ctx: &egui::Context) {
    use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
    for (name, path, family) in [
        (
            "SF Pro",
            "/System/Library/Fonts/SFNS.ttf",
            FontFamily::Proportional,
        ),
        (
            "SF Mono",
            "/System/Library/Fonts/SFNSMono.ttf",
            FontFamily::Monospace,
        ),
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            ctx.add_font(FontInsert {
                name: name.into(),
                data: egui::FontData::from_owned(bytes),
                families: vec![InsertFontFamily {
                    family,
                    priority: FontPriority::Highest,
                }],
            });
        }
    }
}

/// Lets the window be moved (and zoomed on double-click) from an empty strip of UI.
/// Call before adding widgets that overlap `rect` so they stay clickable.
pub fn window_drag_area(ui: &egui::Ui, rect: Rect, salt: &str) {
    let response = ui.interact(rect, ui.id().with(salt), Sense::click_and_drag());
    if response.double_clicked() {
        let maximized = ui.input(|input| input.viewport().maximized.unwrap_or(false));
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
    } else if response.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
}

pub fn primary_button(text: &str, width: f32) -> CenteredButton<'_> {
    CenteredButton {
        text,
        width,
        primary: true,
    }
}

pub fn secondary_button(text: &str, width: f32) -> CenteredButton<'_> {
    CenteredButton {
        text,
        width,
        primary: false,
    }
}

pub struct CenteredButton<'a> {
    text: &'a str,
    width: f32,
    primary: bool,
}

impl Widget for CenteredButton<'_> {
    fn ui(self, ui: &mut egui::Ui) -> Response {
        let font = FontId::proportional(13.0);
        let text_width = ui
            .painter()
            .layout_no_wrap(self.text.to_owned(), font.clone(), TEXT)
            .size()
            .x;
        let (rect, response) = ui.allocate_exact_size(
            vec2(self.width.max(text_width + 24.0), 30.0),
            Sense::click(),
        );
        response
            .widget_info(|| WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), self.text));

        let enabled = ui.is_enabled();
        let fill = if self.primary {
            if !enabled {
                ACCENT.gamma_multiply(0.35)
            } else if response.hovered() {
                Color32::from_rgb(67, 119, 165)
            } else {
                ACCENT
            }
        } else if !enabled {
            SURFACE.gamma_multiply(0.55)
        } else if response.hovered() {
            SURFACE_HOVER
        } else {
            SURFACE
        };
        let stroke = if self.primary {
            Stroke::NONE
        } else {
            Stroke::new(1.0, if response.hovered() { FAINT } else { BORDER })
        };
        let text_color = if enabled {
            if self.primary { Color32::WHITE } else { TEXT }
        } else {
            FAINT
        };
        ui.painter().rect(rect, 7, fill, stroke, StrokeKind::Inside);
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            self.text,
            font,
            text_color,
        );
        response
    }
}

/// Low-emphasis text button that only shows a frame on hover.
pub fn ghost_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).size(12.0).color(MUTED)).frame_when_inactive(false)
}

/// Collapsible sidebar section: chevron, uppercase title, and a host count.
pub fn host_group(
    ui: &mut egui::Ui,
    name: &str,
    count: usize,
    default_open: bool,
    body: impl FnOnce(&mut egui::Ui),
) -> Response {
    let id = ui.make_persistent_id(("host_group", name));
    let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, default_open);
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click_and_drag());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::CollapsingHeader, true, name));
    if response.clicked() {
        state.toggle(ui);
    }
    let openness = state.openness(ui.ctx());

    let center_y = rect.center().y;
    let color = if response.hovered() { TEXT } else { MUTED };
    let pivot = pos2(rect.left() + 15.0, center_y);
    let rotation = egui::emath::Rot2::from_angle(openness * std::f32::consts::FRAC_PI_2);
    let tip = pivot + rotation * vec2(2.0, 0.0);
    let stroke = Stroke::new(1.4, color);
    for wing in [vec2(-1.5, -3.5), vec2(-1.5, 3.5)] {
        ui.painter()
            .line_segment([pivot + rotation * wing, tip], stroke);
    }
    ui.painter().text(
        pos2(rect.left() + 31.0, center_y),
        Align2::LEFT_CENTER,
        name.to_uppercase(),
        FontId::proportional(11.0),
        color,
    );
    ui.painter().text(
        pos2(rect.right() - 10.0, center_y),
        Align2::RIGHT_CENTER,
        count,
        FontId::proportional(11.0),
        FAINT,
    );

    state.show_body_unindented(ui, body);
    ui.add_space(6.0);
    response
}

pub enum HostRowAction {
    Toggle,
    Connect,
    ConnectSelected,
    Edit,
}

pub struct HostRowResponse {
    pub action: Option<HostRowAction>,
    pub response: Response,
}

/// One full-width host line: checkbox, name, address, and hover-only actions.
pub fn host_row(
    ui: &mut egui::Ui,
    name: &str,
    address: &str,
    target: &str,
    selected: bool,
) -> HostRowResponse {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click_and_drag());
    response.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, selected, name));
    let hovered = ui.rect_contains_pointer(rect);
    let center_y = rect.center().y;

    let mut action = None;
    if response.double_clicked() {
        action = Some(HostRowAction::Connect);
    } else if response.clicked() {
        action = Some(HostRowAction::Toggle);
    }

    let fill = if selected {
        ACCENT.gamma_multiply(0.16)
    } else if hovered {
        SURFACE
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 6, fill);

    let box_rect = Rect::from_center_size(pos2(rect.left() + 15.0, center_y), Vec2::splat(14.0));
    if selected {
        ui.painter().rect_filled(box_rect, 4, ACCENT);
        let check = Stroke::new(1.6, Color32::WHITE);
        let knee = pos2(box_rect.left() + 6.0, box_rect.bottom() - 4.0);
        ui.painter()
            .line_segment([pos2(box_rect.left() + 3.5, center_y), knee], check);
        ui.painter().line_segment(
            [knee, pos2(box_rect.right() - 3.0, box_rect.top() + 4.0)],
            check,
        );
    } else {
        let color = if hovered { MUTED } else { FAINT };
        ui.painter()
            .rect_stroke(box_rect, 4, Stroke::new(1.0, color), StrokeKind::Inside);
    }

    let edit_rect = Rect::from_center_size(pos2(rect.right() - 16.0, center_y), Vec2::splat(22.0));
    let connect_rect = edit_rect.translate(vec2(-26.0, 0.0));
    let text_left = box_rect.right() + 9.0;
    let text_right = if hovered {
        connect_rect.left() - 6.0
    } else {
        rect.right() - 10.0
    };

    let name_galley =
        ui.painter()
            .layout_no_wrap(name.to_owned(), FontId::proportional(13.0), TEXT);
    let name_width = name_galley.size().x;
    let clip = Rect::from_min_max(pos2(text_left, rect.top()), pos2(text_right, rect.bottom()));
    ui.painter()
        .with_clip_rect(clip.intersect(ui.clip_rect()))
        .galley(
            pos2(text_left, center_y - name_galley.size().y / 2.0),
            name_galley,
            TEXT,
        );

    if hovered {
        let connect = icon_button(ui, connect_rect, response.id.with("connect"))
            .on_hover_text(format!("Connect to {target}"));
        let center = connect_rect.center();
        let stroke = Stroke::new(1.5, icon_color(&connect));
        let tip = center + vec2(3.0, 0.0);
        ui.painter()
            .line_segment([center + vec2(-2.0, -4.5), tip], stroke);
        ui.painter()
            .line_segment([center + vec2(-2.0, 4.5), tip], stroke);
        if connect.clicked() {
            action = Some(HostRowAction::Connect);
        }

        let edit = icon_button(ui, edit_rect, response.id.with("edit")).on_hover_text("Edit host");
        let color = icon_color(&edit);
        for offset in [-4.5, 0.0, 4.5] {
            ui.painter()
                .circle_filled(edit_rect.center() + vec2(offset, 0.0), 1.3, color);
        }
        if edit.clicked() {
            action = Some(HostRowAction::Edit);
        }
    } else if !address.is_empty() && address != name {
        let galley =
            ui.painter()
                .layout_no_wrap(address.to_owned(), FontId::proportional(11.5), FAINT);
        if text_left + name_width + 14.0 + galley.size().x <= text_right {
            ui.painter().galley(
                pos2(
                    text_right - galley.size().x,
                    center_y - galley.size().y / 2.0,
                ),
                galley,
                FAINT,
            );
        }
    }

    HostRowResponse { action, response }
}

pub struct TabResponse {
    pub clicked: bool,
    pub close: bool,
    pub response: Response,
}

/// A tab chip with a status dot and inline close button (shown while active or hovered).
pub fn tab(
    ui: &mut egui::Ui,
    title: &str,
    selected: bool,
    closable: bool,
    status_color: Color32,
) -> TabResponse {
    let color = if selected { TEXT } else { MUTED };
    let galley = ui
        .painter()
        .layout_no_wrap(title.to_owned(), FontId::proportional(12.5), color);
    let text_width = galley.size().x.min(180.0);
    let status_space = 14.0;
    let close_space = if closable { 22.0 } else { 0.0 };
    let width = (12.0 + status_space + text_width + close_space + 12.0).max(72.0);
    let (rect, response) = ui.allocate_exact_size(vec2(width, 28.0), Sense::click_and_drag());
    response
        .widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, true, selected, title));
    let hovered = ui.rect_contains_pointer(rect);

    let fill = if selected {
        SURFACE_HOVER
    } else if hovered {
        SURFACE
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 7, fill);
    ui.painter()
        .circle_filled(pos2(rect.left() + 13.0, rect.center().y), 3.0, status_color);

    let text_pos = pos2(
        rect.left() + 12.0 + status_space,
        rect.center().y - galley.size().y / 2.0,
    );
    let clip = Rect::from_min_size(
        rect.min,
        vec2(12.0 + status_space + text_width, rect.height()),
    );
    ui.painter()
        .with_clip_rect(clip.intersect(ui.clip_rect()))
        .galley(text_pos, galley, color);

    let mut close = false;
    if closable && (selected || hovered) {
        let close_rect = Rect::from_center_size(
            pos2(rect.right() - 15.0, rect.center().y),
            Vec2::splat(18.0),
        );
        let button =
            icon_button(ui, close_rect, response.id.with("close")).on_hover_text("Close tab");
        let stroke = Stroke::new(1.3, icon_color(&button));
        let center = close_rect.center();
        ui.painter()
            .line_segment([center + vec2(-3.5, -3.5), center + vec2(3.5, 3.5)], stroke);
        ui.painter()
            .line_segment([center + vec2(-3.5, 3.5), center + vec2(3.5, -3.5)], stroke);
        close = button.clicked();
    }

    TabResponse {
        clicked: response.clicked(),
        close,
        response,
    }
}

pub fn new_tab_button(ui: &mut egui::Ui) -> Response {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::hover());
    let response = icon_button(ui, rect, ui.id().with("new_tab"));
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "New local terminal"));
    let stroke = Stroke::new(1.4, icon_color(&response));
    let center = rect.center();
    ui.painter()
        .line_segment([center - vec2(5.0, 0.0), center + vec2(5.0, 0.0)], stroke);
    ui.painter()
        .line_segment([center - vec2(0.0, 5.0), center + vec2(0.0, 5.0)], stroke);
    response
}

fn icon_button(ui: &egui::Ui, rect: Rect, id: egui::Id) -> Response {
    let response = ui.interact(rect, id, Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 5, Color32::from_white_alpha(22));
    }
    response
}

fn icon_color(response: &Response) -> Color32 {
    if response.hovered() { TEXT } else { MUTED }
}

/// Centered text for empty lists and placeholders.
pub fn empty_hint(ui: &mut egui::Ui, text: &str) {
    let rect = ui.available_rect_before_wrap();
    ui.painter().text(
        pos2(rect.center().x, rect.top() + 28.0),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(12.5),
        FAINT,
    );
}
