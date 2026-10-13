//! Rendering: turns the latest [`EngineSnapshot`] plus [`UiState`] into widgets.
//!
//! Layout (full size):
//!
//! ```text
//! ┌ top bar (3 rows) ───────────────────────────────────────────┐
//! │ Layers (45%)            │ Stage (canvas)                     │
//! │                         │ Piano roll (canvas)                │
//! │                         │ Spectrum (sparkline)               │
//! ├ Events ticker           │ Sources                            ┤
//! └ footer / key legend ────────────────────────────────────────┘
//! ```
//!
//! Below 80 columns the right column is dropped; below 24 rows the spectrum
//! goes, then the bottom pane.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::canvas::{Canvas, Context, Line as CanvasLine, Points, Rectangle};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, TableState, Wrap,
};
use ratatui::Frame;
use sonder_core::EngineSnapshot;

use crate::format::{
    dim_color, fmt_bar_beat, fmt_num, fmt_time, fmt_time_tenths, layer_color, meter_color,
    meter_glyphs, mini_gauge, resample_max, source_state_style, summarize_attrs, truncate,
};
use crate::state::{freeway_lanes, UiState, HISTORY_LEN, MAP_FADE_SECONDS, ROLL_SECONDS};

/// Width of the inline sparkline column in the layers table.
const SPARK_W: u16 = 14;
/// Cells in the RMS meter.
const METER_CELLS: usize = 8;
/// Lowest and highest MIDI note on the piano roll.
const ROLL_MIDI: (f64, f64) = (36.0, 96.0);

/// Static part of the footer legend.
pub const KEY_LEGEND: &str =
    "q quit · space pause · [ ] tempo · - = density · k/K transpose · n/N mode · c chord · m/s mute/solo · ^s save · w web · ? help";

fn block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ))
}

/// Draw one frame.
pub fn draw(
    frame: &mut Frame<'_>,
    snap: &EngineSnapshot,
    state: &mut UiState,
    web_url: Option<&str>,
) {
    let area = frame.area();
    if area.height < 4 || area.width < 20 {
        frame.render_widget(Paragraph::new("terminal too small"), area);
        return;
    }
    let wide = area.width >= 80;
    let show_bottom = area.height >= 20;
    let show_spectrum = area.height >= 24;

    let mut constraints = vec![Constraint::Length(3), Constraint::Fill(1)];
    if show_bottom {
        constraints.push(Constraint::Length(10));
    }
    constraints.push(Constraint::Length(1));
    let rows = Layout::vertical(constraints).split(area);
    let (top, main, bottom, footer) = if show_bottom {
        (rows[0], rows[1], Some(rows[2]), rows[3])
    } else {
        (rows[0], rows[1], None, rows[2])
    };

    draw_top(frame, top, snap);

    if wide {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Fill(1)]).areas(main);
        draw_layers(frame, left, snap, state);
        let mut rc = vec![Constraint::Fill(3), Constraint::Fill(2)];
        if show_spectrum {
            rc.push(Constraint::Length(6));
        }
        let rparts = Layout::vertical(rc).split(right);
        draw_stage(frame, rparts[0], snap, state);
        draw_piano_roll(frame, rparts[1], snap, state);
        if show_spectrum {
            draw_spectrum(frame, rparts[2], snap);
        }
    } else {
        draw_layers(frame, main, snap, state);
    }

    if let Some(bottom) = bottom {
        let [ev, src] =
            Layout::horizontal([Constraint::Percentage(55), Constraint::Fill(1)]).areas(bottom);
        draw_events(frame, ev, snap, state);
        draw_sources(frame, src, snap);
    }

    draw_footer(frame, footer, state, web_url);

    if state.show_help {
        draw_help(frame, area, web_url);
    }
}

