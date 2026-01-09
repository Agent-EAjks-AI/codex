use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::sync::Arc;

use tracing::warn;

use crate::codex::Session;
use crate::codex::TurnContext;
use crate::protocol::EventMsg;
use crate::protocol::SkillDependencyRequestEvent;
use crate::skills::SkillDependencyInfo;
use crate::skills::SkillDependencyResponse;
use crate::skills::load_env_var;
use crate::skills::save_env_var;

/// Resolve required dependency values (session cache, env vars, then env store),
/// and prompt the UI for any missing ones.
pub(crate) async fn resolve_skill_dependencies_for_turn(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    dependencies: &[SkillDependencyInfo],
) {
    if dependencies.is_empty() {
        return;
    }

    let codex_home = sess.codex_home().await;
    let existing_env = sess.dependency_env().await;
    let mut loaded_values = HashMap::new();
    let mut missing = Vec::new();
    let mut seen_names = HashSet::new();

    for dependency in dependencies {
        let name = dependency.dependency.name.clone();
        if !seen_names.insert(name.clone()) {
            continue;
        }
        if existing_env.contains_key(&name) {
            continue;
        }
        match env::var(&name) {
            Ok(value) => {
                loaded_values.insert(name.clone(), value);
                continue;
            }
            Err(env::VarError::NotPresent) => {}
            Err(err) => {
                warn!("failed to read env var {name}: {err}");
            }
        }

        match load_env_var(&codex_home, &name) {
            Ok(Some(value)) => {
                loaded_values.insert(name.clone(), value);
            }
            Ok(None) => {
                missing.push(dependency.clone());
            }
            Err(err) => {
                warn!("failed to load env var {name}: {err}");
                missing.push(dependency.clone());
            }
        }
    }

    if !loaded_values.is_empty() {
        sess.set_dependency_env(loaded_values).await;
    }

    if !missing.is_empty() {
        let _response = request_skill_dependencies(sess, turn_context, &missing).await;
    }
}

/// Emit per-skill dependency requests and wait for user responses.
pub(crate) async fn request_skill_dependencies(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    dependencies: &[SkillDependencyInfo],
) -> Vec<SkillDependencyResponse> {
    let mut grouped: HashMap<String, Vec<SkillDependencyInfo>> = HashMap::new();
    for dependency in dependencies {
        grouped
            .entry(dependency.skill_name.clone())
            .or_default()
            .push(dependency.clone());
    }

    let mut responses = Vec::with_capacity(grouped.len());
    for (skill_name, deps) in grouped {
        let request_id = format!("skill_dependencies:{}:{}", turn_context.sub_id, skill_name);
        let (tx_response, rx_response) = tokio::sync::oneshot::channel();
        let prev_entry = sess
            .insert_pending_skill_dependencies(request_id.clone(), tx_response)
            .await;
        if prev_entry.is_some() {
            warn!("Overwriting existing pending skill dependencies for {request_id}");
        }

        let dependencies = deps
            .into_iter()
            .map(|dep| codex_protocol::approvals::SkillDependency {
                dependency_type: "env_var".to_string(),
                name: dep.dependency.name,
                description: dep.dependency.description,
            })
            .collect::<Vec<_>>();

        let event = EventMsg::SkillDependencyRequest(SkillDependencyRequestEvent {
            id: request_id.clone(),
            turn_id: turn_context.sub_id.clone(),
            skill_name,
            dependencies,
        });
        sess.send_event(turn_context, event).await;

        let response = rx_response.await.unwrap_or(SkillDependencyResponse {
            values: HashMap::new(),
        });
        responses.push(response);
    }

    responses
}

/// Persist provided values, update session env, and unblock the pending request.
pub(crate) async fn handle_skill_dependency_response(
    sess: &Arc<Session>,
    request_id: &str,
    response: SkillDependencyResponse,
) {
    if !response.values.is_empty() {
        let codex_home = sess.codex_home().await;
        for (name, value) in &response.values {
            if let Err(err) = save_env_var(&codex_home, name, value) {
                warn!("failed to persist env var {name}: {err}");
            }
        }
        sess.set_dependency_env(response.values.clone()).await;
    }
    let entry = sess.remove_pending_skill_dependencies(request_id).await;
    match entry {
        Some(tx_response) => {
            tx_response.send(response).ok();
        }
        None => {
            warn!("No pending skill dependency request found for {request_id}");
        }
    }
}
