// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Plugin discovery and lifecycle management for Arte Ogre.
//!
//! [`PluginManager`] scans a directory of plugin bundles, each of which is an
//! immediate child directory containing a `plugin.toml` manifest. Invalid
//! plugins are retained in the list so the UI can surface errors instead of
//! silently dropping them.

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// Maximum size of a `plugin.toml` manifest. Bounds a hostile/oversized (or
/// symlink-to-`/dev/zero`) manifest from exhausting memory during discovery.
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// Read a file as UTF-8, failing if it exceeds `max` bytes.
fn read_capped(path: &Path, max: u64) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut buf = String::new();
    file.by_ref().take(max).read_to_string(&mut buf)?;
    // Anything past the cap means the file is too large to trust.
    let mut overflow = [0u8; 1];
    if file.read(&mut overflow)? != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "plugin.toml exceeds the maximum manifest size",
        ));
    }
    Ok(buf)
}

/// True if `entry` is a plain relative path that stays inside the plugin dir
/// (no absolute paths, no `..` traversal, no drive/root prefixes).
fn entry_is_contained(entry: &str) -> bool {
    let p = Path::new(entry);
    !p.is_absolute()
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Kind of plugin runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginKind {
    /// WebAssembly tile-filter plugin.
    Wasm,
    /// Lua macro/script plugin.
    Lua,
}

/// Discovered plugin, valid or invalid.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginInfo {
    /// Display name from the manifest.
    pub name: String,
    /// Version string from the manifest.
    pub version: String,
    /// Plugin runtime kind.
    pub kind: PluginKind,
    /// Directory containing the plugin.
    pub dir: PathBuf,
    /// Path to the entry file (relative to `dir`, resolved absolutely).
    pub entry: PathBuf,
    /// Whether the plugin is currently enabled.
    pub enabled: bool,
    /// Whether the manifest parsed and the entry file exists.
    pub valid: bool,
    /// Error message if the plugin is invalid.
    pub error: Option<String>,
}

