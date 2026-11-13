//! Chart rendering — draws OOXML charts onto a canvas context.
//!
//! Parses chart XML via ooxml-dml's typed API and renders directly
//! to the HTML5 Canvas 2D context. Supports bar, line, pie, scatter,
//! area, doughnut, and radar charts.
//!
//! Entry point: `render_chart()` takes the chart XML bytes, a canvas
//! context, and a bounding rectangle, and draws the chart.

use crate::engine::paint::Painter;

// ─── Public API ─────────────────────────────────────────────────────────────

/// Render a chart from its XML bytes into the given canvas region.
///
/// Returns `true` if the chart was rendered, `false` if unsupported or empty.
pub fn render_chart(
    chart_xml: &[u8],
    p: &dyn Painter,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> bool {
    let chart_space = match parse_chart_space(chart_xml) {
        Some(cs) => cs,
        None => return false,
    };

    let chart_data = extract_chart_data(&chart_space);
    if chart_data.series.is_empty() {
        return false;
    }

    let plot = PlotRect::new(x, y, width, height, &chart_data);

    match chart_data.kind {
        ChartType::Bar => render_bar_chart(p, &plot, &chart_data),
        ChartType::Line => render_line_chart(p, &plot, &chart_data),
        ChartType::Pie => render_pie_chart(p, &plot, &chart_data),
        ChartType::Scatter => render_scatter_chart(p, &plot, &chart_data),
        ChartType::Area => render_area_chart(p, &plot, &chart_data),
        ChartType::Doughnut => render_doughnut_chart(p, &plot, &chart_data),
        ChartType::Radar => render_radar_chart(p, &plot, &chart_data),
    }
    true
}

/// Chart copy data — the SAME parsed model the painter uses,
/// re-shaped for the clipboard TSV/data exports: series `(name, values)` in
/// chart order + the category labels. NO new XML parsing — this reuses
/// `parse_chart_space` + `extract_chart_data` (the painter's exact pipeline),
/// so the copied numbers are always the numbers that were painted. `None` when
/// the XML fails to parse or carries no series (mirrors `render_chart`'s guard).
pub(crate) struct ChartCopyData {
    pub(crate) categories: Vec<String>,
    pub(crate) series: Vec<(String, Vec<f64>)>,
}

/// Extract the copy model from chart XML bytes (see `ChartCopyData`).
pub(crate) fn copy_data(chart_xml: &[u8]) -> Option<ChartCopyData> {
    let chart_space = parse_chart_space(chart_xml)?;
    let data = extract_chart_data(&chart_space);
    if data.series.is_empty() {
        return None;
    }
    Some(ChartCopyData {
        categories: data.categories,
        series: data
            .series
            .into_iter()
            .map(|s| (s.name, s.values))
            .collect(),
    })
}

// ─── Internal types ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum ChartType {
    Bar,
    Line,
    Pie,
    Scatter,
    Area,
    Doughnut,
    Radar,
}

// `title`/`bar_dir_horizontal`/`bar_grouping_stacked` are parsed and retained
// but not yet consumed by the chart painter (partially-wired chart feature).
#[allow(dead_code)]
struct ChartData {
    kind: ChartType,
    series: Vec<Series>,
    categories: Vec<String>,
    title: Option<String>,
    legend_entries: Vec<(String, String)>, // (name, color)
    bar_dir_horizontal: bool,
    bar_grouping_stacked: bool,
}

struct Series {
    name: String,
    values: Vec<f64>,
    color: String,
}

/// The plot area with margins for axes and legend.
struct PlotRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    val_min: f64,
    val_max: f64,
}

const AXIS_MARGIN_LEFT: f64 = 40.0;
const AXIS_MARGIN_BOTTOM: f64 = 30.0;
const AXIS_MARGIN_TOP: f64 = 10.0;
const LEGEND_WIDTH: f64 = 90.0;
const CHART_FONT: &str = "11px -apple-system, BlinkMacSystemFont, sans-serif";
const AXIS_COLOR: &str = "#b3b3b3";
const GRID_COLOR: &str = "#e0e0e0";

