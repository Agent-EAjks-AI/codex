#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDependency {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillDependencyInfo {
    pub(crate) skill_name: String,
    pub(crate) dependency: SkillDependency,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillDependencyResponse {
    pub(crate) values: std::collections::HashMap<String, String>,
}
