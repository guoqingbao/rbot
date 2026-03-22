use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::util::workspace_state_dir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub name: String,
    pub path: PathBuf,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct SkillsLoader {
    workspace: PathBuf,
    workspace_skills: PathBuf,
    builtin_skills: Option<PathBuf>,
}

impl SkillsLoader {
    pub fn new(workspace: impl AsRef<Path>, builtin_skills_dir: Option<PathBuf>) -> Self {
        let workspace = workspace.as_ref().to_path_buf();
        Self {
            workspace_skills: workspace_state_dir(&workspace).join("skills"),
            workspace,
            builtin_skills: builtin_skills_dir.or_else(default_builtin_skills_dir),
        }
    }

    pub fn list_skills(&self, filter_unavailable: bool) -> Vec<SkillInfo> {
        let mut skills = Vec::new();

        if self.workspace_skills.exists() {
            for entry in read_skill_dirs(&self.workspace_skills) {
                if !skills
                    .iter()
                    .any(|skill: &SkillInfo| skill.name == entry.name)
                {
                    skills.push(entry);
                }
            }
        }
        let legacy_workspace_skills = self.workspace.join("skills");
        if legacy_workspace_skills.exists() {
            for entry in read_skill_dirs(&legacy_workspace_skills) {
                if !skills
                    .iter()
                    .any(|skill: &SkillInfo| skill.name == entry.name)
                {
                    skills.push(entry);
                }
            }
        }

        if let Some(builtin) = &self.builtin_skills {
            if builtin.exists() {
                for entry in read_skill_dirs(builtin) {
                    if !skills
                        .iter()
                        .any(|skill: &SkillInfo| skill.name == entry.name)
                    {
                        skills.push(entry);
                    }
                }
            }
        }

        if filter_unavailable {
            skills
                .into_iter()
                .filter(|skill| {
                    self.get_skill_metadata(&skill.name)
                        .map(|meta| self.check_requirements(&self.get_skill_meta_map(&meta)))
                        .unwrap_or(true)
                })
                .collect()
        } else {
            skills
        }
    }

    pub fn load_skill(&self, name: &str) -> Option<String> {
        let workspace_skill = self.workspace_skills.join(name).join("SKILL.md");
        if workspace_skill.exists() {
            return fs::read_to_string(workspace_skill).ok();
        }
        let legacy_workspace_skill = self.workspace.join("skills").join(name).join("SKILL.md");
        if legacy_workspace_skill.exists() {
            return fs::read_to_string(legacy_workspace_skill).ok();
        }
        let builtin_skill = self
            .builtin_skills
            .as_ref()
            .map(|dir| dir.join(name).join("SKILL.md"))?;
        fs::read_to_string(builtin_skill).ok()
    }

    pub fn load_skills_for_context(&self, skill_names: &[String]) -> String {
        let mut parts = Vec::new();
        for name in skill_names {
            if let Some(content) = self.load_skill(name) {
                parts.push(format!(
                    "### Skill: {}\n\n{}",
                    name,
                    self.strip_frontmatter(&content)
                ));
            }
        }
        parts.join("\n\n---\n\n")
    }

    pub fn build_skills_summary(&self) -> String {
        let skills = self.list_skills(false);
        if skills.is_empty() {
            return String::new();
        }
        let mut lines = vec!["<skills>".to_string()];
        for skill in skills {
            let metadata = self.get_skill_metadata(&skill.name).unwrap_or_default();
            let desc = escape_xml(
                metadata
                    .get("description")
                    .cloned()
                    .unwrap_or_else(|| skill.name.clone()),
            );
            let meta = self.get_skill_meta_map(&metadata);
            let available = self.check_requirements(&meta);
            lines.push(format!(
                "  <skill available=\"{}\">",
                if available { "true" } else { "false" }
            ));
            lines.push(format!(
                "    <name>{}</name>",
                escape_xml(skill.name.clone())
            ));
            lines.push(format!("    <description>{desc}</description>"));
            lines.push(format!("    <location>{}</location>", skill.path.display()));
            if !available {
                let missing = self.get_missing_requirements(&meta);
                if !missing.is_empty() {
                    lines.push(format!("    <requires>{}</requires>", escape_xml(missing)));
                }
            }
            lines.push("  </skill>".to_string());
        }
        lines.push("</skills>".to_string());
        lines.join("\n")
    }

