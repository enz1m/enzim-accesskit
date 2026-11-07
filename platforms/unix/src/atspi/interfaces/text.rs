// Modified by enzUI contributors. See FORK.md.
// Copyright 2024 The AccessKit Authors. All rights reserved.
// Licensed under the Apache License, Version 2.0 (found in
// the LICENSE-APACHE file) or the MIT license (found in
// the LICENSE-MIT file), at your option.

use accesskit_atspi_common::{PlatformNode, Rect};
use atspi::{CoordType, Granularity, ScrollType};
use std::collections::HashMap;
use zbus::{fdo, interface};

use crate::virtual_text::{VirtualTextError, VirtualTextUnixConsumer};

pub(crate) struct TextInterface {
    node: PlatformNode,
}

impl TextInterface {
    pub fn new(node: PlatformNode) -> Self {
        Self { node }
    }

    fn map_error(&self) -> impl '_ + FnOnce(accesskit_atspi_common::Error) -> fdo::Error {
        |error| crate::util::map_error_from_node(&self.node, error)
    }

    fn virtual_consumer(&self) -> Option<VirtualTextUnixConsumer> {
        VirtualTextUnixConsumer::for_platform_node(&self.node)
    }

    pub(super) fn map_virtual(error: VirtualTextError) -> fdo::Error {
        // Preserve refusal classes at the D-Bus boundary. In particular, a
        // bounded/stale request is never an empty string that an AT could
        // mistake for document content.
        match error {
            VirtualTextError::Range => fdo::Error::InvalidArgs(error.to_string()),
            VirtualTextError::ByteBudget { .. }
            | VirtualTextError::CharacterBudget { .. }
            | VirtualTextError::RunBudget { .. }
            | VirtualTextError::ReplacementBudget { .. } => {
                fdo::Error::LimitsExceeded(error.to_string())
            }
            VirtualTextError::Cancelled => fdo::Error::NoReply(error.to_string()),
            VirtualTextError::ReadOnly => fdo::Error::AccessDenied(error.to_string()),
            VirtualTextError::WrongDocument
            | VirtualTextError::StaleRevision { .. }
            | VirtualTextError::Unavailable => fdo::Error::Failed(error.to_string()),
        }
    }
}

#[interface(name = "org.a11y.atspi.Text")]
impl TextInterface {
    #[zbus(property)]
    fn character_count(&self) -> fdo::Result<i32> {
        if let Some(consumer) = self.virtual_consumer() {
            return consumer
                .summary()
                .and_then(|summary| {
                    i32::try_from(summary.characters).map_err(|_| VirtualTextError::Range)
                })
                .map_err(Self::map_virtual);
        }
        self.node.character_count().map_err(self.map_error())
    }

    #[zbus(property)]
    fn caret_offset(&self) -> fdo::Result<i32> {
        if let Some(consumer) = self.virtual_consumer() {
            return consumer
                .summary()
                .and_then(|summary| {
                    i32::try_from(summary.selection_focus).map_err(|_| VirtualTextError::Range)
                })
                .map_err(Self::map_virtual);
        }
        self.node.caret_offset().map_err(self.map_error())
    }

