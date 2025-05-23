use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use color_eyre::eyre::Result;
use indexmap::IndexMap;
use itertools::Itertools;
use rayon::prelude::*;

pub use builder::ToolsetBuilder;
pub use tool_source::ToolSource;
pub use tool_version::ToolVersion;
pub use tool_version_list::ToolVersionList;
pub use tool_version_request::ToolVersionRequest;

use crate::config::Config;
use crate::env;
use crate::install_context::InstallContext;
use crate::path_env::PathEnv;
use crate::plugins::{Plugin, PluginName};
use crate::runtime_symlinks;
use crate::shims;
use crate::ui::multi_progress_report::MultiProgressReport;

mod builder;
mod tool_source;
mod tool_version;
mod tool_version_list;
mod tool_version_request;

pub type ToolVersionOptions = BTreeMap<String, String>;

/// a toolset is a collection of tools for various plugins
///
/// one example is a .tool-versions file
/// the idea is that we start with an empty toolset, then
/// merge in other toolsets from various sources
#[derive(Debug, Default, Clone)]
pub struct Toolset {
    pub versions: IndexMap<PluginName, ToolVersionList>,
    pub source: Option<ToolSource>,
    pub latest_versions: bool,
    pub disable_tools: BTreeSet<PluginName>,
}

impl Toolset {
    pub fn new(source: ToolSource) -> Self {
        Self {
            source: Some(source),
            ..Default::default()
        }
    }
    pub fn add_version(&mut self, tvr: ToolVersionRequest, opts: ToolVersionOptions) {
        if self.disable_tools.contains(tvr.plugin_name()) {
            return;
        }
        let tvl = self
            .versions
            .entry(tvr.plugin_name().clone())
            .or_insert_with(|| {
                ToolVersionList::new(tvr.plugin_name().to_string(), self.source.clone().unwrap())
            });
        tvl.requests.push((tvr, opts));
    }
    pub fn merge(&mut self, other: &Toolset) {
        let mut versions = other.versions.clone();
        for (plugin, tvl) in self.versions.clone() {
            if !other.versions.contains_key(&plugin) {
                versions.insert(plugin, tvl);
            }
        }
        versions.retain(|_, tvl| !self.disable_tools.contains(&tvl.plugin_name));
        self.versions = versions;
        self.source = other.source.clone();
    }
    pub fn resolve(&mut self, config: &mut Config) {
        self.list_missing_plugins(config);
        self.versions
            .iter_mut()
            .collect::<Vec<_>>()
            .par_iter_mut()
            .for_each(|(_, v)| v.resolve(config, self.latest_versions));
    }
    pub fn install_arg_versions(&mut self, config: &mut Config) -> Result<()> {
        let mpr = MultiProgressReport::new(config.show_progress_bars());
        let versions = self
            .list_missing_versions(config)
            .into_iter()
            .filter(|tv| matches!(self.versions[&tv.plugin_name].source, ToolSource::Argument))
            .cloned()
            .collect_vec();
        self.install_versions(config, versions, &mpr, false)
    }

    pub fn list_missing_plugins(&self, config: &mut Config) -> Vec<PluginName> {
        self.versions
            .keys()
            .map(|p| config.get_or_create_plugin(p))
            .filter(|p| !p.is_installed())
            .map(|p| p.name().into())
            .collect()
    }

