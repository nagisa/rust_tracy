//! Collect [Tracy] profiles in tracing-enabled applications.
//!
//! Assuming the application is well instrumented, this should in practice be a very low effort way
//! to gain great amounts of insight into an application performance.
//!
//! Note, however that Tracy is ultimately a profiling, not an observability, tool. As thus, some
//! of tracing concepts cannot be represented well by Tracy. For instance, out-of-order span
//! entries and exits, are not supported, and neither are spans that are entered and exited on
//! different threads. This crate will attempt to mitigate the problems and retain trace validity
//! at the cost of potentially invalid data. When such a mitigation occurs, trace will contain a
//! message with a note about the problem.
//!
//! Some other caveats to keep in mind:
//!
//! * Only span entries and exits are recorded;
//! * Events show up as messages in Tracy, however Tracy can struggle with large numbers of
//! messages;
//! * Some additional functionality such as plotting and memory allocation profiling is only
//! available as part of the [tracy-client](client) crate.
//!
//! # Examples
//!
//! The most basic way to setup the tracy subscriber globally is as follows:
//!
//! ```rust
//! use tracing_subscriber::layer::SubscriberExt;
//!
//! tracing::subscriber::set_global_default(
//!     tracing_subscriber::registry()
//!         .with(tracing_tracy::TracyLayer::new()),
//! ).expect("set up the subscriber");
//! ```
//!
//! # Important note
//!
//! Depending on the configuration Tracy may broadcast discovery packets to the local network and
//! expose the data it collects in the background to that same network. Traces collected by Tracy
//! may include source and assembly code as well.
//!
//! As thus, you may want make sure to only enable the `tracing-tracy` crate conditionally, via the
//! `enable` feature flag provided by this crate.
//!
//! [Tracy]: https://github.com/wolfpld/tracy
//!
//! # Features
//!
//! The following crate features are provided to customize the functionality of the Tracy client:
//!
#![doc = include_str!("../FEATURES.mkd")]
#![cfg_attr(tracing_tracy_docs, feature(doc_auto_cfg))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::{borrow::Cow, fmt::Write, mem};
use tracing_core::{
    field::{Field, Visit},
    span::{Attributes, Id, Record},
    Event, Subscriber,
};
use tracing_subscriber::fmt::format::{DefaultFields, FormatFields};
use tracing_subscriber::{
    fmt::FormattedFields,
    layer::{Context, Layer},
    registry,
};

use client::{Client, Span};
use utils::{StrCache, StrCacheGuard, VecCell};

pub use client;

thread_local! {
    /// A stack of spans currently active on the current thread.
    static TRACY_SPAN_STACK: VecCell<(Span, u64)> = const { VecCell::new() };
}

/// A tracing layer that collects data in Tracy profiling format.
#[derive(Clone)]
pub struct TracyLayer<F = DefaultFields> {
    fmt: F,
    stack_depth: u16,
    client: Client,
}

impl TracyLayer<DefaultFields> {
    /// Create a new `TracyLayer`.
    ///
    /// Defaults to collecting stack traces.
    #[must_use]
    pub fn new() -> Self {
        Self {
            fmt: DefaultFields::default(),
            stack_depth: 0,
            client: Client::start(),
        }
    }
}

impl<F> TracyLayer<F> {
    /// Specify the maximum number of stack frames that will be collected.
    ///
    /// Note that enabling callstack collection can and will introduce a non-trivial overhead at
    /// every instrumentation point. Specifying 0 frames (which is the default) will disable stack
    /// trace collection.
    #[must_use]
    pub const fn with_stackdepth(mut self, stack_depth: u16) -> Self {
        self.stack_depth = stack_depth;
        self
    }

    /// Use a custom field formatting implementation.
    #[must_use]
    pub fn with_formatter<Fmt>(self, fmt: Fmt) -> TracyLayer<Fmt> {
        TracyLayer {
            fmt,
            stack_depth: self.stack_depth,
            client: self.client,
        }
    }

