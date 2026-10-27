//! Logging to stderr, a file, and/or syslog, through `tracing`.
//!
//! - stderr: always on, human format, filtered by `RUST_LOG` (default
//!   `info`, or `debug` with `--debug`).
//! - file: `--log-file PATH`, appended, no ANSI colours.
//! - syslog: `--syslog`, via `openlog(3)`/`syslog(3)`; on macOS these lines
//!   land in the unified log (`log stream --predicate 'process == "gliff-mac"'`).

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Layer};

//= Version of the logging setup.
pub const LOGGING_VERSION: &str = "0.1.0";

/// Where logs go.
#[derive(Clone, Debug, Default)]
pub struct LogOptions {
    pub debug: bool,
    pub file: Option<std::path::PathBuf>,
    pub syslog: bool,
}

/// A shared, appending log file.
#[derive(Clone)]
struct FileWriter(Arc<Mutex<std::fs::File>>);

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

impl<'a> MakeWriter<'a> for FileWriter {
    type Writer = FileWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// One formatted event per `write`, forwarded to `syslog(3)`.
#[derive(Clone, Copy)]
struct SyslogWriter;

impl Write for SyslogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let line = String::from_utf8_lossy(buf);
        let line = line.trim_end();
        if !line.is_empty() {
            //= Interior NULs cannot cross into C; replace rather than drop.
            let text = CString::new(line.replace('\0', "\u{FFFD}"))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            // SAFETY: "%s" with one valid NUL-terminated argument.
            unsafe {
                libc::syslog(libc::LOG_NOTICE, c"%s".as_ptr(), text.as_ptr());
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for SyslogWriter {
    type Writer = SyslogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        *self
    }
}

fn open_log_file(path: &Path) -> io::Result<std::fs::File> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Install the global subscriber. Returns a note for anything that could not
/// be set up (the caller logs it once logging works).
pub fn init(opts: &LogOptions) -> Option<String> {
    let default = if opts.debug { "debug" } else { "info" };
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let mut note = None;

    let stderr = fmt::layer()
        .with_writer(io::stderr)
        .with_target(false)
        .with_filter(filter());

    let file = opts.file.as_deref().and_then(|p| match open_log_file(p) {
        Ok(f) => Some(
            fmt::layer()
                .with_ansi(false)
                .with_writer(FileWriter(Arc::new(Mutex::new(f))))
                .with_filter(filter()),
        ),
        Err(e) => {
            note = Some(format!("cannot open log file {}: {e}", p.display()));
            None
        }
    });

    let syslog = opts.syslog.then(|| {
        // SAFETY: the ident is a 'static C string; openlog keeps the pointer.
        unsafe {
            libc::openlog(c"gliff-mac".as_ptr(), libc::LOG_PID, libc::LOG_USER);
        }
        fmt::layer()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(SyslogWriter)
            .with_filter(filter())
    });

    let installed = tracing_subscriber::registry()
        .with(stderr)
        .with(file)
        .with(syslog)
        .try_init();
    if let Err(e) = installed {
        note = Some(format!("logging already initialised: {e}"));
    }
    note
}
