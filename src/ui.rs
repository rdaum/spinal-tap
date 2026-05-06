// Copyright (C) 2026 Ryan Daum <ryan.daum@gmail.com>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, version 3.
//
// This program is distributed in the hope that it will be useful, but WITHOUT
// ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
// FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License along with
// this program. If not, see <https://www.gnu.org/licenses/>.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Cell, Chart, Clear, Dataset, GraphType, Paragraph, Row, Table, Wrap,
};

use crate::config::{Config, MetricConfig, MetricView};
use crate::dogstatsd::{MetricKind, Sample};

const DEFAULT_CHART_HEIGHT: u16 = 9;
const MIN_CHART_HEIGHT: u16 = 6;
const MAX_CHART_HEIGHT: u16 = 24;

pub fn run(config: Config, rx: Receiver<Sample>) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal, App::new(config, rx));
    ratatui::restore();
    result
}

fn run_app(terminal: &mut ratatui::DefaultTerminal, mut app: App) -> io::Result<()> {
    loop {
        app.drain_samples();
        terminal.draw(|frame| render(frame, &mut app))?;

        app.clamp_scrolls();

        let redraw_interval = app.config.redraw_interval();
        if handle_events(&mut app, redraw_interval)? {
            break Ok(());
        }
    }
}

fn handle_events(app: &mut App, timeout: Duration) -> io::Result<bool> {
    if !event::poll(timeout)? {
        return Ok(false);
    }

    loop {
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && app.handle_key(key.code, key.modifiers)
        {
            return Ok(true);
        }

        if !event::poll(Duration::ZERO)? {
            break;
        }
    }

    Ok(false)
}

struct App {
    config: Config,
    rx: Receiver<Sample>,
    metrics: HashMap<String, MetricState>,
    metric_config: HashMap<String, MetricConfig>,
    received: u64,
    started_at: Instant,
    focus: Focus,
    numeric_scroll: usize,
    chart_scroll: usize,
    numeric_selected: usize,
    chart_selected: usize,
    numeric_page: usize,
    chart_page: usize,
    chart_height: u16,
    details_open: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Focus {
    Numeric,
    Charts,
}

impl App {
    fn new(config: Config, rx: Receiver<Sample>) -> Self {
        let metric_config = config.metric_map();
        Self {
            config,
            rx,
            metrics: HashMap::new(),
            metric_config,
            received: 0,
            started_at: Instant::now(),
            focus: Focus::Numeric,
            numeric_scroll: 0,
            chart_scroll: 0,
            numeric_selected: 0,
            chart_selected: 0,
            numeric_page: 1,
            chart_page: 1,
            chart_height: DEFAULT_CHART_HEIGHT,
            details_open: false,
        }
    }

    fn drain_samples(&mut self) {
        while let Ok(sample) = self.rx.try_recv() {
            self.received += 1;
            if !self.should_track(&sample.name) {
                continue;
            }
            let history_points = self.config.history_points();
            let key = series_key(&sample.name, &sample.tags);
            let name = sample.name.clone();
            let labels = labels_text(&sample.tags);
            self.metrics
                .entry(key)
                .or_insert_with(|| MetricState::new(name, labels, history_points))
                .push(sample, history_points);
        }
    }

    fn should_track(&self, name: &str) -> bool {
        self.metric_config.is_empty() || self.metric_config.contains_key(name)
    }

    fn configured_view(&self, name: &str) -> MetricView {
        self.metric_config
            .get(name)
            .map(|config| config.view)
            .unwrap_or(MetricView::Numeric)
    }

    fn unit(&self, name: &str) -> &str {
        self.metric_config
            .get(name)
            .map(|config| config.unit.as_str())
            .unwrap_or("")
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        if self.details_open {
            if matches!(code, KeyCode::Enter | KeyCode::Esc) {
                self.details_open = false;
                return false;
            }
            return matches!(code, KeyCode::Char('q'))
                || (code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL));
        }

        if matches!(code, KeyCode::Char('q') | KeyCode::Esc)
            || (code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL))
        {
            return true;
        }

