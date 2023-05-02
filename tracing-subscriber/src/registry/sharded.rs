use sharded_slab::{pool::Ref, Clear, Pool};
use thread_local::ThreadLocal;

use super::stack::SpanStack;
use crate::{
    filter::{FilterId, FilterMap, FilterState},
    registry::{
        extensions::{Extensions, ExtensionsInner, ExtensionsMut},
        LookupSpan, SpanData,
    },
    sync::RwLock,
};
use std::{
    cell::{self, Cell, RefCell},
    sync::atomic::{fence, AtomicUsize, Ordering},
};
use std::hash::BuildHasherDefault;
use tracing_core::{
    dispatcher::{self, Dispatch},
    span::{self, Current, Id},
    Event, Interest, Metadata, Subscriber,
};

/// A shared, reusable store for spans.
///
/// A `Registry` is a [`Subscriber`] around which multiple [`Layer`]s
/// implementing various behaviors may be [added]. Unlike other types
/// implementing `Subscriber`, `Registry` does not actually record traces itself:
/// instead, it collects and stores span data that is exposed to any [`Layer`]s
/// wrapping it through implementations of the [`LookupSpan`] trait.
/// The `Registry` is responsible for storing span metadata, recording
/// relationships between spans, and tracking which spans are active and which
/// are closed. In addition, it provides a mechanism for [`Layer`]s to store
/// user-defined per-span data, called [extensions], in the registry. This
/// allows [`Layer`]-specific data to benefit from the `Registry`'s
/// high-performance concurrent storage.
///
/// This registry is implemented using a [lock-free sharded slab][slab], and is
/// highly optimized for concurrent access.
///
/// # Span ID Generation
///
/// Span IDs are not globally unique, but the registry ensures that
/// no two currently active spans have the same ID within a process.
///
/// One of the primary responsibilities of the registry is to generate [span
/// IDs]. Therefore, it's important for other code that interacts with the
/// registry, such as [`Layer`]s, to understand the guarantees of the
/// span IDs that are generated.
///
/// The registry's span IDs are guaranteed to be unique **at a given point
/// in time**. This means that an active span will never be assigned the
/// same ID as another **currently active** span. However, the registry
/// **will** eventually reuse the IDs of [closed] spans, although an ID
/// will never be reassigned immediately after a span has closed.
///
/// Spans are not [considered closed] by the `Registry` until *every*
/// [`Span`] reference with that ID has been dropped.
///
/// Thus: span IDs generated by the registry should be considered unique
/// only at a given point in time, and only relative to other spans
/// generated by the same process. Two spans with the same ID will not exist
/// in the same process concurrently. However, if historical span data is
/// being stored, the same ID may occur for multiple spans times in that
/// data. If spans must be uniquely identified in historical data, the user
/// code storing this data must assign its own unique identifiers to those
/// spans. A counter is generally sufficient for this.
///
/// Similarly, span IDs generated by the registry are not unique outside of
/// a given process. Distributed tracing systems may require identifiers
/// that are unique across multiple processes on multiple machines (for
/// example, [OpenTelemetry's `SpanId`s and `TraceId`s][ot]). `tracing` span
/// IDs generated by the registry should **not** be used for this purpose.
/// Instead, code which integrates with a distributed tracing system should
/// generate and propagate its own IDs according to the rules specified by
/// the distributed tracing system. These IDs can be associated with
/// `tracing` spans using [fields] and/or [stored span data].
///
/// [span IDs]: tracing_core::span::Id
/// [slab]: sharded_slab
/// [`Layer`]: crate::Layer
/// [added]: crate::layer::Layer#composing-layers
/// [extensions]: super::Extensions
/// [closed]: https://docs.rs/tracing/latest/tracing/span/index.html#closing-spans
/// [considered closed]: tracing_core::subscriber::Subscriber::try_close()
/// [`Span`]: https://docs.rs/tracing/latest/tracing/span/struct.Span.html
/// [ot]: https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/trace/api.md#spancontext
/// [fields]: tracing_core::field
/// [stored span data]: crate::registry::SpanData::extensions_mut
#[cfg(feature = "registry")]
#[cfg_attr(docsrs, doc(cfg(all(feature = "registry", feature = "std"))))]
#[derive(Debug)]
pub struct Registry {
    spans: Pool<DataInner>,
    current_spans: ThreadLocal<RefCell<SpanStack>>,
    next_filter_id: u8,
}

