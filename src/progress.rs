//! ASCII progress bars for vcp — Midnight Commander-like "feeling",
//! but plain text: no curses, works over ssh in any dumb-ish terminal.
//!
//! Bars are redrawn in place using '\r' plus minimal ANSI cursor-up /
//! erase-line sequences.  Bar content itself is pure ASCII.  All progress
//! output goes to stderr and is automatically disabled when stderr is
//! not a tty.
//!
//! Single object copied: one full-width bar plus speed and ETA lines:
//!   |=============== (filename   34.01%) ===>          |
//!   | speed: current    4.213 M/s    average    3.987 M/s          |
//!   | eta:   current          1h 32m 16s    average          1h 40m 02s |
//! More than one object: three labeled bars plus speed and ETA lines:
//!   count: |===== ( 12/345    3.48%) =>                |
//!   bytes: |===== ( 259.31M/2.03T   12.34%) ==>        |
//!   file:  |===== (...current/file.c   45.00%) ====>   |
//!   speed: | current    4.213 M/s    average    3.987 M/s        |
//!   eta:   | current          1h 32m 16s    average          1h 40m 02s |
//!
//! `current` is the transfer rate over the last few seconds
//! (SPEED_WINDOW), `average` the rate since copying started; the ETA
//! line shows the time remaining at each of the two rates.  All are
//! fixed-width fields so nothing moves while the numbers change.
//!
//! The `(...)` status text in the middle is not part of the gauge: the
//! fill track is the cells left of `(` plus the cells right of `)`, so
//! `|=====(` already means ~half done and the arrow re-appears after
//! `)` on its way to the right edge.  Numeric fields are padded to a
//! constant width for the whole copy, so `/` and `%` never move.
//!
//! Bar width is taken from the `COLUMNS` environment variable if set
//! (`COLUMNS=100 vcp ...`), otherwise from the terminal size.  Bars are
//! never drawn narrower than 80 columns: smaller `COLUMNS` values are
//! ignored and 80 is also the fallback when no width can be determined.
//!
//! Names too long for the bar are shortened with an ASCII ellipsis whose
//! position slides back and forth over time ("long...name", "lon...ame",
//! ...), so the whole name can be read over a few redraws.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

const THROTTLE: Duration = Duration::from_millis(50);
/// Sliding-ellipsis animation advances one position per this many ms.
const ANIM_MS: u128 = 100;
/// "current" speed is measured over (at least) this much recent history.
const SPEED_WINDOW: Duration = Duration::from_secs(3);
/// Minimum sensible bar width; narrower `COLUMNS`/terminal values are
/// bumped up to this, and it doubles as the fallback when the width
/// cannot be determined at all.
const MIN_WIDTH: usize = 80;

pub struct Progress {
    enabled: bool,
    three: bool,
    total_files: u64,
    done_files: u64,
    total_bytes: u64,
    done_bytes: u64,
    cur_size: u64,
    cur_done: u64,
    cur_name: String,
    drawn: usize,
    started: bool,
    last: Instant,
    anim0: Instant,
    /// Bytes actually transferred (unlike done_bytes, which also
    /// accounts for skipped/failed files to keep the bars honest).
    xfer: u64,
    /// When copying started; the base of the average speed.
    t0: Instant,
    /// (time, xfer) samples covering at least SPEED_WINDOW; the front
    /// one is the reference point for the current speed.
    samples: VecDeque<(Instant, u64)>,
}

fn term_width() -> usize {
    // Explicit COLUMNS wins: shells don't export it by default, so if
    // it is in the environment the user asked for that width
    // (COLUMNS=100 vcp ...); also useful under script(1) or over ssh.
    // Anything below MIN_WIDTH does not make sense for these bars.
    if let Ok(v) = std::env::var("COLUMNS") {
        if let Ok(n) = v.trim().parse::<usize>() {
            if n >= MIN_WIDTH {
                return n;
            }
        }
    }
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return (ws.ws_col as usize).max(MIN_WIDTH);
        }
    }
    MIN_WIDTH
}

/// Humanize a byte count with two decimals: 0.00B, 512.00B, 1.31M, ...
fn humanize(n: u64) -> String {
    const UNITS: [&str; 7] = ["B", "K", "M", "G", "T", "P", "E"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.2}{}", v, UNITS[i])
}

/// Width the humanized "done" field must be padded to so that it can
/// hold any value from 0 up to `total` without moving the `/`.
fn hum_width(total: u64) -> usize {
    if total < 1024 {
        humanize(total).len()
    } else {
        "1023.00B".len() // widest possible rendering of any smaller value
    }
}

/// `done/total` for the bytes bar, `done` padded to a constant width.
fn bytes_info(done: u64, total: u64) -> String {
    format!(
        "{:>w$}/{}",
        humanize(done.min(total)),
        humanize(total),
        w = hum_width(total)
    )
}

/// `done/total` for the count bar, `done` padded to a constant width.
fn count_info(done: u64, total: u64) -> String {
    format!("{:>w$}/{}", done, total, w = total.to_string().len())
}