    pub fn install_versions(
        &mut self,
        config: &mut Config,
        versions: Vec<ToolVersion>,
        mpr: &MultiProgressReport,
        force: bool,
    ) -> Result<()> {
        if versions.is_empty() {
            return Ok(());
        }
        self.latest_versions = true;
        let queue: Vec<_> = versions
            .into_iter()
            .group_by(|v| v.plugin_name.clone())
            .into_iter()
            .map(|(pn, v)| (config.get_or_create_plugin(&pn), v.collect_vec()))
            .collect();
        for (t, _) in &queue {
            if !t.is_installed() {
                t.ensure_installed(config, Some(mpr), false)?;
            }
        }
        let queue = Arc::new(Mutex::new(queue));
        thread::scope(|s| {
            (0..config.settings.jobs)
                .map(|_| {
                    let queue = queue.clone();
                    let config = &*config;
                    let ts = &*self;
                    s.spawn(move || {
                        let next_job = || queue.lock().unwrap().pop();
                        while let Some((t, versions)) = next_job() {
                            for tv in versions {
                                let ctx = InstallContext {
                                    config,
                                    ts,
                                    tv: tv.request.resolve(
                                        config,
                                        t.clone(),
                                        tv.opts.clone(),
                                        true,
                                    )?,
                                    pr: mpr.add(),
                                    force,
                                };
                                t.install_version(ctx)?;
                            }
                        }
                        Ok(())
                    })
                })
                .collect_vec()
                .into_iter()
                .map(|t| t.join().unwrap())
                .collect::<Result<Vec<()>>>()
        })?;
        self.resolve(config);
        shims::reshim(config, self)?;
        runtime_symlinks::rebuild(config)
    }
    pub fn list_missing_versions(&self, config: &Config) -> Vec<&ToolVersion> {
        self.versions
            .iter()
            .map(|(p, tvl)| {
                let p = config.plugins.get(p).unwrap();
                (p, tvl)
            })
            .flat_map(|(p, tvl)| {
                tvl.versions
                    .iter()
                    .filter(|tv| !p.is_version_installed(tv))
                    .collect_vec()
            })
            .collect()
    }
    pub fn list_installed_versions(
        &self,
        config: &Config,
    ) -> Result<Vec<(Arc<dyn Plugin>, ToolVersion)>> {
        let current_versions: HashMap<(PluginName, String), (Arc<dyn Plugin>, ToolVersion)> = self
            .list_current_versions(config)
            .into_iter()
            .map(|(p, tv)| ((p.name().into(), tv.version.clone()), (p.clone(), tv)))
            .collect();
        let versions = config
            .plugins
            .values()
            .collect_vec()
            .into_par_iter()
            .map(|p| {
                let versions = p.list_installed_versions()?;
                Ok(versions.into_iter().map(|v| {
                    match current_versions.get(&(p.name().into(), v.clone())) {
                        Some((p, tv)) => (p.clone(), tv.clone()),
                        None => {
                            let tv = ToolVersionRequest::new(p.name().into(), &v)
                                .resolve(config, p.clone(), Default::default(), false)
                                .unwrap();
                            (p.clone(), tv)
                        }
                    }
                }))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();

        Ok(versions)
    }
    pub fn list_versions_by_plugin(
        &self,
        config: &Config,
    ) -> Vec<(Arc<dyn Plugin>, &Vec<ToolVersion>)> {
        self.versions
            .iter()
            .map(|(p, v)| {
                let p = config.plugins.get(p).unwrap();
                (p.clone(), &v.versions)
            })
            .collect()
    }
    pub fn list_current_versions(&self, config: &Config) -> Vec<(Arc<dyn Plugin>, ToolVersion)> {
        self.list_versions_by_plugin(config)
            .iter()
            .flat_map(|(p, v)| v.iter().map(|v| (p.clone(), v.clone())))
            .collect()
    }
    pub fn list_current_installed_versions(
        &self,
        config: &Config,
    ) -> Vec<(Arc<dyn Plugin>, ToolVersion)> {
        self.list_current_versions(config)
            .into_iter()
            .filter(|(p, v)| p.is_version_installed(v))
            .collect()
    }
    pub fn list_outdated_versions(
        &self,
        config: &Config,
    ) -> Vec<(Arc<dyn Plugin>, ToolVersion, String)> {
        self.list_current_versions(config)
            .into_iter()
            .filter_map(|(t, tv)| {
                if t.symlink_path(&tv).is_some() {
                    // do not consider symlinked versions to be outdated
                    return None;
                }
                let latest = match tv.latest_version(config, t.clone()) {
                    Ok(latest) => latest,
                    Err(e) => {
                        warn!("Error getting latest version for {t}: {e:#}");
                        return None;
                    }
                };
                if !t.is_version_installed(&tv) || tv.version != latest {
                    Some((t, tv, latest))
                } else {
                    None
                }
            })
            .collect()
    }
    pub fn env_with_path(&self, config: &Config) -> BTreeMap<String, String> {
        let mut path_env = PathEnv::from_iter(env::PATH.clone());
        for p in config.path_dirs.clone() {
            path_env.add(p);
        }
        for p in self.list_paths(config) {
            path_env.add(p);
        }
        let mut env = self.env(config);
        if let Some(path) = env.get("PATH") {
            path_env.add(PathBuf::from(path));
        }
        env.insert("PATH".to_string(), path_env.to_string());
        env
    }
    pub fn env(&self, config: &Config) -> BTreeMap<String, String> {
        let entries = self
            .list_current_installed_versions(config)
            .into_par_iter()
            .flat_map(|(p, tv)| match p.exec_env(config, self, &tv) {
                Ok(env) => env.into_iter().collect(),
                Err(e) => {
                    warn!("Error running exec-env: {:#}", e);
                    Vec::new()
                }
            })
            .collect::<Vec<(String, String)>>();
        let add_paths = entries
            .iter()
            .filter(|(k, _)| k == "RTX_ADD_PATH")
            .map(|(_, v)| v)
            .join(":");
        let mut entries: BTreeMap<String, String> = entries
            .into_iter()
            .filter(|(k, _)| k != "RTX_ADD_PATH")
            .filter(|(k, _)| !k.starts_with("RTX_TOOL_OPTS__"))
            .rev()
            .collect();
        if !add_paths.is_empty() {
            entries.insert("PATH".to_string(), add_paths);
        }
        entries.extend(config.env.clone());
        entries
    }
    pub fn list_paths(&self, config: &Config) -> Vec<PathBuf> {
        self.list_current_installed_versions(config)
            .into_par_iter()
            .flat_map(|(p, tv)| match p.list_bin_paths(config, self, &tv) {
                Ok(paths) => paths,
                Err(e) => {
                    warn!("Error listing bin paths for {}: {:#}", tv, e);
                    Vec::new()
                }
            })
            .collect()
    }
    pub fn which(&self, config: &Config, bin_name: &str) -> Option<(Arc<dyn Plugin>, ToolVersion)> {
        self.list_current_installed_versions(config)
            .into_par_iter()
            .find_first(|(p, tv)| {
                if let Ok(x) = p.which(config, self, tv, bin_name) {
                    x.is_some()
                } else {
                    false
                }
            })
    }

    pub fn list_rtvs_with_bin(&self, config: &Config, bin_name: &str) -> Result<Vec<ToolVersion>> {
        Ok(self
            .list_installed_versions(config)?
            .into_par_iter()
            .filter(|(p, tv)| match p.which(config, self, tv, bin_name) {
                Ok(x) => x.is_some(),
                Err(e) => {
                    warn!("Error running which: {:#}", e);
                    false
                }
            })
            .map(|(_, tv)| tv)
            .collect())
    }
}

impl Display for Toolset {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        let plugins = &self
            .versions
            .iter()
            .map(|(_, v)| v.requests.iter().map(|(tvr, _)| tvr.to_string()).join(" "))
            .collect_vec();
        write!(f, "{}", plugins.join(", "))
    }
}