    pub fn get_always_skills(&self) -> Vec<String> {
        self.list_skills(true)
            .into_iter()
            .filter_map(|skill| {
                let metadata = self.get_skill_metadata(&skill.name)?;
                let meta = self.get_skill_meta_map(&metadata);
                if meta
                    .get("always")
                    .map(|value| value == "true")
                    .unwrap_or_else(|| {
                        metadata
                            .get("always")
                            .map(|value| value == "true")
                            .unwrap_or(false)
                    })
                {
                    Some(skill.name)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn suggest_skills(&self, prompt: &str, limit: usize) -> Vec<String> {
        let lowered = prompt.to_ascii_lowercase();
        let mut matches = Vec::new();
        for skill in self.list_skills(true) {
            let Some(metadata) = self.get_skill_metadata(&skill.name) else {
                continue;
            };
            let meta = self.get_skill_meta_map(&metadata);
            let triggers = meta
                .get("triggers")
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                .and_then(|value| value.as_array().cloned())
                .unwrap_or_default();
            if triggers
                .iter()
                .filter_map(|value| value.as_str())
                .any(|trigger| {
                    let trigger = trigger.to_ascii_lowercase();
                    lowered.contains(&trigger)
                })
            {
                matches.push(skill.name.clone());
            }
            if matches.len() >= limit {
                break;
            }
        }
        matches
    }

    pub fn get_skill_metadata(&self, name: &str) -> Option<BTreeMap<String, String>> {
        let content = self.load_skill(name)?;
        parse_frontmatter(&content)
    }

    fn get_skill_meta_map(&self, metadata: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        metadata
            .get("metadata")
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|value| value.get("rbot").cloned())
            .and_then(|value| {
                value.as_object().map(|obj| {
                    obj.iter()
                        .map(|(key, value)| {
                            (
                                key.clone(),
                                value
                                    .as_str()
                                    .map(ToOwned::to_owned)
                                    .unwrap_or_else(|| value.to_string()),
                            )
                        })
                        .collect::<BTreeMap<_, _>>()
                })
            })
            .unwrap_or_default()
    }

    fn check_requirements(&self, meta: &BTreeMap<String, String>) -> bool {
        let requires = meta.get("requires").cloned().unwrap_or_default();
        if requires.is_empty() {
            return true;
        }
        let parsed = serde_json::from_str::<serde_json::Value>(&requires).ok();
        let Some(parsed) = parsed else {
            return true;
        };
        if let Some(bins) = parsed.get("bins").and_then(|value| value.as_array()) {
            for bin in bins.iter().filter_map(|value| value.as_str()) {
                if which(bin).is_none() {
                    return false;
                }
            }
        }
        if let Some(envs) = parsed.get("env").and_then(|value| value.as_array()) {
            for key in envs.iter().filter_map(|value| value.as_str()) {
                if env::var_os(key).is_none() {
                    return false;
                }
            }
        }
        true
    }

    fn get_missing_requirements(&self, meta: &BTreeMap<String, String>) -> String {
        let mut missing = Vec::new();
        let requires = meta.get("requires").cloned().unwrap_or_default();
        let Some(parsed) = serde_json::from_str::<serde_json::Value>(&requires).ok() else {
            return String::new();
        };
        if let Some(bins) = parsed.get("bins").and_then(|value| value.as_array()) {
            for bin in bins.iter().filter_map(|value| value.as_str()) {
                if which(bin).is_none() {
                    missing.push(format!("CLI: {bin}"));
                }
            }
        }
        if let Some(envs) = parsed.get("env").and_then(|value| value.as_array()) {
            for key in envs.iter().filter_map(|value| value.as_str()) {
                if env::var_os(key).is_none() {
                    missing.push(format!("ENV: {key}"));
                }
            }
        }
        missing.join(", ")
    }

    fn strip_frontmatter(&self, content: &str) -> String {
        if let Some((_, body)) = split_frontmatter(content) {
            body.trim().to_string()
        } else {
            content.to_string()
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
}

fn default_builtin_skills_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("skills");
    dir.exists().then_some(dir)
}

fn read_skill_dirs(dir: &Path) -> Vec<SkillInfo> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_file = path.join("SKILL.md");
        if !skill_file.exists() {
            continue;
        }
        out.push(SkillInfo {
            name: entry.file_name().to_string_lossy().to_string(),
            path: skill_file,
            source: if dir.ends_with("skills") && dir.parent().is_some() {
                if dir.ends_with("skills") && dir.to_string_lossy().contains(".rbot") {
                    "workspace".to_string()
                } else {
                    "builtin".to_string()
                }
            } else {
                "builtin".to_string()
            },
        });
    }
    out
}

fn split_frontmatter(content: &str) -> Option<(String, String)> {
    let mut lines = content.lines();
    if lines.next()? != "---" {
        return None;
    }
    let mut frontmatter = Vec::new();
    let mut body = Vec::new();
    let mut in_frontmatter = true;
    for line in content.lines().skip(1) {
        if in_frontmatter && line == "---" {
            in_frontmatter = false;
            continue;
        }
        if in_frontmatter {
            frontmatter.push(line);
        } else {
            body.push(line);
        }
    }
    (!in_frontmatter).then(|| (frontmatter.join("\n"), body.join("\n")))
}

fn parse_frontmatter(content: &str) -> Option<BTreeMap<String, String>> {
    let (frontmatter, _) = split_frontmatter(content)?;
    let mut metadata = BTreeMap::new();
    for line in frontmatter.lines() {
        if let Some((key, value)) = line.split_once(':') {
            metadata.insert(
                key.trim().to_string(),
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            );
        }
    }
    Some(metadata)
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let candidate_exe = dir.join(format!("{bin}.exe"));
            if candidate_exe.is_file() {
                return Some(candidate_exe);
            }
        }
        None
    })
}

