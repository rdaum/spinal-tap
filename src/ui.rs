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

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::PathBuf;
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

use crate::config::{Config, MetricConfig, MetricDisplay, MetricKindConfig, MetricView};
use crate::dogstatsd::{MetricKind, Sample};

const DEFAULT_CHART_HEIGHT: u16 = 9;
const MIN_CHART_HEIGHT: u16 = 6;
const MAX_CHART_HEIGHT: u16 = 24;

pub fn run(config: Config, config_path: PathBuf, rx: Receiver<Sample>) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal, App::new(config, config_path, rx));
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
    config_path: PathBuf,
    rx: Receiver<Sample>,
    metrics: HashMap<String, MetricState>,
    seen_metrics: HashMap<String, SeenMetric>,
    metric_config: HashMap<String, MetricConfig>,
    hidden_metrics: HashSet<String>,
    track_all: bool,
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
    search_active: bool,
    search_query: String,
    add_open: bool,
    add_query: String,
    add_selected: usize,
    add_view: MetricView,
    add_kind: MetricKindConfig,
    add_display: MetricDisplay,
    add_unit: String,
    add_field: AddField,
    ctrl_x_armed: bool,
    status_message: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Numeric,
    Charts,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AddField {
    Metric,
    View,
    Kind,
    Display,
    Unit,
}

impl App {
    fn new(config: Config, config_path: PathBuf, rx: Receiver<Sample>) -> Self {
        let metric_config = config.metric_map();
        let track_all = metric_config.is_empty();
        let mut app = Self {
            config,
            config_path,
            rx,
            metrics: HashMap::new(),
            seen_metrics: HashMap::new(),
            metric_config,
            hidden_metrics: HashSet::new(),
            track_all,
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
            search_active: false,
            search_query: String::new(),
            add_open: false,
            add_query: String::new(),
            add_selected: 0,
            add_view: MetricView::Numeric,
            add_kind: MetricKindConfig::Auto,
            add_display: MetricDisplay::Default,
            add_unit: String::new(),
            add_field: AddField::Metric,
            ctrl_x_armed: false,
            status_message: None,
        };
        app.ensure_config_placeholders();
        app
    }

    fn drain_samples(&mut self) {
        while let Ok(sample) = self.rx.try_recv() {
            self.received += 1;
            self.record_seen(&sample);
            if !self.should_track(&sample.name) {
                continue;
            }
            let history_points = self.config.history_points();
            let key = series_key(&sample.name, &sample.tags);
            let name = sample.name.clone();
            let labels = labels_text(&sample.tags);
            self.remove_empty_base_placeholder(&name, &sample.tags);
            self.metrics
                .entry(key)
                .or_insert_with(|| MetricState::new(name, labels, history_points))
                .push(sample, history_points);
        }
    }

    fn record_seen(&mut self, sample: &Sample) {
        self.seen_metrics
            .entry(sample.name.clone())
            .or_insert_with(|| SeenMetric::new(sample.name.clone()))
            .push(sample);
    }

    fn remove_empty_base_placeholder(&mut self, name: &str, tags: &[(String, String)]) {
        if tags.is_empty() {
            return;
        }

        let base_key = series_key(name, &[]);
        if self
            .metrics
            .get(&base_key)
            .is_some_and(|metric| metric.samples.is_empty())
        {
            self.metrics.remove(&base_key);
        }
    }

    fn should_track(&self, name: &str) -> bool {
        (self.track_all || self.metric_config.contains_key(name))
            && !self.hidden_metrics.contains(name)
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

    fn configured_kind(&self, name: &str) -> MetricKindConfig {
        self.metric_config
            .get(name)
            .map(|config| config.kind)
            .unwrap_or(MetricKindConfig::Auto)
    }

    fn display_mode(&self, name: &str) -> MetricDisplay {
        self.metric_config
            .get(name)
            .map(|config| config.display)
            .unwrap_or(MetricDisplay::Default)
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        if self.ctrl_x_armed {
            return self.handle_ctrl_x_key(code, modifiers);
        }

        if is_ctrl_x(code, modifiers) {
            self.ctrl_x_armed = true;
            self.status_message = Some("C-x".to_string());
            return false;
        }

        if self.add_open {
            return self.handle_add_key(code, modifiers);
        }

        if self.search_active {
            return self.handle_search_key(code, modifiers);
        }

        if self.details_open {
            if matches!(code, KeyCode::Char('d' | 'D'))
                && (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT)
            {
                if let Some(name) = self.selected_metric().map(|metric| metric.name.clone()) {
                    self.remove_metric(&name);
                }
                return false;
            }

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
            KeyCode::Char('a' | 'A') => self.open_add(),
            KeyCode::Char('/') => self.search_active = true,
            KeyCode::Char('c' | 'C') if !self.search_query.is_empty() => {
                self.search_query.clear();
                self.sync_viewports();
            }
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

    fn handle_add_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }

        if code == KeyCode::Char('v') && modifiers.contains(KeyModifiers::CONTROL) {
            self.add_view = next_metric_view(self.add_view, 1);
            return false;
        }

        if code == KeyCode::Char('k') && modifiers.contains(KeyModifiers::CONTROL) {
            self.add_kind = next_metric_kind_config(self.add_kind, 1);
            return false;
        }

        if code == KeyCode::Char('d') && modifiers.contains(KeyModifiers::CONTROL) {
            self.add_display = next_metric_display(self.add_display, 1);
            return false;
        }

        match code {
            KeyCode::Esc => self.add_open = false,
            KeyCode::Enter => self.add_selected_metric(),
            KeyCode::Tab => self.next_add_field(),
            KeyCode::Left => self.adjust_add_field(-1),
            KeyCode::Right => self.adjust_add_field(1),
            KeyCode::Down => self.move_add_selection(1),
            KeyCode::Up => self.move_add_selection(-1),
            KeyCode::Backspace if self.add_field == AddField::Unit => {
                self.add_unit.pop();
            }
            KeyCode::Backspace if self.add_field == AddField::Metric => {
                self.add_query.pop();
                self.clamp_add_selection();
            }
            KeyCode::Char('w' | 'W') if modifiers.contains(KeyModifiers::CONTROL) => {
                match self.add_field {
                    AddField::Metric => {
                        self.add_query.clear();
                        self.clamp_add_selection();
                    }
                    AddField::Unit => self.add_unit.clear(),
                    AddField::View | AddField::Kind | AddField::Display => {}
                }
            }
            KeyCode::Char(ch) if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
                match self.add_field {
                    AddField::Metric => {
                        self.add_query.push(ch);
                        self.clamp_add_selection();
                    }
                    AddField::Unit => self.add_unit.push(ch),
                    AddField::View | AddField::Kind | AddField::Display => {}
                }
            }
            _ => {}
        }

        false
    }

