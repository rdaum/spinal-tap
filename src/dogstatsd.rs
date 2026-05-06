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

use std::fmt;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct Sample {
    pub name: String,
    pub value: f64,
    pub kind: MetricKind,
    pub tags: Vec<(String, String)>,
    pub received_at: Instant,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
    Timer,
    Distribution,
    Set,
    Unknown(String),
}

impl MetricKind {
    fn parse(raw: &str) -> Self {
        match raw {
            "c" => Self::Counter,
            "g" => Self::Gauge,
            "h" => Self::Histogram,
            "ms" => Self::Timer,
            "d" => Self::Distribution,
            "s" => Self::Set,
            other => Self::Unknown(other.to_string()),
        }
    }
}

impl fmt::Display for MetricKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Counter => f.write_str("counter"),
            Self::Gauge => f.write_str("gauge"),
            Self::Histogram => f.write_str("histogram"),
            Self::Timer => f.write_str("timer"),
            Self::Distribution => f.write_str("distribution"),
            Self::Set => f.write_str("set"),
            Self::Unknown(kind) => f.write_str(kind),
        }
    }
}

pub fn parse_datagram(bytes: &[u8], received_at: Instant) -> Vec<Sample> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };

    text.lines()
        .filter_map(|line| parse_line(line.trim(), received_at))
        .collect()
}

fn parse_line(line: &str, received_at: Instant) -> Option<Sample> {
    if line.is_empty() {
        return None;
    }

    let (name, rest) = line.split_once(':')?;
    let mut fields = rest.split('|');
    let value = fields.next()?.parse::<f64>().ok()?;
    let kind = MetricKind::parse(fields.next()?);
    let mut tags = Vec::new();

    for field in fields {
        if let Some(raw_tags) = field.strip_prefix('#') {
            for tag in raw_tags.split(',').filter(|tag| !tag.is_empty()) {
                let (key, value) = tag.split_once(':').unwrap_or((tag, ""));
                tags.push((key.to_string(), value.to_string()));
            }
        }
    }

    Some(Sample {
        name: name.to_string(),
        value,
        kind,
        tags,
        received_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_newline_delimited_metrics() {
        let samples = parse_datagram(
            b"requests:3|c|#service:api\nqueue.depth:7|g\nlatency:12.5|ms",
            Instant::now(),
        );

        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0].name, "requests");
        assert_eq!(samples[0].value, 3.0);
        assert_eq!(samples[0].kind, MetricKind::Counter);
        assert_eq!(
            samples[0].tags,
            vec![("service".to_string(), "api".to_string())]
        );
        assert_eq!(samples[2].kind, MetricKind::Timer);
    }

    #[test]
    fn skips_invalid_lines() {
        let samples = parse_datagram(b"nope\nok:1|g\nalso_nope:wat|c", Instant::now());

        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].name, "ok");
    }
}