/// Fixed-width (12 chars) transfer rate: `   4.213 M/s`, `1023.999 K/s`.
fn speed_str(bps: f64) -> String {
    const UNITS: [&str; 7] = ["B", "K", "M", "G", "T", "P", "E"];
    let mut v = if bps.is_finite() && bps > 0.0 { bps } else { 0.0 };
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    format!("{:>8.3} {}/s", v, UNITS[i])
}

/// Render the speed/ETA status block, boxed like the bars and `width - 1`
/// columns wide, two lines:
///   speed: | current    4.213 M/s    average    3.987 M/s               |
///   eta:   | current          1h 32m 16s    average          1h 40m 02s |
/// With an empty label (single-object mode) the words move inside:
///   | speed: current    4.213 M/s    average    3.987 M/s               |
///   | eta:   current          1h 32m 16s    average          1h 40m 02s |
fn speed_line(label: &str, cur: f64, avg: f64, width: usize) -> String {
    boxed_line(
        label,
        &format!(
            "{} current {}    average {}",
            if label.is_empty() { " speed:" } else { "" },
            speed_str(cur),
            speed_str(avg)
        ),
        width,
    )
}

fn eta_line(label: &str, cur: f64, avg: f64, remaining: u64, width: usize) -> String {
    boxed_line(
        label,
        &format!(
            "{} current {}    average {}",
            if label.is_empty() { " eta:  " } else { "" },
            eta_str(remaining, cur),
            eta_str(remaining, avg)
        ),
        width,
    )
}

fn boxed_line(label: &str, txt: &str, width: usize) -> String {
    let inner = width.saturating_sub(label.len() + 3).max(13);
    let mut cells = txt.as_bytes().to_vec();
    cells.truncate(inner);
    cells.resize(inner, b' ');
    format!("{}|{}|", label, String::from_utf8_lossy(&cells))
}

/// Width of the ETA field: fits the longest value, `999w 6d 23h 59m 59s`.
const ETA_W: usize = 19;

/// Fixed-width time remaining for `remaining` bytes at `bps`: `1w 3d 1h
/// 32m 16s`-style, seconds precision, weeks the largest unit, right
/// aligned; `done` when nothing is left, `unknown` when the rate is zero
/// (stalled / not yet measured), `>999w` when hopeless.
fn eta_str(remaining: u64, bps: f64) -> String {
    if remaining == 0 {
        return format!("{:>w$}", "done", w = ETA_W);
    }
    if !(bps.is_finite() && bps > 0.0) {
        return format!("{:>w$}", "unknown", w = ETA_W);
    }
    let secs = (remaining as f64 / bps).ceil();
    if secs >= 1000.0 * 7.0 * 86400.0 {
        return format!("{:>w$}", ">999w", w = ETA_W);
    }
    format!("{:>w$}", human_dur(secs as u64), w = ETA_W)
}

/// `1w 3d 1h 32m 16s` — leading zero units dropped, inner ones kept
/// (`1h 00m 05s`), seconds granularity, weeks the largest unit.
fn human_dur(mut s: u64) -> String {
    let w = s / 604800;
    s %= 604800;
    let d = s / 86400;
    s %= 86400;
    let h = s / 3600;
    s %= 3600;
    let m = s / 60;
    s %= 60;
    let mut out = String::new();
    let mut started = false;
    for (v, u, width) in [(w, "w", 1), (d, "d", 1), (h, "h", 1), (m, "m", 2), (s, "s", 2)] {
        if !started && v == 0 && u != "s" {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        if started {
            out.push_str(&format!("{:0w$}{}", v, u, w = width));
        } else {
            out.push_str(&format!("{}{}", v, u));
        }
        started = true;
    }
    out
}

/// Shorten a long name to `max` chars with an ASCII "..." replacing a
/// sliding window of the hidden part.  The ellipsis position advances
/// with `tick` and ping-pongs across the name: "...long/tail" ->
/// "s...ng/tail" -> ... -> "some/long..." -> back.  Output length is
/// constant for a given `max`, so bars never jiggle.
fn slide_trunc(s: &str, max: usize, tick: usize) -> String {
    let ch: Vec<char> = s.chars().collect();
    if ch.len() <= max {
        return s.to_string();
    }
    if max <= 3 {
        return ch[ch.len() - max..].iter().collect();
    }
    let visible = max - 3; // name chars shown
    let period = visible + 1; // ellipsis positions: 0..=visible
    let cycle = if period > 1 { 2 * period - 2 } else { 1 };
    let t = tick % cycle;
    let p = if t < period { t } else { cycle - t };
    let head: String = ch[..p].iter().collect();
    let tail: String = ch[ch.len() - (visible - p)..].iter().collect();
    format!("{}...{}", head, tail)
}

/// Render one bar line: `label|====> (info   34.01%)      |`.
/// The whole line is at most `width - 1` columns to avoid autowrap.
/// The centered `(...)` text is excluded from the fill track: the fill
/// occupies the cells before `(`, then continues after `)`.
fn bar(label: &str, info: &str, pct: f64, width: usize, tick: usize) -> String {
    let pct = if pct.is_finite() { pct.clamp(0.0, 1.0) } else { 0.0 };
    let inner = width.saturating_sub(label.len() + 3).max(13);
    let pcts = format!("{:6.2}%", pct * 100.0);
    let mut txt = if info.is_empty() {
        format!(" ({}) ", pcts)
    } else {
        let room = inner.saturating_sub(pcts.len() + 8).max(1);
        format!(" ({}   {}) ", slide_trunc(info, room, tick), pcts)
    };
    if txt.len() > inner {
        txt = format!(" ({}) ", pcts);
    }
    if txt.len() > inner {
        txt = pcts.clone();
    }
    let tlen = txt.len().min(inner);
    let start = (inner - tlen) / 2;
    let left_w = start;
    let right_w = inner - start - tlen;
    let track = left_w + right_w;
    let filled = (((track as f64) * pct).round() as usize).min(track);
    let mut cells: Vec<u8> = vec![b' '; inner];
    for c in cells.iter_mut().take(filled.min(left_w)) {
        *c = b'=';
    }
    for i in 0..filled.saturating_sub(left_w) {
        cells[start + tlen + i] = b'=';
    }
    if filled > 0 && filled < track {
        let head = if filled <= left_w {
            filled - 1
        } else {
            start + tlen + (filled - left_w) - 1
        };
        cells[head] = b'>';
    }
    cells[start..start + tlen].copy_from_slice(&txt.as_bytes()[..tlen]);
    format!("{}|{}|", label, String::from_utf8_lossy(&cells))
}

fn ascii_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' })
        .collect()
}

