use std::collections::HashMap;
use std::collections::HashSet;

use crate::instructions::SkillInstructions;
use crate::skills::SkillDependencyInfo;
use crate::skills::SkillLoadOutcome;
use crate::skills::SkillMetadata;
use codex_otel::OtelManager;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use tokio::fs;

#[derive(Debug, Default)]
pub(crate) struct SkillTurnPrep {
    pub(crate) items: Vec<ResponseItem>,
    pub(crate) dependencies: Vec<SkillDependencyInfo>,
    pub(crate) warnings: Vec<String>,
}

/// Builds turn-ready skill instructions and collects declared dependencies from SKILL.toml.
pub(crate) async fn build_skill_turn_prep(
    inputs: &[UserInput],
    skills: Option<&SkillLoadOutcome>,
    otel: Option<&OtelManager>,
) -> SkillTurnPrep {
    if inputs.is_empty() {
        return SkillTurnPrep::default();
    }

    let Some(outcome) = skills else {
        return SkillTurnPrep::default();
    };

    let mentioned_skills = collect_explicit_skill_mentions(inputs, outcome);
    if mentioned_skills.is_empty() {
        return SkillTurnPrep::default();
    }

    let mut result = SkillTurnPrep {
        items: Vec::with_capacity(mentioned_skills.len()),
        dependencies: Vec::new(),
        warnings: Vec::new(),
    };

    for selection in mentioned_skills {
        let skill = selection.skill;
        match fs::read_to_string(&skill.path).await {
            Ok(contents) => {
                emit_skill_injected_metric(otel, &skill, "ok");
                if selection.validate_dependencies {
                    for dependency in &skill.dependencies {
                        result.dependencies.push(SkillDependencyInfo {
                            skill_name: skill.name.clone(),
                            dependency: dependency.clone(),
                        });
                    }
                }
                result.items.push(ResponseItem::from(SkillInstructions {
                    name: skill.name,
                    path: skill.path.to_string_lossy().into_owned(),
                    contents,
                }));
            }
            Err(err) => {
                emit_skill_injected_metric(otel, &skill, "error");
                let message = format!(
                    "Failed to load skill {name} at {path}: {err:#}",
                    name = skill.name,
                    path = skill.path.display()
                );
                result.warnings.push(message);
            }
        }
    }

    result
}

struct SkillSelection {
    skill: SkillMetadata,
    validate_dependencies: bool,
}

fn collect_explicit_skill_mentions(
    inputs: &[UserInput],
    outcome: &SkillLoadOutcome,
) -> Vec<SkillSelection> {
    let mut selected: Vec<SkillSelection> = Vec::new();
    let mut seen: HashSet<(String, std::path::PathBuf)> = HashSet::new();
    let mut indexes: HashMap<(String, std::path::PathBuf), usize> = HashMap::new();

    for input in inputs {
        if let UserInput::Skill {
            name,
            path,
            validate_dependencies,
        } = input
            && let Some(skill) = outcome
                .skills
                .iter()
                .find(|s| s.name == *name && s.path == *path)
            && outcome.is_skill_enabled(skill)
        {
            let key = (name.clone(), path.clone());
            if let Some(index) = indexes.get(&key) {
                if *validate_dependencies {
                    selected[*index].validate_dependencies = true;
                }
                continue;
            }
            if seen.insert(key.clone()) {
                indexes.insert(key.clone(), selected.len());
                selected.push(SkillSelection {
                    skill: skill.clone(),
                    validate_dependencies: *validate_dependencies,
                });
            }
        }
    }

    selected
}

fn emit_skill_injected_metric(otel: Option<&OtelManager>, skill: &SkillMetadata, status: &str) {
    let Some(otel) = otel else {
        return;
    };

    otel.counter(
        "codex.skill.injected",
        1,
        &[("status", status), ("skill", skill.name.as_str())],
    );
}