    fn open_add(&mut self) {
        self.add_open = true;
        self.add_query.clear();
        self.add_selected = 0;
        self.add_view = MetricView::Numeric;
        self.add_kind = MetricKindConfig::Auto;
        self.add_display = MetricDisplay::Default;
        self.add_unit.clear();
        self.add_field = AddField::Metric;
    }

    fn next_add_field(&mut self) {
        self.add_field = match self.add_field {
            AddField::Metric => AddField::View,
            AddField::View => AddField::Kind,
            AddField::Kind => AddField::Display,
            AddField::Display => AddField::Unit,
            AddField::Unit => AddField::Metric,
        };
    }

    fn adjust_add_field(&mut self, direction: isize) {
        match self.add_field {
            AddField::View => self.add_view = next_metric_view(self.add_view, direction),
            AddField::Kind => self.add_kind = next_metric_kind_config(self.add_kind, direction),
            AddField::Display => {
                self.add_display = next_metric_display(self.add_display, direction)
            }
            AddField::Metric | AddField::Unit => {}
        }
    }

    fn move_add_selection(&mut self, delta: isize) {
        let count = self.add_candidates().len();
        if count == 0 {
            self.add_selected = 0;
            return;
        }

        if delta.is_negative() {
            self.add_selected = self.add_selected.saturating_sub(delta.unsigned_abs());
        } else {
            self.add_selected = self
                .add_selected
                .saturating_add(delta as usize)
                .min(count - 1);
        }
    }

    fn clamp_add_selection(&mut self) {
        self.add_selected = self
            .add_selected
            .min(self.add_candidates().len().saturating_sub(1));
    }

    fn add_selected_metric(&mut self) {
        let selected = self
            .add_candidates()
            .get(self.add_selected)
            .map(|metric| (metric.name.clone(), metric.kind.clone()));
        let name = selected.as_ref().map(|(name, _)| name.clone()).or_else(|| {
            let name = self.add_query.trim();
            (!name.is_empty()).then(|| name.to_string())
        });

        let Some(name) = name else {
            return;
        };

        self.hidden_metrics.remove(&name);
        self.metric_config.insert(
            name.clone(),
            MetricConfig {
                name: name.clone(),
                view: self.add_view,
                kind: self.add_kind,
                display: self.add_display,
                unit: self.add_unit.trim().to_string(),
            },
        );
        let placeholder_kind = if self.add_kind == MetricKindConfig::Auto {
            selected
                .as_ref()
                .map(|(_, kind)| metric_kind_config_from_metric_kind(kind))
                .unwrap_or(MetricKindConfig::Auto)
        } else {
            self.add_kind
        };
        self.ensure_placeholder_metric(&name, placeholder_kind);
        self.add_open = false;
        self.add_query.clear();
        self.add_unit.clear();
        self.add_selected = 0;
        self.add_field = AddField::Metric;
        self.select_metric_by_name(&name);
    }

    fn ensure_placeholder_metric(&mut self, name: &str, kind: MetricKindConfig) {
        if self.metrics.values().any(|metric| metric.name == name) {
            return;
        }

        let mut metric = MetricState::new(
            name.to_string(),
            String::new(),
            self.config.history_points(),
        );
        if let Some(kind) = metric_kind_from_config(kind) {
            metric.kind = kind;
        }

        self.metrics.insert(series_key(name, &[]), metric);
    }

    fn ensure_config_placeholders(&mut self) {
        let configs = self.metric_config.values().cloned().collect::<Vec<_>>();
        for config in configs {
            self.ensure_placeholder_metric(&config.name, config.kind);
        }
    }

    fn remove_metric(&mut self, name: &str) {
        self.metric_config.remove(name);
        self.hidden_metrics.insert(name.to_string());
        self.metrics.retain(|_, metric| metric.name != name);
        self.details_open = false;
        self.sync_viewports();
    }

    fn handle_ctrl_x_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        if is_ctrl_x(code, modifiers) {
            self.status_message = Some("C-x".to_string());
            return false;
        }

