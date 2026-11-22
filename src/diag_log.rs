//! Small on-card diagnostic log, readable with VitaShell at
//! `ux0:data/green-vita/logs/greenvita.log` (the previous run is kept as `greenvita.old.log`).
//!
//! Every line is also echoed to stderr. Writing stops once the file reaches `MAX_LOG_BYTES`
//! so a long session cannot fill the memory card.

use std::fmt::Display;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const LOG_DIR: &str = "ux0:data/green-vita/logs";
const LOG_PATH: &str = "ux0:data/green-vita/logs/greenvita.log";
const OLD_LOG_PATH: &str = "ux0:data/green-vita/logs/greenvita.old.log";
const MAX_LOG_BYTES: u64 = 512 * 1024;

struct LogFile {
    file: Option<File>,
    written: u64,
}

static LOG: Mutex<Option<LogFile>> = Mutex::new(None);
static STARTED_AT: OnceLock<Instant> = OnceLock::new();

fn open_log() -> LogFile {
    let _ = std::fs::create_dir_all(LOG_DIR);
    // Keep exactly one previous run so a crash report is not overwritten by the next launch.
    let _ = std::fs::remove_file(OLD_LOG_PATH);
    let _ = std::fs::rename(LOG_PATH, OLD_LOG_PATH);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(LOG_PATH)
        .ok();
    LogFile { file, written: 0 }
}

/// Appends one line, tagged with the seconds since launch and a short category.
pub fn log(category: &str, message: impl Display) {
    let started_at = *STARTED_AT.get_or_init(Instant::now);
    let elapsed = started_at.elapsed();
    let line = format!(
        "[{:>5}.{:03}] [{category}] {message}\n",
        elapsed.as_secs(),
        elapsed.subsec_millis()
    );
    eprint!("{line}");

    let Ok(mut guard) = LOG.lock() else {
        return;
    };
    let log = guard.get_or_insert_with(open_log);
    if log.written >= MAX_LOG_BYTES {
        return;
    }
    let Some(file) = log.file.as_mut() else {
        return;
    };
    if file.write_all(line.as_bytes()).is_ok() {
        let _ = file.flush();
        log.written += line.len() as u64;
        if log.written >= MAX_LOG_BYTES {
            let _ = file.write_all(b"[log size limit reached, further lines skipped]\n");
            let _ = file.flush();
        }
    }
}
