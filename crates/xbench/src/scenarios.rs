use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Scenario {
    pub name: String,
    pub pattern: String,
}

#[derive(Deserialize)]
struct ScenarioFile {
    scenarios: Vec<Scenario>,
}

pub(crate) fn read_scenarios(path: &Path) -> Result<Vec<Scenario>> {
    let body = std::fs::read_to_string(path)?;
    let file: ScenarioFile = toml::from_str(&body)?;
    Ok(file.scenarios)
}
