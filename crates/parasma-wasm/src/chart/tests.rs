//! Chart model tests: each builds chart XML inline and checks the model or
//! the recorded paint.

use super::*;
use crate::engine::displaylist::{DrawCmd, RecordingPainter};

/// A chart part around `plot` (the chart group(s) and axes) and `chart_tail`
/// (legend, dispBlanksAs) inside c:chart.
fn chart_xml(plot: &str, chart_tail: &str) -> Vec<u8> {
    format!(
        r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><c:chart><c:plotArea>{plot}</c:plotArea>{chart_tail}</c:chart></c:chartSpace>"#
    )
    .into_bytes()
}

/// A series: `idx`/`order`, optional `tx`, categories and values as
/// `(idx, value)` points with `count` points in all.
fn ser(idx: u32, order: u32, tx: &str, cats: &str, pts: &[(u32, &str)], count: u32) -> String {
    let pts: String = pts.iter().map(|(i, v)| format!(r#"<c:pt idx="{i}"><c:v>{v}</c:v></c:pt>"#)).collect();
    format!(r#"<c:ser><c:idx val="{idx}"/><c:order val="{order}"/>{tx}{cats}<c:val><c:numRef><c:f>S!B1</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val="{count}"/>{pts}</c:numCache></c:numRef></c:val></c:ser>"#)
}

fn str_cats(names: &[&str]) -> String {
    let pts: String = names.iter().enumerate().map(|(i, n)| format!(r#"<c:pt idx="{i}"><c:v>{n}</c:v></c:pt>"#)).collect();
    format!(r#"<c:cat><c:strRef><c:f>S!A1</c:f><c:strCache><c:ptCount val="{}"/>{pts}</c:strCache></c:strRef></c:cat>"#, names.len())
}

fn model(xml: &[u8]) -> ChartModel {
    ChartModel::from_chart_space(&parse_chart_space(xml).expect("parses")).expect("a chart")
}

const AXES: &str = r#"<c:catAx><c:axId val="1"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="b"/><c:crossAx val="2"/></c:catAx><c:valAx><c:axId val="2"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="l"/><c:majorGridlines/><c:crossAx val="1"/></c:valAx>"#;

fn bar(dir: &str, grouping: &str, series: &str, extra: &str) -> String {
    format!(r#"<c:barChart><c:barDir val="{dir}"/><c:grouping val="{grouping}"/>{series}{extra}<c:axId val="1"/><c:axId val="2"/></c:barChart>{AXES}"#)
}

#[test]
fn value_reading_rules() {
    let present = ooxml_dml::types::CTBoolean { value: None, ..Default::default() };
    assert_eq!(chart_bool(Some(&present)), Some(true), "<c:delete/> deletes");
    assert_eq!(chart_bool(None), None);
    assert_eq!(chart_pct(Some("150")), Some(1.5));
    assert_eq!(chart_pct(Some("-27%")), Some(-0.27));
}

/// §21.2.2.150: a cache omits blank cells; each point keeps its index.
#[test]
fn sparse_caches_keep_each_point_at_its_index() {
    let s = ser(0, 0, "", &str_cats(&["Q1", "Q2", "Q3"]), &[(0, "1"), (2, "3")], 3);
    let m = model(&chart_xml(&bar("col", "clustered", &s, ""), ""));
    assert_eq!(m.series[0].values, vec![Some(1.0), None, Some(3.0)]);
    assert_eq!(m.categories, vec!["Q1", "Q2", "Q3"]);
}

/// Names from a literal or "SeriesN", colours by c:idx, plotted by c:order.
#[test]
fn series_identity() {
    let lit = r#"<c:tx><c:v>Literal</c:v></c:tx>"#;
    let a = ser(3, 1, lit, "", &[(0, "1")], 1);
    let b = ser(5, 0, "", "", &[(0, "2")], 1);
    let m = model(&chart_xml(&bar("col", "clustered", &format!("{a}{b}"), ""), ""));
    assert_eq!(m.series[0].name, "Series6", "order 0 first; no tx is SeriesN from idx");
    assert_eq!(m.series[1].name, "Literal");
    assert_eq!(m.series[1].color, DEFAULT_COLORS[3], "colour by idx, not position");
}

/// Categories given as numbers keep their labels; none at all number 1..N.
#[test]
fn categories_from_numbers_or_none() {
    let years = r#"<c:cat><c:numRef><c:f>S!A1</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val="2"/><c:pt idx="0"><c:v>2023</c:v></c:pt><c:pt idx="1"><c:v>2024</c:v></c:pt></c:numCache></c:numRef></c:cat>"#;
    let m = model(&chart_xml(&bar("col", "clustered", &ser(0, 0, "", years, &[(0, "1"), (1, "2")], 2), ""), ""));
    assert_eq!(m.categories, vec!["2023", "2024"]);
    let m = model(&chart_xml(&bar("col", "clustered", &ser(0, 0, "", "", &[(0, "1"), (1, "2"), (2, "3")], 3), ""), ""));
    assert_eq!(m.categories, vec!["1", "2", "3"]);
}

/// §21.2.2.93: no c:legend, no legend; legendPos and deleted entries.
#[test]
fn legend_presence_position_and_deleted_entries() {
    let two = format!("{}{}", ser(0, 0, "", "", &[(0, "1")], 1), ser(1, 1, "", "", &[(0, "2")], 1));
    let m = model(&chart_xml(&bar("col", "clustered", &two, ""), ""));
    assert!(m.legend.is_none());
    let lg = r#"<c:legend><c:legendPos val="b"/><c:legendEntry><c:idx val="1"/><c:delete val="1"/></c:legendEntry><c:overlay val="0"/></c:legend>"#;
    let m = model(&chart_xml(&bar("col", "clustered", &two, ""), lg));
    assert_eq!(m.legend.as_ref().map(|l| l.pos), Some(LegendPos::Bottom));
    assert_eq!(m.legend_entries().len(), 1, "the deleted entry is gone");
}

/// gapWidth and overlap are read; a stacked chart overlaps fully.
#[test]
fn bar_gap_and_overlap() {
    let s = ser(0, 0, "", "", &[(0, "1")], 1);
    let m = model(&chart_xml(&bar("col", "clustered", &s, r#"<c:gapWidth val="219"/><c:overlap val="-27"/>"#), ""));
    assert!((m.gap_width - 2.19).abs() < 1e-9 && (m.overlap + 0.27).abs() < 1e-9);
    let m = model(&chart_xml(&bar("col", "stacked", &s, r#"<c:overlap val="0"/>"#), ""));
    assert_eq!(m.overlap, 1.0);
}

/// Stacked series sit on the ones before; a 100 % stack sums to 1.
#[test]
fn stacking() {
    let two = format!("{}{}", ser(0, 0, "", "", &[(0, "1")], 1), ser(1, 1, "", "", &[(0, "3")], 1));
    let m = model(&chart_xml(&bar("col", "stacked", &two, ""), ""));
    assert_eq!(m.plotted(0), vec![Some((0.0, 1.0)), Some((1.0, 4.0))]);
    let m = model(&chart_xml(&bar("col", "percentStacked", &two, ""), ""));
    assert_eq!(m.plotted(0), vec![Some((0.0, 0.25)), Some((0.25, 1.0))]);
}

fn painted(xml: &[u8]) -> Vec<DrawCmd> {
    let rec = RecordingPainter::new();
    assert!(render_chart(xml, &rec, 0.0, 0.0, 400.0, 300.0));
    let cmds: Vec<DrawCmd> = rec.cmds().to_vec();
    cmds
}

/// barDir="bar" draws horizontal bars: their widths carry the values.
#[test]
fn horizontal_bars() {
    let s = ser(0, 0, "", "", &[(0, "1"), (1, "4")], 2);
    let rects: Vec<(f64, f64)> = painted(&chart_xml(&bar("bar", "clustered", &s, ""), ""))
        .into_iter()
        .filter_map(|c| match c { DrawCmd::FillRect { w, h, .. } => Some((w, h)), _ => None })
        .skip(1) // the chart-area ground
        .collect();
    assert_eq!(rects.len(), 2, "{rects:?}");
    assert!((rects[0].1 - rects[1].1).abs() < 1e-6, "same thickness");
    assert!(rects[1].0 > rects[0].0 * 3.0, "length follows the value: {rects:?}");
}

/// Axis settings: pinned min/max, deleted axes draw no labels, log scale.
#[test]
fn axis_scaling_and_visibility() {
    let s = ser(0, 0, "", "", &[(0, "85"), (1, "95")], 2);
    let pinned = bar("col", "clustered", &s, "").replace(
        r#"<c:valAx><c:axId val="2"/><c:scaling><c:orientation val="minMax"/></c:scaling>"#,
        r#"<c:valAx><c:axId val="2"/><c:scaling><c:orientation val="minMax"/><c:max val="100"/><c:min val="80"/></c:scaling>"#,
    );
    let sc = model(&chart_xml(&pinned, "")).scale();
    assert_eq!((sc.min, sc.max), (80.0, 100.0));

    let deleted = bar("col", "clustered", &s, "").replace(r#"<c:delete val="0"/>"#, r#"<c:delete/>"#);
    let texts = painted(&chart_xml(&deleted, "")).into_iter().filter(|c| matches!(c, DrawCmd::FillText { .. })).count();
    assert_eq!(texts, 0, "both axes deleted: no labels");

    let log = bar("col", "clustered", &ser(0, 0, "", "", &[(0, "10"), (1, "1000")], 2), "").replace(
        r#"<c:valAx><c:axId val="2"/><c:scaling>"#,
        r#"<c:valAx><c:axId val="2"/><c:scaling><c:logBase val="10"/>"#,
    );
    assert_eq!(model(&chart_xml(&log, "")).scale().ticks(), vec![10.0, 100.0, 1000.0]);
}

fn line(series: &str, extra: &str) -> String {
    format!(r#"<c:lineChart><c:grouping val="standard"/>{series}{extra}<c:axId val="1"/><c:axId val="2"/></c:lineChart>{AXES}"#)
}

/// Line markers: symbol none draws none; smooth lines are curves.
#[test]
fn line_markers_and_smoothing() {
    let pts = [(0, "1"), (1, "3"), (2, "2")];
    let plain = ser(0, 0, "", "", &pts, 3).replace("</c:ser>", r#"<c:marker><c:symbol val="none"/></c:marker></c:ser>"#);
    let cmds = painted(&chart_xml(&line(&plain, ""), ""));
    assert!(!cmds.iter().any(|c| matches!(c, DrawCmd::Arc { .. })), "no markers");
    let smooth = ser(0, 0, "", "", &pts, 3).replace("</c:ser>", r#"<c:marker><c:symbol val="none"/></c:marker><c:smooth val="1"/></c:ser>"#);
    assert!(painted(&chart_xml(&line(&smooth, ""), "")).iter().any(|c| matches!(c, DrawCmd::BezierCurveTo { .. })));
    // A blank point breaks the line (dispBlanksAs gap, the default).
    let gappy = ser(0, 0, "", "", &[(0, "1"), (1, "2"), (3, "3"), (4, "4")], 5)
        .replace("</c:ser>", r#"<c:marker><c:symbol val="none"/></c:marker></c:ser>"#);
    let moves = painted(&chart_xml(&line(&gappy, ""), "")).iter().filter(|c| matches!(c, DrawCmd::MoveTo { .. })).count();
    assert!(moves >= 2, "two runs of line");
}

/// 3-D bar charts are drawn as flat bar charts.
#[test]
fn three_d_bars_draw_flat() {
    let s = ser(0, 0, "", "", &[(0, "1")], 1);
    let xml = chart_xml(&format!(r#"<c:bar3DChart><c:barDir val="col"/><c:grouping val="clustered"/>{s}<c:axId val="1"/><c:axId val="2"/></c:bar3DChart>{AXES}"#), "");
    assert_eq!(model(&xml).kind, ChartType::Bar);
}

/// Theme colours come from the host's theme; automatic colours cycle its
/// accents; a dPt colours one bar; the chart area is white unless set.
#[test]
fn theme_point_and_ground_colours() {
    let scheme = |n: &str| match n {
        "accent1" => Some("#112233".to_owned()),
        "accent2" => Some("#445566".to_owned()),
        _ => None,
    };
    let themed = ser(0, 0, "", "", &[(0, "1"), (1, "2")], 2).replace(
        "<c:val>",
        r#"<c:spPr><a:solidFill><a:schemeClr val="accent2"/></a:solidFill></c:spPr><c:dPt><c:idx val="1"/><c:spPr><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill></c:spPr></c:dPt><c:val>"#,
    );
    let xml = chart_xml(&bar("col", "clustered", &themed, ""), "");
    let m = with_scheme(scheme, || model(&xml));
    assert_eq!(m.series[0].color, "#445566");
    assert_eq!(m.point_color(0, 1), "#ff0000", "the dPt colours its bar");
    assert_eq!(m.point_color(0, 0), "#445566");
    assert_eq!(with_scheme(scheme, || palette(0)), "#112233", "automatic colours from the theme");
    assert_eq!(m.chart_ground.fill.as_deref(), Some("#FFFFFF"));

    let no_fill = String::from_utf8(chart_xml(&bar("col", "clustered", &ser(0, 0, "", "", &[(0, "1")], 1), ""), "")).unwrap()
        .replace("</c:chart>", r#"</c:chart><c:spPr><a:noFill/></c:spPr>"#);
    assert_eq!(model(no_fill.as_bytes()).chart_ground.fill, None, "noFill: no ground");
}

/// varyColors=0 on a pie draws every slice in the series colour.
#[test]
fn a_pie_without_varied_colours_is_one_colour() {
    let s = ser(0, 0, "", &str_cats(&["a", "b"]), &[(0, "1"), (1, "1")], 2);
    let xml = chart_xml(&format!(r#"<c:pieChart><c:varyColors val="0"/>{s}</c:pieChart>"#), "");
    let m = model(&xml);
    assert_eq!(m.point_color(0, 0), m.point_color(0, 1));
    let xml = chart_xml(&format!(r#"<c:pieChart><c:varyColors val="1"/>{s}</c:pieChart>"#), "");
    let m = model(&xml);
    assert_ne!(m.point_color(0, 0), m.point_color(0, 1));
}

/// §21.2.2.121 numFmt: axis labels use the axis format, or the data's when
/// source-linked; display units divide them; date categories show as dates.
#[test]
fn axis_and_category_label_formats() {
    let s = ser(0, 0, "", "", &[(0, "0.25"), (1, "0.5")], 2);
    let with_fmt = |fmt: &str| bar("col", "clustered", &s, "").replace(
        r#"<c:majorGridlines/>"#,
        &format!(r#"<c:majorGridlines/><c:numFmt formatCode="{fmt}" sourceLinked="0"/>"#),
    );
    let m = model(&chart_xml(&with_fmt("0%"), ""));
    assert_eq!(m.value_label(0.5, false), "50%");

    let money = ser(0, 0, "", "", &[(0, "1500000")], 1).replace("<c:formatCode>General</c:formatCode>", r##"<c:formatCode>"$"#,##0</c:formatCode>"##);
    let m = model(&chart_xml(&bar("col", "clustered", &money, ""), ""));
    assert_eq!(m.value_label(1_500_000.0, false), "$1,500,000", "no axis format: the data's");

    let millions = bar("col", "clustered", &money, "").replace(
        r#"<c:majorGridlines/>"#,
        r#"<c:majorGridlines/><c:numFmt formatCode="0.0" sourceLinked="0"/><c:dispUnits><c:builtInUnit val="millions"/><c:dispUnitsLbl/></c:dispUnits>"#,
    );
    let m = model(&chart_xml(&millions, ""));
    assert_eq!(m.value_label(1_500_000.0, false), "1.5");
    assert_eq!(m.val_axis.unit, Some((1e6, Some("Millions"))));

    // Date serials as categories, in the cache's date format.
    let dates = r#"<c:cat><c:numRef><c:f>S!A1</c:f><c:numCache><c:formatCode>mmm-yy</c:formatCode><c:ptCount val="1"/><c:pt idx="0"><c:v>45292</c:v></c:pt></c:numCache></c:numRef></c:cat>"#;
    let m = model(&chart_xml(&bar("col", "clustered", &ser(0, 0, "", dates, &[(0, "1")], 1), ""), ""));
    assert_eq!(m.categories, vec!["Jan-24"]);
}
