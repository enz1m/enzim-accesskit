// Modified by enzUI contributors. See FORK.md.
// Copyright 2023 The AccessKit Authors. All rights reserved.
// Licensed under the Apache License, Version 2.0 (found in
// the LICENSE-APACHE file) or the MIT license (found in
// the LICENSE-MIT file), at your option.

use accesskit::{ActivationHandler, DeactivationHandler};
use accesskit_atspi_common::{Adapter as AdapterImpl, AppContext, Event};
#[cfg(not(feature = "tokio"))]
use async_channel::{Receiver, Sender};
use atspi::proxy::bus::StatusProxy;
#[cfg(not(feature = "tokio"))]
use futures_util::{StreamExt, pin_mut as pin, select};
use std::{
    sync::{Arc, Mutex, OnceLock, RwLock},
    thread,
};
#[cfg(feature = "tokio")]
use tokio::{
    pin, select,
    sync::mpsc::{UnboundedReceiver as Receiver, UnboundedSender as Sender},
};
#[cfg(feature = "tokio")]
use tokio_stream::{StreamExt, wrappers::UnboundedReceiverStream};
use zbus::{Connection, connection::Builder, proxy::PropertyChanged};

use crate::{
    adapter::{AdapterState, Callback, Message},
    atspi::{Bus, map_or_ignoring_recoverable_error, zbus_error_is_unrecoverable},
    executor::Executor,
    util::block_on,
};

static APP_CONTEXT: OnceLock<Arc<RwLock<AppContext>>> = OnceLock::new();
static MESSAGES: OnceLock<Sender<Message>> = OnceLock::new();

fn app_name() -> Option<String> {
    std::env::current_exe().ok().and_then(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_string())
    })
}

pub(crate) fn get_or_init_app_context<'a>() -> &'a Arc<RwLock<AppContext>> {
    APP_CONTEXT.get_or_init(|| AppContext::new(app_name()))
}

pub(crate) fn get_or_init_messages() -> Sender<Message> {
    MESSAGES
        .get_or_init(|| {
            #[cfg(not(feature = "tokio"))]
            let (tx, rx) = async_channel::unbounded();
            #[cfg(feature = "tokio")]
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

            thread::spawn(|| {
                let executor = Executor::new();
                block_on(executor.run(async {
                    if let Ok(session_bus) = Builder::session() {
                        if let Ok(session_bus) = session_bus.internal_executor(false).build().await
                        {
                            if let Err(error) = run_event_loop(&executor, session_bus, rx).await {
                                if zbus_error_is_unrecoverable(&error) {
                                    panic!("Accessibility event loop failed: {error}");
                                }
                            }
                        }
                    }
                }))
            });

            tx
        })
        .clone()
}

struct AdapterEntry {
    id: usize,
    activation_handler: Box<dyn ActivationHandler>,
    deactivation_handler: Box<dyn DeactivationHandler>,
    state: Arc<Mutex<AdapterState>>,
}

// Sorted by adapter id: the domain of `RemoveAdapter`'s binary search and the
// order every adapter is activated in when accessibility turns on.
fn insert_adapter_entry(adapters: &mut Vec<AdapterEntry>, entry: AdapterEntry) -> usize {
    let id = entry.id;
    match adapters.binary_search_by(|candidate| candidate.id.cmp(&id)) {
        // Ids are taken when a window is created and this message is sent from
        // that thread, so a higher id can arrive first; appending would leave
        // the entry unfindable and the adapter would outlive its window.
        Err(index) => {
            adapters.insert(index, entry);
            index
        }
        // One live adapter per id, so an id seen again belongs to a window
        // whose adapter is already gone.
        Ok(index) => {
            adapters[index] = entry;
            index
        }
    }
}

fn activate_adapter(entry: &mut AdapterEntry) {
    let mut state = entry.state.lock().unwrap();
    if let AdapterState::Inactive {
        is_window_focused,
        root_window_bounds,
        action_handler,
    } = &*state
    {
        *state = match entry.activation_handler.request_initial_tree() {
            Some(initial_state) => {
                let _scope =
                    crate::virtual_text::TreeUpdateScope::enter(entry.id, initial_state.tree_id);
                let r#impl = AdapterImpl::with_wrapped_action_handler(
                    entry.id,
                    get_or_init_app_context(),
                    Callback::new(),
                    initial_state,
                    *is_window_focused,
                    *root_window_bounds,
                    Arc::clone(action_handler),
                );
                AdapterState::Active(r#impl)
            }
            None => AdapterState::Pending {
                is_window_focused: *is_window_focused,
                root_window_bounds: *root_window_bounds,
                action_handler: Arc::clone(action_handler),
            },
        };
    }
}

