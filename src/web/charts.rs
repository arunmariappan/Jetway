//! Server-rendered SVG charts (phase 4.3). Colours are CSS classes, so light and dark themes need no redraw. Marks
//! follow the dashboard's data-viz rules: 2px lines with end dots ringed in the surface colour, columns at most 24px
//! wide with a 4px rounded top and a 2px surface gap between segments, solid hairline grids. Each time slot has a
//! hit area wider than its marks: hovering or focusing it shows a crosshair (CSS) and a native tooltip listing every
//! series, so no JavaScript is needed; every chart also has a table view in its template.

use std::fmt::Write as _;

const WIDTH: f64 = 640.0;
const HEIGHT: f64 = 220.0;
const LEFT: f64 = 52.0;
const RIGHT: f64 = 60.0;
const TOP: f64 = 12.0;
const BOTTOM: f64 = 28.0;
const BAR_MAX: f64 = 24.0;
const GAP: f64 = 2.0;
const RADIUS: f64 = 4.0;

/// How values read on the axis, labels and tooltips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Millis,
    Count,
}

impl Unit {
    pub fn format(self, value: f64) -> String {
        match self {
            Self::Millis if value >= 1000.0 => format!("{:.1} s", value / 1000.0),
            Self::Millis if value >= 10.0 => format!("{value:.0} ms"),
            Self::Millis if value >= 1.0 || value == 0.0 => format!("{value:.1} ms"),
            // Cache hits take a fraction of a millisecond.
            Self::Millis => format!("{value:.2} ms"),
            Self::Count => compact(value),
        }
    }
}

/// 1,284 → 1.3K; whole numbers below 1 000 as they are.
pub fn compact(value: f64) -> String {
    let abs = value.abs();
    if abs >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.1}K", value / 1_000.0)
    } else if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// One series: a CSS class for its colour, and a value per time slot (`None` where there was nothing to measure).
#[derive(Debug, Clone)]
pub struct Series {
    pub name: String,
    pub class: &'static str,
    pub values: Vec<Option<f64>>,
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A clean axis maximum (1, 2, 2.5 or 5 times a power of ten) and its tick step, four ticks above zero.
fn nice_scale(max: f64) -> (f64, f64) {
    if max <= 0.0 || !max.is_finite() {
        return (1.0, 0.25);
    }
    let raw_step = max / 4.0;
    let magnitude = 10f64.powf(raw_step.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0]
        .into_iter()
        .map(|m| m * magnitude)
        .find(|s| s * 4.0 >= max)
        .unwrap_or(10.0 * magnitude);
    (step * 4.0, step)
}

struct Frame {
    slots: usize,
    max: f64,
    step: f64,
}

impl Frame {
    fn plot_width() -> f64 {
        WIDTH - LEFT - RIGHT
    }

    fn plot_height() -> f64 {
        HEIGHT - TOP - BOTTOM
    }

    fn slot_width(&self) -> f64 {
        Self::plot_width() / self.slots.max(1) as f64
    }

    fn x(&self, slot: usize) -> f64 {
        LEFT + self.slot_width() * (slot as f64 + 0.5)
    }