/// Span data stored in a [`Registry`].
///
/// The registry stores well-known data defined by tracing: span relationships,
/// metadata and reference counts. Additional user-defined data provided by
/// [`Layer`s], such as formatted fields, metrics, or distributed traces should
/// be stored in the [extensions] typemap.
///
/// [`Layer`s]: crate::layer::Layer
/// [extensions]: Extensions
#[cfg(feature = "registry")]
#[cfg_attr(docsrs, doc(cfg(all(feature = "registry", feature = "std"))))]
#[derive(Debug)]
pub struct Data<'a> {
    /// Immutable reference to the pooled `DataInner` entry.
    inner: Ref<'a, DataInner>,
}

/// Stored data associated with a span.
///
/// This type is pooled using [`sharded_slab::Pool`]; when a span is
/// dropped, the `DataInner` entry at that span's slab index is cleared
/// in place and reused by a future span. Thus, the `Default` and
/// [`sharded_slab::Clear`] implementations for this type are
/// load-bearing.
#[derive(Debug)]
struct DataInner {
    filter_map: FilterMap,
    metadata: &'static Metadata<'static>,
    parent: Option<Id>,
    ref_count: AtomicUsize,
    // The span's `Extensions` typemap. Allocations for the `HashMap` backing
    // this are pooled and reused in place.
    pub(crate) extensions: RwLock<ExtensionsInner>,
}

// === impl Registry ===

impl Default for Registry {
    fn default() -> Self {
        Self {
            spans: Pool::new(),
            current_spans: ThreadLocal::new(),
            next_filter_id: 0,
        }
    }
}

#[inline]
fn idx_to_id(idx: usize) -> Id {
    Id::from_u64(idx as u64 + 1)
}

#[inline]
fn id_to_idx(id: &Id) -> usize {
    id.into_u64() as usize - 1
}

/// A guard that tracks how many [`Registry`]-backed `Layer`s have
/// processed an `on_close` event.
///
/// This is needed to enable a [`Registry`]-backed Layer to access span
/// data after the `Layer` has recieved the `on_close` callback.
///
/// Once all `Layer`s have processed this event, the [`Registry`] knows
/// that is able to safely remove the span tracked by `id`. `CloseGuard`
/// accomplishes this through a two-step process:
/// 1. Whenever a [`Registry`]-backed `Layer::on_close` method is
///    called, `Registry::start_close` is closed.
///    `Registry::start_close` increments a thread-local `CLOSE_COUNT`
///    by 1 and returns a `CloseGuard`.
/// 2. The `CloseGuard` is dropped at the end of `Layer::on_close`. On
///    drop, `CloseGuard` checks thread-local `CLOSE_COUNT`. If
///    `CLOSE_COUNT` is 0, the `CloseGuard` removes the span with the
///    `id` from the registry, as all `Layers` that might have seen the
///    `on_close` notification have processed it. If `CLOSE_COUNT` is
///    greater than 0, `CloseGuard` decrements the counter by one and
///    _does not_ remove the span from the [`Registry`].
///
pub(crate) struct CloseGuard<'a> {
    id: Id,
    registry: &'a Registry,
    is_closing: bool,
}
use std::sync::atomic::AtomicI64;
use dashmap::DashMap;
use lazy_static::lazy_static;
use rustc_hash::FxHasher;

// pub static SPAN_TRACKER: Dash
pub static LIVE_SPANS: AtomicI64 = AtomicI64::new(0);
pub static OPEN_SPANS: AtomicI64 = AtomicI64::new(0);
pub static IN_SPANS: AtomicI64 = AtomicI64::new(0);

lazy_static! {
    pub static ref SPAN_TRACKER: DashMap<Id, SpanInfo, BuildHasherDefault<FxHasher>> = DashMap::default();
}

#[derive(Debug, Default, Copy, Clone)]
pub struct SpanInfo {
    pub too_many_refs: usize,
    pub panicking: usize,
}

