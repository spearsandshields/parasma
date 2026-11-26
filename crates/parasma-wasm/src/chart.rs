//! Chart rendering — draws OOXML charts (§21.2) onto a canvas context, for
//! every host (Word, Excel, PowerPoint).
//!
//! Parses chart XML via ooxml-dml's typed API into a [`ChartModel`] and
//! paints it. Supports bar/column (clustered, stacked, 100 % stacked,
//! horizontal), line, area, pie, doughnut, scatter and radar charts; the 3-D
//! bar, line and area types are drawn as their flat counterparts.
//!
//! Entry point: `render_chart()` takes the chart XML bytes, a painter and a
//! bounding rectangle, and draws the chart.

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
    let Some(chart_space) = parse_chart_space(chart_xml) else {
        return false;
    };
    let Some(model) = ChartModel::from_chart_space(&chart_space) else {
        return false;
    };
    model.paint(p, x, y, width, height);
    true
}

/// Chart copy data — the SAME parsed model the painter uses,
/// re-shaped for the clipboard TSV/data exports: series `(name, values)` in
/// chart order + the category labels. NO new XML parsing — this reuses
/// `parse_chart_space` + `ChartModel::from_chart_space` (the painter's exact
/// pipeline), so the copied numbers are always the numbers that were painted.
/// A blank point is `NaN` (the TSV writes an empty cell). `None` when the XML
/// fails to parse or carries no series (mirrors `render_chart`'s guard).
pub(crate) struct ChartCopyData {
    pub(crate) categories: Vec<String>,
    pub(crate) series: Vec<(String, Vec<f64>)>,
}

/// Extract the copy model from chart XML bytes (see `ChartCopyData`).
pub(crate) fn copy_data(chart_xml: &[u8]) -> Option<ChartCopyData> {
    let chart_space = parse_chart_space(chart_xml)?;
    let model = ChartModel::from_chart_space(&chart_space)?;
    Some(ChartCopyData {
        categories: model.categories,
        series: model
            .series
            .into_iter()
            .map(|s| (s.name, s.values.into_iter().map(|v| v.unwrap_or(f64::NAN)).collect()))
            .collect(),
    })
}

// ─── Value-reading rules (§21.2.2) ──────────────────────────────────────────

/// A chart `CT_Boolean`: present with no `val` means true (§21.2.2.1, the
/// schema default), so `<c:delete/>` deletes and `<c:smooth/>` smooths.
fn chart_bool(b: Option<&ooxml_dml::types::CTBoolean>) -> Option<bool> {
    b.map(|b| b.value.as_deref().is_none_or(|v| matches!(v, "1" | "true" | "on")))
}

/// A chart percentage (`ST_GapAmount`, `ST_Overlap`, `ST_HoleSize`): a bare
/// integer or the transitional `NN%` form, as a fraction (`150` → 1.5).
fn chart_pct(raw: Option<&str>) -> Option<f64> {
    let s = raw?.trim();
    s.strip_suffix('%').unwrap_or(s).trim().parse::<f64>().ok().filter(|v| v.is_finite()).map(|v| v / 100.0)
}