        self.ctrl_x_armed = false;
        match code {
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Char('s' | 'S')
                if modifiers.is_empty()
                    || modifiers == KeyModifiers::SHIFT
                    || modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.save_config()
            }
            KeyCode::Esc => self.status_message = Some("C-x canceled".to_string()),
            _ => self.status_message = Some("unknown C-x command".to_string()),
        }

        false
    }

    fn save_config(&mut self) {
        let path = self.config_path.display().to_string();
        match self.write_config() {
            Ok(()) => self.status_message = Some(format!("saved {path}")),
            Err(err) => self.status_message = Some(format!("save failed: {err}")),
        }
    }

    fn write_config(&self) -> io::Result<()> {
        if let Some(parent) = self
            .config_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }

        let contents = toml::to_string_pretty(&self.config_for_save()).map_err(io::Error::other)?;
        fs::write(&self.config_path, contents)
    }

    fn config_for_save(&self) -> Config {
        let mut metric_config = self.metric_config.clone();

        if self.track_all {
            for metric in self.metrics.values() {
                if self.hidden_metrics.contains(&metric.name) {
                    continue;
                }

                metric_config
                    .entry(metric.name.clone())
                    .or_insert_with(|| MetricConfig {
                        name: metric.name.clone(),
                        view: self.configured_view(&metric.name),
                        kind: metric_kind_config_from_metric_kind(&metric.kind),
                        display: self.display_mode(&metric.name),
                        unit: self.unit(&metric.name).to_string(),
                    });
            }
        }

        let mut metrics = metric_config
            .into_values()
            .filter(|metric| !self.hidden_metrics.contains(&metric.name))
            .collect::<Vec<_>>();
        metrics.sort_by(|a, b| a.name.cmp(&b.name));

        Config {
            listen: self.config.listen.clone(),
            history_points: self.config.history_points,
            redraw_millis: self.config.redraw_millis,
            metrics,
        }
    }

    fn handle_search_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }

        match code {
            KeyCode::Enter => self.search_active = false,
            KeyCode::Esc => self.search_active = false,
            KeyCode::Backspace => {
                self.search_query.pop();
                self.sync_viewports();
            }
            KeyCode::Char('w' | 'W') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_query.clear();
                self.sync_viewports();
            }
            KeyCode::Char(ch) if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
                self.search_query.push(ch);
                self.sync_viewports();
            }
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
        self.numeric_metrics().len()
    }

    fn chart_metric_count(&self) -> usize {
        self.chart_metrics().len()
    }

    fn numeric_filter(&self, metric: &MetricState) -> bool {
        self.configured_view(&metric.name) == MetricView::Numeric && self.matches_search(metric)
    }

    fn chart_filter(&self, metric: &MetricState) -> bool {
        self.configured_view(&metric.name) == MetricView::Chart && self.matches_search(metric)
    }

    fn numeric_metrics(&self) -> Vec<&MetricState> {
        sorted_metrics(self)
            .into_iter()
            .filter(|metric| self.numeric_filter(metric))
            .collect()
    }

    fn chart_metrics(&self) -> Vec<&MetricState> {
        sorted_metrics(self)
            .into_iter()
            .filter(|metric| self.chart_filter(metric))
            .collect()
    }

    fn matches_search(&self, metric: &MetricState) -> bool {
        let query = self.search_query.trim();
        if query.is_empty() {
            return true;
        }

        metric_matches_query(metric, query)
    }

    fn selected_metric(&self) -> Option<&MetricState> {
        let selected = match self.focus {
            Focus::Numeric => self.numeric_selected,
            Focus::Charts => self.chart_selected,
        };

        match self.focus {
            Focus::Numeric => self.numeric_metrics(),
            Focus::Charts => self.chart_metrics(),
        }
        .into_iter()
        .nth(selected)
    }

    fn select_metric_by_name(&mut self, name: &str) {
        self.focus = match self.configured_view(name) {
            MetricView::Numeric => Focus::Numeric,
            MetricView::Chart => Focus::Charts,
        };

        match self.focus {
            Focus::Numeric => {
                if let Some(index) = metric_index_by_name(&self.numeric_metrics(), name) {
                    self.numeric_selected = index;
                }
            }
            Focus::Charts => {
                if let Some(index) = metric_index_by_name(&self.chart_metrics(), name) {
                    self.chart_selected = index;
                }
            }
        }

        self.sync_viewports();
    }

    fn add_candidates(&self) -> Vec<&SeenMetric> {
        let mut metrics = self.seen_metrics.values().collect::<Vec<_>>();
        metrics.sort_by(|a, b| a.name.cmp(&b.name));

        let query = self.add_query.trim();
        if query.is_empty() {
            return metrics;
        }

        metrics
            .into_iter()
            .filter(|metric| seen_metric_matches_query(metric, query))
            .collect()
    }
}

struct SeenMetric {
    name: String,
    kind: MetricKind,
    samples: u64,
    last_seen: Instant,
    series: HashSet<String>,
    example_labels: String,
}

impl SeenMetric {
    fn new(name: String) -> Self {
        Self {
            name,
            kind: MetricKind::Unknown("?".to_string()),
            samples: 0,
            last_seen: Instant::now(),
            series: HashSet::new(),
            example_labels: String::new(),
        }
    }

    fn push(&mut self, sample: &Sample) {
        self.kind = sample.kind.clone();
        self.samples += 1;
        self.last_seen = sample.received_at;
        self.series.insert(series_key(&sample.name, &sample.tags));

        let labels = labels_text(&sample.tags);
        if !labels.is_empty() {
            self.example_labels = labels;
        }
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
        let rate_per_sec =
            if matches!(&sample.kind, MetricKind::Counter) && !self.samples.is_empty() {
                sample
                    .received_at
                    .checked_duration_since(self.last_seen)
                    .map(|duration| duration.as_secs_f64())
                    .filter(|seconds| *seconds > 0.0)
                    .map(|seconds| sample.value / seconds)
            } else {
                None
            };

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
            latest: sample.value,
            total: self.total,
            rate_per_sec,
        });

        while self.samples.len() > history_points {
            self.samples.pop_front();
        }
    }
}

#[derive(Clone, Copy)]
struct Point {
    latest: f64,
    total: f64,
    rate_per_sec: Option<f64>,
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