impl Registry {
    fn get(&self, id: &Id) -> Option<Ref<'_, DataInner>> {
        self.spans.get(id_to_idx(id))
    }

    /// Returns a guard which tracks how many `Layer`s have
    /// processed an `on_close` notification via the `CLOSE_COUNT` thread-local.
    /// For additional details, see [`CloseGuard`].
    ///
    pub(crate) fn start_close(&self, id: Id) -> CloseGuard<'_> {
        CLOSE_COUNT.with(|count| {
            let c = count.get();
            count.set(c + 1);
        });
        CloseGuard {
            id,
            registry: self,
            is_closing: false,
        }
    }

    pub(crate) fn has_per_layer_filters(&self) -> bool {
        self.next_filter_id > 0
    }

    pub(crate) fn span_stack(&self) -> cell::Ref<'_, SpanStack> {
        self.current_spans.get_or_default().borrow()
    }
}

thread_local! {
    /// `CLOSE_COUNT` is the thread-local counter used by `CloseGuard` to
    /// track how many layers have processed the close.
    /// For additional details, see [`CloseGuard`].
    ///
    static CLOSE_COUNT: Cell<usize> = Cell::new(0);
}

impl Subscriber for Registry {
    fn register_callsite(&self, _: &'static Metadata<'static>) -> Interest {
        if self.has_per_layer_filters() {
            return FilterState::take_interest().unwrap_or_else(Interest::always);
        }

        Interest::always()
    }

    fn enabled(&self, _: &Metadata<'_>) -> bool {
        if self.has_per_layer_filters() {
            return FilterState::event_enabled();
        }
        true
    }

    #[inline]
    fn new_span(&self, attrs: &span::Attributes<'_>) -> span::Id {
        let parent = if attrs.is_root() {
            None
        } else if attrs.is_contextual() {
            self.current_span().id().map(|id| self.clone_span(id))
        } else {
            attrs.parent().map(|id| self.clone_span(id))
        };

        let id = self
            .spans
            // Check out a `DataInner` entry from the pool for the new span. If
            // there are free entries already allocated in the pool, this will
            // preferentially reuse one; otherwise, a new `DataInner` is
            // allocated and added to the pool.
            .create_with(|data| {
                data.metadata = attrs.metadata();
                data.parent = parent;
                data.filter_map = crate::filter::FILTERING.with(|filtering| filtering.filter_map());
                #[cfg(debug_assertions)]
                {
                    if data.filter_map != FilterMap::default() {
                        debug_assert!(self.has_per_layer_filters());
                    }
                }

                let refs = data.ref_count.get_mut();
                debug_assert_eq!(*refs, 0);
                *refs = 1;
            })
            .expect("Unable to allocate another span");
        let id = idx_to_id(id);
        SPAN_TRACKER.insert(id.clone(), SpanInfo::default());
        LIVE_SPANS.fetch_add(1, Ordering::Release);
        OPEN_SPANS.fetch_add(1, Ordering::Release);
        id
    }

    /// This is intentionally not implemented, as recording fields
    /// on a span is the responsibility of layers atop of this registry.
    #[inline]
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {
    }

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event_enabled(&self, _event: &Event<'_>) -> bool {
        if self.has_per_layer_filters() {
            return FilterState::event_enabled();
        }
        true
    }

    /// This is intentionally not implemented, as recording events
    /// is the responsibility of layers atop of this registry.
    fn event(&self, _: &Event<'_>) {
    }

    fn enter(&self, id: &span::Id) {
        if self
            .current_spans
            .get_or_default()
            .borrow_mut()
            .push(id.clone())
        {
            self.clone_span(id);
        }
        IN_SPANS.fetch_add(1, Ordering::Release);
    }

    fn exit(&self, id: &span::Id) {
        if let Some(spans) = self.current_spans.get() {
            if spans.borrow_mut().pop(id) {
                dispatcher::get_default(|dispatch| dispatch.try_close(id.clone()));
            }
        }
        IN_SPANS.fetch_sub(1, Ordering::Release);
    }

    fn clone_span(&self, id: &span::Id) -> span::Id {
        let span = self
            .get(id)
            .unwrap_or_else(|| panic!(
                "tried to clone {:?}, but no span exists with that ID\n\
                This may be caused by consuming a parent span (`parent: span`) rather than borrowing it (`parent: &span`).",
                id,
            ));
        // Like `std::sync::Arc`, adds to the ref count (on clone) don't require
        // a strong ordering; if we call` clone_span`, the reference count must
        // always at least 1. The only synchronization necessary is between
        // calls to `try_close`: we have to ensure that all threads have
        // dropped their refs to the span before the span is closed.
        let refs = span.ref_count.fetch_add(1, Ordering::Relaxed);
        assert_ne!(
            refs, 0,
            "tried to clone a span ({:?}) that already closed",
            id
        );
        let span_info = *SPAN_TRACKER.get(&id).unwrap();
        SPAN_TRACKER.insert(id.clone(), span_info);
        id.clone()
    }