    fn truncate_to_length<'d>(
        &self,
        data: &'d str,
        file: &str,
        function: &str,
        error_msg: &'static str,
    ) -> &'d str {
        // From AllocSourceLocation
        let mut max_len = usize::from(u16::MAX) - 2 - 4 - 4 - function.len() - 1 - file.len() - 1;
        if data.len() >= max_len {
            while !data.is_char_boundary(max_len) {
                max_len -= 1;
            }
            self.client
                .color_message(error_msg, 0xFF000000, self.stack_depth);
            &data[..max_len]
        } else {
            data
        }
    }
}

impl Default for TracyLayer {
    fn default() -> Self {
        Self::new()
    }
}

static MAX_CACHE_SIZE: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Specify the maximum number of bytes used in thread local caches.
///
/// A value of zero disables the cache, while a value of [`usize::MAX`] denotes an unlimited
/// cache size.
///
/// Note: the upper bound on the cache size is respected on a best effort basis only. We make
/// no guarantees on the maximum memory used by tracing-tracy. Notably, changes to this value
/// are eventually consistent, i.e. caches are not flushed upon an update.
///
/// Defaults to [`usize::MAX`].
pub fn set_max_cache_size(max_bytes_used_per_thread: usize) {
    MAX_CACHE_SIZE.store(max_bytes_used_per_thread, Ordering::Relaxed);
}

thread_local! {
    static CACHE: StrCache = const { StrCache::new() };
}

impl<S, F> Layer<S> for TracyLayer<F>
where
    S: Subscriber + for<'a> registry::LookupSpan<'a>,
    F: for<'writer> FormatFields<'writer> + 'static,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };

        let mut extensions = span.extensions_mut();
        if extensions.get_mut::<FormattedFields<F>>().is_none() {
            let mut fields =
                FormattedFields::<F>::new(CACHE.with(|cache| cache.acquire().into_inner()));
            if self.fmt.format_fields(fields.as_writer(), attrs).is_ok() {
                extensions.insert(fields);
            }
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };

        let mut extensions = span.extensions_mut();
        if let Some(fields) = extensions.get_mut::<FormattedFields<F>>() {
            let _ = self.fmt.add_fields(fields, values);
        } else {
            let mut fields =
                FormattedFields::<F>::new(CACHE.with(|cache| cache.acquire().into_inner()));
            if self.fmt.format_fields(fields.as_writer(), values).is_ok() {
                extensions.insert(fields);
            }
        }
    }

    fn on_event(&self, event: &Event, _: Context<'_, S>) {
        CACHE.with(|cache| {
            let mut buf = cache.acquire();
            let mut visitor = TracyEventFieldVisitor {
                dest: &mut buf,
                first: true,
                frame_mark: false,
            };

            event.record(&mut visitor);
            if !visitor.first {
                self.client.message(
                    self.truncate_to_length(
                        visitor.dest,
                        "",
                        "",
                        "event message is too long and was truncated",
                    ),
                    self.stack_depth,
                );
            }
            if visitor.frame_mark {
                self.client.frame_mark();
            }
        });
    }

    fn on_enter(&self, id: &Id, ctx: Context<S>) {
        let Some(span) = ctx.span(id) else { return };

        let stack_frame = {
            let metadata = span.metadata();
            let name: Cow<str> = if let Some(fields) = span.extensions().get::<FormattedFields<F>>()
            {
                if fields.fields.is_empty() {
                    metadata.name().into()
                } else {
                    format!("{}{{{}}}", metadata.name(), fields.fields.as_str()).into()
                }
            } else {
                metadata.name().into()
            };

            let file = metadata.file().unwrap_or("<not available>");
            let line = metadata.line().unwrap_or(0);
            (
                self.client.clone().span_alloc(
                    Some(self.truncate_to_length(
                        &name,
                        file,
                        "",
                        "span information is too long and was truncated",
                    )),
                    "",
                    file,
                    line,
                    self.stack_depth,
                ),
                id.into_u64(),
            )
        };

        TRACY_SPAN_STACK.with(|s| {
            s.push(stack_frame);
        });
    }

    fn on_exit(&self, id: &Id, _: Context<S>) {
        let stack_frame = TRACY_SPAN_STACK.with(VecCell::pop);

        if let Some((span, span_id)) = stack_frame {
            if id.into_u64() != span_id {
                self.client.color_message(
                    "Tracing spans exited out of order! \
                        Trace may not be accurate for this span stack.",
                    0xFF000000,
                    self.stack_depth,
                );
            }
            drop(span);
        } else {
            self.client.color_message(
                "Exiting a tracing span, but got nothing on the tracy span stack!",
                0xFF000000,
                self.stack_depth,
            );
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };

        if let Some(fields) = span.extensions_mut().get_mut::<FormattedFields<F>>() {
            let buf = mem::take(&mut fields.fields);
            CACHE.with(|cache| drop(StrCacheGuard::new(cache, buf)));
        };
    }
}

