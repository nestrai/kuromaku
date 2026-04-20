use std::collections::{HashMap, HashSet, VecDeque};

use crate::config::{FlowConfig, Stage};

#[derive(Debug, thiserror::Error)]
pub enum DagError {
    #[error("cycle detected: {0}")]
    Cycle(String),

    #[error("stage '{stage}' references unknown stage '{referenced}'")]
    UnknownStage { stage: String, referenced: String },

    #[error("stage '{stage}' references unknown agent '{agent}'")]
    UnknownAgent { stage: String, agent: String },
}

/// Validates the DAG formed by stage dependencies and returns stages in topological order.
///
/// Checks:
/// - All referenced stage IDs exist
/// - All referenced agent IDs exist
/// - No cycles in the dependency graph
pub fn validate_dag(config: &FlowConfig) -> Result<Vec<&Stage>, DagError> {
    let stage_ids: HashSet<&str> = config.stages.iter().map(|s| s.id.as_str()).collect();
    let agent_ids: HashSet<&str> = config.agents.iter().map(|a| a.id.as_str()).collect();

    // Validate references
    for stage in &config.stages {
        if !agent_ids.contains(stage.agent.as_str()) {
            return Err(DagError::UnknownAgent {
                stage: stage.id.clone(),
                agent: stage.agent.clone(),
            });
        }
        for dep in &stage.needs {
            if !stage_ids.contains(dep.as_str()) {
                return Err(DagError::UnknownStage {
                    stage: stage.id.clone(),
                    referenced: dep.clone(),
                });
            }
        }
    }

    // Topological sort using Kahn's algorithm
    let stage_map: HashMap<&str, &Stage> =
        config.stages.iter().map(|s| (s.id.as_str(), s)).collect();

    let mut in_degree: HashMap<&str, usize> = config
        .stages
        .iter()
        .map(|s| (s.id.as_str(), s.needs.len()))
        .collect();

    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for stage in &config.stages {
        for dep in &stage.needs {
            dependents
                .entry(dep.as_str())
                .or_default()
                .push(stage.id.as_str());
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(&id, _)| id)
        .collect();

    // Sort the initial queue for deterministic output
    let mut initial: Vec<&str> = queue.drain(..).collect();
    initial.sort();
    queue.extend(initial);

    let mut result: Vec<&Stage> = Vec::with_capacity(config.stages.len());

    while let Some(id) = queue.pop_front() {
        result.push(stage_map[id]);

        if let Some(deps) = dependents.get(id) {
            let mut next: Vec<&str> = Vec::new();
            for &dependent in deps {
                let deg = in_degree.get_mut(dependent).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    next.push(dependent);
                }
            }
            next.sort();
            queue.extend(next);
        }
    }

    if result.len() != config.stages.len() {
        // Cycle detected -- find and report it
        let cycle_path = find_cycle(config);
        return Err(DagError::Cycle(cycle_path));
    }

    Ok(result)
}

/// Finds a cycle in the dependency graph and returns a formatted path string.
fn find_cycle(config: &FlowConfig) -> String {
    let stage_needs: HashMap<&str, &[String]> = config
        .stages
        .iter()
        .map(|s| (s.id.as_str(), s.needs.as_slice()))
        .collect();

    let mut visited: HashSet<&str> = HashSet::new();
    let mut in_stack: HashSet<&str> = HashSet::new();
    let mut path: Vec<&str> = Vec::new();

    for stage in &config.stages {
        if !visited.contains(stage.id.as_str())
            && let Some(cycle) = dfs_find_cycle(
                stage.id.as_str(),
                &stage_needs,
                &mut visited,
                &mut in_stack,
                &mut path,
            )
        {
            return cycle;
        }
    }

    "unknown cycle".to_string()
}

fn dfs_find_cycle<'a>(
    node: &'a str,
    graph: &'a HashMap<&'a str, &'a [String]>,
    visited: &mut HashSet<&'a str>,
    in_stack: &mut HashSet<&'a str>,
    path: &mut Vec<&'a str>,
) -> Option<String> {
    visited.insert(node);
    in_stack.insert(node);
    path.push(node);

    if let Some(deps) = graph.get(node) {
        for dep in *deps {
            let dep_str = dep.as_str();
            if !visited.contains(dep_str) {
                if let Some(cycle) = dfs_find_cycle(dep_str, graph, visited, in_stack, path) {
                    return Some(cycle);
                }
            } else if in_stack.contains(dep_str) {
                // Found a cycle -- extract the cycle path
                let cycle_start = path.iter().position(|&n| n == dep_str).unwrap();
                let mut cycle_nodes: Vec<&str> = path[cycle_start..].to_vec();
                cycle_nodes.push(dep_str);
                return Some(cycle_nodes.join(" -> "));
            }
        }
    }

    path.pop();
    in_stack.remove(node);
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config_from_str;

    #[test]
    fn cycle_two_stages() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: a
    agent: dev
    task: "do a"
    needs: [b]
  - id: b
    agent: dev
    task: "do b"
    needs: [a]
"#;
        let config = load_config_from_str(yaml).unwrap();
        let err = validate_dag(&config).unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("cycle detected:"), "got: {msg}");
        assert!(msg.contains("a") && msg.contains("b"), "got: {msg}");
        assert!(msg.contains(" -> "), "got: {msg}");
    }

    #[test]
    fn missing_stage_reference() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: a
    agent: dev
    task: "do a"
    needs: [nonexistent]
