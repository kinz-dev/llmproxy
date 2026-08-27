//! Project-scoped routing overlay.
//!
//! A repository may carry a `.llmproxy.toml` that adds integrated route
//! targets and model aliases for work done inside it. It may define nothing
//! else: providers, base URLs, and credentials stay user-level, because a
//! project file lives in a repository and cloning an untrusted repository must
//! never be able to decide where prompts are sent or which key pays for them.
//!
//! The scope travels to the daemon as a path inside the launcher session
//! token. The token is unsigned, so the daemon re-reads the file from that path
//! and re-validates it against its own provider set rather than trusting
//! anything the token carried.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::ModelAlias;
use crate::interceptor::IntegratedTarget;

pub(crate) const PROJECT_FILE_NAME: &str = ".llmproxy.toml";

/// How deep a project search walks before giving up. A repository nested more
/// than this far below its own root is not a case worth serving, and an
/// unbounded walk would climb out of the user's home on a stray cwd.
const MAX_SEARCH_DEPTH: usize = 64;

/// The complete set of keys a project file may define. `deny_unknown_fields`
/// is the enforcement: a project that tries to declare `[providers.*]`,
/// `listen_addr`, or an interceptor override is rejected by name rather than
/// quietly ignored.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProjectFile {
    pub(crate) integrated: ProjectIntegrated,
    pub(crate) model_aliases: HashMap<String, ModelAlias>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProjectIntegrated {
    pub(crate) targets: BTreeMap<String, IntegratedTarget>,
}

impl ProjectFile {
    pub(crate) fn is_empty(&self) -> bool {
        self.integrated.targets.is_empty() && self.model_aliases.is_empty()
    }

    pub(crate) fn parse(raw: &str) -> Result<Self> {
        toml::from_str(raw).context("invalid project configuration")
    }
}

/// A loaded project overlay plus the identity used to attribute its traffic.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProjectScope {
    root: PathBuf,
    name: String,
    file: ProjectFile,
}

impl ProjectScope {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Stable, human-readable project identity: the directory name holding the
    /// project file. Spend and memory are keyed by it.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn file(&self) -> &ProjectFile {
        &self.file
    }

    /// Read and parse `<root>/.llmproxy.toml`. Structural validation against a
    /// provider set happens later, in [`crate::validate_config`], because only
    /// the daemon knows which providers exist.
    pub(crate) fn load(root: &Path) -> Result<Self> {
        let path = root.join(PROJECT_FILE_NAME);
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read project config {}", path.display()))?;
        let file = ProjectFile::parse(&raw)
            .with_context(|| format!("invalid project config {}", path.display()))?;
        Ok(Self {
            name: project_name(root),
            root: root.to_path_buf(),
            file,
        })
    }

    /// Merge this overlay into a user-level config. Project entries win on a
    /// name collision; nothing user-level is removed.
    pub(crate) fn apply(&self, config: &mut crate::Config) {
        for (alias, route) in &self.file.model_aliases {
            config.model_aliases.insert(alias.clone(), route.clone());
        }
        for (name, target) in &self.file.integrated.targets {
            config
                .integrated
                .targets
                .insert(name.clone(), target.clone());
        }
    }
}

/// Directory name of the project root, falling back to a stable placeholder
/// for a root with no usable name (a bare `/`, or a non-UTF-8 component).
fn project_name(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "project".to_string())
}

/// Nearest ancestor of `start` (inclusive) that holds a project file.
pub(crate) fn discover_from(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    for _ in 0..MAX_SEARCH_DEPTH {
        let directory = current?;
        if directory.join(PROJECT_FILE_NAME).is_file() {
            return Some(directory.to_path_buf());
        }
        current = directory.parent();
    }
    None
}

/// Resolve the scope for a launcher: an explicit `--project` path when given,
/// otherwise the nearest project file at or above the current directory.
pub(crate) fn resolve(explicit: Option<&Path>, cwd: &Path) -> Result<Option<ProjectScope>> {
    let Some(root) = (match explicit {
        Some(path) => {
            let root = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            if !root.join(PROJECT_FILE_NAME).is_file() {
                bail!(
                    "--project {} does not contain a {PROJECT_FILE_NAME}",
                    root.display()
                );
            }
            Some(root)
        }
        None => discover_from(cwd),
    }) else {
        return Ok(None);
    };
    let scope = ProjectScope::load(&root)?;
    if scope.file.is_empty() {
        // An empty overlay still names the project, which is what spend
        // attribution and project memory need.
        tracing::debug!(project = scope.name(), "project config defines no overlay");
    }
    Ok(Some(scope))
}

/// Bounded, briefly cached project files.
///
/// The scope is resolved on every launcher request, so re-reading the file
/// each time would put filesystem I/O on the hot path. The TTL is short so an
/// edit to a project file takes effect without restarting the daemon.
const CACHE_TTL: Duration = Duration::from_secs(15);
const CACHE_CAPACITY: usize = 64;

struct CacheEntry {
    scope: Arc<ProjectScope>,
    loaded_at: Instant,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn cached_scope(root: &Path) -> Result<Arc<ProjectScope>> {
    {
        let cache = cache().lock().expect("project cache poisoned");
        if let Some(entry) = cache.get(root)
            && entry.loaded_at.elapsed() < CACHE_TTL
        {
            return Ok(entry.scope.clone());
        }
    }
    let scope = Arc::new(ProjectScope::load(root)?);
    let mut cache = cache().lock().expect("project cache poisoned");
    if cache.len() >= CACHE_CAPACITY {
        cache.retain(|_, entry| entry.loaded_at.elapsed() < CACHE_TTL);
        if cache.len() >= CACHE_CAPACITY {
            cache.clear();
        }
    }
    cache.insert(
        root.to_path_buf(),
        CacheEntry {
            scope: scope.clone(),
            loaded_at: Instant::now(),
        },
    );
    Ok(scope)
}

#[cfg(test)]
pub(crate) fn clear_cache() {
    cache().lock().expect("project cache poisoned").clear();
}

/// Structural validation of the merged result. `providers` is the daemon's own
/// effective provider set, so a project can only ever name a provider the user
/// already configured.
pub(crate) fn validate_overlay(
    file: &ProjectFile,
    has_provider: impl Fn(&str) -> bool,
    default_provider: &str,
) -> Result<()> {
    for (alias, route) in &file.model_aliases {
        if alias == crate::INTEGRATED_AUTO_MODEL {
            bail!(
                "project model alias {:?} is reserved for integrated routing",
                crate::INTEGRATED_AUTO_MODEL
            );
        }
        let provider = route.provider(default_provider);
        if !has_provider(provider) {
            bail!(
                "project model alias {alias:?} references provider {provider:?}, \
                 which is not defined in the user-level configuration"
            );
        }
    }
    for (name, target) in &file.integrated.targets {
        if !has_provider(&target.provider) {
            bail!(
                "project target {name:?} references provider {:?}, \
                 which is not defined in the user-level configuration",
                target.provider
            );
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "project_tests.rs"]
mod tests;