impl Progress {
    pub fn new(total_files: u64, total_bytes: u64) -> Self {
        let tty = unsafe { libc::isatty(libc::STDERR_FILENO) == 1 };
        Progress {
            enabled: tty && total_files >= 1,
            three: total_files > 1,
            total_files,
            done_files: 0,
            total_bytes,
            done_bytes: 0,
            cur_size: 0,
            cur_done: 0,
            cur_name: String::new(),
            drawn: 0,
            started: false,
            last: Instant::now(),
            anim0: Instant::now(),
            xfer: 0,
            t0: Instant::now(),
            samples: VecDeque::new(),
        }
    }

    pub fn start(&mut self) {
        if self.enabled {
            self.started = true;
            let t0 = Instant::now();
            self.t0 = t0;
            self.samples.clear();
            self.record(t0);
            self.draw(true);
        }
    }

    pub fn file_start(&mut self, name: &str, size: u64) {
        self.cur_name = ascii_name(name);
        self.cur_size = size;
        self.cur_done = 0;
        self.draw(false);
    }

    pub fn add(&mut self, n: u64) {
        self.cur_done += n;
        self.done_bytes += n;
        self.xfer += n;
        self.draw(false);
    }

    pub fn file_end(&mut self) {
        self.done_files += 1;
        self.done_bytes += self.cur_size.saturating_sub(self.cur_done);
        self.cur_done = self.cur_size;
        self.draw(false);
    }

    pub fn finish(&mut self) {
        if !self.started {
            return;
        }
        self.draw(true);
        if self.drawn > 0 {
            let mut e = io::stderr().lock();
            let _ = e.write_all(b"\n");
            let _ = e.flush();
            self.drawn = 0;
        }
        self.enabled = false;
        self.started = false;
    }

    fn pct_files(&self) -> f64 {
        if self.total_files == 0 {
            1.0
        } else {
            self.done_files as f64 / self.total_files as f64
        }
    }

    fn pct_bytes(&self) -> f64 {
        if self.total_bytes == 0 {
            self.pct_files()
        } else {
            self.done_bytes as f64 / self.total_bytes as f64
        }
    }

    fn pct_cur(&self) -> f64 {
        if self.cur_size == 0 {
            if self.cur_name.is_empty() {
                0.0
            } else {
                1.0
            }
        } else {
            self.cur_done as f64 / self.cur_size as f64
        }
    }

    /// Take a speed sample at `now`, dropping history older than the
    /// window while keeping the last sample before its boundary so the
    /// front always spans at least SPEED_WINDOW (once that much has
    /// elapsed).
    fn record(&mut self, now: Instant) {
        self.samples.push_back((now, self.xfer));
        while self.samples.len() >= 2 && now.duration_since(self.samples[1].0) >= SPEED_WINDOW {
            self.samples.pop_front();
        }
    }

    /// (current, average) transfer rates in bytes/second at `now`.
    fn speeds(&self, now: Instant) -> (f64, f64) {
        let el = now.duration_since(self.t0).as_secs_f64();
        let avg = if el > 0.0 { self.xfer as f64 / el } else { 0.0 };
        let cur = match self.samples.front() {
            Some(&(t, b)) => {
                let dt = now.duration_since(t).as_secs_f64();
                if dt > 0.0 {
                    self.xfer.saturating_sub(b) as f64 / dt
                } else {
                    avg
                }
            }
            None => avg,
        };
        (cur, avg)
    }

    /// Bytes still to be transferred; the basis of the ETA.
    fn remaining(&self) -> u64 {
        self.total_bytes.saturating_sub(self.done_bytes)
    }

