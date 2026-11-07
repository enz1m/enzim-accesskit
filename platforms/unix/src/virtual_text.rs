// Modified by enzUI contributors. See FORK.md.
// Copyright 2026 The AccessKit Authors and enzUI contributors.
// Licensed under the Apache License, Version 2.0 or the MIT license.

//! Optional lazy text seam for documents that cannot be represented by eager
//! AccessKit `TextRun` nodes.
//!
//! Providers are registered against a concrete Unix adapter, AccessKit tree,
//! and local node ID. During a tree update that selector is bound to the exact
//! `(adapter_id, accesskit_consumer::NodeId)` used by AT-SPI. The consumer ID
//! contains both the local node ID and AccessKit's tree index, so equal local
//! IDs in simultaneous windows or embedded trees cannot collide.

use accesskit::{NodeId as LocalNodeId, TreeId};
use accesskit_atspi_common::{NodeId, PlatformNode};
use atspi::InterfaceSet;
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock, Weak};

/// Stable identity and current generation of a virtual text document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtualTextSummary {
    pub document: u128,
    pub revision: u64,
    pub bytes: u64,
    pub characters: u64,
    pub selection_anchor: Option<u64>,
    pub selection_focus: u64,
    pub read_only: bool,
}

/// One bounded range result.
#[derive(Clone, Debug, PartialEq)]
pub struct VirtualTextRange {
    pub document: u128,
    pub revision: u64,
    pub characters: Range<u64>,
    pub value: String,
    /// UTF-8 length for each Unicode scalar in `value`.
    pub character_lengths: Vec<u8>,
}

/// Cancellation observable by providers while walking backing runs.
#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Explicit refusal from the provider boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VirtualTextError {
    WrongDocument,
    StaleRevision {
        requested: u64,
        current: u64,
    },
    Cancelled,
    Range,
    ByteBudget {
        requested: usize,
        limit: usize,
    },
    CharacterBudget {
        requested: usize,
        limit: usize,
    },
    RunBudget {
        requested: usize,
        limit: usize,
    },
    ReplacementBudget {
        requested: usize,
        limit: usize,
    },
    ReadOnly,
    /// The application reducer did not answer before the request deadline.
    ///
    /// Distinct from `Unavailable`: the provider exists and accepted the
    /// request, but the application never answered it. An AT that collapses
    /// this into a generic failure cannot tell "no such document" from
    /// "the application is wedged" (enzUI-ICED #2677).
    Timeout,
    /// The provider's bounded mutation queue is full; retry after the
    /// application drains it.
    MailboxFull,
    /// The provider's mutation queue is closed because the document is
    /// shutting down. Retrying cannot succeed.
    MailboxClosed,
    /// The application reducer refused this mutation (document invariant such
    /// as a maximum paragraph length), rather than failing to run.
    ReducerRefused,
    Unavailable,
}

/// The AT-SPI/D-Bus error class one refusal is reported as.
///
/// A **typed** class rather than a string: `map_virtual` matches this
/// exhaustively with no fall-through arm, so adding a refusal (or renaming a
/// class) is a compile error in this crate instead of a silent collapse into
/// `Failed` that no test can see (enzUI-ICED #2677).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AtspiErrorClass {
    /// Malformed request (`org.freedesktop.DBus.Error.InvalidArgs`).
    InvalidArgs,
    /// Bounded budget or queue capacity exceeded.
    LimitsExceeded,
    /// Request abandoned before an answer existed.
    NoReply,
    /// Refused by policy or by the application's own document invariant.
    AccessDenied,
    /// Accepted, but the application never answered in time.
    TimedOut,
    /// The document/queue is shutting down; retrying cannot succeed.
    Disconnected,
    /// Anything else that went wrong.
    Failed,
}

impl AtspiErrorClass {
    /// Member name of the `org.freedesktop.DBus.Error.*` this class becomes.
    ///
    /// Exhaustive by construction; the prefix is added by zbus.
    pub fn dbus_name(self) -> &'static str {
        match self {
            Self::InvalidArgs => "InvalidArgs",
            Self::LimitsExceeded => "LimitsExceeded",
            Self::NoReply => "NoReply",
            Self::AccessDenied => "AccessDenied",
            Self::TimedOut => "TimedOut",
            Self::Disconnected => "Disconnected",
            Self::Failed => "Failed",
        }
    }
}