const DEFAULT_COLORS: &[&str] = &[
    "#4472c4", "#ed7d31", "#a5a5a5", "#ffc000", "#5b9bd5",
    "#70ad47", "#264478", "#9b57a0", "#636363", "#eb6c0a",
];

impl PlotRect {
    fn new(x: f64, y: f64, w: f64, h: f64, data: &ChartData) -> Self {
        let has_legend = !data.legend_entries.is_empty();
        let legend_w = if has_legend { LEGEND_WIDTH } else { 0.0 };

        let is_cartesian = matches!(
            data.kind,
            ChartType::Bar | ChartType::Line | ChartType::Scatter | ChartType::Area
        );

        let (px, py, pw, ph) = if is_cartesian {
            (
                x + AXIS_MARGIN_LEFT,
                y + AXIS_MARGIN_TOP,
                w - AXIS_MARGIN_LEFT - legend_w - 10.0,
                h - AXIS_MARGIN_TOP - AXIS_MARGIN_BOTTOM,
            )
        } else {
            (
                x + 10.0,
                y + AXIS_MARGIN_TOP,
                w - legend_w - 20.0,
                h - AXIS_MARGIN_TOP - 10.0,
            )
        };

        let (val_min, val_max) = compute_axis_range(data);

        Self { x: px, y: py, w: pw, h: ph, val_min, val_max }
    }

    fn val_to_y(&self, val: f64) -> f64 {
        let range = self.val_max - self.val_min;
        if range == 0.0 {
            return self.y + self.h / 2.0;
        }
        self.y + self.h - ((val - self.val_min) / range) * self.h
    }
}

// ─── Parsing ────────────────────────────────────────────────────────────────

fn parse_chart_space(xml: &[u8]) -> Option<ooxml_dml::types::ChartSpace> {
    ooxml_dml::parse_chart_space(xml).ok()
}

fn extract_chart_data(cs: &ooxml_dml::types::ChartSpace) -> ChartData {
    use ooxml_dml::ext::{ChartSpaceExt, ChartExt};

    let plot_area = cs.chart().plot_area();
    let title = cs.title_text();

    // Try each chart type in priority order
    if let Some(bar) = plot_area.bar_chart.first() {
        return extract_bar_data(bar, title);
    }
    if let Some(line) = plot_area.line_chart.first() {
        return extract_line_data(line, title);
    }
    if let Some(pie) = plot_area.pie_chart.first() {
        return extract_pie_data(&pie.ser, title);
    }
    if let Some(pie) = plot_area.pie3_d_chart.first() {
        return extract_pie_data(&pie.ser, title);
    }
    if let Some(scatter) = plot_area.scatter_chart.first() {
        return extract_scatter_data(scatter, title);
    }
    if let Some(area) = plot_area.area_chart.first() {
        return extract_area_data(area, title);
    }
    if let Some(doughnut) = plot_area.doughnut_chart.first() {
        return extract_doughnut_data(doughnut, title);
    }
    if let Some(radar) = plot_area.radar_chart.first() {
        return extract_radar_data(radar, title);
    }

    ChartData {
        kind: ChartType::Bar,
        series: Vec::new(),
        categories: Vec::new(),
        title,
        legend_entries: Vec::new(),
        bar_dir_horizontal: false,
        bar_grouping_stacked: false,
    }
}

fn extract_bar_data(
    bar: &ooxml_dml::types::BarChart,
    title: Option<String>,
) -> ChartData {
    let horizontal = bar.bar_dir.value == Some(ooxml_dml::types::BarDirectionType::Bar);
    let stacked = bar.grouping.as_ref()
        .and_then(|g| g.value.as_ref())
        .map(|v| matches!(v, ooxml_dml::types::BarGroupingType::Stacked | ooxml_dml::types::BarGroupingType::PercentStacked))
        .unwrap_or(false);

    let mut categories = Vec::new();
    let mut series_list = Vec::new();
    let mut legend = Vec::new();

    for (i, ser) in bar.ser.iter().enumerate() {
        let name = series_name(&ser.tx);
        let color = series_color(&ser.sp_pr, i);
        let values = numeric_values(&ser.value);

        if categories.is_empty() {
            categories = category_labels(&ser.cat);
        }

        legend.push((name.clone(), color.clone()));
        series_list.push(Series { name, values, color });
    }

    ChartData {
        kind: ChartType::Bar,
        series: series_list,
        categories,
        title,
        legend_entries: legend,
        bar_dir_horizontal: horizontal,
        bar_grouping_stacked: stacked,
    }
}

