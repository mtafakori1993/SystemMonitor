//! Docker management: Engine API client, cleanup planner, daemon/proxy configuration, audit, live data.
//!
//! Security model (see README "Docker management"):
//!  * Everything talks to the Engine API over its Unix socket; the socket is never exposed elsewhere.
//!  * Access to that socket is Docker's own administrator boundary (root or the `docker` group, which is
//!    root-equivalent). Without it the module stays read-only and explains why; nothing is escalated.
//!  * Host-level changes (daemon.json, restarting dockerd) need PolicyKit administrator authentication
//!    and run in a separate validating helper process (`config::helper_main`).
//!  * Destructive operations are confirmed in the UI and written to the audit log (`audit`).
pub mod api;
pub mod audit;
pub mod cleanup;
pub mod cli;
pub mod config;
pub mod http;
pub mod live;
#[cfg(feature = "gui")]
pub mod ui;
#[cfg(test)]
mod tests;

use api::{Container, Engine, Image, Info, Network, Version, Volume};
pub use http::Error;
use std::collections::HashMap;

/// Everything the lists show, fetched together on a worker thread.
#[derive(Clone, Default)]
pub struct Inventory {
    pub version: Version,
    pub info: Info,
    pub containers: Vec<Container>,
    pub images: Vec<Image>,
    pub volumes: Vec<Volume>,
    pub networks: Vec<Network>,
    pub fetched: u64,
}

impl Inventory {
    pub fn fetch(e: &Engine) -> Result<Inventory, Error> {
        Ok(Inventory { version: e.version()?, info: e.info()?, containers: e.containers()?, images: e.images()?, volumes: e.volumes()?, networks: e.networks()?, fetched: crate::linux::now_secs() })
    }

    /// Image ID → names of the containers created from it.
    pub fn image_users(&self) -> HashMap<&str, Vec<String>> {
        let mut m: HashMap<&str, Vec<String>> = HashMap::new();
        for c in &self.containers {
            m.entry(c.image_id.as_str()).or_default().push(c.name());
        }
        m
    }

    /// Volume name → names of the containers mounting it.
    pub fn volume_users(&self) -> HashMap<&str, Vec<String>> {
        let mut m: HashMap<&str, Vec<String>> = HashMap::new();
        for c in &self.containers {
            for mt in c.mounts.iter().filter(|m| m.kind == "volume") {
                m.entry(mt.name.as_str()).or_default().push(c.name());
            }
        }
        m
    }

    /// Network ID → (container ID, container name) attached to it.
    pub fn network_users(&self) -> HashMap<String, Vec<(String, String)>> {
        let mut m: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for c in &self.containers {
            for (name, ep) in &c.network_settings.networks {
                let key = if ep.network_id.is_empty() { self.networks.iter().find(|n| &n.name == name).map(|n| n.id.clone()).unwrap_or_default() } else { ep.network_id.clone() };
                m.entry(key).or_default().push((c.id.clone(), c.name()));
            }
        }
        m
    }

    pub fn count_state(&self, state: &str) -> usize {
        self.containers.iter().filter(|c| c.state == state).count()
    }
}

/// Images sharing a repository: older versions and tags that point at the same image.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImageFlags {
    /// Another image of the same repository is newer.
    pub outdated: bool,
    /// This image ID carries more than one tag.
    pub duplicate_tags: bool,
}

pub fn image_flags(images: &[Image]) -> HashMap<String, ImageFlags> {
    let mut newest: HashMap<String, i64> = HashMap::new();
    for i in images {
        for (repo, _) in i.repo_tag_pairs() {
            if repo != "<none>" {
                let e = newest.entry(repo).or_insert(i.created);
                *e = (*e).max(i.created);
            }
        }
    }
    images
        .iter()
        .map(|i| {
            let outdated = i.repo_tag_pairs().iter().any(|(r, _)| newest.get(r).map(|n| *n > i.created).unwrap_or(false));
            (i.id.clone(), ImageFlags { outdated, duplicate_tags: i.tags().len() > 1 })
        })
        .collect()
}

/// (total, available) bytes of the filesystem holding Docker's data root.
pub fn host_disk(root: &str) -> Option<(u64, u64)> {
    let p = if root.is_empty() { "/var/lib/docker" } else { root };
    // the data root itself is usually not readable for users; its filesystem is the same as its parent's
    let mut path = std::path::Path::new(p);
    loop {
        if let Some((total, _, avail)) = crate::storage::statvfs(&path.display().to_string()) {
            return Some((total, avail));
        }
        path = path.parent()?;
    }
}

/// Connection status, distinguishing "not installed", "not running" and "not allowed".
#[derive(Clone, Debug)]
pub enum Access {
    Ready(Engine),
    NotInstalled,
    NotRunning(String),
    Denied(String),
    Failed(String),
}

pub fn probe() -> Access {
    match Engine::connect() {
        Ok(e) => Access::Ready(e),
        Err(Error::Denied(s)) => Access::Denied(s),
        Err(Error::NotRunning(s)) => {
            if crate::linux::which("dockerd").is_none() && crate::linux::which("docker").is_none() {
                Access::NotInstalled
            } else {
                Access::NotRunning(s)
            }
        }
        Err(e) => Access::Failed(e.to_string()),
    }
}
