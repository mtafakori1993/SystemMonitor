//! Typed Docker Engine API: socket discovery, version negotiation, models, and every call the module makes.
//! Every name/ID that ends up in a URL is validated and percent-encoded first.
use super::http::{self, enc, Error, Stream};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Newest API version this module was written against; older daemons are spoken to in their own version.
pub const MAX_API: (u32, u32) = (1, 47);
/// One-shot stats (1.41) is the newest feature relied on.
pub const MIN_API: (u32, u32) = (1, 41);

const T_SHORT: Duration = Duration::from_secs(15);
const T_LONG: Duration = Duration::from_secs(120);

/// `null` → `Default` (the engine sends `null` for many empty lists and maps).
fn nd<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

// ---------------------------------------------------------------- models

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct Version {
    pub version: String,
    pub api_version: String,
    #[serde(rename = "MinAPIVersion")]
    pub min_api_version: String,
    pub os: String,
    pub arch: String,
    pub kernel_version: String,
    #[serde(deserialize_with = "nd")]
    pub components: Vec<Component>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct Component {
    pub name: String,
    pub version: String,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct Info {
    pub name: String,
    pub containers: u64,
    pub containers_running: u64,
    pub containers_paused: u64,
    pub containers_stopped: u64,
    pub images: u64,
    pub driver: String,
    pub docker_root_dir: String,
    pub logging_driver: String,
    pub cgroup_driver: String,
    pub cgroup_version: String,
    pub operating_system: String,
    #[serde(rename = "NCPU")]
    pub ncpu: u64,
    pub mem_total: u64,
    /// The engine replaces credentials in these with "xxxxx" itself.
    pub http_proxy: String,
    pub https_proxy: String,
    pub no_proxy: String,
    pub live_restore_enabled: bool,
    #[serde(deserialize_with = "nd")]
    pub registry_config: RegistryConfig,
    #[serde(deserialize_with = "nd")]
    pub warnings: Vec<String>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct RegistryConfig {
    #[serde(deserialize_with = "nd")]
    pub mirrors: Vec<String>,
    #[serde(rename = "InsecureRegistryCIDRs", deserialize_with = "nd")]
    pub insecure_registry_cidrs: Vec<String>,
}

#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct Port {
    #[serde(rename = "IP")]
    pub ip: String,
    pub private_port: u16,
    pub public_port: u16,
    #[serde(rename = "Type")]
    pub kind: String,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct Mount {
    #[serde(rename = "Type")]
    pub kind: String,
    pub name: String,
    pub source: String,
    pub destination: String,
    #[serde(rename = "RW")]
    pub rw: bool,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct Endpoint {
    #[serde(rename = "NetworkID")]
    pub network_id: String,
    #[serde(rename = "IPAddress")]
    pub ip_address: String,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct NetSettings {
    #[serde(deserialize_with = "nd")]
    pub networks: HashMap<String, Endpoint>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct HealthSummary {
    pub status: String,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct Container {
    pub id: String,
    #[serde(deserialize_with = "nd")]
    pub names: Vec<String>,
    pub image: String,
    #[serde(rename = "ImageID")]
    pub image_id: String,
    pub command: String,
    pub created: i64,
    #[serde(deserialize_with = "nd")]
    pub ports: Vec<Port>,
    pub size_rw: Option<i64>,
    pub size_root_fs: Option<i64>,
    #[serde(deserialize_with = "nd")]
    pub labels: HashMap<String, String>,
    /// created, running, paused, restarting, removing, exited, dead
    pub state: String,
    pub status: String,
    pub health: Option<HealthSummary>,
    #[serde(deserialize_with = "nd")]
    pub mounts: Vec<Mount>,
    #[serde(deserialize_with = "nd")]
    pub network_settings: NetSettings,
}

impl Container {
    pub fn name(&self) -> String {
        self.names.first().map(|n| n.trim_start_matches('/').to_string()).unwrap_or_else(|| short_id(&self.id).to_string())
    }
    /// Health from the summary (API ≥ 1.52) or from the status text ("Up 3 minutes (healthy)").
    pub fn health(&self) -> &str {
        if let Some(h) = self.health.as_ref().filter(|h| !h.status.is_empty() && h.status != "none") {
            return &h.status;
        }
        let s = self.status.as_str();
        if s.contains("(healthy)") {
            "healthy"
        } else if s.contains("(unhealthy)") {
            "unhealthy"
        } else if s.contains("(health: starting)") {
            "starting"
        } else {
            ""
        }
    }
    pub fn is_running(&self) -> bool {
        matches!(self.state.as_str(), "running" | "restarting")
    }
    /// Stopped in the `docker container prune` sense (paused and running containers are not).
    pub fn is_stopped(&self) -> bool {
        matches!(self.state.as_str(), "exited" | "created" | "dead")
    }
    pub fn ports_text(&self) -> String {
        let mut v: Vec<String> = self
            .ports
            .iter()
            .map(|p| if p.public_port > 0 { format!("{}:{}->{}/{}", if p.ip.is_empty() { "*" } else { &p.ip }, p.public_port, p.private_port, p.kind) } else { format!("{}/{}", p.private_port, p.kind) })
            .collect();
        v.dedup();
        v.join(", ")
    }
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct Image {
    pub id: String,
    pub parent_id: String,
    #[serde(deserialize_with = "nd")]
    pub repo_tags: Vec<String>,
    #[serde(deserialize_with = "nd")]
    pub repo_digests: Vec<String>,
    pub created: i64,
    pub size: i64,
    pub shared_size: i64,
    /// Number of containers using the image; -1 when the engine did not compute it (only `/system/df` does).
    pub containers: i64,
    #[serde(deserialize_with = "nd")]
    pub labels: HashMap<String, String>,
}

impl Image {
    pub fn tags(&self) -> Vec<&str> {
        self.repo_tags.iter().map(String::as_str).filter(|t| *t != "<none>:<none>").collect()
    }
    pub fn dangling(&self) -> bool {
        self.tags().is_empty()
    }
    /// (repository, tag) of each tag; dangling images yield ("<none>", "<none>").
    pub fn repo_tag_pairs(&self) -> Vec<(String, String)> {
        let t = self.tags();
        if t.is_empty() {
            let repo = self.repo_digests.first().and_then(|d| d.split('@').next()).unwrap_or("<none>");
            return vec![(repo.to_string(), "<none>".into())];
        }
        t.iter().map(|t| split_tag(t)).collect()
    }
}

/// "registry:5000/a/b:tag" → ("registry:5000/a/b", "tag"); the colon of a registry port is not a tag separator.
pub fn split_tag(r: &str) -> (String, String) {
    match r.rfind(':') {
        Some(i) if !r[i..].contains('/') => (r[..i].to_string(), r[i + 1..].to_string()),
        _ => (r.to_string(), "latest".to_string()),
    }
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct UsageData {
    /// Bytes, or -1 when the driver cannot measure it.
    pub size: i64,
    pub ref_count: i64,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct Volume {
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
    pub created_at: String,
    pub scope: String,
    #[serde(deserialize_with = "nd")]
    pub labels: HashMap<String, String>,
    pub usage_data: Option<UsageData>,
}

impl Volume {
    /// Anonymous volumes are created implicitly for a container (`VOLUME` in a Dockerfile, `-v /path`).
    pub fn anonymous(&self) -> bool {
        self.labels.contains_key("com.docker.volume.anonymous") || (self.name.len() == 64 && self.name.bytes().all(|b| b.is_ascii_hexdigit()))
    }
    pub fn size(&self) -> Option<u64> {
        self.usage_data.as_ref().and_then(|u| u64::try_from(u.size).ok())
    }
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct IpamConfig {
    pub subnet: String,
    pub gateway: String,
    #[serde(rename = "IPRange")]
    pub ip_range: String,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct Ipam {
    pub driver: String,
    #[serde(deserialize_with = "nd")]
    pub config: Vec<IpamConfig>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct NetContainer {
    pub name: String,
    #[serde(rename = "IPv4Address")]
    pub ipv4: String,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct Network {
    pub name: String,
    pub id: String,
    pub created: String,
    pub scope: String,
    pub driver: String,
    #[serde(rename = "EnableIPv6")]
    pub enable_ipv6: bool,
    #[serde(rename = "IPAM", deserialize_with = "nd")]
    pub ipam: Ipam,
    pub internal: bool,
    pub attachable: bool,
    /// Filled by `inspect`, empty in the list.
    #[serde(deserialize_with = "nd")]
    pub containers: HashMap<String, NetContainer>,
    #[serde(deserialize_with = "nd")]
    pub labels: HashMap<String, String>,
}

impl Network {
    /// bridge/host/none always exist and cannot be removed.
    pub fn predefined(&self) -> bool {
        matches!(self.name.as_str(), "bridge" | "host" | "none") || self.labels.contains_key("com.docker.network.predefined")
    }
    pub fn subnets(&self) -> String {
        self.ipam.config.iter().map(|c| c.subnet.clone()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ")
    }
    pub fn gateways(&self) -> String {
        self.ipam.config.iter().map(|c| c.gateway.clone()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ")
    }
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct BuildCache {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Type")]
    pub kind: String,
    pub description: String,
    pub in_use: bool,
    pub shared: bool,
    pub size: i64,
    pub last_used_at: Option<String>,
    pub usage_count: i64,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct DiskUsage {
    pub layers_size: i64,
    #[serde(deserialize_with = "nd")]
    pub images: Vec<Image>,
    #[serde(deserialize_with = "nd")]
    pub containers: Vec<Container>,
    #[serde(deserialize_with = "nd")]
    pub volumes: Vec<Volume>,
    #[serde(deserialize_with = "nd")]
    pub build_cache: Vec<BuildCache>,
}

/// Totals per category, computed the way `docker system df` does.
#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub struct DfTotals {
    pub images: u64,
    pub images_reclaimable: u64,
    pub containers: u64,
    pub containers_reclaimable: u64,
    pub volumes: u64,
    pub volumes_reclaimable: u64,
    pub build_cache: u64,
    pub build_cache_reclaimable: u64,
}

impl DfTotals {
    pub fn total(&self) -> u64 {
        self.images + self.containers + self.volumes + self.build_cache
    }
    pub fn reclaimable(&self) -> u64 {
        self.images_reclaimable + self.containers_reclaimable + self.volumes_reclaimable + self.build_cache_reclaimable
    }
}

fn pos(v: i64) -> u64 {
    v.max(0) as u64
}

impl DiskUsage {
    pub fn totals(&self) -> DfTotals {
        let mut t = DfTotals::default();
        // Layers shared between images are counted once (LayersSize); fall back to the sum when it is absent.
        t.images = if self.layers_size > 0 { pos(self.layers_size) } else { self.images.iter().map(|i| pos(i.size)).sum() };
        let unused: u64 = self.images.iter().filter(|i| i.containers == 0).map(|i| pos(i.size - i.shared_size.max(0))).sum();
        t.images_reclaimable = unused.min(t.images);
        t.containers = self.containers.iter().map(|c| pos(c.size_rw.unwrap_or(0))).sum();
        t.containers_reclaimable = self.containers.iter().filter(|c| !c.is_running() && c.state != "paused").map(|c| pos(c.size_rw.unwrap_or(0))).sum();
        t.volumes = self.volumes.iter().filter_map(Volume::size).sum();
        t.volumes_reclaimable = self.volumes.iter().filter(|v| v.usage_data.as_ref().map(|u| u.ref_count == 0).unwrap_or(false)).filter_map(Volume::size).sum();
        t.build_cache = self.build_cache.iter().filter(|b| !b.shared).map(|b| pos(b.size)).sum();
        t.build_cache_reclaimable = self.build_cache.iter().filter(|b| !b.shared && !b.in_use).map(|b| pos(b.size)).sum();
        t
    }
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct Actor {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(deserialize_with = "nd")]
    pub attributes: HashMap<String, String>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Event {
    #[serde(rename = "Type")]
    pub kind: String,
    #[serde(rename = "Action")]
    pub action: String,
    #[serde(rename = "Actor", deserialize_with = "nd")]
    pub actor: Actor,
    pub scope: String,
    pub time: i64,
    #[serde(rename = "timeNano")]
    pub time_nano: i64,
}

impl Event {
    pub fn name(&self) -> String {
        self.actor.attributes.get("name").cloned().unwrap_or_else(|| short_id(&self.actor.id).to_string())
    }
    /// Lifecycle events that change what the lists show.
    pub fn changes_inventory(&self) -> bool {
        let a = self.action.split(':').next().unwrap_or("");
        matches!(self.kind.as_str(), "container" | "image" | "volume" | "network")
            && !matches!(a, "exec_create" | "exec_start" | "exec_die" | "attach" | "resize" | "top" | "export" | "copy" | "archive-path" | "extract-to-dir" | "health_status")
    }
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "PascalCase", default)]
pub struct HistoryEntry {
    pub id: String,
    pub created: i64,
    pub created_by: String,
    #[serde(deserialize_with = "nd")]
    pub tags: Vec<String>,
    pub size: i64,
    pub comment: String,
}

pub fn short_id(id: &str) -> &str {
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    &id[..id.len().min(12)]
}

// ---------------------------------------------------------------- validation

/// Container, volume and network names/IDs: `[a-zA-Z0-9][a-zA-Z0-9_.-]*` (Docker's own rule), ≤ 128 characters.
pub fn valid_object_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.as_bytes()[0].is_ascii_alphanumeric() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn check_name(kind: &str, s: &str) -> Result<(), Error> {
    if valid_object_name(s) { Ok(()) } else { Err(Error::Invalid(format!("invalid {kind} name or ID '{}'", s.chars().take(64).collect::<String>()))) }
}

/// Image reference: `[host[:port]/]path[:tag][@sha256:digest]` with Docker's grammar for each part.
pub fn valid_image_ref(r: &str) -> bool {
    if r.is_empty() || r.len() > 255 || r.contains("..") || r.contains("//") {
        return false;
    }
    let (rest, digest) = match r.split_once('@') {
        Some((a, d)) => (a, Some(d)),
        None => (r, None),
    };
    if let Some(d) = digest {
        let Some(hex) = d.strip_prefix("sha256:") else { return false };
        if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return false;
        }
    }
    let (name, tag) = match rest.rfind(':') {
        Some(i) if !rest[i..].contains('/') => (&rest[..i], Some(&rest[i + 1..])),
        _ => (rest, None),
    };
    if let Some(t) = tag {
        let b = t.as_bytes();
        if b.is_empty() || b.len() > 128 || !(b[0].is_ascii_alphanumeric() || b[0] == b'_') || !b.iter().all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(c)) {
            return false;
        }
    }
    let parts: Vec<&str> = name.split('/').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return false;
    }
    let first_is_host = parts.len() > 1 && (parts[0].contains('.') || parts[0].contains(':') || parts[0] == "localhost");
    for (i, p) in parts.iter().enumerate() {
        if i == 0 && first_is_host {
            let (h, port) = match p.split_once(':') {
                Some((h, po)) => (h, Some(po)),
                None => (*p, None),
            };
            if !crate::linux::valid_host(h) || port.map(|po| crate::linux::parse_port(po).is_none()).unwrap_or(false) {
                return false;
            }
            continue;
        }
        // path component: lowercase alphanumerics separated by '.', '_', '__' or runs of '-'
        let b = p.as_bytes();
        if !b[0].is_ascii_lowercase() && !b[0].is_ascii_digit() || !b[b.len() - 1].is_ascii_lowercase() && !b[b.len() - 1].is_ascii_digit() {
            return false;
        }
        if !b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(c)) || p.contains("___") || p.contains("._") || p.contains("_.") || p.contains(".-") || p.contains("-.") {
            return false;
        }
    }
    true
}

/// Image references go into the URL path; '/', ':' and '@' are kept literal (the engine routes on them).
fn image_path(r: &str) -> Result<String, Error> {
    let id = r.strip_prefix("sha256:").unwrap_or(r);
    let is_id = id.len() >= 12 && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit());
    if !is_id && !valid_image_ref(r) {
        return Err(Error::Invalid(format!("invalid image reference '{}'", r.chars().take(80).collect::<String>())));
    }
    Ok(r.split('/').map(|seg| seg.split(':').map(|x| x.split('@').map(enc).collect::<Vec<_>>().join("@")).collect::<Vec<_>>().join(":")).collect::<Vec<_>>().join("/"))
}

/// argv for exec: split on whitespace (no shell is involved; the engine execs the argv directly).
pub fn parse_exec_cmd(s: &str) -> Result<Vec<String>, Error> {
    let s = s.trim();
    if s.is_empty() || s.len() > 512 || s.chars().any(|c| c.is_control()) {
        return Err(Error::Invalid("command must be 1-512 printable characters".into()));
    }
    Ok(s.split_whitespace().map(String::from).collect())
}

// ---------------------------------------------------------------- engine

#[derive(Clone, Debug)]
pub struct Engine {
    pub socket: PathBuf,
    /// Negotiated API version, e.g. "1.47".
    pub api: String,
}

/// Socket path: `DOCKER_HOST=unix://…`, then the system socket, then the rootless one.
pub fn socket_path() -> Result<PathBuf, Error> {
    if let Ok(h) = std::env::var("DOCKER_HOST") {
        if let Some(p) = h.strip_prefix("unix://") {
            return Ok(PathBuf::from(p));
        }
        if !h.is_empty() {
            return Err(Error::Invalid(format!("DOCKER_HOST={h}: only unix:// sockets are supported (a TCP endpoint would need TLS client certificates)")));
        }
    }
    let sys = PathBuf::from("/var/run/docker.sock");
    if sys.exists() {
        return Ok(sys);
    }
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(rt).join("docker.sock");
        if p.exists() {
            return Ok(p);
        }
    }
    Ok(sys)
}

fn parse_ver(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.trim().split_once('.')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Highest version both sides speak.
pub fn negotiate(server: &str) -> Result<String, Error> {
    let v = parse_ver(server).ok_or_else(|| Error::Protocol(format!("unknown API version '{server}'")))?;
    let use_v = v.min(MAX_API);
    if use_v < MIN_API {
        return Err(Error::Invalid(format!("Docker API {server} is too old (need {}.{} or newer, Docker 20.10+)", MIN_API.0, MIN_API.1)));
    }
    Ok(format!("{}.{}", use_v.0, use_v.1))
}

impl Engine {
    pub fn connect() -> Result<Engine, Error> {
        Self::at(socket_path()?)
    }

    pub fn at(socket: PathBuf) -> Result<Engine, Error> {
        let r = http::request(&socket, Duration::from_secs(5), "GET", "/version", None)?;
        if r.status != 200 {
            return Err(Error::Api { status: r.status, message: http::error_message(&r.body) });
        }
        let v: Version = serde_json::from_slice(&r.body).map_err(|e| Error::Protocol(e.to_string()))?;
        Ok(Engine { api: negotiate(&v.api_version)?, socket })
    }

    fn url(&self, p: &str) -> String {
        format!("/v{}{p}", self.api)
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>, t: Duration) -> Result<Vec<u8>, Error> {
        let b = body.map(|v| v.to_string().into_bytes());
        let r = http::request(&self.socket, t, method, &self.url(path), b.as_deref())?;
        if r.status >= 400 {
            return Err(Error::Api { status: r.status, message: http::error_message(&r.body) });
        }
        Ok(r.body)
    }

    fn get<T: DeserializeOwned>(&self, path: &str, t: Duration) -> Result<T, Error> {
        let b = self.call("GET", path, None, t)?;
        serde_json::from_slice(&b).map_err(|e| Error::Protocol(format!("{path}: {e}")))
    }

    fn json(&self, method: &str, path: &str, body: Option<&Value>, t: Duration) -> Result<Value, Error> {
        let b = self.call(method, path, body, t)?;
        if b.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&b).map_err(|e| Error::Protocol(format!("{path}: {e}")))
    }

    pub fn version(&self) -> Result<Version, Error> {
        self.get("/version", T_SHORT)
    }

    pub fn info(&self) -> Result<Info, Error> {
        self.get("/info", T_SHORT)
    }

    pub fn containers(&self) -> Result<Vec<Container>, Error> {
        self.get("/containers/json?all=1", T_SHORT)
    }

    pub fn images(&self) -> Result<Vec<Image>, Error> {
        self.get("/images/json", T_SHORT)
    }

    pub fn volumes(&self) -> Result<Vec<Volume>, Error> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct L {
            #[serde(rename = "Volumes", deserialize_with = "nd")]
            volumes: Vec<Volume>,
        }
        Ok(self.get::<L>("/volumes", T_SHORT)?.volumes)
    }

    pub fn networks(&self) -> Result<Vec<Network>, Error> {
        self.get("/networks", T_SHORT)
    }

    /// `/system/df` walks every layer and volume; it can take a while on large hosts.
    pub fn disk_usage(&self) -> Result<DiskUsage, Error> {
        self.get("/system/df", T_LONG)
    }

    // ---- containers

    pub fn container_action(&self, id: &str, action: &str) -> Result<(), Error> {
        check_name("container", id)?;
        let (path, t) = match action {
            "start" | "pause" | "unpause" => (format!("/containers/{}/{action}", enc(id)), T_SHORT),
            // the engine waits up to 10 s for a graceful stop before SIGKILL
            "stop" | "restart" => (format!("/containers/{}/{action}?t=10", enc(id)), Duration::from_secs(45)),
            _ => return Err(Error::Invalid(format!("unknown container action '{action}'"))),
        };
        match self.call("POST", &path, None, t) {
            // 304: already in the requested state
            Err(Error::Api { status: 304, .. }) => Ok(()),
            r => r.map(|_| ()),
        }
    }

    /// Never removes the container's anonymous volumes (`v=false`): volumes have their own confirmation.
    pub fn remove_container(&self, id: &str, force: bool) -> Result<(), Error> {
        check_name("container", id)?;
        self.call("DELETE", &format!("/containers/{}?v=false&force={force}", enc(id)), None, Duration::from_secs(45)).map(|_| ())
    }

    pub fn inspect_container(&self, id: &str) -> Result<Value, Error> {
        check_name("container", id)?;
        self.json("GET", &format!("/containers/{}/json", enc(id)), None, T_SHORT)
    }

    /// Single stats sample without the engine's 1 s wait; CPU % is computed from two consecutive samples.
    pub fn stats_once(&self, id: &str) -> Result<Value, Error> {
        check_name("container", id)?;
        self.json("GET", &format!("/containers/{}/stats?stream=false&one-shot=true", enc(id)), None, Duration::from_secs(10))
    }

    pub fn logs(&self, id: &str, tail: u32, follow: bool, timestamps: bool) -> Result<Stream, Error> {
        check_name("container", id)?;
        http::stream(&self.socket, T_SHORT, "GET", &self.url(&format!("/containers/{}/logs?stdout=1&stderr=1&tail={tail}&follow={follow}&timestamps={timestamps}", enc(id))), None)
    }

    pub fn exec_create(&self, id: &str, cmd: &[String], tty: bool) -> Result<String, Error> {
        check_name("container", id)?;
        let body = json!({"AttachStdin": true, "AttachStdout": true, "AttachStderr": true, "Tty": tty, "Cmd": cmd, "Env": ["TERM=dumb", "COLUMNS=160", "LINES=40"]});
        let v = self.json("POST", &format!("/containers/{}/exec", enc(id)), Some(&body), T_SHORT)?;
        let eid = v.get("Id").and_then(Value::as_str).unwrap_or_default().to_string();
        check_name("exec", &eid)?;
        Ok(eid)
    }

    pub fn exec_start(&self, exec_id: &str, tty: bool) -> Result<(std::io::BufReader<std::os::unix::net::UnixStream>, std::os::unix::net::UnixStream), Error> {
        check_name("exec", exec_id)?;
        let body = json!({"Detach": false, "Tty": tty}).to_string();
        http::upgrade(&self.socket, T_SHORT, &self.url(&format!("/exec/{}/start", enc(exec_id))), body.as_bytes())
    }

    pub fn exec_resize(&self, exec_id: &str, cols: u16, rows: u16) -> Result<(), Error> {
        check_name("exec", exec_id)?;
        self.call("POST", &format!("/exec/{}/resize?h={rows}&w={cols}", enc(exec_id)), None, T_SHORT).map(|_| ())
    }

    pub fn exec_exit_code(&self, exec_id: &str) -> Result<Option<i64>, Error> {
        check_name("exec", exec_id)?;
        let v = self.json("GET", &format!("/exec/{}/json", enc(exec_id)), None, T_SHORT)?;
        Ok(if v.get("Running").and_then(Value::as_bool).unwrap_or(false) { None } else { v.get("ExitCode").and_then(Value::as_i64) })
    }

    // ---- images

    pub fn inspect_image(&self, r: &str) -> Result<Value, Error> {
        self.json("GET", &format!("/images/{}/json", image_path(r)?), None, T_SHORT)
    }

    pub fn image_history(&self, r: &str) -> Result<Vec<HistoryEntry>, Error> {
        self.get(&format!("/images/{}/history", image_path(r)?), T_SHORT)
    }

    /// Pull with progress lines ("status id progress"). Fails on the first error message in the stream.
    pub fn pull(&self, reference: &str, mut progress: impl FnMut(String)) -> Result<(), Error> {
        if !valid_image_ref(reference) {
            return Err(Error::Invalid(format!("invalid image reference '{reference}'")));
        }
        let (from, tag) = match reference.split_once('@') {
            Some((n, d)) => (n.to_string(), d.to_string()),
            None => split_tag(reference),
        };
        let mut s = http::stream(&self.socket, T_SHORT, "POST", &self.url(&format!("/images/create?fromImage={}&tag={}", enc(&from), enc(&tag))), None)?;
        let mut line = String::new();
        loop {
            line.clear();
            if s.reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
            if let Some(e) = v.get("error").and_then(Value::as_str) {
                return Err(Error::Api { status: 500, message: e.to_string() });
            }
            let st = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
            progress(format!("{} {} {}", st("id"), st("status"), st("progress")).trim().to_string());
        }
    }

    /// Removes one reference (tag) or, given an ID, the image. Never forces: images used by containers stay.
    pub fn remove_image(&self, r: &str, force: bool) -> Result<Vec<String>, Error> {
        let v = self.json("DELETE", &format!("/images/{}?force={force}&noprune=false", image_path(r)?), None, T_LONG)?;
        Ok(v.as_array().map(|a| a.iter().flat_map(|e| e.as_object().into_iter().flat_map(|o| o.iter().map(|(k, v)| format!("{k}: {}", v.as_str().unwrap_or_default())))).collect()).unwrap_or_default())
    }

    // ---- networks

    pub fn inspect_network_raw(&self, id: &str) -> Result<Value, Error> {
        check_name("network", id)?;
        self.json("GET", &format!("/networks/{}", enc(id)), None, T_SHORT)
    }

    pub fn create_network(&self, spec: &NetworkSpec) -> Result<String, Error> {
        spec.validate().map_err(Error::Invalid)?;
        let v = self.json("POST", "/networks/create", Some(&spec.body()), T_SHORT)?;
        Ok(v.get("Id").and_then(Value::as_str).unwrap_or_default().to_string())
    }

    pub fn remove_network(&self, id: &str) -> Result<(), Error> {
        check_name("network", id)?;
        self.call("DELETE", &format!("/networks/{}", enc(id)), None, T_SHORT).map(|_| ())
    }

    pub fn connect_network(&self, net: &str, container: &str) -> Result<(), Error> {
        check_name("network", net)?;
        check_name("container", container)?;
        self.call("POST", &format!("/networks/{}/connect", enc(net)), Some(&json!({"Container": container})), T_SHORT).map(|_| ())
    }

    pub fn disconnect_network(&self, net: &str, container: &str) -> Result<(), Error> {
        check_name("network", net)?;
        check_name("container", container)?;
        self.call("POST", &format!("/networks/{}/disconnect", enc(net)), Some(&json!({"Container": container, "Force": false})), T_SHORT).map(|_| ())
    }

    // ---- volumes

    pub fn inspect_volume(&self, name: &str) -> Result<Value, Error> {
        check_name("volume", name)?;
        self.json("GET", &format!("/volumes/{}", enc(name)), None, T_SHORT)
    }

    pub fn create_volume(&self, name: &str, driver: &str) -> Result<(), Error> {
        check_name("volume", name)?;
        if !valid_object_name(driver) {
            return Err(Error::Invalid("invalid volume driver".into()));
        }
        self.call("POST", "/volumes/create", Some(&json!({"Name": name, "Driver": driver})), T_SHORT).map(|_| ())
    }

    /// Never forced: a volume in use by any container is refused by the engine.
    pub fn remove_volume(&self, name: &str) -> Result<(), Error> {
        check_name("volume", name)?;
        self.call("DELETE", &format!("/volumes/{}?force=false", enc(name)), None, T_LONG).map(|_| ())
    }

    // ---- build cache / events

    /// Returns (records removed, bytes reclaimed). `all=false` removes only unreferenced (dangling) cache.
    pub fn prune_build_cache(&self, all: bool) -> Result<(usize, u64), Error> {
        let v = self.json("POST", &format!("/build/prune?all={all}"), None, Duration::from_secs(600))?;
        Ok((v.get("CachesDeleted").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0), v.get("SpaceReclaimed").and_then(Value::as_u64).unwrap_or(0)))
    }

    /// Event stream starting `since` (unix seconds; past events come from the engine's in-memory buffer).
    pub fn events(&self, since: u64) -> Result<Stream, Error> {
        http::stream(&self.socket, T_SHORT, "GET", &self.url(&format!("/events?since={since}")), None)
    }
}

// ---------------------------------------------------------------- network creation

pub const NETWORK_DRIVERS: [&str; 4] = ["bridge", "macvlan", "ipvlan", "overlay"];

#[derive(Clone, Default, Debug)]
pub struct NetworkSpec {
    pub name: String,
    pub driver: String,
    pub subnet: String,
    pub gateway: String,
    pub internal: bool,
    pub attachable: bool,
    /// Host interface for macvlan/ipvlan.
    pub parent: String,
}

/// "10.1.0.0/16" or "fd00::/64".
pub fn valid_cidr(s: &str) -> bool {
    let Some((ip, len)) = s.trim().split_once('/') else { return false };
    let Ok(ip) = ip.parse::<std::net::IpAddr>() else { return false };
    let max = if ip.is_ipv4() { 32 } else { 128 };
    len.parse::<u8>().map(|l| l <= max).unwrap_or(false)
}

fn in_cidr(ip: std::net::IpAddr, cidr: &str) -> bool {
    use std::net::IpAddr::*;
    let Some((net, len)) = cidr.split_once('/') else { return false };
    let (Ok(net), Ok(len)) = (net.parse::<std::net::IpAddr>(), len.parse::<u32>()) else { return false };
    match (ip, net) {
        (V4(a), V4(b)) => len == 0 || (u32::from(a) ^ u32::from(b)) >> (32 - len.min(32)) == 0,
        (V6(a), V6(b)) => len == 0 || (u128::from(a) ^ u128::from(b)) >> (128 - len.min(128)) == 0,
        _ => false,
    }
}

impl NetworkSpec {
    pub fn validate(&self) -> Result<(), String> {
        if !valid_object_name(&self.name) {
            return Err("Network name: letters, digits, '_', '.', '-' (must start with a letter or digit)".into());
        }
        if !NETWORK_DRIVERS.contains(&self.driver.as_str()) {
            return Err(format!("Unsupported driver '{}'", self.driver));
        }
        let subnet = self.subnet.trim();
        if !subnet.is_empty() && !valid_cidr(subnet) {
            return Err(format!("Subnet '{subnet}' is not a CIDR such as 172.30.0.0/16"));
        }
        let gw = self.gateway.trim();
        if !gw.is_empty() {
            let ip: std::net::IpAddr = gw.parse().map_err(|_| format!("Gateway '{gw}' is not an IP address"))?;
            if subnet.is_empty() {
                return Err("A gateway needs a subnet".into());
            }
            if !in_cidr(ip, subnet) {
                return Err(format!("Gateway {gw} is outside {subnet}"));
            }
        }
        if matches!(self.driver.as_str(), "macvlan" | "ipvlan") && !self.parent.trim().is_empty() && !valid_object_name(self.parent.trim()) {
            return Err("Parent interface name is invalid".into());
        }
        Ok(())
    }

    pub fn body(&self) -> Value {
        let mut cfg = serde_json::Map::new();
        if !self.subnet.trim().is_empty() {
            cfg.insert("Subnet".into(), json!(self.subnet.trim()));
        }
        if !self.gateway.trim().is_empty() {
            cfg.insert("Gateway".into(), json!(self.gateway.trim()));
        }
        let mut opts = serde_json::Map::new();
        if !self.parent.trim().is_empty() {
            opts.insert("parent".into(), json!(self.parent.trim()));
        }
        json!({
            "Name": self.name.trim(), "Driver": self.driver, "Internal": self.internal, "Attachable": self.attachable,
            "CheckDuplicate": true, "Options": opts,
            "IPAM": {"Driver": "default", "Config": if cfg.is_empty() { vec![] } else { vec![Value::Object(cfg)] }},
        })
    }
}
