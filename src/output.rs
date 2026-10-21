//! Process-wide output mode. Streaming (the default) prints step lines and
//! lets child processes write straight to the terminal; batch mode — several
//! parallel workers on a TTY — suppresses both so the summary stays readable,
//! and shows a single live counter line instead.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

static STREAMING: AtomicBool = AtomicBool::new(true);
static PROMPTING: AtomicBool = AtomicBool::new(false);

pub fn set_streaming(on: bool) {
    STREAMING.store(on, Ordering::Relaxed);
}

pub fn streaming() -> bool {
    STREAMING.load(Ordering::Relaxed)
}

/// Batch mode only: redraws the live `done/total` line in place.
pub fn counter(done: usize, total: usize) {
    if !streaming() && !PROMPTING.load(Ordering::Relaxed) {
        eprint!("{}rig: {done}/{total} done", clear_seq());
        let _ = std::io::stderr().flush();
    }
}

/// Asks on stderr, beside the counter; the counter holds still until answered
/// so a redraw can't erase the question.
pub fn prompt(question: &str) -> std::io::Result<String> {
    PROMPTING.store(true, Ordering::Relaxed);
    eprint!("{}{question}", clear_seq());
    let mut line = String::new();
    let read = std::io::stderr()
        .flush()
        .and_then(|()| std::io::stdin().read_line(&mut line));
    PROMPTING.store(false, Ordering::Relaxed);
    read.map(|_| line)
}

/// Batch mode only: erases the live counter line before other output.
pub fn clear_line() {
    eprint!("{}", clear_seq());
}

/// The erase sequence, empty while streaming; `warning!` inlines it so the
/// erase and the message land in one write.
pub fn clear_seq() -> &'static str {
    if streaming() { "" } else { "\r\x1b[K" }
}

/// A step line (`foo: updating …`), printed only while streaming.
macro_rules! progress {
    ($($arg:tt)*) => {
        if $crate::output::streaming() {
            eprintln!($($arg)*);
        }
    };
}

/// Always printed; clears the batch counter first so the two don't merge.
macro_rules! warning {
    ($($arg:tt)*) => {
        eprintln!("{}{}", $crate::output::clear_seq(), format_args!($($arg)*))
    };
}

pub(crate) use {progress, warning};
