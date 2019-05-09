//! Provides types for working with Volta hooks.

use std::env;
use std::fs::File;
use std::marker::PhantomData;
use std::path::Path;
use std::str::FromStr;

use lazycell::LazyCell;
use toml;

use crate::distro::node::NodeDistro;
use crate::distro::package::PackageDistro;
use crate::distro::yarn::YarnDistro;
use crate::distro::Distro;
use crate::error::ErrorDetails;
use crate::fs::touch;
use crate::path::{project_for_dir, user_hooks_file};
use readext::ReadExt;
use volta_fail::{Fallible, ResultExt, VoltaError};

pub(crate) mod serial;
pub mod tool;

/// A hook for publishing Volta events.
#[derive(PartialEq, Debug)]
pub enum Publish {
    /// Reports an event by sending a POST request to a URL.
    Url(String),

    /// Reports an event by forking a process and sending the event by IPC.
    Bin(String),
}

/// Lazily loaded Volta hook configuration
pub struct LazyHookConfig {
    settings: LazyCell<HookConfig>,
}

impl LazyHookConfig {
    /// Constructs a new `LazyHookConfig` (but does not initialize it).
    pub fn new() -> LazyHookConfig {
        LazyHookConfig {
            settings: LazyCell::new(),
        }
    }

    /// Forces the loading of the hook configuration
    pub fn get(&self) -> Fallible<&HookConfig> {
        self.settings.try_borrow_with(|| HookConfig::current())
    }
}

/// Volta hook configuration
pub struct HookConfig {
    pub node: Option<ToolHooks<NodeDistro>>,
    pub yarn: Option<ToolHooks<YarnDistro>>,
    pub package: Option<ToolHooks<PackageDistro>>,
    pub events: Option<EventHooks>,
}

/// Volta hooks for an individual tool
pub struct ToolHooks<D: Distro> {
    /// The hook for resolving the URL for a distro version
    pub distro: Option<tool::DistroHook>,
    /// The hook for resolving the URL for the latest version
    pub latest: Option<tool::MetadataHook>,
    /// The hook for resolving the Tool Index URL
    pub index: Option<tool::MetadataHook>,

    pub phantom: PhantomData<D>,
}

impl<D: Distro> ToolHooks<D> {
    /// Creates a merged struct, with "right" having precedence over "left".
    fn merge(left: Self, right: Self) -> Self {
        Self {
            distro: right.distro.or(left.distro),
            latest: right.latest.or(left.latest),
            index: right.index.or(left.index),
            phantom: PhantomData,
        }
    }
}

impl HookConfig {
    /// Returns the current hooks, which are a merge between the user hooks and
    /// the project hooks (if any).
    fn current() -> Fallible<Self> {
        let maybe_project_config = Self::for_current_dir()?;
        let user_config = Self::for_user()?;

        Ok(if let Some(project_config) = maybe_project_config {
            Self::merge(user_config, project_config)
        } else {
            user_config
        })
    }

    /// Returns the per-project hooks for the current directory.
    fn for_current_dir() -> Fallible<Option<Self>> {
        Self::for_dir(&env::current_dir().with_context(|_| ErrorDetails::CurrentDirError)?)
    }

    /// Returns the per-project hooks for the specified directory.  If the
    /// specified directory is not itself a project, its ancestors will be
    /// searched.
    fn for_dir(base_dir: &Path) -> Fallible<Option<Self>> {
        match project_for_dir(base_dir) {
            Some(dir) => {
                let path = dir.join("hooks.toml");

                if !path.is_file() {
                    return Ok(None);
                }

                let src = File::open(&path)
                    .and_then(|mut file| file.read_into_string())
                    .with_context(|_| ErrorDetails::ReadHooksError {
                        file: path.to_string_lossy().to_string(),
                    })?;
                src.parse().map(|hooks| Some(hooks))
            }
            None => Ok(None),
        }
    }

    /// Returns the per-user hooks, loaded from the filesystem.
    fn for_user() -> Fallible<Self> {
        let path = user_hooks_file()?;
        let src = touch(&path)
            .and_then(|mut file| file.read_into_string())
            .with_context(|_| ErrorDetails::ReadHooksError {
                file: path.to_string_lossy().to_string(),
            })?;
        src.parse()
    }