    fn y(&self, value: f64) -> f64 {
        TOP + Self::plot_height() * (1.0 - (value / self.max).clamp(0.0, 1.0))
    }
}

fn open(out: &mut String, label: &str) {
    let _ = write!(
        out,
        r#"<svg class="chart" viewBox="0 0 {WIDTH} {HEIGHT}" role="img" aria-label="{}" preserveAspectRatio="xMidYMid meet">"#,
        escape(label)
    );
}

/// Gridlines, y ticks, the baseline and x labels.
fn axes(out: &mut String, frame: &Frame, labels: &[String], unit: Unit) {
    let mut tick = 0.0;
    while tick <= frame.max + 1e-9 {
        let y = frame.y(tick);
        let class = if tick == 0.0 { "axis" } else { "grid" };
        let _ = write!(out, r#"<line class="{class}" x1="{LEFT}" x2="{}" y1="{y:.1}" y2="{y:.1}"/>"#, WIDTH - RIGHT);
        let _ = write!(
            out,
            r#"<text class="tick" x="{}" y="{:.1}" text-anchor="end">{}</text>"#,
            LEFT - 6.0,
            y + 4.0,
            unit.format(tick)
        );
        tick += frame.step;
    }
    let every = labels.len().div_ceil(6).max(1);
    for (slot, label) in labels.iter().enumerate() {
        if slot % every == 0 {
            let _ = write!(
                out,
                r#"<text class="tick" x="{:.1}" y="{}" text-anchor="middle">{}</text>"#,
                frame.x(slot),
                HEIGHT - 8.0,
                escape(label)
            );
        }
    }
}

/// Per-slot hit areas: a crosshair on hover or focus, and a tooltip with every series at that time.
fn hits(out: &mut String, frame: &Frame, labels: &[String], series: &[Series], unit: Unit) {
    let width = frame.slot_width();
    for (slot, label) in labels.iter().enumerate() {
        let mut title = escape(label);
        for s in series {
            let value = s.values.get(slot).copied().flatten().map_or_else(|| "–".to_string(), |v| unit.format(v));
            let _ = write!(title, "\n{value}  {}", escape(&s.name));
        }
        let x = frame.x(slot);
        let _ = write!(
            out,
            r#"<g class="hit" tabindex="0"><title>{title}</title><rect x="{:.1}" y="{TOP}" width="{width:.1}" height="{:.1}"/><line class="cross" x1="{x:.1}" x2="{x:.1}" y1="{TOP}" y2="{:.1}"/></g>"#,
            x - width / 2.0,
            Frame::plot_height(),
            TOP + Frame::plot_height()
        );
    }
}

/// A line chart of several series over the same time slots.
pub fn lines(label: &str, labels: &[String], series: &[Series], unit: Unit) -> String {
    let max = series.iter().flat_map(|s| s.values.iter().flatten()).copied().fold(0.0, f64::max);
    let (max, step) = nice_scale(max);
    let frame = Frame { slots: labels.len(), max, step };
    let mut out = String::new();
    open(&mut out, label);
    axes(&mut out, &frame, labels, unit);

    let mut end_labels: Vec<(f64, String)> = Vec::new();
    for s in series {
        // A path per unbroken run; a run of one point is drawn as a dot.
        let mut run: Vec<(f64, f64)> = Vec::new();
        let flush = |run: &mut Vec<(f64, f64)>, out: &mut String| {
            if run.len() == 1 {
                let (x, y) = run[0];
                let _ = write!(out, r#"<circle class="dot {}" cx="{x:.1}" cy="{y:.1}" r="4"/>"#, s.class);
            } else if run.len() > 1 {
                let mut d = String::new();
                for (i, (x, y)) in run.iter().enumerate() {
                    let _ = write!(d, "{}{x:.1} {y:.1} ", if i == 0 { "M" } else { "L" });
                }
                let _ = write!(out, r#"<path class="line {}" d="{}"/>"#, s.class, d.trim_end());
            }
            run.clear();
        };
        for (slot, value) in s.values.iter().enumerate() {
            match value {
                Some(v) => run.push((frame.x(slot), frame.y(*v))),
                None => flush(&mut run, &mut out),
            }
        }
        flush(&mut run, &mut out);
        // An end dot and a direct label at the latest value.
        if let Some((slot, value)) = s.values.iter().enumerate().rev().find_map(|(i, v)| v.map(|v| (i, v))) {
            let (x, y) = (frame.x(slot), frame.y(value));
            let _ = write!(out, r#"<circle class="dot {}" cx="{x:.1}" cy="{y:.1}" r="4"/>"#, s.class);
            if slot + 1 == s.values.len() {
                end_labels.push((y, format!("{} {}", unit.format(value), s.name)));
            }
        }
    }
    // Labels that would collide are left to the legend and tooltip rather than nudged away from their lines.
    end_labels.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut last_y = f64::NEG_INFINITY;
    for (y, text) in end_labels {
        if y - last_y >= 13.0 {
            let _ = write!(
                out,
                r#"<text class="end" x="{}" y="{:.1}">{}</text>"#,
                WIDTH - RIGHT + 8.0,
                y + 4.0,
                escape(&text)
            );
            last_y = y;
        }
    }
    hits(&mut out, &frame, labels, series, unit);
    out.push_str("</svg>");
    out
}

/// A column per time slot, its series stacked in order from the baseline.
pub fn stacked(label: &str, labels: &[String], series: &[Series], unit: Unit) -> String {
    let totals: Vec<f64> = (0..labels.len())
        .map(|slot| series.iter().filter_map(|s| s.values.get(slot).copied().flatten()).sum())
        .collect();
    let (max, step) = nice_scale(totals.iter().copied().fold(0.0, f64::max));
    let frame = Frame { slots: labels.len(), max, step };
    let mut out = String::new();
    open(&mut out, label);
    axes(&mut out, &frame, labels, unit);

    let width = (frame.slot_width() - GAP).clamp(1.0, BAR_MAX);
    for slot in 0..labels.len() {
        let x = frame.x(slot) - width / 2.0;
        let mut base = TOP + Frame::plot_height();
        let segments: Vec<(&Series, f64)> = series
            .iter()
            .filter_map(|s| s.values.get(slot).copied().flatten().filter(|v| *v > 0.0).map(|v| (s, v)))
            .collect();
        for (index, (s, value)) in segments.iter().enumerate() {
            let full = Frame::plot_height() * (value / frame.max);
            // The surface gap separates a segment from the one below it.
            let height = if index == 0 { full } else { (full - GAP).max(0.5) };
            let top = base - full;
            let y = top;
            if index + 1 == segments.len() {
                let r = RADIUS.min(height / 2.0).min(width / 2.0);
                let bottom = y + height;
                let _ = write!(
                    out,
                    r#"<path class="bar {}" d="M{x:.1} {bottom:.1} V{:.1} Q{x:.1} {y:.1} {:.1} {y:.1} H{:.1} Q{:.1} {y:.1} {:.1} {:.1} V{bottom:.1} Z"/>"#,
                    s.class,
                    y + r,
                    x + r,
                    x + width - r,
                    x + width,
                    x + width,
                    y + r
                );
            } else {
                let _ = write!(
                    out,
                    r#"<rect class="bar {}" x="{x:.1}" y="{y:.1}" width="{width:.1}" height="{height:.1}"/>"#,
                    s.class
                );
            }
            base = top;
        }
    }
    hits(&mut out, &frame, labels, series, unit);
    out.push_str("</svg>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_scales() {
        assert_eq!(nice_scale(0.0), (1.0, 0.25));
        assert_eq!(nice_scale(87.0), (100.0, 25.0));
        assert_eq!(nice_scale(950.0), (1000.0, 250.0));
        assert_eq!(nice_scale(31.0), (40.0, 10.0));
        assert_eq!(nice_scale(7.0), (8.0, 2.0));
    }

    #[test]
    fn formats() {
        assert_eq!(Unit::Millis.format(4.25), "4.2 ms");
        assert_eq!(Unit::Millis.format(0.043), "0.04 ms");
        assert_eq!(Unit::Millis.format(0.0), "0.0 ms");
        assert_eq!(Unit::Millis.format(120.4), "120 ms");
        assert_eq!(Unit::Millis.format(6486.0), "6.5 s");
        assert_eq!(Unit::Count.format(1284.0), "1.3K");
        assert_eq!(Unit::Count.format(12.0), "12");
    }

    fn series() -> Vec<Series> {
        vec![
            Series { name: "p50".into(), class: "o1", values: vec![Some(10.0), None, Some(30.0), Some(20.0)] },
            Series { name: "p95".into(), class: "o2", values: vec![Some(40.0), None, Some(90.0), Some(80.0)] },
        ]
    }

    #[test]
    fn line_charts_are_well_formed_svg_with_gaps_and_hit_areas() {
        let labels: Vec<String> = ["10:00", "10:01", "10:02", "10:03"].map(String::from).to_vec();
        let svg = lines("Latency", &labels, &series(), Unit::Millis);
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
        // The gap at 10:01 splits each line: a lone dot, then a path.
        assert_eq!(svg.matches(r#"<path class="line o1""#).count(), 1);
        assert_eq!(svg.matches("<g class=\"hit\"").count(), 4);
        assert!(svg.contains("<title>10:02\n30 ms  p50\n90 ms  p95</title>"), "{svg}");
        assert!(svg.contains("80 ms p95"), "an end label");
        assert_eq!(svg.matches('<').count(), svg.matches('>').count());
    }

    #[test]
    fn stacked_columns_round_only_the_top_segment() {
        let labels: Vec<String> = ["a", "b"].map(String::from).to_vec();
        let series = vec![
            Series { name: "ok".into(), class: "s1", values: vec![Some(5.0), Some(2.0)] },
            Series { name: "errors".into(), class: "critical", values: vec![Some(1.0), None] },
        ];
        let svg = stacked("Requests", &labels, &series, Unit::Count);
        assert_eq!(svg.matches(r#"<rect class="bar s1""#).count(), 1, "slot a: ok sits under errors");
        assert_eq!(svg.matches(r#"<path class="bar"#).count(), 2, "each column's top segment is rounded");
        assert!(!svg.contains("NaN"));
    }
}