fn extract_line_data(line: &ooxml_dml::types::LineChart, title: Option<String>) -> ChartData {
    let mut categories = Vec::new();
    let mut series_list = Vec::new();
    let mut legend = Vec::new();

    for (i, ser) in line.ser.iter().enumerate() {
        let name = series_name(&ser.tx);
        let color = series_color(&ser.sp_pr, i);
        let values = numeric_values(&ser.value);
        if categories.is_empty() { categories = category_labels(&ser.cat); }
        legend.push((name.clone(), color.clone()));
        series_list.push(Series { name, values, color });
    }

    ChartData { kind: ChartType::Line, series: series_list, categories, title, legend_entries: legend, bar_dir_horizontal: false, bar_grouping_stacked: false }
}

fn extract_pie_data(pie_ser: &[ooxml_dml::types::PieSeries], title: Option<String>) -> ChartData {
    let mut categories = Vec::new();
    let mut series_list = Vec::new();
    let mut legend = Vec::new();

    if let Some(ser) = pie_ser.first() {
        let values = numeric_values(&ser.value);
        categories = category_labels(&ser.cat);
        for (i, cat) in categories.iter().enumerate() {
            let color = series_color_by_index(i);
            legend.push((cat.clone(), color.clone()));
        }
        series_list.push(Series {
            name: series_name(&ser.tx),
            values,
            color: String::new(),
        });
    }

    ChartData { kind: ChartType::Pie, series: series_list, categories, title, legend_entries: legend, bar_dir_horizontal: false, bar_grouping_stacked: false }
}

fn extract_scatter_data(scatter: &ooxml_dml::types::ScatterChart, title: Option<String>) -> ChartData {
    let mut series_list = Vec::new();
    let mut legend = Vec::new();

    for (i, ser) in scatter.ser.iter().enumerate() {
        let name = series_name(&ser.tx);
        let color = series_color(&ser.sp_pr, i);
        let values = numeric_values(&ser.y_val);
        legend.push((name.clone(), color.clone()));
        series_list.push(Series { name, values, color });
    }

    ChartData { kind: ChartType::Scatter, series: series_list, categories: Vec::new(), title, legend_entries: legend, bar_dir_horizontal: false, bar_grouping_stacked: false }
}

fn extract_area_data(area: &ooxml_dml::types::AreaChart, title: Option<String>) -> ChartData {
    let mut categories = Vec::new();
    let mut series_list = Vec::new();
    let mut legend = Vec::new();

    for (i, ser) in area.ser.iter().enumerate() {
        let name = series_name(&ser.tx);
        let color = series_color(&ser.sp_pr, i);
        let values = numeric_values(&ser.value);
        if categories.is_empty() { categories = category_labels(&ser.cat); }
        legend.push((name.clone(), color.clone()));
        series_list.push(Series { name, values, color });
    }

    ChartData { kind: ChartType::Area, series: series_list, categories, title, legend_entries: legend, bar_dir_horizontal: false, bar_grouping_stacked: false }
}

fn extract_doughnut_data(doughnut: &ooxml_dml::types::DoughnutChart, title: Option<String>) -> ChartData {
    let mut categories = Vec::new();
    let mut series_list = Vec::new();
    let mut legend = Vec::new();

    if let Some(ser) = doughnut.ser.first() {
        let values = numeric_values(&ser.value);
        categories = category_labels(&ser.cat);
        for (i, cat) in categories.iter().enumerate() {
            let color = series_color_by_index(i);
            legend.push((cat.clone(), color.clone()));
        }
        series_list.push(Series { name: series_name(&ser.tx), values, color: String::new() });
    }

    ChartData { kind: ChartType::Doughnut, series: series_list, categories, title, legend_entries: legend, bar_dir_horizontal: false, bar_grouping_stacked: false }
}