    fn current_span(&self) -> Current {
        self.current_spans
            .get()
            .and_then(|spans| {
                let spans = spans.borrow();
                let id = spans.current()?;
                let span = self.get(id)?;
                Some(Current::new(id.clone(), span.metadata))
            })
            .unwrap_or_else(Current::none)
    }

    /// Decrements the reference count of the span with the given `id`, and
    /// removes the span if it is zero.
    ///
    /// The allocated span slot will be reused when a new span is created.
    fn try_close(&self, id: span::Id) -> bool {
        let span = match self.get(&id) {
            Some(span) => span,
            None if std::thread::panicking() => {
                SPAN_TRACKER.get_mut(&id).unwrap().panicking += 1;
                return false
            },
            None => {
                panic!("tried to drop a ref to {:?}, but no such span exists!", id)
            },
        };

        let refs = span.ref_count.fetch_sub(1, Ordering::Release);
        if !std::thread::panicking() {
            assert!(refs < std::usize::MAX, "reference count overflow!");
        }
        if refs > 1 {
            SPAN_TRACKER.get_mut(&id).unwrap().too_many_refs += 1;
            return false;
        }

        // Synchronize if we are actually removing the span (stolen
        // from std::Arc); this ensures that all other `try_close` calls on
        // other threads happen-before we actually remove the span.
        fence(Ordering::Acquire);
        SPAN_TRACKER.remove(&id);
        OPEN_SPANS.fetch_sub(1, Ordering::Release);
        true
    }
}

impl<'a> LookupSpan<'a> for Registry {
    type Data = Data<'a>;

    fn span_data(&'a self, id: &Id) -> Option<Self::Data> {
        let inner = self.get(id)?;
        Some(Data { inner })
    }

    fn register_filter(&mut self) -> FilterId {
        let id = FilterId::new(self.next_filter_id);
        self.next_filter_id += 1;
        id
    }
}

// === impl CloseGuard ===

impl<'a> CloseGuard<'a> {
    pub(crate) fn set_closing(&mut self) {
        self.is_closing = true;
    }
}

impl<'a> Drop for CloseGuard<'a> {
    fn drop(&mut self) {
        // If this returns with an error, we are already panicking. At
        // this point, there's nothing we can really do to recover
        // except by avoiding a double-panic.
        let _ = CLOSE_COUNT.try_with(|count| {
            let c = count.get();
            // Decrement the count to indicate that _this_ guard's
            // `on_close` callback has completed.
            //
            // Note that we *must* do this before we actually remove the span
            // from the registry, since dropping the `DataInner` may trigger a
            // new close, if this span is the last reference to a parent span.
            count.set(c - 1);

            // If the current close count is 1, this stack frame is the last
            // `on_close` call. If the span is closing, it's okay to remove the
            // span.
            if c == 1 && self.is_closing {
                self.registry.spans.clear(id_to_idx(&self.id));
                LIVE_SPANS.fetch_sub(1, Ordering::Release);
            }
        });
    }
}

// === impl Data ===

impl<'a> SpanData<'a> for Data<'a> {
    fn id(&self) -> Id {
        idx_to_id(self.inner.key())
    }

    fn metadata(&self) -> &'static Metadata<'static> {
        (*self).inner.metadata
    }

    fn parent(&self) -> Option<&Id> {
        self.inner.parent.as_ref()
    }

    fn extensions(&self) -> Extensions<'_> {
        Extensions::new(self.inner.extensions.read().expect("Mutex poisoned"))
    }

    fn extensions_mut(&self) -> ExtensionsMut<'_> {
        ExtensionsMut::new(self.inner.extensions.write().expect("Mutex poisoned"))
    }

    #[inline]
    fn is_enabled_for(&self, filter: FilterId) -> bool {
        self.inner.filter_map.is_enabled(filter)
    }
}

// === impl DataInner ===