        match code {
            KeyCode::Enter => self.details_open = self.selected_metric().is_some(),
            KeyCode::Tab | KeyCode::Right | KeyCode::Left => self.toggle_focus(),
            KeyCode::Down | KeyCode::Char('j') => self.move_selected(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selected(-1),
            KeyCode::PageDown => self.page_focused(1),
            KeyCode::PageUp => self.page_focused(-1),
            KeyCode::Home => self.select_start(),
            KeyCode::End => self.select_end(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.resize_charts(1),
            KeyCode::Char('-') | KeyCode::Char('_') => self.resize_charts(-1),
            _ => {}
        }

        false
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Numeric => Focus::Charts,
            Focus::Charts => Focus::Numeric,
        };
    }

    fn move_selected(&mut self, delta: isize) {
        let count = self.focused_count();
        if count == 0 {
            return;
        }

        let selected = match self.focus {
            Focus::Numeric => &mut self.numeric_selected,
            Focus::Charts => &mut self.chart_selected,
        };

        if delta.is_negative() {
            *selected = selected.saturating_sub(delta.unsigned_abs());
        } else {
            *selected = selected.saturating_add(delta as usize).min(count - 1);
        }

        self.sync_viewports();
    }

    fn page_focused(&mut self, direction: isize) {
        let page = match self.focus {
            Focus::Numeric => self.numeric_page,
            Focus::Charts => self.chart_page,
        }
        .max(1);
        self.move_selected(direction.saturating_mul(page as isize));
    }

    fn resize_charts(&mut self, delta: i16) {
        let chart_height = i16::try_from(self.chart_height).unwrap_or(DEFAULT_CHART_HEIGHT as i16);
        self.chart_height =
            (chart_height + delta).clamp(MIN_CHART_HEIGHT as i16, MAX_CHART_HEIGHT as i16) as u16;
    }

    fn select_start(&mut self) {
        match self.focus {
            Focus::Numeric => self.numeric_selected = 0,
            Focus::Charts => self.chart_selected = 0,
        }
        self.sync_viewports();
    }

    fn select_end(&mut self) {
        match self.focus {
            Focus::Numeric => self.numeric_selected = self.numeric_metric_count().saturating_sub(1),
            Focus::Charts => self.chart_selected = self.chart_metric_count().saturating_sub(1),
        }
        self.sync_viewports();
    }

    fn clamp_scrolls(&mut self) {
        self.sync_viewports();
    }

    fn sync_viewports(&mut self) {
        let numeric_count = self.numeric_metric_count();
        let chart_count = self.chart_metric_count();

        sync_viewport(
            &mut self.numeric_scroll,
            &mut self.numeric_selected,
            self.numeric_page,
            numeric_count,
        );
        sync_viewport(
            &mut self.chart_scroll,
            &mut self.chart_selected,
            self.chart_page,
            chart_count,
        );
    }

    fn focused_count(&self) -> usize {
        match self.focus {
            Focus::Numeric => self.numeric_metric_count(),
            Focus::Charts => self.chart_metric_count(),
        }
    }

    fn numeric_metric_count(&self) -> usize {
        sorted_metrics(self)
            .into_iter()
            .filter(|metric| self.numeric_filter(metric))
            .count()
    }

    fn chart_metric_count(&self) -> usize {
        sorted_metrics(self)
            .into_iter()
            .filter(|metric| self.configured_view(&metric.name) == MetricView::Chart)
            .count()
    }

    fn numeric_filter(&self, metric: &MetricState) -> bool {
        self.configured_view(&metric.name) == MetricView::Numeric
            || (self.metric_config.is_empty() && self.chart_metric_count() == 0)
    }

    fn selected_metric(&self) -> Option<&MetricState> {
        let selected = match self.focus {
            Focus::Numeric => self.numeric_selected,
            Focus::Charts => self.chart_selected,
        };

        sorted_metrics(self)
            .into_iter()
            .filter(|metric| match self.focus {
                Focus::Numeric => self.numeric_filter(metric),
                Focus::Charts => self.configured_view(&metric.name) == MetricView::Chart,
            })
            .nth(selected)
    }
}

struct MetricState {
    name: String,
    labels: String,
    kind: MetricKind,
    latest: f64,
    total: f64,
    samples: VecDeque<Point>,
    last_seen: Instant,
    tags: Vec<(String, String)>,
}

impl MetricState {
    fn new(name: String, labels: String, history_points: usize) -> Self {
        Self {
            name,
            labels,
            kind: MetricKind::Unknown("?".to_string()),
            latest: 0.0,
            total: 0.0,
            samples: VecDeque::with_capacity(history_points),
            last_seen: Instant::now(),
            tags: Vec::new(),
        }
    }