// ─── Model ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
enum ChartType {
    Bar,
    Line,
    Pie,
    Scatter,
    Area,
    Doughnut,
    Radar,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Grouping {
    /// Side by side (bar `clustered`, line and area `standard`).
    Clustered,
    Stacked,
    PercentStacked,
}

/// How a blank point is drawn (§21.2.2.42 `dispBlanksAs`).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Blanks {
    Gap,
    Zero,
    Span,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum LegendPos {
    Right,
    Bottom,
    Top,
    Left,
    TopRight,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Marker {
    None,
    Circle,
    Square,
    Diamond,
    Triangle,
    X,
    Plus,
    Star,
    Dash,
    Dot,
}

struct Series {
    name: String,
    /// One value per category position; `None` is a blank cell.
    values: Vec<Option<f64>>,
    /// Fill colour (bars, areas, markers, legend key).
    color: String,
    /// Line colour (line, scatter and radar series).
    line_color: String,
    line_width: f64,
    marker: Marker,
    marker_px: f64,
    smooth: bool,
}

#[derive(Default)]
struct Axis {
    deleted: bool,
    min: Option<f64>,
    max: Option<f64>,
    major: Option<f64>,
    log_base: Option<f64>,
    /// `orientation maxMin` (§21.2.2.130): the axis runs from its maximum.
    reversed: bool,
    /// `tickLblPos none`: no labels.
    no_labels: bool,
    gridlines: bool,
}

struct Legend {
    pos: LegendPos,
    overlay: bool,
    /// Entries removed with `legendEntry/delete` (by entry index).
    hidden: Vec<usize>,
}

struct ChartModel {
    kind: ChartType,
    horizontal: bool,
    grouping: Grouping,
    /// Gap between category groups, as a fraction of a bar's width.
    gap_width: f64,
    /// Overlap between bars of one category, -1..1.
    overlap: f64,
    categories: Vec<String>,
    series: Vec<Series>,
    legend: Option<Legend>,
    val_axis: Axis,
    cat_axis: Axis,
    blanks: Blanks,
    /// Pie, doughnut (and a single-series chart with varyColors): each point
    /// is its own colour and the legend lists the categories.
    vary_colors: bool,
    marker_default: bool,
}

const AXIS_LABEL_W: f64 = 40.0;
const AXIS_LABEL_H: f64 = 20.0;
const MARGIN: f64 = 8.0;
const LEGEND_ROW: f64 = 18.0;
const CHART_FONT: &str = "11px -apple-system, BlinkMacSystemFont, sans-serif";
const AXIS_COLOR: &str = "#b3b3b3";
const GRID_COLOR: &str = "#e0e0e0";
const LABEL_COLOR: &str = "#595959";

/// Office's default series colours: accents 1-6, then the same accents darker.
const DEFAULT_COLORS: &[&str] = &[
    "#4472c4", "#ed7d31", "#a5a5a5", "#ffc000", "#5b9bd5",
    "#70ad47", "#264478", "#9e480e", "#636363", "#997300",
];

fn palette(idx: usize) -> String {
    DEFAULT_COLORS[idx % DEFAULT_COLORS.len()].to_string()
}

fn parse_chart_space(xml: &[u8]) -> Option<ooxml_dml::types::ChartSpace> {
    ooxml_dml::parse_chart_space(xml).ok()
}

/// The parts every series type shares, read once.
struct RawSeries<'a> {
    idx: u32,
    order: u32,
    tx: Option<&'a ooxml_dml::types::SeriesText>,
    sp_pr: Option<&'a ooxml_dml::types::CTShapeProperties>,
    cat: Option<&'a ooxml_dml::types::AxisDataSource>,
    val: Option<&'a ooxml_dml::types::NumericDataSource>,
    marker: Option<&'a ooxml_dml::types::ChartMarker>,
    smooth: Option<bool>,
}

/// The shared parts of each series of a chart group. `$cat` and `$val` name
/// the category and value fields (scatter series use `x_val`/`y_val`);
/// `marker` and `smooth` are read where the series type has them.
macro_rules! raw_series {
    (@one $s:ident, $cat:ident, $val:ident, $marker:expr, $smooth:expr) => {
        RawSeries {
            idx: $s.idx.value,
            order: $s.order.value,
            tx: $s.tx.as_deref(),
            sp_pr: $s.sp_pr.as_deref(),
            cat: $s.$cat.as_deref(),
            val: $s.$val.as_deref(),
            marker: $marker,
            smooth: $smooth,
        }
    };
    ($ser:expr, $cat:ident, $val:ident) => {
        $ser.iter().map(|s| raw_series!(@one s, $cat, $val, None, None)).collect::<Vec<_>>()
    };
    ($ser:expr, $cat:ident, $val:ident, marker) => {
        $ser.iter().map(|s| raw_series!(@one s, $cat, $val, s.marker.as_deref(), None)).collect::<Vec<_>>()
    };
    ($ser:expr, $cat:ident, $val:ident, marker, smooth) => {
        $ser.iter()
            .map(|s| raw_series!(@one s, $cat, $val, s.marker.as_deref(), chart_bool(s.smooth.as_deref())))
            .collect::<Vec<_>>()
    };
}

impl ChartModel {
    fn from_chart_space(cs: &ooxml_dml::types::ChartSpace) -> Option<ChartModel> {
        use ooxml_dml::types::{BarDirectionType, BarGroupingType, GroupingType};
        let chart = &cs.chart;
        let plot = &chart.plot_area;
        let bar_grouping = |g: Option<&ooxml_dml::types::BarGrouping>| match g.and_then(|g| g.value.as_ref()) {
            Some(BarGroupingType::Stacked) => Grouping::Stacked,
            Some(BarGroupingType::PercentStacked) => Grouping::PercentStacked,
            _ => Grouping::Clustered,
        };
        let grouping = |g: Option<&ooxml_dml::types::ChartGrouping>| match g.and_then(|g| g.value.as_ref()) {
            Some(GroupingType::Stacked) => Grouping::Stacked,
            Some(GroupingType::PercentStacked) => Grouping::PercentStacked,
            _ => Grouping::Clustered,
        };

        // One chart group per chart: the first one present, in this order.
        let (kind, raw, horizontal, group, gap, overlap, vary, marker_default) =
            if let Some(b) = plot.bar_chart.first() {
                (ChartType::Bar, raw_series!(b.ser, cat, value),
                 b.bar_dir.value == Some(BarDirectionType::Bar), bar_grouping(b.grouping.as_deref()),
                 chart_pct(b.gap_width.as_deref().and_then(|g| g.value.as_deref())),
                 chart_pct(b.overlap.as_deref().and_then(|o| o.value.as_deref())),
                 chart_bool(b.vary_colors.as_deref()), true)
            } else if let Some(b) = plot.bar3_d_chart.first() {
                (ChartType::Bar, raw_series!(b.ser, cat, value),
                 b.bar_dir.value == Some(BarDirectionType::Bar), bar_grouping(b.grouping.as_deref()),
                 chart_pct(b.gap_width.as_deref().and_then(|g| g.value.as_deref())), None,
                 chart_bool(b.vary_colors.as_deref()), true)
            } else if let Some(l) = plot.line_chart.first() {
                (ChartType::Line,
                 raw_series!(l.ser, cat, value, marker, smooth),
                 false, grouping(Some(&l.grouping)), None, None, chart_bool(l.vary_colors.as_deref()),
                 chart_bool(l.marker.as_deref()).unwrap_or(true))
            } else if let Some(l) = plot.line3_d_chart.first() {
                (ChartType::Line,
                 raw_series!(l.ser, cat, value, marker, smooth),
                 false, grouping(Some(&l.grouping)), None, None, chart_bool(l.vary_colors.as_deref()), false)
            } else if let Some(pie) = plot.pie_chart.first() {
                (ChartType::Pie, raw_series!(pie.ser, cat, value),
                 false, Grouping::Clustered, None, None, chart_bool(pie.vary_colors.as_deref()).or(Some(true)), false)
            } else if let Some(pie) = plot.pie3_d_chart.first() {
                (ChartType::Pie, raw_series!(pie.ser, cat, value),
                 false, Grouping::Clustered, None, None, chart_bool(pie.vary_colors.as_deref()).or(Some(true)), false)
            } else if let Some(d) = plot.doughnut_chart.first() {
                (ChartType::Doughnut, raw_series!(d.ser, cat, value),
                 false, Grouping::Clustered, None, None, chart_bool(d.vary_colors.as_deref()).or(Some(true)), false)
            } else if let Some(s) = plot.scatter_chart.first() {
                (ChartType::Scatter, raw_series!(s.ser, x_val, y_val, marker, smooth),
                 false, Grouping::Clustered, None, None, chart_bool(s.vary_colors.as_deref()), true)
            } else if let Some(a) = plot.area_chart.first() {
                (ChartType::Area, raw_series!(a.ser, cat, value),
                 false, grouping(a.grouping.as_deref()), None, None, chart_bool(a.vary_colors.as_deref()), false)
            } else if let Some(a) = plot.area3_d_chart.first() {
                (ChartType::Area, raw_series!(a.ser, cat, value),
                 false, grouping(a.grouping.as_deref()), None, None, chart_bool(a.vary_colors.as_deref()), false)
            } else if let Some(r) = plot.radar_chart.first() {
                (ChartType::Radar, raw_series!(r.ser, cat, value, marker),
                 false, Grouping::Clustered, None, None, chart_bool(r.vary_colors.as_deref()), true)
            } else {
                return None;
            };

        let mut raw = raw;
        if raw.is_empty() {
            return None;
        }
        // §21.2.2.128 `order`: series plot and list in this order.
        raw.sort_by_key(|s| s.order);

        // Categories: the first series that names any; else 1..N.
        let n_points = raw.iter().map(|s| point_count(s.val)).max().unwrap_or(0);
        let categories = raw
            .iter()
            .find_map(|s| category_labels(s.cat))
            .unwrap_or_else(|| (1..=n_points.max(1)).map(|i| i.to_string()).collect());
        let n_cats = categories.len().max(n_points);

        let series = raw
            .iter()
            .map(|s| {
                let mut values = numeric_values(s.val);
                values.resize(n_cats, None);
                let color = series_fill(s.sp_pr).unwrap_or_else(|| palette(s.idx as usize));
                let line_color = series_line(s.sp_pr).unwrap_or_else(|| color.clone());
                let line_width = series_line_width(s.sp_pr).unwrap_or(match kind {
                    ChartType::Line | ChartType::Scatter | ChartType::Radar => 2.25 * 96.0 / 72.0,
                    _ => 1.0,
                });
                let (marker, marker_px) = series_marker(s.marker, marker_default);
                SeriesBuilder { name: series_name(s.tx, s.idx), values, color, line_color, line_width, marker, marker_px, smooth: s.smooth.unwrap_or(false) }
                    .build()
            })
            .collect::<Vec<_>>();

        let (val_axis, cat_axis) = axes(plot);
        let stacked = matches!(group, Grouping::Stacked | Grouping::PercentStacked);
        Some(ChartModel {
            kind,
            horizontal,
            grouping: group,
            gap_width: gap.unwrap_or(1.5),
            // Stacked bars overlap fully whatever the file says.
            overlap: if stacked && kind == ChartType::Bar { 1.0 } else { overlap.unwrap_or(0.0).clamp(-1.0, 1.0) },
            categories,
            series,
            legend: legend(chart.legend.as_deref()),
            val_axis,
            cat_axis,
            blanks: match chart.disp_blanks_as.as_deref().and_then(|d| d.value.as_ref()) {
                Some(ooxml_dml::types::DisplayBlanksAsType::Span) => Blanks::Span,
                Some(ooxml_dml::types::DisplayBlanksAsType::Zero) => Blanks::Zero,
                _ => Blanks::Gap,
            },
            vary_colors: matches!(kind, ChartType::Pie | ChartType::Doughnut)
                || (vary.unwrap_or(false) && raw.len() == 1 && matches!(kind, ChartType::Bar)),
            marker_default,
        })
    }
}

struct SeriesBuilder {
    name: String,
    values: Vec<Option<f64>>,
    color: String,
    line_color: String,
    line_width: f64,
    marker: Marker,
    marker_px: f64,
    smooth: bool,
}

impl SeriesBuilder {
    fn build(self) -> Series {
        Series {
            name: self.name,
            values: self.values,
            color: self.color,
            line_color: self.line_color,
            line_width: self.line_width,
            marker: self.marker,
            marker_px: self.marker_px,
            smooth: self.smooth,
        }
    }
}

// ─── Data extraction helpers ────────────────────────────────────────────────

/// A series' name (§21.2.2.215 `tx`): its cached string, its literal `v`,
/// else Office's "Series N" from its index.
fn series_name(tx: Option<&ooxml_dml::types::SeriesText>, idx: u32) -> String {
    tx.and_then(|t| {
        t.str_ref
            .as_ref()
            .and_then(|sr| sr.str_cache.as_ref())
            .and_then(|sc| sc.pt.first())
            .map(|pt| pt.v.to_string())
            .or_else(|| t.v.as_ref().map(|v| v.to_string()))
    })
    .unwrap_or_else(|| format!("Series{}", idx + 1))
}

/// A series' solid fill colour (§20.1.2.3).
/// A `schemeClr` falls back to the palette: charts have no theme here yet.
fn series_fill(sp_pr: Option<&ooxml_dml::types::CTShapeProperties>) -> Option<String> {
    let fill = sp_pr?.fill_properties.as_deref()?;
    match fill {
        ooxml_dml::types::EGFillProperties::SolidFill(sf) => resolve(sf.color_choice.as_deref()),
        _ => None,
    }
}

/// A series' line colour (`spPr/a:ln/a:solidFill`): where line, scatter and
/// radar series keep their colour.
fn series_line(sp_pr: Option<&ooxml_dml::types::CTShapeProperties>) -> Option<String> {
    let ln = sp_pr?.line.as_deref()?;
    match ln.line_fill_properties.as_deref()? {
        ooxml_dml::types::EGLineFillProperties::SolidFill(sf) => resolve(sf.color_choice.as_deref()),
        _ => None,
    }
}

/// A series' line width (`a:ln@w`, EMU) in px.
fn series_line_width(sp_pr: Option<&ooxml_dml::types::CTShapeProperties>) -> Option<f64> {
    let w = sp_pr?.line.as_deref()?.width?;
    Some(crate::engine::units::emu_to_px(i64::from(w)))
}

fn resolve(choice: Option<&ooxml_dml::types::EGColorChoice>) -> Option<String> {
    crate::engine::color::resolve_color_choice(choice?, |_| None)
        .map(|(hex, alpha)| crate::engine::color::css_color(&hex, alpha))
}

/// A line series' marker (§21.2.2.106): its symbol and size (points), or the
/// chart's default (a circle) when the series says nothing and the chart
/// shows markers.
fn series_marker(m: Option<&ooxml_dml::types::ChartMarker>, chart_default: bool) -> (Marker, f64) {
    use ooxml_dml::types::MarkerStyleType as M;
    let size = m.and_then(|m| m.size.as_deref()).and_then(|s| s.value).map_or(5.0, f64::from);
    let symbol = match m.and_then(|m| m.symbol.as_deref()).map(|s| &s.value) {
        Some(M::None) => Marker::None,
        Some(M::Square) => Marker::Square,
        Some(M::Diamond) => Marker::Diamond,
        Some(M::Triangle) => Marker::Triangle,
        Some(M::X) => Marker::X,
        Some(M::Plus) => Marker::Plus,
        Some(M::Star) => Marker::Star,
        Some(M::Dash) => Marker::Dash,
        Some(M::Dot) => Marker::Dot,
        Some(M::Circle | M::Picture | M::Auto) => Marker::Circle,
        None if chart_default => Marker::Circle,
        None => Marker::None,
    };
    (symbol, size * 96.0 / 72.0)
}

/// How many points a value source holds: its `ptCount`, else one past the
/// highest point index.
fn point_count(val: Option<&ooxml_dml::types::NumericDataSource>) -> usize {
    let Some(data) = val.and_then(|v| v.num_ref.as_ref().and_then(|r| r.num_cache.as_deref()).or(v.num_lit.as_deref())) else {
        return 0;
    };
    data.pt_count.as_ref().map(|c| c.value as usize)
        .unwrap_or_else(|| data.pt.iter().map(|p| p.idx as usize + 1).max().unwrap_or(0))
}

/// A series' values by point index (§21.2.2.150 `pt@idx`): a cache omits
/// blank cells, so each point goes to its own index and gaps stay `None`.
fn numeric_values(val: Option<&ooxml_dml::types::NumericDataSource>) -> Vec<Option<f64>> {
    let Some(data) = val.and_then(|v| v.num_ref.as_ref().and_then(|r| r.num_cache.as_deref()).or(v.num_lit.as_deref())) else {
        return Vec::new();
    };
    let mut out = vec![None; point_count(val)];
    for pt in &data.pt {
        let i = pt.idx as usize;
        if i >= out.len() {
            out.resize(i + 1, None);
        }
        out[i] = pt.v.trim().parse::<f64>().ok().filter(|v| v.is_finite());
    }
    out
}

/// Category labels by point index: strings, numbers (as written), string or
/// number literals, or the first level of a multi-level reference. `None`
/// when the series names no categories.
fn category_labels(cat: Option<&ooxml_dml::types::AxisDataSource>) -> Option<Vec<String>> {
    let cat = cat?;
    let place = |pts: Vec<(u32, String)>, count: Option<u32>| {
        let n = count.map(|c| c as usize).unwrap_or_else(|| pts.iter().map(|p| p.0 as usize + 1).max().unwrap_or(0));
        let mut out = vec![String::new(); n];
        for (i, v) in pts {
            let i = i as usize;
            if i >= out.len() {
                out.resize(i + 1, String::new());
            }
            out[i] = v;
        }
        out
    };
    if let Some(sd) = cat.str_ref.as_ref().and_then(|r| r.str_cache.as_deref()).or(cat.str_lit.as_deref()) {
        return Some(place(sd.pt.iter().map(|p| (p.idx, p.v.to_string())).collect(), sd.pt_count.as_ref().map(|c| c.value)));
    }
    if let Some(nd) = cat.num_ref.as_ref().and_then(|r| r.num_cache.as_deref()).or(cat.num_lit.as_deref()) {
        return Some(place(nd.pt.iter().map(|p| (p.idx, p.v.to_string())).collect(), nd.pt_count.as_ref().map(|c| c.value)));
    }
    let ml = cat.multi_lvl_str_ref.as_ref()?.multi_lvl_str_cache.as_deref()?;
    let level = ml.lvl.first()?;
    Some(place(level.pt.iter().map(|p| (p.idx, p.v.to_string())).collect(), ml.pt_count.as_ref().map(|c| c.value)))
}

fn axes(plot: &ooxml_dml::types::PlotArea) -> (Axis, Axis) {
    use ooxml_dml::types::{AxisOrientationType, TickLabelPositionType};
    let scaling = |s: &ooxml_dml::types::AxisScaling| (
        s.min.as_ref().map(|m| m.value),
        s.max.as_ref().map(|m| m.value),
        s.log_base.as_ref().map(|l| l.value).filter(|b| *b > 1.0),
        s.orientation.as_ref().and_then(|o| o.value.as_ref()) == Some(&AxisOrientationType::MaxMin),
    );
    let no_labels = |t: Option<&ooxml_dml::types::TickLabelPosition>| {
        t.and_then(|t| t.value.as_ref()) == Some(&TickLabelPositionType::None)
    };
    let val = plot.val_ax.first().map(|v| {
        let (min, max, log_base, reversed) = scaling(&v.scaling);
        Axis {
            deleted: chart_bool(v.delete.as_deref()).unwrap_or(false),
            min, max,
            major: v.major_unit.as_ref().map(|u| u.value).filter(|u| *u > 0.0),
            log_base, reversed,
            no_labels: no_labels(v.tick_lbl_pos.as_deref()),
            gridlines: v.major_gridlines.is_some(),
        }
    }).unwrap_or(Axis { gridlines: true, ..Axis::default() });
    let cat = plot.cat_ax.first().map(|c| {
        let (_, _, _, reversed) = scaling(&c.scaling);
        Axis {
            deleted: chart_bool(c.delete.as_deref()).unwrap_or(false),
            reversed,
            no_labels: no_labels(c.tick_lbl_pos.as_deref()),
            gridlines: c.major_gridlines.is_some(),
            ..Axis::default()
        }
    }).unwrap_or_default();
    (val, cat)
}

/// The legend (§21.2.2.93), or `None` when the chart has none.
fn legend(l: Option<&ooxml_dml::types::Legend>) -> Option<Legend> {
    use ooxml_dml::types::LegendPositionType as P;
    let l = l?;
    Some(Legend {
        // ST_LegendPos defaults to "r".
        pos: match l.legend_pos.as_deref().and_then(|p| p.value.as_ref()) {
            Some(P::B) => LegendPos::Bottom,
            Some(P::T) => LegendPos::Top,
            Some(P::L) => LegendPos::Left,
            Some(P::Tr) => LegendPos::TopRight,
            _ => LegendPos::Right,
        },
        overlay: chart_bool(l.overlay.as_deref()).unwrap_or(false),
        hidden: l.legend_entry.iter()
            .filter(|e| chart_bool(e.delete.as_deref()).unwrap_or(false))
            .map(|e| e.idx.value as usize)
            .collect(),
    })
}

// ─── Value scale ────────────────────────────────────────────────────────────

/// The value axis's range and step.
struct Scale {
    min: f64,
    max: f64,
    step: f64,
    log_base: Option<f64>,
}

impl Scale {
    /// Position of `v` along the axis as a fraction 0 (min) to 1 (max).
    fn frac(&self, v: f64) -> f64 {
        match self.log_base {
            Some(b) => {
                let (lo, hi) = (self.min.max(f64::MIN_POSITIVE).log(b), self.max.log(b));
                if hi <= lo { 0.5 } else { (v.max(f64::MIN_POSITIVE).log(b) - lo) / (hi - lo) }
            }
            None => {
                let range = self.max - self.min;
                if range == 0.0 { 0.5 } else { (v - self.min) / range }
            }
        }
    }