struct TracyEventFieldVisitor<'a> {
    dest: &'a mut String,
    frame_mark: bool,
    first: bool,
}

impl Visit for TracyEventFieldVisitor<'_> {
    fn record_bool(&mut self, field: &Field, value: bool) {
        match (value, field.name()) {
            (true, "tracy.frame_mark") => self.frame_mark = true,
            _ => self.record_debug(field, &value),
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // FIXME: this is a very crude formatter, but we don’t have
        // an easy way to do anything better...
        if self.first {
            self.first = false;
        } else {
            self.dest.push_str(", ");
        }
        let _ = write!(&mut self.dest, "{} = {value:?}", field.name());
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
fn main() {
    if std::env::args_os().any(|p| p == std::ffi::OsStr::new("--bench")) {
        tests::bench();
    } else {
        tests::test();
    }
}

mod utils {
    use crate::MAX_CACHE_SIZE;
    use std::cell::{Cell, UnsafeCell};
    use std::mem;
    use std::mem::ManuallyDrop;
    use std::ops::{Deref, DerefMut};
    use std::sync::atomic::Ordering;

    pub struct VecCell<T>(UnsafeCell<Vec<T>>);

    impl<T> VecCell<T> {
        pub const fn new() -> Self {
            Self(UnsafeCell::new(Vec::new()))
        }

        pub fn push(&self, item: T) {
            unsafe { &mut *self.0.get() }.push(item);
        }

        pub fn pop(&self) -> Option<T> {
            unsafe { &mut *self.0.get() }.pop()
        }
    }

    pub struct StrCache {
        str_bufs: VecCell<String>,
        total_size: Cell<usize>,
    }

    impl StrCache {
        pub const fn new() -> Self {
            Self {
                str_bufs: VecCell::new(),
                total_size: Cell::new(0),
            }
        }

        pub fn acquire(&self) -> StrCacheGuard {
            StrCacheGuard::new(
                self,
                self.str_bufs
                    .pop()
                    // TODO use inspect once 1.76 is stable
                    .map(|buf| {
                        self.total_size.set(self.total_size.get() - buf.capacity());
                        buf
                    })
                    .unwrap_or_else(|| String::with_capacity(64)),
            )
        }

        fn release(&self, mut buf: String) {
            let new_cache_size = self.total_size.get().saturating_add(buf.capacity());
            if new_cache_size == usize::MAX {
                // This is never going to happen, but if we've used the entire address space,
                // don't bother adding another cache entry as this keeps the logic simpler.
                return;
            };
            if buf.capacity() == 0 || new_cache_size > MAX_CACHE_SIZE.load(Ordering::Relaxed) {
                return;
            }
            self.total_size.set(new_cache_size);

            buf.clear();
            self.str_bufs.push(buf);
        }
    }

    pub struct StrCacheGuard<'a> {
        cache: &'a StrCache,
        buf: String,
    }

    impl<'a> StrCacheGuard<'a> {
        pub fn new(cache: &'a StrCache, buf: String) -> Self {
            Self { cache, buf }
        }

        pub fn into_inner(self) -> String {
            let mut this = ManuallyDrop::new(self);
            mem::take(&mut this.buf)
        }
    }

    impl Deref for StrCacheGuard<'_> {
        type Target = String;

        fn deref(&self) -> &Self::Target {
            &self.buf
        }
    }

    impl DerefMut for StrCacheGuard<'_> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.buf
        }
    }

    impl Drop for StrCacheGuard<'_> {
        fn drop(&mut self) {
            self.cache.release(mem::take(&mut self.buf));
        }
    }
}