fn deactivate_adapter(entry: &mut AdapterEntry) {
    let mut state = entry.state.lock().unwrap();
    match &*state {
        AdapterState::Inactive { .. } => (),
        AdapterState::Pending {
            is_window_focused,
            root_window_bounds,
            action_handler,
        } => {
            *state = AdapterState::Inactive {
                is_window_focused: *is_window_focused,
                root_window_bounds: *root_window_bounds,
                action_handler: Arc::clone(action_handler),
            };
            drop(state);
            entry.deactivation_handler.deactivate_accessibility();
        }
        AdapterState::Active(r#impl) => {
            *state = AdapterState::Inactive {
                is_window_focused: r#impl.is_window_focused(),
                root_window_bounds: r#impl.root_window_bounds(),
                action_handler: r#impl.wrapped_action_handler(),
            };
            drop(state);
            entry.deactivation_handler.deactivate_accessibility();
        }
    }
}

async fn bus_after_status_change(
    change: Option<PropertyChanged<'_, bool>>,
    session_bus: &Connection,
    executor: &Executor<'_>,
) -> zbus::Result<Option<Bus>> {
    let enabled = match change {
        Some(change) => change.get().await?,
        None => false,
    };
    if enabled {
        map_or_ignoring_recoverable_error(Bus::new(session_bus, executor).await, None, Some)
    } else {
        Ok(None)
    }
}

fn sync_adapters(adapters: &mut [AdapterEntry], atspi_bus: &Option<Bus>) {
    let active = atspi_bus.is_some();
    for entry in adapters {
        if active {
            activate_adapter(entry);
        } else {
            deactivate_adapter(entry);
        }
    }
}

async fn run_event_loop(
    executor: &Executor<'_>,
    session_bus: Connection,
    rx: Receiver<Message>,
) -> zbus::Result<()> {
    let session_bus_copy = session_bus.clone();
    let _session_bus_task = executor.spawn(
        async move {
            loop {
                session_bus_copy.executor().tick().await;
            }
        },
        "accesskit_session_bus_task",
    );

    let status = StatusProxy::new(&session_bus).await?;
    let changes = status.receive_is_enabled_changed().await.fuse();
    pin!(changes);

    #[cfg(not(feature = "tokio"))]
    let messages = rx.fuse();
    #[cfg(feature = "tokio")]
    let messages = UnboundedReceiverStream::new(rx).fuse();
    pin!(messages);

    let mut atspi_bus = None;
    let mut adapters: Vec<AdapterEntry> = Vec::new();

    loop {
        select! {
            change = changes.next() => {
                atspi_bus = bus_after_status_change(change, &session_bus, executor).await?;
                sync_adapters(&mut adapters, &atspi_bus);

            }
            message = messages.next() => {
                if let Some(message) = message {
                    process_adapter_message(&atspi_bus, &mut adapters, message).await?;
                }
            }
        }
    }
}

