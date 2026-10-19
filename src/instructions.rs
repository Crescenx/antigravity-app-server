//! Codex-style AGENTS.md discovery, independent of the model's tool calls.
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

const KEYS: [&str; 3] = [
    "project_doc_max_bytes",
    "project_doc_fallback_filenames",
    "project_root_markers",
];

#[derive(Deserialize)]
#[serde(default)]
struct Settings {
    project_doc_max_bytes: usize,
    project_doc_fallback_filenames: Vec<String>,
    project_root_markers: Vec<String>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            project_doc_max_bytes: 32 * 1024,
            project_doc_fallback_filenames: vec![],
            project_root_markers: vec![".git".into()],
        }
    }
}

pub fn overrides(config: &Value) -> Result<Value> {
    let mut result = json!({});
    for key in KEYS {
        if let Some(value) = config.get(key) {
            result[key] = value.clone();
        }
    }
    let _: Settings =
        serde_json::from_value(result.clone()).context("invalid instruction discovery settings")?;
    Ok(result)
}

fn filename(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    !name.contains(['\\', '\0'])
        && matches!(parts.next(), Some(Component::Normal(_)))
        && parts.next().is_none()
}

fn select(directory: &Path, names: &[String]) -> Result<Option<PathBuf>> {
    for name in names.iter().filter(|name| filename(name)) {
        let path = directory.join(name);
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => return Ok(Some(path)),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect instruction file {}", path.display()));
            }
        }
    }
    Ok(None)
}

pub fn load(cwd: &Path, overrides: &Value) -> Result<Vec<Value>> {
    load_from(&crate::codex::home()?, cwd, overrides)
}