    render_footer(frame, footer, app);

    if app.details_open {
        render_details(frame, app);
    }

    if app.add_open {
        render_add(frame, app);
    }
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    frame.render_widget(Clear, area);

    let mut spans = vec![
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
        Span::styled(" / ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" search  "),
        Span::styled(" a ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" add  "),
        Span::styled(" C-x s ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" save  "),
        Span::styled(" +/- ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(format!(
            " chart height {}  listening on {}",
            app.chart_height, app.config.listen
        )),
    ];

    if app.ctrl_x_armed {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            " C-x ",
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ));
        spans.push(Span::raw(" s save  Esc cancel"));
    } else if let Some(message) = &app.status_message {
        spans.push(Span::raw(format!("  {message}")));
    }

    if app.search_active || !app.search_query.is_empty() {
        spans.push(Span::raw("  "));
        spans.extend(search_footer_spans(app));
    }

    frame.render_widget(Line::from(spans), area);
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
    let display = app.display_mode(&metric.name);
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
            Span::raw(effective_metric_kind_name(app, metric)),
            Span::styled("  view ", detail_label_style()),
            Span::raw(match view {
                MetricView::Chart => "chart",
                MetricView::Numeric => "numeric",
            }),
            Span::styled("  display ", detail_label_style()),
            Span::raw(metric_display_name(display)),
            Span::styled("  unit ", detail_label_style()),
            Span::raw(if unit.is_empty() { "none" } else { unit }),
            Span::styled("  configured ", detail_label_style()),
            Span::raw(if configured { "yes" } else { "auto" }),
        ]),
        Line::from(vec![
            Span::styled("value ", detail_label_style()),
            Span::raw(format_display_value(metric, display, unit)),
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
            Span::styled(" d ", Style::default().fg(Color::Black).bg(Color::Gray)),
            Span::raw(" remove metric  "),
            Span::styled(
                " Enter/Esc ",
                Style::default().fg(Color::Black).bg(Color::Gray),
            ),
            Span::raw(" close"),
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

fn render_add(frame: &mut Frame, app: &App) {
    let area = bottom_third(frame.area());
    frame.render_widget(Clear, area);

    let [form_area, list_area, help_area] = area.layout(&Layout::vertical([
        Constraint::Length(5),
        Constraint::Fill(1),
        Constraint::Length(1),
    ]));

    let metric_input = if app.add_query.is_empty() {
        "<type to filter or enter a new metric>"
    } else {
        app.add_query.as_str()
    };
    let unit_input = if app.add_unit.is_empty() {
        "<none>"
    } else {
        app.add_unit.as_str()
    };
    let active_field = add_field_name(app.add_field);

    let form = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("metric ", detail_label_style()),
            add_field_value_span(app, AddField::Metric, metric_input),
        ]),
        Line::from(vec![
            Span::styled("view ", detail_label_style()),
            add_field_value_span(app, AddField::View, metric_view_name(app.add_view)),
            Span::styled("  kind ", detail_label_style()),
            add_field_value_span(app, AddField::Kind, metric_kind_config_name(app.add_kind)),
            Span::styled("  display ", detail_label_style()),
            add_field_value_span(app, AddField::Display, metric_display_name(app.add_display)),
        ]),
        Line::from(vec![
            Span::styled("unit ", detail_label_style()),
            add_field_value_span(app, AddField::Unit, unit_input),
            Span::styled("  editing ", detail_label_style()),
            Span::raw(active_field),
        ]),
    ])
    .block(
        Block::bordered()
            .title(" Add Metric ")
            .border_style(selected_style(true)),
    )
    .wrap(Wrap { trim: true });
    frame.render_widget(form, form_area);

    let candidates = app.add_candidates();
    let visible_rows = usize::from(list_area.height.saturating_sub(3)).max(1);
    let selected = app.add_selected.min(candidates.len().saturating_sub(1));
    let start = selected.saturating_sub(visible_rows.saturating_sub(1));

    let rows = candidates
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
        .map(|(index, metric)| {
            let row = Row::new(vec![
                Cell::from(metric.name.clone()),
                Cell::from(metric.kind.to_string()),
                Cell::from(metric.series.len().to_string()),
                Cell::from(metric.samples.to_string()),
                Cell::from(format!("{:.1}s", metric.last_seen.elapsed().as_secs_f64())),
                Cell::from(if app.metric_config.contains_key(&metric.name) {
                    "yes"
                } else {
                    "no"
                }),
                Cell::from(metric.example_labels.clone()),
            ]);

            if index == selected {
                row.style(selected_style(true))
            } else {
                row
            }
        });

    let table = Table::new(
        rows,
        [
            Constraint::Fill(2),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Fill(1),
        ],
    )
    .header(
        Row::new([
            "metric",
            "kind",
            "series",
            "samples",
            "age",
            "configured",
            "example labels",
        ])
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(Block::bordered().title(format!(
        " Discovered {}/{} ",
        candidates.len(),
        app.seen_metrics.len()
    )));
    frame.render_widget(table, list_area);

    let help = Line::from(vec![
        Span::styled(" Enter ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" add  "),
        Span::styled(" Esc ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" cancel  "),
        Span::styled(" Tab ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" field  "),
        Span::styled(
            " Left/Right ",
            Style::default().fg(Color::Black).bg(Color::Gray),
        ),
        Span::raw(" change  "),
        Span::styled(
            " Ctrl-V ",
            Style::default().fg(Color::Black).bg(Color::Gray),
        ),
        Span::raw(" view  "),
        Span::styled(
            " Ctrl-K ",
            Style::default().fg(Color::Black).bg(Color::Gray),
        ),
        Span::raw(" kind  "),
        Span::styled(
            " Ctrl-D ",
            Style::default().fg(Color::Black).bg(Color::Gray),
        ),
        Span::raw(" display  "),
        Span::styled(
            " Ctrl-W ",
            Style::default().fg(Color::Black).bg(Color::Gray),
        ),
        Span::raw(" clear text"),
    ]);
    frame.render_widget(help, help_area);
}

fn search_footer_spans(app: &App) -> Vec<Span<'static>> {
    let marker = if app.search_active { "/" } else { "filter" };
    let text = if app.search_active {
        format!("/{:<width$}", app.search_query, width = 20)
    } else {
        format!("filter: {}", app.search_query)
    };

    let mut spans = vec![
        Span::styled(
            format!(" {marker} "),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ),
        Span::raw(format!(" {text} ")),
        Span::styled(" Enter ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" keep  "),
        Span::styled(" Esc ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" stop"),
    ];

    if app.search_active {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            " Ctrl-W ",
            Style::default().fg(Color::Black).bg(Color::Gray),
        ));
        spans.push(Span::raw(" clear input"));
    } else {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            " c ",
            Style::default().fg(Color::Black).bg(Color::Gray),
        ));
        spans.push(Span::raw(" clear filter"));
    }

    spans
}