impl VirtualTextError {
    /// Stable AT-SPI/D-Bus error class for this refusal.
    ///
    /// Single source of truth for the boundary mapping so the four
    /// application-lifecycle refusals cannot silently collapse into one class:
    /// `map_virtual` builds its `fdo::Error` from this by an exhaustive typed
    /// match, and the framework can assert the classification without a live
    /// bus.
    pub fn atspi_class(&self) -> AtspiErrorClass {
        match self {
            Self::Range => AtspiErrorClass::InvalidArgs,
            Self::ByteBudget { .. }
            | Self::CharacterBudget { .. }
            | Self::RunBudget { .. }
            | Self::ReplacementBudget { .. } => AtspiErrorClass::LimitsExceeded,
            Self::Cancelled => AtspiErrorClass::NoReply,
            Self::ReadOnly => AtspiErrorClass::AccessDenied,
            Self::Timeout => AtspiErrorClass::TimedOut,
            Self::MailboxFull => AtspiErrorClass::LimitsExceeded,
            Self::MailboxClosed => AtspiErrorClass::Disconnected,
            Self::ReducerRefused => AtspiErrorClass::AccessDenied,
            Self::WrongDocument | Self::StaleRevision { .. } | Self::Unavailable => {
                AtspiErrorClass::Failed
            }
        }
    }
}

impl fmt::Display for VirtualTextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lazy virtual text request refused: {self:?}")
    }
}

impl std::error::Error for VirtualTextError {}

/// Application-owned document implementation called by the Unix adapter.
pub trait VirtualTextProvider: Send + Sync {
    fn summary(&self) -> Result<VirtualTextSummary, VirtualTextError>;

    fn read(
        &self,
        document: u128,
        revision: u64,
        characters: Range<u64>,
        cancellation: &Cancellation,
    ) -> Result<VirtualTextRange, VirtualTextError>;

    fn set_selection(
        &self,
        document: u128,
        revision: u64,
        anchor: Option<u64>,
        focus: u64,
        cancellation: &Cancellation,
    ) -> Result<u64, VirtualTextError>;

    fn replace(
        &self,
        document: u128,
        revision: u64,
        characters: Range<u64>,
        value: &str,
        cancellation: &Cancellation,
    ) -> Result<u64, VirtualTextError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Selector {
    adapter: usize,
    tree: TreeId,
    local_node: LocalNodeId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ExactNode {
    adapter: usize,
    node: u128,
}

impl ExactNode {
    fn new(adapter: usize, node: NodeId) -> Self {
        Self {
            adapter,
            node: node.into(),
        }
    }
}

#[derive(Clone)]
struct RegistrationEntry {
    serial: u64,
    provider: Weak<dyn VirtualTextProvider>,
    active_requests: Vec<Weak<AtomicBool>>,
}

#[derive(Clone)]
struct ObservedNode {
    platform: PlatformNode,
    native_text: bool,
}

#[derive(Default)]
struct Registry {
    registrations: HashMap<Selector, RegistrationEntry>,
    observed: HashMap<Selector, ObservedNode>,
    exact_to_selector: HashMap<ExactNode, Selector>,
    updating_tree: HashMap<usize, TreeId>,
}

static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();
static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);

fn registry() -> &'static RwLock<Registry> {
    REGISTRY.get_or_init(|| RwLock::new(Registry::default()))
}

pub(crate) struct TreeUpdateScope {
    adapter: usize,
}

impl TreeUpdateScope {
    pub(crate) fn enter(adapter: usize, tree: TreeId) -> Self {
        if let Ok(mut registry) = registry().write() {
            registry.updating_tree.insert(adapter, tree);
        }
        Self { adapter }
    }
}

impl Drop for TreeUpdateScope {
    fn drop(&mut self) {
        if let Ok(mut registry) = registry().write() {
            registry.updating_tree.remove(&self.adapter);
        }
    }
}

pub(crate) struct InterfaceRefresh {
    pub(crate) node: PlatformNode,
    pub(crate) enabled: bool,
    pub(crate) native_text: bool,
}

fn local_node_id(node: NodeId) -> LocalNodeId {
    LocalNodeId((u128::from(node) >> 64) as u64)
}

