//! Cleanup: a preview (`plan`) that lists exactly what would be deleted, and an executor that deletes
//! only those items, one by one, without `force`. The engine itself then refuses anything that became
//! used in the meantime (a container that was started, an image or volume that got a new user).
//!
//! Safety rules, all enforced here and covered by tests:
//!  * running, paused and restarting containers are never selected;
//!  * images used by any container that is kept are never selected;
//!  * volumes are only deleted when the caller passes the separate `confirm_volumes` flag;
//!  * named volumes are only selected when the administrator explicitly asks for them;
//!  * removing a container never removes its volumes (`v=false`).
use super::api::{short_id, Container, DiskUsage, Engine, Image, Network, Volume};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Category {
    StoppedContainers,
    DanglingImages,
    UnusedImages,
    UnusedNetworks,
    UnusedVolumes,
    BuildCache,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::StoppedContainers => "Stopped containers",
            Category::DanglingImages => "Dangling images",
            Category::UnusedImages => "Unused images",
            Category::UnusedNetworks => "Unused networks",
            Category::UnusedVolumes => "Unused volumes",
            Category::BuildCache => "Build cache",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum VolumeScope {
    #[default]
    None,
    /// What `docker volume prune` removes by default since Docker 23.
    Anonymous,
    /// Also named volumes (`docker volume prune --all`).
    All,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Selection {
    pub stopped_containers: bool,
    pub dangling_images: bool,
    /// All images not used by a kept container (`prune -a`); implies dangling ones.
    pub unused_images: bool,
    pub unused_networks: bool,
    pub volumes: VolumeScope,
    pub build_cache: bool,
}

impl Selection {
    /// `docker system prune`
    pub fn system_prune() -> Self {
        Self { stopped_containers: true, dangling_images: true, unused_networks: true, build_cache: true, ..Default::default() }
    }
    /// `docker system prune -a`
    pub fn system_prune_all() -> Self {
        Self { unused_images: true, ..Self::system_prune() }
    }
    /// `docker system prune --volumes` (anonymous volumes, as current Docker does)
    pub fn system_prune_volumes() -> Self {
        Self { volumes: VolumeScope::Anonymous, ..Self::system_prune() }
    }
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    pub cat: Category,
    /// What is passed to the engine (container/network ID, image tag or ID, volume name).
    pub target: String,
    pub name: String,
    pub detail: String,
    /// Estimated bytes freed; `None` when unmeasurable.
    pub size: Option<u64>,
    /// Shown prominently in the preview (e.g. "volume holds 1.2 GiB of data").
    pub warning: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub selection: Selection,
    pub items: Vec<Item>,
    /// Things that matched a category but are protected, with the reason.
    pub protected: Vec<(String, String)>,
}

impl Plan {
    pub fn estimate(&self) -> u64 {
        self.items.iter().filter_map(|i| i.size).sum()
    }
    pub fn count(&self, c: Category) -> usize {
        self.items.iter().filter(|i| i.cat == c).count()
    }
    pub fn bytes(&self, c: Category) -> u64 {
        self.items.iter().filter(|i| i.cat == c).filter_map(|i| i.size).sum()
    }
    pub fn has_volumes(&self) -> bool {
        self.count(Category::UnusedVolumes) > 0
    }
}

/// Everything `plan` looks at; normally fetched fresh right before the preview.
pub struct Inputs<'a> {
    pub containers: &'a [Container],
    pub images: &'a [Image],
    pub networks: &'a [Network],
    pub volumes: &'a [Volume],
    pub df: Option<&'a DiskUsage>,
}

fn pos(v: i64) -> Option<u64> {
    u64::try_from(v).ok()
}

pub fn plan(sel: Selection, inp: &Inputs) -> Plan {
    let mut p = Plan { selection: sel, ..Default::default() };
    // the engine omits SizeRw when it is 0; a measured SizeRootFs tells the two cases apart
    let df_sizes: HashMap<&str, i64> = inp.df.map(|d| d.containers.iter().filter_map(|c| c.size_rw.or(c.size_root_fs.map(|_| 0)).map(|s| (c.id.as_str(), s))).collect()).unwrap_or_default();
    let df_images: HashMap<&str, &Image> = inp.df.map(|d| d.images.iter().map(|i| (i.id.as_str(), i)).collect()).unwrap_or_default();
    let df_volumes: HashMap<&str, &Volume> = inp.df.map(|d| d.volumes.iter().map(|v| (v.name.as_str(), v)).collect()).unwrap_or_default();

    // ---- containers
    let mut removed: HashSet<&str> = HashSet::new();
    for c in inp.containers {
        if !sel.stopped_containers {
            break;
        }
        if c.is_stopped() {
            removed.insert(&c.id);
            p.items.push(Item { cat: Category::StoppedContainers, target: c.id.clone(), name: c.name(), detail: format!("{} · {} · {}", short_id(&c.id), c.image, c.status), size: df_sizes.get(c.id.as_str()).copied().and_then(pos), warning: None });
        } else {
            p.protected.push((c.name(), format!("container is {}", c.state)));
        }
    }
    let kept: Vec<&Container> = inp.containers.iter().filter(|c| !removed.contains(c.id.as_str())).collect();

    // ---- images
    let used_by_kept: HashSet<&str> = kept.iter().map(|c| c.image_id.as_str()).collect();
    let used_by_running: HashSet<&str> = inp.containers.iter().filter(|c| !c.is_stopped()).map(|c| c.image_id.as_str()).collect();
    if sel.dangling_images || sel.unused_images {
        for i in inp.images {
            let dangling = i.dangling();
            if !(sel.unused_images || dangling) {
                continue;
            }
            let label = if dangling { format!("<none> {}", short_id(&i.id)) } else { i.tags().join(", ") };
            if used_by_running.contains(i.id.as_str()) {
                p.protected.push((label, "image used by a running or paused container".into()));
                continue;
            }
            if used_by_kept.contains(i.id.as_str()) {
                p.protected.push((label, "image used by a container that is kept".into()));
                continue;
            }
            // shared layers stay as long as another image needs them
            let size = df_images.get(i.id.as_str()).map(|d| d.size - d.shared_size.max(0)).unwrap_or(i.size - i.shared_size.max(0));
            let cat = if dangling { Category::DanglingImages } else { Category::UnusedImages };
            // A tagged image is removed by deleting each tag; the last one removes the image itself.
            let tags = i.tags();
            if tags.is_empty() {
                p.items.push(Item { cat, target: i.id.clone(), name: label, detail: short_id(&i.id).into(), size: pos(size), warning: None });
            } else {
                for (k, t) in tags.iter().enumerate() {
                    p.items.push(Item { cat, target: t.to_string(), name: t.to_string(), detail: short_id(&i.id).into(), size: if k + 1 == tags.len() { pos(size) } else { Some(0) }, warning: None });
                }
            }
        }
    }

    // ---- networks
    if sel.unused_networks {
        let attached: HashSet<&str> = kept.iter().flat_map(|c| c.network_settings.networks.values().map(|e| e.network_id.as_str()).chain(c.network_settings.networks.keys().map(String::as_str))).collect();
        for n in inp.networks {
            if n.predefined() {
                continue;
            }
            if n.scope == "swarm" {
                p.protected.push((n.name.clone(), "swarm network (managed by the swarm)".into()));
            } else if attached.contains(n.id.as_str()) || attached.contains(n.name.as_str()) || !n.containers.is_empty() {
                p.protected.push((n.name.clone(), "containers are attached".into()));
            } else {
                p.items.push(Item { cat: Category::UnusedNetworks, target: n.id.clone(), name: n.name.clone(), detail: format!("{} · {}", n.driver, n.subnets()), size: None, warning: None });
            }
        }
    }

    // ---- volumes
    if sel.volumes != VolumeScope::None {
        let mounted: HashSet<&str> = kept.iter().flat_map(|c| c.mounts.iter().filter(|m| m.kind == "volume").map(|m| m.name.as_str())).collect();
        for v in inp.volumes {
            let d = df_volumes.get(v.name.as_str()).copied().unwrap_or(v);
            let refs = d.usage_data.as_ref().map(|u| u.ref_count).unwrap_or(-1);
            // ref_count counts containers about to be removed too, so the kept containers' mounts decide
            if mounted.contains(v.name.as_str()) || (refs > 0 && !sel.stopped_containers) {
                p.protected.push((v.name.clone(), "volume is used by a container".into()));
                continue;
            }
            if sel.volumes == VolumeScope::Anonymous && !v.anonymous() {
                p.protected.push((v.name.clone(), "named volume (select \"include named volumes\" to remove)".into()));
                continue;
            }
            let size = d.size();
            let warning = match size {
                Some(0) => None,
                Some(b) => Some(format!("holds {} of data — deleted permanently", crate::linux::human_bytes(b))),
                None => Some("size unknown (driver cannot measure it); may hold data".into()),
            };
            p.items.push(Item { cat: Category::UnusedVolumes, target: v.name.clone(), name: if v.anonymous() { format!("{} (anonymous)", short_id(&v.name)) } else { v.name.clone() }, detail: format!("{} · {}", v.driver, v.mountpoint), size, warning });
        }
    }

    // ---- build cache (removed in one call, listed for the preview)
    if sel.build_cache {
        if let Some(df) = inp.df {
            for b in df.build_cache.iter() {
                if b.in_use {
                    p.protected.push((short_id(&b.id).into(), "build cache record in use".into()));
                } else {
                    p.items.push(Item { cat: Category::BuildCache, target: b.id.clone(), name: format!("{} {}", b.kind, short_id(&b.id)), detail: b.description.chars().take(80).collect(), size: if b.shared { Some(0) } else { pos(b.size) }, warning: None });
                }
            }
        }
    }
    p
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub cat: Category,
    pub name: String,
    pub result: Result<(), String>,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub outcomes: Vec<Outcome>,
    /// Bytes reported freed: estimates for deleted items plus the engine's own figure for build cache.
    pub reclaimed: u64,
    pub host_free_before: Option<u64>,
    pub host_free_after: Option<u64>,
}

impl Report {
    pub fn ok(&self) -> usize {
        self.outcomes.iter().filter(|o| o.result.is_ok()).count()
    }
    pub fn failed(&self) -> usize {
        self.outcomes.len() - self.ok()
    }
}

/// The deletion primitives `execute` needs (the engine, or a fake in tests).
pub trait Remover {
    fn remove_container(&self, id: &str) -> Result<(), String>;
    fn remove_image(&self, r: &str) -> Result<(), String>;
    fn remove_network(&self, id: &str) -> Result<(), String>;
    fn remove_volume(&self, name: &str) -> Result<(), String>;
    fn prune_build_cache(&self) -> Result<u64, String>;
}

impl Remover for Engine {
    fn remove_container(&self, id: &str) -> Result<(), String> {
        Engine::remove_container(self, id, false).map_err(|e| e.to_string())
    }
    fn remove_image(&self, r: &str) -> Result<(), String> {
        Engine::remove_image(self, r, false).map(|_| ()).map_err(|e| e.to_string())
    }
    fn remove_network(&self, id: &str) -> Result<(), String> {
        Engine::remove_network(self, id).map_err(|e| e.to_string())
    }
    fn remove_volume(&self, name: &str) -> Result<(), String> {
        Engine::remove_volume(self, name).map_err(|e| e.to_string())
    }
    fn prune_build_cache(&self) -> Result<u64, String> {
        Engine::prune_build_cache(self, true).map(|(_, b)| b).map_err(|e| e.to_string())
    }
}

/// Delete exactly the planned items, in dependency order. Volume items are skipped unless `confirm_volumes`.
pub fn execute(r: &dyn Remover, plan: &Plan, confirm_volumes: bool, free: &dyn Fn() -> Option<u64>) -> Report {
    let mut rep = Report { host_free_before: free(), ..Default::default() };
    let order = [Category::StoppedContainers, Category::DanglingImages, Category::UnusedImages, Category::UnusedNetworks, Category::UnusedVolumes];
    for cat in order {
        for it in plan.items.iter().filter(|i| i.cat == cat) {
            let result = match cat {
                Category::StoppedContainers => r.remove_container(&it.target),
                Category::DanglingImages | Category::UnusedImages => r.remove_image(&it.target),
                Category::UnusedNetworks => r.remove_network(&it.target),
                Category::UnusedVolumes if !confirm_volumes => Err("skipped: volume deletion was not explicitly confirmed".into()),
                Category::UnusedVolumes => r.remove_volume(&it.target),
                Category::BuildCache => unreachable!(),
            };
            if result.is_ok() {
                rep.reclaimed += it.size.unwrap_or(0);
            }
            super::audit::record(&format!("cleanup: remove {}", cat.label().to_lowercase()), &it.name, &result);
            rep.outcomes.push(Outcome { cat, name: it.name.clone(), result, size: it.size });
        }
    }
    if plan.selection.build_cache && plan.count(Category::BuildCache) > 0 {
        let result = r.prune_build_cache();
        let size = result.as_ref().ok().copied();
        rep.reclaimed += size.unwrap_or(0);
        let result = result.map(|_| ());
        super::audit::record("cleanup: prune build cache", &format!("{} records", plan.count(Category::BuildCache)), &result);
        rep.outcomes.push(Outcome { cat: Category::BuildCache, name: format!("{} build cache records", plan.count(Category::BuildCache)), result, size });
    }
    rep.host_free_after = free();
    rep
}