    fn ticks(&self) -> Vec<f64> {
        let mut out = Vec::new();
        match self.log_base {
            Some(b) => {
                let first = self.min.log(b).round() as i32;
                let last = self.max.log(b).round() as i32;
                for e in first..=last.min(first + 64) {
                    out.push(b.powi(e));
                }
            }
            None => {
                let mut v = self.min;
                while v <= self.max + self.step * 0.001 && out.len() < 200 {
                    out.push(v);
                    v += self.step;
                }
            }
        }
        out
    }
}

impl ChartModel {
    fn stacked(&self) -> bool {
        matches!(self.grouping, Grouping::Stacked | Grouping::PercentStacked)
            && matches!(self.kind, ChartType::Bar | ChartType::Line | ChartType::Area)
    }

    /// Each series' value as plotted at category `ci`: blank as the chart
    /// says, stacked on the series before it, and as a share of the
    /// category total in a 100 % stack. Returns (base, top) per series.
    fn plotted(&self, ci: usize) -> Vec<Option<(f64, f64)>> {
        let raw: Vec<Option<f64>> = self.series.iter()
            .map(|s| match s.values.get(ci).copied().flatten() {
                Some(v) => Some(v),
                None if self.blanks == Blanks::Zero => Some(0.0),
                None => None,
            })
            .collect();
        if !self.stacked() {
            return raw.into_iter().map(|v| v.map(|v| (0.0, v))).collect();
        }
        let total: f64 = raw.iter().flatten().map(|v| v.abs()).sum();
        let mut pos = 0.0;
        let mut neg = 0.0;
        raw.into_iter()
            .map(|v| {
                let v = v?;
                let v = if self.grouping == Grouping::PercentStacked && total > 0.0 { v / total } else { v };
                let base = if v >= 0.0 { pos } else { neg };
                if v >= 0.0 { pos += v } else { neg += v }
                Some((base, base + v))
            })
            .collect()
    }