    fn get_string_at_offset(
        &self,
        offset: i32,
        granularity: Granularity,
    ) -> fdo::Result<(String, i32, i32)> {
        if let Some(consumer) = self.virtual_consumer() {
            const CONTEXT: u64 = 4_096;
            let request = consumer.begin_request().map_err(Self::map_virtual)?;
            let summary = consumer.summary().map_err(Self::map_virtual)?;
            let offset = u64::try_from(offset)
                .map_err(|_| fdo::Error::InvalidArgs("offset is negative".into()))?;
            if offset >= summary.characters {
                return Err(fdo::Error::InvalidArgs("offset is outside text".into()));
            }
            if granularity == Granularity::Char {
                let range = consumer
                    .text(summary, offset..offset + 1, request.cancellation())
                    .map_err(Self::map_virtual)?;
                return Ok((range.value, offset as i32, (offset + 1) as i32));
            }

            // Ask only for bounded context, then find the requested semantic
            // unit locally. If the unit reaches an edge without a delimiter,
            // refuse instead of presenting a truncated word/line as complete.
            let window_start = offset.saturating_sub(CONTEXT);
            let window_end = summary.characters.min(offset.saturating_add(CONTEXT + 1));
            let range = consumer
                .text(summary, window_start..window_end, request.cancellation())
                .map_err(Self::map_virtual)?;
            let chars: Vec<char> = range.value.chars().collect();
            let local = usize::try_from(offset - window_start)
                .map_err(|_| fdo::Error::InvalidArgs("offset is too large".into()))?;
            let delimiter = |ch: char| match granularity {
                Granularity::Word => ch.is_whitespace(),
                Granularity::Sentence => matches!(ch, '.' | '!' | '?' | '\n'),
                Granularity::Line | Granularity::Paragraph => ch == '\n',
                Granularity::Char => false,
            };
            let mut start = local;
            while start > 0 && !delimiter(chars[start - 1]) {
                start -= 1;
            }
            let mut end = local + 1;
            while end < chars.len() && !delimiter(chars[end - 1]) {
                end += 1;
            }
            if (start == 0 && window_start != 0)
                || (end == chars.len() && window_end != summary.characters)
            {
                return Err(Self::map_virtual(VirtualTextError::CharacterBudget {
                    requested: chars.len().saturating_add(1),
                    limit: chars.len(),
                }));
            }
            let value: String = chars[start..end].iter().collect();
            let global_start = window_start + start as u64;
            let global_end = window_start + end as u64;
            Ok((
                value,
                i32::try_from(global_start)
                    .map_err(|_| Self::map_virtual(VirtualTextError::Range))?,
                i32::try_from(global_end)
                    .map_err(|_| Self::map_virtual(VirtualTextError::Range))?,
            ))
        } else {
            self.node
                .string_at_offset(offset, granularity)
                .map_err(self.map_error())
        }
    }

    fn get_text(&self, start_offset: i32, end_offset: i32) -> fdo::Result<String> {
        if let Some(consumer) = self.virtual_consumer() {
            let request = consumer.begin_request().map_err(Self::map_virtual)?;
            let summary = consumer.summary().map_err(Self::map_virtual)?;
            let start = u64::try_from(start_offset)
                .map_err(|_| fdo::Error::InvalidArgs("start offset is negative".into()))?;
            let end = if end_offset == -1 {
                summary.characters
            } else {
                u64::try_from(end_offset)
                    .map_err(|_| fdo::Error::InvalidArgs("end offset is negative".into()))?
            };
            return consumer
                .text(summary, start..end, request.cancellation())
                .map(|range| range.value)
                .map_err(Self::map_virtual);
        }
        self.node
            .text(start_offset, end_offset)
            .map_err(self.map_error())
    }

    fn set_caret_offset(&self, offset: i32) -> fdo::Result<bool> {
        if let Some(consumer) = self.virtual_consumer() {
            let request = consumer.begin_request().map_err(Self::map_virtual)?;
            let summary = consumer.summary().map_err(Self::map_virtual)?;
            let offset = u64::try_from(offset)
                .map_err(|_| fdo::Error::InvalidArgs("offset is negative".into()))?;
            consumer
                .set_selection(summary, None, offset, request.cancellation())
                .map_err(Self::map_virtual)?;
            return Ok(true);
        }
        self.node.set_caret_offset(offset).map_err(self.map_error())
    }

    fn get_attribute_value(&self, offset: i32, attribute_name: &str) -> fdo::Result<String> {
        self.node
            .text_attribute_value(offset, attribute_name)
            .map_err(self.map_error())
    }

    fn get_attributes(
        &self,
        offset: i32,
    ) -> fdo::Result<(HashMap<&'static str, String>, i32, i32)> {
        self.node.text_attributes(offset).map_err(self.map_error())
    }