    fn push(&mut self, sample: Sample, history_points: usize) {
        self.kind = sample.kind;
        self.latest = sample.value;
        if self.kind == MetricKind::Counter {
            self.total += sample.value;
        } else {
            self.total = sample.value;
        }
        self.last_seen = sample.received_at;
        self.tags = sample.tags;
        self.samples.push_back(Point {
            value: sample.value,
        });

        while self.samples.len() > history_points {
            self.samples.pop_front();
        }
    }
}

#[derive(Clone, Copy)]
struct Point {
    value: f64,
}

fn render(frame: &mut Frame, app: &mut App) {
    let [header, body, footer] = frame.area().layout(&Layout::vertical([
        Constraint::Length(3),
        Constraint::Fill(1),
        Constraint::Length(1),
    ]));

    render_header(frame, header, app);

    let [numeric_area, chart_area] = body.layout(&Layout::horizontal([
        Constraint::Percentage(42),
        Constraint::Percentage(58),
    ]));
    render_numeric(frame, numeric_area, app);
    render_charts(frame, chart_area, app);

    let footer_text = Line::from(vec![
        Span::styled(" q ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" quit  "),
        Span::styled(" Tab ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" focus  "),
        Span::styled(
            " PgUp/PgDn ",
            Style::default().fg(Color::Black).bg(Color::Gray),
        ),
        Span::raw(" page  "),
        Span::styled(" Enter ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" details  "),
        Span::styled(" +/- ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(format!(
            " chart height {}  listening on {}",
            app.chart_height, app.config.listen
        )),
    ]);
    frame.render_widget(footer_text, footer);

    if app.details_open {
        render_details(frame, app);
    }
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let uptime = app.started_at.elapsed().as_secs();
    let title = Line::from(vec![
        Span::styled(
            "spinal-tap",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  dogstatsd dashboard"),
    ]);
    let stats = Line::from(format!(
        "{} samples  {} metrics  uptime {}s",
        app.received,
        app.metrics.len(),
        uptime
    ));

    frame.render_widget(
        Paragraph::new(vec![title, stats])
            .block(Block::default().borders(Borders::BOTTOM))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_details(frame: &mut Frame, app: &App) {
    let area = bottom_third(frame.area());
    frame.render_widget(Clear, area);

    let Some(metric) = app.selected_metric() else {
        frame.render_widget(
            Paragraph::new("No metric selected").block(Block::bordered().title(" Metric Details ")),
            area,
        );
        return;
    };

    let unit = app.unit(&metric.name);
    let configured = app.metric_config.contains_key(&metric.name);
    let view = app.configured_view(&metric.name);
    let stats = retained_stats(metric);
    let delta = retained_delta(metric);
    let tags = if metric.tags.is_empty() {
        "none".to_string()
    } else {
        format_tags_full(&metric.tags)
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("name ", detail_label_style()),
            Span::raw(metric.name.clone()),
        ]),
        Line::from(vec![
            Span::styled("labels ", detail_label_style()),
            Span::raw(if metric.labels.is_empty() {
                "none".to_string()
            } else {
                metric.labels.clone()
            }),
        ]),
        Line::from(vec![
            Span::styled("kind ", detail_label_style()),
            Span::raw(metric.kind.to_string()),
            Span::styled("  view ", detail_label_style()),
            Span::raw(match view {
                MetricView::Chart => "chart",
                MetricView::Numeric => "numeric",
            }),
            Span::styled("  unit ", detail_label_style()),
            Span::raw(if unit.is_empty() { "none" } else { unit }),
            Span::styled("  configured ", detail_label_style()),
            Span::raw(if configured { "yes" } else { "auto" }),
        ]),
        Line::from(vec![
            Span::styled("latest ", detail_label_style()),
            Span::raw(format_value(metric.latest, unit)),
            Span::styled("  total ", detail_label_style()),
            Span::raw(format_value(metric.total, unit)),
            Span::styled("  delta ", detail_label_style()),
            Span::raw(delta.map_or_else(|| "n/a".to_string(), format_compact)),
            Span::styled("  last seen ", detail_label_style()),
            Span::raw(format!(
                "{:.1}s ago",
                metric.last_seen.elapsed().as_secs_f64()
            )),
        ]),
        Line::from(vec![
            Span::styled("retained ", detail_label_style()),
            Span::raw(format!(
                "{}/{} samples",
                metric.samples.len(),
                app.config.history_points()
            )),
            Span::styled("  min ", detail_label_style()),
            Span::raw(
                stats.map_or_else(|| "n/a".to_string(), |stats| format_value(stats.min, unit)),
            ),
            Span::styled("  max ", detail_label_style()),
            Span::raw(
                stats.map_or_else(|| "n/a".to_string(), |stats| format_value(stats.max, unit)),
            ),
            Span::styled("  avg ", detail_label_style()),
            Span::raw(
                stats.map_or_else(|| "n/a".to_string(), |stats| format_value(stats.avg, unit)),
            ),
        ]),
        Line::from(vec![
            Span::styled("tags ", detail_label_style()),
            Span::raw(tags),
        ]),
        Line::from(vec![
            Span::styled(
                "Enter/Esc ",
                Style::default().fg(Color::Black).bg(Color::Gray),
            ),
            Span::raw(" close  "),
            Span::styled("future ", detail_label_style()),
            Span::raw("actions can target this selected metric"),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .title(" Metric Details ")
                    .border_style(selected_style(true)),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn render_numeric(frame: &mut Frame, area: Rect, app: &mut App) {
    let visible_rows = usize::from(area.height.saturating_sub(3)).max(1);
    app.numeric_page = visible_rows;
    app.sync_viewports();

    let metrics = sorted_metrics(app)
        .into_iter()
        .filter(|metric| app.numeric_filter(metric))
        .collect::<Vec<_>>();

    let start = app.numeric_scroll.min(metrics.len().saturating_sub(1));
    let end = (start + visible_rows).min(metrics.len());

    let rows = metrics
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
        .map(|(index, metric)| {
            let unit = app.unit(&metric.name);
            let display_name = grouped_metric_name(&metrics, index);
            let row = Row::new(vec![
                Cell::from(display_name),
                Cell::from(metric.kind.to_string()),
                Cell::from(format_value(metric.latest, unit)),
                Cell::from(format_value(metric.total, unit)),
                Cell::from(format!("{:.1}s", metric.last_seen.elapsed().as_secs_f64())),
            ]);

            if index == app.numeric_selected {
                row.style(selected_style(app.focus == Focus::Numeric))
            } else {
                row
            }
        });

    let title = pane_title(
        "Numeric",
        app.focus == Focus::Numeric,
        start,
        end,
        metrics.len(),
        app.numeric_selected,
    );
    let table = Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(10),
            Constraint::Length(16),
            Constraint::Length(16),
            Constraint::Length(7),
        ],
    )
    .header(
        Row::new(["metric", "kind", "latest", "total", "age"]).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(Block::bordered().title(title));

    frame.render_widget(table, area);
}

fn render_charts(frame: &mut Frame, area: Rect, app: &mut App) {
    let visible_count = usize::from(area.height / app.chart_height).max(1);
    app.chart_page = visible_count;
    app.sync_viewports();

    let charted = sorted_metrics(app)
        .into_iter()
        .filter(|metric| app.configured_view(&metric.name) == MetricView::Chart)
        .collect::<Vec<_>>();

    if charted.is_empty() {
        frame.render_widget(
            Paragraph::new("No chart metrics configured yet.")
                .block(Block::bordered().title(" Charts "))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let start = app.chart_scroll.min(charted.len().saturating_sub(1));
    let end = (start + visible_count).min(charted.len());
    let visible_charted = &charted[start..end];

    for (index, metric) in visible_charted.iter().copied().enumerate() {
        let Some(area) = chart_area_at(area, index, app.chart_height) else {
            break;
        };
        let pane_title = if index == 0 {
            Some(pane_title(
                "Charts",
                app.focus == Focus::Charts,
                start,
                end,
                charted.len(),
                app.chart_selected,
            ))
        } else {
            None
        };
        let selected = start + index == app.chart_selected;
        render_chart(ChartRender {
            frame,
            area,
            metric,
            unit: app.unit(&metric.name),
            history_points: app.config.history_points(),
            pane_title,
            selected,
            focused: app.focus == Focus::Charts,
        });
    }
}

fn chart_area_at(area: Rect, index: usize, chart_height: u16) -> Option<Rect> {
    let offset = u16::try_from(index).ok()?.checked_mul(chart_height)?;
    if offset >= area.height {
        return None;
    }

    Some(Rect {
        x: area.x,
        y: area.y + offset,
        width: area.width,
        height: chart_height.min(area.height - offset),
    })
}

struct ChartRender<'a, 'b> {
    frame: &'a mut Frame<'b>,
    area: Rect,
    metric: &'a MetricState,
    unit: &'a str,
    history_points: usize,
    pane_title: Option<String>,
    selected: bool,
    focused: bool,
}

fn render_chart(args: ChartRender<'_, '_>) {
    let ChartRender {
        frame,
        area,
        metric,
        unit,
        history_points,
        pane_title,
        selected,
        focused,
    } = args;

    if metric.samples.is_empty() {
        return;
    }

    let history_points = history_points.max(2);
    let data = metric
        .samples
        .iter()
        .enumerate()
        .map(|(index, point)| (index as f64 + 0.5, point.value))
        .collect::<Vec<_>>();

    let min_y = data
        .iter()
        .map(|(_, value)| *value)
        .fold(f64::INFINITY, f64::min);
    let max_y = data
        .iter()
        .map(|(_, value)| *value)
        .fold(f64::NEG_INFINITY, f64::max);
    let y_pad = ((max_y - min_y) * 0.1).max(1.0);
    let y_bounds = [min_y - y_pad, max_y + y_pad];
    let x_bounds = [0.0, history_points as f64];

    let dataset = Dataset::default()
        .name(metric_chart_title(metric))
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(if selected { Color::Yellow } else { Color::Cyan })
        .data(&data);

    let y_title = if unit.is_empty() {
        metric.kind.to_string()
    } else {
        format!("{} ({unit})", metric.kind)
    };

    let title = match pane_title {
        Some(pane_title) => format!("{pane_title}  {}", metric_chart_title(metric)),
        None => format!(" {} ", metric_chart_title(metric)),
    };

    let chart = Chart::new(vec![dataset])
        .block(Block::bordered().title(title).border_style(if selected {
            selected_style(focused)
        } else {
            Style::default()
        }))
        .x_axis(
            Axis::default()
                .bounds(x_bounds)
                .labels(["0".to_string(), format_compact(history_points as f64)]),
        )
        .y_axis(
            Axis::default()
                .title(y_title.yellow())
                .bounds(y_bounds)
                .labels([
                    format_compact(y_bounds[0]),
                    format_compact((y_bounds[0] + y_bounds[1]) / 2.0),
                    format_compact(y_bounds[1]),
                ]),
        );

    frame.render_widget(chart, area);
}

fn pane_title(
    label: &str,
    focused: bool,
    start: usize,
    end: usize,
    total: usize,
    selected: usize,
) -> String {
    let marker = if focused { "*" } else { " " };
    if total == 0 {
        format!(" {marker} {label} 0/0 ")
    } else {
        format!(
            " {marker} {label} selected {}/{} visible {}-{} ",
            selected + 1,
            total,
            start + 1,
            end
        )
    }
}

fn grouped_metric_name(metrics: &[&MetricState], index: usize) -> String {
    let metric = metrics[index];
    if metric.labels.is_empty() {
        return metric.name.clone();
    }

    if index == 0 || metrics[index - 1].name != metric.name {
        format!("{} {}", metric.name, metric.labels)
    } else {
        format!("  {}", metric.labels)
    }
}

fn metric_chart_title(metric: &MetricState) -> String {
    if metric.labels.is_empty() {
        metric.name.clone()
    } else {
        format!("{} {}", metric.name, metric.labels)
    }
}

fn series_key(name: &str, tags: &[(String, String)]) -> String {
    let mut key = name.to_string();
    for (tag_key, tag_value) in canonical_tags(tags) {
        key.push('|');
        key.push_str(&tag_key);
        key.push('=');
        key.push_str(&tag_value);
    }
    key
}

fn labels_text(tags: &[(String, String)]) -> String {
    let tags = canonical_tags(tags);
    if tags.is_empty() {
        return String::new();
    }

    format!(
        "[{}]",
        tags.into_iter()
            .map(|(key, value)| {
                if value.is_empty() {
                    key
                } else {
                    format!("{key}:{value}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn canonical_tags(tags: &[(String, String)]) -> Vec<(String, String)> {
    let mut tags = tags.to_vec();
    tags.sort();
    tags
}

#[derive(Clone, Copy)]
struct RetainedStats {
    min: f64,
    max: f64,
    avg: f64,
}

fn retained_stats(metric: &MetricState) -> Option<RetainedStats> {
    let mut samples = metric.samples.iter();
    let first = samples.next()?.value;
    let mut min = first;
    let mut max = first;
    let mut sum = first;
    let mut count = 1usize;

    for point in samples {
        min = min.min(point.value);
        max = max.max(point.value);
        sum += point.value;
        count += 1;
    }

    Some(RetainedStats {
        min,
        max,
        avg: sum / count as f64,
    })
}

fn retained_delta(metric: &MetricState) -> Option<f64> {
    let mut samples = metric.samples.iter().rev();
    let latest = samples.next()?.value;
    let previous = samples.next()?.value;
    Some(latest - previous)
}

fn bottom_third(area: Rect) -> Rect {
    let height = (area.height / 3).max(8).min(area.height);
    Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(height),
        width: area.width,
        height,
    }
}

fn detail_label_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

fn selected_style(focused: bool) -> Style {
    let style = Style::default().fg(Color::Yellow);
    if focused {
        style.add_modifier(Modifier::BOLD).bg(Color::DarkGray)
    } else {
        style.add_modifier(Modifier::DIM)
    }
}

fn sync_viewport(scroll: &mut usize, selected: &mut usize, page: usize, count: usize) {
    if count == 0 {
        *scroll = 0;
        *selected = 0;
        return;
    }

    let page = page.max(1);
    *selected = (*selected).min(count - 1);

    if *selected < *scroll {
        *scroll = *selected;
    } else if *selected >= scroll.saturating_add(page) {
        *scroll = selected.saturating_add(1).saturating_sub(page);
    }

    *scroll = (*scroll).min(count.saturating_sub(page));
}

fn sorted_metrics(app: &App) -> Vec<&MetricState> {
    let mut metrics = app.metrics.values().collect::<Vec<_>>();
    metrics.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.labels.cmp(&b.labels)));
    metrics
}

fn format_value(value: f64, unit: &str) -> String {
    if unit.is_empty() {
        format_compact(value)
    } else {
        format!("{} {unit}", format_compact(value))
    }
}

fn format_compact(value: f64) -> String {
    if value.abs() >= 1000.0 || value.fract() == 0.0 {
        format!("{value:.0}")
    } else if value.abs() >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn format_tags_full(tags: &[(String, String)]) -> String {
    tags.iter()
        .map(|(key, value)| {
            if value.is_empty() {
                key.clone()
            } else {
                format!("{key}:{value}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn labelled_series_keys_are_tag_order_independent() {
        let left = vec![
            ("service".to_string(), "api".to_string()),
            ("host".to_string(), "a".to_string()),
        ];
        let right = vec![
            ("host".to_string(), "a".to_string()),
            ("service".to_string(), "api".to_string()),
        ];

        assert_eq!(
            series_key("requests", &left),
            series_key("requests", &right)
        );
        assert_eq!(labels_text(&left), "[host:a, service:api]");
    }

    #[test]
    fn labelled_samples_are_distinct_series() {
        let (_tx, rx) = mpsc::channel();
        let mut app = App::new(Config::default(), rx);
        let now = Instant::now();

        app.metrics.insert(
            series_key("requests", &[("service".to_string(), "api".to_string())]),
            MetricState::new(
                "requests".to_string(),
                labels_text(&[("service".to_string(), "api".to_string())]),
                8,
            ),
        );
        app.metrics.insert(
            series_key("requests", &[("service".to_string(), "worker".to_string())]),
            MetricState::new(
                "requests".to_string(),
                labels_text(&[("service".to_string(), "worker".to_string())]),
                8,
            ),
        );

        app.metrics
            .get_mut(&series_key(
                "requests",
                &[("service".to_string(), "api".to_string())],
            ))
            .unwrap()
            .push(
                Sample {
                    name: "requests".to_string(),
                    value: 1.0,
                    kind: MetricKind::Counter,
                    tags: vec![("service".to_string(), "api".to_string())],
                    received_at: now,
                },
                8,
            );

        assert_eq!(app.metrics.len(), 2);
    }
}