fn render_numeric(frame: &mut Frame, area: Rect, app: &mut App) {
    let visible_rows = usize::from(area.height.saturating_sub(3)).max(1);
    app.numeric_page = visible_rows;
    app.sync_viewports();

    let metrics = app.numeric_metrics();

    let start = app.numeric_scroll.min(metrics.len().saturating_sub(1));
    let end = (start + visible_rows).min(metrics.len());

    let rows = metrics
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
        .map(|(index, metric)| {
            let unit = app.unit(&metric.name);
            let display = app.display_mode(&metric.name);
            let display_name = grouped_metric_name(&metrics, index);
            let row = Row::new(vec![
                Cell::from(display_name),
                Cell::from(metric_kind_display(app, metric)),
                Cell::from(format_display_value(metric, display, unit)),
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
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Length(7),
        ],
    )
    .header(
        Row::new(["metric", "kind", "value", "total", "age"]).style(
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

    let charted = app.chart_metrics();

    if charted.is_empty() {
        let message = if app.search_query.trim().is_empty() {
            "No chart metrics configured yet."
        } else {
            "No chart metrics match the current search."
        };
        frame.render_widget(
            Paragraph::new(message)
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
            display: app.display_mode(&metric.name),
            kind_label: effective_metric_kind_name(app, metric),
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
    display: MetricDisplay,
    kind_label: String,
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
        display,
        kind_label,
        history_points,
        pane_title,
        selected,
        focused,
    } = args;

    let history_points = history_points.max(2);
    let data = metric
        .samples
        .iter()
        .enumerate()
        .filter_map(|(index, point)| {
            point_display_value(point, display).map(|value| (index as f64 + 0.5, value))
        })
        .collect::<Vec<_>>();

    let y_bounds = if data.is_empty() {
        [0.0, 1.0]
    } else {
        let min_y = data
            .iter()
            .map(|(_, value)| *value)
            .fold(f64::INFINITY, f64::min);
        let max_y = data
            .iter()
            .map(|(_, value)| *value)
            .fold(f64::NEG_INFINITY, f64::max);
        let y_pad = ((max_y - min_y) * 0.1).max(1.0);
        [min_y - y_pad, max_y + y_pad]
    };
    let x_bounds = [0.0, history_points as f64];

    let datasets = if data.is_empty() {
        Vec::new()
    } else {
        vec![
            Dataset::default()
                .name(metric_chart_title(metric))
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(if selected { Color::Yellow } else { Color::Cyan })
                .data(&data),
        ]
    };

    let y_title = chart_axis_title(&kind_label, display, unit);

    let title = match pane_title {
        Some(pane_title) => format!("{pane_title}  {}", metric_chart_title(metric)),
        None => format!(" {} ", metric_chart_title(metric)),
    };

    let chart = Chart::new(datasets)
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

fn metric_matches_query(metric: &MetricState, query: &str) -> bool {
    let haystack = format!("{} {}", metric.name, metric.labels).to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|term| haystack.contains(term))
}

fn seen_metric_matches_query(metric: &SeenMetric, query: &str) -> bool {
    let haystack = format!("{} {}", metric.name, metric.example_labels).to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|term| haystack.contains(term))
}

fn metric_view_name(view: MetricView) -> &'static str {
    match view {
        MetricView::Chart => "chart",
        MetricView::Numeric => "numeric",
    }
}

fn metric_kind_config_name(kind: MetricKindConfig) -> &'static str {
    match kind {
        MetricKindConfig::Auto => "auto",
        MetricKindConfig::Counter => "counter",
        MetricKindConfig::Gauge => "gauge",
        MetricKindConfig::Histogram => "histogram",
        MetricKindConfig::Timer => "timer",
        MetricKindConfig::Distribution => "distribution",
        MetricKindConfig::Set => "set",
    }
}

fn metric_display_name(display: MetricDisplay) -> &'static str {
    match display {
        MetricDisplay::Default => "default",
        MetricDisplay::Latest => "latest",
        MetricDisplay::Total => "total",
        MetricDisplay::Rate => "rate",
    }
}

fn add_field_name(field: AddField) -> &'static str {
    match field {
        AddField::Metric => "metric",
        AddField::View => "view",
        AddField::Kind => "kind",
        AddField::Display => "display",
        AddField::Unit => "unit",
    }
}