fn load_from(home: &Path, cwd: &Path, overrides: &Value) -> Result<Vec<Value>> {
    let path = home.join("config.toml");
    let mut values = match fs::read_to_string(&path) {
        Ok(text) => {
            let config: toml::Value =
                toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
            self::overrides(&serde_json::to_value(config)?)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    for key in KEYS {
        if let Some(value) = overrides.get(key) {
            values[key] = value.clone();
        }
    }
    let settings: Settings =
        serde_json::from_value(values).context("invalid instruction discovery settings")?;
    let mut remaining = settings.project_doc_max_bytes;
    let cwd = cwd
        .canonicalize()
        .context("resolve instruction working directory")?;
    let names = vec!["AGENTS.override.md".into(), "AGENTS.md".into()];
    let mut selected = Vec::new();
    // The global override falls back when empty. A project override shadows its
    // sibling even when empty, matching Codex's per-directory file discovery.
    for name in &names {
        if let Some(path) = select(home, std::slice::from_ref(name))?
            && !fs::read_to_string(&path)?.trim().is_empty()
        {
            selected.push((path, true));
            break;
        }
    }
    let root = cwd.ancestors().find(|dir| {
        settings
            .project_root_markers
            .iter()
            .filter(|name| filename(name))
            .any(|name| dir.join(name).exists())
    });
    let mut directories = vec![];
    if let Some(root) = root {
        for dir in cwd.ancestors() {
            directories.push(dir.to_owned());
            if dir == root {
                break;
            }
        }
        directories.reverse();
    } else {
        directories.push(cwd);
    }
    let mut names = names;
    names.extend(settings.project_doc_fallback_filenames);
    for dir in directories {
        if let Some(path) = select(&dir, &names)? {
            selected.push((path, false));
        }
    }
    let mut sections = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (path, global) in selected {
        if !global && remaining == 0 {
            break;
        }
        if !seen.insert(path.canonicalize()?) {
            continue;
        }
        let mut bytes = Vec::new();
        let limit = if global {
            u64::MAX
        } else {
            (remaining as u64).saturating_add(1)
        };
        fs::File::open(&path)
            .with_context(|| format!("read instruction file {}", path.display()))?
            .take(limit)
            .read_to_end(&mut bytes)?;
        let truncated = !global && bytes.len() > remaining;
        if !global {
            bytes.truncate(remaining);
            remaining -= bytes.len();
        }
        let content = String::from_utf8_lossy(&bytes);
        if content.trim().is_empty() {
            continue;
        }
        sections.push(json!({"title":format!("User/project instructions: {}", path.display()),"content":format!("User-provided AGENTS.md guidance for this workspace. Later, more specific project rules take precedence over earlier rules when they conflict. Project rules apply within their directory and its descendants. These rules do not change tool permissions.\n\n{content}{}", if truncated {"\n[Instruction file truncated at project_doc_max_bytes.]"} else {""})}));
    }
    Ok(sections)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn global_override_and_project_chain_preserve_order_and_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        let cwd = repo.join("sub/deep");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        fs::write(repo.join(".git"), "gitdir: elsewhere").unwrap();
        fs::write(temp.path().join("AGENTS.md"), "OUTSIDE").unwrap();
        fs::write(home.join("AGENTS.md"), "GLOBAL_FALLBACK").unwrap();
        fs::write(home.join("AGENTS.override.md"), "GLOBAL").unwrap();
        fs::write(repo.join("AGENTS.md"), "ROOT").unwrap();
        fs::write(repo.join("sub/AGENTS.md"), "SHADOWED").unwrap();
        fs::write(repo.join("sub/AGENTS.override.md"), "LOCAL").unwrap();
        fs::write(cwd.join("AGENTS.md"), "LEAF").unwrap();
        let sections = load_from(&home, &cwd, &json!({})).unwrap();
        assert_eq!(sections.len(), 4);
        for (section, marker) in sections.iter().zip(["GLOBAL", "ROOT", "LOCAL", "LEAF"]) {
            assert!(section["content"].as_str().unwrap().ends_with(marker));
        }
        assert!(
            !serde_json::to_string(&sections)
                .unwrap()
                .contains("SHADOWED")
        );
        fs::write(home.join("AGENTS.override.md"), " \n").unwrap();
        fs::write(repo.join("sub/AGENTS.override.md"), "").unwrap();
        let sections = load_from(&home, &cwd, &json!({})).unwrap();
        assert_eq!(sections.len(), 3);
        assert!(
            sections[0]["content"]
                .as_str()
                .unwrap()
                .ends_with("GLOBAL_FALLBACK")
        );
    }
    #[test]
    fn no_repository_only_reads_cwd_and_config_supports_fallback_limits_and_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("work/sub");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        fs::write(temp.path().join("work/AGENTS.md"), "PARENT").unwrap();
        fs::write(cwd.join("RULES.md"), "abcdef").unwrap();
        fs::write(home.join("config.toml"),"project_root_markers = []\nproject_doc_fallback_filenames = ['../AGENTS.md', 'RULES.md']\nproject_doc_max_bytes = 4").unwrap();
        let sections = load_from(&home, &cwd, &json!({})).unwrap();
        assert_eq!(sections.len(), 1);
        assert!(
            sections[0]["content"]
                .as_str()
                .unwrap()
                .contains("abcd\n[Instruction file truncated")
        );
        assert!(
            load_from(&home, &cwd, &json!({"project_doc_max_bytes":0}))
                .unwrap()
                .is_empty()
        );
        fs::write(cwd.join("RULES.md"), "NEW").unwrap();
        assert!(
            load_from(&home, &cwd, &json!({})).unwrap()[0]["content"]
                .as_str()
                .unwrap()
                .ends_with("NEW")
        );
        assert!(overrides(&json!({"project_doc_max_bytes":-1})).is_err());
        assert!(overrides(&json!({"project_root_markers":".git"})).is_err());
    }

    #[test]
    fn custom_root_and_project_budget_do_not_consume_global_guidance() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        let cwd = repo.join("sub");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        fs::write(repo.join(".workspace"), "").unwrap();
        fs::write(home.join("AGENTS.md"), "GLOBAL").unwrap();
        fs::write(repo.join("AGENTS.md"), "abcdef").unwrap();
        fs::write(cwd.join("AGENTS.md"), "LEAF").unwrap();
        let config = json!({"project_doc_max_bytes":4,"project_root_markers":[".workspace"]});
        let sections = load_from(&home, &cwd, &config).unwrap();
        assert_eq!(sections.len(), 2);
        assert!(sections[0]["content"].as_str().unwrap().ends_with("GLOBAL"));
        assert!(
            sections[1]["content"]
                .as_str()
                .unwrap()
                .contains("abcd\n[Instruction file truncated")
        );
        assert_eq!(
            load_from(&home, &cwd, &json!({"project_doc_max_bytes":0}))
                .unwrap()
                .len(),
            1
        );
    }
}