fn extract_radar_data(radar: &ooxml_dml::types::RadarChart, title: Option<String>) -> ChartData {
    let mut categories = Vec::new();
    let mut series_list = Vec::new();
    let mut legend = Vec::new();

    for (i, ser) in radar.ser.iter().enumerate() {
        let name = series_name(&ser.tx);
        let color = series_color(&ser.sp_pr, i);
        let values = numeric_values(&ser.value);
        if categories.is_empty() { categories = category_labels(&ser.cat); }
        legend.push((name.clone(), color.clone()));
        series_list.push(Series { name, values, color });
    }

    ChartData { kind: ChartType::Radar, series: series_list, categories, title, legend_entries: legend, bar_dir_horizontal: false, bar_grouping_stacked: false }
}

// ─── Data extraction helpers ────────────────────────────────────────────────

fn series_name(tx: &Option<Box<ooxml_dml::types::SeriesText>>) -> String {
    tx.as_ref()
        .and_then(|t| t.str_ref.as_ref())
        .and_then(|sr| sr.str_cache.as_ref())
        .and_then(|sc| sc.pt.first())
        .map(|pt| pt.v.clone())
        .unwrap_or_default()
}

fn series_color(sp_pr: &Option<Box<ooxml_dml::types::CTShapeProperties>>, fallback_idx: usize) -> String {
    sp_pr.as_ref()
        .and_then(|sp| sp.fill_properties.as_ref())
        .and_then(|fp| match fp.as_ref() {
            ooxml_dml::types::EGFillProperties::SolidFill(sf) => sf.color_choice.as_ref(),
            _ => None,
        })
        .and_then(|cc| match cc.as_ref() {
            ooxml_dml::types::EGColorChoice::SrgbClr(c) => {
                let hex: String = c.value.iter().map(|b| format!("{:02x}", b)).collect();
                Some(format!("#{}", hex))
            }
            _ => None,
        })
        .unwrap_or_else(|| series_color_by_index(fallback_idx))
}

fn series_color_by_index(idx: usize) -> String {
    DEFAULT_COLORS[idx % DEFAULT_COLORS.len()].to_string()
}

fn numeric_values(val: &Option<Box<ooxml_dml::types::NumericDataSource>>) -> Vec<f64> {
    val.as_ref()
        .and_then(|v| v.num_ref.as_ref())
        .and_then(|nr| nr.num_cache.as_ref())
        .map(|nc| {
            nc.pt.iter()
                .map(|pt| pt.v.parse::<f64>().unwrap_or(0.0))
                .collect()
        })
        .or_else(|| {
            val.as_ref()
                .and_then(|v| v.num_lit.as_ref())
                .map(|nl| nl.pt.iter().map(|pt| pt.v.parse::<f64>().unwrap_or(0.0)).collect())
        })
        .unwrap_or_default()
}

fn category_labels(cat: &Option<Box<ooxml_dml::types::AxisDataSource>>) -> Vec<String> {
    cat.as_ref()
        .and_then(|c| c.str_ref.as_ref())
        .and_then(|sr| sr.str_cache.as_ref())
        .map(|sc| sc.pt.iter().map(|pt| pt.v.to_string()).collect())
        .unwrap_or_default()
}

fn compute_axis_range(data: &ChartData) -> (f64, f64) {
    let mut min = f64::MAX;
    let mut max = f64::MIN;
    for s in &data.series {
        for &v in &s.values {
            if v < min { min = v; }
            if v > max { max = v; }
        }
    }
    if min == f64::MAX { min = 0.0; }
    if max == f64::MIN { max = 1.0; }
    if min > 0.0 { min = 0.0; }

    let range = max - min;
    let step = nice_step(range);
    // Excel/Word leave headroom so the tallest bar never touches the axis top:
    // a data max of 9.6 yields an axis max of 12, not the bare ceil() of 10. A
    // small (~5%) headroom factor before the ceil reproduces that, and does so
    // surgically — it only bumps a chart whose max sits within ~5% of its nice
    // ceiling (e.g. 9.6→12); charts with comfortable clearance (max 7 → 8) are
    // unchanged because 7·1.05 still ceils to 8.
    let nice_max = if max > 0.0 {
        ((max * 1.05) / step).ceil() * step
    } else {
        (max / step).ceil() * step
    };
    let nice_min = (min / step).floor() * step;
    (nice_min, nice_max)
}

