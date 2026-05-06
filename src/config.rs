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

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_history_points")]
    pub history_points: usize,
    #[serde(default = "default_redraw_millis")]
    pub redraw_millis: u64,
    #[serde(default)]
    pub metrics: Vec<MetricConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetricConfig {
    pub name: String,
    #[serde(default)]
    pub view: MetricView,
    #[serde(default)]
    pub kind: MetricKindConfig,
    #[serde(default)]
    pub display: MetricDisplay,
    #[serde(default)]
    pub unit: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetricView {
    Chart,
    #[default]
    Numeric,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetricKindConfig {
    #[default]
    Auto,
    Counter,
    Gauge,
    Histogram,
    Timer,
    Distribution,
    Set,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetricDisplay {
    #[default]
    Default,
    Latest,
    Total,
    Rate,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self, Box<dyn std::error::Error>> {
        let path = path.unwrap_or_else(|| Path::new("spinal-tap.toml"));
        match fs::read_to_string(path) {
            Ok(contents) => Ok(toml::from_str(&contents)?),
            Err(err)
                if err.kind() == io::ErrorKind::NotFound
                    && path == Path::new("spinal-tap.toml") =>
            {
                Ok(Self::default())
            }
            Err(err) => Err(Box::new(err)),
        }
    }

    pub fn redraw_interval(&self) -> Duration {
        Duration::from_millis(self.redraw_millis.max(50))
    }

    pub fn history_points(&self) -> usize {
        self.history_points.max(2)
    }

    pub fn metric_map(&self) -> HashMap<String, MetricConfig> {
        self.metrics
            .iter()
            .map(|metric| (metric.name.clone(), metric.clone()))
            .collect()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            history_points: default_history_points(),
            redraw_millis: default_redraw_millis(),
            metrics: Vec::new(),
        }
    }
}

fn default_listen() -> String {
    "127.0.0.1:8125".to_string()
}

fn default_history_points() -> usize {
    120
}

fn default_redraw_millis() -> u64 {
    250
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_kind_and_display_default_for_existing_configs() {
        let config: Config = toml::from_str(
            r#"
            [[metrics]]
            name = "requests"
            view = "chart"
            unit = "req"
            "#,
        )
        .unwrap();

        assert_eq!(config.metrics[0].kind, MetricKindConfig::Auto);
        assert_eq!(config.metrics[0].display, MetricDisplay::Default);
    }
}