    fn get_default_attributes(&self) -> fdo::Result<HashMap<&'static str, String>> {
        self.node
            .default_text_attributes()
            .map_err(self.map_error())
    }

    fn get_character_extents(&self, offset: i32, coord_type: CoordType) -> fdo::Result<Rect> {
        self.node
            .character_extents(offset, coord_type)
            .map_err(self.map_error())
    }

    fn get_offset_at_point(&self, x: i32, y: i32, coord_type: CoordType) -> fdo::Result<i32> {
        self.node
            .offset_at_point(x, y, coord_type)
            .map_err(self.map_error())
    }

    fn get_n_selections(&self) -> fdo::Result<i32> {
        if let Some(consumer) = self.virtual_consumer() {
            return consumer
                .summary()
                .map(|summary| {
                    i32::from(
                        summary
                            .selection_anchor
                            .is_some_and(|anchor| anchor != summary.selection_focus),
                    )
                })
                .map_err(Self::map_virtual);
        }
        self.node.n_selections().map_err(self.map_error())
    }

    fn get_selection(&self, selection_num: i32) -> fdo::Result<(i32, i32)> {
        if let Some(consumer) = self.virtual_consumer() {
            if selection_num != 0 {
                return Ok((-1, -1));
            }
            return consumer
                .summary()
                .and_then(|summary| {
                    let Some(anchor) = summary.selection_anchor else {
                        return Ok((-1, -1));
                    };
                    let start = i32::try_from(anchor.min(summary.selection_focus))
                        .map_err(|_| VirtualTextError::Range)?;
                    let end = i32::try_from(anchor.max(summary.selection_focus))
                        .map_err(|_| VirtualTextError::Range)?;
                    Ok((start, end))
                })
                .map_err(Self::map_virtual);
        }
        self.node.selection(selection_num).map_err(self.map_error())
    }

    fn add_selection(&self, start_offset: i32, end_offset: i32) -> fdo::Result<bool> {
        if self.virtual_consumer().is_some() {
            return self.set_selection(0, start_offset, end_offset);
        }
        self.node
            .add_selection(start_offset, end_offset)
            .map_err(self.map_error())
    }

    fn remove_selection(&self, selection_num: i32) -> fdo::Result<bool> {
        if let Some(consumer) = self.virtual_consumer() {
            if selection_num != 0 {
                return Ok(false);
            }
            let request = consumer.begin_request().map_err(Self::map_virtual)?;
            let summary = consumer.summary().map_err(Self::map_virtual)?;
            consumer
                .set_selection(
                    summary,
                    None,
                    summary.selection_focus,
                    request.cancellation(),
                )
                .map_err(Self::map_virtual)?;
            return Ok(true);
        }
        self.node
            .remove_selection(selection_num)
            .map_err(self.map_error())
    }

    fn set_selection(
        &self,
        selection_num: i32,
        start_offset: i32,
        end_offset: i32,
    ) -> fdo::Result<bool> {
        if let Some(consumer) = self.virtual_consumer() {
            if selection_num != 0 {
                return Ok(false);
            }
            let request = consumer.begin_request().map_err(Self::map_virtual)?;
            let summary = consumer.summary().map_err(Self::map_virtual)?;
            let start = u64::try_from(start_offset)
                .map_err(|_| fdo::Error::InvalidArgs("start offset is negative".into()))?;
            let end = u64::try_from(end_offset)
                .map_err(|_| fdo::Error::InvalidArgs("end offset is negative".into()))?;
            consumer
                .set_selection(summary, Some(start), end, request.cancellation())
                .map_err(Self::map_virtual)?;
            return Ok(true);
        }
        self.node
            .set_selection(selection_num, start_offset, end_offset)
            .map_err(self.map_error())
    }

    fn get_range_extents(
        &self,
        start_offset: i32,
        end_offset: i32,
        coord_type: CoordType,
    ) -> fdo::Result<Rect> {
        self.node
            .range_extents(start_offset, end_offset, coord_type)
            .map_err(self.map_error())
    }

    fn get_attribute_run(
        &self,
        offset: i32,
        include_defaults: bool,
    ) -> fdo::Result<(HashMap<&'static str, String>, i32, i32)> {
        self.node
            .text_attribute_run(offset, include_defaults)
            .map_err(self.map_error())
    }

    fn scroll_substring_to(
        &self,
        start_offset: i32,
        end_offset: i32,
        scroll_type: ScrollType,
    ) -> fdo::Result<bool> {
        self.node
            .scroll_substring_to(start_offset, end_offset, scroll_type)
            .map_err(self.map_error())
    }

    fn scroll_substring_to_point(
        &self,
        start_offset: i32,
        end_offset: i32,
        coord_type: CoordType,
        x: i32,
        y: i32,
    ) -> fdo::Result<bool> {
        self.node
            .scroll_substring_to_point(start_offset, end_offset, coord_type, x, y)
            .map_err(self.map_error())
    }
}

