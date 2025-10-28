use std::sync::Arc;

use async_channel::Receiver;
use async_channel::Sender;
use codex_async_utils::OrCancelExt;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use tokio_util::sync::CancellationToken;

use crate::AuthManager;
use crate::codex::Codex;
use crate::codex::CodexSpawnOk;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::config::Config;
use crate::error::CodexErr;
use codex_protocol::protocol::InitialHistory;

pub(crate) async fn run_codex_conversation(
    config: Config,
    auth_manager: Arc<AuthManager>,
    input: Vec<UserInput>,
    parent_session: Arc<Session>,
    parent_ctx: Arc<TurnContext>,
    cancel_token: CancellationToken,
) -> Result<Receiver<EventMsg>, CodexErr> {
    let (tx_sub, rx_sub) = async_channel::unbounded();
    let CodexSpawnOk { codex, .. } = Codex::spawn(
        config,
        auth_manager,
        InitialHistory::New,
        SessionSource::SubAgent,
    )
    .await?;

    codex.submit(Op::UserInput { items: input }).await?;

    // Use a child token so parent cancel cascades but we can scope it to this task
    let cancel_token = cancel_token.child_token();
    let parent_session_clone = Arc::clone(&parent_session);
    let parent_ctx_clone = Arc::clone(&parent_ctx);
    tokio::spawn(async move {
        let _ = forward_events(codex, tx_sub, parent_session_clone, parent_ctx_clone)
            .or_cancel(&cancel_token)
            .await;
    });

    Ok(rx_sub)
}

async fn forward_events(
    codex: Codex,
    tx_sub: Sender<EventMsg>,
    parent_session: Arc<Session>,
    parent_ctx: Arc<TurnContext>,
) {
    while let Ok(event) = codex.next_event().await {
        match event {
            Event {
                id: _,
                msg: EventMsg::SessionConfigured(_),
            } => continue,
            Event {
                id,
                msg: EventMsg::ExecApprovalRequest(event),
            } => {
                // Initiate approval via parent session; do not surface to consumer.
                let decision = parent_session
                    .request_command_approval(
                        parent_ctx.as_ref(),
                        parent_ctx.sub_id.clone(),
                        event.command.clone(),
                        event.cwd.clone(),
                        event.reason.clone(),
                    )
                    .await;
                let _ = codex.submit(Op::ExecApproval { id, decision }).await;
            }
            Event {
                id,
                msg: EventMsg::ApplyPatchApprovalRequest(event),
            } => {
                let decision = parent_session
                    .request_patch_approval(
                        parent_ctx.as_ref(),
                        parent_ctx.sub_id.clone(),
                        event.changes.clone(),
                        event.reason.clone(),
                        event.grant_root.clone(),
                    )
                    .await;
                let _ = codex
                    .submit(Op::PatchApproval {
                        id,
                        decision: decision.await.unwrap_or_default(),
                    })
                    .await;
            }
            other => {
                let _ = tx_sub.send(other.msg).await;
            }
        }
    }
}
