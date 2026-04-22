use std::collections::{HashMap, HashSet, VecDeque};

use crate::config::{FlowConfig, Step};

#[derive(Debug, thiserror::Error)]
pub enum DagError {
    #[error("cycle detected: {0}")]
    Cycle(String),

    #[error("step '{step}' references unknown step '{referenced}'")]
    UnknownStep { step: String, referenced: String },
}

/// Validates the DAG formed by step dependencies and returns steps in topological order.
pub fn validate_dag(config: &FlowConfig) -> Result<Vec<&Step>, DagError> {
    let step_ids: HashSet<&str> = config.steps.iter().map(|s| s.id.as_str()).collect();

    // Validate references
    for step in &config.steps {
        for dep in &step.needs {
            if !step_ids.contains(dep.as_str()) {
                return Err(DagError::UnknownStep {
                    step: step.id.clone(),
                    referenced: dep.clone(),
                });
            }
        }
    }

    // Topological sort using Kahn's algorithm
    let step_map: HashMap<&str, &Step> = config.steps.iter().map(|s| (s.id.as_str(), s)).collect();

    let mut in_degree: HashMap<&str, usize> = config
        .steps
        .iter()
        .map(|s| (s.id.as_str(), s.needs.len()))
        .collect();

    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for step in &config.steps {
        for dep in &step.needs {
            dependents
                .entry(dep.as_str())
                .or_default()
                .push(step.id.as_str());
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

    let mut result: Vec<&Step> = Vec::with_capacity(config.steps.len());

    while let Some(id) = queue.pop_front() {
        result.push(step_map[id]);

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

    if result.len() != config.steps.len() {
        let cycle_path = find_cycle(config);
        return Err(DagError::Cycle(cycle_path));
    }

    Ok(result)
}

fn find_cycle(config: &FlowConfig) -> String {
    let step_needs: HashMap<&str, &[String]> = config
        .steps
        .iter()
        .map(|s| (s.id.as_str(), s.needs.as_slice()))
        .collect();

    let mut visited: HashSet<&str> = HashSet::new();
    let mut in_stack: HashSet<&str> = HashSet::new();
    let mut path: Vec<&str> = Vec::new();

    for step in &config.steps {
        if !visited.contains(step.id.as_str())
            && let Some(cycle) = dfs_find_cycle(
                step.id.as_str(),
                &step_needs,
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
    use crate::config::load_flow_from_str;

    #[test]
    fn cycle_two_steps() {
        let yaml = r#"
version: "1"
name: test
flow:
  a:
    agent: dev
    needs: [b]
  b:
    agent: dev
    needs: [a]
"#;
        let config = load_flow_from_str(yaml).unwrap();
        let err = validate_dag(&config).unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("cycle detected:"), "got: {msg}");
        assert!(msg.contains("a") && msg.contains("b"), "got: {msg}");
        assert!(msg.contains(" -> "), "got: {msg}");
    }

    #[test]
    fn missing_step_reference() {
        let config = FlowConfig {
            version: "1".to_string(),
            name: "test".to_string(),
            prompt: None,
            defaults: crate::config::Defaults {
                model: "m".to_string(),
                backend: crate::config::Backend::Api,
            },
            roles: std::collections::HashMap::new(),
            steps: vec![Step {
                id: "a".to_string(),
                agent: "dev".to_string(),
                task: None,
                input: vec![],
                needs: vec!["nonexistent".to_string()],
                model: None,
                backend: None,
                print_output: false,
            }],
            stack: crate::config::StackConfig {
                backend: "local".to_string(),
                path: String::new(),
            },
        };
        let err = validate_dag(&config).unwrap_err();
        assert!(
            err.to_string()
                .contains("step 'a' references unknown step 'nonexistent'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn valid_dag_topological_order() {
        let yaml = r#"
version: "1"
name: test
flow:
  a:
    agent: dev
  b:
    agent: dev
    needs: [a]
  c:
    agent: dev
    needs: [a]
  d:
    agent: dev
    needs: [b, c]
"#;
        let config = load_flow_from_str(yaml).unwrap();
        let order = validate_dag(&config).unwrap();
        let ids: Vec<&str> = order.iter().map(|s| s.id.as_str()).collect();

        assert_eq!(ids.len(), 4);
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
flow:
  a:
    agent: dev
  b:
    agent: dev
    needs: [a]
  c:
    agent: dev
    needs: [b]
  d:
    agent: dev
    needs: [c]
"#;
        let config = load_flow_from_str(yaml).unwrap();
        let order = validate_dag(&config).unwrap();
        let ids: Vec<&str> = order.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn no_dependencies() {
        let yaml = r#"
version: "1"
name: test
flow:
  a:
    agent: dev
  b:
    agent: dev
  c:
    agent: dev
"#;
        let config = load_flow_from_str(yaml).unwrap();
        let order = validate_dag(&config).unwrap();
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn input_implies_needs_in_dag() {
        let yaml = r#"
version: "1"
name: test
flow:
  a:
    agent: dev
  b:
    agent: dev
    input: [a]
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert!(config.steps[1].needs.contains(&"a".to_string()));

        let order = validate_dag(&config).unwrap();
        let ids: Vec<&str> = order.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn three_node_cycle() {
        let yaml = r#"
version: "1"
name: test
flow:
  a:
    agent: dev
    needs: [c]
  b:
    agent: dev
    needs: [a]
  c:
    agent: dev
    needs: [b]
"#;
        let config = load_flow_from_str(yaml).unwrap();
        let err = validate_dag(&config).unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("cycle detected:"), "got: {msg}");
        assert!(msg.contains(" -> "), "got: {msg}");
    }
}
