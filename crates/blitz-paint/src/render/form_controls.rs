use super::ElementCx;
use crate::color::{Color, ToColorColor as _};
use anyrender::{PaintScene, ScenePart};
use blitz_dom::local_name;
use kurbo::{Affine, BezPath, Cap, Circle, Join, Point, RoundedRect, Stroke, Vec2};
use peniko::Fill;
use style::dom::TElement as _;

impl ElementCx<'_, '_> {
    pub(super) fn draw_input(&self, scene: &mut impl PaintScene) {
        // A closed <select> gets a UA dropdown chevron on
        // the right so it reads as a popup control (the label text
        // is a synthetic child; see set_select_label).
        if self.node.local_name() == "select" {
            let multiple = self.node.attr(local_name!("multiple")).is_some();
            let size_gt_1 = self
                .node
                .attr(local_name!("size"))
                .and_then(|v| v.parse::<i32>().ok())
                .map(|n| n > 1)
                .unwrap_or(false);
            if !multiple && !size_gt_1 {
                draw_select_chevron(
                    scene,
                    self.frame.border_box,
                    self.transform,
                    self.style.clone_color().as_srgb_color(),
                );
            }
            return;
        }
        if self.node.local_name() != "input" {
            return;
        }
        let disabled = self.node.attr(local_name!("disabled")).is_some();

        // CSS accent-color drives the widget color like real
        // browsers. `auto` falls back to Chrome's default light-mode
        // accent blue rather than the element text color, which
        // would paint black checkboxes and sliders on most pages.
        const DEFAULT_ACCENT: Color = Color::from_rgba8(0, 117, 255, 255);
        let accent_color = if disabled {
            Color::from_rgba8(209, 209, 209, 255)
        } else {
            match &self.style.get_inherited_ui().accent_color {
                style::values::computed::ColorOrAuto::Auto => DEFAULT_ACCENT,
                style::values::computed::ColorOrAuto::Color(c) => {
                    let current = self.style.clone_color();
                    c.resolve_to_absolute(&current).as_srgb_color()
                }
            }
        };

        // Range sliders carry their live value in
        // SpecialElementData::RangeInput (seeded at construct,
        // updated by the embedder). Handle them before the
        // checkbox guard, since sliders have no checked state.
        if let Some(value) = self.element.range_input_value() {
            let parse = |name| {
                self.node
                    .attr(name)
                    .and_then(|v: &str| v.parse::<f64>().ok())
            };
            let min = parse(local_name!("min")).unwrap_or(0.0);
            let max = parse(local_name!("max")).unwrap_or(100.0).max(min);
            let span = max - min;
            let fraction = if span > 0.0 {
                ((value - min) / span).clamp(0.0, 1.0)
            } else {
                0.5
            };
            draw_range_slider(
                scene,
                fraction,
                self.frame.border_box,
                self.transform,
                accent_color,
            );
            return;
        }

        let Some(checked) = self.element.checkbox_input_checked() else {
            return;
        };

        let type_attr = self.node.attr(local_name!("type"));

        let width = self.frame.border_box.width();
        let height = self.frame.border_box.height();
        let min_dimension = width.min(height);
        let scale = (min_dimension - 4.0).max(0.0) / 16.0;

        let frame = self.frame.border_box.to_rounded_rect(scale * 2.0);

        match type_attr {
            Some("checkbox") => {
                scene.begin_part(self.node.id.as_u64(), ScenePart::Checkbox { checked });
                draw_checkbox(scene, checked, frame, self.transform, accent_color, scale);
                scene.end_part();
            }
            Some("radio") => {
                scene.begin_part(self.node.id.as_u64(), ScenePart::Radio { checked });
                let center = frame.center();
                draw_radio_button(scene, checked, center, self.transform, accent_color, scale);
                scene.end_part();
            }
            _ => {}
        }
    }
}

