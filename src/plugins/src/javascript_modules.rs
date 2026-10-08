//! `import` for JavaScript plugins, confined to the plugin's own folder.
//!
//! A plugin's modules import each other by relative path: `import { x } from
//! "./util.js"` loads `util.js` next to the module that imports it, and `../`
//! goes up a folder, but never above the plugin's own. The extension may be
//! left out (`./util` finds `util.js` or `util.mjs`), and a folder stands for
//! its `index.js`. A `.json` file imports as its default export.
//!
//! One module is built in: `@bedrock-rs/core`, the plugin API. It is native:
//! its exports are the Rust-backed objects the plugin's runtime was given
//! (see [`crate::javascript`]), not JavaScript source. Other bare names, such
//! as npm packages, are refused with a message saying what can be imported.
//!
//! Files are checked by where they really are, after following symbolic
//! links, so a link inside the folder cannot reach outside it. Each module
//! runs once per plugin.

use std::fs;
use std::path::{Path, PathBuf};

use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::{Declarations, Exports, ModuleDef};
use rquickjs::{Ctx, Error, Module, Object, Result, Value};

/// The plugin API's module name.
pub(crate) const CORE: &str = "@bedrock-rs/core";

/// What `@bedrock-rs/core` exports, besides `default` (all of them).
const CORE_EXPORTS: [&str; 4] = ["Logger", "Server", "Player", "World"];

/// Extensions tried, in order, for an import that leaves them out.
const EXTENSIONS: [&str; 2] = ["js", "mjs"];

/// Finds and loads one plugin's modules. Module names are paths, as the
/// plugin directory spells them (`plugins/hello/lib/util.js`), so errors and
/// stack traces point at the file.
#[derive(Debug, Clone)]
pub(crate) struct PluginModules {
    /// The plugin's folder, as module names spell it.
    prefix: String,
    /// The folder where it really is, to check files against.
    root: Option<PathBuf>,
}

impl PluginModules {
    pub fn new(folder: &Path) -> Self {
        Self {
            prefix: spelled(folder),
            root: fs::canonicalize(folder).ok(),
        }
    }

    /// The module name of the file at `relative` below `folder`.
    pub fn name(folder: &Path, relative: &Path) -> String {
        spelled(&folder.join(relative))
    }

    /// `name`'s path below the plugin's folder, if it is one of its modules.
    fn relative<'a>(&self, name: &'a str) -> Option<&'a str> {
        name.strip_prefix(&self.prefix)?.strip_prefix('/')
    }

    /// The module `specifier` names, imported from `base`: its path below
    /// the folder.
    fn resolve_relative(&self, base: &str, specifier: &str) -> std::result::Result<String, String> {
        let root = self.root.as_ref().ok_or("the plugin's folder is gone")?;
        let base = self
            .relative(base)
            .ok_or("only the plugin's own modules can import files")?;
        // The importing module's folder, then each step of the specifier.
        let mut parts: Vec<&str> = base.split('/').collect();
        parts.pop();
        for part in specifier.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop().ok_or("it is outside the plugin's folder")?;
                }
                part => parts.push(part),
            }
        }
        let path = parts.join("/");
        if path.is_empty() {
            return Err("it is the plugin's folder itself".into());
        }
        let mut candidates = vec![path.clone()];
        candidates.extend(
            EXTENSIONS
                .iter()
                .map(|extension| format!("{path}.{extension}")),
        );
        candidates.extend(
            EXTENSIONS
                .iter()
                .map(|extension| format!("{path}/index.{extension}")),
        );
        let found = candidates
            .into_iter()
            .find(|candidate| root.join(candidate).is_file())
            .ok_or("there is no such file in the plugin's folder")?;
        if !is_importable(Path::new(&found)) {
            return Err("only .js, .mjs and .json files can be imported".into());
        }
        // Where the file really is, past any links, must be in the folder.
        let real = fs::canonicalize(root.join(&found)).map_err(|err| err.to_string())?;
        if !real.starts_with(root) {
            return Err("it is outside the plugin's folder".into());
        }
        Ok(found)
    }
}

