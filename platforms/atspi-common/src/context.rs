// Modified by enzUI contributors. See FORK.md.
// Copyright 2023 The AccessKit Authors. All rights reserved.
// Licensed under the Apache License, Version 2.0 (found in
// the LICENSE-APACHE file) or the MIT license (found in
// the LICENSE-MIT file), at your option.

use accesskit::{ActionHandler, ActionRequest};
use accesskit_consumer::Tree;
use std::fmt::{Debug, Formatter};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::WindowBounds;

/// This is an implementation detail of `accesskit_unix`, required for robust
/// state transitions with minimal overhead.
pub trait ActionHandlerNoMut {
    fn do_action(&self, request: ActionRequest);
}

/// This is an implementation detail of `accesskit_unix`, required for robust
/// state transitions with minimal overhead.
pub struct ActionHandlerWrapper<H: ActionHandler + Send>(Mutex<H>);

impl<H: 'static + ActionHandler + Send> ActionHandlerWrapper<H> {
    pub fn new(inner: H) -> Self {
        Self(Mutex::new(inner))
    }
}

impl<H: ActionHandler + Send> ActionHandlerNoMut for ActionHandlerWrapper<H> {
    fn do_action(&self, request: ActionRequest) {
        self.0.lock().unwrap().do_action(request)
    }
}

pub(crate) struct Context {
    pub(crate) app_context: Arc<RwLock<AppContext>>,
    pub(crate) tree: RwLock<Tree>,
    pub(crate) action_handler: Arc<dyn ActionHandlerNoMut + Send + Sync>,
    pub(crate) root_window_bounds: RwLock<WindowBounds>,
}

impl Debug for Context {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("app_context", &self.app_context)
            .field("tree", &self.tree)
            .field("action_handler", &"ActionHandler")
            .field("root_window_bounds", &self.root_window_bounds)
            .finish()
    }
}

impl Context {
    pub(crate) fn new(
        app_context: &Arc<RwLock<AppContext>>,
        tree: Tree,
        action_handler: Arc<dyn ActionHandlerNoMut + Send + Sync>,
        root_window_bounds: WindowBounds,
    ) -> Arc<Self> {
        Arc::new(Self {
            app_context: Arc::clone(app_context),
            tree: RwLock::new(tree),
            action_handler,
            root_window_bounds: RwLock::new(root_window_bounds),
        })
    }

    pub(crate) fn read_tree(&self) -> RwLockReadGuard<'_, Tree> {
        self.tree.read().unwrap()
    }

    pub(crate) fn read_root_window_bounds(&self) -> RwLockReadGuard<'_, WindowBounds> {
        self.root_window_bounds.read().unwrap()
    }

    pub fn do_action(&self, request: ActionRequest) {
        self.action_handler.do_action(request);
    }

    pub(crate) fn read_app_context(&self) -> RwLockReadGuard<'_, AppContext> {
        self.app_context.read().unwrap()
    }

    pub(crate) fn write_app_context(&self) -> RwLockWriteGuard<'_, AppContext> {
        self.app_context.write().unwrap()
    }
}

#[derive(Debug)]
pub struct AppContext {
    pub(crate) name: Option<String>,
    pub(crate) toolkit_name: Option<String>,
    pub(crate) toolkit_version: Option<String>,
    pub(crate) id: Option<i32>,
    // Sorted by adapter id: the AT-SPI child order of the application root and
    // the domain of `adapter_index`'s binary search.
    pub(crate) adapters: Vec<(usize, Arc<Context>)>,
}

impl AppContext {
    pub fn new(name: Option<String>) -> Arc<RwLock<Self>> {
        Arc::new(RwLock::new(Self {
            name,
            toolkit_name: None,
            toolkit_version: None,
            id: None,
            adapters: Vec::new(),
        }))
    }

    pub(crate) fn adapter_index(&self, id: usize) -> Result<usize, usize> {
        self.adapters.binary_search_by(|adapter| adapter.0.cmp(&id))
    }