fn nice_step(range: f64) -> f64 {
    if range <= 0.0 { return 1.0; }
    let rough = range / 5.0;
    let mag = 10.0_f64.powf(rough.log10().floor());
    let norm = rough / mag;
    let step = if norm <= 1.5 { 1.0 }
        else if norm <= 3.0 { 2.0 }
        else if norm <= 7.0 { 5.0 }
        else { 10.0 };
    step * mag
}

// ─── Renderers ──────────────────────────────────────────────────────────────

fn render_bar_chart(p: &dyn Painter, plot: &PlotRect, data: &ChartData) {
    draw_axes_and_grid(p, plot, data);

    let n_cats = data.categories.len().max(1);
    let n_ser = data.series.len().max(1);
    let group_width = plot.w / n_cats as f64;
    let gap = group_width * 0.2;
    let bar_area = group_width - gap;
    let bar_width = bar_area / n_ser as f64;
    let zero_y = plot.val_to_y(0.0);

    for (si, ser) in data.series.iter().enumerate() {
        p.set_fill_style_str(&ser.color);
        for (ci, &val) in ser.values.iter().enumerate() {
            let x = plot.x + ci as f64 * group_width + gap / 2.0 + si as f64 * bar_width;
            let val_y = plot.val_to_y(val);
            let bar_h = zero_y - val_y;
            if bar_h >= 0.0 {
                p.fill_rect(x, val_y, bar_width - 1.0, bar_h);
            } else {
                p.fill_rect(x, zero_y, bar_width - 1.0, -bar_h);
            }
        }
    }

    draw_legend(p, plot, data);
}

fn render_line_chart(p: &dyn Painter, plot: &PlotRect, data: &ChartData) {
    draw_axes_and_grid(p, plot, data);

    let n_cats = data.categories.len().max(1);

    for ser in &data.series {
        p.set_stroke_style_str(&ser.color);
        p.set_line_width(2.0);
        p.begin_path();
        for (i, &val) in ser.values.iter().enumerate() {
            let x = plot.x + (i as f64 + 0.5) * (plot.w / n_cats as f64);
            let y = plot.val_to_y(val);
            if i == 0 { p.move_to(x, y); } else { p.line_to(x, y); }
        }
        p.stroke();

        p.set_fill_style_str(&ser.color);
        for (i, &val) in ser.values.iter().enumerate() {
            let x = plot.x + (i as f64 + 0.5) * (plot.w / n_cats as f64);
            let y = plot.val_to_y(val);
            p.begin_path();
            let _ = p.arc(x, y, 3.0, 0.0, std::f64::consts::TAU);
            p.fill();
        }
    }

    draw_legend(p, plot, data);
}

fn render_pie_chart(p: &dyn Painter, plot: &PlotRect, data: &ChartData) {
    let Some(ser) = data.series.first() else { return };
    let total: f64 = ser.values.iter().sum();
    if total <= 0.0 { return; }

    let cx = plot.x + plot.w / 2.0;
    let cy = plot.y + plot.h / 2.0;
    let r = (plot.w.min(plot.h) / 2.0) - 5.0;

    draw_arc_chart(p, cx, cy, r, 0.0, &ser.values, total, &data.legend_entries);
    draw_legend(p, plot, data);
}

fn render_doughnut_chart(p: &dyn Painter, plot: &PlotRect, data: &ChartData) {
    let Some(ser) = data.series.first() else { return };
    let total: f64 = ser.values.iter().sum();
    if total <= 0.0 { return; }

    let cx = plot.x + plot.w / 2.0;
    let cy = plot.y + plot.h / 2.0;
    let r = (plot.w.min(plot.h) / 2.0) - 5.0;
    let inner_r = r * 0.5;

    draw_arc_chart(p, cx, cy, r, inner_r, &ser.values, total, &data.legend_entries);
    draw_legend(p, plot, data);
}

