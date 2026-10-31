//! Tests for the Docker module. A fake engine on a temporary Unix socket exercises the real HTTP client;
//! `live_*` tests talk to a real daemon and are ignored by default (`cargo test -- --ignored docker`).
use super::api::*;
use super::cleanup::{self, Category, Inputs, Remover, Selection, VolumeScope};
use super::config::{self, ProxySet, ProxyUrl};
use super::http::{self, Error};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn tmpdir(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!("sysmon-docker-{tag}-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

type Handler = dyn Fn(&str, &str, &str) -> (u16, String, bool) + Send + Sync;

/// Serve `handler(method, path, body) -> (status, body, chunked)` on a fresh socket; records requests.
fn fake_engine(handler: Arc<Handler>) -> (PathBuf, Arc<Mutex<Vec<String>>>) {
    let dir = tmpdir("sock");
    let sock = dir.join("docker.sock");
    let l = UnixListener::bind(&sock).unwrap();
    let log = Arc::new(Mutex::new(vec![]));
    let log2 = log.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let (h, log) = (handler.clone(), log2.clone());
            std::thread::spawn(move || {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let mut it = line.split_whitespace();
                let (m, p) = (it.next().unwrap_or("").to_string(), it.next().unwrap_or("").to_string());
                let mut len = 0;
                loop {
                    let mut l = String::new();
                    r.read_line(&mut l).unwrap();
                    if l.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0u8; len];
                r.read_exact(&mut body).unwrap();
                let body = String::from_utf8(body).unwrap();
                log.lock().unwrap().push(format!("{m} {p}"));
                let (status, out, chunked) = h(&m, &p, &body);
                let mut w = s;
                if chunked {
                    let _ = write!(w, "HTTP/1.1 {status} X\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n");
                    for part in out.as_bytes().chunks(7) {
                        let _ = write!(w, "{:x}\r\n", part.len());
                        let _ = w.write_all(part);
                        let _ = w.write_all(b"\r\n");
                    }
                    let _ = w.write_all(b"0\r\n\r\n");
                } else {
                    let _ = write!(w, "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{out}", out.len());
                }
            });
        }
    });
    (sock, log)
}

fn engine_with(h: impl Fn(&str, &str, &str) -> (u16, String, bool) + Send + Sync + 'static) -> (Engine, Arc<Mutex<Vec<String>>>) {
    let (sock, log) = fake_engine(Arc::new(move |m: &str, p: &str, b: &str| if p == "/version" { (200, json!({"ApiVersion": "1.56", "Version": "29.0.0"}).to_string(), false) } else { h(m, p, b) }));
    (Engine::at(sock).unwrap(), log)
}

// ---------------------------------------------------------------- http / api

#[test]
fn chunked_decoding() {
    let raw = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\nE\r\n in\r\n\r\nchunks.\r\n0\r\nTrailer: x\r\n\r\n";
    let mut out = String::new();
    http::Chunked::new(&raw[..]).read_to_string(&mut out).unwrap();
    assert_eq!(out, "Wikipedia in\r\n\r\nchunks.");
    let mut bad = String::new();
    assert!(http::Chunked::new(&b"zz\r\nabc"[..]).read_to_string(&mut bad).is_err());
}

#[test]
fn multiplexed_frames() {
    let mut buf = vec![1, 0, 0, 0, 0, 0, 0, 3];
    buf.extend(b"out");
    buf.extend([2, 0, 0, 0, 0, 0, 0, 2]);
    buf.extend(b"er");
    let mut r = &buf[..];
    assert_eq!(http::read_frame(&mut r).unwrap(), Some((1, b"out".to_vec())));
    assert_eq!(http::read_frame(&mut r).unwrap(), Some((2, b"er".to_vec())));
    assert_eq!(http::read_frame(&mut r).unwrap(), None);
    let mut raw = &b"plain tty text"[..];
    assert!(http::read_frame(&mut raw).is_err(), "TTY output is not multiplexed");
}

#[test]
fn version_negotiation() {
    assert_eq!(negotiate("1.56").unwrap(), "1.47");
    assert_eq!(negotiate("1.43").unwrap(), "1.43");
    assert!(negotiate("1.40").is_err());
    assert!(negotiate("x").is_err());
}