fn escape_xml(value: String) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::SkillsLoader;
    use tempfile::tempdir;

    #[test]
    fn workspace_skill_overrides_builtin_skill() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let builtin = dir.path().join("builtin");
        std::fs::create_dir_all(workspace.join(".rbot/skills/echo")).unwrap();
        std::fs::create_dir_all(builtin.join("echo")).unwrap();
        std::fs::write(
            workspace.join(".rbot/skills/echo/SKILL.md"),
            "---\ndescription: Workspace Echo\n---\nworkspace skill",
        )
        .unwrap();
        std::fs::write(
            builtin.join("echo/SKILL.md"),
            "---\ndescription: Builtin Echo\n---\nbuiltin skill",
        )
        .unwrap();

        let loader = SkillsLoader::new(&workspace, Some(builtin));
        let skills = loader.list_skills(false);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "echo");
        assert!(
            loader
                .load_skill("echo")
                .unwrap()
                .contains("workspace skill")
        );
    }

    #[test]
    fn summary_reflects_rbot_metadata() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let builtin = dir.path().join("builtin");
        std::fs::create_dir_all(builtin.join("always")).unwrap();
        std::fs::write(
            builtin.join("always/SKILL.md"),
            "---\ndescription: Always skill\nmetadata: {\"rbot\":{\"always\":true}}\n---\nhello",
        )
        .unwrap();

        let loader = SkillsLoader::new(&workspace, Some(builtin));
        let summary = loader.build_skills_summary();
        assert!(summary.contains("<skills>"));
        assert!(summary.contains("Always skill"));
        assert_eq!(loader.get_always_skills(), vec!["always".to_string()]);
        assert!(
            loader
                .load_skills_for_context(&["always".to_string()])
                .contains("### Skill: always")
        );
    }
}