/// Remember the exact consumer-tree identity produced by a real adapter update.
pub(crate) fn observe_platform_node(
    node: PlatformNode,
    interfaces: InterfaceSet,
) -> Option<InterfaceRefresh> {
    let exact = ExactNode::new(node.adapter_id(), node.id());
    let mut registry = registry().write().ok()?;
    let tree = *registry.updating_tree.get(&exact.adapter)?;
    let selector = Selector {
        adapter: exact.adapter,
        tree,
        local_node: local_node_id(node.id()),
    };
    let native_text = {
        let observed = registry
            .observed
            .entry(selector)
            .or_insert_with(|| ObservedNode {
                platform: node.clone(),
                native_text: false,
            });
        observed.native_text |= interfaces.contains(atspi::Interface::Text);
        observed.native_text
    };
    registry.exact_to_selector.insert(exact, selector);
    registry
        .registrations
        .get(&selector)
        .and_then(|entry| entry.provider.upgrade())
        .map(|_| InterfaceRefresh {
            node,
            enabled: true,
            native_text,
        })
}

pub(crate) fn unobserve_platform_interfaces(
    adapter: usize,
    node: NodeId,
    interfaces: InterfaceSet,
) {
    let exact = ExactNode::new(adapter, node);
    let Ok(mut registry) = registry().write() else {
        return;
    };
    let Some(selector) = registry.exact_to_selector.get(&exact).copied() else {
        return;
    };
    if interfaces.contains(atspi::Interface::Text)
        && let Some(observed) = registry.observed.get_mut(&selector)
    {
        observed.native_text = false;
    }
}

pub(crate) fn forget_platform_node(adapter: usize, node: NodeId) {
    let exact = ExactNode::new(adapter, node);
    let Ok(mut registry) = registry().write() else {
        return;
    };
    if let Some(selector) = registry.exact_to_selector.remove(&exact) {
        registry.observed.remove(&selector);
    }
}

pub(crate) fn forget_adapter(adapter: usize) {
    let Ok(mut registry) = registry().write() else {
        return;
    };
    registry.updating_tree.remove(&adapter);
    registry
        .observed
        .retain(|selector, _| selector.adapter != adapter);
    registry
        .exact_to_selector
        .retain(|exact, _| exact.adapter != adapter);
    registry
        .registrations
        .retain(|selector, _| selector.adapter != adapter);
}

/// RAII registration for one node in one concrete adapter/tree.
///
/// Dropping the token cancels all in-flight requests before removing the
/// provider and asks the object server to withdraw provider-only interfaces.
pub struct VirtualTextRegistration {
    selector: Selector,
    serial: u64,
}

impl VirtualTextRegistration {
    /// Cooperatively cancel every request currently executing for this node.
    pub fn cancel_pending_requests(&self) {
        let Ok(mut registry) = registry().write() else {
            return;
        };
        let Some(entry) = registry.registrations.get_mut(&self.selector) else {
            return;
        };
        entry.active_requests.retain(|request| {
            if let Some(request) = request.upgrade() {
                request.store(true, Ordering::Release);
                true
            } else {
                false
            }
        });
    }
}

impl Drop for VirtualTextRegistration {
    fn drop(&mut self) {
        self.cancel_pending_requests();
        let refresh = {
            let Ok(mut registry) = registry().write() else {
                return;
            };
            if !registry
                .registrations
                .get(&self.selector)
                .is_some_and(|entry| entry.serial == self.serial)
            {
                return;
            }
            registry.registrations.remove(&self.selector);
            registry
                .observed
                .get(&self.selector)
                .map(|observed| InterfaceRefresh {
                    node: observed.platform.clone(),
                    enabled: false,
                    native_text: observed.native_text,
                })
        };
        if let Some(refresh) = refresh {
            crate::adapter::refresh_virtual_text_interfaces(refresh);
        }
    }
}

pub(crate) fn register_virtual_text_provider(
    adapter: usize,
    tree: TreeId,
    node: LocalNodeId,
    provider: &Arc<dyn VirtualTextProvider>,
) -> Result<VirtualTextRegistration, VirtualTextError> {
    let selector = Selector {
        adapter,
        tree,
        local_node: node,
    };
    let serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
    let refresh = {
        let mut registry = registry()
            .write()
            .map_err(|_| VirtualTextError::Unavailable)?;
        registry
            .registrations
            .retain(|_, entry| entry.provider.strong_count() > 0);
        if registry
            .registrations
            .get(&selector)
            .and_then(|entry| entry.provider.upgrade())
            .is_some()
        {
            return Err(VirtualTextError::Unavailable);
        }
        registry.registrations.insert(
            selector,
            RegistrationEntry {
                serial,
                provider: Arc::downgrade(provider),
                active_requests: Vec::new(),
            },
        );
        registry
            .observed
            .get(&selector)
            .map(|observed| InterfaceRefresh {
                node: observed.platform.clone(),
                enabled: true,
                native_text: observed.native_text,
            })
    };
    if let Some(refresh) = refresh {
        crate::adapter::refresh_virtual_text_interfaces(refresh);
    }
    Ok(VirtualTextRegistration { selector, serial })
}