    fn draw(&mut self, force: bool) {
        if !self.enabled || !self.started {
            return;
        }
        if !force && self.drawn > 0 && self.last.elapsed() < THROTTLE {
            return;
        }
        let now = Instant::now();
        self.last = now;
        self.record(now);
        let (cur, avg) = self.speeds(now);
        let rem = self.remaining();
        let w = term_width();
        let tick = (self.anim0.elapsed().as_millis() / ANIM_MS) as usize;
        let name = self.cur_name.clone();
        let mut lines: Vec<String> = Vec::new();
        if self.three {
            lines.push(bar(
                "count: ",
                &count_info(self.done_files, self.total_files),
                self.pct_files(),
                w,
                tick,
            ));
            lines.push(bar(
                "bytes: ",
                &bytes_info(self.done_bytes, self.total_bytes),
                self.pct_bytes(),
                w,
                tick,
            ));
            lines.push(bar("file:  ", &name, self.pct_cur(), w, tick));
            lines.push(speed_line("speed: ", cur, avg, w));
            lines.push(eta_line("eta:   ", cur, avg, rem, w));
        } else {
            lines.push(bar("", &name, self.pct_bytes(), w, tick));
            lines.push(speed_line("", cur, avg, w));
            lines.push(eta_line("", cur, avg, rem, w));
        }
        let mut out = String::new();
        if self.drawn > 0 {
            out.push('\r');
            for _ in 1..self.drawn {
                out.push_str("\x1b[A");
            }
        }
        for (i, l) in lines.iter().enumerate() {
            out.push_str(l);
            out.push_str("\x1b[K");
            if i + 1 < lines.len() {
                out.push('\n');
            }
        }
        let mut e = io::stderr().lock();
        let _ = e.write_all(out.as_bytes());
        let _ = e.flush();
        self.drawn = lines.len();
    }

    pub fn clear(&mut self) {
        if self.drawn == 0 {
            return;
        }
        let mut out = String::from("\r\x1b[2K");
        for _ in 1..self.drawn {
            out.push_str("\x1b[A\x1b[2K");
        }
        let mut e = io::stderr().lock();
        let _ = e.write_all(out.as_bytes());
        let _ = e.flush();
        self.drawn = 0;
    }

    /// Warning to stderr, "vcp: " prefixed, keeping bars intact.
    pub fn warn(&mut self, msg: &str) {
        self.clear();
        eprintln!("vcp: {}", msg);
        if self.started {
            self.draw(true);
        }
    }

    /// Raw text to stderr (no prefix, no added newline).
    pub fn raw_err(&mut self, msg: &str) {
        self.clear();
        {
            let mut e = io::stderr().lock();
            let _ = e.write_all(msg.as_bytes());
            let _ = e.flush();
        }
        if self.started {
            self.draw(true);
        }
    }

    /// Line to stdout (used by -v), keeping bars intact.
    pub fn out(&mut self, msg: &str) {
        self.clear();
        println!("{}", msg);
        let _ = io::stdout().flush();
        if self.started {
            self.draw(true);
        }
    }

