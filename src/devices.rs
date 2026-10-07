//! Kindle screen profiles.

use anyhow::{Context, Result, bail};
use std::io::{self, BufRead, IsTerminal, Write};

#[derive(Clone, Debug)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub ppi: u32,
    pub color: bool,
}

const PROFILES: &[(&str, &str, u32, u32, u32, bool)] = &[
    ("kindle-basic", "Kindle basic 4/5/7/8/10th gen, Kindle Touch (6\", 167 ppi)", 600, 800, 167, false),
    ("kindle-11", "Kindle 11th gen 2022 / Kindle 2024 (6\", 300 ppi)", 1072, 1448, 300, false),
    ("paperwhite-1", "Paperwhite 1/2 (2012-2013, 212 ppi)", 758, 1024, 212, false),
    ("paperwhite-3", "Paperwhite 3/4 (2015-2018), Voyage, Oasis 1 (300 ppi)", 1072, 1448, 300, false),
    ("paperwhite-5", "Paperwhite 5 / Signature Edition (2021, 6.8\")", 1236, 1648, 300, false),
    ("paperwhite-6", "Paperwhite 12th gen (2024, 7\")", 1264, 1680, 300, false),
    ("oasis", "Oasis 2/3 (2017-2019, 7\")", 1264, 1680, 300, false),
    ("colorsoft", "Kindle Colorsoft (2024, 7\", color)", 1264, 1680, 300, true),
    ("scribe", "Kindle Scribe (10.2\")", 1860, 2480, 300, false),
    ("dx", "Kindle DX (9.7\")", 824, 1200, 150, false),
];

pub fn all() -> Vec<Device> {
    PROFILES
        .iter()
        .map(|&(id, name, width, height, ppi, color)| Device {
            id: id.into(),
            name: name.into(),
            width,
            height,
            ppi,
            color,
        })
        .collect()
}

pub fn find(id: &str) -> Option<Device> {
    let id = id.to_ascii_lowercase();
    all().into_iter().find(|d| d.id == id)
}

pub fn print_list() {
    for d in all() {
        println!(
            "  {:<14} {:>4}x{:<4} {:>3} ppi  {}",
            d.id, d.width, d.height, d.ppi, d.name
        );
    }
}

/// Ask the user which Kindle they own.
pub fn prompt() -> Result<Device> {
    if !io::stdin().is_terminal() {
        bail!("no --device given and stdin is not a terminal; pass --device (see --list-devices)");
    }
    let devices = all();
    println!("Which Kindle will you read on?");
    for (i, d) in devices.iter().enumerate() {
        println!("  {:>2}) {}  [{}x{}]", i + 1, d.name, d.width, d.height);
    }
    let stdin = io::stdin();
    loop {
        print!("Select 1-{}: ", devices.len());
        io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).context("reading selection")? == 0 {
            bail!("no device selected");
        }
        let line = line.trim();
        if let Ok(n) = line.parse::<usize>()
            && (1..=devices.len()).contains(&n)
        {
            return Ok(devices[n - 1].clone());
        }
        if let Some(d) = find(line) {
            return Ok(d);
        }
        println!("Invalid choice '{line}'.");
    }
}