fn provider_for_exact(exact: ExactNode) -> Option<(Selector, u64, Arc<dyn VirtualTextProvider>)> {
    let registry = registry().read().ok()?;
    let selector = *registry.exact_to_selector.get(&exact)?;
    let entry = registry.registrations.get(&selector)?;
    Some((selector, entry.serial, entry.provider.upgrade()?))
}

pub(crate) fn has_provider_for_platform_node(node: &PlatformNode) -> bool {
    has_provider_for_consumer_identity(node.adapter_id(), node.id())
}

pub(crate) fn has_provider_for_consumer_identity(adapter: usize, node: NodeId) -> bool {
    provider_for_exact(ExactNode::new(adapter, node)).is_some()
}

/// One in-flight provider request. Dropping the registration cancels its token.
pub struct VirtualTextRequest {
    cancellation: Cancellation,
}

impl VirtualTextRequest {
    pub fn cancellation(&self) -> &Cancellation {
        &self.cancellation
    }
}

/// The exact synchronous consumer used by the AT-SPI Text interface.
#[derive(Clone)]
pub struct VirtualTextUnixConsumer {
    selector: Selector,
    serial: u64,
    provider: Arc<dyn VirtualTextProvider>,
}

impl VirtualTextUnixConsumer {
    pub(crate) fn for_platform_node(node: &PlatformNode) -> Option<Self> {
        let (selector, serial, provider) =
            provider_for_exact(ExactNode::new(node.adapter_id(), node.id()))?;
        Some(Self {
            selector,
            serial,
            provider,
        })
    }

    /// Start a request whose token is owned by this registration.
    pub fn begin_request(&self) -> Result<VirtualTextRequest, VirtualTextError> {
        let cancellation = Cancellation::default();
        let mut registry = registry()
            .write()
            .map_err(|_| VirtualTextError::Unavailable)?;
        let entry = registry
            .registrations
            .get_mut(&self.selector)
            .filter(|entry| entry.serial == self.serial)
            .ok_or(VirtualTextError::Unavailable)?;
        entry
            .active_requests
            .retain(|request| request.strong_count() > 0);
        entry.active_requests.push(Arc::downgrade(&cancellation.0));
        Ok(VirtualTextRequest { cancellation })
    }

    pub fn summary(&self) -> Result<VirtualTextSummary, VirtualTextError> {
        self.provider.summary()
    }

    pub fn text(
        &self,
        summary: VirtualTextSummary,
        range: Range<u64>,
        cancellation: &Cancellation,
    ) -> Result<VirtualTextRange, VirtualTextError> {
        let result = self.provider.read(
            summary.document,
            summary.revision,
            range.clone(),
            cancellation,
        )?;
        if cancellation.is_cancelled() {
            return Err(VirtualTextError::Cancelled);
        }
        if result.document != summary.document
            || result.revision != summary.revision
            || result.characters != range
            || result.character_lengths.len() as u64 != range.end.saturating_sub(range.start)
            || result.value.chars().count() != result.character_lengths.len()
            || !result
                .value
                .chars()
                .zip(&result.character_lengths)
                .all(|(character, length)| character.len_utf8() == usize::from(*length))
        {
            return Err(VirtualTextError::Unavailable);
        }
        Ok(result)
    }

    pub fn set_selection(
        &self,
        summary: VirtualTextSummary,
        anchor: Option<u64>,
        focus: u64,
        cancellation: &Cancellation,
    ) -> Result<u64, VirtualTextError> {
        self.provider.set_selection(
            summary.document,
            summary.revision,
            anchor,
            focus,
            cancellation,
        )
    }

