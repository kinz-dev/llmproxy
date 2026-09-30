use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Context, Result, bail};
use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::{
    EnvFilter, Layer, filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt,
};

const LOG_PREFIX: &str = "llmproxy-";
const LOG_SUFFIX: &str = ".log";
const TIMESTAMP_LEN: usize = 14;
const LOG_QUEUE_LINES: usize = 256;
const ROTATION_CHECK: Duration = Duration::from_secs(60);
const SECONDS_PER_DAY: u64 = 86_400;

pub(crate) struct LoggingGuard {
    _worker: Option<WorkerGuard>,
    current_name: Option<Arc<Mutex<String>>>,
}

impl LoggingGuard {
    pub(crate) fn file_name(&self) -> Option<String> {
        self.current_name.as_ref().map(|name| {
            name.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        })
    }
}

pub(crate) fn init(
    file_logging: bool,
    home: Option<&Path>,
    retention_days: u64,
) -> Result<LoggingGuard> {
    let console_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "llmproxy=info,tower_http=info".into());
    let console = tracing_subscriber::fmt::layer().with_filter(console_filter);

    if !file_logging {
        tracing_subscriber::registry()
            .with(console)
            .try_init()
            .context("failed to initialize console logging")?;
        return Ok(LoggingGuard {
            _worker: None,
            current_name: None,
        });
    }

    let home = home.context("cannot create ~/.llmproxy log without a home directory")?;
    let directory = home.join(".llmproxy");
    fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create log directory {}", directory.display()))?;
    let (initial_file, initial_name) = create_instance_log(&directory)?;
    let current_name = Arc::new(Mutex::new(initial_name.clone()));
    let rotator = DailyRotatingFile::new(
        initial_file,
        directory.clone(),
        retention_days,
        Arc::clone(&current_name),
    )?;
    let (writer, worker) = NonBlockingBuilder::default()
        .buffered_lines_limit(LOG_QUEUE_LINES)
        .lossy(false)
        .thread_name("llmproxy-log")
        .finish(rotator);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(writer)
        .with_filter(LevelFilter::TRACE);

    tracing_subscriber::registry()
        .with(console)
        .with(file_layer)
        .try_init()
        .context("failed to initialize file logging")?;
    let removed = match remove_expired_logs(&directory, retention_days) {
        Ok(removed) => removed,
        Err(cleanup_error) => {
            tracing::error!(
                error = %cleanup_error,
                retention_days,
                "failed to complete diagnostic log cleanup"
            );
            0
        }
    };
    tracing::info!(
        log_file = %initial_name,
        retention_days,
        expired_logs_removed = removed,
        queue_lines = LOG_QUEUE_LINES,
        lossy = false,
        rotate_daily = true,
        "diagnostic file logging initialized"
    );

    Ok(LoggingGuard {
        _worker: Some(worker),
        current_name: Some(current_name),
    })
}

/// A `Write` sink that rolls the active instance log over at each UTC calendar
/// day boundary and prunes expired logs on every rotation, so a long-running
/// daemon never grows a single file past one day and never keeps more than
/// `retention_days` of history. The rotation check runs on the non-blocking
/// worker thread, never the request path, and never re-enters `tracing`.
struct DailyRotatingFile {
    directory: PathBuf,
    retention_days: u64,
    current_name: Arc<Mutex<String>>,
    file: File,
    day: u64,
    next_check: Instant,
}

impl DailyRotatingFile {
    fn new(
        file: File,
        directory: PathBuf,
        retention_days: u64,
        current_name: Arc<Mutex<String>>,
    ) -> Result<Self> {
        let day = utc_day_number(SystemTime::now())?;
        Ok(Self {
            directory,
            retention_days,
            current_name,
            file,
            day,
            next_check: Instant::now() + ROTATION_CHECK,
        })
    }

    fn rotate_if_needed(&mut self) -> io::Result<()> {
        let now = Instant::now();
        if now < self.next_check {
            return Ok(());
        }
        self.next_check = now + ROTATION_CHECK;
        let now_day = utc_day_number(SystemTime::now())
            .map_err(|error| io::Error::other(error.to_string()))?;
        if now_day == self.day {
            return Ok(());
        }
        let _ = self.file.flush();
        match self.open_rotated_log() {
            Ok((new_file, new_name, removed)) => {
                self.file = new_file;
                self.day = now_day;
                *self
                    .current_name
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = new_name.clone();
                let marker = format!(
                    "{LOG_PREFIX}rotated to {new_name} on day {now_day}, removed {removed} expired logs\n"
                );
                self.file.write_all(marker.as_bytes())
            }
            Err(error) => {
                let marker = format!("{LOG_PREFIX}daily rotation failed: {error}\n");
                let _ = self.file.write_all(marker.as_bytes());
                Err(error)
            }
        }
    }

    fn open_rotated_log(&mut self) -> io::Result<(File, String, usize)> {
        let (new_file, new_name) = create_instance_log(&self.directory)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let removed = remove_expired_logs(&self.directory, self.retention_days).unwrap_or(0);
        Ok((new_file, new_name, removed))
    }
}