    /// Creates a merged struct, with "right" having precedence over "left".
    fn merge(left: Self, right: Self) -> Self {
        Self {
            node: match (left.node, right.node) {
                (Some(left), Some(right)) => Some(ToolHooks::merge(left, right)),
                (Some(left), None) => Some(left),
                (None, Some(right)) => Some(right),
                (None, None) => None,
            },
            yarn: match (left.yarn, right.yarn) {
                (Some(left), Some(right)) => Some(ToolHooks::merge(left, right)),
                (Some(left), None) => Some(left),
                (None, Some(right)) => Some(right),
                (None, None) => None,
            },
            package: match (left.package, right.package) {
                (Some(left), Some(right)) => Some(ToolHooks::merge(left, right)),
                (Some(left), None) => Some(left),
                (None, Some(right)) => Some(right),
                (None, None) => None,
            },
            events: match (left.events, right.events) {
                (Some(left), Some(right)) => Some(EventHooks::merge(left, right)),
                (Some(left), None) => Some(left),
                (None, Some(right)) => Some(right),
                (None, None) => None,
            },
        }
    }
}

impl FromStr for HookConfig {
    type Err = VoltaError;

    fn from_str(src: &str) -> Result<Self, Self::Err> {
        let serial: serial::HookConfig =
            toml::from_str(src).with_context(|_| ErrorDetails::ParseHooksError)?;
        serial.into_hook_config()
    }
}

/// Volta hooks related to events.
pub struct EventHooks {
    /// The hook for publishing events, if any.
    pub publish: Option<Publish>,
}

impl EventHooks {
    /// Creates a merged struct, with "right" having precedence over "left".
    fn merge(left: Self, right: Self) -> Self {
        Self {
            publish: right.publish.or(left.publish),
        }
    }
}

#[cfg(test)]
pub mod tests {

    use super::{tool, HookConfig, Publish};
    use std::fs;
    use std::path::PathBuf;

    fn fixture_path(fixture_dir: &str) -> PathBuf {
        let mut cargo_manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        cargo_manifest_dir.push("fixtures");
        cargo_manifest_dir.push(fixture_dir);
        cargo_manifest_dir
    }

    #[test]
    fn test_from_str_event_url() {
        let fixture_dir = fixture_path("hooks");
        let mut url_file = fixture_dir.clone();

        url_file.push("event_url.toml");
        let hooks: HookConfig = fs::read_to_string(url_file)
            .expect("Chould not read event_url.toml")
            .parse()
            .expect("Could not parse event_url.toml");
        assert_eq!(
            hooks.events.unwrap().publish,
            Some(Publish::Url("https://google.com".to_string()))
        );
    }

    #[test]
    fn test_from_str_bins() {
        let fixture_dir = fixture_path("hooks");
        let mut url_file = fixture_dir.clone();

        url_file.push("bins.toml");
        let hooks: HookConfig = fs::read_to_string(url_file)
            .expect("Chould not read bins.toml")
            .parse()
            .expect("Could not parse bins.toml");

        let node = hooks.node.unwrap();
        let yarn = hooks.yarn.unwrap();
        assert_eq!(
            node.distro,
            Some(tool::DistroHook::Bin(
                "/some/bin/for/node/distro".to_string()
            ))
        );
        assert_eq!(
            node.latest,
            Some(tool::MetadataHook::Bin(
                "/some/bin/for/node/latest".to_string()
            ))
        );
        assert_eq!(
            node.index,
            Some(tool::MetadataHook::Bin(
                "/some/bin/for/node/index".to_string()
            ))
        );
        assert_eq!(
            yarn.distro,
            Some(tool::DistroHook::Bin("/bin/to/yarn/distro".to_string()))
        );
        assert_eq!(
            yarn.latest,
            Some(tool::MetadataHook::Bin("/bin/to/yarn/latest".to_string()))
        );
        assert_eq!(
            yarn.index,
            Some(tool::MetadataHook::Bin("/bin/to/yarn/index".to_string()))
        );
        assert_eq!(
            hooks.events.unwrap().publish,
            Some(Publish::Bin("/events/bin".to_string()))
        );
    }

    #[test]
    fn test_from_str_prefixes() {
        let fixture_dir = fixture_path("hooks");
        let mut url_file = fixture_dir.clone();

        url_file.push("prefixes.toml");
        let hooks: HookConfig = fs::read_to_string(url_file)
            .expect("Chould not read prefixes.toml")
            .parse()
            .expect("Could not parse prefixes.toml");

        let node = hooks.node.unwrap();
        let yarn = hooks.yarn.unwrap();
        assert_eq!(
            node.distro,
            Some(tool::DistroHook::Prefix(
                "http://localhost/node/distro/".to_string()
            ))
        );
        assert_eq!(
            node.latest,
            Some(tool::MetadataHook::Prefix(
                "http://localhost/node/latest/".to_string()
            ))
        );
        assert_eq!(
            node.index,
            Some(tool::MetadataHook::Prefix(
                "http://localhost/node/index/".to_string()
            ))
        );
        assert_eq!(
            yarn.distro,
            Some(tool::DistroHook::Prefix(
                "http://localhost/yarn/distro/".to_string()
            ))
        );
        assert_eq!(
            yarn.latest,
            Some(tool::MetadataHook::Prefix(
                "http://localhost/yarn/latest/".to_string()
            ))
        );
        assert_eq!(
            yarn.index,
            Some(tool::MetadataHook::Prefix(
                "http://localhost/yarn/index/".to_string()
            ))
        );
    }