fn add_field_value_span(app: &App, field: AddField, value: &str) -> Span<'static> {
    if app.add_field == field {
        Span::styled(value.to_string(), selected_style(true))
    } else {
        Span::raw(value.to_string())
    }
}

fn next_metric_view(view: MetricView, direction: isize) -> MetricView {
    let views = [MetricView::Numeric, MetricView::Chart];
    views[next_index(
        views
            .iter()
            .position(|candidate| *candidate == view)
            .unwrap_or(0),
        views.len(),
        direction,
    )]
}

fn next_metric_kind_config(kind: MetricKindConfig, direction: isize) -> MetricKindConfig {
    let kinds = [
        MetricKindConfig::Auto,
        MetricKindConfig::Counter,
        MetricKindConfig::Gauge,
        MetricKindConfig::Histogram,
        MetricKindConfig::Timer,
        MetricKindConfig::Distribution,
        MetricKindConfig::Set,
    ];
    kinds[next_index(
        kinds
            .iter()
            .position(|candidate| *candidate == kind)
            .unwrap_or(0),
        kinds.len(),
        direction,
    )]
}

fn next_metric_display(display: MetricDisplay, direction: isize) -> MetricDisplay {
    let displays = [
        MetricDisplay::Default,
        MetricDisplay::Latest,
        MetricDisplay::Total,
        MetricDisplay::Rate,
    ];
    displays[next_index(
        displays
            .iter()
            .position(|candidate| *candidate == display)
            .unwrap_or(0),
        displays.len(),
        direction,
    )]
}

fn next_index(current: usize, len: usize, direction: isize) -> usize {
    if len == 0 {
        return 0;
    }

    if direction.is_negative() {
        current.checked_sub(1).unwrap_or(len - 1)
    } else {
        (current + 1) % len
    }
}

fn metric_kind_from_config(kind: MetricKindConfig) -> Option<MetricKind> {
    match kind {
        MetricKindConfig::Auto => None,
        MetricKindConfig::Counter => Some(MetricKind::Counter),
        MetricKindConfig::Gauge => Some(MetricKind::Gauge),
        MetricKindConfig::Histogram => Some(MetricKind::Histogram),
        MetricKindConfig::Timer => Some(MetricKind::Timer),
        MetricKindConfig::Distribution => Some(MetricKind::Distribution),
        MetricKindConfig::Set => Some(MetricKind::Set),
    }
}

fn metric_kind_config_from_metric_kind(kind: &MetricKind) -> MetricKindConfig {
    match kind {
        MetricKind::Counter => MetricKindConfig::Counter,
        MetricKind::Gauge => MetricKindConfig::Gauge,
        MetricKind::Histogram => MetricKindConfig::Histogram,
        MetricKind::Timer => MetricKindConfig::Timer,
        MetricKind::Distribution => MetricKindConfig::Distribution,
        MetricKind::Set => MetricKindConfig::Set,
        MetricKind::Unknown(_) => MetricKindConfig::Auto,
    }
}

fn effective_metric_kind_name(app: &App, metric: &MetricState) -> String {
    if matches!(metric.kind, MetricKind::Unknown(_))
        && app.configured_kind(&metric.name) != MetricKindConfig::Auto
    {
        metric_kind_config_name(app.configured_kind(&metric.name)).to_string()
    } else {
        metric.kind.to_string()
    }
}

fn metric_kind_display(app: &App, metric: &MetricState) -> String {
    let kind = effective_metric_kind_name(app, metric);
    let display = app.display_mode(&metric.name);
    if display == MetricDisplay::Default {
        kind
    } else {
        format!("{kind}/{}", metric_display_name(display))
    }
}

fn point_display_value(point: &Point, display: MetricDisplay) -> Option<f64> {
    match display {
        MetricDisplay::Default | MetricDisplay::Latest => Some(point.latest),
        MetricDisplay::Total => Some(point.total),
        MetricDisplay::Rate => point.rate_per_sec,
    }
}

fn format_display_value(metric: &MetricState, display: MetricDisplay, unit: &str) -> String {
    metric
        .samples
        .back()
        .and_then(|point| point_display_value(point, display))
        .map_or_else(|| "n/a".to_string(), |value| format_value(value, unit))
}