async fn process_adapter_message(
    atspi_bus: &Option<Bus>,
    adapters: &mut Vec<AdapterEntry>,
    message: Message,
) -> zbus::Result<()> {
    match message {
        Message::AddAdapter {
            id,
            activation_handler,
            deactivation_handler,
            state,
        } => {
            let index = insert_adapter_entry(
                adapters,
                AdapterEntry {
                    id,
                    activation_handler,
                    deactivation_handler,
                    state,
                },
            );
            if atspi_bus.is_some() {
                activate_adapter(&mut adapters[index]);
            }
        }
        Message::RemoveAdapter { id } => {
            if let Ok(index) = adapters.binary_search_by(|entry| entry.id.cmp(&id)) {
                adapters.remove(index);
            }
        }
        Message::RegisterInterfaces { node, interfaces } => {
            if let Some(bus) = atspi_bus {
                bus.register_interfaces(node, interfaces).await?
            }
        }
        Message::UnregisterInterfaces {
            adapter_id,
            node_id,
            interfaces,
        } => {
            if let Some(bus) = atspi_bus {
                bus.unregister_interfaces(adapter_id, node_id, interfaces)
                    .await?
            }
        }
        Message::EmitEvent {
            adapter_id,
            event: Event::Object { target, event },
        } => {
            if let Some(bus) = atspi_bus {
                bus.emit_object_event(adapter_id, target, event).await?
            }
        }
        Message::EmitEvent {
            adapter_id,
            event:
                Event::Window {
                    target,
                    name,
                    event,
                },
        } => {
            if let Some(bus) = atspi_bus {
                bus.emit_window_event(adapter_id, target, name, event)
                    .await?;
            }
        }
        Message::EmitEvent {
            event: Event::Cache(_),
            ..
        } => unreachable!("cache events are sent as EmitCacheAdd/EmitCacheRemove"),
        Message::EmitCacheAdd { node } => {
            if let Some(bus) = atspi_bus {
                bus.emit_cache_add(node).await?;
            }
        }
        Message::EmitCacheRemove {
            adapter_id,
            node_id,
        } => {
            if let Some(bus) = atspi_bus {
                bus.emit_cache_remove(adapter_id, node_id).await?;
            }
        }
        Message::RefreshVirtualTextInterfaces { refresh } => {
            if let Some(bus) = atspi_bus {
                bus.refresh_virtual_text_interfaces(refresh).await?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod adapter_entry_registry {
    use super::{AdapterEntry, AdapterState, Bus, Message, block_on, process_adapter_message};
    use accesskit::{
        ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, TreeUpdate,
    };
    use accesskit_atspi_common::{ActionHandlerWrapper, WindowBounds};
    use std::sync::{Arc, Mutex};

    struct NeverActivates;

    impl ActivationHandler for NeverActivates {
        fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
            None
        }
    }

    struct NoOpDeactivation;

    impl DeactivationHandler for NoOpDeactivation {
        fn deactivate_accessibility(&mut self) {}
    }

    struct NoOpActionHandler;

    impl ActionHandler for NoOpActionHandler {
        fn do_action(&mut self, _: ActionRequest) {}
    }

    fn add_adapter(id: usize) -> Message {
        Message::AddAdapter {
            id,
            activation_handler: Box::new(NeverActivates),
            deactivation_handler: Box::new(NoOpDeactivation),
            state: Arc::new(Mutex::new(AdapterState::Inactive {
                is_window_focused: false,
                root_window_bounds: WindowBounds::default(),
                action_handler: Arc::new(ActionHandlerWrapper::new(NoOpActionHandler)),
            })),
        }
    }

    // With no bus the message loop only maintains its own registry, so these
    // deliveries reach neither D-Bus nor an activation handler.
    fn deliver(adapters: &mut Vec<AdapterEntry>, message: Message) {
        let no_bus: Option<Bus> = None;
        block_on(process_adapter_message(&no_bus, adapters, message))
            .expect("registry maintenance performs no fallible D-Bus work");
    }

    fn ids(adapters: &[AdapterEntry]) -> Vec<usize> {
        adapters.iter().map(|entry| entry.id).collect()
    }

    #[test]
    fn adapters_added_out_of_id_order_stay_removable() {
        let mut adapters = Vec::new();
        deliver(&mut adapters, add_adapter(2));
        deliver(&mut adapters, add_adapter(0));
        deliver(&mut adapters, add_adapter(1));
        assert_eq!(ids(&adapters), vec![0, 1, 2]);

        deliver(&mut adapters, Message::RemoveAdapter { id: 2 });
        assert_eq!(ids(&adapters), vec![0, 1]);
        deliver(&mut adapters, Message::RemoveAdapter { id: 0 });
        assert_eq!(ids(&adapters), vec![1]);
        deliver(&mut adapters, Message::RemoveAdapter { id: 1 });
        assert!(ids(&adapters).is_empty());
    }

    #[test]
    fn adapters_added_in_id_order_stay_removable() {
        let mut adapters = Vec::new();
        deliver(&mut adapters, add_adapter(0));
        deliver(&mut adapters, add_adapter(1));
        deliver(&mut adapters, add_adapter(2));
        assert_eq!(ids(&adapters), vec![0, 1, 2]);

        deliver(&mut adapters, Message::RemoveAdapter { id: 1 });
        assert_eq!(ids(&adapters), vec![0, 2]);
        deliver(&mut adapters, Message::RemoveAdapter { id: 0 });
        assert_eq!(ids(&adapters), vec![2]);
        deliver(&mut adapters, Message::RemoveAdapter { id: 2 });
        assert!(ids(&adapters).is_empty());
    }

    #[test]
    fn a_reused_id_leaves_one_entry() {
        let mut adapters = Vec::new();
        deliver(&mut adapters, add_adapter(5));
        deliver(&mut adapters, add_adapter(5));
        assert_eq!(ids(&adapters), vec![5]);

        deliver(&mut adapters, Message::RemoveAdapter { id: 5 });
        assert!(ids(&adapters).is_empty());
    }
}