#[test]
fn engine_calls_and_errors() {
    let (e, log) = engine_with(|m, p, _| match (m, p) {
        ("GET", "/v1.47/containers/json?all=1") => (200, json!([{"Id": "abc123", "Names": ["/web"], "State": "running", "Status": "Up 2 hours (healthy)", "Ports": [{"IP": "0.0.0.0", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"}], "Mounts": null, "Labels": null}]).to_string(), true),
        ("POST", "/v1.47/containers/web/stop?t=10") => (304, String::new(), false),
        ("DELETE", "/v1.47/containers/web?v=false&force=false") => (409, json!({"message": "cannot remove a running container"}).to_string(), false),
        _ => (404, json!({"message": format!("no route {m} {p}")}).to_string(), false),
    });
    let c = e.containers().unwrap();
    assert_eq!(c[0].name(), "web");
    assert_eq!(c[0].health(), "healthy");
    assert_eq!(c[0].ports_text(), "0.0.0.0:8080->80/tcp");
    assert!(c[0].mounts.is_empty() && c[0].labels.is_empty(), "null lists become empty");
    assert!(e.container_action("web", "stop").is_ok(), "304 = already stopped");
    match e.remove_container("web", false) {
        Err(Error::Api { status: 409, message }) => assert!(message.contains("running")),
        r => panic!("{r:?}"),
    }
    // invalid input is rejected before any request is sent
    let before = log.lock().unwrap().len();
    for bad in ["../../containers/x", "a b", "x?force=true", "", "-x"] {
        assert!(matches!(e.container_action(bad, "start"), Err(Error::Invalid(_))), "{bad}");
        assert!(matches!(e.remove_volume(bad), Err(Error::Invalid(_))), "{bad}");
    }
    assert!(matches!(e.container_action("web", "kill"), Err(Error::Invalid(_))));
    assert_eq!(log.lock().unwrap().len(), before);
}

#[test]
fn socket_errors_are_classified() {
    let missing = tmpdir("none").join("docker.sock");
    assert!(matches!(Engine::at(missing), Err(Error::NotRunning(_))));
    if unsafe { libc::geteuid() } != 0 {
        let (sock, _) = fake_engine(Arc::new(|_: &str, _: &str, _: &str| (200, "{}".into(), false)));
        std::fs::set_permissions(&sock, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        assert!(matches!(Engine::at(sock), Err(Error::Denied(_))), "EACCES → Denied (not in the docker group)");
    }
}

#[test]
fn streams_events_and_pull_progress() {
    let (e, _) = engine_with(|_, p, _| {
        if p.starts_with("/v1.47/events") {
            (200, "{\"Type\":\"container\",\"Action\":\"start\",\"Actor\":{\"ID\":\"abc\",\"Attributes\":{\"name\":\"web\"}},\"time\":1,\"timeNano\":1}\n{\"Type\":\"container\",\"Action\":\"exec_start: sh\",\"Actor\":{\"ID\":\"abc\"},\"time\":2,\"timeNano\":2}\n".into(), true)
        } else if p.starts_with("/v1.47/images/create?fromImage=library%2Falpine&tag=3.20") {
            (200, "{\"status\":\"Pulling\",\"id\":\"3.20\"}\n{\"status\":\"Done\"}\n".into(), true)
        } else if p.starts_with("/v1.47/images/create") {
            (200, "{\"status\":\"Pulling\"}\n{\"error\":\"manifest unknown\"}\n".into(), true)
        } else {
            (404, "{}".into(), false)
        }
    });
    let mut s = e.events(0).unwrap();
    let evs: Vec<Event> = (&mut s.reader).lines().map_while(Result::ok).filter_map(|l| serde_json::from_str(&l).ok()).collect();
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0].name(), "web");
    assert!(evs[0].changes_inventory() && !evs[1].changes_inventory());
    let mut lines = vec![];
    e.pull("library/alpine:3.20", |l| lines.push(l)).unwrap();
    assert_eq!(lines.len(), 2);
    match e.pull("nosuch:1", |_| {}) {
        Err(Error::Api { message, .. }) => assert_eq!(message, "manifest unknown"),
        r => panic!("{r:?}"),
    }
    assert!(matches!(e.pull("UPPER/case", |_| {}), Err(Error::Invalid(_))));
}

#[test]
fn image_reference_validation() {
    for ok in ["alpine", "alpine:3.20", "library/nginx:1.27-alpine", "ghcr.io/org/app:v1.2.3", "localhost:5000/x/y", "registry.example.com:443/team/img@sha256:".to_string().as_str(), "a_b/c__d/e-f:latest", "img@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"] {
        if ok.ends_with("sha256:") {
            continue;
        }
        assert!(valid_image_ref(ok), "{ok}");
    }
    for bad in ["", "Alpine", "a..b", "a//b", "-x", "x:", "x:tag with space", "x;rm -rf", "x@sha256:abc", "../etc", "x:$(id)", "a/b?c=d", "a_.b"] {
        assert!(!valid_image_ref(bad), "{bad}");
    }
    assert_eq!(split_tag("localhost:5000/app"), ("localhost:5000/app".into(), "latest".into()));
    assert_eq!(split_tag("localhost:5000/app:v2"), ("localhost:5000/app".into(), "v2".into()));
    assert_eq!(parse_exec_cmd(" sh  -c  ls ").unwrap(), vec!["sh", "-c", "ls"]);
    assert!(parse_exec_cmd("").is_err() && parse_exec_cmd("sh\n-c").is_err());
}

#[test]
fn network_spec_validation() {
    let mut s = NetworkSpec { name: "backend".into(), driver: "bridge".into(), subnet: "172.30.0.0/16".into(), gateway: "172.30.0.1".into(), ..Default::default() };
    assert!(s.validate().is_ok());
    assert_eq!(s.body()["IPAM"]["Config"][0]["Gateway"], "172.30.0.1");
    s.gateway = "10.0.0.1".into();
    assert!(s.validate().unwrap_err().contains("outside"));
    s.gateway.clear();
    s.subnet = "172.30.0.0/33".into();
    assert!(s.validate().is_err());
    s.subnet = "fd00:1::/64".into();
    assert!(s.validate().is_ok());
    s.name = "bad name".into();
    assert!(s.validate().is_err());
    s.name = "ok".into();
    s.driver = "host".into();
    assert!(s.validate().is_err());
}

// ---------------------------------------------------------------- cleanup

fn c(id: &str, state: &str, image: &str, vols: &[&str], nets: &[&str]) -> Container {
    Container {
        id: id.into(),
        names: vec![format!("/{id}")],
        image_id: image.into(),
        state: state.into(),
        mounts: vols.iter().map(|v| Mount { kind: "volume".into(), name: v.to_string(), ..Default::default() }).collect(),
        network_settings: NetSettings { networks: nets.iter().map(|n| (n.to_string(), Endpoint { network_id: format!("id-{n}"), ..Default::default() })).collect() },
        ..Default::default()
    }
}

fn img(id: &str, tags: &[&str], size: i64, created: i64) -> Image {
    Image { id: id.into(), repo_tags: tags.iter().map(|t| t.to_string()).collect(), size, shared_size: 0, created, containers: -1, ..Default::default() }
}

fn vol(name: &str, size: i64, refs: i64) -> Volume {
    Volume { name: name.into(), driver: "local".into(), usage_data: Some(UsageData { size, ref_count: refs }), ..Default::default() }
}

fn net(name: &str) -> Network {
    Network { name: name.into(), id: format!("id-{name}"), driver: "bridge".into(), scope: "local".into(), ..Default::default() }
}

struct World {
    containers: Vec<Container>,
    images: Vec<Image>,
    networks: Vec<Network>,
    volumes: Vec<Volume>,
    df: DiskUsage,
}

fn world() -> World {
    let anon = "a".repeat(64);
    let containers = vec![
        c("run1", "running", "sha256:img-run", &["dbdata"], &["appnet"]),
        c("pause1", "paused", "sha256:img-pause", &[], &[]),
        c("old1", "exited", "sha256:img-old", &[anon.as_str()], &["oldnet"]),
        c("new1", "created", "sha256:img-run", &[], &[]),
    ];
    let images = vec![img("sha256:img-run", &["app:2"], 100, 20), img("sha256:img-pause", &["tool:1"], 50, 10), img("sha256:img-old", &["app:1", "app:legacy"], 80, 10), img("sha256:dangling", &["<none>:<none>"], 30, 5), img("sha256:unused", &["other:9"], 40, 5)];
    let networks = vec![net("bridge"), net("host"), net("none"), net("appnet"), net("oldnet"), net("emptynet")];
    let volumes = vec![vol("dbdata", 1 << 30, 1), vol(&anon, 4096, 1), vol("named-unused", 2048, 0), vol(&"b".repeat(64), 0, 0), vol("unmeasurable", -1, 0)];
    let mut dfc = containers.clone();
    for x in &mut dfc {
        x.size_rw = Some(10);
    }
    let df = DiskUsage { layers_size: 300, images: images.clone(), containers: dfc, volumes: volumes.clone(), build_cache: vec![BuildCache { id: "bc1".into(), kind: "regular".into(), size: 500, ..Default::default() }, BuildCache { id: "bc2".into(), in_use: true, size: 99, ..Default::default() }] };
    World { containers, images, networks, volumes, df }
}

fn plan_for(w: &World, sel: Selection) -> cleanup::Plan {
    cleanup::plan(sel, &Inputs { containers: &w.containers, images: &w.images, networks: &w.networks, volumes: &w.volumes, df: Some(&w.df) })
}

fn names(p: &cleanup::Plan, cat: Category) -> Vec<String> {
    let mut v: Vec<String> = p.items.iter().filter(|i| i.cat == cat).map(|i| i.target.clone()).collect();
    v.sort();
    v
}

#[test]
fn cleanup_never_touches_running_or_their_images() {
    let w = world();
    let p = plan_for(&w, Selection { stopped_containers: true, unused_images: true, unused_networks: true, volumes: VolumeScope::All, build_cache: true, ..Default::default() });
    assert_eq!(names(&p, Category::StoppedContainers), vec!["new1", "old1"]);
    let images: Vec<String> = p.items.iter().filter(|i| matches!(i.cat, Category::UnusedImages | Category::DanglingImages)).map(|i| i.target.clone()).collect();
    // app:2 is used by a running container (and by new1, which is removed): protected
    assert!(!images.iter().any(|t| t == "app:2") && !images.iter().any(|t| t == "tool:1"), "{images:?}");
    // the old image becomes unused once its stopped container is removed: every tag is listed
    assert!(images.contains(&"app:1".to_string()) && images.contains(&"app:legacy".to_string()) && images.contains(&"other:9".to_string()));
    assert_eq!(names(&p, Category::DanglingImages), vec!["sha256:dangling"]);
    // networks: predefined skipped, attached to a running container protected
    assert_eq!(names(&p, Category::UnusedNetworks), vec!["id-emptynet", "id-oldnet"]);
    // volumes: the running container's volume is never listed
    assert!(!names(&p, Category::UnusedVolumes).contains(&"dbdata".to_string()));
    assert!(p.protected.iter().any(|(n, why)| n == "dbdata" && why.contains("used")));
    // build cache in use is protected
    assert_eq!(names(&p, Category::BuildCache), vec!["bc1"]);
    // tag sizes are not double counted
    assert_eq!(p.bytes(Category::UnusedImages), 80 + 40);
}

#[test]
fn cleanup_volume_scopes_and_warnings() {
    let w = world();
    let anon_only = plan_for(&w, Selection { volumes: VolumeScope::Anonymous, ..Default::default() });
    let v = names(&anon_only, Category::UnusedVolumes);
    assert_eq!(v, vec!["b".repeat(64)], "anonymous only; the stopped container still holds the other anonymous volume");
    assert!(anon_only.protected.iter().any(|(n, _)| n == "named-unused"));
    let all = plan_for(&w, Selection { volumes: VolumeScope::All, ..Default::default() });
    let it = |n: &str| all.items.iter().find(|i| i.target == n).unwrap().clone();
    assert!(it("named-unused").warning.unwrap().contains("2.0 KiB"));
    assert!(it("unmeasurable").warning.unwrap().contains("unknown"));
    assert!(it(&"b".repeat(64)).warning.is_none(), "empty volume");
    // removing the stopped container frees its anonymous volume as well
    let both = plan_for(&w, Selection { stopped_containers: true, volumes: VolumeScope::Anonymous, ..Default::default() });
    assert!(names(&both, Category::UnusedVolumes).contains(&"a".repeat(64)));
}

#[test]
fn system_prune_presets_match_docker() {
    let w = world();
    let p = plan_for(&w, Selection::system_prune());
    assert!(p.count(Category::UnusedImages) == 0 && p.count(Category::UnusedVolumes) == 0 && p.count(Category::DanglingImages) == 1);
    let pa = plan_for(&w, Selection::system_prune_all());
    assert!(pa.count(Category::UnusedImages) > 0 && pa.count(Category::UnusedVolumes) == 0);
    let pv = plan_for(&w, Selection::system_prune_volumes());
    assert!(pv.count(Category::UnusedVolumes) > 0 && pv.items.iter().filter(|i| i.cat == Category::UnusedVolumes).all(|i| i.name.contains("anonymous")));
}

#[derive(Default)]
struct FakeRemover(Mutex<Vec<String>>);

impl Remover for FakeRemover {
    fn remove_container(&self, id: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("c:{id}"));
        Ok(())
    }
    fn remove_image(&self, r: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("i:{r}"));
        if r == "other:9" { Err("conflict: image is in use".into()) } else { Ok(()) }
    }
    fn remove_network(&self, id: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("n:{id}"));
        Ok(())
    }
    fn remove_volume(&self, name: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("v:{name}"));
        Ok(())
    }
    fn prune_build_cache(&self) -> Result<u64, String> {
        self.0.lock().unwrap().push("b".into());
        Ok(500)
    }
}

#[test]
fn cleanup_execution_order_confirmation_and_results() {
    let dir = tmpdir("audit");
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
    let w = world();
    let p = plan_for(&w, Selection { stopped_containers: true, unused_images: true, unused_networks: true, volumes: VolumeScope::All, build_cache: true, ..Default::default() });
    let r = FakeRemover::default();
    let rep = cleanup::execute(&r, &p, false, &|| Some(1000));
    let calls = r.0.lock().unwrap().clone();
    assert!(!calls.iter().any(|c| c.starts_with("v:")), "volumes need explicit confirmation: {calls:?}");
    assert!(rep.outcomes.iter().filter(|o| o.cat == Category::UnusedVolumes).all(|o| o.result.as_ref().unwrap_err().contains("not explicitly confirmed")));
    let first = |pfx: &str| calls.iter().position(|c| c.starts_with(pfx)).unwrap();
    assert!(first("c:") < first("i:") && first("i:") < first("n:") && first("n:") < first("b"), "containers → images → networks → cache");
    assert_eq!(rep.outcomes.iter().find(|o| o.name == "other:9").unwrap().result.clone().unwrap_err(), "conflict: image is in use");
    assert_eq!(rep.host_free_before, Some(1000));
    assert!(rep.reclaimed >= 500 + 80, "build cache + old image");
    // with confirmation the volumes are deleted
    let r2 = FakeRemover::default();
    cleanup::execute(&r2, &p, true, &|| None);
    assert!(r2.0.lock().unwrap().iter().any(|c| c == "v:named-unused"));
    // every destructive step is audited
    let log = super::audit::recent(500);
    assert!(log.iter().any(|e| e.action.contains("stopped containers") && e.target == "old1" && e.ok));
    assert!(log.iter().any(|e| e.target == "other:9" && !e.ok && e.error.contains("in use")));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn df_totals_and_image_flags() {
    let w = world();
    let t = w.df.totals();
    assert_eq!(t.images, 300);
    assert_eq!(t.containers, 40);
    assert_eq!(t.containers_reclaimable, 20, "exited + created only");
    assert_eq!(t.volumes, (1 << 30) + 4096 + 2048);
    assert_eq!(t.volumes_reclaimable, 2048);
    assert_eq!(t.build_cache_reclaimable, 500);
    let f = super::image_flags(&w.images);
    assert!(f["sha256:img-old"].outdated && f["sha256:img-old"].duplicate_tags);
    assert!(!f["sha256:img-run"].outdated);
}

// ---------------------------------------------------------------- live stats

#[test]
fn stats_counters_and_cpu() {
    let v = json!({
        "cpu_stats": {"cpu_usage": {"total_usage": 2_000_000_000u64}, "system_cpu_usage": 10_000_000_000u64, "online_cpus": 4},
        "memory_stats": {"usage": 300, "limit": 1000, "stats": {"inactive_file": 100}},
        "networks": {"eth0": {"rx_bytes": 10, "tx_bytes": 20}, "eth1": {"rx_bytes": 1, "tx_bytes": 2}},
        "blkio_stats": {"io_service_bytes_recursive": [{"op": "Read", "value": 7}, {"op": "write", "value": 9}, {"op": "Total", "value": 16}]},
        "pids_stats": {"current": 3}
    });
    let c1 = super::live::counters(&v);
    assert_eq!((c1.mem, c1.mem_limit, c1.net_rx, c1.net_tx, c1.blk_r, c1.blk_w, c1.pids), (200, 1000, 11, 22, 7, 9, 3));
    let c2 = super::live::Counters { cpu_total: 3_000_000_000, system: 14_000_000_000, ..c1 };
    assert!((super::live::cpu_pct(&c1, &c2) - 100.0).abs() < 0.01, "1 of 4 cores");
    assert_eq!(super::live::cpu_pct(&c2, &c2), 0.0);
}

// ---------------------------------------------------------------- daemon config / proxy

#[test]
fn daemon_validation() {
    let ok = r#"{"registry-mirrors":["https://mirror.example.com"],"insecure-registries":["10.0.0.0/8","reg.local:5000"],"dns":["1.1.1.1"],"log-driver":"json-file","log-opts":{"max-size":"10m","max-file":"3"},"live-restore":true,"features":{"buildkit":true}}"#;
    let (m, c) = config::check_text(ok);
    assert!(m.is_some() && c.ok(), "{:?}", c.errors);
    assert!(c.warnings.iter().any(|w| w.contains("features")), "unknown keys are preserved but flagged");
    for (bad, why) in [
        (r#"{"log-opts":{"max-file":3}}"#, "must be a string"),
        (r#"{"registry-mirrors":["ftp://x"]}"#, "http"),
        (r#"{"dns":["999.1.1.1"]}"#, "not a valid IP"),
        (r#"{"hosts":["tcp://0.0.0.0:2375"]}"#, "without \"tlsverify\""),
        (r#"{"proxies":{"http-proxy":"gopher://x:1"}}"#, "not supported"),
        (r#"{"proxies":{"all-proxy":"socks5://x:1"}}"#, "not a daemon proxy setting"),
        (r#"{"live-restore":"yes"}"#, "true or false"),
        (r#"[1,2]"#, "JSON object"),
        (r#"{"a":"#, "invalid JSON"),
    ] {
        let (_, c) = config::check_text(bad);
        assert!(c.errors.iter().any(|e| e.contains(why)), "{bad}: {:?}", c.errors);
    }
}

#[test]
fn daemon_merge_preserves_unrelated_settings() {
    let base: serde_json::Map<String, Value> = serde_json::from_str(r#"{"data-root":"/srv/docker","features":{"containerd-snapshotter":true},"proxies":{"http-proxy":"http://old:1"}}"#).unwrap();
    let p = ProxySet { http: "http://u:p%40ss@proxy.corp:3128".into(), https: "socks5://proxy.corp:1080".into(), no_proxy: "localhost,.corp".into(), ..Default::default() };
    p.validate(true).unwrap();
    let m = config::merge(&base, &[("proxies", config::daemon_proxy_change(&p)), ("registry-mirrors", Some(json!(["https://m.example"])))]);
    assert_eq!(m["data-root"], "/srv/docker");
    assert_eq!(m["features"]["containerd-snapshotter"], true);
    assert_eq!(m["proxies"]["https-proxy"], "socks5://proxy.corp:1080");
    assert!(m["proxies"].get("all-proxy").is_none());
    assert_eq!(config::daemon_proxies(&m), ProxySet { all: String::new(), ..p.clone() });
    let off = config::merge(&m, &[("proxies", config::daemon_proxy_change(&ProxySet::default()))]);
    assert!(off.get("proxies").is_none() && off["data-root"] == "/srv/docker");
    assert!(ProxySet { all: "socks5://x:1".into(), ..Default::default() }.validate(true).unwrap_err().contains("not supported by the Docker daemon"));
}

#[test]
fn client_config_merge_keeps_auths() {
    let cur: serde_json::Map<String, Value> = serde_json::from_str(r#"{"auths":{"ghcr.io":{"auth":"c2VjcmV0"}},"credsStore":"desktop","proxies":{"tcp://remote:2376":{"httpProxy":"http://other:1"}}}"#).unwrap();
    let p = ProxySet { http: "http://p:3128".into(), all: "socks5://p:1080".into(), ..Default::default() };
    let m = config::merge_client_proxies(cur.clone(), &p);
    assert_eq!(m["auths"], cur["auths"]);
    assert_eq!(m["credsStore"], "desktop");
    assert_eq!(m["proxies"]["tcp://remote:2376"]["httpProxy"], "http://other:1");
    assert_eq!(m["proxies"]["default"]["allProxy"], "socks5://p:1080");
    let cleared = config::merge_client_proxies(m, &ProxySet::default());
    assert!(cleared["proxies"].get("default").is_none() && cleared["proxies"].get("tcp://remote:2376").is_some());
}

#[test]
fn proxy_urls_and_secrets() {
    let u = ProxyUrl::parse("http://alice:p%40ss:w@proxy.corp:3128").unwrap();
    assert_eq!((u.user.as_str(), u.password.as_str(), u.port), ("alice", "p@ss:w", 3128));
    assert_eq!(ProxyUrl::parse(&u.to_url()).unwrap(), u, "round trip");
    assert_eq!(u.masked(), "http://alice:***@proxy.corp:3128");
    assert_eq!(ProxyUrl::parse("socks5h://[::1]").unwrap().port, 1080);
    for bad in ["proxy:3128", "ftp://x:1", "http://bad host:1", "http://x:0", "http://x:99999"] {
        assert!(ProxyUrl::parse(bad).is_err(), "{bad}");
    }
    let s = ProxySet { http: "http://bob:hunter2@p:1".into(), ..Default::default() };
    assert!(!format!("{:?}", s.masked()).contains("hunter2"));
    let scrubbed = super::audit::scrub("set http://bob:hunter2@p:1/x and socks5://u:pw@h:2, https://plain.example/a@b");
    assert!(!scrubbed.contains("hunter2") && !scrubbed.contains(":pw@"), "{scrubbed}");
    assert!(scrubbed.contains("https://plain.example/a@b"), "an @ in the path is not a credential: {scrubbed}");
    assert_eq!(config::b64_for_test("Aladdin:open sesame"), "QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
    assert_eq!(config::b64_for_test("ab"), "YWI=");
}

/// Minimal HTTP CONNECT and SOCKS5 proxies to test the connectivity check end to end.
fn fake_proxy(kind: &'static str) -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut s in l.incoming().flatten() {
            let mut buf = [0u8; 1024];
            if kind == "http" {
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let ok = req.starts_with("CONNECT registry.test:443 ") && req.contains("Proxy-Authorization: Basic YWxpY2U6czNjcmV0");
                let _ = s.write_all(if ok { b"HTTP/1.1 200 Connection established\r\n\r\n" } else { b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n" });
            } else {
                let _ = s.read(&mut buf);
                let _ = s.write_all(&[5, 2]);
                let n = s.read(&mut buf).unwrap_or(0);
                let good = &buf[..n] == [&[1u8, 5][..], b"alice", &[6], b"s3cret"].concat().as_slice();
                let _ = s.write_all(&[1, if good { 0 } else { 1 }]);
                if good {
                    let _ = s.read(&mut buf);
                    let _ = s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
                }
            }
        }
    });
    port
}

#[test]
fn proxy_connectivity_test() {
    let hp = fake_proxy("http");
    let good = ProxyUrl { scheme: config::ProxyScheme::Http, host: "127.0.0.1".into(), port: hp, user: "alice".into(), password: "s3cret".into() };
    assert!(config::test_proxy(&good, "registry.test", 443).is_ok());
    let wrong = ProxyUrl { password: "nope".into(), ..good.clone() };
    assert!(config::test_proxy(&wrong, "registry.test", 443).unwrap_err().contains("407"));
    let sp = fake_proxy("socks");
    let s = ProxyUrl { scheme: config::ProxyScheme::Socks5, port: sp, ..good.clone() };
    assert!(config::test_proxy(&s, "registry.test", 443).is_ok());
    assert!(config::test_proxy(&ProxyUrl { password: "x".into(), ..s }, "registry.test", 443).unwrap_err().contains("rejected"));
    assert!(config::test_proxy(&good, "bad host", 443).is_err());
}

#[test]
fn privileged_helper_validates_and_backs_up() {
    if unsafe { libc::geteuid() } == 0 {
        return; // the test redirect is deliberately ignored for root
    }
    let dir = tmpdir("helper");
    unsafe { std::env::set_var("SYSMON_TEST_DOCKER_DIR", &dir) };
    let target = dir.join("daemon.json");
    std::fs::write(&target, r#"{"data-root":"/srv/docker"}"#).unwrap();
    let staged = dir.join("staged.json");
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // dangerous or malformed content is refused and the file is untouched
    for evil in [r#"{"hosts":["tcp://0.0.0.0:2375"]}"#, "not json", r#"{"log-opts":{"max-file":3}}"#] {
        std::fs::write(&staged, evil).unwrap();
        assert_eq!(config::helper_main(&args(&["write", staged.to_str().unwrap()])), 2, "{evil}");
    }
    assert_eq!(std::fs::read_to_string(&target).unwrap(), r#"{"data-root":"/srv/docker"}"#);
    // symlinks are not followed
    let link = dir.join("link.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_eq!(config::helper_main(&args(&["write", link.to_str().unwrap()])), 2);
    // a valid merged document is written, with a root-side backup of the previous one
    let (cur, _) = config::read_daemon_json().unwrap();
    let new = config::merge(&cur, &[("proxies", Some(json!({"http-proxy": "http://u:pw@p:3128"})))]);
    std::fs::write(&staged, config::pretty(&new)).unwrap();
    assert_eq!(config::helper_main(&args(&["write", staged.to_str().unwrap()])), 0);
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
    assert_eq!(written["data-root"], "/srv/docker");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600, "credentials → root-only");
    let baks: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains("systemmonitor-")).collect();
    assert_eq!(baks.len(), 1);
    assert_eq!(baks[0].metadata().unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(config::helper_main(&args(&["bogus"])), 2);
    assert_eq!(config::helper_main(&args(&["restart", "extra"])), 2);
    unsafe { std::env::remove_var("SYSMON_TEST_DOCKER_DIR") };
    assert_eq!(config::helper_main(&args(&["restart"])), 2, "refuses to run unprivileged");
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------- live daemon (on demand)

#[test]
#[ignore = "needs a running Docker daemon and access to its socket"]
fn live_docker_roundtrip() {
    let e = Engine::connect().expect("docker reachable");
    let inv = super::Inventory::fetch(&e).expect("inventory");
    println!("Docker {} API {} (using {}), {} containers, {} images", inv.version.version, inv.version.api_version, e.api, inv.containers.len(), inv.images.len());
    let df = e.disk_usage().unwrap();
    println!("df: {:?}", df.totals());
    // lifecycle on a throw-away container
    let name = format!("sysmon-test-{}", std::process::id());
    let created = e.json_for_test("POST", &format!("/containers/create?name={name}"), json!({"Image": "hello-world", "Cmd": ["/hello"]}));
    assert!(created.is_ok(), "{created:?}");
    let netname = format!("sysmon-net-{}", std::process::id());
    e.create_network(&NetworkSpec { name: netname.clone(), driver: "bridge".into(), subnet: "172.31.250.0/24".into(), gateway: "172.31.250.1".into(), ..Default::default() }).unwrap();
    e.connect_network(&netname, &name).unwrap();
    assert!(e.inspect_network_raw(&netname).is_ok());
    e.disconnect_network(&netname, &name).unwrap();
    e.remove_network(&netname).unwrap();
    let vname = format!("sysmon-vol-{}", std::process::id());
    e.create_volume(&vname, "local").unwrap();
    assert_eq!(e.inspect_volume(&vname).unwrap()["Name"], vname);
    e.remove_volume(&vname).unwrap();
    e.container_action(&name, "start").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));
    let mut logs = e.logs(&name, 50, false, false).unwrap();
    let mut out = vec![];
    while let Ok(Some((_, p))) = http::read_frame(&mut logs.reader) {
        out.extend(p);
    }
    assert!(String::from_utf8_lossy(&out).contains("Hello from Docker"));
    e.remove_container(&name, false).unwrap();
    assert!(e.image_history("hello-world").unwrap().len() >= 1);
}

impl Engine {
    fn json_for_test(&self, method: &str, path: &str, body: Value) -> Result<Value, Error> {
        let r = http::request(&self.socket, std::time::Duration::from_secs(20), method, &format!("/v{}{path}", self.api), Some(body.to_string().as_bytes()))?;
        if r.status >= 400 { Err(Error::Api { status: r.status, message: http::error_message(&r.body) }) } else { Ok(serde_json::from_slice(&r.body).unwrap_or(Value::Null)) }
    }
}

#[test]
fn editor_masks_and_restores_proxy_passwords() {
    let mut m: serde_json::Map<String, Value> = serde_json::from_str(r#"{"proxies":{"http-proxy":"http://bob:s3cr%40t@p:3128","https-proxy":"http://p:3128","no-proxy":"localhost"},"debug":true}"#).unwrap();
    let saved = config::mask_proxy_secrets(&mut m);
    let shown = config::pretty(&m);
    assert!(!shown.contains("s3cr") && shown.contains(config::SECRET_PLACEHOLDER) && shown.contains("bob"));
    assert!(config::check_text(&shown).1.ok(), "the masked document still validates");
    // untouched placeholder: original password restored
    let mut back = m.clone();
    config::restore_proxy_secrets(&mut back, &saved);
    assert_eq!(ProxyUrl::parse(back["proxies"]["http-proxy"].as_str().unwrap()).unwrap().password, "s3cr@t");
    // replaced by the administrator: the new password wins
    let mut edited = m.clone();
    edited["proxies"]["http-proxy"] = json!("http://bob:new@p:3128");
    config::restore_proxy_secrets(&mut edited, &saved);
    assert_eq!(edited["proxies"]["http-proxy"], "http://bob:new@p:3128");
}
