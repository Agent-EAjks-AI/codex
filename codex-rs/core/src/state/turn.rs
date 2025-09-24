use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use codex_utils_readiness::ReadinessFlag;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::client::ModelClient;
use crate::config_types::ShellEnvironmentPolicy;
use crate::openai_tools::ToolsConfig;
use crate::protocol::AskForApproval;
use crate::protocol::FileChange;
use crate::protocol::InputItem;
use crate::protocol::SandboxPolicy;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;

#[derive(Debug)]
pub(crate) struct TurnContext {
    pub(crate) client: ModelClient,
    /// The session's current working directory. All relative paths provided by
    /// the model as well as sandbox policies are resolved against this path
    /// instead of `std::env::current_dir()`.
    pub(crate) cwd: PathBuf,
    pub(crate) base_instructions: Option<String>,
    pub(crate) user_instructions: Option<String>,
    pub(crate) approval_policy: AskForApproval,
    pub(crate) sandbox_policy: SandboxPolicy,
    pub(crate) shell_environment_policy: ShellEnvironmentPolicy,
    pub(crate) tools_config: ToolsConfig,
    pub(crate) is_review_mode: bool,
    pub(crate) final_output_json_schema: Option<Value>,
}

impl TurnContext {
    pub(crate) fn resolve_path(&self, path: Option<String>) -> PathBuf {
        path.as_ref()
            .map(PathBuf::from)
            .map_or_else(|| self.cwd.clone(), |p| self.cwd.join(p))
    }
}

struct TurnRuntime {
    initial_input: Option<ResponseInputItem>,
    current_readiness: Option<Arc<ReadinessFlag>>,
    pending: VecDeque<ResponseInputItem>,
    review_history: Vec<ResponseItem>,
    last_agent_message: Option<String>,
    auto_compact_recently_attempted: bool,
    diff_tracker: TurnDiffTracker,
}

impl TurnRuntime {
    fn new(
        initial_input: Option<ResponseInputItem>,
        readiness: Option<Arc<ReadinessFlag>>,
    ) -> Self {
        Self {
            initial_input,
            current_readiness: readiness,
            pending: VecDeque::new(),
            review_history: Vec::new(),
            last_agent_message: None,
            auto_compact_recently_attempted: false,
            diff_tracker: TurnDiffTracker::new(),
        }
    }
}

pub(crate) struct TurnState {
    sub_id: String,
    turn_context: Arc<TurnContext>,
    runtime: Mutex<TurnRuntime>,
}

impl TurnState {
    pub(crate) fn new(
        sub_id: String,
        turn_context: Arc<TurnContext>,
        initial_input: Vec<InputItem>,
        readiness: Option<Arc<ReadinessFlag>>,
    ) -> Self {
        let initial_input = if initial_input.is_empty() {
            None
        } else {
            Some(initial_input.into())
        };
        let runtime = TurnRuntime::new(initial_input, readiness);
        Self {
            sub_id,
            turn_context,
            runtime: Mutex::new(runtime),
        }
    }

    pub(crate) fn sub_id(&self) -> &str {
        &self.sub_id
    }

    pub(crate) fn turn_context(&self) -> Arc<TurnContext> {
        Arc::clone(&self.turn_context)
    }

    pub(crate) async fn take_initial_input(&self) -> Option<ResponseInputItem> {
        let mut runtime = self.runtime.lock().await;
        runtime.initial_input.take()
    }

    pub(crate) async fn drain_mailbox(&self) -> (Vec<ResponseItem>, Option<Arc<ReadinessFlag>>) {
        let mut runtime = self.runtime.lock().await;
        let items = runtime
            .pending
            .drain(..)
            .map(ResponseItem::from)
            .collect::<Vec<_>>();
        let readiness = runtime.current_readiness.clone();
        (items, readiness)
    }

    pub(crate) async fn enqueue_user_input(
        &self,
        items: Vec<InputItem>,
        readiness: Option<Arc<ReadinessFlag>>,
    ) {
        if readiness.is_some() {
            let mut runtime = self.runtime.lock().await;
            runtime.current_readiness = readiness;
            if items.is_empty() {
                return;
            }
            let response: ResponseInputItem = items.into();
            runtime.pending.push_back(response);
            return;
        }

        if items.is_empty() {
            return;
        }

        let mut runtime = self.runtime.lock().await;
        let response: ResponseInputItem = items.into();
        runtime.pending.push_back(response);
    }

    pub(crate) async fn set_review_history(&self, history: Vec<ResponseItem>) {
        let mut runtime = self.runtime.lock().await;
        runtime.review_history = history;
    }

    pub(crate) async fn extend_review_history(&self, items: &[ResponseItem]) {
        if items.is_empty() {
            return;
        }
        let mut runtime = self.runtime.lock().await;
        runtime.review_history.extend(items.iter().cloned());
    }

    pub(crate) async fn review_history(&self) -> Vec<ResponseItem> {
        let runtime = self.runtime.lock().await;
        runtime.review_history.clone()
    }

    pub(crate) async fn mark_auto_compact_attempted(&self) -> bool {
        let mut runtime = self.runtime.lock().await;
        let already_attempted = runtime.auto_compact_recently_attempted;
        runtime.auto_compact_recently_attempted = true;
        already_attempted
    }

    pub(crate) async fn reset_auto_compact_attempted(&self) {
        let mut runtime = self.runtime.lock().await;
        runtime.auto_compact_recently_attempted = false;
    }

    pub(crate) async fn set_last_agent_message(&self, message: Option<String>) {
        let mut runtime = self.runtime.lock().await;
        runtime.last_agent_message = message;
    }

    pub(crate) async fn last_agent_message(&self) -> Option<String> {
        let runtime = self.runtime.lock().await;
        runtime.last_agent_message.clone()
    }

    pub(crate) async fn on_patch_begin(&self, changes: &HashMap<PathBuf, FileChange>) {
        let mut runtime = self.runtime.lock().await;
        runtime.diff_tracker.on_patch_begin(changes);
    }

    pub(crate) async fn take_unified_diff(&self) -> Result<Option<String>> {
        let mut runtime = self.runtime.lock().await;
        runtime.diff_tracker.get_unified_diff()
    }
}
