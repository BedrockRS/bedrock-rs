//! Plugin folders and their `plugin.json` manifests.
//!
//! Each plugin lives in its own folder in the plugin directory:
//!
//! ```text
//! plugins/
//!   hello/
//!     plugin.json
//!     main.luau
//! ```
//!
//! ```json
//! {
//!   "name": "hello",
//!   "description": "Welcomes players",
//!   "version": "1.0.0",
//!   "author": "Mistvale Studios",
//!   "main": "main.luau"
//! }
//! ```

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// The manifest file every plugin folder holds.
pub const MANIFEST_FILE: &str = "plugin.json";
/// Longest plugin name allowed.
const MAX_NAME_LEN: usize = 64;

/// What a plugin says about itself in its `plugin.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Unique among the server's plugins: letters, digits, `-` and `_`.
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: String,
    /// The script the plugin starts from, relative to its folder.
    pub main: String,
}

/// Why a plugin folder could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("failed to read {}: {source}", .path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("invalid {MANIFEST_FILE}: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid {MANIFEST_FILE}: {0}")]
    Invalid(String),
}

/// A plugin folder read from disk: its manifest, entry script, and the other
/// scripts it may `require`, so a change to any of them reloads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSource {
    pub manifest: Manifest,
    /// Where the entry script is, for error messages.
    pub main_path: PathBuf,
    pub source: String,
    /// Every `.luau` and `.lua` file in the folder and below, by path, with
    /// its contents. Symbolic links are skipped.
    pub modules: Vec<(PathBuf, String)>,
}

/// Most scripts a plugin folder may hold, so a stray huge folder cannot make
/// every rescan read it all.
const MAX_SCRIPTS: usize = 1024;

/// Reads the scripts in `folder` and below into `scripts`.
fn read_scripts(folder: &Path, scripts: &mut Vec<(PathBuf, String)>) -> Result<(), ManifestError> {
    let read_error = |path: &Path, source| ManifestError::Read {
        path: path.to_owned(),
        source,
    };
    let entries = fs::read_dir(folder).map_err(|source| read_error(folder, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| read_error(folder, source))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|source| read_error(&path, source))?;
        if kind.is_dir() {
            read_scripts(&path, scripts)?;
        } else if kind.is_file()
            && path.extension().is_some_and(|extension| {
                extension.eq_ignore_ascii_case("luau") || extension.eq_ignore_ascii_case("lua")
            })
        {
            if scripts.len() == MAX_SCRIPTS {
                return Err(ManifestError::Invalid(format!(
                    "the plugin folder holds more than {MAX_SCRIPTS} scripts"
                )));
            }
            let source = fs::read_to_string(&path).map_err(|source| read_error(&path, source))?;
            scripts.push((path, source));
        }
    }
    Ok(())
}

impl Manifest {
    pub fn parse(json: &str) -> Result<Self, ManifestError> {
        let manifest: Self = serde_json::from_str(json)?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        let invalid = |message: String| Err(ManifestError::Invalid(message));
        if self.name.is_empty() || self.name.len() > MAX_NAME_LEN {
            return invalid(format!(
                "\"name\" must be 1 to {MAX_NAME_LEN} characters long"
            ));
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return invalid(format!(
                "\"name\" {:?} may only hold letters, digits, '-' and '_'",
                self.name
            ));
        }
        if self.version.trim().is_empty() {
            return invalid("\"version\" is empty".into());
        }
        let main = Path::new(&self.main);
        if !main
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("luau"))
        {
            return invalid(format!("\"main\" {:?} is not a .luau script", self.main));
        }
        // The entry script must be inside the plugin's folder.
        if !main
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
        {
            return invalid(format!(
                "\"main\" {:?} must be a path inside the plugin's folder",
                self.main
            ));
        }
        Ok(())
    }
}

impl PluginSource {
    /// Reads the plugin in `folder`. `Ok(None)` means the folder has no
    /// manifest, so it is not a plugin.
    pub fn read(folder: &Path) -> Result<Option<Self>, ManifestError> {
        let manifest_path = folder.join(MANIFEST_FILE);
        let json = match fs::read_to_string(&manifest_path) {
            Ok(json) => json,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(ManifestError::Read {
                    path: manifest_path,
                    source,
                });
            }
        };
        let manifest = Manifest::parse(&json)?;
        let main_path = folder.join(&manifest.main);
        let source = fs::read_to_string(&main_path).map_err(|source| ManifestError::Read {
            path: main_path.clone(),
            source,
        })?;
        let mut modules = Vec::new();
        read_scripts(folder, &mut modules)?;
        modules.sort();
        Ok(Some(Self {
            manifest,
            main_path,
            source,
            modules,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_json(name: &str, main: &str) -> String {
        serde_json::json!({
            "name": name,
            "description": "A test plugin",
            "version": "1.0.0",
            "author": "Tester",
            "main": main,
        })
        .to_string()
    }

    #[test]
    fn parses_a_complete_manifest() {
        let manifest = Manifest::parse(&manifest_json("hello", "main.luau")).unwrap();
        assert_eq!(
            manifest,
            Manifest {
                name: "hello".into(),
                description: "A test plugin".into(),
                version: "1.0.0".into(),
                author: "Tester".into(),
                main: "main.luau".into(),
            }
        );
        assert!(Manifest::parse(&manifest_json("hello", "src/main.luau")).is_ok());
    }

    #[test]
    fn every_field_is_required() {
        let err = Manifest::parse(r#"{"name":"hello","version":"1.0.0","main":"main.luau"}"#)
            .unwrap_err();
        assert!(err.to_string().contains("missing field"), "{err}");
    }

    #[test]
    fn rejects_bad_names_and_entry_scripts() {
        for (name, main) in [
            ("", "main.luau"),
            ("two words", "main.luau"),
            ("hello", "main.lua"),
            ("hello", "../other/main.luau"),
            ("hello", "/etc/main.luau"),
        ] {
            assert!(
                Manifest::parse(&manifest_json(name, main)).is_err(),
                "{name:?} {main:?}"
            );
        }
    }

    #[test]
    fn reads_a_plugin_folder() {
        let folder =
            std::env::temp_dir().join(format!("bedrockrs-manifest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        assert_eq!(
            PluginSource::read(&folder).unwrap(),
            None,
            "no manifest yet"
        );

        fs::write(
            folder.join(MANIFEST_FILE),
            manifest_json("hello", "main.luau"),
        )
        .unwrap();
        let err = PluginSource::read(&folder).unwrap_err();
        assert!(err.to_string().contains("main.luau"), "{err}");

        fs::write(folder.join("main.luau"), "print('hi')").unwrap();
        let plugin = PluginSource::read(&folder).unwrap().unwrap();
        assert_eq!(plugin.manifest.name, "hello");
        assert_eq!(plugin.source, "print('hi')");
        fs::remove_dir_all(&folder).unwrap();
    }
}
