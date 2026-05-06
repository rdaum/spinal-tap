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

use std::net::SocketAddr;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Instant;

use compio::net::UdpSocket;

use crate::dogstatsd::{Sample, parse_datagram};

pub fn spawn_ingest(listen: String, tx: Sender<Sample>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        if let Err(err) = run_ingest(&listen, tx) {
            eprintln!("spinal-tap ingest failed: {err}");
        }
    })
}

fn run_ingest(
    listen: &str,
    tx: Sender<Sample>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = listen.parse()?;
    let runtime = compio::runtime::Runtime::new()?;

    runtime.block_on(async move {
        let socket = UdpSocket::bind(addr).await?;
        let mut buffer = Vec::with_capacity(8192);

        loop {
            let (result, returned_buffer) = socket.recv_from(buffer).await.into_parts();
            buffer = returned_buffer;

            let Ok((n, _peer)) = result else {
                continue;
            };

            let received_at = Instant::now();
            for sample in parse_datagram(&buffer[..n], received_at) {
                if tx.send(sample).is_err() {
                    return Ok::<_, std::io::Error>(());
                }
            }

            buffer.clear();
            if buffer.capacity() < 8192 {
                buffer.reserve(8192 - buffer.capacity());
            }
        }
    })?;

    Ok(())
}
