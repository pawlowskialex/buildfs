//! `buildfs integrate`: point toolchains' global artifact/cache locations at
//! the mount through each tool's own configuration, so builds, IDEs and CI
//! scripts pick it up without per-project changes.
//!
//! Settings are only changed when the user names the tool; existing values
//! that don't point into the mount are never overwritten, and `--revert`
//! only removes values that do. Settings made through an environment variable
//! (the Nix modules do that) or a file that Nix manages are reported, and left to
//! whatever set them.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use crate::mounts::find_root_mount;

fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).context("HOME is not set")
}

fn have(cmd: &str) -> bool {
    Command::new("/usr/bin/which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn output(cmd: &mut Command) -> Option<String> {
    let out = cmd.stderr(std::process::Stdio::null()).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// A setting stored in a TOML file (`cargo`'s config.toml, `uv.toml`).
struct TomlKey {
    file: PathBuf,
    table: Option<&'static str>,
    key: &'static str,
}

impl TomlKey {
    fn doc(&self) -> anyhow::Result<toml_edit::DocumentMut> {
        match std::fs::read_to_string(&self.file) {
            Ok(text) => text
                .parse()
                .with_context(|| format!("parsing {}", self.file.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml_edit::DocumentMut::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn get(&self) -> anyhow::Result<Option<String>> {
        let doc = self.doc()?;
        let item = match self.table {
            Some(t) => doc.get(t).and_then(|t| t.get(self.key)),
            None => doc.get(self.key),
        };
        Ok(item.and_then(|i| i.as_str()).map(str::to_owned))
    }

    fn set(&self, value: Option<&str>) -> anyhow::Result<()> {
        let mut doc = self.doc()?;
        let table = match self.table {
            Some(t) => {
                if !doc.contains_key(t) {
                    doc[t] = toml_edit::table();
                }
                doc[t].as_table_mut().with_context(|| format!("[{t}] is not a table"))?
            }
            None => doc.as_table_mut(),
        };
        match value {
            Some(v) => {
                table[self.key] = toml_edit::value(v);
            }
            None => {
                let _ = table.remove(self.key);
                if let Some(t) = self.table
                    && doc.get(t).and_then(|i| i.as_table()).is_some_and(toml_edit::Table::is_empty)
                {
                    let _ = doc.remove(t);
                }
            }
        }
        if managed_by_nix(&self.file) {
            bail!(
                "{} is managed by Nix, so change this in your configuration (or use services.buildfs.integrate)",
                self.file.display()
            );
        }
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.file, doc.to_string())?;
        Ok(())
    }
}

/// Whether `file` is a link into the (read-only) Nix store, as home-manager makes.
/// It may take more than one hop to get there.
fn managed_by_nix(file: &Path) -> bool {
    std::fs::canonicalize(file)
        .or_else(|_| std::fs::read_link(file))
        .is_ok_and(|target| target.starts_with("/nix/store"))
}

enum Store {
    Toml(TomlKey),
    /// `defaults` domain + key (Xcode).
    Defaults(String, &'static str),
    /// `go env -w KEY=...`.
    GoEnv(&'static str),
}

impl Store {
    fn get(&self) -> anyhow::Result<Option<String>> {
        match self {
            Store::Toml(k) => k.get(),
            Store::Defaults(domain, key) => Ok(output(Command::new("/usr/bin/defaults").args(["read", domain, key]))),
            // Only values set via `go env -w` (the env file), not defaults.
            Store::GoEnv(key) => {
                let file = output(Command::new("go").args(["env", "GOENV"])).context("go env GOENV")?;
                let text = std::fs::read_to_string(file).unwrap_or_default();
                Ok(text
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("{key}=")))
                    .map(str::to_owned))
            }
        }
    }

    fn set(&self, value: Option<&str>) -> anyhow::Result<()> {
        let ok = match (self, value) {
            (Store::Toml(k), v) => {
                k.set(v)?;
                true
            }
            (Store::Defaults(domain, key), Some(v)) => Command::new("/usr/bin/defaults")
                .args(["write", domain, key, "-string", v])
                .status()?
                .success(),
            (Store::Defaults(domain, key), None) => Command::new("/usr/bin/defaults")
                .args(["delete", domain, key])
                .status()?
                .success(),
            (Store::GoEnv(key), Some(v)) => Command::new("go")
                .args(["env", "-w", &format!("{key}={v}")])
                .status()?
                .success(),
            (Store::GoEnv(key), None) => Command::new("go").args(["env", "-u", key]).status()?.success(),
        };
        if !ok {
            bail!("updating setting failed");
        }
        Ok(())
    }

    fn describe(&self) -> String {
        match self {
            Store::Toml(k) => match k.table {
                Some(t) => format!("{} [{t}] {}", k.file.display(), k.key),
                None => format!("{} {}", k.file.display(), k.key),
            },
            Store::Defaults(domain, key) => format!("defaults {domain} {key}"),
            Store::GoEnv(key) => format!("go env {key}"),
        }
    }
}

struct Integration {
    name: &'static str,
    what: &'static str,
    installed: bool,
    store: Store,
    /// The environment variable that overrides `store`, if the tool has one.
    env: Option<&'static str>,
    /// Value relative to the mount root.
    value: &'static str,
    note: Option<&'static str>,
}

fn integrations() -> anyhow::Result<Vec<Integration>> {
    let home = home()?;
    let cargo_home = std::env::var_os("CARGO_HOME").map_or_else(|| home.join(".cargo"), PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
    let xcode_domain = std::env::var("BUILDFS_XCODE_DOMAIN").unwrap_or_else(|_| "com.apple.dt.Xcode".into());
    Ok(vec![
        Integration {
            name: "rust",
            what: "cargo build-dir: dependencies, incremental data and build scripts of every workspace \
                   (final binaries stay in target/)",
            installed: have("cargo"),
            store: Store::Toml(TomlKey {
                file: cargo_home.join("config.toml"),
                table: Some("build"),
                key: "build-dir",
            }),
            env: Some("CARGO_BUILD_BUILD_DIR"),
            value: "cargo/{workspace-path-hash}",
            note: None,
        },
        Integration {
            name: "xcode",
            what: "Xcode DerivedData (Xcode and xcodebuild)",
            installed: Path::new("/Applications/Xcode.app").exists() || have("xcodebuild"),
            store: Store::Defaults(xcode_domain, "IDECustomDerivedDataLocation"),
            env: None,
            value: "xcode/DerivedData",
            note: Some("restart Xcode to apply"),
        },
        Integration {
            name: "go",
            what: "Go build cache (GOCACHE)",
            installed: have("go"),
            store: Store::GoEnv("GOCACHE"),
            env: Some("GOCACHE"),
            value: "go/cache",
            note: None,
        },
        Integration {
            name: "uv",
            what: "uv cache (Python packages and builds)",
            installed: have("uv"),
            store: Store::Toml(TomlKey {
                file: xdg.join("uv/uv.toml"),
                table: None,
                key: "cache-dir",
            }),
            env: Some("UV_CACHE_DIR"),
            value: "uv/cache",
            note: Some("uv falls back to copying across filesystems; set UV_LINK_MODE=copy to silence the warning"),
        },
    ])
}

/// Tools configured only through environment variables.
const ENV_ONLY: &[(&str, &str, &str)] = &[
    ("zig", "ZIG_GLOBAL_CACHE_DIR", "zig/cache"),
    ("gradle", "GRADLE_USER_HOME", "gradle"),
    ("python bytecode", "PYTHONPYCACHEPREFIX", "pycache"),
];

impl Integration {
    /// The environment variable overriding this setting, and its value.
    fn overridden(&self) -> Option<(&'static str, String)> {
        let var = self.env?;
        Some((var, std::env::var(var).ok().filter(|v| !v.is_empty())?))
    }
}

/// What to say about a setting made through `var`, which a tool's config can't
/// override and this command shouldn't try to.
fn describe_override(var: &str, value: &str, root: &Path) -> String {
    if Path::new(value).starts_with(root) {
        format!("integrated through ${var}")
    } else {
        format!("set elsewhere through ${var}: {value}")
    }
}

pub fn run(tools: &[String], mount: Option<&Path>, revert: bool) -> anyhow::Result<()> {
    let root = find_root_mount(mount)?.path;
    let all = integrations()?;
    let known: Vec<&str> = all.iter().map(|i| i.name).collect();
    let selected: Vec<&Integration> = if tools.iter().any(|t| t == "all") {
        all.iter().filter(|i| i.installed).collect()
    } else {
        for t in tools {
            if !known.contains(&t.as_str()) {
                bail!("unknown tool {t:?} (known: {}, all)", known.join(", "));
            }
        }
        all.iter().filter(|i| tools.iter().any(|t| t == i.name)).collect()
    };

    if selected.is_empty() {
        // Status / dry run.
        println!("buildfs mount: {}\n", root.display());
        for i in &all {
            let target = root.join(i.value);
            let current = i.store.get().unwrap_or(None);
            let state = match (&current, i.overridden()) {
                _ if !i.installed => "not installed".to_owned(),
                (_, Some((var, value))) => describe_override(var, &value, &root),
                (Some(v), None) if Path::new(v).starts_with(&root) => "integrated".to_owned(),
                (Some(v), None) => format!("set elsewhere: {v}"),
                (None, None) => "not integrated".to_owned(),
            };
            println!("{:6} {state}\n       {}\n       {} = {}\n", i.name, i.what, i.store.describe(), target.display());
        }
        println!("apply with: buildfs integrate <tool>... (or `all`), undo with --revert\n");
        println!("environment-only tools (add to your shell profile; use `launchctl setenv` for GUI apps):");
        for (tool, var, rel) in ENV_ONLY {
            println!("  {tool:16} export {var}={}", root.join(rel).display());
        }
        println!("\nper-project directories (node_modules, .next, SwiftPM .build, build/, .venv):\n  buildfs attach <dir>");
        return Ok(());
    }

    for i in selected {
        if let Some((var, value)) = i.overridden() {
            println!("{}: {}; left alone (change it where it's set)", i.name, describe_override(var, &value, &root));
            continue;
        }
        let target = root.join(i.value);
        let current = i.store.get()?;
        let ours = current.as_deref().is_some_and(|v| Path::new(v).starts_with(&root));
        if revert {
            if ours {
                i.store.set(None)?;
                println!("{}: reverted", i.name);
            } else {
                println!("{}: not pointing at buildfs, left alone", i.name);
            }
            continue;
        }
        if !i.installed {
            println!("{}: not installed, skipped", i.name);
            continue;
        }
        if let Some(v) = &current
            && !ours
        {
            println!("{}: already set to {v}; not overwriting (change it by hand if you want)", i.name);
            continue;
        }
        // Templates such as `{workspace-path-hash}` are expanded by the tool.
        let base = i.value.split("/{").next().unwrap_or(i.value);
        std::fs::create_dir_all(root.join(base))?;
        i.store.set(Some(&target.display().to_string()))?;
        println!("{}: {} -> {}", i.name, i.store.describe(), target.display());
        if let Some(note) = i.note {
            println!("  note: {note}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_are_told_apart() {
        let root = Path::new("/m");
        assert_eq!(describe_override("GOCACHE", "/m/go/cache", root), "integrated through $GOCACHE");
        assert_eq!(
            describe_override("GOCACHE", "/other", root),
            "set elsewhere through $GOCACHE: /other"
        );
        // Cargo expands the template, so it isn't a real path yet.
        assert!(describe_override("X", "/m/cargo/{workspace-path-hash}", root).starts_with("integrated"));
    }

    #[test]
    fn files_in_the_nix_store_are_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink("/nix/store/does-not-exist/config.toml", &link).unwrap();
        assert!(managed_by_nix(&link));
        let key = TomlKey { file: link, table: Some("build"), key: "build-dir" };
        let err = key.set(Some("/m/cargo")).unwrap_err().to_string();
        assert!(err.contains("managed by Nix"), "{err}");

        let plain = dir.path().join("plain.toml");
        std::fs::write(&plain, "").unwrap();
        assert!(!managed_by_nix(&plain));
        let key = TomlKey { file: plain.clone(), table: Some("build"), key: "build-dir" };
        key.set(Some("/m/cargo")).unwrap();
        assert!(std::fs::read_to_string(plain).unwrap().contains("build-dir"));
    }
}
