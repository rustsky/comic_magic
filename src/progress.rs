//! Terminal progress bars (drawn on stderr, hidden automatically when it isn't a terminal).

use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

const BAR: &str = "{prefix:>10.cyan.bold} [{bar:30.green/dim}] {pos}/{len} {msg} ({elapsed}, eta {eta})";
const SPIN: &str = "{prefix:>10.cyan.bold} {spinner} {msg} ({elapsed})";

pub fn spinner(prefix: &'static str, msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner()
        .with_style(ProgressStyle::with_template(SPIN).unwrap())
        .with_prefix(prefix)
        .with_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

pub fn bar(prefix: &'static str, len: usize, msg: &str) -> ProgressBar {
    ProgressBar::new(len as u64)
        .with_style(ProgressStyle::with_template(BAR).unwrap().progress_chars("=> "))
        .with_prefix(prefix)
        .with_message(msg.to_string())
}

/// Turn a spinner into a bar once the total is known.
pub fn to_bar(pb: &ProgressBar, len: usize) {
    pb.set_length(len as u64);
    pb.set_style(ProgressStyle::with_template(BAR).unwrap().progress_chars("=> "));
}
