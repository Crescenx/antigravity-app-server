use crate::*;
use std::path::Path;

pub fn roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![cwd.join(".agents/skills"), cwd.join(".codex/skills")];
    if let Ok(path) = std::env::var("AGY_SKILLS_ROOT") {
        roots.push(PathBuf::from(path));
    } else if let Ok(home) = std::env::var("HOME") {
        roots.push(PathBuf::from(home).join(".agents/skills"));
    }
    roots
}
fn metadata(source: &str) -> Result<(String, String)> {
    let mut lines = source.lines();
    ensure!(lines.next() == Some("---"), "missing skill frontmatter");
    let mut name = None;
    let mut description = None;
    let mut multiline = false;
    for line in lines {
        if line == "---" {
            return Ok((
                name.context("missing skill name")?,
                description.context("missing skill description")?,
            ));
        }
        if multiline && (line.starts_with(' ') || line.trim().is_empty()) {
            let text = description.get_or_insert_with(String::new);
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(line.trim());
            continue;
        }
        multiline = false;
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key != "name" && key != "description" {
            continue;
        }
        let value = value.trim();
        if key == "description" && [">", ">-", "|", "|-"].contains(&value) {
            description = Some(String::new());
            multiline = true;
            continue;
        }
        ensure!(
            !value.is_empty() && !value.starts_with(['[', '{', '&', '*']),
            "unsupported skill metadata scalar"
        );
        let value = if value.starts_with('"') {
            serde_json::from_str::<String>(value).context("unsupported quoted skill scalar")?
        } else if value.starts_with('\'') {
            ensure!(value.ends_with('\''), "unclosed skill scalar");
            value[1..value.len() - 1].replace("''", "'")
        } else {
            value.to_owned()
        };
        if key == "name" {
            name = Some(value);
        } else {
            description = Some(value);
        }
    }
    bail!("unclosed skill frontmatter")
}
fn settings(store: &Store) -> Result<BTreeMap<String, bool>> {
    let path = store.root.join("skills.json");
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
pub fn discover(store: &Store, cwd: &Path) -> Result<Value> {
    let settings = settings(store)?;
    let mut skills = Vec::new();
    let mut errors = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    for root in roots(cwd) {
        if !root.exists() {
            continue;
        }
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) => {
                errors.push(json!({"path":root,"message":error.to_string()}));
                continue;
            }
        };
        let mut paths = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect::<Vec<_>>();
        paths.sort();
        for folder in paths {
            let path = folder.join("SKILL.md");
            if !path.exists() {
                continue;
            }
            let read = || -> Result<Value> {
                let canonical = path.canonicalize()?;
                ensure!(
                    std::fs::metadata(&canonical)?.len() <= 1_000_000,
                    "skill metadata file too large"
                );
                let (name, description) = metadata(&std::fs::read_to_string(&canonical)?)?;
                let enabled = settings
                    .get(canonical.to_str().context("skill path is not UTF-8")?)
                    .copied()
                    .unwrap_or(true);
                Ok(
                    json!({"name":name,"description":description,"path":canonical,"enabled":enabled,"scope":if root.starts_with(cwd){"repo"}else{"user"}}),
                )
            };
            match read() {
                Ok(skill) => {
                    if names.insert(skill["name"].as_str().unwrap().to_owned()) {
                        skills.push(skill);
                    }
                }
                Err(error) => errors.push(json!({"path":path,"message":error.to_string()})),
            }
        }
    }
    Ok(json!({"cwd":cwd,"skills":skills,"errors":errors}))
}
pub fn write(store: &Store, params: &Value, cwds: &[PathBuf]) -> Result<Value> {
    let enabled = params["enabled"]
        .as_bool()
        .context("enabled must be boolean")?;
    let mut matches = std::collections::BTreeSet::new();
    for cwd in cwds {
        let group = discover(store, cwd)?;
        for skill in group["skills"].as_array().unwrap() {
            if params["path"].as_str().is_some_and(|p| {
                PathBuf::from(p)
                    .canonicalize()
                    .ok()
                    .is_some_and(|p| json!(p) == skill["path"])
            }) || (params["path"].is_null() && params["name"] == skill["name"])
            {
                matches.insert(skill["path"].as_str().unwrap().to_owned());
            }
        }
    }
    ensure!(
        matches.len() == 1,
        "skill selector must match exactly one discovered skill"
    );
    let mut settings = settings(store)?;
    settings.insert(matches.into_iter().next().unwrap(), enabled);
    let path = store.root.join("skills.json");
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(&settings)?)?;
    std::fs::File::open(&temp)?.sync_all()?;
    std::fs::rename(temp, path)?;
    std::fs::File::open(&store.root)?.sync_all()?;
    Ok(json!({"effectiveEnabled":enabled}))
}
