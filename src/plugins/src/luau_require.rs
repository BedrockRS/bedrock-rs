//! `require` for Luau plugins, confined to the plugin's own folder.
//!
//! Plugins split themselves across files with Luau's require-by-string:
//! `require("./util")` loads `util.luau` (or `util.lua`) next to the script
//! that requires it, `require("./lib")` loads `lib/init.luau`, and `../`
//! goes up a folder, but never above the plugin's own. `@self/…` is relative
//! to the requiring module itself. Configuration files (`.luaurc`) and their
//! aliases are not read: they could point anywhere on disk.
//!
//! One alias is built in: `require("@bedrock-rs/core")` is the plugin API
//! (`Logger`, `Server`, `Player`, `World`), as JavaScript plugins import it
//! from `@bedrock-rs/core`.
//!
//! Files are checked by where they really are, after following symbolic
//! links, so a link inside the folder cannot reach outside it. A module runs
//! once per VM; requiring it again returns what it returned the first time.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use mlua::luau::{NavigateError, Require};
use mlua::{Function, Lua};

/// File extensions a module may have, in the order tried.
const EXTENSIONS: [&str; 2] = ["luau", "lua"];

/// The built-in alias, `@bedrock-rs`, and its one module.
const ALIAS: &str = "bedrock-rs";
const CORE_MODULE: &str = "core";

/// Where in the built-in alias the requirer points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Builtin {
    /// `@bedrock-rs` itself.
    Alias,
    /// `@bedrock-rs/core`.
    Core,
}

/// Finds and loads modules for one plugin.
#[derive(Debug)]
pub(crate) struct PluginRequirer {
    /// The plugin's folder, as its scripts' chunk names spell it.
    folder: PathBuf,
    /// The folder where it really is, to check files against.
    root: Option<PathBuf>,
    /// The current module, as names below the folder: `["lib", "init"]`.
    module: Vec<String>,
    /// The file the current module is, if it is one.
    file: Option<PathBuf>,
    /// Set while pointing inside `@bedrock-rs`, rather than the folder.
    builtin: Option<Builtin>,
}

impl PluginRequirer {
    /// A requirer for the plugin in `folder`.
    pub(crate) fn new(folder: &Path) -> Self {
        Self {
            folder: folder.to_owned(),
            root: fs::canonicalize(folder).ok(),
            module: Vec::new(),
            file: None,
            builtin: None,
        }
    }

    /// The chunk name of a script at `relative` below the folder, as errors
    /// and `require` itself see it.
    pub(crate) fn chunk_name(folder: &Path, relative: &Path) -> String {
        format!("@{}", folder.join(relative).display())
    }

    /// Points at the module `module`, finding the file it is, if any.
    fn go_to(&mut self, module: Vec<String>) -> Result<(), NavigateError> {
        let file = self.resolve(&module)?;
        self.module = module;
        self.file = file;
        self.builtin = None;
        Ok(())
    }

    /// The file module `module` is: `name.luau`, `name.lua`, or a folder's
    /// `init.luau`. `None` for a folder without one, which may still lead to
    /// modules inside it.
    fn resolve(&self, module: &[String]) -> Result<Option<PathBuf>, NavigateError> {
        let root = self.root.as_ref().ok_or(NavigateError::NotFound)?;
        let path = module
            .iter()
            .fold(root.clone(), |path, name| path.join(name));
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(name) = module.last() {
            candidates.extend(
                EXTENSIONS
                    .iter()
                    .map(|extension| path.with_file_name(format!("{name}.{extension}"))),
            );
        }
        let is_dir = path.is_dir();
        if is_dir {
            candidates.extend(
                EXTENSIONS
                    .iter()
                    .map(|extension| path.join(format!("init.{extension}"))),
            );
        }
        let mut found = candidates
            .into_iter()
            .filter(|candidate| candidate.is_file());
        let Some(file) = found.next() else {
            return if is_dir {
                Ok(None)
            } else {
                Err(NavigateError::NotFound)
            };
        };
        if found.next().is_some() {
            return Err(NavigateError::Ambiguous);
        }
        // Where the file really is, past any links, must be in the folder.
        let real = fs::canonicalize(&file).map_err(|_| NavigateError::NotFound)?;
        if !real.starts_with(root) {
            return Err(NavigateError::Other(mlua::Error::runtime(format!(
                "{} is outside the plugin's folder",
                self.relative_display(&module.join("/"))
            ))));
        }
        Ok(Some(real))
    }

    fn relative_display(&self, module: &str) -> String {
        self.folder.join(module).display().to_string()
    }
}

/// The chunk name without a `:line` suffix or the `@`, as a path.
fn chunk_path(chunk_name: &str) -> Option<&Path> {
    let name = chunk_name.strip_prefix('@')?;
    let name = match name.rsplit_once(':') {
        Some((path, line)) if line.parse::<u32>().is_ok() => path,
        _ => name,
    };
    Some(Path::new(name))
}