fn render_scatter_chart(p: &dyn Painter, plot: &PlotRect, data: &ChartData) {
    draw_axes_and_grid(p, plot, data);

    for ser in &data.series {
        p.set_fill_style_str(&ser.color);
        for (i, &val) in ser.values.iter().enumerate() {
            let x = plot.x + (i as f64 + 0.5) * (plot.w / ser.values.len().max(1) as f64);
            let y = plot.val_to_y(val);
            p.begin_path();
            let _ = p.arc(x, y, 4.0, 0.0, std::f64::consts::TAU);
            p.fill();
        }
    }

    draw_legend(p, plot, data);
}

fn render_area_chart(p: &dyn Painter, plot: &PlotRect, data: &ChartData) {
    draw_axes_and_grid(p, plot, data);

    let n_cats = data.categories.len().max(data.series.first().map(|s| s.values.len()).unwrap_or(1));
    let zero_y = plot.val_to_y(0.0);

    for ser in &data.series {
        p.set_fill_style_str(&ser.color);
        p.set_global_alpha(0.5);
        p.begin_path();
        p.move_to(plot.x, zero_y);
        for (i, &val) in ser.values.iter().enumerate() {
            let x = plot.x + (i as f64 + 0.5) * (plot.w / n_cats as f64);
            let y = plot.val_to_y(val);
            p.line_to(x, y);
        }
        let last_x = plot.x + (ser.values.len() as f64 - 0.5) * (plot.w / n_cats as f64);
        p.line_to(last_x, zero_y);
        p.close_path();
        p.fill();
        p.set_global_alpha(1.0);
    }

    draw_legend(p, plot, data);
}

fn render_radar_chart(p: &dyn Painter, plot: &PlotRect, data: &ChartData) {
    let n_cats = data.categories.len().max(1);
    let cx = plot.x + plot.w / 2.0;
    let cy = plot.y + plot.h / 2.0;
    let r = (plot.w.min(plot.h) / 2.0) - 20.0;
    let angle_step = std::f64::consts::TAU / n_cats as f64;

    // Grid rings
    p.set_stroke_style_str(GRID_COLOR);
    p.set_line_width(0.5);
    for ring in 1..=5 {
        let rr = r * ring as f64 / 5.0;
        p.begin_path();
        for i in 0..n_cats {
            let a = -std::f64::consts::FRAC_PI_2 + i as f64 * angle_step;
            let px = cx + rr * a.cos();
            let py = cy + rr * a.sin();
            if i == 0 { p.move_to(px, py); } else { p.line_to(px, py); }
        }
        p.close_path();
        p.stroke();
    }

    // Spokes
    for i in 0..n_cats {
        let a = -std::f64::consts::FRAC_PI_2 + i as f64 * angle_step;
        p.begin_path();
        p.move_to(cx, cy);
        p.line_to(cx + r * a.cos(), cy + r * a.sin());
        p.stroke();
    }

    // Category labels
    p.set_fill_style_str("#333");
    p.set_font(CHART_FONT);
    for (i, cat) in data.categories.iter().enumerate() {
        let a = -std::f64::consts::FRAC_PI_2 + i as f64 * angle_step;
        let lx = cx + (r + 12.0) * a.cos();
        let ly = cy + (r + 12.0) * a.sin();
        p.set_text_align("center");
        let _ = p.fill_text(cat, lx, ly + 4.0);
    }

    // Data series
    let range = plot.val_max - plot.val_min;
    for ser in &data.series {
        p.set_stroke_style_str(&ser.color);
        p.set_fill_style_str(&ser.color);
        p.set_line_width(2.0);
        p.set_global_alpha(0.3);
        p.begin_path();
        for (i, &val) in ser.values.iter().enumerate() {
            let norm = if range > 0.0 { (val - plot.val_min) / range } else { 0.5 };
            let a = -std::f64::consts::FRAC_PI_2 + i as f64 * angle_step;
            let px = cx + r * norm * a.cos();
            let py = cy + r * norm * a.sin();
            if i == 0 { p.move_to(px, py); } else { p.line_to(px, py); }
        }
        p.close_path();
        p.fill();
        p.set_global_alpha(1.0);
        p.stroke();
    }

    draw_legend(p, plot, data);
}