fn draw_top(frame: &mut Frame<'_>, area: Rect, snap: &EngineSnapshot) {
    let wide = area.width >= 100;
    let t = &snap.transport;
    let h = &snap.harmony;
    let sep = Span::styled(" · ", Style::default().fg(Color::DarkGray));
    let mut spans = vec![
        Span::styled(
            format!(
                " {} ",
                if snap.scene_name.is_empty() {
                    "sonder"
                } else {
                    &snap.scene_name
                }
            ),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Indexed(81))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    if snap.paused {
        spans.push(Span::styled(
            " PAUSED ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
    }
    spans.extend([
        Span::styled(fmt_time(snap.time), Style::default().fg(Color::Gray)),
        sep.clone(),
        Span::styled(
            fmt_bar_beat(t.bar, t.beat),
            Style::default().fg(Color::White),
        ),
        sep.clone(),
        Span::raw(format!("{} bpm", fmt_num(t.bpm))),
        sep.clone(),
        Span::styled(
            format!("{} {}", h.key_name, h.mode),
            Style::default().fg(Color::Indexed(220)),
        ),
        sep.clone(),
        Span::styled(
            h.chord_name.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    if wide {
        spans.extend([
            sep.clone(),
            Span::styled("E", Style::default().fg(Color::DarkGray)),
            Span::styled(
                mini_gauge(snap.mood.energy, 5),
                Style::default().fg(Color::Indexed(209)),
            ),
            Span::raw(" "),
            Span::styled("T", Style::default().fg(Color::DarkGray)),
            Span::styled(
                mini_gauge(snap.mood.tension, 5),
                Style::default().fg(Color::Indexed(171)),
            ),
            Span::raw(" "),
            Span::styled("B", Style::default().fg(Color::DarkGray)),
            Span::styled(
                mini_gauge(snap.mood.brightness, 5),
                Style::default().fg(Color::Indexed(220)),
            ),
        ]);
        spans.push(sep.clone());
        spans.push(Span::styled(
            if snap.audio_backend.is_empty() {
                "no audio".to_string()
            } else {
                snap.audio_backend.clone()
            },
            Style::default().fg(Color::DarkGray),
        ));
    }
    if snap.underruns > 0 {
        spans.push(sep);
        spans.push(Span::styled(
            format!("{} underruns", snap.underruns),
            Style::default().fg(Color::Red),
        ));
    }
    let note = Line::from(Span::styled(
        format!(" {}", snap.conductor_note),
        Style::default().fg(Color::DarkGray),
    ));
    let text = Text::from(vec![Line::from(spans), note]);
    let widget = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(widget, area);
}

fn draw_layers(frame: &mut Frame<'_>, area: Rect, snap: &EngineSnapshot, state: &mut UiState) {
    let blk = block("Layers");
    let inner = blk.inner(area);
    let show_spark = inner.width >= 60;
    let show_instr = inner.width >= 48;

    let header_cells = {
        let mut h = vec!["#", "name", "role"];
        if show_instr {
            h.push("instrument");
        }
        h.extend(["level", "n", "MS"]);
        if show_spark {
            h.push("signal");
        }
        h
    };
    let header = Row::new(
        header_cells
            .into_iter()
            .map(|c| Cell::from(c).style(Style::default().fg(Color::DarkGray))),
    );

    let mut widths = vec![
        Constraint::Length(3),
        Constraint::Fill(2),
        Constraint::Length(7),
    ];
    if show_instr {
        widths.push(Constraint::Fill(2));
    }
    widths.extend([
        Constraint::Length(METER_CELLS as u16),
        Constraint::Length(3),
        Constraint::Length(2),
    ]);
    if show_spark {
        widths.push(Constraint::Length(SPARK_W));
    }

    let rows: Vec<Row> = snap
        .layers
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let color = layer_color(i);
            let (lit, unlit) = meter_glyphs(l.meter.rms, METER_CELLS);
            let meter = Line::from(vec![
                Span::styled(lit, Style::default().fg(meter_color(l.meter.rms))),
                Span::styled(unlit, Style::default().fg(Color::Indexed(238))),
            ]);
            let flags = Line::from(vec![
                if l.muted {
                    Span::styled("M", Style::default().fg(Color::Black).bg(Color::Red))
                } else {
                    Span::styled("·", Style::default().fg(Color::Indexed(238)))
                },
                if l.soloed {
                    Span::styled("S", Style::default().fg(Color::Black).bg(Color::Yellow))
                } else {
                    Span::styled("·", Style::default().fg(Color::Indexed(238)))
                },
            ]);
            let name_style = if l.muted {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::CROSSED_OUT)
            } else {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            };
            let mut cells = vec![
                Cell::from(Span::styled(
                    if i < 9 {
                        format!("[{}]", i + 1)
                    } else {
                        "   ".into()
                    },
                    Style::default().fg(Color::DarkGray),
                )),
                Cell::from(Span::styled(l.name.clone(), name_style)),
                Cell::from(Span::styled(
                    l.role.name(),
                    Style::default().fg(Color::Gray),
                )),
            ];
            if show_instr {
                cells.push(Cell::from(Span::styled(
                    l.instrument.clone(),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            cells.push(Cell::from(meter));
            cells.push(Cell::from(Span::styled(
                format!("{:>2}", l.active_notes.min(99)),
                Style::default().fg(if l.active_notes > 0 {
                    Color::White
                } else {
                    Color::DarkGray
                }),
            )));
            cells.push(Cell::from(flags));
            if show_spark {
                cells.push(Cell::from(""));
            }
            Row::new(cells)
        })
        .collect();

    let mut ts = TableState::default().with_selected(if snap.layers.is_empty() {
        None
    } else {
        Some(state.selected)
    });
    let table = Table::new(rows, widths)
        .header(header)
        .block(blk)
        .row_highlight_style(Style::default().bg(Color::Indexed(236)))
        .column_spacing(1);
    frame.render_stateful_widget(table, area, &mut ts);

    // Overlay a Sparkline widget in the last column of each visible row.
    if show_spark && inner.height > 1 {
        let first_row_y = inner.y + 1; // header
        let visible = (inner.height - 1) as usize;
        let offset = ts.offset();
        let spark_x = inner.right().saturating_sub(SPARK_W);
        for (row_i, layer_i) in (offset..snap.layers.len()).take(visible).enumerate() {
            let Some(hist) = state.signal_hist.get(layer_i) else {
                continue;
            };
            let y = first_row_y + row_i as u16;
            if y >= inner.bottom() {
                break;
            }
            let rect = Rect {
                x: spark_x,
                y,
                width: SPARK_W,
                height: 1,
            };
            let data = hist.tail_u64(SPARK_W as usize);
            let spark = Sparkline::default()
                .data(data)
                .max(100)
                .style(Style::default().fg(layer_color(layer_i)));
            frame.render_widget(spark, rect);
        }
    }
}

fn draw_stage(frame: &mut Frame<'_>, area: Rect, snap: &EngineSnapshot, state: &UiState) {
    let kind = if snap.viz_kind.is_empty() {
        "generic"
    } else {
        snap.viz_kind.as_str()
    };
    let has_cars = snap.viz.values().any(|p| p.get("cars").is_some());
    let has_blobs = snap.viz.values().any(|p| p.get("blobs").is_some());
    let kind = match kind {
        "freeway" | "camera" | "map" | "generic" => kind,
        _ if has_cars => "freeway",
        _ if has_blobs => "camera",
        _ => "generic",
    };
    let title = format!("Stage · {kind}");
    let blk = block(&title);
    let inner = blk.inner(area);
    if inner.width == 0 || inner.height == 0 {
        frame.render_widget(blk, area);
        return;
    }
    let canvas = Canvas::default()
        .block(blk)
        .marker(Marker::Braille)
        .x_bounds([0.0, 1.0])
        .y_bounds([0.0, 1.0])
        .paint(|ctx| match kind {
            "freeway" => paint_freeway(ctx, snap, state),
            "camera" => paint_camera(ctx, snap),
            "map" => paint_map(ctx, state),
            _ => paint_generic(ctx, snap, state),
        });
    frame.render_widget(canvas, area);
}

fn paint_freeway(ctx: &mut Context<'_>, snap: &EngineSnapshot, state: &UiState) {
    let lanes = freeway_lanes(&snap.viz);
    if lanes > 1 {
        for l in 1..lanes {
            let x = l as f64 / lanes as f64;
            ctx.draw(&CanvasLine::new(x, 0.0, x, 1.0, Color::Indexed(236)));
        }
    }
    ctx.layer();
    // Trails first (dim), newest positions brighter.
    let mut by_age: Vec<Vec<(f64, f64)>> = vec![Vec::new(); crate::state::TRAIL_LEN];
    for trail in &state.trails {
        let n = trail.len();
        for (i, &(x, y)) in trail.iter().enumerate() {
            let age = n - 1 - i; // 0 = newest
            if age > 0 {
                let slot = age.min(by_age.len() - 1);
                by_age[slot].push((x, 1.0 - y));
            }
        }
    }
    const TRAIL_COLORS: [Color; 8] = [
        Color::White,
        Color::Indexed(250),
        Color::Indexed(246),
        Color::Indexed(243),
        Color::Indexed(240),
        Color::Indexed(238),
        Color::Indexed(236),
        Color::Indexed(235),
    ];
    for (age, pts) in by_age.iter().enumerate().rev() {
        if !pts.is_empty() {
            ctx.draw(&Points {
                coords: pts,
                color: TRAIL_COLORS[age.min(7)],
            });
        }
    }
    ctx.layer();
    let cars: Vec<(f64, f64)> = crate::state::freeway_cars(snap)
        .into_iter()
        .map(|(x, y)| (x, 1.0 - y))
        .collect();
    ctx.draw(&Points {
        coords: &cars,
        color: Color::Indexed(231),
    });
    // Bright headlights get a second, slightly offset dot so they look bigger.
    let bright: Vec<(f64, f64)> = snap
        .viz
        .values()
        .filter_map(|p| p.get("cars").and_then(|c| c.as_array()))
        .flatten()
        .filter(|c| c.get("bright").and_then(|b| b.as_f64()).unwrap_or(0.0) > 0.7)
        .map(|c| {
            let x = c.get("x").and_then(|v| v.as_f64()).unwrap_or(0.5);
            let y = c.get("y").and_then(|v| v.as_f64()).unwrap_or(0.5);
            (x + 0.004, 1.0 - y)
        })
        .collect();
    ctx.draw(&Points {
        coords: &bright,
        color: Color::Indexed(229),
    });
}

fn paint_camera(ctx: &mut Context<'_>, snap: &EngineSnapshot) {
    let blobs = snap
        .viz
        .values()
        .filter_map(|p| p.get("blobs").and_then(|b| b.as_array()))
        .flatten();
    for (i, b) in blobs.enumerate() {
        let x = b.get("x").and_then(|v| v.as_f64()).unwrap_or(0.5);
        let y = b.get("y").and_then(|v| v.as_f64()).unwrap_or(0.5);
        let w = b
            .get("w")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.02)
            .max(0.01);
        let h = b
            .get("h")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.02)
            .max(0.01);
        let bright = b.get("b").and_then(|v| v.as_f64()).unwrap_or(1.0);
        let color = if bright > 0.66 {
            Color::Indexed(231)
        } else if bright > 0.33 {
            Color::Indexed(250)
        } else {
            Color::Indexed(244)
        };
        let _ = i;
        ctx.draw(&Rectangle {
            x: x - w / 2.0,
            y: 1.0 - y - h / 2.0,
            width: w,
            height: h,
            color,
        });
    }
}

fn paint_map(ctx: &mut Context<'_>, state: &UiState) {
    // A faint graticule so an empty map still reads as a map.
    for i in 1..4 {
        let p = i as f64 / 4.0;
        ctx.draw(&CanvasLine::new(p, 0.0, p, 1.0, Color::Indexed(235)));
        ctx.draw(&CanvasLine::new(0.0, p, 1.0, p, Color::Indexed(235)));
    }
    ctx.layer();
    // Oldest first so fresh events paint on top.
    let mut buckets: Vec<Vec<(f64, f64)>> = vec![Vec::new(); 4];
    let mut colors: Vec<Vec<Color>> = vec![Vec::new(); 4];
    for me in state.map_events.iter() {
        let age = ((state.now - me.at) / MAP_FADE_SECONDS).clamp(0.0, 0.999);
        let bucket = (age * 4.0) as usize;
        buckets[bucket].push((me.lon01, me.lat01));
        colors[bucket].push(if bucket == 0 {
            layer_color(me.layer as usize)
        } else {
            dim_color(me.layer as usize)
        });
    }
    for bucket in (0..4).rev() {
        for (pt, color) in buckets[bucket].iter().zip(&colors[bucket]) {
            let color = if bucket >= 2 {
                Color::Indexed(240 - (bucket as u8 - 2) * 3)
            } else {
                *color
            };
            ctx.draw(&Points {
                coords: std::slice::from_ref(pt),
                color,
            });
        }
    }
}

fn paint_generic(ctx: &mut Context<'_>, snap: &EngineSnapshot, state: &UiState) {
    let n = snap.layers.len().max(1);
    let band = 1.0 / n as f64;
    for (i, hist) in state.signal_hist.iter().enumerate().take(snap.layers.len()) {
        let base = 1.0 - (i as f64 + 1.0) * band;
        let pts: Vec<f64> = hist.iter().collect();
        if pts.len() < 2 {
            continue;
        }
        let color = if snap.layers[i].muted {
            dim_color(i)
        } else {
            layer_color(i)
        };
        // Right-align: newest at x = 1.
        let x0 = 1.0 - (pts.len() as f64 - 1.0) / (HISTORY_LEN as f64 - 1.0);
        for w in pts.windows(2).enumerate() {
            let (j, pair) = w;
            let xa = x0 + j as f64 / (HISTORY_LEN as f64 - 1.0);
            let xb = x0 + (j + 1) as f64 / (HISTORY_LEN as f64 - 1.0);
            let ya = base + pair[0].clamp(0.0, 1.0) * band * 0.9;
            let yb = base + pair[1].clamp(0.0, 1.0) * band * 0.9;
            ctx.draw(&CanvasLine::new(xa, ya, xb, yb, color));
        }
        ctx.print(
            0.0,
            base + band * 0.5,
            Span::styled(
                truncate(&snap.layers[i].name, 10),
                Style::default().fg(color),
            ),
        );
    }
    if snap.layers.is_empty() {
        ctx.print(
            0.3,
            0.5,
            Span::styled("waiting for layers…", Style::default().fg(Color::DarkGray)),
        );
    }
}

fn draw_piano_roll(frame: &mut Frame<'_>, area: Rect, snap: &EngineSnapshot, state: &UiState) {
    let blk = block("Piano roll");
    let inner = blk.inner(area);
    if inner.width == 0 || inner.height == 0 {
        frame.render_widget(blk, area);
        return;
    }
    let now = snap.time.max(state.now);
    let t0 = now - ROLL_SECONDS;
    let canvas = Canvas::default()
        .block(blk)
        .marker(Marker::Braille)
        .x_bounds([t0, now])
        .y_bounds([ROLL_MIDI.0, ROLL_MIDI.1])
        .paint(|ctx| {
            // Octave guides.
            for midi in (48..96).step_by(12) {
                ctx.draw(&CanvasLine::new(
                    t0,
                    midi as f64,
                    now,
                    midi as f64,
                    Color::Indexed(235),
                ));
            }
            // Bar lines.
            let bpm = snap.transport.bpm.max(1.0);
            let bar_len = 60.0 / bpm * snap.transport.beats_per_bar.max(1) as f64;
            let mut t =
                now - snap.transport.phase * 60.0 / bpm - snap.transport.beat as f64 * 60.0 / bpm;
            while t > t0 {
                ctx.draw(&CanvasLine::new(
                    t,
                    ROLL_MIDI.0,
                    t,
                    ROLL_MIDI.1,
                    Color::Indexed(237),
                ));
                t -= bar_len;
            }
            ctx.layer();
            for n in state.notes.iter() {
                let start = n.at;
                let end = if n.active {
                    now
                } else {
                    start + n.duration.unwrap_or(0.25).max(0.08)
                };
                if end < t0 {
                    continue;
                }
                let midi = (n.midi as f64).clamp(ROLL_MIDI.0, ROLL_MIDI.1);
                let idx = n.layer.0 as usize;
                let color = if n.active {
                    layer_color(idx)
                } else {
                    dim_color(idx)
                };
                ctx.draw(&CanvasLine::new(
                    start.max(t0),
                    midi,
                    end.min(now),
                    midi,
                    color,
                ));
            }
        });
    frame.render_widget(canvas, area);
}

fn draw_spectrum(frame: &mut Frame<'_>, area: Rect, snap: &EngineSnapshot) {
    let title = format!(
        "Spectrum · master {}",
        fmt_num(f64::from((snap.master.rms * 100.0).round()))
    );
    let blk = block(&title);
    let inner = blk.inner(area);
    if inner.width == 0 || inner.height == 0 {
        frame.render_widget(blk, area);
        return;
    }
    let bins = resample_max(&snap.spectrum, inner.width as usize);
    let data: Vec<u64> = bins
        .iter()
        .map(|v| (v.clamp(0.0, 1.0) * 100.0).round() as u64)
        .collect();
    let spark = Sparkline::default()
        .block(blk)
        .data(data)
        .max(100)
        .style(Style::default().fg(Color::Indexed(81)));
    frame.render_widget(spark, area);
}

fn draw_events(frame: &mut Frame<'_>, area: Rect, snap: &EngineSnapshot, state: &UiState) {
    let blk = block("Events");
    let inner = blk.inner(area);
    let rows = inner.height as usize;
    let lines: Vec<Line> = state
        .recent_events(rows.max(1))
        .map(|ev| {
            let idx = ev.layer.0 as usize;
            let lname = snap.layers.get(idx).map(|l| l.name.as_str()).unwrap_or("?");
            let bar = mini_gauge(ev.intensity, 5);
            Line::from(vec![
                Span::styled(
                    format!("{:>7} ", fmt_time_tenths(ev.at)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{:<10} ", truncate(lname, 10)),
                    Style::default().fg(layer_color(idx)),
                ),
                Span::styled(
                    format!("{:<10} ", truncate(&ev.name, 10)),
                    Style::default().fg(Color::White),
                ),
                Span::styled(bar, Style::default().fg(Color::Indexed(209))),
                Span::raw(" "),
                Span::styled(
                    summarize_attrs(&ev.attrs, 2),
                    Style::default().fg(Color::Gray),
                ),
            ])
        })
        .collect();
    let text = if lines.is_empty() {
        Text::from(Span::styled(
            " no events yet",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Text::from(lines)
    };
    frame.render_widget(Paragraph::new(text).block(blk), area);
}

fn draw_sources(frame: &mut Frame<'_>, area: Rect, snap: &EngineSnapshot) {
    let blk = block("Sources");
    let inner = blk.inner(area);
    let wide = inner.width >= 50;
    let lines: Vec<Line> = snap
        .sources
        .iter()
        .map(|s| {
            let (label, color) = source_state_style(s.state);
            let mut spans = vec![
                Span::styled(
                    format!("{:<12} ", truncate(&s.name, 12)),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("{:<12} ", truncate(&s.kind, 12)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{label:<12} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:>6}/s ", fmt_num((s.rate * 10.0).round() / 10.0)),
                    Style::default().fg(Color::Gray),
                ),
            ];
            if wide {
                spans.push(Span::styled(
                    truncate(&s.message, 40),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Line::from(spans)
        })
        .collect();
    let text = if lines.is_empty() {
        Text::from(Span::styled(
            " no sources",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Text::from(lines)
    };
    frame.render_widget(Paragraph::new(text).block(blk), area);
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, state: &UiState, web_url: Option<&str>) {
    let mut spans = Vec::new();
    if let Some((msg, _)) = &state.status {
        spans.push(Span::styled(
            format!(" {msg} "),
            Style::default().fg(Color::Black).bg(Color::Indexed(114)),
        ));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        format!(" {KEY_LEGEND}"),
        Style::default().fg(Color::DarkGray),
    ));
    let url_w = web_url.map(|u| u.chars().count() as u16 + 2).unwrap_or(0);
    let (legend_area, url_area) = if url_w > 0 && area.width > url_w + 20 {
        let [l, r] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(url_w)]).areas(area);
        (l, Some(r))
    } else {
        (area, None)
    };
    frame.render_widget(Paragraph::new(Line::from(spans)), legend_area);
    if let (Some(url), Some(r)) = (web_url, url_area) {
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!("{url} "),
                Style::default().fg(Color::Indexed(81)),
            ))
            .right_aligned(),
            r,
        );
    }
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, web_url: Option<&str>) {
    let lines = [
        ("q / Esc / Ctrl-C", "quit"),
        ("space", "pause / resume"),
        ("↑ / ↓", "select layer"),
        ("m / s", "mute / solo selected layer"),
        ("1-9", "toggle mute of layer N"),
        ("shift+1-9  (!@#$%^&*()", "toggle solo of layer N"),
        ("[ / ]", "tempo -2 / +2 bpm"),
        ("- / =", "density -0.1 / +0.1"),
        ("k / K", "transpose +1 / -1 semitone"),
        ("n / N", "next / previous mode"),
        ("c", "force chord change"),
        ("w", "open the web UI in a browser"),
        ("Ctrl-S", "save the current scene to <scene>.toml"),
        ("?", "toggle this help"),
    ];
    let width = 56u16.min(area.width.saturating_sub(2));
    let height = (lines.len() as u16 + 4).min(area.height.saturating_sub(1));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let mut text: Vec<Line> = lines
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(
                    format!(" {k:<24}"),
                    Style::default().fg(Color::Indexed(220)),
                ),
                Span::styled(*d, Style::default().fg(Color::White)),
            ])
        })
        .collect();
    text.push(Line::raw(""));
    text.push(Line::from(Span::styled(
        match web_url {
            Some(u) => format!(" web UI: {u}"),
            None => " web UI: not running (start with --web)".to_string(),
        },
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Clear, popup);
    let para = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .alignment(Alignment::Left)
        .block(
            block("Keys")
                .border_style(Style::default().fg(Color::Indexed(81)))
                .style(Style::default().bg(Color::Indexed(234))),
        );
    frame.render_widget(para, popup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use sonder_core::{
        EventSnapshot, HarmonySnapshot, LayerId, LayerSnapshot, Meter, Mood, NoteSnapshot, Role,
        SourceSnapshot, SourceState, TransportPos, Value,
    };
    use std::collections::BTreeMap;

    fn fake_snapshot() -> EngineSnapshot {
        let mk_layer = |i: u16, name: &str, role: Role, instr: &str, sig: &str, v: f64| {
            let mut l = LayerSnapshot {
                id: LayerId(i),
                name: name.into(),
                role,
                instrument: instr.into(),
                muted: i == 2,
                soloed: i == 1,
                meter: Meter {
                    rms: 0.2 * (i as f32 + 1.0),
                    peak: 0.5,
                },
                active_notes: i * 2,
                source: Some("traffic".into()),
                ..Default::default()
            };
            l.signals.insert(sig.into(), v);
            l
        };
        let mut attrs = BTreeMap::new();
        attrs.insert("lane".to_string(), Value::Num(2.0));
        attrs.insert("speed".to_string(), Value::Num(61.25));
        attrs.insert("lat01".to_string(), Value::Num(0.4));
        attrs.insert("lon01".to_string(), Value::Num(0.7));
        let events = (0..10)
            .map(|i| EventSnapshot {
                at: 20.0 + i as f64 * 0.7,
                layer: LayerId(i % 3),
                name: if i % 2 == 0 {
                    "car".into()
                } else {
                    "quake".into()
                },
                intensity: 0.1 * i as f64,
                attrs: attrs.clone(),
            })
            .collect();
        let notes = (0..30)
            .map(|i| NoteSnapshot {
                at: 15.0 + i as f64 * 0.5,
                layer: LayerId(i % 3),
                midi: 48.0 + (i % 24) as f32,
                velocity: 0.7,
                pan: 0.5,
                duration: Some(0.4),
                active: i > 26,
            })
            .collect();
        let mut viz = BTreeMap::new();
        viz.insert(
            "traffic".to_string(),
            serde_json::json!({"lanes": 4, "cars": [{"x": 0.1, "y": 0.3, "lane": 0, "bright": 0.9}, {"x": 0.6, "y": 0.8, "lane": 2, "bright": 0.2}]}),
        );
        EngineSnapshot {
            scene_name: "Night Freeway".into(),
            viz_kind: "freeway".into(),
            time: 30.0,
            paused: false,
            transport: TransportPos {
                bpm: 62.0,
                beats_per_bar: 4,
                bar: 11,
                beat: 2,
                phase: 0.5,
                beats: 46.5,
            },
            harmony: HarmonySnapshot {
                tonic: 2,
                key_name: "D".into(),
                mode: "lydian".into(),
                scale: vec![2, 4, 6, 8, 9, 11, 1],
                chord: vec![2, 6, 9, 1],
                chord_name: "Dmaj7".into(),
                chord_degree: 1,
                beats_to_change: 5.5,
            },
            mood: Mood {
                energy: 0.6,
                tension: 0.3,
                brightness: 0.8,
            },
            conductor_note: "energy 0.62 (driven by traffic.density), tension rising".into(),
            layers: vec![
                mk_layer(0, "traffic", Role::Pad, "pad.glass", "density", 0.4),
                mk_layer(1, "headlights", Role::Pluck, "pluck.nylon", "rate", 0.7),
                mk_layer(2, "quakes", Role::Bells, "bells.glass", "mag", 0.1),
            ],
            sources: vec![
                SourceSnapshot {
                    name: "traffic".into(),
                    kind: "synthetic.freeway".into(),
                    state: SourceState::Live,
                    message: "flow 0.4".into(),
                    rate: 10.0,
                    last_sample_at: Some(29.9),
                    readings: BTreeMap::new(),
                },
                SourceSnapshot {
                    name: "usgs".into(),
                    kind: "usgs.earthquakes".into(),
                    state: SourceState::Reconnecting,
                    message: "HTTP 503, retry in 8s".into(),
                    rate: 0.0,
                    last_sample_at: None,
                    readings: BTreeMap::new(),
                },
            ],
            events,
            notes,
            master: Meter {
                rms: 0.3,
                peak: 0.6,
            },
            spectrum: (0..48)
                .map(|i| ((i as f32) / 48.0 * std::f32::consts::PI).sin())
                .collect(),
            viz,
            underruns: 3,
            audio_backend: "cpal:pulse".into(),
        }
    }

    fn buffer_text(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn render(w: u16, h: u16, snap: &EngineSnapshot, state: &mut UiState) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, snap, state, Some("http://127.0.0.1:7878")))
            .unwrap();
        buffer_text(&term)
    }

    #[test]
    fn renders_full_layout() {
        let snap = fake_snapshot();
        let mut state = UiState::new();
        state.ingest(&snap);
        // Second tick so histories/trails have a couple of points.
        state.ingest(&snap);
        let text = render(120, 40, &snap, &mut state);
        assert!(text.contains("Night Freeway"), "{text}");
        assert!(text.contains("Dmaj7"), "{text}");
        assert!(text.contains("D lydian"), "{text}");
        assert!(text.contains("12.3"), "{text}");
        assert!(text.contains("62 bpm"), "{text}");
        assert!(text.contains("3 underruns"), "{text}");
        for name in ["traffic", "headlights", "quakes"] {
            assert!(text.contains(name), "missing layer {name}\n{text}");
        }
        assert!(text.contains("live"), "{text}");
        assert!(text.contains("reconnecting"), "{text}");
        assert!(text.contains("Stage · freeway"), "{text}");
        assert!(text.contains("Piano roll"), "{text}");
        assert!(text.contains("Spectrum"), "{text}");
        assert!(text.contains("Events"), "{text}");
        assert!(text.contains("speed=61.25"), "{text}");
        assert!(text.contains("q quit"), "{text}");
        assert!(text.contains("127.0.0.1:7878"), "{text}");
    }

    #[test]
    fn renders_small_and_paused_without_panicking() {
        let mut snap = fake_snapshot();
        snap.paused = true;
        let mut state = UiState::new();
        state.ingest(&snap);
        let text = render(60, 16, &snap, &mut state);
        assert!(text.contains("Night Freeway"), "{text}");
        assert!(text.contains("PAUSED"), "{text}");
        assert!(text.contains("Layers"), "{text}");
        // Right column and bottom pane hidden at this size.
        assert!(!text.contains("Piano roll"), "{text}");
        for (w, h) in [(20, 4), (10, 2), (80, 24), (79, 23), (200, 60)] {
            let _ = render(w, h, &snap, &mut state);
        }
    }

    /// Tiny terminals (and the help popup on them) must never panic.
    #[test]
    fn renders_at_tiny_sizes() {
        let snap = fake_snapshot();
        let mut state = UiState::new();
        state.ingest(&snap);
        for help in [false, true] {
            state.show_help = help;
            for (w, h) in [(1, 1), (10, 3), (40, 10), (20, 5), (80, 5), (21, 24), (120, 4)] {
                let _ = render(w, h, &snap, &mut state);
            }
        }
    }

    /// The layer list can shrink between snapshots; drawing must index nothing stale.
    #[test]
    fn layers_shrinking_between_snapshots_does_not_panic() {
        let full = fake_snapshot();
        let mut state = UiState::new();
        state.ingest(&full);
        state.selected = 2;
        let mut small = fake_snapshot();
        small.layers.truncate(1);
        state.ingest(&small);
        let text = render(120, 40, &small, &mut state);
        assert!(text.contains("traffic"), "{text}");
        // Events referencing layers that no longer exist render with a placeholder.
        assert!(text.contains("?"), "{text}");
        // Draw with a snapshot the state has not ingested (resize path races).
        let _ = render(120, 40, &full, &mut state);
        let mut none = fake_snapshot();
        none.layers.clear();
        let _ = render(120, 40, &none, &mut state);
    }

    #[test]
    fn renders_each_viz_kind_and_help() {
        let mut state = UiState::new();
        for kind in ["camera", "map", "generic", "freeway", "unknown", ""] {
            let mut snap = fake_snapshot();
            snap.viz_kind = kind.into();
            snap.viz.insert(
                "cam".into(),
                serde_json::json!({"blobs": [{"x": 0.5, "y": 0.5, "w": 0.1, "h": 0.1, "b": 0.9}]}),
            );
            state.ingest(&snap);
            let text = render(100, 30, &snap, &mut state);
            assert!(text.contains("Stage"), "{text}");
        }
        state.show_help = true;
        let snap = fake_snapshot();
        let text = render(100, 30, &snap, &mut state);
        assert!(text.contains("toggle this help"), "{text}");
        // Empty snapshot renders too.
        let mut empty_state = UiState::new();
        let empty = EngineSnapshot::default();
        empty_state.ingest(&empty);
        let text = render(120, 40, &empty, &mut empty_state);
        assert!(text.contains("no sources"), "{text}");
        assert!(text.contains("no events yet"), "{text}");
    }
}