"#;
        // Config parser already catches this, but let's test with a crafted FlowConfig
        let config = load_config_from_str(yaml);
        assert!(config.is_err()); // caught at config level

        // Test via DAG directly with a manually constructed config
        let config = FlowConfig {
            version: "1".to_string(),
            name: "test".to_string(),
            defaults: crate::config::Defaults {
                model: "m".to_string(),
                backend: crate::config::Backend::Api,
            },
            agents: vec![crate::config::Agent {
                id: "dev".to_string(),
                role: "dev".to_string(),
                model: "m".to_string(),
                backend: crate::config::Backend::Api,
            }],
            stages: vec![Stage {
                id: "a".to_string(),
                agent: "dev".to_string(),
                task: crate::config::TaskSource::Inline("do a".to_string()),
                input: vec![],
                needs: vec!["nonexistent".to_string()],
                output: None,
                model: None,
                backend: None,
            }],
            state: crate::config::StateConfig {
                backend: "local".to_string(),
                path: ".koto/state".to_string(),
            },
        };
        let err = validate_dag(&config).unwrap_err();
        assert!(
            err.to_string()
                .contains("stage 'a' references unknown stage 'nonexistent'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn missing_agent_reference() {
        let config = FlowConfig {
            version: "1".to_string(),
            name: "test".to_string(),
            defaults: crate::config::Defaults {
                model: "m".to_string(),
                backend: crate::config::Backend::Api,
            },
            agents: vec![],
            stages: vec![Stage {
                id: "a".to_string(),
                agent: "ghost".to_string(),
                task: crate::config::TaskSource::Inline("do a".to_string()),
                input: vec![],
                needs: vec![],
                output: None,
                model: None,
                backend: None,
            }],
            state: crate::config::StateConfig {
                backend: "local".to_string(),
                path: ".koto/state".to_string(),
            },
        };
        let err = validate_dag(&config).unwrap_err();
        assert!(
            err.to_string()
                .contains("stage 'a' references unknown agent 'ghost'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn valid_dag_topological_order() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: a
    agent: dev
    task: "do a"
  - id: b
    agent: dev
    task: "do b"
    needs: [a]
  - id: c
    agent: dev
    task: "do c"
    needs: [a]
  - id: d
    agent: dev
    task: "do d"
    needs: [b, c]
"#;
        let config = load_config_from_str(yaml).unwrap();
        let order = validate_dag(&config).unwrap();
        let ids: Vec<&str> = order.iter().map(|s| s.id.as_str()).collect();

        assert_eq!(ids.len(), 4);
        // a must come before b and c, b and c before d
        let pos = |id: &str| ids.iter().position(|&s| s == id).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("a") < pos("c"));
        assert!(pos("b") < pos("d"));
        assert!(pos("c") < pos("d"));
    }

    #[test]
    fn linear_chain() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: a
    agent: dev
    task: "do a"
  - id: b
    agent: dev
    task: "do b"
    needs: [a]
  - id: c
    agent: dev
    task: "do c"
    needs: [b]
  - id: d
    agent: dev
    task: "do d"
    needs: [c]
"#;
        let config = load_config_from_str(yaml).unwrap();
        let order = validate_dag(&config).unwrap();
        let ids: Vec<&str> = order.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn diamond_dag() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: a
    agent: dev
    task: "do a"
  - id: b
    agent: dev
    task: "do b"
    needs: [a]
  - id: c
    agent: dev
    task: "do c"
    needs: [a]
  - id: d
    agent: dev
    task: "do d"
    needs: [b, c]
"#;
        let config = load_config_from_str(yaml).unwrap();
        let order = validate_dag(&config).unwrap();
        let ids: Vec<&str> = order.iter().map(|s| s.id.as_str()).collect();

        let pos = |id: &str| ids.iter().position(|&s| s == id).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("a") < pos("c"));
        assert!(pos("b") < pos("d"));
        assert!(pos("c") < pos("d"));
    }

    #[test]
    fn no_dependencies() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: a
    agent: dev
    task: "do a"
  - id: b
    agent: dev
    task: "do b"
  - id: c
    agent: dev
    task: "do c"
"#;
        let config = load_config_from_str(yaml).unwrap();
        let order = validate_dag(&config).unwrap();
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn input_implies_needs_in_dag() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: a
    agent: dev
    task: "do a"
    output: result.md
  - id: b
    agent: dev
    task: "do b"
    input: [a]
"#;
        let config = load_config_from_str(yaml).unwrap();
        // input merges into needs during config resolution
        assert!(config.stages[1].needs.contains(&"a".to_string()));

        let order = validate_dag(&config).unwrap();
        let ids: Vec<&str> = order.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn three_node_cycle() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: a
    agent: dev
    task: "do a"
    needs: [c]
  - id: b
    agent: dev
    task: "do b"
    needs: [a]
  - id: c
    agent: dev
    task: "do c"
    needs: [b]
"#;
        let config = load_config_from_str(yaml).unwrap();
        let err = validate_dag(&config).unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("cycle detected:"), "got: {msg}");
        assert!(msg.contains(" -> "), "got: {msg}");
    }
}
