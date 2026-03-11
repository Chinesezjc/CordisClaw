//! Package discovery and contract validation.
//! This module implements Phase A (`discover/resolve`) from the plan.

use crate::core::error::RuntimeError;
use crate::core::models::{ChildPluginSpec, CordisMetadata, PluginDocs};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ChildEdge {
    /// Canonical child plugin path (`parent/child`).
    pub child_path: String,
    /// Required edge controls failure propagation.
    pub required: bool,
    /// Parent -> child service allowlist.
    pub grants: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPlugin {
    /// Unique path id used as primary key.
    pub plugin_path: String,
    /// Normalized crate name generated from plugin_path.
    pub crate_name: String,
    pub dir: PathBuf,
    pub metadata: CordisMetadata,
    /// Parsed docs contract from `docs/agent/interfaces.json`.
    pub docs: PluginDocs,
    /// Parent plugin path if this is not a root plugin.
    pub parent: Option<String>,
    /// Whether this plugin is required by its parent.
    pub required: bool,
    /// Allowed inherited services from parent.
    pub grants_from_parent: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPluginGraph {
    pub plugins: BTreeMap<String, ResolvedPlugin>,
    pub children: BTreeMap<String, Vec<ChildEdge>>,
    pub topo_order: Vec<String>,
}

#[derive(Debug)]
pub struct PackageResolver {
    plugins_root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct WorkspaceToml {
    workspace: Option<WorkspaceSection>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceSection {
    #[serde(default)]
    members: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PluginCargoToml {
    package: Option<PackageSection>,
}

#[derive(Debug, Deserialize)]
struct PackageSection {
    name: String,
    metadata: Option<MetadataSection>,
}

#[derive(Debug, Deserialize)]
struct MetadataSection {
    cordis: Option<CordisMetadata>,
}

#[derive(Debug)]
struct VisitState {
    plugins: BTreeMap<String, ResolvedPlugin>,
    children: BTreeMap<String, Vec<ChildEdge>>,
    dir_by_plugin_path: HashMap<String, PathBuf>,
    parent_of: HashMap<String, String>,
    visiting: HashSet<String>,
    visit_stack: Vec<String>,
    topo_order: Vec<String>,
}

impl PackageResolver {
    pub fn new(plugins_root: impl Into<PathBuf>) -> Self {
        Self {
            plugins_root: plugins_root.into(),
        }
    }

    pub fn resolve(&self) -> Result<ResolvedPluginGraph, RuntimeError> {
        // Start from top-level workspace members only. No implicit filesystem scan.
        let workspace_manifest = self.plugins_root.join("Cargo.toml");
        let workspace_text = fs::read_to_string(&workspace_manifest).map_err(|e| RuntimeError::Io {
            path: workspace_manifest.clone(),
            message: e.to_string(),
        })?;

        let workspace: WorkspaceToml = toml::from_str(&workspace_text).map_err(|e| RuntimeError::CargoParse {
            path: workspace_manifest.clone(),
            message: e.to_string(),
        })?;

        let members = workspace
            .workspace
            .ok_or_else(|| RuntimeError::InvalidWorkspace {
                path: workspace_manifest.clone(),
            })?
            .members;

        if members.is_empty() {
            return Err(RuntimeError::InvalidWorkspace {
                path: workspace_manifest,
            });
        }

        let mut state = VisitState {
            plugins: BTreeMap::new(),
            children: BTreeMap::new(),
            dir_by_plugin_path: HashMap::new(),
            parent_of: HashMap::new(),
            visiting: HashSet::new(),
            visit_stack: Vec::new(),
            topo_order: Vec::new(),
        };

        for member in members {
            let member_dir = self.plugins_root.join(member);
            // Root plugins are treated as required roots.
            self.visit_plugin(
                &member_dir,
                None,
                true,
                BTreeSet::new(),
                &mut state,
            )?;
        }

        Ok(ResolvedPluginGraph {
            plugins: state.plugins,
            children: state.children,
            topo_order: state.topo_order,
        })
    }

    fn visit_plugin(
        &self,
        dir: &Path,
        parent: Option<&str>,
        required: bool,
        grants_from_parent: BTreeSet<String>,
        state: &mut VisitState,
    ) -> Result<(), RuntimeError> {
        // Load package manifest and read `package.metadata.cordis`.
        let cargo_path = dir.join("Cargo.toml");
        if !cargo_path.exists() {
            return Err(RuntimeError::ChildNotFound {
                parent: parent.unwrap_or("<root>").to_string(),
                child_source: dir.display().to_string(),
            });
        }

        let cargo_text = fs::read_to_string(&cargo_path).map_err(|e| RuntimeError::Io {
            path: cargo_path.clone(),
            message: e.to_string(),
        })?;

        let plugin_toml: PluginCargoToml = toml::from_str(&cargo_text).map_err(|e| RuntimeError::CargoParse {
            path: cargo_path.clone(),
            message: e.to_string(),
        })?;

        let package = plugin_toml
            .package
            .ok_or_else(|| RuntimeError::InvalidWorkspace {
                path: cargo_path.clone(),
            })?;

        let metadata = package
            .metadata
            .and_then(|m| m.cordis)
            .ok_or_else(|| RuntimeError::MissingCordisMetadata {
                path: cargo_path.clone(),
            })?;

        let expected_plugin_path = self.expected_plugin_path(dir)?;
        if metadata.plugin_path != expected_plugin_path {
            return Err(RuntimeError::PluginPathMismatch {
                path: cargo_path,
                expected: expected_plugin_path,
                actual: metadata.plugin_path,
            });
        }

        let expected_crate_name = normalize_crate_name(&metadata.plugin_path);
        if package.name != expected_crate_name {
            return Err(RuntimeError::CrateNameMismatch {
                path: dir.join("Cargo.toml"),
                expected: expected_crate_name,
                actual: package.name,
            });
        }

        // Hard scaffold checks keep plugin projects uniform for both humans and agents.
        self.validate_scaffold(&metadata.plugin_path, dir)?;
        let docs = self.validate_docs_contract(&metadata.plugin_path, dir)?;

        let plugin_path = metadata.plugin_path.clone();

        if let Some(previous_dir) = state.dir_by_plugin_path.get(&plugin_path) {
            if previous_dir != dir {
                return Err(RuntimeError::DuplicatePluginPath {
                    plugin_path,
                    first: previous_dir.clone(),
                    second: dir.to_path_buf(),
                });
            }
        }

        if let Some(p) = parent {
            if let Some(existing_parent) = state.parent_of.get(&plugin_path) {
                if existing_parent != p {
                    return Err(RuntimeError::DuplicatePluginPath {
                        plugin_path,
                        first: state
                            .dir_by_plugin_path
                            .get(existing_parent)
                            .cloned()
                            .unwrap_or_else(|| PathBuf::from(existing_parent)),
                        second: dir.to_path_buf(),
                    });
                }
            }
        }

        // Cycle check uses current DFS stack and returns full loop path.
        if state.visiting.contains(&plugin_path) {
            let mut cycle = Vec::new();
            if let Some(idx) = state.visit_stack.iter().position(|x| x == &plugin_path) {
                cycle.extend(state.visit_stack[idx..].iter().cloned());
            }
            cycle.push(plugin_path.clone());
            return Err(RuntimeError::CycleDetected { cycle });
        }

        // Already visited by another path (same plugin path) is allowed only once.
        if state.plugins.contains_key(&plugin_path) {
            return Ok(());
        }

        state.visiting.insert(plugin_path.clone());
        state.visit_stack.push(plugin_path.clone());
        state.topo_order.push(plugin_path.clone());
        state
            .dir_by_plugin_path
            .insert(plugin_path.clone(), dir.to_path_buf());

        if let Some(p) = parent {
            state.parent_of.insert(plugin_path.clone(), p.to_string());
        }

        state.plugins.insert(
            plugin_path.clone(),
            ResolvedPlugin {
                plugin_path: plugin_path.clone(),
                crate_name: normalize_crate_name(&plugin_path),
                dir: dir.to_path_buf(),
                metadata: metadata.clone(),
                docs,
                parent: parent.map(str::to_string),
                required,
                grants_from_parent,
            },
        );

        for child in &metadata.children {
            // Child source must be direct relative path (`./child`) and cannot escape.
            let (child_dir, child_component) = self.resolve_child_dir(&plugin_path, dir, child)?;
            let expected_child_path = format!("{}/{}", plugin_path, child_component);
            state
                .children
                .entry(plugin_path.clone())
                .or_default()
                .push(ChildEdge {
                    child_path: expected_child_path,
                    required: child.required,
                    grants: child.grants.iter().cloned().collect(),
                });

            self.visit_plugin(
                &child_dir,
                Some(&plugin_path),
                child.required,
                child.grants.iter().cloned().collect(),
                state,
            )?;
        }

        state.visiting.remove(&plugin_path);
        state.visit_stack.pop();

        Ok(())
    }

    fn expected_plugin_path(&self, dir: &Path) -> Result<String, RuntimeError> {
        // Canonical plugin_path is derived from directory relative to plugins_root.
        let relative = dir
            .strip_prefix(&self.plugins_root)
            .map_err(|_| RuntimeError::Invariant {
                message: format!(
                    "plugin dir {} is not under plugins root {}",
                    dir.display(),
                    self.plugins_root.display()
                ),
            })?;

        let mut segments = Vec::new();
        for component in relative.components() {
            if let Component::Normal(seg) = component {
                segments.push(seg.to_string_lossy().to_string());
            }
        }

        Ok(segments.join("/"))
    }

    fn validate_scaffold(&self, plugin_path: &str, dir: &Path) -> Result<(), RuntimeError> {
        let required = [
            "src",
            "tests",
            "docs",
            "docs/agent/interfaces.json",
            "docs/human/overview.md",
        ];

        let mut missing = Vec::new();
        for item in required {
            let p = dir.join(item);
            if !p.exists() {
                missing.push(item.to_string());
            }
        }

        if missing.is_empty() {
            Ok(())
        } else {
            Err(RuntimeError::MissingScaffold {
                plugin_path: plugin_path.to_string(),
                missing,
            })
        }
    }

    fn validate_docs_contract(&self, plugin_path: &str, dir: &Path) -> Result<PluginDocs, RuntimeError> {
        // `interfaces.json` is machine-facing contract; parsing failure is fatal.
        let docs_path = dir.join("docs/agent/interfaces.json");
        let docs_text = fs::read_to_string(&docs_path).map_err(|e| RuntimeError::Io {
            path: docs_path.clone(),
            message: e.to_string(),
        })?;

        let docs: PluginDocs = serde_json::from_str(&docs_text).map_err(|e| RuntimeError::DocsContract {
            plugin_path: plugin_path.to_string(),
            message: format!("interfaces.json parse failed: {e}"),
        })?;

        if docs.plugin_path != plugin_path {
            return Err(RuntimeError::DocsContract {
                plugin_path: plugin_path.to_string(),
                message: format!(
                    "docs.plugin_path mismatch: expected {plugin_path}, got {}",
                    docs.plugin_path
                ),
            });
        }

        let mut seen = HashSet::new();
        for node in &docs.nodes {
            // Keep node ids unique inside a single plugin.
            if !seen.insert(node.id.clone()) {
                return Err(RuntimeError::DocsContract {
                    plugin_path: plugin_path.to_string(),
                    message: format!("duplicated node id in docs: {}", node.id),
                });
            }
        }

        Ok(docs)
    }

    fn resolve_child_dir(
        &self,
        parent_plugin_path: &str,
        parent_dir: &Path,
        child: &ChildPluginSpec,
    ) -> Result<(PathBuf, String), RuntimeError> {
        if !child.source.starts_with("./") {
            return Err(RuntimeError::InvalidChildSource {
                parent: parent_plugin_path.to_string(),
                child_source: child.source.clone(),
                reason: "must start with ./".to_string(),
            });
        }

        let child_path = Path::new(&child.source);
        let mut normal_components = Vec::new();
        for component in child_path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(seg) => normal_components.push(seg.to_string_lossy().to_string()),
                Component::ParentDir => {
                    return Err(RuntimeError::InvalidChildSource {
                        parent: parent_plugin_path.to_string(),
                        child_source: child.source.clone(),
                        reason: "../ is forbidden".to_string(),
                    });
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(RuntimeError::InvalidChildSource {
                        parent: parent_plugin_path.to_string(),
                        child_source: child.source.clone(),
                        reason: "absolute path is forbidden".to_string(),
                    });
                }
            }
        }

        if normal_components.len() != 1 {
            return Err(RuntimeError::InvalidChildSource {
                parent: parent_plugin_path.to_string(),
                child_source: child.source.clone(),
                reason: "each plugin may only declare direct children".to_string(),
            });
        }

        let child_component = normal_components[0].clone();
        let child_dir = parent_dir.join(&child_component);
        if !child_dir.exists() {
            return Err(RuntimeError::ChildNotFound {
                parent: parent_plugin_path.to_string(),
                child_source: child.source.clone(),
            });
        }

        Ok((child_dir, child_component))
    }
}

pub fn normalize_crate_name(plugin_path: &str) -> String {
    plugin_path
        .chars()
        .map(|ch| match ch {
            '/' | '-' | '.' => '_',
            _ => ch,
        })
        .collect()
}
