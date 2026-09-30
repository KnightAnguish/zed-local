//! Read the existing profile registry. Never write another registry or enable a disabled skill.
use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    version: u32,
    #[serde(default)]
    groups: HashMap<String, Vec<String>>,
    profiles: HashMap<String, Profile>,
    projects: Vec<Project>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Profile {
    #[serde(default)]
    disable_groups: Vec<String>,
    #[serde(default)]
    disable: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Project {
    root: String,
    profile: String,
}

pub(super) struct Selected {
    selectors: Vec<String>,
    cached: Vec<(String, String)>,
}

fn normalized(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path.trim_end_matches('/');
    if path.as_bytes().get(1) == Some(&b':') || path.starts_with("//") {
        path.to_lowercase()
    } else {
        path.to_owned()
    }
}

fn select(source: &str, cwd: &str) -> Result<Option<Selected>> {
    let registry: Registry = serde_json::from_str(source.trim_start_matches('\u{feff}'))?;
    ensure!(
        registry.version == 1,
        "Unsupported skill profile registry version"
    );
    for project in &registry.projects {
        let bytes = project.root.as_bytes();
        let absolute = project.root.starts_with('/')
            || project.root.starts_with("\\\\")
            || (bytes.len() > 2
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && matches!(bytes[2], b'/' | b'\\'));
        ensure!(absolute, "Skill profile root must be absolute");
        ensure!(
            registry.profiles.contains_key(&project.profile),
            "Unknown skill profile {}",
            project.profile
        );
    }
    for profile in registry.profiles.values() {
        for group in &profile.disable_groups {
            ensure!(
                registry.groups.contains_key(group),
                "Unknown skill group {group}"
            );
        }
        ensure!(
            profile.disable.iter().all(|selector| !selector.is_empty()),
            "Empty skill selector"
        );
    }
    ensure!(
        registry
            .groups
            .values()
            .flatten()
            .all(|selector| !selector.is_empty()),
        "Empty skill group selector"
    );
    let cwd = normalized(cwd);
    let selected = registry
        .projects
        .iter()
        .filter(|project| {
            let root = normalized(&project.root);
            cwd == root || cwd.starts_with(&(root + "/"))
        })
        .max_by_key(|project| normalized(&project.root).len());
    let Some(selected) = selected else {
        return Ok(None);
    };
    let profile = &registry.profiles[&selected.profile];
    let selectors = profile
        .disable_groups
        .iter()
        .flat_map(|group| registry.groups[group].iter())
        .chain(profile.disable.iter())
        .cloned()
        .collect();
    Ok(Some(Selected {
        selectors,
        cached: Vec::new(),
    }))
}

fn read_limited(path: &Path, limit: u64) -> Result<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)?;
    let mut content = String::new();
    file.take(limit + 1).read_to_string(&mut content)?;
    ensure!(
        content.len() as u64 <= limit,
        "Skill configuration file is too large"
    );
    Ok(content)
}

fn directories(path: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry?;
        // Do not traverse links outside the cache.
        if entry.file_type()?.is_dir() {
            directories.push(entry.path());
        }
    }
    Ok(directories)
}