    pub(crate) fn push_adapter(&mut self, id: usize, context: &Arc<Context>) {
        match self.adapter_index(id) {
            // Ids are handed out at window creation but registered when each
            // window first publishes a tree, so a later window can register
            // first; appending would leave the search domain unsorted.
            Err(index) => self.adapters.insert(index, (id, Arc::clone(context))),
            // `next_adapter_id` guarantees one live adapter per id, so a
            // duplicate replaces rather than adding an entry no index lookup
            // could distinguish.
            Ok(index) => self.adapters[index] = (id, Arc::clone(context)),
        }
    }

    pub(crate) fn remove_adapter(&mut self, id: usize) {
        if let Ok(index) = self.adapter_index(id) {
            self.adapters.remove(index);
        }
    }
}

#[cfg(test)]
mod adapter_registry {
    use super::AppContext;
    use crate::{
        Adapter, AdapterCallback, Event, InterfaceSet, NodeId, NodeIdOrRoot, ObjectEvent,
        PlatformRoot, WindowBounds,
    };
    use accesskit::{
        ActionHandler, ActionRequest, Node, NodeId as LocalNodeId, Role, Tree, TreeId, TreeUpdate,
    };
    use std::sync::{Arc, Mutex, RwLock};

    #[derive(Clone, Default)]
    struct ChildAdditions(Arc<Mutex<Vec<(usize, usize)>>>);

    impl ChildAdditions {
        // Announced child index paired with the adapter that announced it.
        fn announced(&self) -> Vec<(usize, usize)> {
            self.0.lock().unwrap().clone()
        }
    }

    impl AdapterCallback for ChildAdditions {
        fn register_interfaces(&self, _: &Adapter, _: NodeId, _: InterfaceSet) {}

        fn unregister_interfaces(&self, _: &Adapter, _: NodeId, _: InterfaceSet) {}

        fn emit_event(&self, adapter: &Adapter, event: Event) {
            if let Event::Object {
                target: NodeIdOrRoot::Root,
                event: ObjectEvent::ChildAdded(index, _),
            } = event
            {
                self.0.lock().unwrap().push((index, adapter.id()));
            }
        }
    }

    struct NoOpActionHandler;

    impl ActionHandler for NoOpActionHandler {
        fn do_action(&mut self, _: ActionRequest) {}
    }

    fn window(root: LocalNodeId) -> TreeUpdate {
        TreeUpdate {
            nodes: vec![(root, Node::new(Role::Window))],
            tree: Some(Tree::new(root)),
            tree_id: TreeId::ROOT,
            focus: root,
        }
    }

    // A window publishing its first tree, which is what registers an adapter
    // that was given `id` when its window was created.
    fn activate(
        app_context: &Arc<RwLock<AppContext>>,
        id: usize,
        events: &ChildAdditions,
    ) -> Adapter {
        activate_window(app_context, id, LocalNodeId(id as u64 * 10 + 1), events)
    }

    fn activate_window(
        app_context: &Arc<RwLock<AppContext>>,
        id: usize,
        root: LocalNodeId,
        events: &ChildAdditions,
    ) -> Adapter {
        Adapter::with_id(
            id,
            app_context,
            events.clone(),
            window(root),
            false,
            WindowBounds::default(),
            NoOpActionHandler,
        )
    }

    fn child_adapter_ids(root: &PlatformRoot) -> Vec<usize> {
        (0..root.child_count().unwrap() as usize)
            .map(|index| root.child_id_at_index(index).unwrap().unwrap().0)
            .collect()
    }

    fn child_root_ids(root: &PlatformRoot) -> Vec<NodeId> {
        (0..root.child_count().unwrap() as usize)
            .map(|index| root.child_id_at_index(index).unwrap().unwrap().1)
            .collect()
    }