impl Resolver for PluginModules {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> Result<String> {
        if name == CORE {
            return Ok(CORE.to_owned());
        }
        if !(name.starts_with("./") || name.starts_with("../")) {
            return Err(Error::new_resolving_message(
                base,
                name,
                format!(
                    "a plugin can import \"{CORE}\" and its own files, by relative path \
                     (\"./util.js\"); bundle anything else into the plugin"
                ),
            ));
        }
        match self.resolve_relative(base, name) {
            Ok(relative) => Ok(format!("{}/{relative}", self.prefix)),
            Err(reason) => Err(Error::new_resolving_message(base, name, reason)),
        }
    }
}

impl Loader for PluginModules {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> Result<Module<'js>> {
        if name == CORE {
            return Module::declare_def::<CoreModule, _>(ctx.clone(), CORE);
        }
        let (Some(root), Some(relative)) = (&self.root, self.relative(name)) else {
            return Err(Error::new_loading_message(
                name,
                "not a module of this plugin",
            ));
        };
        let path = root.join(relative);
        let source = fs::read_to_string(&path)
            .map_err(|err| Error::new_loading_message(name, err.to_string()))?;
        let source = if has_extension(&path, "json") {
            format!("export default {source};")
        } else {
            source
        };
        Module::declare(ctx.clone(), name, source)
    }
}

/// `@bedrock-rs/core`: exports the API objects stored in the runtime's
/// userdata when the plugin was set up.
struct CoreModule;

impl ModuleDef for CoreModule {
    fn declare<'js>(declarations: &Declarations<'js>) -> Result<()> {
        for name in CORE_EXPORTS {
            declarations.declare(name)?;
        }
        declarations.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &Exports<'js>) -> Result<()> {
        let core = ctx
            .userdata::<Object<'js>>()
            .map(|core| core.clone())
            .ok_or_else(|| Error::new_loading_message(CORE, "the plugin API is missing"))?;
        for name in CORE_EXPORTS {
            exports.export(name, core.get::<_, Value<'js>>(name)?)?;
        }
        exports.export("default", core)?;
        Ok(())
    }
}

/// A path as module names spell it, with `/` between folders on every system.
fn spelled(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn has_extension(path: &Path, wanted: &str) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(wanted))
}

fn is_importable(path: &Path) -> bool {
    ["js", "mjs", "json"]
        .iter()
        .any(|extension| has_extension(path, extension))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(test: &str) -> PathBuf {
        let folder =
            std::env::temp_dir().join(format!("bedrockrs-modules-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(folder.join("lib")).unwrap();
        for file in [
            "index.js",
            "lib/util.js",
            "lib/index.mjs",
            "data.json",
            "notes.txt",
        ] {
            fs::write(folder.join(file), "").unwrap();
        }
        folder
    }

    #[test]
    fn relative_imports_stay_in_the_folder() {
        let folder = folder("relative");
        let modules = PluginModules::new(&folder);
        let base = PluginModules::name(&folder, Path::new("index.js"));
        let nested = PluginModules::name(&folder, Path::new("lib/util.js"));
        let resolve = |base: &str, specifier: &str| modules.resolve_relative(base, specifier);
        assert_eq!(resolve(&base, "./lib/util.js").unwrap(), "lib/util.js");
        assert_eq!(resolve(&base, "./lib/util").unwrap(), "lib/util.js");
        assert_eq!(resolve(&base, "./lib").unwrap(), "lib/index.mjs");
        assert_eq!(resolve(&base, "./data.json").unwrap(), "data.json");
        assert_eq!(resolve(&nested, "../index.js").unwrap(), "index.js");
        assert_eq!(resolve(&nested, "./../lib/./util").unwrap(), "lib/util.js");
        assert!(
            resolve(&base, "../index.js")
                .unwrap_err()
                .contains("outside")
        );
        assert!(
            resolve(&nested, "../../x.js")
                .unwrap_err()
                .contains("outside")
        );
        assert!(
            resolve(&base, "./missing.js")
                .unwrap_err()
                .contains("no such file")
        );
        assert!(
            resolve(&base, "./notes.txt")
                .unwrap_err()
                .contains("can be imported")
        );
        assert!(resolve(CORE, "./index.js").is_err());
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn module_names_use_forward_slashes() {
        let name = PluginModules::name(
            Path::new("plugins").join("hello").as_path(),
            Path::new("index.js"),
        );
        assert_eq!(name, "plugins/hello/index.js");
    }
}