#[cfg(test)]
mod virtual_text_dbus_tests {
    use super::*;
    use crate::atspi::interfaces::EditableTextInterface;
    use crate::virtual_text::{
        self, Cancellation, VirtualTextProvider, VirtualTextRange, VirtualTextSummary,
    };
    use accesskit::{
        ActionHandler, ActionRequest, Node, NodeId as LocalNodeId, Role, Tree, TreeId, TreeUpdate,
    };
    use accesskit_atspi_common::{
        Adapter, AdapterCallback, AppContext, Event, NodeId, WindowBounds, next_adapter_id,
    };
    use atspi::InterfaceSet;
    use std::ops::Range;
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use zbus::connection::Builder;

    #[derive(Debug)]
    struct Document(Mutex<(u64, String, Option<u64>, u64)>);

    impl Document {
        fn new(value: &str) -> Self {
            Self(Mutex::new((1, value.to_owned(), None, 0)))
        }
    }

    impl VirtualTextProvider for Document {
        fn summary(&self) -> Result<VirtualTextSummary, VirtualTextError> {
            let state = self.0.lock().map_err(|_| VirtualTextError::Unavailable)?;
            Ok(VirtualTextSummary {
                document: 2677,
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
            if characters == (0..1) {
                return Err(VirtualTextError::ByteBudget {
                    requested: 13,
                    limit: 12,
                });
            }
            if characters == (1..2) {
                return Err(VirtualTextError::CharacterBudget {
                    requested: 13,
                    limit: 12,
                });
            }
            if characters == (2..3) {
                return Err(VirtualTextError::RunBudget {
                    requested: 2,
                    limit: 1,
                });
            }
            if characters == (3..4) {
                return Err(VirtualTextError::StaleRevision {
                    requested: revision,
                    current: revision.saturating_add(1),
                });
            }
            if characters == (4..5) {
                return Err(VirtualTextError::Cancelled);
            }
            let state = self.0.lock().map_err(|_| VirtualTextError::Unavailable)?;
            if document != 2677 {
                return Err(VirtualTextError::WrongDocument);
            }
            if revision != state.0 {
                return Err(VirtualTextError::StaleRevision {
                    requested: revision,
                    current: state.0,
                });
            }
            let count = usize::try_from(characters.end.saturating_sub(characters.start))
                .map_err(|_| VirtualTextError::Range)?;
            if count > 12 {
                return Err(VirtualTextError::CharacterBudget {
                    requested: count,
                    limit: 12,
                });
            }
            let value: String = state
                .1
                .chars()
                .skip(characters.start as usize)
                .take(count)
                .collect();
            if value.chars().count() != count {
                return Err(VirtualTextError::Range);
            }
            Ok(VirtualTextRange {
                document,
                revision,
                characters,
                character_lengths: value.chars().map(|ch| ch.len_utf8() as u8).collect(),
                value,
            })
        }

        fn set_selection(
            &self,
            _document: u128,
            revision: u64,
            anchor: Option<u64>,
            focus: u64,
            cancellation: &Cancellation,
        ) -> Result<u64, VirtualTextError> {
            if cancellation.is_cancelled() {
                return Err(VirtualTextError::Cancelled);
            }
            let mut state = self.0.lock().map_err(|_| VirtualTextError::Unavailable)?;
            if revision != state.0 {
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
            _document: u128,
            revision: u64,
            characters: Range<u64>,
            value: &str,
            cancellation: &Cancellation,
        ) -> Result<u64, VirtualTextError> {
            if cancellation.is_cancelled() {
                return Err(VirtualTextError::Cancelled);
            }
            if value == "READONLY" {
                return Err(VirtualTextError::ReadOnly);
            }
            if value.len() > 12 {
                return Err(VirtualTextError::ReplacementBudget {
                    requested: value.len(),
                    limit: 12,
                });
            }
            let mut state = self.0.lock().map_err(|_| VirtualTextError::Unavailable)?;
            if revision != state.0 {
                return Err(VirtualTextError::StaleRevision {
                    requested: revision,
                    current: state.0,
                });
            }
            let char_count = state.1.chars().count() as u64;
            if characters.start > characters.end || characters.end > char_count {
                return Err(VirtualTextError::Range);
            }
            let byte_at = |value: &str, character: u64| {
                value
                    .char_indices()
                    .nth(character as usize)
                    .map_or(value.len(), |(byte, _)| byte)
            };
            let start = byte_at(&state.1, characters.start);
            let end = byte_at(&state.1, characters.end);
            state.1.replace_range(start..end, value);
            state.0 += 1;
            state.2 = None;
            state.3 = characters.start + value.chars().count() as u64;
            Ok(state.0)
        }
    }

    #[derive(Debug)]
    struct BlockingDocument {
        entered: Arc<std::sync::Barrier>,
    }

    impl VirtualTextProvider for BlockingDocument {
        fn summary(&self) -> Result<VirtualTextSummary, VirtualTextError> {
            Ok(VirtualTextSummary {
                document: 900,
                revision: 1,
                bytes: 1,
                characters: 1,
                selection_anchor: None,
                selection_focus: 0,
                read_only: false,
            })
        }

        fn read(
            &self,
            _document: u128,
            _revision: u64,
            _characters: Range<u64>,
            cancellation: &Cancellation,
        ) -> Result<VirtualTextRange, VirtualTextError> {
            self.entered.wait();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while !cancellation.is_cancelled() {
                if std::time::Instant::now() >= deadline {
                    return Err(VirtualTextError::Unavailable);
                }
                std::thread::yield_now();
            }
            Err(VirtualTextError::Cancelled)
        }

        fn set_selection(
            &self,
            _document: u128,
            _revision: u64,
            _anchor: Option<u64>,
            _focus: u64,
            _cancellation: &Cancellation,
        ) -> Result<u64, VirtualTextError> {
            Err(VirtualTextError::Unavailable)
        }

        fn replace(
            &self,
            _document: u128,
            _revision: u64,
            _characters: Range<u64>,
            _value: &str,
            _cancellation: &Cancellation,
        ) -> Result<u64, VirtualTextError> {
            Err(VirtualTextError::Unavailable)
        }
    }

    struct NoOpAction;
    impl ActionHandler for NoOpAction {
        fn do_action(&mut self, _request: ActionRequest) {}
    }

    struct Observe;
    impl AdapterCallback for Observe {
        fn register_interfaces(&self, adapter: &Adapter, id: NodeId, interfaces: InterfaceSet) {
            let _ = virtual_text::observe_platform_node(adapter.platform_node(id), interfaces);
        }
        fn unregister_interfaces(
            &self,
            _adapter: &Adapter,
            _id: NodeId,
            _interfaces: InterfaceSet,
        ) {
        }
        fn emit_event(&self, _adapter: &Adapter, _event: Event) {}
    }

    fn platform_node(
        provider: &Arc<dyn VirtualTextProvider>,
    ) -> (Adapter, virtual_text::VirtualTextRegistration, PlatformNode) {
        let adapter_id = next_adapter_id();
        let local = LocalNodeId(44);
        let registration =
            virtual_text::register_virtual_text_provider(adapter_id, TreeId::ROOT, local, provider)
                .unwrap();
        let update = TreeUpdate {
            nodes: vec![(local, Node::new(Role::TextInput))],
            tree: Some(Tree::new(local)),
            tree_id: TreeId::ROOT,
            focus: local,
        };
        let context = AppContext::new(None);
        let _scope = virtual_text::TreeUpdateScope::enter(adapter_id, TreeId::ROOT);
        let adapter = Adapter::with_id(
            adapter_id,
            &context,
            Observe,
            update,
            true,
            WindowBounds::default(),
            NoOpAction,
        );
        let node = adapter.platform_node(adapter.root_id());
        (adapter, registration, node)
    }

    async fn call<R>(
        connection: &zbus::Connection,
        interface: &str,
        method: &str,
        body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
    ) -> zbus::Result<R>
    where
        R: serde::de::DeserializeOwned + zbus::zvariant::Type,
    {
        let reply = connection
            .call_method(
                None::<&str>,
                "/org/a11y/atspi/accessible/2677/44",
                Some(interface),
                method,
                body,
            )
            .await?;
        reply.body().deserialize()
    }

    #[test]
    fn dbus_read_is_cancelled_during_provider_traversal() {
        futures_lite::future::block_on(async {
            let entered = Arc::new(std::sync::Barrier::new(2));
            let concrete = Arc::new(BlockingDocument {
                entered: Arc::clone(&entered),
            });
            let provider: Arc<dyn VirtualTextProvider> = concrete;
            let (_adapter, registration, node) = platform_node(&provider);
            let path = "/org/a11y/atspi/accessible/2677/44";
            let (server_stream, client_stream) = UnixStream::pair().unwrap();
            let guid = zbus::Guid::generate();
            let server_builder = Builder::unix_stream(server_stream)
                .server(guid)
                .unwrap()
                .p2p()
                .serve_at(path, TextInterface::new(node))
                .unwrap();
            let client_builder = Builder::unix_stream(client_stream).p2p();
            let (server, client) =
                futures_lite::future::zip(server_builder.build(), client_builder.build()).await;
            let server = server.unwrap();
            let client = client.unwrap();
            let worker = std::thread::spawn(move || {
                futures_lite::future::block_on(call::<String>(
                    &client,
                    "org.a11y.atspi.Text",
                    "GetText",
                    &(0_i32, 1_i32),
                ))
            });
            entered.wait();
            registration.cancel_pending_requests();
            let error = worker.join().unwrap().unwrap_err();
            assert!(error.to_string().contains("NoReply"));
            drop(server);
        });
    }

    #[test]
    fn ordinary_text_survives_late_provider_registration_and_teardown() {
        let adapter_id = next_adapter_id();
        let root = LocalNodeId(51);
        let run = LocalNodeId(52);
        let mut root_node = Node::new(Role::TextInput);
        root_node.set_children(vec![run]);
        let mut run_node = Node::new(Role::TextRun);
        run_node.set_value("ordinary");
        run_node.set_character_lengths(vec![1; 8]);
        let update = TreeUpdate {
            nodes: vec![(root, root_node), (run, run_node)],
            tree: Some(Tree::new(root)),
            tree_id: TreeId::ROOT,
            focus: root,
        };
        let context = AppContext::new(None);
        let _scope = virtual_text::TreeUpdateScope::enter(adapter_id, TreeId::ROOT);
        let adapter = Adapter::with_id(
            adapter_id,
            &context,
            Observe,
            update,
            true,
            WindowBounds::default(),
            NoOpAction,
        );
        let node = adapter.platform_node(adapter.root_id());
        let interface = TextInterface::new(node.clone());
        assert_eq!(interface.get_text(0, -1).unwrap(), "ordinary");

        let provider: Arc<dyn VirtualTextProvider> = Arc::new(Document::new("late provider"));
        let registration =
            virtual_text::register_virtual_text_provider(adapter_id, TreeId::ROOT, root, &provider)
                .unwrap();
        assert_eq!(interface.get_text(5, 8).unwrap(), "pro");
        drop(registration);
        assert_eq!(interface.get_text(0, -1).unwrap(), "ordinary");
    }

    #[test]
    fn object_server_routes_text_editing_and_typed_refusals() {
        futures_lite::future::block_on(async {
            let provider: Arc<dyn VirtualTextProvider> =
                Arc::new(Document::new("visible offscreen tail"));
            let (_adapter, registration, node) = platform_node(&provider);
            let bus_name = zbus::names::UniqueName::try_from(":1.2677").unwrap();
            let cache_item =
                crate::atspi::interfaces::cache_item_for_node(&bus_name, &node).unwrap();
            assert!(cache_item.ifaces.contains(atspi::Interface::Text));
            assert!(cache_item.ifaces.contains(atspi::Interface::EditableText));
            let path = "/org/a11y/atspi/accessible/2677/44";
            let (server_stream, client_stream) = UnixStream::pair().unwrap();
            let guid = zbus::Guid::generate();
            let server_builder = Builder::unix_stream(server_stream)
                .server(guid)
                .unwrap()
                .p2p()
                .serve_at(path, TextInterface::new(node.clone()))
                .unwrap()
                .serve_at(path, EditableTextInterface::new(node))
                .unwrap();
            let client_builder = Builder::unix_stream(client_stream).p2p();
            let (server, client) =
                futures_lite::future::zip(server_builder.build(), client_builder.build()).await;
            let server = server.unwrap();
            let client = client.unwrap();

            let value: String = call(&client, "org.a11y.atspi.Text", "GetText", &(8_i32, 17_i32))
                .await
                .unwrap();
            assert_eq!(value, "offscreen");
            let selected: bool = call(
                &client,
                "org.a11y.atspi.Text",
                "SetSelection",
                &(0_i32, 8_i32, 17_i32),
            )
            .await
            .unwrap();
            assert!(selected);
            let selected_range: (i32, i32) =
                call(&client, "org.a11y.atspi.Text", "GetSelection", &(0_i32,))
                    .await
                    .unwrap();
            assert_eq!(selected_range, (8, 17));
            let deleted: bool = call(
                &client,
                "org.a11y.atspi.EditableText",
                "DeleteText",
                &(8_i32, 17_i32),
            )
            .await
            .unwrap();
            assert!(deleted);
            let inserted: bool = call(
                &client,
                "org.a11y.atspi.EditableText",
                "InsertText",
                &(8_i32, "replacement", 11_i32),
            )
            .await
            .unwrap();
            assert!(inserted);
            let value: String = call(&client, "org.a11y.atspi.Text", "GetText", &(8_i32, 19_i32))
                .await
                .unwrap();
            assert_eq!(value, "replacement");
            let inserted_prefix: bool = call(
                &client,
                "org.a11y.atspi.EditableText",
                "InsertText",
                &(19_i32, "🙂x", 4_i32),
            )
            .await
            .unwrap();
            assert!(inserted_prefix);
            let value: String = call(&client, "org.a11y.atspi.Text", "GetText", &(19_i32, 20_i32))
                .await
                .unwrap();
            assert_eq!(value, "🙂");
            let character: (String, i32, i32) = call(
                &client,
                "org.a11y.atspi.Text",
                "GetStringAtOffset",
                &(19_i32, atspi::Granularity::Char),
            )
            .await
            .unwrap();
            assert_eq!(character, ("🙂".into(), 19, 20));

            for offsets in [(0_i32, 1_i32), (1, 2), (2, 3)] {
                let oversized = call::<String>(&client, "org.a11y.atspi.Text", "GetText", &offsets)
                    .await
                    .unwrap_err();
                assert!(oversized.to_string().contains("LimitsExceeded"));
            }
            let stale = call::<String>(&client, "org.a11y.atspi.Text", "GetText", &(3_i32, 4_i32))
                .await
                .unwrap_err();
            assert!(stale.to_string().contains("Failed"));
            let cancelled =
                call::<String>(&client, "org.a11y.atspi.Text", "GetText", &(4_i32, 5_i32))
                    .await
                    .unwrap_err();
            assert!(cancelled.to_string().contains("NoReply"));
            let invalid =
                call::<String>(&client, "org.a11y.atspi.Text", "GetText", &(-2_i32, 2_i32))
                    .await
                    .unwrap_err();
            assert!(invalid.to_string().contains("InvalidArgs"));
            let oversized_edit = call::<bool>(
                &client,
                "org.a11y.atspi.EditableText",
                "InsertText",
                &(0_i32, "payload exceeds", 15_i32),
            )
            .await
            .unwrap_err();
            assert!(oversized_edit.to_string().contains("LimitsExceeded"));
            let read_only = call::<bool>(
                &client,
                "org.a11y.atspi.EditableText",
                "InsertText",
                &(0_i32, "READONLY", 8_i32),
            )
            .await
            .unwrap_err();
            assert!(read_only.to_string().contains("AccessDenied"));

            let introspection: String = call(
                &client,
                "org.freedesktop.DBus.Introspectable",
                "Introspect",
                &(),
            )
            .await
            .unwrap();
            assert!(introspection.contains("org.a11y.atspi.Text"));
            assert!(introspection.contains("org.a11y.atspi.EditableText"));

            drop(registration);
            let cache_item = crate::atspi::interfaces::cache_item_for_node(
                &bus_name,
                &_adapter.platform_node(_adapter.root_id()),
            )
            .unwrap();
            assert!(!cache_item.ifaces.contains(atspi::Interface::Text));
            assert!(!cache_item.ifaces.contains(atspi::Interface::EditableText));
            drop(server);
        });
    }
}