/// UA paint for `<input type=range>`: a horizontal track
/// with the left (filled) portion in the accent color and a round
/// thumb at `fraction` across the drag span. Geometry mirrors the
/// common UA layout: thumb diameter tracks the box height (capped),
/// the track is a slim centered bar, and the thumb center travels
/// [radius, width - radius] so it never overhangs the border box.
fn draw_range_slider(
    scene: &mut impl PaintScene,
    fraction: f64,
    border_box: kurbo::Rect,
    transform: Affine,
    accent_color: Color,
) {
    let width = border_box.width();
    let height = border_box.height();
    if width <= 0.0 || height <= 0.0 {
        return;
    }
    let cy = border_box.y0 + height / 2.0;
    let thumb_r = (height * 0.5).min(width * 0.25).max(2.0) * 0.8;
    let track_h = (height * 0.25).clamp(2.0, 6.0);
    let track = RoundedRect::new(
        border_box.x0,
        cy - track_h / 2.0,
        border_box.x1,
        cy + track_h / 2.0,
        track_h / 2.0,
    );
    let travel = (width - 2.0 * thumb_r).max(0.0);
    let thumb_cx = border_box.x0 + thumb_r + travel * fraction;
    let filled = RoundedRect::new(
        border_box.x0,
        cy - track_h / 2.0,
        thumb_cx,
        cy + track_h / 2.0,
        track_h / 2.0,
    );

    const TRACK_GRAY: Color = Color::from_rgba8(222, 226, 230, 255);
    scene.fill(Fill::NonZero, transform, TRACK_GRAY, None, &track);
    scene.fill(Fill::NonZero, transform, accent_color, None, &filled);
    scene.fill(
        Fill::NonZero,
        transform,
        accent_color,
        None,
        &Circle::new(Point::new(thumb_cx, cy), thumb_r),
    );
}

/// Downward chevron near the right edge of a closed
/// <select>, vertically centered, sized off the box height.
fn draw_select_chevron(
    scene: &mut impl PaintScene,
    border_box: kurbo::Rect,
    transform: Affine,
    color: Color,
) {
    let h = border_box.height();
    let w = border_box.width();
    if h <= 4.0 || w <= 14.0 {
        return;
    }
    let size = (h * 0.35).clamp(4.0, 9.0);
    let cx = border_box.x1 - size - 4.0;
    let cy = border_box.y0 + h / 2.0;
    let mut path = BezPath::new();
    path.move_to((cx - size / 2.0, cy - size / 4.0));
    path.line_to((cx, cy + size / 4.0));
    path.line_to((cx + size / 2.0, cy - size / 4.0));
    let style = Stroke {
        width: 1.6,
        join: Join::Round,
        miter_limit: 10.0,
        start_cap: Cap::Round,
        end_cap: Cap::Round,
        dash_pattern: Default::default(),
        dash_offset: 0.0,
    };
    scene.stroke(&style, transform, color, None, &path);
}

fn draw_checkbox(
    scene: &mut impl PaintScene,
    checked: bool,
    frame: RoundedRect,
    transform: Affine,
    accent_color: Color,
    scale: f64,
) {
    if checked {
        scene.fill(Fill::NonZero, transform, accent_color, None, &frame);
        //Tick code derived from masonry
        let mut path = BezPath::new();
        path.move_to((2.0, 9.0));
        path.line_to((6.0, 13.0));
        path.line_to((14.0, 2.0));

        path.apply_affine(Affine::translate(Vec2 { x: 2.0, y: 1.0 }).then_scale(scale));

        let style = Stroke {
            width: 2.0 * scale,
            join: Join::Round,
            miter_limit: 10.0,
            start_cap: Cap::Round,
            end_cap: Cap::Round,
            dash_pattern: Default::default(),
            dash_offset: 0.0,
        };

        scene.stroke(&style, transform, Color::WHITE, None, &path);
    } else {
        scene.fill(Fill::NonZero, transform, Color::WHITE, None, &frame);
        scene.stroke(&Stroke::default(), transform, accent_color, None, &frame);
    }
}

fn draw_radio_button(
    scene: &mut impl PaintScene,
    checked: bool,
    center: Point,
    transform: Affine,
    accent_color: Color,
    scale: f64,
) {
    let outer_ring = Circle::new(center, 8.0 * scale);
    let gap = Circle::new(center, 6.0 * scale);
    let inner_circle = Circle::new(center, 4.0 * scale);
    if checked {
        scene.fill(Fill::NonZero, transform, accent_color, None, &outer_ring);
        scene.fill(Fill::NonZero, transform, Color::WHITE, None, &gap);
        scene.fill(Fill::NonZero, transform, accent_color, None, &inner_circle);
    } else {
        const GRAY: Color = color::palette::css::GRAY;
        scene.fill(Fill::NonZero, transform, GRAY, None, &outer_ring);
        scene.fill(Fill::NonZero, transform, Color::WHITE, None, &gap);
    }
}