pub(super) fn load(
    home: &Path,
    cwd: &str,
    override_path: Option<&str>,
) -> Result<Option<Selected>> {
    if override_path == Some("off") {
        return Ok(None);
    }
    let file = override_path
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("acp-skill-profiles.json"));
    let source = match read_limited(&file, 1024 * 1024) {
        Ok(source) => source,
        Err(error)
            if override_path.is_none()
                && error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error.context("Cannot read skill profile registry")),
    };
    let Some(mut selected) = select(&source, cwd)? else {
        return Ok(None);
    };
    let plugins: Vec<_> = selected
        .selectors
        .iter()
        .filter_map(|selector| {
            selector
                .split_once(':')
                .map(|(plugin, _)| plugin.to_owned())
        })
        .collect();
    if plugins.is_empty() {
        return Ok(Some(selected));
    }
    for marketplace in directories(&home.join("plugins/cache"))? {
        for plugin in directories(&marketplace)? {
            let Some(plugin_name) = plugin.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !plugins.iter().any(|name| name == plugin_name) {
                continue;
            }
            for version in directories(&plugin)? {
                for folder in directories(&version.join("skills"))? {
                    let file = folder.join("SKILL.md");
                    let source = match read_limited(&file, 256 * 1024) {
                        Ok(source) => source,
                        Err(error)
                            if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                                error.kind() == std::io::ErrorKind::NotFound
                            }) =>
                        {
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let mut lines = source.trim_start_matches('\u{feff}').lines();
                    if lines.next() != Some("---") {
                        continue;
                    }
                    for line in lines.take_while(|line| *line != "---") {
                        if let Some(name) = line.strip_prefix("name:") {
                            let name = name.trim().trim_matches(['\'', '"']);
                            if !name.is_empty()
                                && name.chars().all(|character| {
                                    character.is_ascii_alphanumeric()
                                        || matches!(character, '_' | '-')
                                })
                            {
                                selected.cached.push((
                                    format!("{plugin_name}:{name}"),
                                    file.to_string_lossy().into_owned(),
                                ));
                                ensure!(selected.cached.len() <= 10_000, "Too many cached skills");
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(Some(selected))
}

impl Selected {
    fn disables(&self, name: &str) -> bool {
        self.selectors.iter().any(|selector| {
            selector
                .strip_suffix('*')
                .map_or(name == selector, |prefix| name.starts_with(prefix))
        })
    }
    pub(super) fn apply(&self, catalog: &Value, inherited: &Value) -> Result<Value> {
        let mut entries = BTreeMap::new();
        if let Some(inherited) = inherited.as_array() {
            for entry in inherited {
                if let Some(path) = entry["path"].as_str() {
                    entries.insert(normalized(path), entry.clone());
                }
            }
        }
        for entry in catalog["data"]
            .as_array()
            .context("Missing Codex skill catalog")?
        {
            for skill in entry["skills"]
                .as_array()
                .context("Missing skills in Codex catalog")?
            {
                let name = skill["name"].as_str().context("Missing skill name")?;
                let path = skill["path"].as_str().context("Missing skill path")?;
                if self.disables(name) {
                    entries.insert(normalized(path), json!({"path":path,"enabled":false}));
                }
            }
        }
        for (name, path) in &self.cached {
            if self.disables(name) {
                entries.insert(normalized(path), json!({"path":path,"enabled":false}));
            }
        }
        Ok(json!({"skills.config":entries.into_values().collect::<Vec<_>>()}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn longest_root_and_path_boundary_choose_profile() {
        let source = json!({"version":1,"groups":{"web":["vercel:*"]},"profiles":{"outer":{"disableGroups":["web"]},"inner":{"disable":["exact"]}},"projects":[{"root":"C:/Books","profile":"outer"},{"root":"C:/Books/One","profile":"inner"}]}).to_string();
        assert!(
            select(&source, "c:\\BOOKS\\one\\chapter")
                .unwrap()
                .unwrap()
                .disables("exact")
        );
        assert!(
            select(&source, "C:/Books/two")
                .unwrap()
                .unwrap()
                .disables("vercel:deploy")
        );
        assert!(select(&source, "C:/BooksElse").unwrap().is_none());
    }
    #[test]
    fn disable_merges_without_enabling_inherited_entries() {
        let selected = Selected {
            selectors: vec!["vercel:*".into()],
            cached: vec![("vercel:deploy".into(), "C:/cache/SKILL.md".into())],
        };
        let result = selected.apply(&json!({"data":[{"skills":[{"name":"other","path":"C:/other"}]}]}), &json!([{"path":"c:/CACHE/SKILL.md","enabled":true},{"path":"C:/other","enabled":false}])).unwrap();
        assert_eq!(result["skills.config"].as_array().unwrap().len(), 2);
        assert!(
            result["skills.config"]
                .as_array()
                .unwrap()
                .iter()
                .all(|value| value["enabled"] == false)
        );
    }
    #[test]
    fn malformed_registry_is_not_silently_ignored() {
        for source in [
            json!({"version":2,"profiles":{},"projects":[]}),
            json!({"version":1,"profiles":{},"projects":[{"root":"C:/Books","profile":"missing"}]}),
            json!({"version":1,"profiles":{"a":{"disableGroups":["missing"]}},"projects":[]}),
        ] {
            assert!(select(&source.to_string(), "C:/Books").is_err());
        }
    }
}
