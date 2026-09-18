//! A live line for a long-running fetch, so a slow mirror does not look like
//! a hang.
//!
//! Two renderings: on a real terminal, one line redrawn in place; piped,
//! logged, or run under CI, one line per file finished instead — redrawing
//! there would leave a stream of `\r` that nothing ever collapses back down.

use crate::color::{self, Stream};
use kiln_alpm::DownloadEvent;
use std::collections::BTreeMap;
use std::io::Write;
use std::time::{Duration, Instant};

pub struct FetchProgress {
    tty: bool,
    /// In-flight downloads: filename → (bytes so far, total once known).
    /// `total` is `0` until the server has sent one — not every mirror
    /// sends a `Content-Length` up front.
    active: BTreeMap<String, (i64, i64)>,
    done: usize,
    failed: usize,
    /// Bytes and their (best-known) totals for files that have already
    /// finished, folded out of `active` so the running total does not lose
    /// them.
    downloaded_done: i64,
    known_total_done: i64,
    /// Bytes per second, exponentially smoothed so one slow chunk does not
    /// make the number visibly jump.
    speed: f64,
    last_sample: (Instant, i64),
    last_render: Instant,
}

impl FetchProgress {
    pub fn new() -> FetchProgress {
        let now = Instant::now();
        FetchProgress {
            tty: Stream::Out.is_tty(),
            active: BTreeMap::new(),
            done: 0,
            failed: 0,
            downloaded_done: 0,
            known_total_done: 0,
            speed: 0.0,
            last_sample: (now, 0),
            last_render: now,
        }
    }

    pub fn event(&mut self, event: DownloadEvent) {
        match event {
            DownloadEvent::Started { file } => {
                self.active.entry(file).or_insert((0, 0));
            }
            DownloadEvent::Progress {
                file,
                downloaded,
                total,
            } => {
                self.active.insert(file, (downloaded, total));
            }
            DownloadEvent::Finished { file, ok } => {
                if let Some((downloaded, total)) = self.active.remove(&file) {
                    self.downloaded_done += downloaded;
                    self.known_total_done += total.max(downloaded);
                }
                if ok {
                    self.done += 1;
                } else {
                    self.failed += 1;
                }
                if !self.tty {
                    let tag = if ok {
                        color::green(Stream::Out, "fetched")
                    } else {
                        color::red(Stream::Out, "failed")
                    };
                    println!("  {tag} {file}");
                }
            }
        }
        if self.tty {
            self.render();
        }
    }

    /// Leaves the terminal on a clean line. A no-op when nothing was ever
    /// drawn — an empty transaction, or output that was never a terminal.
    pub fn finish(&mut self) {
        if self.tty && self.done + self.failed + self.active.len() > 0 {
            println!();
        }
    }

    fn total_downloaded(&self) -> i64 {
        self.downloaded_done + self.active.values().map(|(d, _)| d).sum::<i64>()
    }

    fn total_known(&self) -> i64 {
        self.known_total_done + self.active.values().map(|(d, t)| (*t).max(*d)).sum::<i64>()
    }

    /// Throttled to a few times a second: libalpm's `dl_cb` fires far more
    /// often than a terminal needs redrawing, and `\r` writes are not free.
    fn render(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_render) < Duration::from_millis(120) {
            return;
        }
        self.last_render = now;

        let downloaded = self.total_downloaded();
        let (last_time, last_bytes) = self.last_sample;
        let elapsed = now.duration_since(last_time).as_secs_f64();
        if elapsed > 0.2 {
            let rate = (downloaded - last_bytes).max(0) as f64 / elapsed;
            self.speed = if self.speed == 0.0 {
                rate
            } else {
                self.speed * 0.7 + rate * 0.3
            };
            self.last_sample = (now, downloaded);
        }

        let known = self.total_known();
        // A rough estimate, not a promise: `known` only covers files that
        // have started, so it grows as the transaction goes — the same
        // reason a download manager's ETA wanders early on and settles late.
        let eta = (self.speed > 1024.0 && known > downloaded)
            .then(|| (known - downloaded) as f64 / self.speed);

        let mut line = format!(
            "  {} {} done",
            color::bold(Stream::Out, "fetching"),
            color::green(Stream::Out, &self.done.to_string())
        );
        if self.failed > 0 {
            line.push_str(&format!(
                ", {} failed",
                color::red(Stream::Out, &self.failed.to_string())
            ));
        }
        if !self.active.is_empty() {
            line.push_str(&format!(
                ", {} active",
                color::cyan(Stream::Out, &self.active.len().to_string())
            ));
        }
        line.push_str(&format!(
            " · {}",
            color::dim(Stream::Out, &human_bytes(downloaded))
        ));
        if self.speed > 1024.0 {
            line.push_str(&format!(
                " @ {}/s",
                color::cyan(Stream::Out, &human_bytes(self.speed as i64))
            ));
        }
        if let Some(eta) = eta {
            line.push_str(&format!(
                " · eta {}",
                color::bold(Stream::Out, &human_duration(eta))
            ));
        }

        print!("\r\x1b[K{line}");
        let _ = std::io::stdout().flush();
    }
}

impl Default for FetchProgress {
    fn default() -> FetchProgress {
        FetchProgress::new()
    }
}

fn human_bytes(n: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn human_duration(secs: f64) -> String {
    let secs = secs.round().max(0.0) as u64;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_pick_the_largest_unit_that_keeps_one_digit_of_whole_part() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
    }

    #[test]
    fn duration_switches_units_at_the_minute_and_hour() {
        assert_eq!(human_duration(42.0), "42s");
        assert_eq!(human_duration(125.0), "2m05s");
        assert_eq!(human_duration(3725.0), "1h02m");
    }
}
