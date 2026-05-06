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

mod config;
mod dogstatsd;
mod ingest;
mod ui;

use std::path::PathBuf;
use std::sync::mpsc;

use config::Config;
use ingest::spawn_ingest;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = std::env::args_os().nth(1).map(PathBuf::from);
    let config = Config::load(config_path.as_deref())?;

    let (tx, rx) = mpsc::channel();
    let _ingest = spawn_ingest(config.listen.clone(), tx);

    ui::run(config, rx)?;
    Ok(())
}