impl PluginInfo {
    /// Create an invalid placeholder for a plugin directory.
    fn invalid(dir: PathBuf, name: String, error: impl Into<String>) -> Self {
        Self {
            name,
            version: String::new(),
            kind: PluginKind::Lua, // arbitrary; invalid anyway
            dir,
            entry: PathBuf::new(),
            enabled: false,
            valid: false,
            error: Some(error.into()),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Manifest {
    name: Option<String>,
    version: Option<String>,
    kind: Option<String>,
    entry: Option<String>,
}

/// Manager that discovers and tracks plugins in a directory.
#[derive(Clone, Debug, Default)]
pub struct PluginManager {
    plugins: Vec<PluginInfo>,
    plugins_dir: PathBuf,
}

impl PluginManager {
    /// Create a manager and immediately discover plugins in `plugins_dir`.
    pub fn new<P: AsRef<Path>>(plugins_dir: P) -> Self {
        let plugins_dir = plugins_dir.as_ref().to_path_buf();
        let mut manager = Self {
            plugins: Vec::new(),
            plugins_dir,
        };
        manager.discover();
        manager
    }

    /// Rescan `plugins_dir`. Invalid plugins are kept in the list with
    /// `valid == false` and an error message; discovery never panics.
    pub fn discover(&mut self) {
        self.plugins.clear();

        let entries = match fs::read_dir(&self.plugins_dir) {
            Ok(entries) => entries,
            Err(e) => {
                // The plugins directory itself is unreadable; record one
                // synthetic invalid plugin describing the problem.
                self.plugins.push(PluginInfo::invalid(
                    self.plugins_dir.clone(),
                    String::from("."),
                    format!("could not read plugins directory: {e}"),
                ));
                return;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let dir_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            let manifest_path = path.join("plugin.toml");
            let raw = match read_capped(&manifest_path, MAX_MANIFEST_BYTES) {
                Ok(raw) => raw,
                Err(e) => {
                    self.plugins.push(PluginInfo::invalid(
                        path,
                        dir_name,
                        format!("could not read plugin.toml: {e}"),
                    ));
                    continue;
                }
            };

            let manifest: Manifest = match toml::from_str(&raw) {
                Ok(m) => m,
                Err(e) => {
                    self.plugins.push(PluginInfo::invalid(
                        path,
                        dir_name,
                        format!("could not parse plugin.toml: {e}"),
                    ));
                    continue;
                }
            };

            let name = manifest.name.unwrap_or(dir_name);
            let version = manifest.version.unwrap_or_default();
            let kind_str = manifest.kind.unwrap_or_default();
            let entry_name = manifest.entry.unwrap_or_default();

            let kind = match kind_str.as_str() {
                "wasm" => PluginKind::Wasm,
                "lua" => PluginKind::Lua,
                _ => {
                    self.plugins.push(PluginInfo::invalid(
                        path,
                        name,
                        format!("invalid plugin kind `{kind_str}`; expected `wasm` or `lua`"),
                    ));
                    continue;
                }
            };

            if entry_name.is_empty() {
                self.plugins.push(PluginInfo::invalid(
                    path.clone(),
                    name,
                    "missing required field `entry`",
                ));
                continue;
            }

            if !entry_is_contained(&entry_name) {
                self.plugins.push(PluginInfo::invalid(
                    path.clone(),
                    name,
                    format!(
                        "entry path `{entry_name}` must be a relative path inside the plugin directory"
                    ),
                ));
                continue;
            }

            let entry_path = path.join(&entry_name);
            if !entry_path.exists() {
                self.plugins.push(PluginInfo::invalid(
                    path,
                    name,
                    format!("entry file `{}` does not exist", entry_name),
                ));
                continue;
            }

            self.plugins.push(PluginInfo {
                name,
                version,
                kind,
                dir: path.clone(),
                entry: entry_path,
                enabled: true,
                valid: true,
                error: None,
            });
        }

        // Keep the list stable across rescans so indices are predictable.
        self.plugins
            .sort_by(|a, b| a.dir.as_path().cmp(b.dir.as_path()));
    }

    /// All discovered plugins.
    pub fn plugins(&self) -> &[PluginInfo] {
        &self.plugins
    }

    /// Only valid plugins.
    pub fn valid_plugins(&self) -> Vec<&PluginInfo> {
        self.plugins.iter().filter(|p| p.valid).collect()
    }

    /// Valid plugins that are enabled.
    pub fn enabled_plugins(&self) -> Vec<&PluginInfo> {
        self.plugins
            .iter()
            .filter(|p| p.valid && p.enabled)
            .collect()
    }

    /// Enable or disable a plugin by name. Returns `true` if a matching valid
    /// plugin was found.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        let mut found = false;
        for plugin in self.plugins.iter_mut() {
            if plugin.valid && plugin.name == name {
                plugin.enabled = enabled;
                found = true;
            }
        }
        found
    }

    /// Enable or disable a plugin by its (unique) directory. Returns `true` if
    /// a matching valid plugin was found.
    ///
    /// Prefer this over [`Self::set_enabled`]: plugin display names come from
    /// untrusted manifests and are not unique, so name-based toggling can flip
    /// the wrong plugin (or several at once).
    pub fn set_enabled_dir(&mut self, dir: &Path, enabled: bool) -> bool {
        for plugin in self.plugins.iter_mut() {
            if plugin.valid && plugin.dir == dir {
                plugin.enabled = enabled;
                return true;
            }
        }
        false
    }

    /// The directory being scanned.
    pub fn plugins_dir(&self) -> &Path {
        &self.plugins_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_temp_plugins_dir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("ogre-plugins-test-{}", uuid()));
        fs::create_dir_all(&base).expect("create temp dir");
        base
    }

    fn uuid() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{nanos}")
    }

    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    fn write_file(path: &Path, content: &str) {
        let mut f = fs::File::create(path).expect("create file");
        f.write_all(content.as_bytes()).expect("write file");
    }

    #[test]
    fn discover_lists_valid_plugins() {
        let dir = make_temp_plugins_dir();
        let _guard = CleanupOnDrop(&dir);

        let plugin_dir = dir.join("cut_selection_lua");
        fs::create_dir(&plugin_dir).unwrap();
        write_file(
            &plugin_dir.join("plugin.toml"),
            r#"
name = "Cut Selection"
version = "1.0.0"
kind = "lua"
entry = "cut_selection.lua"
"#,
        );
        write_file(&plugin_dir.join("cut_selection.lua"), "-- noop\n");

        let manager = PluginManager::new(&dir);
        assert_eq!(manager.plugins().len(), 1);

        let plugin = &manager.plugins()[0];
        assert!(plugin.valid);
        assert!(plugin.enabled);
        assert_eq!(plugin.name, "Cut Selection");
        assert_eq!(plugin.version, "1.0.0");
        assert_eq!(plugin.kind, PluginKind::Lua);
        assert_eq!(plugin.entry, plugin_dir.join("cut_selection.lua"));
        assert_eq!(plugin.error, None);
    }

    #[test]
    fn discover_marks_missing_manifest_invalid() {
        let dir = make_temp_plugins_dir();
        let _guard = CleanupOnDrop(&dir);

        let plugin_dir = dir.join("no_manifest");
        fs::create_dir(&plugin_dir).unwrap();

        let manager = PluginManager::new(&dir);
        assert_eq!(manager.plugins().len(), 1);

        let plugin = &manager.plugins()[0];
        assert!(!plugin.valid);
        assert!(!plugin.enabled);
        assert_eq!(plugin.name, "no_manifest");
        assert!(plugin.error.as_ref().unwrap().contains("plugin.toml"));
    }

    #[test]
    fn discover_marks_missing_entry_invalid() {
        let dir = make_temp_plugins_dir();
        let _guard = CleanupOnDrop(&dir);

        let plugin_dir = dir.join("missing_entry");
        fs::create_dir(&plugin_dir).unwrap();
        write_file(
            &plugin_dir.join("plugin.toml"),
            r#"
name = "Missing Entry"
version = "1.0.0"
kind = "lua"
entry = "ghost.lua"
"#,
        );

        let manager = PluginManager::new(&dir);
        assert_eq!(manager.plugins().len(), 1);

        let plugin = &manager.plugins()[0];
        assert!(!plugin.valid);
        assert!(plugin.error.as_ref().unwrap().contains("ghost.lua"));
    }

    #[test]
    fn discover_rejects_malformed_kind() {
        let dir = make_temp_plugins_dir();
        let _guard = CleanupOnDrop(&dir);

        let plugin_dir = dir.join("bad_kind");
        fs::create_dir(&plugin_dir).unwrap();
        write_file(
            &plugin_dir.join("plugin.toml"),
            r#"
name = "Bad Kind"
version = "1.0.0"
kind = "python"
entry = "script.py"
"#,
        );
        write_file(&plugin_dir.join("script.py"), "# noop\n");

        let manager = PluginManager::new(&dir);
        assert_eq!(manager.plugins().len(), 1);

        let plugin = &manager.plugins()[0];
        assert!(!plugin.valid);
        assert!(plugin.error.as_ref().unwrap().contains("python"));
    }

    #[test]
    fn enable_and_disable() {
        let dir = make_temp_plugins_dir();
        let _guard = CleanupOnDrop(&dir);

        let plugin_dir = dir.join("toggle");
        fs::create_dir(&plugin_dir).unwrap();
        write_file(
            &plugin_dir.join("plugin.toml"),
            r#"
name = "Toggle Me"
version = "1.0.0"
kind = "lua"
entry = "toggle.lua"
"#,
        );
        write_file(&plugin_dir.join("toggle.lua"), "-- noop\n");

        let mut manager = PluginManager::new(&dir);
        assert_eq!(manager.enabled_plugins().len(), 1);

        assert!(manager.set_enabled("Toggle Me", false));
        assert!(manager.enabled_plugins().is_empty());
        assert!(!manager.plugins()[0].enabled);

        assert!(manager.set_enabled("Toggle Me", true));
        assert_eq!(manager.enabled_plugins().len(), 1);
        assert!(manager.plugins()[0].enabled);

        // Unknown plugins are not found.
        assert!(!manager.set_enabled("No Such Plugin", true));
    }

    #[test]
    fn discover_does_not_panic_on_empty_dir() {
        let dir = make_temp_plugins_dir();
        let _guard = CleanupOnDrop(&dir);

        let manager = PluginManager::new(&dir);
        assert!(manager.plugins().is_empty());
        assert!(manager.valid_plugins().is_empty());
        assert!(manager.enabled_plugins().is_empty());
    }

    #[test]
    fn discover_rejects_path_traversal_entry() {
        let dir = make_temp_plugins_dir();
        let _guard = CleanupOnDrop(&dir);

        let plugin_dir = dir.join("evil");
        fs::create_dir(&plugin_dir).unwrap();
        write_file(
            &plugin_dir.join("plugin.toml"),
            r#"
name = "Evil"
version = "1.0.0"
kind = "lua"
entry = "../../../../../../etc/hosts"
"#,
        );

        let manager = PluginManager::new(&dir);
        assert_eq!(manager.plugins().len(), 1);
        let plugin = &manager.plugins()[0];
        assert!(
            !plugin.valid,
            "an entry escaping the plugin directory must be rejected"
        );
        let err = plugin.error.as_ref().unwrap().to_lowercase();
        assert!(
            err.contains("entry") || err.contains("path") || err.contains("outside"),
            "error should explain the rejected entry path, got: {err}"
        );
    }

    #[test]
    fn set_enabled_dir_toggles_only_the_matching_plugin() {
        let dir = make_temp_plugins_dir();
        let _guard = CleanupOnDrop(&dir);

        // Two distinct plugins that share the same display name.
        for sub in ["a", "b"] {
            let pdir = dir.join(sub);
            fs::create_dir(&pdir).unwrap();
            write_file(
                &pdir.join("plugin.toml"),
                "name = \"Dup\"\nversion = \"1.0.0\"\nkind = \"lua\"\nentry = \"m.lua\"\n",
            );
            write_file(&pdir.join("m.lua"), "-- noop\n");
        }

        let mut manager = PluginManager::new(&dir);
        assert_eq!(manager.enabled_plugins().len(), 2);

        let first_dir = manager.plugins()[0].dir.clone();
        assert!(manager.set_enabled_dir(&first_dir, false));

        // Exactly one plugin (the one in `first_dir`) is now disabled.
        assert_eq!(manager.enabled_plugins().len(), 1);
        let disabled: Vec<_> = manager.plugins().iter().filter(|p| !p.enabled).collect();
        assert_eq!(disabled.len(), 1);
        assert_eq!(disabled[0].dir, first_dir);
    }

    struct CleanupOnDrop<'a>(&'a Path);

    impl<'a> Drop for CleanupOnDrop<'a> {
        fn drop(&mut self) {
            cleanup(self.0);
        }
    }
}
