use backtrace::Backtrace;
use log::warn;
use rustc_span::{Span, DUMMY_SP};
use std::any::Any;
use std::cell::Cell;
use std::fmt::Write as _;
use std::panic::{self, PanicInfo, UnwindSafe};

/// Detailed information about a panic.
#[derive(Clone, Debug)]
pub struct PanicDetail {
    msg: String,
    loc: Option<String>,
    relevant_loc: Option<String>,
    backtrace: Option<Backtrace>,
    span: Span,
}

impl PanicDetail {
    /// Create a new `PanicDetail` containing only a message, with no location or backtrace
    /// information.
    pub fn new(msg: String) -> PanicDetail {
        PanicDetail {
            msg,
            loc: None,
            relevant_loc: None,
            backtrace: None,
            span: DUMMY_SP,
        }
    }

    /// Returns `true` if this `PanicDetail` contains a backtrace.
    pub fn has_backtrace(&self) -> bool {
        self.backtrace.is_some()
    }

    /// Return a short (usually one-line) description of this panic.
    pub fn to_string_short(&self) -> String {
        let loc_str = self
            .relevant_loc
            .as_ref()
            .or(self.loc.as_ref())
            .map_or("[unknown]", |s| &*s);
        format!("{}: {}", loc_str, self.msg.trim())
    }

    /// Return a full description of this panic, including a complete backtrace if available.
    pub fn to_string_full(&self) -> String {
        let mut s = String::new();
        let loc_str = self.loc.as_ref().map_or("[unknown]", |s| &*s);
        writeln!(s, "panic at {}: {}", loc_str, self.msg).unwrap();
        if let Some(ref relevant_loc) = self.relevant_loc {
            writeln!(s, "related location: {}", relevant_loc).unwrap();
        }
        if !self.span.is_dummy() {
            writeln!(s, "source location: {:?}", self.span).unwrap();
        }
        if let Some(ref bt) = self.backtrace {
            writeln!(s, "{:?}", bt).unwrap();
        }
        s
    }
}

thread_local! {
    static CURRENT_PANIC_DETAIL: Cell<Option<PanicDetail>> = Cell::new(None);
}

/// Panic hook for use with [`std::panic::set_hook`].  This builds a `PanicDetail` for each panic
/// and stores it for later retrieval by [`take_current`].
pub fn panic_hook(info: &PanicInfo) {
    let bt = Backtrace::new();
    let detail = PanicDetail {
        msg: panic_to_string(info.payload()),
        loc: info.location().map(|l| l.to_string()),
        relevant_loc: guess_relevant_loc(&bt),
        backtrace: Some(bt),
        span: CURRENT_SPAN.with(|cell| cell.get()),
    };
    let old = CURRENT_PANIC_DETAIL.with(|cell| cell.replace(Some(detail)));
    if let Some(old) = old {
        warn!("discarding old panic detail: {:?}", old);
    }
}

/// Get the [`PanicDetail`] of the most recent panic.  This clears the internal storage, so if this
/// is called twice in a row without an intervening panic, the second call always returns `None`.
fn take_current() -> Option<PanicDetail> {
    CURRENT_PANIC_DETAIL.with(|cell| cell.take())
}

/// Like `std::panic::catch_unwind`, but returns a `PanicDetail` instead of `Box<dyn Any>` on
/// panic.
pub fn catch_unwind<F: FnOnce() -> R + UnwindSafe, R>(f: F) -> Result<R, PanicDetail> {
    panic::catch_unwind(f).map_err(|e| {
        take_current().unwrap_or_else(|| {
            let msg = panic_to_string(&e);
            warn!("missing panic detail; caught message {:?}", msg);
            PanicDetail::new(msg)
        })
    })
}

/// Crude heuristic to guess the first interesting location in a [`Backtrace`], skipping over
/// helper functions, wrappers, and panic machinery.  The resulting location is used in the summary
/// message produced by [`PanicDetail::to_string_short`].
fn guess_relevant_loc(bt: &Backtrace) -> Option<String> {
    for frame in bt.frames() {
        for symbol in frame.symbols() {
            let name = match symbol.name() {
                Some(x) => x.to_string(),
                None => continue,
            };
            if name.starts_with("c2rust_analyze::dataflow")
                || name.starts_with("c2rust_analyze::borrowck")
                || name.starts_with("c2rust_analyze::rewrite")
                || name.contains("type_of_rvalue")
                || name.contains("TypeOf")
                || name.contains("lty_project")
            {
                let filename_str = match symbol.filename() {
                    Some(x) => x.display().to_string(),
                    None => "[unknown]".to_string(),
                };
                return Some(format!(
                    "{} @ {}:{}:{}",
                    name,
                    filename_str,
                    symbol.lineno().unwrap_or(0),
                    symbol.colno().unwrap_or(0)
                ));
            }
        }
    }
    None
}

fn panic_to_string(e: &(dyn Any + Send + 'static)) -> String {
    if let Some(s) = e.downcast_ref::<&'static str>() {
        return s.to_string();
    }

    if let Some(s) = e.downcast_ref::<String>() {
        return (*s).clone();
    }

    format!("unknown error: {:?}", e.type_id())
}

thread_local! {
    static CURRENT_SPAN: Cell<Span> = Cell::new(DUMMY_SP);
}

pub struct CurrentSpanGuard {
    old: Span,
}

impl Drop for CurrentSpanGuard {
    fn drop(&mut self) {
        CURRENT_SPAN.with(|cell| cell.set(self.old));
    }
}

/// Set the current span.  Returns a guard that will reset the current span to its previous value
/// on drop.
pub fn set_current_span(span: Span) -> CurrentSpanGuard {
    let old = CURRENT_SPAN.with(|cell| cell.replace(span));
    CurrentSpanGuard { old }
}