impl Default for DataInner {
    fn default() -> Self {
        // Since `DataInner` owns a `&'static Callsite` pointer, we need
        // something to use as the initial default value for that callsite.
        // Since we can't access a `DataInner` until it has had actual span data
        // inserted into it, the null metadata will never actually be accessed.
        struct NullCallsite;
        impl tracing_core::callsite::Callsite for NullCallsite {
            fn set_interest(&self, _: Interest) {
                unreachable!(
                    "/!\\ Tried to register the null callsite /!\\\n \
                    This should never have happened and is definitely a bug. \
                    A `tracing` bug report would be appreciated."
                )
            }

            fn metadata(&self) -> &Metadata<'_> {
                unreachable!(
                    "/!\\ Tried to access the null callsite's metadata /!\\\n \
                    This should never have happened and is definitely a bug. \
                    A `tracing` bug report would be appreciated."
                )
            }
        }

        static NULL_CALLSITE: NullCallsite = NullCallsite;
        static NULL_METADATA: Metadata<'static> = tracing_core::metadata! {
            name: "",
            target: "",
            level: tracing_core::Level::TRACE,
            fields: &[],
            callsite: &NULL_CALLSITE,
            kind: tracing_core::metadata::Kind::SPAN,
        };

        Self {
            filter_map: FilterMap::default(),
            metadata: &NULL_METADATA,
            parent: None,
            ref_count: AtomicUsize::new(0),
            extensions: RwLock::new(ExtensionsInner::new()),
        }
    }
}

