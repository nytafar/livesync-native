//! The `log` to `tracing` bridge, with one change: library warnings that are
//! known to be harmless are passed on at debug level.
//!
//! - `notify` (inotify) warns "Unable to remove the path from the watches:
//!   WatchNotFound" when a watched folder is deleted: the parent's `DELETE`
//!   and the folder's own `DELETE_SELF` both queue the same removal, and the
//!   second finds the watch gone. Nothing is lost.
//!
//! Install it after the global `tracing` subscriber, in place of the bridge
//! `tracing_subscriber`'s `init()` would set up.

use log::{Level, Log, Metadata, Record};
use tracing_log::LogTracer;

struct Bridge(LogTracer);

/// The level a `log` record is passed on at.
fn level_for(r: &Record<'_>) -> Level {
    if r.level() == Level::Warn
        && r.target().starts_with("notify")
        && r.args()
            .to_string()
            .starts_with("Unable to remove the path from the watches")
    {
        return Level::Debug;
    }
    r.level()
}

impl Log for Bridge {
    fn enabled(&self, m: &Metadata<'_>) -> bool {
        self.0.enabled(m)
    }

    fn log(&self, r: &Record<'_>) {
        let level = level_for(r);
        if level == r.level() {
            return self.0.log(r);
        }
        let mut b = Record::builder();
        b.level(level)
            .target(r.target())
            .args(*r.args())
            .module_path(r.module_path())
            .file(r.file())
            .line(r.line());
        self.0.log(&b.build());
    }

    fn flush(&self) {}
}

/// Route `log` records into `tracing`, up to the current subscriber's
/// maximum level. Fails if a `log` logger is already installed.
pub fn init() -> Result<(), log::SetLoggerError> {
    let max = tracing::level_filters::LevelFilter::current();
    let max = match max.into_level() {
        None => log::LevelFilter::Off,
        Some(tracing::Level::ERROR) => log::LevelFilter::Error,
        Some(tracing::Level::WARN) => log::LevelFilter::Warn,
        Some(tracing::Level::INFO) => log::LevelFilter::Info,
        Some(tracing::Level::DEBUG) => log::LevelFilter::Debug,
        Some(tracing::Level::TRACE) => log::LevelFilter::Trace,
    };
    log::set_boxed_logger(Box::new(Bridge(LogTracer::new())))?;
    log::set_max_level(max);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(level: Level, target: &str, args: std::fmt::Arguments<'_>) -> Level {
        level_for(&Record::builder().level(level).target(target).args(args).build())
    }

    #[test]
    fn only_the_known_watch_removal_warning_is_downgraded() {
        let err = "Error { kind: WatchNotFound, paths: [] }";
        assert_eq!(
            rec(
                Level::Warn,
                "notify::inotify",
                format_args!("Unable to remove the path from the watches: {err}")
            ),
            Level::Debug
        );
        assert_eq!(
            rec(Level::Warn, "notify::inotify", format_args!("something else")),
            Level::Warn
        );
        assert_eq!(
            rec(
                Level::Warn,
                "other",
                format_args!("Unable to remove the path from the watches: {err}")
            ),
            Level::Warn
        );
        assert_eq!(
            rec(
                Level::Error,
                "notify::inotify",
                format_args!("Unable to remove the path from the watches: {err}")
            ),
            Level::Error
        );
    }
}
