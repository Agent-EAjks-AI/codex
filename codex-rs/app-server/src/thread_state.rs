use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::ConnectionRequestId;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequestPayload;
use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnError;
use codex_core::CodexThread;
use codex_core::ThreadConfigSnapshot;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use uuid::Uuid;

type PendingInterruptQueue = Vec<(
    ConnectionRequestId,
    crate::codex_message_processor::ApiVersion,
)>;

pub(crate) struct PendingThreadResumeRequest {
    pub(crate) request_id: ConnectionRequestId,
    pub(crate) rollout_path: PathBuf,
    pub(crate) config_snapshot: ThreadConfigSnapshot,
}

pub(crate) enum ThreadListenerCommand {
    SendThreadResumeResponse(PendingThreadResumeRequest),
}

#[derive(Clone)]
pub(crate) struct PendingItemRequest {
    pub(crate) item_id: String,
    pub(crate) request_id: RequestId,
    pub(crate) payload: ServerRequestPayload,
    delivered_to_connections: HashSet<ConnectionId>,
}

impl PendingItemRequest {
    pub(crate) fn new(
        item_id: String,
        request_id: RequestId,
        payload: ServerRequestPayload,
        delivered_to_connections: &[ConnectionId],
    ) -> Self {
        Self {
            item_id,
            request_id,
            payload,
            delivered_to_connections: delivered_to_connections.iter().copied().collect(),
        }
    }

    pub(crate) fn needs_delivery_to(&self, connection_id: ConnectionId) -> bool {
        !self.delivered_to_connections.contains(&connection_id)
    }

    pub(crate) fn mark_delivered_if_request_matches(
        &mut self,
        request_id: &RequestId,
        connection_id: ConnectionId,
    ) {
        if self.request_id == *request_id {
            self.delivered_to_connections.insert(connection_id);
        }
    }
}

/// Per-conversation accumulation of the latest states e.g. error message while a turn runs.
#[derive(Default, Clone)]
pub(crate) struct TurnSummary {
    pub(crate) file_change_started: HashSet<String>,
    pub(crate) command_execution_started: HashSet<String>,
    pub(crate) last_error: Option<TurnError>,
}

#[derive(Default)]
pub(crate) struct ThreadState {
    pub(crate) pending_interrupts: PendingInterruptQueue,
    pub(crate) pending_rollbacks: Option<ConnectionRequestId>,
    pending_item_requests: HashMap<String, PendingItemRequest>,
    pending_item_request_locks: HashMap<String, Arc<Mutex<()>>>,
    pub(crate) turn_summary: TurnSummary,
    pub(crate) cancel_tx: Option<oneshot::Sender<()>>,
    pub(crate) experimental_raw_events: bool,
    pub(crate) listener_generation: u64,
    listener_command_tx: Option<mpsc::UnboundedSender<ThreadListenerCommand>>,
    current_turn_history: ThreadHistoryBuilder,
    listener_thread: Option<Weak<CodexThread>>,
    subscribed_connections: HashSet<ConnectionId>,
}