    // What each child answers when asked where it sits, which must agree with
    // the position it was reached at.
    fn reported_indexes(root: &PlatformRoot) -> Vec<i32> {
        (0..root.child_count().unwrap() as usize)
            .map(|index| {
                root.child_at_index(index)
                    .unwrap()
                    .unwrap()
                    .index_in_parent()
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn windows_activating_in_ascending_id_order_all_register() {
        let app_context = AppContext::new(None);
        let events = ChildAdditions::default();
        let _first = activate(&app_context, 0, &events);
        let _second = activate(&app_context, 1, &events);
        let _third = activate(&app_context, 2, &events);

        let root = PlatformRoot::new(&app_context);
        assert_eq!(child_adapter_ids(&root), vec![0, 1, 2]);
        assert_eq!(reported_indexes(&root), vec![0, 1, 2]);
        assert_eq!(events.announced(), vec![(0, 0), (1, 1), (2, 2)]);
    }

    #[test]
    fn a_single_window_registers_at_the_application_root() {
        let app_context = AppContext::new(None);
        let events = ChildAdditions::default();
        let only = activate(&app_context, 3, &events);

        let root = PlatformRoot::new(&app_context);
        assert_eq!(child_adapter_ids(&root), vec![3]);
        assert_eq!(child_root_ids(&root), vec![only.root_id()]);
        assert_eq!(reported_indexes(&root), vec![0]);
        assert_eq!(events.announced(), vec![(0, 3)]);
    }

    #[test]
    fn a_later_window_activating_first_keeps_both_windows_navigable() {
        let app_context = AppContext::new(None);
        let events = ChildAdditions::default();
        let later = activate(&app_context, 2, &events);
        let earlier = activate(&app_context, 0, &events);

        let root = PlatformRoot::new(&app_context);
        assert_eq!(child_adapter_ids(&root), vec![0, 2]);
        assert_eq!(
            child_root_ids(&root),
            vec![earlier.root_id(), later.root_id()]
        );
        assert_eq!(reported_indexes(&root), vec![0, 1]);

        drop(later);
        assert_eq!(child_adapter_ids(&root), vec![0]);
        drop(earlier);
        assert_eq!(root.child_count().unwrap(), 0);
    }

    #[test]
    fn three_windows_activating_in_descending_id_order_all_register() {
        let app_context = AppContext::new(None);
        let events = ChildAdditions::default();
        let third = activate(&app_context, 2, &events);
        let second = activate(&app_context, 1, &events);
        let first = activate(&app_context, 0, &events);

        let root = PlatformRoot::new(&app_context);
        assert_eq!(child_adapter_ids(&root), vec![0, 1, 2]);
        assert_eq!(
            child_root_ids(&root),
            vec![first.root_id(), second.root_id(), third.root_id()]
        );
        assert_eq!(reported_indexes(&root), vec![0, 1, 2]);
        assert_eq!(events.announced(), vec![(0, 2), (0, 1), (0, 0)]);
    }

    #[test]
    fn reactivating_a_middle_window_restores_its_child_position() {
        let app_context = AppContext::new(None);
        let events = ChildAdditions::default();
        let _first = activate(&app_context, 0, &events);
        let middle = activate(&app_context, 1, &events);
        let _last = activate(&app_context, 2, &events);

        let root = PlatformRoot::new(&app_context);
        drop(middle);
        assert_eq!(child_adapter_ids(&root), vec![0, 2]);

        let middle = activate(&app_context, 1, &events);
        assert_eq!(child_adapter_ids(&root), vec![0, 1, 2]);
        assert_eq!(child_root_ids(&root)[1], middle.root_id());
        assert_eq!(reported_indexes(&root), vec![0, 1, 2]);
    }

    #[test]
    fn a_reused_id_leaves_one_child_for_that_id() {
        let app_context = AppContext::new(None);
        let events = ChildAdditions::default();
        let _replaced = activate(&app_context, 4, &events);
        let replacement = activate_window(&app_context, 4, LocalNodeId(999), &events);

        let root = PlatformRoot::new(&app_context);
        assert_eq!(child_adapter_ids(&root), vec![4]);
        assert_eq!(child_root_ids(&root), vec![replacement.root_id()]);
    }
}