impl io::Write for DailyRotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rotate_if_needed()?;
        self.file.write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.rotate_if_needed()?;
        self.file.write_all(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn utc_day_number(time: SystemTime) -> Result<u64> {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    Ok(seconds / SECONDS_PER_DAY)
}

fn create_instance_log(directory: &Path) -> Result<(File, String)> {
    for attempt in 0..5 {
        if attempt > 0 {
            thread::sleep(Duration::from_secs(1));
        }
        let timestamp = compact_utc_timestamp(SystemTime::now())?;
        let file_name = format!("{LOG_PREFIX}{timestamp}{LOG_SUFFIX}");
        let path = directory.join(&file_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
            Ok(file) => return Ok((file, file_name)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create log file {}", path.display()));
            }
        }
    }
    bail!("failed to allocate a unique timestamped llmproxy log file")
}

fn remove_expired_logs(directory: &Path, retention_days: u64) -> Result<usize> {
    if retention_days == 0 {
        return Ok(0);
    }
    let retention = Duration::from_secs(retention_days.saturating_mul(24 * 60 * 60));
    let cutoff = SystemTime::now()
        .checked_sub(retention)
        .unwrap_or(UNIX_EPOCH);
    let mut removed = 0;

    for entry in fs::read_dir(directory)
        .with_context(|| format!("failed to scan log directory {}", directory.display()))?
    {
        let entry = entry.with_context(|| {
            format!(
                "failed to read an entry in log directory {}",
                directory.display()
            )
        })?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !is_instance_log_name(file_name) || !entry.file_type()?.is_file() {
            continue;
        }
        let modified = entry.metadata()?.modified().unwrap_or(SystemTime::now());
        if modified < cutoff {
            fs::remove_file(entry.path()).with_context(|| {
                format!("failed to remove expired log {}", entry.path().display())
            })?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn is_instance_log_name(name: &str) -> bool {
    name.len() == LOG_PREFIX.len() + TIMESTAMP_LEN + LOG_SUFFIX.len()
        && name.starts_with(LOG_PREFIX)
        && name.ends_with(LOG_SUFFIX)
        && name[LOG_PREFIX.len()..LOG_PREFIX.len() + TIMESTAMP_LEN]
            .bytes()
            .all(|byte| byte.is_ascii_digit())
}

fn compact_utc_timestamp(time: SystemTime) -> Result<String> {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    Ok(format!(
        "{year:04}{month:02}{day:02}{hour:02}{minute:02}{second:02}"
    ))
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u64, u64) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u64, day as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn timestamp_and_log_name_are_stable() {
        let timestamp = compact_utc_timestamp(UNIX_EPOCH + Duration::from_secs(1_704_067_199))
            .expect("timestamp");
        assert_eq!(timestamp, "20231231235959");
        assert!(is_instance_log_name("llmproxy-20231231235959.log"));
        assert!(!is_instance_log_name("llmproxy-current.log"));
    }

    #[test]
    fn retention_removes_only_expired_instance_logs() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "llmproxy-log-retention-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("test directory");
        let expired = directory.join("llmproxy-20200101000000.log");
        let current = directory.join("llmproxy-20990101000000.log");
        let unrelated = directory.join("application.log");
        fs::write(&expired, b"old").expect("expired log");
        fs::write(&current, b"current").expect("current log");
        fs::write(&unrelated, b"unrelated").expect("unrelated log");
        File::options()
            .write(true)
            .open(&expired)
            .expect("open expired log")
            .set_modified(SystemTime::now() - Duration::from_secs(8 * 86_400))
            .expect("age expired log");

        assert_eq!(remove_expired_logs(&directory, 7).expect("cleanup"), 1);
        assert!(!expired.exists());
        assert!(current.exists());
        assert!(unrelated.exists());
        assert_eq!(remove_expired_logs(&directory, 0).expect("disabled"), 0);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    #[cfg(unix)]
    fn instance_logs_are_owner_only() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "llmproxy-log-permissions-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("test directory");
        let (file, file_name) = create_instance_log(&directory).expect("instance log");
        drop(file);
        let path = directory.join(file_name);
        let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_file(path).expect("remove log");
        fs::remove_dir(directory).expect("remove directory");
    }

    #[test]
    fn daily_rotation_opens_a_new_file_and_prunes_expired() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "llmproxy-log-rotation-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("test directory");

        let expired = directory.join("llmproxy-20200101000000.log");
        fs::write(&expired, b"old").expect("expired log");
        File::options()
            .write(true)
            .open(&expired)
            .expect("open expired log")
            .set_modified(SystemTime::now() - Duration::from_secs(8 * 86_400))
            .expect("age expired log");

        let (file, initial_name) = create_instance_log(&directory).expect("initial log");
        let current_name = Arc::new(Mutex::new(initial_name.clone()));
        let yesterday = utc_day_number(SystemTime::now()).expect("day") - 1;
        let mut rotator = DailyRotatingFile {
            directory: directory.clone(),
            retention_days: 7,
            current_name: Arc::clone(&current_name),
            file,
            day: yesterday,
            next_check: Instant::now() - ROTATION_CHECK,
        };
        rotator.write_all(b"hello\n").expect("write rotates");

        assert!(!expired.exists(), "expired log pruned on rotation");
        let active = current_name.lock().expect("name").clone();
        assert_ne!(active, initial_name, "a new file became active");
        assert!(is_instance_log_name(&active));
        assert!(
            directory.join(&active).exists(),
            "active file exists on disk"
        );
        assert!(
            directory.join(&initial_name).exists(),
            "rotated-away file is retained"
        );

        fs::remove_dir_all(&directory).expect("remove test directory");
    }
}