fn chart_axis_title(kind: &str, display: MetricDisplay, unit: &str) -> String {
    let display = match display {
        MetricDisplay::Default => "latest",
        display => metric_display_name(display),
    };
    if unit.is_empty() {
        format!("{kind} {display}")
    } else if matches!(display, "rate") {
        format!("{kind} {display} ({unit}/s)")
    } else {
        format!("{kind} {display} ({unit})")
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
    let first = samples.next()?.latest;
    let mut min = first;
    let mut max = first;
    let mut sum = first;
    let mut count = 1usize;

    for point in samples {
        min = min.min(point.latest);
        max = max.max(point.latest);
        sum += point.latest;
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
    let latest = samples.next()?.latest;
    let previous = samples.next()?.latest;
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

fn is_ctrl_x(code: KeyCode, modifiers: KeyModifiers) -> bool {
    code == KeyCode::Char('x') && modifiers.contains(KeyModifiers::CONTROL)
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

fn metric_index_by_name(metrics: &[&MetricState], name: &str) -> Option<usize> {
    metrics.iter().position(|metric| metric.name == name)
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
    use std::time::{SystemTime, UNIX_EPOCH};

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
        let mut app = App::new(Config::default(), PathBuf::from("test.toml"), rx);
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

    #[test]
    fn search_matches_metric_name_and_labels() {
        let metric = MetricState::new(
            "buffer_pool_dirty_wal_page_image_pages".to_string(),
            "[page_kind:leaf_index, service:boxter]".to_string(),
            8,
        );

        assert!(metric_matches_query(&metric, "dirty leaf"));
        assert!(metric_matches_query(&metric, "SERVICE:BOXTER"));
        assert!(!metric_matches_query(&metric, "resident"));
    }

    #[test]
    fn search_filters_metric_lists() {
        let (_tx, rx) = mpsc::channel();
        let mut app = App::new(Config::default(), PathBuf::from("test.toml"), rx);

        app.metrics.insert(
            "requests|service=api".to_string(),
            MetricState::new("requests".to_string(), "[service:api]".to_string(), 8),
        );
        app.metrics.insert(
            "requests|service=worker".to_string(),
            MetricState::new("requests".to_string(), "[service:worker]".to_string(), 8),
        );
        app.search_query = "worker".to_string();

        let metrics = app.numeric_metrics();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].labels, "[service:worker]");
    }

    #[test]
    fn unconfigured_metrics_are_seen_without_being_tracked() {
        let (tx, rx) = mpsc::channel();
        let config = Config {
            metrics: vec![MetricConfig {
                name: "tracked".to_string(),
                view: MetricView::Numeric,
                kind: MetricKindConfig::Auto,
                display: MetricDisplay::Default,
                unit: String::new(),
            }],
            ..Config::default()
        };
        let mut app = App::new(config, PathBuf::from("test.toml"), rx);

        tx.send(Sample {
            name: "untracked".to_string(),
            value: 1.0,
            kind: MetricKind::Counter,
            tags: vec![("service".to_string(), "api".to_string())],
            received_at: Instant::now(),
        })
        .unwrap();
        app.drain_samples();

        assert!(app.seen_metrics.contains_key("untracked"));
        assert!(
            app.metrics
                .values()
                .all(|metric| metric.name != "untracked")
        );
    }

    #[test]
    fn add_promotes_seen_metric_to_runtime_config() {
        let (_tx, rx) = mpsc::channel();
        let config = Config {
            metrics: vec![MetricConfig {
                name: "tracked".to_string(),
                view: MetricView::Numeric,
                kind: MetricKindConfig::Auto,
                display: MetricDisplay::Default,
                unit: String::new(),
            }],
            ..Config::default()
        };
        let mut app = App::new(config, PathBuf::from("test.toml"), rx);

        app.seen_metrics.insert(
            "untracked".to_string(),
            SeenMetric::new("untracked".to_string()),
        );
        app.add_query = "untracked".to_string();
        app.add_view = MetricView::Chart;
        app.add_kind = MetricKindConfig::Counter;
        app.add_display = MetricDisplay::Rate;
        app.add_unit = "rows".to_string();
        app.hidden_metrics.insert("untracked".to_string());

        app.add_selected_metric();

        let added = app.metric_config.get("untracked").unwrap();
        assert_eq!(added.view, MetricView::Chart);
        assert_eq!(added.kind, MetricKindConfig::Counter);
        assert_eq!(added.display, MetricDisplay::Rate);
        assert_eq!(added.unit, "rows");
        assert!(app.should_track("untracked"));
        assert!(!app.hidden_metrics.contains("untracked"));
        assert_eq!(app.focus, Focus::Charts);
        assert_eq!(
            app.selected_metric().map(|metric| metric.name.as_str()),
            Some("untracked")
        );
    }

    #[test]
    fn add_selects_new_numeric_metric() {
        let (_tx, rx) = mpsc::channel();
        let mut app = App::new(Config::default(), PathBuf::from("test.toml"), rx);
        app.add_query = "manual.numeric".to_string();
        app.add_view = MetricView::Numeric;

        app.add_selected_metric();

        assert_eq!(app.focus, Focus::Numeric);
        assert_eq!(
            app.selected_metric().map(|metric| metric.name.as_str()),
            Some("manual.numeric")
        );
    }

    #[test]
    fn ctrl_x_ctrl_s_saves_config() {
        let (_tx, rx) = mpsc::channel();
        let path = temp_config_path("ctrl-x-ctrl-s");
        let mut app = App::new(Config::default(), path.clone(), rx);
        app.add_query = "manual.save".to_string();
        app.add_selected_metric();

        app.handle_key(KeyCode::Char('x'), KeyModifiers::CONTROL);
        app.handle_key(KeyCode::Char('s'), KeyModifiers::CONTROL);

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("manual.save"));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn ctrl_x_ctrl_c_quits() {
        let (_tx, rx) = mpsc::channel();
        let mut app = App::new(Config::default(), PathBuf::from("test.toml"), rx);

        assert!(!app.handle_key(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    }

    #[test]
    fn configured_metric_placeholder_uses_configured_kind() {
        let (_tx, rx) = mpsc::channel();
        let config = Config {
            metrics: vec![MetricConfig {
                name: "manual.counter".to_string(),
                view: MetricView::Chart,
                kind: MetricKindConfig::Counter,
                display: MetricDisplay::Rate,
                unit: "req".to_string(),
            }],
            ..Config::default()
        };
        let app = App::new(config, PathBuf::from("test.toml"), rx);

        let metric = app.metrics.values().next().unwrap();
        assert_eq!(metric.name, "manual.counter");
        assert_eq!(metric.kind, MetricKind::Counter);
        assert_eq!(app.display_mode("manual.counter"), MetricDisplay::Rate);
    }

    #[test]
    fn add_pane_control_keys_cycle_kind_and_display() {
        let (_tx, rx) = mpsc::channel();
        let mut app = App::new(Config::default(), PathBuf::from("test.toml"), rx);
        app.open_add();

        app.handle_add_key(KeyCode::Char('v'), KeyModifiers::CONTROL);
        app.handle_add_key(KeyCode::Char('k'), KeyModifiers::CONTROL);
        app.handle_add_key(KeyCode::Char('d'), KeyModifiers::CONTROL);

        assert_eq!(app.add_view, MetricView::Chart);
        assert_eq!(app.add_kind, MetricKindConfig::Counter);
        assert_eq!(app.add_display, MetricDisplay::Latest);
    }

    #[test]
    fn counter_rate_display_uses_sample_delta_over_time() {
        let mut metric = MetricState::new("requests".to_string(), String::new(), 8);
        let start = Instant::now();

        metric.push(
            Sample {
                name: "requests".to_string(),
                value: 10.0,
                kind: MetricKind::Counter,
                tags: Vec::new(),
                received_at: start,
            },
            8,
        );
        metric.push(
            Sample {
                name: "requests".to_string(),
                value: 20.0,
                kind: MetricKind::Counter,
                tags: Vec::new(),
                received_at: start + Duration::from_secs(4),
            },
            8,
        );

        assert_eq!(
            point_display_value(metric.samples.back().unwrap(), MetricDisplay::Rate),
            Some(5.0)
        );
    }

    #[test]
    fn remove_metric_drops_config_and_all_labelled_series() {
        let (_tx, rx) = mpsc::channel();
        let config = Config {
            metrics: vec![
                MetricConfig {
                    name: "tracked".to_string(),
                    view: MetricView::Numeric,
                    kind: MetricKindConfig::Auto,
                    display: MetricDisplay::Default,
                    unit: String::new(),
                },
                MetricConfig {
                    name: "other".to_string(),
                    view: MetricView::Numeric,
                    kind: MetricKindConfig::Auto,
                    display: MetricDisplay::Default,
                    unit: String::new(),
                },
            ],
            ..Config::default()
        };
        let mut app = App::new(config, PathBuf::from("test.toml"), rx);

        app.metrics.insert(
            series_key("tracked", &[("service".to_string(), "api".to_string())]),
            MetricState::new("tracked".to_string(), "[service:api]".to_string(), 8),
        );
        app.metrics.insert(
            series_key("tracked", &[("service".to_string(), "worker".to_string())]),
            MetricState::new("tracked".to_string(), "[service:worker]".to_string(), 8),
        );
        app.metrics.insert(
            series_key("other", &[]),
            MetricState::new("other".to_string(), String::new(), 8),
        );

        app.remove_metric("tracked");

        assert!(!app.metric_config.contains_key("tracked"));
        assert!(app.hidden_metrics.contains("tracked"));
        assert!(!app.should_track("tracked"));
        assert!(app.should_track("other"));
        assert_eq!(app.metrics.len(), 1);
        assert_eq!(
            app.metrics
                .values()
                .next()
                .map(|metric| metric.name.as_str()),
            Some("other")
        );
    }

    #[test]
    fn remove_metric_hides_auto_tracked_metric() {
        let (_tx, rx) = mpsc::channel();
        let mut app = App::new(Config::default(), PathBuf::from("test.toml"), rx);

        app.metrics.insert(
            series_key("auto", &[]),
            MetricState::new("auto".to_string(), String::new(), 8),
        );

        assert!(app.should_track("auto"));

        app.remove_metric("auto");

        assert!(!app.should_track("auto"));
        assert!(app.hidden_metrics.contains("auto"));
        assert!(app.metrics.is_empty());
    }

    #[test]
    fn config_for_save_materializes_auto_tracked_metrics_without_hidden() {
        let (_tx, rx) = mpsc::channel();
        let mut app = App::new(Config::default(), PathBuf::from("test.toml"), rx);

        app.metrics.insert(
            series_key("auto", &[]),
            MetricState::new("auto".to_string(), String::new(), 8),
        );
        app.metrics.insert(
            series_key("removed", &[]),
            MetricState::new("removed".to_string(), String::new(), 8),
        );
        app.hidden_metrics.insert("removed".to_string());

        let saved = app.config_for_save();

        assert_eq!(saved.metrics.len(), 1);
        assert_eq!(saved.metrics[0].name, "auto");
    }

    #[test]
    fn write_config_saves_runtime_metric_config() {
        let (_tx, rx) = mpsc::channel();
        let path = temp_config_path("write-config");
        let config = Config {
            listen: "127.0.0.1:18125".to_string(),
            history_points: 80,
            redraw_millis: 125,
            metrics: vec![MetricConfig {
                name: "original".to_string(),
                view: MetricView::Numeric,
                kind: MetricKindConfig::Auto,
                display: MetricDisplay::Default,
                unit: String::new(),
            }],
        };
        let mut app = App::new(config, path.clone(), rx);

        app.metric_config.insert(
            "added".to_string(),
            MetricConfig {
                name: "added".to_string(),
                view: MetricView::Chart,
                kind: MetricKindConfig::Counter,
                display: MetricDisplay::Rate,
                unit: "rows".to_string(),
            },
        );
        app.remove_metric("original");

        app.write_config().unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let saved: Config = toml::from_str(&contents).unwrap();
        assert_eq!(saved.listen, "127.0.0.1:18125");
        assert_eq!(saved.history_points, 80);
        assert_eq!(saved.redraw_millis, 125);
        assert_eq!(saved.metrics.len(), 1);
        assert_eq!(saved.metrics[0].name, "added");
        assert_eq!(saved.metrics[0].view, MetricView::Chart);
        assert_eq!(saved.metrics[0].kind, MetricKindConfig::Counter);
        assert_eq!(saved.metrics[0].display, MetricDisplay::Rate);
        assert_eq!(saved.metrics[0].unit, "rows");

        fs::remove_file(path).unwrap();
    }

    fn temp_config_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "spinal-tap-{name}-{}-{nanos}.toml",
            std::process::id()
        ))
    }
}