    #[test]
    fn test_from_str_templates() {
        let fixture_dir = fixture_path("hooks");
        let mut url_file = fixture_dir.clone();

        url_file.push("templates.toml");
        let hooks: HookConfig = fs::read_to_string(url_file)
            .expect("Chould not read templates.toml")
            .parse()
            .expect("Could not parse templates.toml");

        let node = hooks.node.unwrap();
        let yarn = hooks.yarn.unwrap();
        assert_eq!(
            node.distro,
            Some(tool::DistroHook::Template(
                "http://localhost/node/distro/{{version}}/".to_string()
            ))
        );
        assert_eq!(
            node.latest,
            Some(tool::MetadataHook::Template(
                "http://localhost/node/latest/{{version}}/".to_string()
            ))
        );
        assert_eq!(
            node.index,
            Some(tool::MetadataHook::Template(
                "http://localhost/node/index/{{version}}/".to_string()
            ))
        );
        assert_eq!(
            yarn.distro,
            Some(tool::DistroHook::Template(
                "http://localhost/yarn/distro/{{version}}/".to_string()
            ))
        );
        assert_eq!(
            yarn.latest,
            Some(tool::MetadataHook::Template(
                "http://localhost/yarn/latest/{{version}}/".to_string()
            ))
        );
        assert_eq!(
            yarn.index,
            Some(tool::MetadataHook::Template(
                "http://localhost/yarn/index/{{version}}/".to_string()
            ))
        );
    }

    #[test]
    fn test_for_dir() {
        let project_dir = fixture_path("hooks/project");
        let hooks = HookConfig::for_dir(&project_dir)
            .expect("Could not read project hooks.toml")
            .expect("Could not find project hooks.toml");
        let node = hooks.node.unwrap();

        assert_eq!(
            node.distro,
            Some(tool::DistroHook::Bin(
                "/some/bin/for/node/distro".to_string()
            ))
        );
        assert_eq!(
            node.latest,
            Some(tool::MetadataHook::Bin(
                "/some/bin/for/node/latest".to_string()
            ))
        );
        assert_eq!(
            node.index,
            Some(tool::MetadataHook::Bin(
                "/some/bin/for/node/index".to_string()
            ))
        );
        assert_eq!(
            hooks.events.unwrap().publish,
            Some(Publish::Bin("/events/bin".to_string()))
        );
    }

    #[test]
    fn test_merge() {
        let fixture_dir = fixture_path("hooks");
        let user_hooks: HookConfig = fs::read_to_string(fixture_dir.join("templates.toml"))
            .expect("Chould not read templates.toml")
            .parse()
            .expect("Could not parse templates.toml");

        let project_dir = fixture_path("hooks/project");
        let project_hooks = HookConfig::for_dir(&project_dir)
            .expect("Could not read project hooks.toml")
            .expect("Could not find project hooks.toml");

        let merged_hooks = HookConfig::merge(user_hooks, project_hooks);
        let node = merged_hooks.node.expect("No node config found");
        let yarn = merged_hooks.yarn.expect("No yarn config found");

        assert_eq!(
            node.distro,
            Some(tool::DistroHook::Bin(
                "/some/bin/for/node/distro".to_string()
            ))
        );
        assert_eq!(
            node.latest,
            Some(tool::MetadataHook::Bin(
                "/some/bin/for/node/latest".to_string()
            ))
        );
        assert_eq!(
            node.index,
            Some(tool::MetadataHook::Bin(
                "/some/bin/for/node/index".to_string()
            ))
        );
        assert_eq!(
            yarn.distro,
            Some(tool::DistroHook::Template(
                "http://localhost/yarn/distro/{{version}}/".to_string()
            ))
        );
        assert_eq!(
            yarn.latest,
            Some(tool::MetadataHook::Template(
                "http://localhost/yarn/latest/{{version}}/".to_string()
            ))
        );
        assert_eq!(
            yarn.index,
            Some(tool::MetadataHook::Template(
                "http://localhost/yarn/index/{{version}}/".to_string()
            ))
        );
        assert_eq!(
            merged_hooks.events.expect("No events config found").publish,
            Some(Publish::Bin("/events/bin".to_string()))
        );
    }
}