impl Clear for DataInner {
    /// Clears the span's data in place, dropping the parent's reference count.
    fn clear(&mut self) {
        // A span is not considered closed until all of its children have closed.
        // Therefore, each span's `DataInner` holds a "reference" to the parent
        // span, keeping the parent span open until all its children have closed.
        // When we close a span, we must then decrement the parent's ref count
        // (potentially, allowing it to close, if this child is the last reference
        // to that span).
        // We have to actually unpack the option inside the `get_default`
        // closure, since it is a `FnMut`, but testing that there _is_ a value
        // here lets us avoid the thread-local access if we don't need the
        // dispatcher at all.
        if self.parent.is_some() {
            // Note that --- because `Layered::try_close` works by calling
            // `try_close` on the inner subscriber and using the return value to
            // determine whether to call the `Layer`'s `on_close` callback ---
            // we must call `try_close` on the entire subscriber stack, rather
            // than just on the registry. If the registry called `try_close` on
            // itself directly, the layers wouldn't see the close notification.
            let subscriber = dispatcher::get_default(Dispatch::clone);
            if let Some(parent) = self.parent.take() {
                let _ = subscriber.try_close(parent);
            }
        }

        // Clear (but do not deallocate!) the pooled `HashMap` for the span's extensions.
        self.extensions
            .get_mut()
            .unwrap_or_else(|l| {
                // This function can be called in a `Drop` impl, such as while
                // panicking, so ignore lock poisoning.
                l.into_inner()
            })
            .clear();

        self.filter_map = FilterMap::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{layer::Context, registry::LookupSpan, Layer};
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex, Weak},
    };
    use tracing::{self, subscriber::with_default};
    use tracing_core::{
        dispatcher,
        span::{Attributes, Id},
        Subscriber,
    };

    #[derive(Debug)]
    struct DoesNothing;
    impl<S: Subscriber> Layer<S> for DoesNothing {}

    struct AssertionLayer;
    impl<S> Layer<S> for AssertionLayer
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_close(&self, id: Id, ctx: Context<'_, S>) {
            dbg!(format_args!("closing {:?}", id));
            assert!(&ctx.span(&id).is_some());
        }
    }

    #[test]
    fn single_layer_can_access_closed_span() {
        let subscriber = AssertionLayer.with_subscriber(Registry::default());

        with_default(subscriber, || {
            let span = tracing::debug_span!("span");
            drop(span);
        });
    }

    #[test]
    fn multiple_layers_can_access_closed_span() {
        let subscriber = AssertionLayer
            .and_then(AssertionLayer)
            .with_subscriber(Registry::default());

        with_default(subscriber, || {
            let span = tracing::debug_span!("span");
            drop(span);
        });
    }

    struct CloseLayer {
        inner: Arc<Mutex<CloseState>>,
    }

    struct CloseHandle {
        state: Arc<Mutex<CloseState>>,
    }

    #[derive(Default)]
    struct CloseState {
        open: HashMap<&'static str, Weak<()>>,
        closed: Vec<(&'static str, Weak<()>)>,
    }

    struct SetRemoved(Arc<()>);

    impl<S> Layer<S> for CloseLayer
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_new_span(&self, _: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
            let span = ctx.span(id).expect("Missing span; this is a bug");
            let mut lock = self.inner.lock().unwrap();
            let is_removed = Arc::new(());
            assert!(
                lock.open
                    .insert(span.name(), Arc::downgrade(&is_removed))
                    .is_none(),
                "test layer saw multiple spans with the same name, the test is probably messed up"
            );
            let mut extensions = span.extensions_mut();
            extensions.insert(SetRemoved(is_removed));
        }

        fn on_close(&self, id: Id, ctx: Context<'_, S>) {
            let span = if let Some(span) = ctx.span(&id) {
                span
            } else {
                println!(
                    "span {:?} did not exist in `on_close`, are we panicking?",
                    id
                );
                return;
            };
            let name = span.name();
            println!("close {} ({:?})", name, id);
            if let Ok(mut lock) = self.inner.lock() {
                if let Some(is_removed) = lock.open.remove(name) {
                    assert!(is_removed.upgrade().is_some());
                    lock.closed.push((name, is_removed));
                }
            }
        }
    }

    impl CloseLayer {
        fn new() -> (Self, CloseHandle) {
            let state = Arc::new(Mutex::new(CloseState::default()));
            (
                Self {
                    inner: state.clone(),
                },
                CloseHandle { state },
            )
        }
    }

    impl CloseState {
        fn is_open(&self, span: &str) -> bool {
            self.open.contains_key(span)
        }

        fn is_closed(&self, span: &str) -> bool {
            self.closed.iter().any(|(name, _)| name == &span)
        }
    }

    impl CloseHandle {
        fn assert_closed(&self, span: &str) {
            let lock = self.state.lock().unwrap();
            assert!(
                lock.is_closed(span),
                "expected {} to be closed{}",
                span,
                if lock.is_open(span) {
                    " (it was still open)"
                } else {
                    ", but it never existed (is there a problem with the test?)"
                }
            )
        }

        fn assert_open(&self, span: &str) {
            let lock = self.state.lock().unwrap();
            assert!(
                lock.is_open(span),
                "expected {} to be open{}",
                span,
                if lock.is_closed(span) {
                    " (it was still open)"
                } else {
                    ", but it never existed (is there a problem with the test?)"
                }
            )
        }

        fn assert_removed(&self, span: &str) {
            let lock = self.state.lock().unwrap();
            let is_removed = match lock.closed.iter().find(|(name, _)| name == &span) {
                Some((_, is_removed)) => is_removed,
                None => panic!(
                    "expected {} to be removed from the registry, but it was not closed {}",
                    span,
                    if lock.is_closed(span) {
                        " (it was still open)"
                    } else {
                        ", but it never existed (is there a problem with the test?)"
                    }
                ),
            };
            assert!(
                is_removed.upgrade().is_none(),
                "expected {} to have been removed from the registry",
                span
            )
        }

        fn assert_not_removed(&self, span: &str) {
            let lock = self.state.lock().unwrap();
            let is_removed = match lock.closed.iter().find(|(name, _)| name == &span) {
                Some((_, is_removed)) => is_removed,
                None if lock.is_open(span) => return,
                None => unreachable!(),
            };
            assert!(
                is_removed.upgrade().is_some(),
                "expected {} to have been removed from the registry",
                span
            )
        }

        #[allow(unused)] // may want this for future tests
        fn assert_last_closed(&self, span: Option<&str>) {
            let lock = self.state.lock().unwrap();
            let last = lock.closed.last().map(|(span, _)| span);
            assert_eq!(
                last,
                span.as_ref(),
                "expected {:?} to have closed last",
                span
            );
        }

        fn assert_closed_in_order(&self, order: impl AsRef<[&'static str]>) {
            let lock = self.state.lock().unwrap();
            let order = order.as_ref();
            for (i, name) in order.iter().enumerate() {
                assert_eq!(
                    lock.closed.get(i).map(|(span, _)| span),
                    Some(name),
                    "expected close order: {:?}, actual: {:?}",
                    order,
                    lock.closed.iter().map(|(name, _)| name).collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn spans_are_removed_from_registry() {
        let (close_layer, state) = CloseLayer::new();
        let subscriber = AssertionLayer
            .and_then(close_layer)
            .with_subscriber(Registry::default());

        // Create a `Dispatch` (which is internally reference counted) so that
        // the subscriber lives to the end of the test. Otherwise, if we just
        // passed the subscriber itself to `with_default`, we could see the span
        // be dropped when the subscriber itself is dropped, destroying the
        // registry.
        let dispatch = dispatcher::Dispatch::new(subscriber);

        dispatcher::with_default(&dispatch, || {
            let span = tracing::debug_span!("span1");
            drop(span);
            let span = tracing::info_span!("span2");
            drop(span);
        });

        state.assert_removed("span1");
        state.assert_removed("span2");

        // Ensure the registry itself outlives the span.
        drop(dispatch);
    }

    #[test]
    fn spans_are_only_closed_when_the_last_ref_drops() {
        let (close_layer, state) = CloseLayer::new();
        let subscriber = AssertionLayer
            .and_then(close_layer)
            .with_subscriber(Registry::default());

        // Create a `Dispatch` (which is internally reference counted) so that
        // the subscriber lives to the end of the test. Otherwise, if we just
        // passed the subscriber itself to `with_default`, we could see the span
        // be dropped when the subscriber itself is dropped, destroying the
        // registry.
        let dispatch = dispatcher::Dispatch::new(subscriber);

        let span2 = dispatcher::with_default(&dispatch, || {
            let span = tracing::debug_span!("span1");
            drop(span);
            let span2 = tracing::info_span!("span2");
            let span2_clone = span2.clone();
            drop(span2);
            span2_clone
        });

        state.assert_removed("span1");
        state.assert_not_removed("span2");

        drop(span2);
        state.assert_removed("span1");

        // Ensure the registry itself outlives the span.
        drop(dispatch);
    }

    #[test]
    fn span_enter_guards_are_dropped_out_of_order() {
        let (close_layer, state) = CloseLayer::new();
        let subscriber = AssertionLayer
            .and_then(close_layer)
            .with_subscriber(Registry::default());

        // Create a `Dispatch` (which is internally reference counted) so that
        // the subscriber lives to the end of the test. Otherwise, if we just
        // passed the subscriber itself to `with_default`, we could see the span
        // be dropped when the subscriber itself is dropped, destroying the
        // registry.
        let dispatch = dispatcher::Dispatch::new(subscriber);

        dispatcher::with_default(&dispatch, || {
            let span1 = tracing::debug_span!("span1");
            let span2 = tracing::info_span!("span2");

            let enter1 = span1.enter();
            let enter2 = span2.enter();

            drop(enter1);
            drop(span1);

            state.assert_removed("span1");
            state.assert_not_removed("span2");

            drop(enter2);
            state.assert_not_removed("span2");

            drop(span2);
            state.assert_removed("span1");
            state.assert_removed("span2");
        });
    }

    #[test]
    fn child_closes_parent() {
        // This test asserts that if a parent span's handle is dropped before
        // a child span's handle, the parent will remain open until child
        // closes, and will then be closed.

        let (close_layer, state) = CloseLayer::new();
        let subscriber = close_layer.with_subscriber(Registry::default());

        let dispatch = dispatcher::Dispatch::new(subscriber);

        dispatcher::with_default(&dispatch, || {
            let span1 = tracing::info_span!("parent");
            let span2 = tracing::info_span!(parent: &span1, "child");

            state.assert_open("parent");
            state.assert_open("child");

            drop(span1);
            state.assert_open("parent");
            state.assert_open("child");

            drop(span2);
            state.assert_closed("parent");
            state.assert_closed("child");
        });
    }

    #[test]
    fn child_closes_grandparent() {
        // This test asserts that, when a span is kept open by a child which
        // is *itself* kept open by a child, closing the grandchild will close
        // both the parent *and* the grandparent.
        let (close_layer, state) = CloseLayer::new();
        let subscriber = close_layer.with_subscriber(Registry::default());

        let dispatch = dispatcher::Dispatch::new(subscriber);

        dispatcher::with_default(&dispatch, || {
            let span1 = tracing::info_span!("grandparent");
            let span2 = tracing::info_span!(parent: &span1, "parent");
            let span3 = tracing::info_span!(parent: &span2, "child");

            state.assert_open("grandparent");
            state.assert_open("parent");
            state.assert_open("child");

            drop(span1);
            drop(span2);
            state.assert_open("grandparent");
            state.assert_open("parent");
            state.assert_open("child");

            drop(span3);

            state.assert_closed_in_order(&["child", "parent", "grandparent"]);
        });
    }
}