    fn scale(&self) -> Scale {
        let mut lo = f64::MAX;
        let mut hi = f64::MIN;
        for ci in 0..self.categories.len() {
            for (base, top) in self.plotted(ci).into_iter().flatten() {
                lo = lo.min(base.min(top));
                hi = hi.max(base.max(top));
            }
        }
        if lo == f64::MAX {
            lo = 0.0;
            hi = 1.0;
        }
        let ax = &self.val_axis;
        if let Some(b) = ax.log_base {
            // A log axis spans the positive values only, from the power of
            // the base at or below the smallest to the one at or above the
            // largest.
            let positive: Vec<f64> = (0..self.categories.len())
                .flat_map(|ci| self.plotted(ci))
                .flatten()
                .map(|(_, top)| top)
                .filter(|v| *v > 0.0)
                .collect();
            let lo = positive.iter().copied().fold(f64::MAX, f64::min);
            let hi = positive.iter().copied().fold(f64::MIN, f64::max);
            let (lo, hi) = if positive.is_empty() { (1.0, b) } else { (lo, hi) };
            let power = |v: f64, up: bool| {
                let e = v.log(b);
                let e = if up { (e - 1e-9).ceil() } else { (e + 1e-9).floor() };
                b.powf(e)
            };
            let min = ax.min.filter(|m| *m > 0.0).unwrap_or_else(|| power(lo, false));
            let max = ax.max.unwrap_or_else(|| power(hi, true)).max(min * b);
            return Scale { min, max, step: b, log_base: Some(b) };
        }
        if self.grouping == Grouping::PercentStacked && self.stacked() {
            let step = ax.major.unwrap_or(0.1);
            return Scale { min: ax.min.unwrap_or(if lo < 0.0 { -1.0 } else { 0.0 }), max: ax.max.unwrap_or(1.0), step, log_base: None };
        }
        // Office starts a value axis at zero when the data is all positive
        // (all negative: ends at zero).
        if lo > 0.0 && self.kind != ChartType::Scatter {
            lo = 0.0;
        }
        if hi < 0.0 {
            hi = 0.0;
        }
        let lo = ax.min.unwrap_or(lo);
        let hi = ax.max.unwrap_or(hi);
        let step = ax.major.unwrap_or_else(|| nice_step(hi - lo));
        // Excel/Word leave headroom so the tallest bar never touches the axis
        // top: a data max of 9.6 yields an axis max of 12, not 10. A small
        // (~5%) headroom factor before the ceil reproduces that and only bumps
        // a chart whose max sits within ~5% of its nice ceiling.
        let max = ax.max.unwrap_or(if hi > 0.0 { ((hi * 1.05) / step).ceil() * step } else { (hi / step).ceil() * step });
        let min = ax.min.unwrap_or((lo / step).floor() * step);
        Scale { min, max: max.max(min + step), step, log_base: None }
    }
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

fn label(v: f64, percent: bool) -> String {
    if percent {
        return format!("{}%", (v * 100.0).round() as i64);
    }
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round() as i64)
    } else {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

// ─── Painting ───────────────────────────────────────────────────────────────

/// A rectangle in page px.
#[derive(Clone, Copy, Debug)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl ChartModel {
    /// The legend's entries: (label, colour), without deleted ones.
    fn legend_entries(&self) -> Vec<(String, String)> {
        let all: Vec<(String, String)> = if self.vary_colors {
            self.categories.iter().enumerate().map(|(i, c)| (c.clone(), palette(i))).collect()
        } else {
            self.series.iter().map(|s| (s.name.clone(), s.color.clone())).collect()
        };
        let hidden = self.legend.as_ref().map(|l| l.hidden.as_slice()).unwrap_or(&[]);
        all.into_iter().enumerate().filter(|(i, _)| !hidden.contains(i)).map(|(_, e)| e).collect()
    }

    fn paint(&self, p: &dyn Painter, x: f64, y: f64, w: f64, h: f64) {
        p.set_font(CHART_FONT);
        let mut area = Rect { x: x + MARGIN, y: y + MARGIN, w: (w - 2.0 * MARGIN).max(1.0), h: (h - 2.0 * MARGIN).max(1.0) };
        let entries = self.legend_entries();
        if let Some(lg) = self.legend.as_ref().filter(|_| !entries.is_empty()) {
            let text_w = |s: &str| {
                let m = p.measure_text_width(s);
                if m > 0.0 { m } else { s.chars().count() as f64 * 6.0 }
            };
            let col_w = entries.iter().map(|(n, _)| text_w(n)).fold(0.0, f64::max) + 22.0;
            let row_w: f64 = entries.iter().map(|(n, _)| text_w(n) + 30.0).sum();
            let lg_rect = match lg.pos {
                LegendPos::Right | LegendPos::TopRight => {
                    let lw = col_w.min(area.w * 0.4);
                    let lh = LEGEND_ROW * entries.len() as f64;
                    let ly = if lg.pos == LegendPos::TopRight { area.y } else { area.y + (area.h - lh).max(0.0) / 2.0 };
                    let r = Rect { x: area.x + area.w - lw, y: ly, w: lw, h: lh };
                    if !lg.overlay { area.w -= lw + MARGIN; }
                    r
                }
                LegendPos::Left => {
                    let lw = col_w.min(area.w * 0.4);
                    let lh = LEGEND_ROW * entries.len() as f64;
                    let r = Rect { x: area.x, y: area.y + (area.h - lh).max(0.0) / 2.0, w: lw, h: lh };
                    if !lg.overlay { area.x += lw + MARGIN; area.w -= lw + MARGIN; }
                    r
                }
                LegendPos::Bottom | LegendPos::Top => {
                    let lw = row_w.min(area.w);
                    let lx = area.x + (area.w - lw) / 2.0;
                    let r = if lg.pos == LegendPos::Bottom {
                        Rect { x: lx, y: area.y + area.h - LEGEND_ROW, w: lw, h: LEGEND_ROW }
                    } else {
                        Rect { x: lx, y: area.y, w: lw, h: LEGEND_ROW }
                    };
                    if !lg.overlay {
                        area.h -= LEGEND_ROW + MARGIN;
                        if lg.pos == LegendPos::Top { area.y += LEGEND_ROW + MARGIN; }
                    }
                    r
                }
            };
            self.paint_plot(p, area);
            paint_legend(p, lg_rect, lg.pos, &entries, &text_w);
            return;
        }
        self.paint_plot(p, area);
    }

    fn paint_plot(&self, p: &dyn Painter, area: Rect) {
        match self.kind {
            ChartType::Pie | ChartType::Doughnut => self.paint_arcs(p, area),
            ChartType::Radar => self.paint_radar(p, area),
            _ => self.paint_cartesian(p, area),
        }
    }

    fn paint_cartesian(&self, p: &dyn Painter, area: Rect) {
        let scale = self.scale();
        let percent = self.grouping == Grouping::PercentStacked && self.stacked();
        // Room for the labels of each axis that shows them.
        let val_labels = !self.val_axis.deleted && !self.val_axis.no_labels;
        let cat_labels = !self.cat_axis.deleted && !self.cat_axis.no_labels && self.kind != ChartType::Scatter;
        let (left, bottom) = if self.horizontal { (cat_labels, val_labels) } else { (val_labels, cat_labels) };
        let plot = Rect {
            x: area.x + if left { AXIS_LABEL_W } else { 0.0 },
            y: area.y,
            w: (area.w - if left { AXIS_LABEL_W } else { 0.0 }).max(1.0),
            h: (area.h - if bottom { AXIS_LABEL_H } else { 0.0 }).max(1.0),
        };
        let n_cats = self.categories.len().max(1);
        // Value → page coordinate along the value direction.
        let val_pos = |v: f64| {
            let f = scale.frac(v);
            let f = if self.val_axis.reversed { 1.0 - f } else { f };
            if self.horizontal { plot.x + f * plot.w } else { plot.y + plot.h - f * plot.h }
        };
        // Category slot `ci` → its start and width along the category direction.
        let band = if self.horizontal { plot.h } else { plot.w } / n_cats as f64;
        let cat_index = |ci: usize| {
            // Horizontal bars list categories bottom-up, so the first is at the
            // bottom; `maxMin` reverses either direction.
            let flip = self.horizontal != self.cat_axis.reversed;
            if flip { n_cats - 1 - ci } else { ci }
        };
        let cat_start = |ci: usize| (if self.horizontal { plot.y } else { plot.x }) + cat_index(ci) as f64 * band;

        // Gridlines and value labels.
        p.set_line_width(0.75);
        for t in scale.ticks() {
            let at = val_pos(t);
            if self.val_axis.gridlines {
                p.set_stroke_style_str(GRID_COLOR);
                p.begin_path();
                if self.horizontal { p.move_to(at, plot.y); p.line_to(at, plot.y + plot.h); }
                else { p.move_to(plot.x, at); p.line_to(plot.x + plot.w, at); }
                p.stroke();
            }
            if val_labels {
                p.set_fill_style_str(LABEL_COLOR);
                if self.horizontal {
                    p.set_text_align("center");
                    p.set_text_baseline("top");
                    let _ = p.fill_text(&label(t, percent), at, plot.y + plot.h + 4.0);
                } else {
                    p.set_text_align("right");
                    p.set_text_baseline("middle");
                    let _ = p.fill_text(&label(t, percent), plot.x - 5.0, at);
                }
            }
        }

        // Series.
        match self.kind {
            ChartType::Bar => self.paint_bars(p, &val_pos, &cat_start, band),
            ChartType::Area => self.paint_areas(p, &val_pos, &cat_start, band),
            _ => self.paint_lines(p, &val_pos, &cat_start, band),
        }

        // Category axis line (at value 0) and labels.
        if !self.cat_axis.deleted {
            p.set_stroke_style_str(AXIS_COLOR);
            p.set_line_width(1.0);
            let base = val_pos(scale.min.max(0.0_f64.min(scale.max)).max(scale.min));
            p.begin_path();
            if self.horizontal { p.move_to(base, plot.y); p.line_to(base, plot.y + plot.h); }
            else { p.move_to(plot.x, base); p.line_to(plot.x + plot.w, base); }
            p.stroke();
        }
        if cat_labels {
            p.set_fill_style_str(LABEL_COLOR);
            for (ci, cat) in self.categories.iter().enumerate() {
                let mid = cat_start(ci) + band / 2.0;
                if self.horizontal {
                    p.set_text_align("right");
                    p.set_text_baseline("middle");
                    let _ = p.fill_text(cat, plot.x - 5.0, mid);
                } else {
                    p.set_text_align("center");
                    p.set_text_baseline("top");
                    let _ = p.fill_text(cat, mid, plot.y + plot.h + 4.0);
                }
            }
        }
    }

    /// Bars (§21.2.2.13 gapWidth, §21.2.2.131 overlap): each category band
    /// holds its bars and one gap; series sit side by side, overlapping by
    /// `overlap` of a bar width (fully when stacked).
    fn paint_bars(&self, p: &dyn Painter, val_pos: &dyn Fn(f64) -> f64, cat_start: &dyn Fn(usize) -> f64, band: f64) {
        let n = if self.stacked() { 1.0 } else { self.series.len().max(1) as f64 };
        let units = n - (n - 1.0) * self.overlap + self.gap_width;
        let bar = band / units.max(0.01);
        let step = bar * (1.0 - self.overlap);
        for ci in 0..self.categories.len() {
            let start = cat_start(ci) + bar * self.gap_width / 2.0;
            for (si, slot) in self.plotted(ci).into_iter().enumerate() {
                let Some((base, top)) = slot else { continue };
                let offset = if self.stacked() { 0.0 } else { si as f64 * step };
                let colour = if self.vary_colors { palette(ci) } else { self.series[si].color.clone() };
                p.set_fill_style_str(&colour);
                let (a, b) = (val_pos(base), val_pos(top));
                if self.horizontal {
                    p.fill_rect(a.min(b), start + offset, (b - a).abs(), bar);
                } else {
                    p.fill_rect(start + offset, a.min(b), bar, (b - a).abs());
                }
            }
        }
    }

    fn paint_areas(&self, p: &dyn Painter, val_pos: &dyn Fn(f64) -> f64, cat_start: &dyn Fn(usize) -> f64, band: f64) {
        let x_of = |ci: usize| cat_start(ci) + band / 2.0;
        let rows: Vec<Vec<Option<(f64, f64)>>> = (0..self.categories.len()).map(|ci| self.plotted(ci)).collect();
        for (si, ser) in self.series.iter().enumerate() {
            let pts: Vec<(f64, f64, f64)> = rows.iter().enumerate()
                .map(|(ci, r)| {
                    let (base, top) = r.get(si).copied().flatten().unwrap_or((0.0, 0.0));
                    (x_of(ci), val_pos(base), val_pos(top))
                })
                .collect();
            if pts.is_empty() { continue; }
            p.set_fill_style_str(&ser.color);
            p.begin_path();
            p.move_to(pts[0].0, pts[0].2);
            for &(x, _, top) in &pts[1..] { p.line_to(x, top); }
            for &(x, base, _) in pts.iter().rev() { p.line_to(x, base); }
            p.close_path();
            p.fill();
        }
    }

    fn paint_lines(&self, p: &dyn Painter, val_pos: &dyn Fn(f64) -> f64, cat_start: &dyn Fn(usize) -> f64, band: f64) {
        let x_of = |ci: usize| cat_start(ci) + band / 2.0;
        let rows: Vec<Vec<Option<(f64, f64)>>> = (0..self.categories.len()).map(|ci| self.plotted(ci)).collect();
        for (si, ser) in self.series.iter().enumerate() {
            // Runs of consecutive points; a blank breaks the line unless the
            // chart spans blanks.
            let mut runs: Vec<Vec<(f64, f64)>> = vec![Vec::new()];
            for (ci, r) in rows.iter().enumerate() {
                match r.get(si).copied().flatten() {
                    Some((_, top)) => runs.last_mut().unwrap().push((x_of(ci), val_pos(top))),
                    None if self.blanks == Blanks::Span => {}
                    None => runs.push(Vec::new()),
                }
            }
            if self.kind != ChartType::Scatter || ser.line_width > 0.0 {
                p.set_stroke_style_str(&ser.line_color);
                p.set_line_width(ser.line_width);
                for run in runs.iter().filter(|r| r.len() > 1) {
                    p.begin_path();
                    p.move_to(run[0].0, run[0].1);
                    if ser.smooth {
                        smooth_path(p, run);
                    } else {
                        for &(x, y) in &run[1..] { p.line_to(x, y); }
                    }
                    p.stroke();
                }
            }
            if (self.marker_default || self.kind == ChartType::Scatter) && ser.marker != Marker::None {
                for &(x, y) in runs.iter().flatten() {
                    paint_marker(p, ser.marker, x, y, ser.marker_px, &ser.color);
                }
            }
        }
    }

    fn paint_arcs(&self, p: &dyn Painter, area: Rect) {
        let rings = if self.kind == ChartType::Doughnut { self.series.len() } else { 1 };
        let cx = area.x + area.w / 2.0;
        let cy = area.y + area.h / 2.0;
        let outer = (area.w.min(area.h) / 2.0 - 4.0).max(1.0);
        let hole = if self.kind == ChartType::Doughnut { outer * 0.5 } else { 0.0 };
        let width = (outer - hole) / rings as f64;
        for (ri, ser) in self.series.iter().take(rings).enumerate() {
            let values: Vec<f64> = ser.values.iter().map(|v| v.unwrap_or(0.0).max(0.0)).collect();
            let total: f64 = values.iter().sum();
            if total <= 0.0 { continue; }
            let r_in = hole + ri as f64 * width;
            let r_out = r_in + width;
            let mut start = -std::f64::consts::FRAC_PI_2;
            for (i, v) in values.iter().enumerate() {
                let sweep = v / total * std::f64::consts::TAU;
                p.set_fill_style_str(&palette(i));
                p.begin_path();
                if r_in > 0.0 {
                    let _ = p.arc(cx, cy, r_out, start, start + sweep);
                    let _ = p.arc_with_anticlockwise(cx, cy, r_in, start + sweep, start, true);
                } else {
                    p.move_to(cx, cy);
                    let _ = p.arc(cx, cy, r_out, start, start + sweep);
                }
                p.close_path();
                p.fill();
                start += sweep;
            }
        }
    }

    fn paint_radar(&self, p: &dyn Painter, area: Rect) {
        let n = self.categories.len().max(1);
        let cx = area.x + area.w / 2.0;
        let cy = area.y + area.h / 2.0;
        let r = (area.w.min(area.h) / 2.0 - 20.0).max(1.0);
        let step = std::f64::consts::TAU / n as f64;
        let at = |i: usize, rr: f64| {
            let a = -std::f64::consts::FRAC_PI_2 + i as f64 * step;
            (cx + rr * a.cos(), cy + rr * a.sin())
        };
        let scale = self.scale();
        p.set_stroke_style_str(GRID_COLOR);
        p.set_line_width(0.5);
        for ring in 1..=5 {
            p.begin_path();
            for i in 0..n {
                let (x, y) = at(i, r * ring as f64 / 5.0);
                if i == 0 { p.move_to(x, y) } else { p.line_to(x, y) }
            }
            p.close_path();
            p.stroke();
        }
        for i in 0..n {
            let (x, y) = at(i, r);
            p.begin_path();
            p.move_to(cx, cy);
            p.line_to(x, y);
            p.stroke();
        }
        p.set_fill_style_str(LABEL_COLOR);
        p.set_text_align("center");
        p.set_text_baseline("middle");
        for (i, cat) in self.categories.iter().enumerate() {
            let (x, y) = at(i, r + 12.0);
            let _ = p.fill_text(cat, x, y);
        }
        for ser in &self.series {
            p.set_stroke_style_str(&ser.line_color);
            p.set_line_width(ser.line_width);
            p.begin_path();
            for (i, v) in ser.values.iter().enumerate() {
                let (x, y) = at(i, r * scale.frac(v.unwrap_or(0.0)).clamp(0.0, 1.0));
                if i == 0 { p.move_to(x, y) } else { p.line_to(x, y) }
            }
            p.close_path();
            p.stroke();
        }
    }
}

/// A smooth curve through `pts` (from the first point, already moved to):
/// Catmull-Rom segments as cubic Béziers, as Office's smoothed lines are.
fn smooth_path(p: &dyn Painter, pts: &[(f64, f64)]) {
    for i in 0..pts.len() - 1 {
        let p0 = pts[i.saturating_sub(1)];
        let p1 = pts[i];
        let p2 = pts[i + 1];
        let p3 = pts[(i + 2).min(pts.len() - 1)];
        let c1 = (p1.0 + (p2.0 - p0.0) / 6.0, p1.1 + (p2.1 - p0.1) / 6.0);
        let c2 = (p2.0 - (p3.0 - p1.0) / 6.0, p2.1 - (p3.1 - p1.1) / 6.0);
        p.bezier_curve_to(c1.0, c1.1, c2.0, c2.1, p2.0, p2.1);
    }
}

fn paint_marker(p: &dyn Painter, m: Marker, x: f64, y: f64, size: f64, colour: &str) {
    if m == Marker::None {
        return;
    }
    let r = size / 2.0;
    p.set_fill_style_str(colour);
    p.set_stroke_style_str(colour);
    p.set_line_width(1.0);
    p.begin_path();
    match m {
        Marker::None => {}
        Marker::Circle => { let _ = p.arc(x, y, r, 0.0, std::f64::consts::TAU); p.fill(); }
        Marker::Dot => { let _ = p.arc(x, y, r / 2.0, 0.0, std::f64::consts::TAU); p.fill(); }
        Marker::Square => p.fill_rect(x - r, y - r, size, size),
        Marker::Diamond => {
            p.move_to(x, y - r); p.line_to(x + r, y); p.line_to(x, y + r); p.line_to(x - r, y);
            p.close_path(); p.fill();
        }
        Marker::Triangle => {
            p.move_to(x, y - r); p.line_to(x + r, y + r); p.line_to(x - r, y + r);
            p.close_path(); p.fill();
        }
        Marker::Dash => p.fill_rect(x - r, y - 1.0, size, 2.0),
        Marker::X | Marker::Plus | Marker::Star => {
            if matches!(m, Marker::X | Marker::Star) {
                p.move_to(x - r, y - r); p.line_to(x + r, y + r);
                p.move_to(x - r, y + r); p.line_to(x + r, y - r);
            }
            if matches!(m, Marker::Plus | Marker::Star) {
                p.move_to(x - r, y); p.line_to(x + r, y);
                p.move_to(x, y - r); p.line_to(x, y + r);
            }
            p.stroke();
        }
    }
}

fn paint_legend(p: &dyn Painter, r: Rect, pos: LegendPos, entries: &[(String, String)], text_w: &dyn Fn(&str) -> f64) {
    p.set_text_align("left");
    p.set_text_baseline("middle");
    let key = |x: f64, y: f64, colour: &str| {
        p.set_fill_style_str(colour);
        p.fill_rect(x, y - 4.0, 8.0, 8.0);
    };
    match pos {
        LegendPos::Bottom | LegendPos::Top => {
            let mut x = r.x;
            let y = r.y + r.h / 2.0;
            for (name, colour) in entries {
                key(x, y, colour);
                p.set_fill_style_str(LABEL_COLOR);
                let _ = p.fill_text(name, x + 12.0, y);
                x += text_w(name) + 30.0;
            }
        }
        _ => {
            for (i, (name, colour)) in entries.iter().enumerate() {
                let y = r.y + LEGEND_ROW * (i as f64 + 0.5);
                key(r.x, y, colour);
                p.set_fill_style_str(LABEL_COLOR);
                let _ = p.fill_text(name, r.x + 12.0, y);
            }
        }
    }
}

#[cfg(test)]
mod tests;