impl ThreadState {
    pub(crate) fn listener_matches(&self, conversation: &Arc<CodexThread>) -> bool {
        self.listener_thread
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|existing| Arc::ptr_eq(&existing, conversation))
    }

    pub(crate) fn set_listener(
        &mut self,
        cancel_tx: oneshot::Sender<()>,
        conversation: &Arc<CodexThread>,
    ) -> (mpsc::UnboundedReceiver<ThreadListenerCommand>, u64) {
        if let Some(previous) = self.cancel_tx.replace(cancel_tx) {
            let _ = previous.send(());
        }
        self.listener_generation = self.listener_generation.wrapping_add(1);
        let (listener_command_tx, listener_command_rx) = mpsc::unbounded_channel();
        self.listener_command_tx = Some(listener_command_tx);
        self.listener_thread = Some(Arc::downgrade(conversation));
        (listener_command_rx, self.listener_generation)
    }

    pub(crate) fn clear_listener(&mut self) {
        if let Some(cancel_tx) = self.cancel_tx.take() {
            let _ = cancel_tx.send(());
        }
        self.listener_command_tx = None;
        self.current_turn_history.reset();
        self.listener_thread = None;
    }

    pub(crate) fn add_connection(&mut self, connection_id: ConnectionId) {
        self.subscribed_connections.insert(connection_id);
    }

    pub(crate) fn remove_connection(&mut self, connection_id: ConnectionId) {
        self.subscribed_connections.remove(&connection_id);
    }

    pub(crate) fn subscribed_connection_ids(&self) -> Vec<ConnectionId> {
        self.subscribed_connections.iter().copied().collect()
    }

    pub(crate) fn set_experimental_raw_events(&mut self, enabled: bool) {
        self.experimental_raw_events = enabled;
    }

    pub(crate) fn listener_command_tx(
        &self,
    ) -> Option<mpsc::UnboundedSender<ThreadListenerCommand>> {
        self.listener_command_tx.clone()
    }

    pub(crate) fn active_turn_snapshot(&self) -> Option<Turn> {
        self.current_turn_history.active_turn_snapshot()
    }

    pub(crate) fn track_current_turn_event(&mut self, event: &EventMsg) {
        self.current_turn_history.handle_event(event);
        if !self.current_turn_history.has_active_turn() {
            self.current_turn_history.reset();
        }
    }

    fn pending_item_requests_for_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Vec<PendingItemRequest> {
        self.pending_item_requests
            .values()
            .filter(|request| request.needs_delivery_to(connection_id))
            .cloned()
            .collect()
    }

    fn pending_item_request_lock(&mut self, item_id: &str) -> Arc<Mutex<()>> {
        self.pending_item_request_locks
            .entry(item_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn item_request_lock(
        thread_state: &Arc<Mutex<ThreadState>>,
        item_id: &str,
    ) -> Arc<Mutex<()>> {
        let mut state = thread_state.lock().await;
        state.pending_item_request_lock(item_id)
    }

    pub(crate) async fn with_pending_item_request<R, F, Fut>(
        thread_state: &Arc<Mutex<ThreadState>>,
        item_id: &str,
        f: F,
    ) -> R
    where
        F: FnOnce(Option<PendingItemRequest>) -> Fut,
        Fut: Future<Output = (Option<PendingItemRequest>, R)>,
    {
        let pending_item_request_lock = Self::item_request_lock(thread_state, item_id).await;
        let _serialization_guard = pending_item_request_lock.lock_owned().await;

        let pending_item_request = {
            let mut state = thread_state.lock().await;
            state.pending_item_requests.remove(item_id)
        };

        let (pending_item_request, result) = f(pending_item_request).await;

        let mut state = thread_state.lock().await;
        if let Some(mut request) = pending_item_request {
            request.item_id = item_id.to_string();
            state
                .pending_item_requests
                .insert(item_id.to_string(), request);
        }
        result
    }

    pub(crate) async fn pending_item_ids_for_connection(
        thread_state: &Arc<Mutex<ThreadState>>,
        connection_id: ConnectionId,
    ) -> Vec<String> {
        let state = thread_state.lock().await;
        state
            .pending_item_requests_for_connection(connection_id)
            .into_iter()
            .map(|request| request.item_id)
            .collect()
    }
}

#[derive(Clone, Copy)]
struct SubscriptionState {
    thread_id: ThreadId,
    connection_id: ConnectionId,
}

#[derive(Default)]
pub(crate) struct ThreadStateManager {
    thread_states: HashMap<ThreadId, Arc<Mutex<ThreadState>>>,
    subscription_state_by_id: HashMap<Uuid, SubscriptionState>,
    thread_ids_by_connection: HashMap<ConnectionId, HashSet<ThreadId>>,
}

impl ThreadStateManager {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn thread_state(&mut self, thread_id: ThreadId) -> Arc<Mutex<ThreadState>> {
        self.thread_states
            .entry(thread_id)
            .or_insert_with(|| Arc::new(Mutex::new(ThreadState::default())))
            .clone()
    }

    pub(crate) async fn remove_listener(&mut self, subscription_id: Uuid) -> Option<ThreadId> {
        let subscription_state = self.subscription_state_by_id.remove(&subscription_id)?;
        let thread_id = subscription_state.thread_id;

        let connection_still_subscribed_to_thread =
            self.subscription_state_by_id.values().any(|state| {
                state.thread_id == thread_id
                    && state.connection_id == subscription_state.connection_id
            });
        if !connection_still_subscribed_to_thread {
            let mut remove_connection_entry = false;
            if let Some(thread_ids) = self
                .thread_ids_by_connection
                .get_mut(&subscription_state.connection_id)
            {
                thread_ids.remove(&thread_id);
                remove_connection_entry = thread_ids.is_empty();
            }
            if remove_connection_entry {
                self.thread_ids_by_connection
                    .remove(&subscription_state.connection_id);
            }
        }

        if let Some(thread_state) = self.thread_states.get(&thread_id) {
            let mut thread_state = thread_state.lock().await;
            if !connection_still_subscribed_to_thread {
                thread_state.remove_connection(subscription_state.connection_id);
            }
            if thread_state.subscribed_connection_ids().is_empty() {
                tracing::debug!(
                    thread_id = %thread_id,
                    subscription_id = %subscription_id,
                    connection_id = ?subscription_state.connection_id,
                    listener_generation = thread_state.listener_generation,
                    "retaining thread listener after last subscription removed"
                );
            }
        }
        Some(thread_id)
    }

    pub(crate) async fn remove_thread_state(&mut self, thread_id: ThreadId) {
        if let Some(thread_state) = self.thread_states.remove(&thread_id) {
            let mut thread_state = thread_state.lock().await;
            tracing::debug!(
                thread_id = %thread_id,
                listener_generation = thread_state.listener_generation,
                had_listener = thread_state.cancel_tx.is_some(),
                had_active_turn = thread_state.active_turn_snapshot().is_some(),
                "clearing thread listener during thread-state teardown"
            );
            thread_state.clear_listener();
        }
        self.subscription_state_by_id
            .retain(|_, state| state.thread_id != thread_id);
        self.thread_ids_by_connection.retain(|_, thread_ids| {
            thread_ids.remove(&thread_id);
            !thread_ids.is_empty()
        });
    }

    pub(crate) async fn set_listener(
        &mut self,
        subscription_id: Uuid,
        thread_id: ThreadId,
        connection_id: ConnectionId,
        experimental_raw_events: bool,
    ) -> Arc<Mutex<ThreadState>> {
        self.subscription_state_by_id.insert(
            subscription_id,
            SubscriptionState {
                thread_id,
                connection_id,
            },
        );
        self.thread_ids_by_connection
            .entry(connection_id)
            .or_default()
            .insert(thread_id);
        let thread_state = self.thread_state(thread_id);
        {
            let mut thread_state_guard = thread_state.lock().await;
            thread_state_guard.add_connection(connection_id);
            thread_state_guard.set_experimental_raw_events(experimental_raw_events);
        }
        thread_state
    }

    pub(crate) async fn ensure_connection_subscribed(
        &mut self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
        experimental_raw_events: bool,
    ) -> Arc<Mutex<ThreadState>> {
        self.thread_ids_by_connection
            .entry(connection_id)
            .or_default()
            .insert(thread_id);
        let thread_state = self.thread_state(thread_id);
        {
            let mut thread_state_guard = thread_state.lock().await;
            thread_state_guard.add_connection(connection_id);
            if experimental_raw_events {
                thread_state_guard.set_experimental_raw_events(true);
            }
        }
        thread_state
    }

    pub(crate) async fn remove_connection(&mut self, connection_id: ConnectionId) {
        let thread_ids = self
            .thread_ids_by_connection
            .remove(&connection_id)
            .unwrap_or_default();
        self.subscription_state_by_id
            .retain(|_, state| state.connection_id != connection_id);

        if thread_ids.is_empty() {
            for thread_state in self.thread_states.values() {
                let mut thread_state = thread_state.lock().await;
                thread_state.remove_connection(connection_id);
                if thread_state.subscribed_connection_ids().is_empty() {
                    tracing::debug!(
                        connection_id = ?connection_id,
                        listener_generation = thread_state.listener_generation,
                        "retaining thread listener after connection disconnect left zero subscribers"
                    );
                }
            }
            return;
        }

        for thread_id in thread_ids {
            if let Some(thread_state) = self.thread_states.get(&thread_id) {
                let mut thread_state = thread_state.lock().await;
                thread_state.remove_connection(connection_id);
                if thread_state.subscribed_connection_ids().is_empty() {
                    tracing::debug!(
                        thread_id = %thread_id,
                        connection_id = ?connection_id,
                        listener_generation = thread_state.listener_generation,
                        "retaining thread listener after connection disconnect left zero subscribers"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use pretty_assertions::assert_eq;

    fn pending_payload(item_id: &str) -> ServerRequestPayload {
        ServerRequestPayload::ToolRequestUserInput(ToolRequestUserInputParams {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            item_id: item_id.to_string(),
            questions: Vec::new(),
        })
    }

    #[test]
    fn pending_item_requests_are_filtered_by_connection_delivery() {
        let mut state = ThreadState::default();
        let connection_a = ConnectionId(1);
        let connection_b = ConnectionId(2);

        state.pending_item_requests.insert(
            "item-1".to_string(),
            PendingItemRequest::new(
                "item-1".to_string(),
                RequestId::Integer(7),
                pending_payload("item-1"),
                &[connection_a],
            ),
        );

        assert!(
            state
                .pending_item_requests_for_connection(connection_a)
                .is_empty()
        );

        let pending_for_b = state.pending_item_requests_for_connection(connection_b);
        assert_eq!(pending_for_b.len(), 1);
        assert_eq!(pending_for_b[0].item_id, "item-1");
        assert_eq!(pending_for_b[0].request_id, RequestId::Integer(7));
    }

    #[test]
    fn mark_pending_item_request_delivered_requires_matching_request_id() {
        let mut state = ThreadState::default();
        let connection_a = ConnectionId(1);
        let connection_b = ConnectionId(2);

        state.pending_item_requests.insert(
            "item-1".to_string(),
            PendingItemRequest::new(
                "item-1".to_string(),
                RequestId::Integer(11),
                pending_payload("item-1"),
                &[connection_a],
            ),
        );

        if let Some(request) = state.pending_item_requests.get_mut("item-1") {
            request.mark_delivered_if_request_matches(&RequestId::Integer(12), connection_b);
        }
        assert_eq!(
            state
                .pending_item_requests_for_connection(connection_b)
                .len(),
            1
        );

        if let Some(request) = state.pending_item_requests.get_mut("item-1") {
            request.mark_delivered_if_request_matches(&RequestId::Integer(11), connection_b);
        }
        assert!(
            state
                .pending_item_requests_for_connection(connection_b)
                .is_empty()
        );
    }
}