    /// Interactive y/n prompt on stderr, answer read from stdin
    /// (same protocol as cp(1): first char y/Y is affirmative).
    pub fn prompt_yn(&mut self, q: &str) -> bool {
        self.clear();
        {
            let mut e = io::stderr().lock();
            let _ = e.write_all(q.as_bytes());
            let _ = e.flush();
        }
        let mut first: Option<u8> = None;
        let mut b = [0u8; 1];
        let mut si = io::stdin().lock();
        loop {
            match si.read(&mut b) {
                Ok(0) => break,
                Ok(_) => {
                    if b[0] == b'\n' {
                        break;
                    }
                    if first.is_none() {
                        first = Some(b[0]);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        if self.started {
            self.draw(true);
        }
        matches!(first, Some(b'y') | Some(b'Y'))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bar, bytes_info, count_info, eta_line, eta_str, human_dur, humanize, slide_trunc,
        speed_line, speed_str, term_width, Progress, ETA_W,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn short_names_untouched() {
        assert_eq!(slide_trunc("abc.txt", 10, 0), "abc.txt");
        assert_eq!(slide_trunc("abc.txt", 10, 99), "abc.txt");
        assert_eq!(slide_trunc("abcdefghij", 10, 5), "abcdefghij");
    }

    #[test]
    fn tiny_slots() {
        assert_eq!(slide_trunc("abcdefgh", 3, 0), "fgh");
        assert_eq!(slide_trunc("abcdefgh", 3, 7), "fgh");
        assert_eq!(slide_trunc("abcdefgh", 1, 4), "h");
    }

    #[test]
    fn slides_forward_then_back() {
        let s = "0123456789"; // len 10, max 7 -> visible 4, p in 0..=4
        assert_eq!(slide_trunc(s, 7, 0), "...6789");
        assert_eq!(slide_trunc(s, 7, 1), "0...789");
        assert_eq!(slide_trunc(s, 7, 2), "01...89");
        assert_eq!(slide_trunc(s, 7, 3), "012...9");
        assert_eq!(slide_trunc(s, 7, 4), "0123...");
        // ping-pong back
        assert_eq!(slide_trunc(s, 7, 5), "012...9");
        assert_eq!(slide_trunc(s, 7, 6), "01...89");
        assert_eq!(slide_trunc(s, 7, 7), "0...789");
        assert_eq!(slide_trunc(s, 7, 8), "...6789");
        // full cycle repeats
        assert_eq!(slide_trunc(s, 7, 9), "0...789");
    }

    #[test]
    fn constant_width_and_coverage() {
        // Name shorter than 2x the visible slot: every char is revealed
        // at some tick.
        let s = "abcdefghijklmn"; // len 14, max 11 -> visible 8
        let max = 11;
        let mut seen = vec![false; s.len()];
        for tick in 0..64 {
            let out = slide_trunc(s, max, tick);
            assert_eq!(out.chars().count(), max, "width must not jiggle");
            let (head, tail) = out.split_once("...").unwrap();
            assert!(s.starts_with(head) && s.ends_with(tail));
            for i in 0..head.len() {
                seen[i] = true;
            }
            for i in s.len() - tail.len()..s.len() {
                seen[i] = true;
            }
        }
        assert!(seen.iter().all(|&b| b));

        // Very long name: the sweep reveals the first and last `visible`
        // chars (the ellipsis hides a fixed-size middle chunk, so deeper
        // middle parts are unreachable by construction).
        let s = "abcdefghijklmnopqrstuvwxyz"; // len 26, max 11 -> visible 8
        let mut seen = vec![false; s.len()];
        for tick in 0..64 {
            let out = slide_trunc(s, max, tick);
            assert_eq!(out.chars().count(), max);
            let (head, tail) = out.split_once("...").unwrap();
            assert!(s.starts_with(head) && s.ends_with(tail));
            for i in 0..head.len() {
                seen[i] = true;
            }
            for i in s.len() - tail.len()..s.len() {
                seen[i] = true;
            }
        }
        let visible = max - 3;
        for (i, &b) in seen.iter().enumerate() {
            let reachable = i < visible || i >= s.len() - visible;
            assert_eq!(b, reachable, "char {} coverage", i);
        }
    }

    #[test]
    fn humanize_two_decimals() {
        assert_eq!(humanize(0), "0.00B");
        assert_eq!(humanize(512), "512.00B");
        assert_eq!(humanize(1023), "1023.00B");
        assert_eq!(humanize(1024), "1.00K");
        assert_eq!(humanize(1085276160), "1.01G");
    }

    #[test]
    fn info_fields_constant_width() {
        let total: u64 = 1085276160; // 1.01G
        let w = bytes_info(0, total).len();
        for done in [0, 1, 1023, 1024, 999 << 20, total - 1, total] {
            let s = bytes_info(done, total);
            assert_eq!(s.len(), w, "width for done={}: {:?}", done, s);
            // '/' never moves
            assert_eq!(s.find('/'), bytes_info(0, total).find('/'));
        }
        assert_eq!(bytes_info(0, total), "   0.00B/1.01G");
        assert_eq!(bytes_info(999 << 20, total), " 999.00M/1.01G");
        // small totals: padded to the total's own width
        assert_eq!(bytes_info(0, 999), "  0.00B/999.00B");
        assert_eq!(count_info(3, 120), "  3/120");
        assert_eq!(count_info(120, 120), "120/120");
    }

    #[test]
    fn bar_excludes_text_from_track() {
        // width 25, label "" -> inner 22; empty info -> txt " ( 50.00%) "
        // (len 11), centered at start 5: left track 5, right track 6.
        assert_eq!(bar("", "", 0.0, 25, 0), "|      (  0.00%)       |");
        // half done: left track full, head just after ')'
        assert_eq!(bar("", "", 0.5, 25, 0), "|===== ( 50.00%) >     |");
        // just before touching '(': head at the end of the left track
        assert_eq!(bar("", "", 5.0 / 11.0, 25, 0), "|====> ( 45.45%)       |");
        // done: both tracks full, no arrow head
        assert_eq!(bar("", "", 1.0, 25, 0), "|===== (100.00%) ======|");
        // no '>' or '=' may ever appear inside the (...) text
        for i in 0..=20 {
            let line = bar("x: ", "name", i as f64 / 20.0, 40, 0);
            let l = line.find('(').unwrap();
            let r = line.rfind(')').unwrap();
            assert!(!line[l..=r].contains('>'), "{}", line);
            assert!(!line[l..=r].contains('='), "{}", line);
        }
    }

    #[test]
    fn bar_line_length_is_constant() {
        // Every bar line is exactly width-1 columns, whatever the state.
        for width in [25, 40, 60, 79, 132] {
            for (label, info) in [
                ("", ""),
                ("count: ", "12/345"),
                ("bytes: ", "   1.00K/3.00M"),
                ("file:  ", "some/long/file/name/deep/inside/a/tree.txt"),
            ] {
                for i in 0..=10 {
                    let line = bar(label, info, i as f64 / 10.0, width, i);
                    assert_eq!(line.chars().count(), width - 1, "{:?}", line);
                }
            }
        }
    }

    #[test]
    fn punctuation_never_moves() {
        // For a fixed total, '(', '/', '%', ')' must sit at the same
        // column in every redraw, whatever `done` or the tick is.
        let total: u64 = 3 << 30;
        let files: u64 = 345;
        let w = 60;
        let mut pos: Option<(usize, usize, usize, usize)> = None;
        for (i, done) in [0u64, 1, 512, 1 << 20, 1 << 30, (3 << 30) - 1, 3 << 30]
            .iter()
            .enumerate()
        {
            let pct = *done as f64 / total as f64;
            let line = bar("bytes: ", &bytes_info(*done, total), pct, w, i);
            let p = (
                line.find('(').unwrap(),
                line.find('/').unwrap(),
                line.find('%').unwrap(),
                line.find(')').unwrap(),
            );
            if let Some(prev) = pos {
                assert_eq!(prev, p, "line: {:?}", line);
            }
            pos = Some(p);
        }
        pos = None;
        for (i, done) in [0u64, 1, 9, 99, 199, 344, 345].iter().enumerate() {
            let pct = *done as f64 / files as f64;
            let line = bar("count: ", &count_info(*done, files), pct, w, i);
            let p = (
                line.find('(').unwrap(),
                line.find('/').unwrap(),
                line.find('%').unwrap(),
                line.find(')').unwrap(),
            );
            if let Some(prev) = pos {
                assert_eq!(prev, p, "line: {:?}", line);
            }
            pos = Some(p);
        }
        // file bar: a sliding (long) name keeps '(', '%', ')' pinned too
        pos = None;
        let name = "quite/a/long/path/to/some/deeply/nested/file.txt";
        for tick in 0..40 {
            let line = bar("file:  ", name, 0.37, w, tick);
            let p = (
                line.find('(').unwrap(),
                0,
                line.find('%').unwrap(),
                line.find(')').unwrap(),
            );
            if let Some(prev) = pos {
                assert_eq!(prev, p, "line: {:?}", line);
            }
            pos = Some(p);
        }
    }

    #[test]
    fn arrow_skips_text_and_moves_monotonically() {
        // As pct grows the head '>' only ever moves right, is never
        // inside the (...) text, and the fill left of '(' plus right of
        // ')' always matches round(pct * track).
        let w = 50;
        let mut prev_head: i64 = -1;
        let mut seen_left = false;
        let mut seen_right = false;
        for i in 0..=1000 {
            let pct = i as f64 / 1000.0;
            let line = bar("", "12/345", pct, w, 0);
            let l = line.find('(').unwrap();
            let r = line.find(')').unwrap();
            let eq_total = line.bytes().filter(|&b| b == b'=').count()
                + line.bytes().filter(|&b| b == b'>').count();
            // excluded text = "(...)" plus one padding space each side
            let track: usize = line.len() - 2 - (r - l + 3);
            assert_eq!(eq_total, ((track as f64) * pct).round() as usize);
            if let Some(h) = line.find('>') {
                assert!(h < l || h > r, "head inside text: {:?}", line);
                assert!(h as i64 >= prev_head, "head moved left: {:?}", line);
                prev_head = h as i64;
                if h < l {
                    seen_left = true;
                }
                if h > r {
                    seen_right = true;
                }
            }
        }
        assert!(seen_left && seen_right, "head must appear on both sides");
        // 0%: empty track; 100%: full track, no head
        let z = bar("", "12/345", 0.0, w, 0);
        assert!(!z.contains('=') && !z.contains('>'));
        let f = bar("", "12/345", 1.0, w, 0);
        assert!(!f.contains('>'));
    }

    #[test]
    fn columns_env_overrides_width_min_80() {
        // This is the only test that touches COLUMNS (process-global).
        std::env::set_var("COLUMNS", "100");
        assert_eq!(term_width(), 100);
        std::env::set_var("COLUMNS", "80");
        assert_eq!(term_width(), 80);
        std::env::set_var("COLUMNS", "79"); // below the minimum: ignored
        assert!(term_width() >= 80);
        std::env::set_var("COLUMNS", "bogus"); // unparsable: ignored
        assert!(term_width() >= 80);
        std::env::remove_var("COLUMNS");
        // no tty in the test harness -> MIN_WIDTH fallback; with a tty
        // the ioctl result is clamped to >= 80 as well
        assert!(term_width() >= 80);
    }

    const MB: f64 = 1048576.0;

    #[test]
    fn speed_str_fixed_width() {
        assert_eq!(speed_str(0.0), "   0.000 B/s");
        assert_eq!(speed_str(999.5), " 999.500 B/s");
        assert_eq!(speed_str(1023.999 * 1024.0), "1023.999 K/s");
        assert_eq!(speed_str(4.213 * MB), "   4.213 M/s");
        assert_eq!(speed_str(3.5 * 1024.0 * MB), "   3.500 G/s");
        assert_eq!(speed_str(f64::NAN), "   0.000 B/s");
        assert_eq!(speed_str(-5.0), "   0.000 B/s");
        for v in [0.0, 1.0, 1023.0, 1024.0, 12345.678, 4.2 * MB, 1e12, 1e15] {
            assert_eq!(speed_str(v).len(), 12, "{}", v);
        }
    }

    #[test]
    fn speed_line_layout() {
        for width in [80, 100, 132] {
            let l = speed_line("speed: ", 4.213 * MB, 3.987 * MB, width);
            assert_eq!(l.chars().count(), width - 1);
            assert!(
                l.starts_with("speed: | current    4.213 M/s    average    3.987 M/s "),
                "{:?}",
                l
            );
            assert!(l.ends_with('|'));
            let s = speed_line("", 4.213 * MB, 3.987 * MB, width);
            assert_eq!(s.chars().count(), width - 1);
            assert!(
                s.starts_with("| speed: current    4.213 M/s    average    3.987 M/s "),
                "{:?}",
                s
            );
            assert!(s.ends_with('|'));
        }
        // words, both "/s" and the closing '|' never move as values change
        let vals = [0.0, 1.5, 999.0 * 1024.0, 4.2 * MB, 3.5 * 1024.0 * MB, 7.0e12];
        for label in ["speed: ", ""] {
            let key = |l: &str| {
                (
                    l.find("current").unwrap(),
                    l.find("average").unwrap(),
                    l.find("/s").unwrap(),
                    l.rfind("/s").unwrap(),
                    l.rfind('|').unwrap(),
                )
            };
            let first = key(&speed_line(label, vals[0], vals[0], 80));
            for &c in &vals {
                for &a in &vals {
                    assert_eq!(key(&speed_line(label, c, a, 80)), first);
                }
            }
        }
    }

    #[test]
    fn speeds_window_and_average() {
        let mb = 1u64 << 20;
        let t0 = Instant::now();
        let mut p = Progress::new(2, 0);
        p.t0 = t0;
        p.samples.clear();
        p.record(t0);
        let (cur, avg) = p.speeds(t0);
        assert_eq!((cur, avg), (0.0, 0.0));

        // 10 s at 10 MB/s (1 MB every 100 ms)
        let mut t = t0;
        for _ in 0..100 {
            t += Duration::from_millis(100);
            p.add(mb);
            p.record(t);
        }
        let (cur, avg) = p.speeds(t);
        assert!((avg - 10.0 * MB).abs() < 0.01 * MB, "avg {}", avg);
        assert!((cur - 10.0 * MB).abs() < 0.01 * MB, "cur {}", cur);
        // history is bounded: ~window/sample-interval entries
        assert!(p.samples.len() <= 32, "{}", p.samples.len());

        // then 5 s at 1 MB/s: average blends, current follows the window
        for _ in 0..50 {
            t += Duration::from_millis(100);
            p.add(mb / 10);
            p.record(t);
        }
        let (cur, avg) = p.speeds(t);
        assert!((avg - 7.0 * MB).abs() < 0.01 * MB, "avg {}", avg); // 105 MB / 15 s
        assert!((cur - 1.0 * MB).abs() < 0.01 * MB, "cur {}", cur);

        // stall for a full window: current drops to 0, average decays
        let t2 = t + Duration::from_secs(3);
        p.record(t2);
        let (cur, avg) = p.speeds(t2);
        assert_eq!(cur, 0.0);
        assert!((avg - 105.0 * MB / 18.0).abs() < 0.01 * MB, "avg {}", avg);
    }

    #[test]
    fn human_dur_units() {
        assert_eq!(human_dur(0), "0s");
        assert_eq!(human_dur(5), "5s");
        assert_eq!(human_dur(59), "59s");
        assert_eq!(human_dur(60), "1m 00s");
        assert_eq!(human_dur(61), "1m 01s");
        assert_eq!(human_dur(3599), "59m 59s");
        assert_eq!(human_dur(3600), "1h 00m 00s");
        assert_eq!(human_dur(3600 + 5), "1h 00m 05s");
        assert_eq!(human_dur(5536), "1h 32m 16s");
        assert_eq!(human_dur(86400), "1d 0h 00m 00s");
        assert_eq!(human_dur(86400 * 7), "1w 0d 0h 00m 00s");
        // the requested example: 1w 3d 1h 32m 16s
        assert_eq!(human_dur(604800 + 3 * 86400 + 3600 + 32 * 60 + 16), "1w 3d 1h 32m 16s");
        // weeks are the largest unit: 52 weeks, not "1y"
        assert_eq!(human_dur(52 * 604800), "52w 0d 0h 00m 00s");
        assert_eq!(human_dur(999 * 604800 + 6 * 86400 + 23 * 3600 + 59 * 60 + 59),
                   "999w 6d 23h 59m 59s");
        assert_eq!("999w 6d 23h 59m 59s".len(), ETA_W);
    }

    #[test]
    fn eta_str_fixed_width_and_cases() {
        assert_eq!(eta_str(0, 1.0), format!("{:>19}", "done"));
        assert_eq!(eta_str(0, 0.0), format!("{:>19}", "done"));
        assert_eq!(eta_str(100, 0.0), format!("{:>19}", "unknown"));
        assert_eq!(eta_str(100, f64::NAN), format!("{:>19}", "unknown"));
        assert_eq!(eta_str(100, -1.0), format!("{:>19}", "unknown"));
        assert_eq!(eta_str(1000, 100.0), format!("{:>19}", "10s"));
        assert_eq!(eta_str(1001, 100.0), format!("{:>19}", "11s")); // rounds up
        assert_eq!(eta_str(5536 * 1024, 1024.0), format!("{:>19}", "1h 32m 16s"));
        // a byte at a snail's pace: capped
        assert_eq!(eta_str(u64::MAX, 1.0), format!("{:>19}", ">999w"));
        assert_eq!(eta_str(1000 * 604800, 1.0), format!("{:>19}", ">999w"));
        assert_eq!(eta_str(1000 * 604800 - 1, 1.0), format!("{:>19}", "999w 6d 23h 59m 59s"));
        for (rem, bps) in [(0u64, 0.0), (1, 0.0), (1, 1e9), (1 << 40, 3.5), (u64::MAX, 0.001)] {
            assert_eq!(eta_str(rem, bps).len(), ETA_W, "rem={} bps={}", rem, bps);
        }
    }

    #[test]
    fn eta_line_layout_and_pinned() {
        for width in [80, 100, 132] {
            let l = eta_line("eta:   ", 1024.0, 512.0, 5536 * 1024, width);
            assert_eq!(l.chars().count(), width - 1);
            assert!(
                l.starts_with("eta:   | current          1h 32m 16s    average          3h 04m 32s "),
                "{:?}",
                l
            );
            assert!(l.ends_with('|'));
            let s = eta_line("", 1024.0, 512.0, 5536 * 1024, width);
            assert_eq!(s.chars().count(), width - 1);
            assert!(
                s.starts_with("| eta:   current          1h 32m 16s    average          3h 04m 32s "),
                "{:?}",
                s
            );
            // the "eta:" word and "current" line up under the speed line
            // in single mode (the wider ETA fields shift "average")
            let sp = speed_line("", 1024.0, 512.0, width);
            assert_eq!(s.find("eta:"), sp.find("speed:"));
            assert_eq!(s.find("current"), sp.find("current"));
        }
        // words and the closing '|' never move whatever the values
        let cases: [(f64, f64, u64); 6] = [
            (0.0, 0.0, 0),
            (0.0, 0.0, 10),
            (1.0, 1.0, 1),
            (1e9, 1e3, 1 << 40),
            (3.5, 7e6, 1 << 50),
            (1.0, 1.0, u64::MAX),
        ];
        for label in ["eta:   ", ""] {
            let key = |l: &str| {
                (
                    l.find("current").unwrap(),
                    l.find("average").unwrap(),
                    l.rfind('|').unwrap(),
                    l.len(),
                )
            };
            let first = key(&eta_line(label, 1.0, 1.0, 1, 80));
            for &(c, a, r) in &cases {
                assert_eq!(key(&eta_line(label, c, a, r, 80)), first);
            }
        }
    }

    #[test]
    fn eta_tracks_remaining_bytes() {
        let mb = 1u64 << 20;
        let t0 = Instant::now();
        let mut p = Progress::new(1, 100 * mb);
        p.t0 = t0;
        p.samples.clear();
        p.record(t0);
        p.file_start("big.bin", 100 * mb);
        assert_eq!(p.remaining(), 100 * mb);
        // nothing measured yet -> unknown at both rates
        let (cur, avg) = p.speeds(t0);
        assert_eq!(eta_str(p.remaining(), cur), format!("{:>19}", "unknown"));
        assert_eq!(eta_str(p.remaining(), avg), format!("{:>19}", "unknown"));
        // 40 MB in 4 s at 10 MB/s -> 60 MB left -> 6s at either rate
        let mut t = t0;
        for _ in 0..40 {
            t += Duration::from_millis(100);
            p.add(mb);
            p.record(t);
        }
        let (cur, avg) = p.speeds(t);
        assert_eq!(p.remaining(), 60 * mb);
        assert_eq!(eta_str(p.remaining(), avg), format!("{:>19}", "6s"));
        assert_eq!(eta_str(p.remaining(), cur), format!("{:>19}", "6s"));
        // slow down to 1 MB/s for 3 s: current says 57s, average blends
        let tenth = 104_858; // 1 MB / 10, rounded UP so nothing is left over
        for _ in 0..30 {
            t += Duration::from_millis(100);
            p.add(tenth);
            p.record(t);
        }
        let (cur, avg) = p.speeds(t);
        let left = 100 * mb - 40 * mb - 30 * tenth; // 57 MB - 12 B
        assert_eq!(p.remaining(), left);
        // 56.99999 s at ~1.000002 MB/s -> ceil -> 57s
        assert_eq!(eta_str(p.remaining(), cur), format!("{:>19}", "57s"));
        // avg = 43 MB / 7 s = 6.14 MB/s -> 57/6.14 = 9.28 -> 10s
        assert_eq!(eta_str(p.remaining(), avg), format!("{:>19}", "10s"));
        // file_end() accounts the rest of the file: remaining hits zero
        // regardless of rate
        p.file_end();
        assert_eq!(p.remaining(), 0);
        assert_eq!(eta_str(p.remaining(), cur), format!("{:>19}", "done"));
    }
}