impl Require for PluginRequirer {
    fn is_require_allowed(&self, chunk_name: &str) -> bool {
        chunk_path(chunk_name).is_some_and(|path| path.starts_with(&self.folder))
    }

    /// Starts from the script that calls `require`.
    fn reset(&mut self, chunk_name: &str) -> Result<(), NavigateError> {
        let path = chunk_path(chunk_name).ok_or(NavigateError::NotFound)?;
        let relative = path
            .strip_prefix(&self.folder)
            .map_err(|_| NavigateError::NotFound)?;
        let mut module = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(name) => {
                    module.push(name.to_str().ok_or(NavigateError::NotFound)?.to_owned());
                }
                Component::CurDir => {}
                _ => return Err(NavigateError::NotFound),
            }
        }
        // The script `main.luau` is the module `main`, and a folder's
        // `init.luau` is the folder itself, as Luau has it: its `./` is next
        // to the folder, its `@self/` inside it.
        if let Some(last) = module.last_mut()
            && let Some((stem, extension)) = last.rsplit_once('.')
            && EXTENSIONS.contains(&extension)
        {
            *last = stem.to_owned();
        }
        if module.len() > 1 && module.last().is_some_and(|last| last == "init") {
            module.pop();
        }
        // The requiring script already runs; only where it is matters.
        self.file = self.resolve(&module).ok().flatten();
        self.module = module;
        self.builtin = None;
        Ok(())
    }

    fn jump_to_alias(&mut self, _path: &str) -> Result<(), NavigateError> {
        Err(NavigateError::NotFound)
    }

    /// `@bedrock-rs`, the built-in alias; others are unknown.
    fn to_alias_override(&mut self, alias: &str) -> Result<(), NavigateError> {
        if !alias.eq_ignore_ascii_case(ALIAS) {
            return Err(NavigateError::NotFound);
        }
        self.builtin = Some(Builtin::Alias);
        self.file = None;
        Ok(())
    }

    /// Up a folder, but not above the plugin's.
    fn to_parent(&mut self) -> Result<(), NavigateError> {
        if self.builtin.is_some() {
            return Err(NavigateError::NotFound);
        }
        let mut module = self.module.clone();
        if module.pop().is_none() {
            return Err(NavigateError::NotFound);
        }
        self.go_to(module)
    }

    fn to_child(&mut self, name: &str) -> Result<(), NavigateError> {
        match self.builtin {
            Some(Builtin::Alias) if name == CORE_MODULE => {
                self.builtin = Some(Builtin::Core);
                return Ok(());
            }
            Some(_) => return Err(NavigateError::NotFound),
            None => {}
        }
        let plain =
            !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':']);
        if !plain {
            return Err(NavigateError::NotFound);
        }
        let mut module = self.module.clone();
        module.push(name.to_owned());
        self.go_to(module)
    }

    fn has_module(&self) -> bool {
        self.builtin == Some(Builtin::Core) || self.file.is_some()
    }

    fn cache_key(&self) -> String {
        if self.builtin == Some(Builtin::Core) {
            return format!("@{ALIAS}/{CORE_MODULE}");
        }
        self.file
            .as_deref()
            .map(|file| file.display().to_string())
            .unwrap_or_default()
    }

    fn has_config(&self) -> bool {
        false
    }

    fn config(&self) -> io::Result<Vec<u8>> {
        Err(io::ErrorKind::NotFound.into())
    }

    fn loader(&self, lua: &Lua) -> mlua::Result<Function> {
        if self.builtin == Some(Builtin::Core) {
            let core: mlua::Table = lua.named_registry_value(crate::luau::CORE)?;
            return lua.create_function(move |_, ()| Ok(core.clone()));
        }
        let file = self
            .file
            .as_deref()
            .ok_or_else(|| mlua::Error::runtime("no module to load"))?;
        let source = fs::read_to_string(file).map_err(|err| {
            mlua::Error::runtime(format!("failed to read {}: {err}", file.display()))
        })?;
        // Named as the requiring script would spell it, so errors and further
        // requires from inside it work the same.
        let relative = file
            .strip_prefix(self.root.as_deref().unwrap_or(Path::new("")))
            .unwrap_or(file);
        lua.load(source)
            .set_name(Self::chunk_name(&self.folder, relative))
            .into_function()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::mpsc;

    use super::*;
    use crate::luau::LuauPlugin;
    use crate::plugin::{Limits, PluginError};
    use crate::state::{self, Phase, PluginState, Shared};

    /// A plugin folder `plugins/req` holding `files`, next to a folder the
    /// plugin must not reach, `outside/secret.luau`. Returns the directory
    /// holding both.
    fn layout(test: &str, files: &[(&str, &str)]) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("bedrockrs-require-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let plugin = base.join("plugins").join("req");
        fs::create_dir_all(&plugin).unwrap();
        for (name, source) in files {
            let path = plugin.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        fs::create_dir_all(base.join("outside")).unwrap();
        fs::write(base.join("outside/secret.luau"), "return 'the secret'").unwrap();
        base
    }

    /// Runs `main` as the plugin in `base`, returning what it printed. The
    /// API is let through at the top level, which is what these tests use.
    fn run(base: &Path, main: &str) -> (Result<(), PluginError>, Vec<String>) {
        let printed = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&printed);
        let limits = Limits {
            memory: 16 * 1024 * 1024,
            execution: Duration::from_millis(250),
        };
        let (actions, _) = mpsc::channel(4);
        let shared = Shared::new(
            actions,
            Arc::new(move |_, _, message: &str| sink.lock().unwrap().push(message.to_owned())),
        );
        let plugin_state = PluginState::new("req", shared);
        {
            let mut plugin_state = state::lock(&plugin_state);
            plugin_state.release_output();
            plugin_state.phase = Phase::Loading;
        }
        let folder = base.join("plugins").join("req");
        let result =
            LuauPlugin::new(plugin_state, &folder, Path::new("main.luau"), main, limits).map(drop);
        let printed = printed.lock().unwrap().clone();
        (result, printed)
    }

    /// What `require(path)` does from the entry script: its result, or the
    /// start of its error.
    fn attempt(path: &str) -> String {
        format!(
            r#"
                local ok, result = pcall(require, "{path}")
                print(if ok then tostring(result) else "error: " .. tostring(result))
            "#
        )
    }

    #[test]
    fn modules_load_from_the_plugin_folder() {
        let base = layout(
            "inside",
            &[
                (
                    "util.luau",
                    "runs += 1 return { name = 'util', runs = function() return runs end }",
                ),
                (
                    "lib/init.luau",
                    "return require('./util').name .. ' and ' .. require('@self/helper')",
                ),
                ("lib/helper.lua", "return 'helper'"),
                (
                    "deep/nested/thing.luau",
                    "return require('../../util').name",
                ),
            ],
        );
        let (result, printed) = run(
            &base,
            r#"
                runs = 0
                local util = require("./util")
                print(util.name)
                print(require("./lib"))
                print(require("./deep/nested/thing"))
                print(require("./util") == util, util.runs())
                print(require("@bedrock-rs/core") == require("@bedrock-rs/core"))
                print(pcall(require, "@bedrock-rs/other"))
            "#,
        );
        result.unwrap();
        assert_eq!(
            printed[..4],
            ["util", "util and helper", "util", "true 1"],
            "a module runs once and is shared"
        );
        assert_eq!(printed[4], "true", "the API is one module");
        assert!(printed[5].starts_with("false"), "{}", printed[5]);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn nothing_outside_the_plugin_folder_loads() {
        let base = layout(
            "outside",
            &[(".luaurc", r#"{ "aliases": { "out": "../../outside" } }"#)],
        );
        for path in [
            "../outside/secret",
            "../../outside/secret",
            "./../../outside/secret",
            "@out/secret",
            "@self/../../outside/secret",
            "secret",
        ] {
            let (result, printed) = run(&base, &attempt(path));
            result.unwrap();
            assert!(printed[0].starts_with("error: "), "{path}: {printed:?}");
            assert!(!printed[0].contains("the secret"), "{path}");
        }
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn links_out_of_the_folder_are_refused() {
        let base = layout("link", &[]);
        let outside = base.join("outside");
        let link = base.join("plugins").join("req").join("linked");
        // A link to the folder outside: a symbolic link, or on Windows a
        // junction, which needs no special rights.
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&outside, &link).is_ok();
        #[cfg(windows)]
        let linked = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .is_ok_and(|output| output.status.success());
        assert!(
            linked,
            "could not link {} to {}",
            link.display(),
            outside.display()
        );
        let (result, printed) = run(&base, &attempt("./linked/secret"));
        result.unwrap();
        assert!(
            printed[0].contains("outside the plugin's folder"),
            "{printed:?}"
        );
        // Removing the link leaves what it points to alone.
        #[cfg(windows)]
        fs::remove_dir(&link).unwrap();
        #[cfg(unix)]
        fs::remove_file(&link).unwrap();
        assert!(outside.join("secret.luau").is_file());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn modules_count_against_the_time_limit() {
        let base = layout("slow", &[("spin.luau", "while true do end")]);
        let (result, _) = run(&base, "require('./spin')");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("execution time limit"), "{err}");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn errors_name_the_module_and_ambiguity_is_refused() {
        let base = layout(
            "errors",
            &[
                ("broken.luau", "\nerror('boom')"),
                ("both.luau", "return 1"),
                ("both.lua", "return 2"),
            ],
        );
        let (result, printed) = run(&base, &attempt("./broken"));
        result.unwrap();
        assert!(printed[0].contains("broken.luau:2: boom"), "{printed:?}");
        let (result, printed) = run(&base, &attempt("./both"));
        result.unwrap();
        assert!(printed[0].starts_with("error: "), "{printed:?}");
        fs::remove_dir_all(&base).unwrap();
    }
}