    pub fn replace(
        &self,
        summary: VirtualTextSummary,
        range: Range<u64>,
        value: &str,
        cancellation: &Cancellation,
    ) -> Result<u64, VirtualTextError> {
        self.provider.replace(
            summary.document,
            summary.revision,
            range,
            value,
            cancellation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use accesskit::{ActionHandler, ActionRequest, Node, Role, Tree, TreeUpdate};
    use accesskit_atspi_common::{
        Adapter, AdapterCallback, AppContext, Event, WindowBounds, next_adapter_id,
    };
    use std::sync::Mutex;

    #[derive(Debug)]
    struct Document {
        id: u128,
        state: Mutex<(u64, String, Option<u64>, u64)>,
    }

    impl Document {
        fn new(id: u128, value: &str) -> Self {
            Self {
                id,
                state: Mutex::new((1, value.to_owned(), None, 0)),
            }
        }
    }

    impl VirtualTextProvider for Document {
        fn summary(&self) -> Result<VirtualTextSummary, VirtualTextError> {
            let state = self
                .state
                .lock()
                .map_err(|_| VirtualTextError::Unavailable)?;
            Ok(VirtualTextSummary {
                document: self.id,
                revision: state.0,
                bytes: state.1.len() as u64,
                characters: state.1.chars().count() as u64,
                selection_anchor: state.2,
                selection_focus: state.3,
                read_only: false,
            })
        }

        fn read(
            &self,
            document: u128,
            revision: u64,
            characters: Range<u64>,
            cancellation: &Cancellation,
        ) -> Result<VirtualTextRange, VirtualTextError> {
            if cancellation.is_cancelled() {
                return Err(VirtualTextError::Cancelled);
            }
            let state = self
                .state
                .lock()
                .map_err(|_| VirtualTextError::Unavailable)?;
            if document != self.id {
                return Err(VirtualTextError::WrongDocument);
            }
            if revision != state.0 {
                return Err(VirtualTextError::StaleRevision {
                    requested: revision,
                    current: state.0,
                });
            }
            let start = usize::try_from(characters.start).map_err(|_| VirtualTextError::Range)?;
            let count = usize::try_from(characters.end.saturating_sub(characters.start))
                .map_err(|_| VirtualTextError::Range)?;
            let value: String = state.1.chars().skip(start).take(count).collect();
            if value.chars().count() != count {
                return Err(VirtualTextError::Range);
            }
            let character_lengths = value.chars().map(|ch| ch.len_utf8() as u8).collect();
            Ok(VirtualTextRange {
                document,
                revision,
                characters,
                value,
                character_lengths,
            })
        }

        fn set_selection(
            &self,
            document: u128,
            revision: u64,
            anchor: Option<u64>,
            focus: u64,
            cancellation: &Cancellation,
        ) -> Result<u64, VirtualTextError> {
            if cancellation.is_cancelled() {
                return Err(VirtualTextError::Cancelled);
            }
            let mut state = self
                .state
                .lock()
                .map_err(|_| VirtualTextError::Unavailable)?;
            if document != self.id || revision != state.0 {
                return Err(VirtualTextError::StaleRevision {
                    requested: revision,
                    current: state.0,
                });
            }
            if focus > state.1.chars().count() as u64
                || anchor.is_some_and(|value| value > state.1.chars().count() as u64)
            {
                return Err(VirtualTextError::Range);
            }
            state.2 = anchor;
            state.3 = focus;
            Ok(state.0)
        }

        fn replace(
            &self,
            document: u128,
            revision: u64,
            characters: Range<u64>,
            value: &str,
            cancellation: &Cancellation,
        ) -> Result<u64, VirtualTextError> {
            if cancellation.is_cancelled() {
                return Err(VirtualTextError::Cancelled);
            }
            let mut state = self
                .state
                .lock()
                .map_err(|_| VirtualTextError::Unavailable)?;
            if document != self.id || revision != state.0 {
                return Err(VirtualTextError::StaleRevision {
                    requested: revision,
                    current: state.0,
                });
            }
            let byte_start = state
                .1
                .char_indices()
                .nth(characters.start as usize)
                .map(|(byte, _)| byte)
                .unwrap_or(state.1.len());
            let byte_end = state
                .1
                .char_indices()
                .nth(characters.end as usize)
                .map(|(byte, _)| byte)
                .unwrap_or(state.1.len());
            if characters.start > characters.end || characters.end > state.1.chars().count() as u64
            {
                return Err(VirtualTextError::Range);
            }
            state.1.replace_range(byte_start..byte_end, value);
            state.0 += 1;
            Ok(state.0)
        }
    }

    struct NoOpAction;
    impl ActionHandler for NoOpAction {
        fn do_action(&mut self, _request: ActionRequest) {}
    }

    struct Observe;
    impl AdapterCallback for Observe {
        fn register_interfaces(&self, adapter: &Adapter, id: NodeId, interfaces: InterfaceSet) {
            let _ = observe_platform_node(adapter.platform_node(id), interfaces);
        }

        fn unregister_interfaces(&self, adapter: &Adapter, id: NodeId, interfaces: InterfaceSet) {
            unobserve_platform_interfaces(adapter.id(), id, interfaces);
        }

        fn emit_event(&self, _adapter: &Adapter, _event: Event) {}
    }

    fn make_adapter(
        adapter_id: usize,
        local_node: LocalNodeId,
        provider: Arc<dyn VirtualTextProvider>,
    ) -> (Adapter, VirtualTextRegistration) {
        let registration =
            register_virtual_text_provider(adapter_id, TreeId::ROOT, local_node, &provider)
                .unwrap();
        let mut node = Node::new(Role::TextInput);
        node.set_label("virtual document");
        let update = TreeUpdate {
            nodes: vec![(local_node, node)],
            tree: Some(Tree::new(local_node)),
            tree_id: TreeId::ROOT,
            focus: local_node,
        };
        let context = AppContext::new(None);
        let _scope = TreeUpdateScope::enter(adapter_id, TreeId::ROOT);
        let adapter = Adapter::with_id(
            adapter_id,
            &context,
            Observe,
            update,
            true,
            WindowBounds::default(),
            NoOpAction,
        );
        (adapter, registration)
    }

    #[test]
    fn exact_selector_refuses_two_live_owners_and_allows_retry_after_drop() {
        let adapter_id = next_adapter_id();
        let local_node = LocalNodeId(6);
        let first: Arc<dyn VirtualTextProvider> = Arc::new(Document::new(1, "first"));
        let second: Arc<dyn VirtualTextProvider> = Arc::new(Document::new(2, "second"));

        let first_registration =
            register_virtual_text_provider(adapter_id, TreeId::ROOT, local_node, &first).unwrap();
        assert!(matches!(
            register_virtual_text_provider(adapter_id, TreeId::ROOT, local_node, &second),
            Err(VirtualTextError::Unavailable)
        ));

        drop(first_registration);
        let second_registration =
            register_virtual_text_provider(adapter_id, TreeId::ROOT, local_node, &second).unwrap();
        drop(second_registration);
    }

    #[test]
    fn identical_local_ids_in_two_live_adapters_route_and_teardown_independently() {
        let local_node = LocalNodeId(7);
        let first: Arc<dyn VirtualTextProvider> = Arc::new(Document::new(101, "first window"));
        let second: Arc<dyn VirtualTextProvider> = Arc::new(Document::new(202, "second window"));
        let first_id = next_adapter_id();
        let second_id = next_adapter_id();
        let (first_adapter, first_registration) =
            make_adapter(first_id, local_node, Arc::clone(&first));
        let (second_adapter, second_registration) =
            make_adapter(second_id, local_node, Arc::clone(&second));
        let first_node = first_adapter.platform_node(first_adapter.root_id());
        let second_node = second_adapter.platform_node(second_adapter.root_id());
        assert_eq!(local_node_id(first_node.id()), local_node);
        assert_eq!(local_node_id(second_node.id()), local_node);

        let first_consumer = VirtualTextUnixConsumer::for_platform_node(&first_node).unwrap();
        let second_consumer = VirtualTextUnixConsumer::for_platform_node(&second_node).unwrap();
        let first_request = first_consumer.begin_request().unwrap();
        let second_request = second_consumer.begin_request().unwrap();
        let first_summary = first_consumer.summary().unwrap();
        let second_summary = second_consumer.summary().unwrap();
        assert_eq!(first_summary.document, 101);
        assert_eq!(second_summary.document, 202);
        assert_eq!(
            first_consumer
                .text(first_summary, 0..5, first_request.cancellation())
                .unwrap()
                .value,
            "first"
        );
        assert_eq!(
            second_consumer
                .text(second_summary, 0..6, second_request.cancellation())
                .unwrap()
                .value,
            "second"
        );
        assert_eq!(
            first_consumer.replace(first_summary, 0..5, "FIRST", first_request.cancellation()),
            Ok(2)
        );
        assert_eq!(
            second_consumer.replace(
                second_summary,
                0..6,
                "SECOND",
                second_request.cancellation()
            ),
            Ok(2)
        );

        drop(first_registration);
        assert!(VirtualTextUnixConsumer::for_platform_node(&first_node).is_none());
        let second_consumer = VirtualTextUnixConsumer::for_platform_node(&second_node).unwrap();
        assert_eq!(second_consumer.summary().unwrap().document, 202);
        drop(second_registration);
        assert!(VirtualTextUnixConsumer::for_platform_node(&second_node).is_none());
    }
}
