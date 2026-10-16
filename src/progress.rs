use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

static COLOR_OVERRIDE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) const MAX_CONCURRENT_S3_COMPARISONS: usize = 8;

const PROGRESS_TEMPLATE: &str = "{prefix} [{bar:32.cyan/blue}] {pos}/{len} ({percent}%) {msg}";

pub(crate) struct ProgressReporter {
    total: usize,
    completed: AtomicUsize,
    prefix: String,
    bar: Option<ProgressBar>,
    active: AtomicUsize,
    line_mode: bool,
}

impl ProgressReporter {
    pub(crate) fn new(
        total: usize,
        prefix: &str,
        initial_message: &str,
        line_mode: bool,
        color: bool,
    ) -> Option<Self> {
        if total == 0 {
            return None;
        }

        let line_mode = line_mode || !std::io::stderr().is_terminal();
        let bar = (!line_mode).then(|| {
            COLOR_OVERRIDE.store(color, Ordering::Relaxed);
            colored::control::set_override(color);
            let bar =
                ProgressBar::with_draw_target(Some(total as u64), ProgressDrawTarget::stderr());
            let style = ProgressStyle::with_template(PROGRESS_TEMPLATE)
                .expect("static progress-bar template is valid")
                .progress_chars(if color { "=>-" } else { "===" });
            bar.set_style(style);
            bar.set_prefix(format!("tfdiff: {prefix}"));
            bar.set_message(initial_message.to_string());
            bar
        });

        if bar.is_none() {
            let _ = writeln!(std::io::stderr(), "tfdiff: {initial_message}");
        }

        Some(Self {
            total,
            completed: AtomicUsize::new(0),
            prefix: prefix.to_string(),
            bar,
            active: AtomicUsize::new(0),
            line_mode,
        })
    }

    pub(crate) fn start(&self, detail: &str) {
        let active = self
            .active
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
                active.checked_add(1)
            })
            .map(|active| active + 1)
            .unwrap_or(usize::MAX);
        if let Some(bar) = &self.bar {
            bar.set_message(format!("{active} active · {detail}"));
        } else {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "tfdiff: {}: {detail}", self.prefix);
            if COLOR_OVERRIDE.load(Ordering::Relaxed) {
                let _ = stderr.flush();
            }
        }
    }

    pub(crate) fn complete(&self, detail: &str) {
        let completed = self
            .completed
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |completed| {
                (completed < self.total).then_some(completed + 1)
            })
            .unwrap_or_else(|completed| completed);
        let active = self
            .active
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
                active.checked_sub(1)
            })
            .map(|active| active - 1)
            .unwrap_or(0);
        if let Some(bar) = &self.bar {
            bar.set_position(completed as u64);
            bar.set_message(format!("{active} active · {detail}"));
        } else {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(
                stderr,
                "tfdiff: {}: {}/{} {detail}",
                self.prefix, completed, self.total
            );
            if COLOR_OVERRIDE.load(Ordering::Relaxed) {
                let _ = stderr.flush();
            }
        }
    }

    pub(crate) fn finish(self, message: &str) {
        if let Some(bar) = self.bar {
            bar.finish_with_message(message.to_string());
        } else if self.line_mode {
            let _ = writeln!(std::io::stderr(), "tfdiff: {}: {message}", self.prefix);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reporter_tracks_bounded_completion_and_finishes_terminal_bar() {
        let reporter = ProgressReporter {
            total: 2,
            completed: AtomicUsize::new(0),
            prefix: "test".into(),
            bar: Some(ProgressBar::hidden()),
            active: AtomicUsize::new(0),
            line_mode: true,
        };
        reporter.start("first item");
        reporter.complete("items processed");
        assert_eq!(reporter.completed.load(Ordering::Relaxed), 1);
        reporter.complete("items processed");
        reporter.complete("items processed");
        assert_eq!(reporter.completed.load(Ordering::Relaxed), 2);
        assert_eq!(reporter.active.load(Ordering::Relaxed), 0);
        reporter.finish("complete");
    }

    #[test]
    fn terminal_style_displays_percentage() {
        assert!(PROGRESS_TEMPLATE.contains("{percent}%"));
        let bar = ProgressBar::hidden();
        bar.set_style(ProgressStyle::with_template(PROGRESS_TEMPLATE).unwrap());
        bar.set_length(4);
        bar.set_position(3);
        assert_eq!(bar.position(), 3);
        bar.finish_and_clear();
    }
}
