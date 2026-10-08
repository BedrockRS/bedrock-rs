//! Plugin folders and their `plugin.json` manifests.
//!
//! Each plugin lives in its own folder in the plugin directory:
//!
//! ```text
//! plugins/
//!   hello/
//!     plugin.json
//!     main.luau          (or index.js)
//! ```
//!
//! What runs a plugin follows from its `main`: Luau for a `.luau` script,
//! JavaScript for a `.js` or `.mjs` module. Both engines are built into the
//! server.
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

/// What runs a plugin, by the extension of its `main`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// A `.luau` script.
    Luau,
    /// A `.js` or `.mjs` ES module.
    JavaScript,
}

impl Engine {
    fn of(main: &Path) -> Option<Self> {
        let extension = main.extension()?.to_str()?.to_ascii_lowercase();
        Some(match extension.as_str() {
            "luau" => Self::Luau,
            "js" | "mjs" => Self::JavaScript,
            _ => return None,
        })
    }

    /// Whether a file in the plugin's folder is part of the plugin, so a
    /// change to it reloads it.
    fn is_source(self, path: &Path) -> bool {
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            return false;
        };
        let extension = extension.to_ascii_lowercase();
        match self {
            Self::Luau => matches!(extension.as_str(), "luau" | "lua"),
            Self::JavaScript => matches!(extension.as_str(), "js" | "mjs" | "json"),
        }
    }
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

/// A plugin folder read from disk: its manifest, entry file, and the other
/// files it is made of, so a change to any of them reloads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSource {
    pub manifest: Manifest,
    pub engine: Engine,
    /// Where the entry file is.
    pub main_path: PathBuf,
    /// The entry file's contents.
    pub source: Vec<u8>,
    /// The plugin's files in the folder and below, by path, with their
    /// contents: Luau scripts; JavaScript, TypeScript and JSON files; or the
    /// WebAssembly module. Symbolic links, hidden folders (such as the
    /// build's `.bedrockrs`) and `node_modules` are skipped.
    pub files: Vec<(PathBuf, Vec<u8>)>,
}

/// Most files a plugin folder may hold, so a stray huge folder cannot make
/// every rescan read it all.
const MAX_FILES: usize = 1024;

/// Reads the plugin's files in `folder` and below into `files`.
fn read_files(
    folder: &Path,
    engine: Engine,
    files: &mut Vec<(PathBuf, Vec<u8>)>,
) -> Result<(), ManifestError> {
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
        let name = entry.file_name();
        let hidden = name.to_string_lossy().starts_with('.');
        if kind.is_dir() {
            if !hidden && name != "node_modules" {
                read_files(&path, engine, files)?;
            }
        } else if kind.is_file() && engine.is_source(&path) {
            if files.len() == MAX_FILES {
                return Err(ManifestError::Invalid(format!(
                    "the plugin folder holds more than {MAX_FILES} files"
                )));
            }
            let contents = fs::read(&path).map_err(|source| read_error(&path, source))?;
            files.push((path, contents));
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

    /// What runs the plugin.
    pub fn engine(&self) -> Engine {
        Engine::of(Path::new(&self.main)).expect("validated when parsed")
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
        if Engine::of(main).is_none() {
            let typescript = main.extension().is_some_and(|extension| {
                ["ts", "mts", "cts", "tsx"]
                    .iter()
                    .any(|typescript| extension.eq_ignore_ascii_case(typescript))
            });
            let hint = if typescript {
                "; TypeScript runs once compiled to JavaScript"
            } else {
                ""
            };
            return invalid(format!(
                "\"main\" {:?} must be a .luau script or a .js module{hint}",
                self.main
            ));
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
        let engine = manifest.engine();
        let main_path = folder.join(&manifest.main);
        let source = fs::read(&main_path).map_err(|source| ManifestError::Read {
            path: main_path.clone(),
            source,
        })?;
        let mut files = Vec::new();
        read_files(folder, engine, &mut files)?;
        files.sort();
        Ok(Some(Self {
            manifest,
            engine,
            main_path,
            source,
            files,
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
        for (main, engine) in [
            ("main.luau", Engine::Luau),
            ("index.js", Engine::JavaScript),
            ("src/plugin.mjs", Engine::JavaScript),
        ] {
            let manifest = Manifest::parse(&manifest_json("hello", main)).unwrap();
            assert_eq!(manifest.engine(), engine, "{main}");
        }
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
            ("hello", "main.py"),
            ("hello", "index.ts"),
            ("hello", "plugin.wasm"),
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
        assert_eq!(plugin.source, b"print('hi')");
        fs::remove_dir_all(&folder).unwrap();
    }
}
