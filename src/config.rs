use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashSet},
    env, fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

/// Why an import of `~/.ssh/config` was requested.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportKind {
    FirstRun,
    Repair,
    Reload,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AppConfig {
    #[serde(default)]
    pub hosts: Vec<SshHost>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub saved_commands: Vec<SavedCommand>,
    #[serde(default)]
    pub open_tabs: Option<Vec<SavedTab>>,
    #[serde(default)]
    pub active_tab: Option<usize>,
    #[serde(default)]
    pub collapsed_groups: BTreeSet<String>,
    #[serde(default)]
    pub host_separators: BTreeSet<String>,
    /// Stored inverted so that word wrap stays on unless it was turned off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_word_wrap: bool,
    #[serde(default, skip_serializing_if = "PresentationSettings::is_default")]
    pub presentation: PresentationSettings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation_position: Option<[i32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation_size: Option<[u32; 2]>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PresentationSettings {
    pub font_size: Option<u8>,
    pub text_color: [u8; 3],
    pub background_color: [u8; 3],
    pub fill_window: bool,
    pub mask_keys: bool,
    pub mask_paths: bool,
    pub hide_host_details: bool,
    pub show_typing: bool,
    pub padding: u8,
}

impl Default for PresentationSettings {
    fn default() -> Self {
        Self {
            font_size: None,
            text_color: [225, 228, 238],
            background_color: [13, 14, 18],
            fill_window: true,
            mask_keys: true,
            mask_paths: false,
            hide_host_details: false,
            show_typing: true,
            padding: 8,
        }
    }
}

impl PresentationSettings {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SavedCommand {
    pub name: String,
    pub command: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedSplitDirection {
    Right,
    Down,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SavedTab {
    Local,
    Ssh {
        host: String,
    },
    Split {
        direction: SavedSplitDirection,
        ratio: u16,
        first: Box<SavedTab>,
        second: Box<SavedTab>,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SshHost {
    pub name: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub user: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub identity_file: String,
    #[serde(default = "default_group")]
    pub group: String,
    /// The `Host` alias in the user's SSH config. When set, connections use the
    /// alias alone so every option in that `Host` block still applies.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub alias: String,
}

fn default_port() -> u16 {
    22
}

fn default_group() -> String {
    "SSH Config".into()
}

impl SshHost {
    pub fn target(&self) -> String {
        let host = if self.address.is_empty() {
            &self.name
        } else {
            &self.address
        };

        if self.user.is_empty() {
            host.to_owned()
        } else {
            format!("{}@{host}", self.user)
        }
    }

    pub fn ssh_args(&self, batch: bool) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(config) = env::var_os("SHELLHOPPER_SSH_CONFIG") {
            args.extend(["-F".into(), PathBuf::from(config).display().to_string()]);
        }
        if batch {
            args.extend([
                "-o".into(),
                "BatchMode=yes".into(),
                "-o".into(),
                "ConnectTimeout=10".into(),
            ]);
        }
        if !self.alias.is_empty() {
            args.extend(["--".into(), self.alias.clone()]);
            return args;
        }
        if self.port != 22 {
            args.extend(["-p".into(), self.port.to_string()]);
        }
        if !self.identity_file.is_empty() {
            args.extend([
                "-i".into(),
                expand_home(&self.identity_file).display().to_string(),
            ]);
        }
        // Stop option parsing so a destination can never be read as a flag.
        args.push("--".into());
        args.push(self.target());
        args
    }

    fn connection_key(&self) -> Option<(String, String, u16)> {
        (!self.address.is_empty()).then(|| {
            (
                self.address.to_ascii_lowercase(),
                self.user.clone(),
                self.port,
            )
        })
    }
}

impl AppConfig {
    /// Loads the saved settings. The second value says whether the caller should
    /// import `~/.ssh/config` (off the UI thread) and pass it to `apply_import`.
    pub fn load() -> Result<(Self, Option<ImportKind>), String> {
        let path = config_path()?;
        let legacy = legacy_config_path()?;
        let source = if path.exists() {
            Some(path.clone())
        } else if legacy.exists() {
            Some(legacy)
        } else {
            None
        };
        if let Some(source) = source {
            let text = fs::read_to_string(&source)
                .map_err(|error| format!("Could not read {}: {error}", source.display()))?;
            let mut config: Self = toml::from_str(&text)
                .map_err(|error| format!("Could not parse {}: {error}", source.display()))?;
            config.normalize_groups();
            if source != path {
                config.save()?;
                debug_assert!(path.exists());
            }
            let import = config
                .hosts
                .iter()
                .any(|host| host.address.is_empty())
                .then_some(ImportKind::Repair);
            Ok((config, import))
        } else {
            Ok((
                Self {
                    groups: vec![default_group()],
                    ..Default::default()
                },
                Some(ImportKind::FirstRun),
            ))
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
        }
        if let Some(parent) = path.parent() {
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
        let text = toml::to_string_pretty(self)
            .map_err(|error| format!("Could not serialize settings: {error}"))?;
        // Write a private temporary file and rename it so a crash cannot truncate the settings.
        let temporary = path.with_extension("toml.tmp");
        let write = || -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, &path)
        };
        write().map_err(|error| format!("Could not save {}: {error}", path.display()))
    }

    /// Applies hosts read by `import_ssh_config`; returns how many were added.
    pub fn apply_import(&mut self, kind: ImportKind, imported: Vec<SshHost>) -> usize {
        if kind == ImportKind::Repair {
            self.repair_from_imported(&imported);
            self.normalize_groups();
            0
        } else {
            self.merge_imported(imported)
        }
    }

    /// Opts one saved host into its same-named SSH config block, but only when
    /// that block still resolves to the saved destination.
    pub fn use_ssh_alias(&mut self, name: &str, resolved: &SshHost) -> Result<SshHost, String> {
        let host = self
            .hosts
            .iter_mut()
            .find(|host| host.name == name)
            .ok_or_else(|| format!("{name} is no longer in the sidebar."))?;
        if host.connection_key() != resolved.connection_key() {
            return Err(format!(
                "The SSH alias {name} resolves to {}, but this saved host is {}. Nothing was changed.",
                connection_label(resolved),
                connection_label(host)
            ));
        }
        host.alias = name.to_owned();
        Ok(host.clone())
    }

    fn merge_imported(&mut self, imported: Vec<SshHost>) -> usize {
        self.repair_from_imported(&imported);
        let mut names: HashSet<String> = self
            .hosts
            .iter()
            .map(|host| host.name.to_ascii_lowercase())
            .collect();
        let mut connections: HashSet<_> = self
            .hosts
            .iter()
            .filter_map(SshHost::connection_key)
            .collect();
        let mut count = 0;
        for host in imported {
            let name = host.name.to_ascii_lowercase();
            if names.contains(&name)
                || host
                    .connection_key()
                    .is_some_and(|key| connections.contains(&key))
            {
                continue;
            }
            names.insert(name);
            if let Some(key) = host.connection_key() {
                connections.insert(key);
            }
            self.hosts.push(host);
            count += 1;
        }
        self.normalize_groups();
        count
    }

    fn repair_from_imported(&mut self, imported: &[SshHost]) -> bool {
        let protected: HashSet<String> = self
            .hosts
            .iter()
            .filter(|host| host.address.is_empty())
            .map(|host| host.name.to_ascii_lowercase())
            .collect();
        if protected.is_empty() {
            return false;
        }

        let mut changed = false;
        for host in &mut self.hosts {
            if protected.contains(&host.name.to_ascii_lowercase())
                && let Some(current) = imported
                    .iter()
                    .find(|current| current.name.eq_ignore_ascii_case(&host.name))
            {
                host.address.clone_from(&current.address);
                host.user.clone_from(&current.user);
                host.port = current.port;
                host.identity_file.clone_from(&current.identity_file);
                host.alias.clone_from(&current.alias);
                changed = true;
            }
        }

        let before = self.hosts.len();
        let mut connections: HashSet<_> = self
            .hosts
            .iter()
            .filter(|host| protected.contains(&host.name.to_ascii_lowercase()))
            .filter_map(SshHost::connection_key)
            .collect();
        self.hosts.retain(|host| {
            protected.contains(&host.name.to_ascii_lowercase())
                || host
                    .connection_key()
                    .is_none_or(|key| connections.insert(key))
        });
        changed || self.hosts.len() != before
    }

    pub fn add_group(&mut self, group: impl Into<String>) {
        let group = group.into();
        if !self.groups.iter().any(|existing| existing == &group) {
            self.groups.push(group);
        }
    }

    pub fn reorder_group(&mut self, group: &str, target: &str, after: bool) -> bool {
        if group == target {
            return false;
        }
        let Some(from) = self.groups.iter().position(|item| item == group) else {
            return false;
        };
        let group = self.groups.remove(from);
        let Some(mut to) = self.groups.iter().position(|item| item == target) else {
            self.groups.insert(from, group);
            return false;
        };
        if after {
            to += 1;
        }
        self.groups.insert(to, group);
        true
    }

    pub fn move_hosts(
        &mut self,
        names: &[String],
        group: &str,
        target: Option<(&str, bool)>,
    ) -> bool {
        let names: HashSet<&str> = names.iter().map(String::as_str).collect();
        if names.is_empty()
            || target.is_some_and(|(name, _)| names.contains(name))
            || target.is_some_and(|(name, _)| !self.hosts.iter().any(|host| host.name == name))
        {
            return false;
        }

        let mut moved = Vec::new();
        let mut remaining = Vec::with_capacity(self.hosts.len());
        for mut host in std::mem::take(&mut self.hosts) {
            if names.contains(host.name.as_str()) {
                host.group = group.to_owned();
                moved.push(host);
            } else {
                remaining.push(host);
            }
        }
        if moved.is_empty() {
            self.hosts = remaining;
            return false;
        }

        let insertion = if let Some((target, after)) = target {
            remaining
                .iter()
                .position(|host| host.name == target)
                .map(|index| index + usize::from(after))
                .unwrap_or(remaining.len())
        } else {
            remaining
                .iter()
                .rposition(|host| host.group == group)
                .map_or(remaining.len(), |index| index + 1)
        };
        remaining.splice(insertion..insertion, moved);
        self.hosts = remaining;
        true
    }

    fn normalize_groups(&mut self) {
        let host_groups: Vec<_> = self.hosts.iter().map(|host| host.group.clone()).collect();
        for group in host_groups {
            self.add_group(group);
        }
        if self.groups.is_empty() {
            self.groups.push(default_group());
        }
    }
}

pub fn import_ssh_config() -> Vec<SshHost> {
    let mut aliases = Vec::new();
    collect_aliases(&expand_home("~/.ssh/config"), 0, &mut aliases);
    aliases.sort_by_key(|alias| alias.to_lowercase());
    aliases.dedup();
    aliases
        .into_iter()
        .map(|alias| resolve_alias(&alias))
        .collect()
}

/// Reads `Host` aliases from one SSH config file, following `Include` lines.
fn collect_aliases(path: &Path, depth: u8, aliases: &mut Vec<String>) {
    if depth > 8 {
        return;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let separator = |character: char| character.is_whitespace() || character == '=';
    for raw_line in text.lines() {
        let Some((key, value)) = raw_line.trim().split_once(separator) else {
            continue;
        };
        let values = value
            .trim_start_matches(separator)
            .split_whitespace()
            .map(|value| value.trim_matches('"'));
        if key.eq_ignore_ascii_case("host") {
            aliases.extend(
                values
                    .filter(|alias| {
                        !alias.is_empty()
                            && !alias.starts_with('-')
                            && !alias.contains(['*', '?', '!'])
                    })
                    .map(str::to_owned),
            );
        } else if key.eq_ignore_ascii_case("include") {
            for pattern in values {
                for file in expand_include(pattern) {
                    collect_aliases(&file, depth + 1, aliases);
                }
            }
        }
    }
}

/// Resolves an `Include` path; wildcards are supported in the file name only.
fn expand_include(pattern: &str) -> Vec<PathBuf> {
    let mut path = expand_home(pattern);
    if path.is_relative() {
        path = expand_home("~/.ssh").join(path);
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    if !name.contains(['*', '?']) {
        return vec![path];
    }
    let pattern: Vec<char> = name.chars().collect();
    let Some(Ok(entries)) = path.parent().map(fs::read_dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|file| {
            file.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| wildcard_match(&pattern, &name.chars().collect::<Vec<_>>()))
        })
        .collect();
    files.sort();
    files
}

fn wildcard_match(pattern: &[char], text: &[char]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some(('*', rest)) => (0..=text.len()).any(|skip| wildcard_match(rest, &text[skip..])),
        Some(('?', rest)) => !text.is_empty() && wildcard_match(rest, &text[1..]),
        Some((character, rest)) => {
            text.first() == Some(character) && wildcard_match(rest, &text[1..])
        }
    }
}

pub fn resolve_ssh_alias(alias: &str) -> Result<SshHost, String> {
    let mut host = SshHost {
        name: alias.into(),
        alias: alias.into(),
        port: 22,
        group: default_group(),
        ..Default::default()
    };

    let mut command = Command::new("/usr/bin/ssh");
    if let Some(config) = env::var_os("SHELLHOPPER_SSH_CONFIG") {
        command.args(["-F", &PathBuf::from(config).display().to_string()]);
    }
    let output = command
        .args(["-G", "--", alias])
        .output()
        .map_err(|error| format!("Could not check SSH alias {alias}: {error}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Could not resolve SSH alias {alias}: {}",
            error.trim()
        ));
    }
    // The address, user and port are kept for display and duplicate detection;
    // the connection itself goes through the alias.
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        match key {
            "hostname" => host.address = value.into(),
            "user" => host.user = value.into(),
            "port" => host.port = value.parse().unwrap_or(22),
            _ => {}
        }
    }
    if host.address.is_empty() {
        return Err(format!("SSH alias {alias} did not resolve to a host."));
    }
    Ok(host)
}

fn resolve_alias(alias: &str) -> SshHost {
    resolve_ssh_alias(alias).unwrap_or_else(|_| SshHost {
        name: alias.into(),
        alias: alias.into(),
        port: 22,
        group: default_group(),
        ..Default::default()
    })
}

fn connection_label(host: &SshHost) -> String {
    format!("{}:{}", host.target(), host.port)
}

fn config_path() -> Result<PathBuf, String> {
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("ShellHopper")
        .join("config.toml"))
}

fn legacy_config_path() -> Result<PathBuf, String> {
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("Termislop")
        .join("config.toml"))
}

fn expand_home(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    let Some(text) = path.to_str() else {
        return path.to_owned();
    };
    if text == "~" || text.starts_with("~/") {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join(text.trim_start_matches("~/"));
        }
    }
    path.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, address: &str, group: &str) -> SshHost {
        SshHost {
            name: name.into(),
            address: address.into(),
            group: group.into(),
            port: 22,
            ..Default::default()
        }
    }

    #[test]
    fn reload_preserves_group_and_skips_aliases_for_the_same_connection() {
        let mut config = AppConfig {
            hosts: vec![host("rhc-dev-web-1", "", "Dev"), host("dev-web", "", "Dev")],
            groups: vec!["Dev".into()],
            ..Default::default()
        };

        let added = config.merge_imported(vec![
            host("rhc-dev-web-1", "10.0.0.1", "SSH Config"),
            host("dev-web", "10.0.0.1", "SSH Config"),
            host("d-web", "10.0.0.1", "SSH Config"),
            host("prod-web-1", "10.0.0.2", "SSH Config"),
        ]);

        assert_eq!(added, 1);
        assert_eq!(config.hosts.len(), 3);
        assert_eq!(config.hosts[0].group, "Dev");
        assert_eq!(config.hosts[0].address, "10.0.0.1");
        assert_eq!(config.hosts[1].name, "dev-web");
    }

    #[test]
    fn reload_never_changes_how_an_existing_host_connects() {
        let mut config = AppConfig {
            hosts: vec![SshHost {
                identity_file: "~/.ssh/manual_key".into(),
                ..host("bastioned", "10.0.0.9", "Prod")
            }],
            ..Default::default()
        };
        let before = config.hosts[0].ssh_args(false);
        let alias = |name: &str, address: &str| SshHost {
            alias: name.into(),
            ..host(name, address, "SSH Config")
        };

        let added = config.apply_import(
            ImportKind::Reload,
            vec![
                alias("bastioned", "10.0.0.9"),
                alias("new-host", "10.0.0.10"),
            ],
        );

        assert_eq!(added, 1);
        assert_eq!(config.hosts[0].ssh_args(false), before);
        assert_eq!(config.hosts[1].ssh_args(false), ["--", "new-host"]);
        assert_eq!(
            host("manual", "-oProxyCommand=x", "Prod").ssh_args(false),
            ["--", "-oProxyCommand=x"]
        );
    }

    #[test]
    fn ssh_alias_requires_the_same_destination_and_preserves_sidebar_fields() {
        let mut config = AppConfig {
            hosts: vec![SshHost {
                user: "ubuntu".into(),
                identity_file: "~/.ssh/manual_key".into(),
                ..host("stage-web", "10.0.0.9", "Stage")
            }],
            ..Default::default()
        };
        let resolved = SshHost {
            name: "stage-web".into(),
            alias: "stage-web".into(),
            address: "10.0.0.9".into(),
            user: "ubuntu".into(),
            port: 22,
            ..Default::default()
        };

        let linked = config.use_ssh_alias("stage-web", &resolved).unwrap();

        assert_eq!(linked.alias, "stage-web");
        assert_eq!(linked.group, "Stage");
        assert_eq!(linked.identity_file, "~/.ssh/manual_key");

        let mut wrong = resolved;
        wrong.address = "10.0.0.10".into();
        assert!(config.use_ssh_alias("stage-web", &wrong).is_err());
        assert_eq!(config.hosts[0].address, "10.0.0.9");
    }

    #[test]
    fn include_wildcards_match_file_names() {
        let matches = |pattern: &str, name: &str| {
            wildcard_match(
                &pattern.chars().collect::<Vec<_>>(),
                &name.chars().collect::<Vec<_>>(),
            )
        };
        assert!(matches("*.conf", "work.conf"));
        assert!(matches("host-?", "host-1"));
        assert!(!matches("*.conf", "work.conf.bak"));
    }

    #[test]
    fn headers_can_be_reordered_before_or_after_another_header() {
        let mut config = AppConfig {
            groups: vec!["Development".into(), "Stage".into(), "Prod".into()],
            ..Default::default()
        };

        assert!(config.reorder_group("Development", "Prod", true));
        assert_eq!(config.groups, ["Stage", "Prod", "Development"]);
        assert!(config.reorder_group("Development", "Stage", false));
        assert_eq!(config.groups, ["Development", "Stage", "Prod"]);
    }

    #[test]
    fn servers_keep_their_order_when_moved_between_headers() {
        let mut config = AppConfig {
            hosts: vec![
                host("dev-1", "10.0.0.1", "Dev"),
                host("dev-2", "10.0.0.2", "Dev"),
                host("prod-1", "10.0.1.1", "Prod"),
                host("prod-2", "10.0.1.2", "Prod"),
            ],
            ..Default::default()
        };

        assert!(config.move_hosts(
            &["dev-1".into(), "dev-2".into()],
            "Prod",
            Some(("prod-2", false)),
        ));
        assert_eq!(
            config
                .hosts
                .iter()
                .map(|host| (host.name.as_str(), host.group.as_str()))
                .collect::<Vec<_>>(),
            [
                ("prod-1", "Prod"),
                ("dev-1", "Prod"),
                ("dev-2", "Prod"),
                ("prod-2", "Prod"),
            ]
        );
    }

    #[test]
    fn tab_layout_round_trips_without_terminal_state() {
        let config = AppConfig {
            open_tabs: Some(vec![
                SavedTab::Local,
                SavedTab::Split {
                    direction: SavedSplitDirection::Right,
                    ratio: 500,
                    first: Box::new(SavedTab::Ssh {
                        host: "stage-web".into(),
                    }),
                    second: Box::new(SavedTab::Local),
                },
            ]),
            active_tab: Some(1),
            collapsed_groups: BTreeSet::from(["Stage".into(), "Prod".into()]),
            host_separators: BTreeSet::from(["stage-web".into()]),
            ..Default::default()
        };

        let text = toml::to_string(&config).unwrap();
        let restored: AppConfig = toml::from_str(&text).unwrap();

        assert_eq!(restored.open_tabs, config.open_tabs);
        assert_eq!(restored.active_tab, Some(1));
        assert_eq!(restored.collapsed_groups, config.collapsed_groups);
        assert_eq!(restored.host_separators, config.host_separators);
        assert!(!text.contains("command"));
        assert!(!text.contains("output"));
    }

    #[test]
    fn presentation_settings_are_saved_only_when_changed() {
        let mut config = AppConfig {
            hosts: vec![SshHost::default()],
            ..Default::default()
        };
        assert!(!toml::to_string(&config).unwrap().contains("presentation"));

        config.presentation.text_color = [0, 255, 0];
        config.presentation.fill_window = false;
        let text = toml::to_string(&config).unwrap();
        let restored: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(restored.presentation, config.presentation);

        let partial: AppConfig = toml::from_str("[presentation]\npadding = 20\n").unwrap();
        assert_eq!(partial.presentation.padding, 20);
        assert!(partial.presentation.show_typing);
    }
}