// ─── Shared drawing helpers ─────────────────────────────────────────────────

fn draw_axes_and_grid(p: &dyn Painter, plot: &PlotRect, data: &ChartData) {
    let step = nice_step(plot.val_max - plot.val_min);
    p.set_font(CHART_FONT);

    // Horizontal grid lines + value labels
    p.set_stroke_style_str(GRID_COLOR);
    p.set_fill_style_str("#666");
    p.set_text_align("right");
    p.set_text_baseline("middle");
    p.set_line_width(0.5);

    let mut val = plot.val_min;
    while val <= plot.val_max + step * 0.001 {
        let y = plot.val_to_y(val);
        p.begin_path();
        p.move_to(plot.x, y);
        p.line_to(plot.x + plot.w, y);
        p.stroke();

        let label = if val == val.floor() { format!("{}", val as i64) } else { format!("{:.1}", val) };
        let _ = p.fill_text(&label, plot.x - 5.0, y);
        val += step;
    }

    // Axis lines
    p.set_stroke_style_str(AXIS_COLOR);
    p.set_line_width(1.0);
    let zero_y = plot.val_to_y(0.0);
    p.begin_path();
    p.move_to(plot.x, zero_y);
    p.line_to(plot.x + plot.w, zero_y);
    p.stroke();
    p.begin_path();
    p.move_to(plot.x, plot.y);
    p.line_to(plot.x, plot.y + plot.h);
    p.stroke();

    // Category labels
    p.set_fill_style_str("#333");
    p.set_text_align("center");
    p.set_text_baseline("top");
    let n_cats = data.categories.len().max(1);
    for (i, cat) in data.categories.iter().enumerate() {
        let x = plot.x + (i as f64 + 0.5) * (plot.w / n_cats as f64);
        let _ = p.fill_text(cat, x, plot.y + plot.h + 5.0);
    }
}

// Geometry-primitive arg list (center/radii/values/legend); a params struct
// would not read more clearly than the positional call.
#[allow(clippy::too_many_arguments)]
fn draw_arc_chart(
    p: &dyn Painter,
    cx: f64, cy: f64, r: f64, inner_r: f64,
    values: &[f64], total: f64,
    legend: &[(String, String)],
) {
    let mut start_angle = -std::f64::consts::FRAC_PI_2;
    for (i, &val) in values.iter().enumerate() {
        let sweep = (val / total) * std::f64::consts::TAU;
        let color = legend.get(i).map(|(_, c)| c.as_str()).unwrap_or(DEFAULT_COLORS[i % DEFAULT_COLORS.len()]);
        p.set_fill_style_str(color);
        p.begin_path();
        if inner_r > 0.0 {
            let _ = p.arc(cx, cy, r, start_angle, start_angle + sweep);
            let _ = p.arc_with_anticlockwise(cx, cy, inner_r, start_angle + sweep, start_angle, true);
        } else {
            p.move_to(cx, cy);
            let _ = p.arc(cx, cy, r, start_angle, start_angle + sweep);
        }
        p.close_path();
        p.fill();
        start_angle += sweep;
    }
}

fn draw_legend(p: &dyn Painter, plot: &PlotRect, data: &ChartData) {
    if data.legend_entries.is_empty() { return; }

    let lx = plot.x + plot.w + 15.0;
    let mut ly = plot.y + 10.0;

    p.set_font(CHART_FONT);
    p.set_text_align("left");
    p.set_text_baseline("middle");

    for (name, color) in &data.legend_entries {
        p.set_fill_style_str(color);
        p.fill_rect(lx, ly, 10.0, 10.0);
        p.set_fill_style_str("#333");
        let _ = p.fill_text(name, lx + 14.0, ly + 5.0);
        ly += 18.0;
    }
}
