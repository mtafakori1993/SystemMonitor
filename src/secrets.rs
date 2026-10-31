//! freedesktop.org Secret Service client (GNOME Keyring, KWallet/ksecretd, KeePassXC...) over D-Bus.
//! Passwords are exchanged in "plain" session mode, which is local to the session bus.
use futures_util::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use zbus::{Connection, OwnedObjectPath, Proxy, Variant};

const DEST: &str = "org.freedesktop.secrets";
const SVC_PATH: &str = "/org/freedesktop/secrets";
const SVC: &str = "org.freedesktop.Secret.Service";
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

fn e(x: impl std::fmt::Display) -> String {
    x.to_string()
}

fn attrs(kind: &str, name: &str) -> HashMap<String, String> {
    HashMap::from([("application".to_string(), "SystemMonitor".to_string()), ("kind".to_string(), kind.to_string()), ("profile".to_string(), name.to_string())])
}

async fn proxy<'a>(c: &'a Connection, path: &'a str, iface: &'a str) -> Result<Proxy<'a>, String> {
    Proxy::new(c, DEST, path, iface).await.map_err(e)
}

async fn session(c: &Connection) -> Result<OwnedObjectPath, String> {
    let svc = proxy(c, SVC_PATH, SVC).await?;
    let (_out, path): (zbus::OwnedVariant, OwnedObjectPath) = svc.call("OpenSession", &("plain", Variant::new(""))).await.map_err(e)?;
    Ok(path)
}

/// Wait for the user to complete a keyring unlock / authorization prompt.
async fn run_prompt(c: &Connection, prompt: &OwnedObjectPath) -> Result<(), String> {
    if prompt.as_str() == "/" {
        return Ok(());
    }
    let p = proxy(c, prompt.as_str(), "org.freedesktop.Secret.Prompt").await?;
    let mut sig = p.receive_signal("Completed").await.map_err(e)?;
    p.call::<_, _, ()>("Prompt", &("",)).await.map_err(e)?;
    match tokio::time::timeout(Duration::from_secs(90), sig.next()).await {
        Ok(Some(_)) => Ok(()),
        _ => Err("keyring prompt timed out".into()),
    }
}

async fn unlock(c: &Connection, objs: Vec<OwnedObjectPath>) -> Result<(), String> {
    let svc = proxy(c, SVC_PATH, SVC).await?;
    let (_u, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) = svc.call("Unlock", &(objs,)).await.map_err(e)?;
    run_prompt(c, &prompt).await
}

pub async fn store(kind: &str, name: &str, label: &str, secret: &str) -> Result<(), String> {
    let c = Connection::session().await.map_err(e)?;
    let sess = session(&c).await?;
    let coll_path = "/org/freedesktop/secrets/aliases/default";
    unlock(&c, vec![OwnedObjectPath::try_from(coll_path).map_err(e)?]).await?;
    let coll = proxy(&c, coll_path, "org.freedesktop.Secret.Collection").await?;
    let props: HashMap<&str, Variant> = HashMap::from([
        ("org.freedesktop.Secret.Item.Label", Variant::new(label)),
        ("org.freedesktop.Secret.Item.Attributes", Variant::new(attrs(kind, name))),
    ]);
    let sec: Secret = (sess, vec![], secret.as_bytes().to_vec(), "text/plain".into());
    let (_item, prompt): (OwnedObjectPath, OwnedObjectPath) = coll.call("CreateItem", &(props, sec, true)).await.map_err(e)?;
    run_prompt(&c, &prompt).await
}

pub async fn lookup(kind: &str, name: &str) -> Result<Option<String>, String> {
    let c = Connection::session().await.map_err(e)?;
    let svc = proxy(&c, SVC_PATH, SVC).await?;
    let (mut open, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = svc.call("SearchItems", &(attrs(kind, name),)).await.map_err(e)?;
    if open.is_empty() && !locked.is_empty() {
        unlock(&c, locked.clone()).await?;
        open = locked;
    }
    let Some(item) = open.first().cloned() else { return Ok(None) };
    let sess = session(&c).await?;
    let secrets: HashMap<OwnedObjectPath, Secret> = svc.call("GetSecrets", &(vec![item.clone()], sess)).await.map_err(e)?;
    Ok(secrets.get(&item).map(|s| String::from_utf8_lossy(&s.2).into_owned()))
}

pub async fn delete(kind: &str, name: &str) -> Result<(), String> {
    let c = Connection::session().await.map_err(e)?;
    let svc = proxy(&c, SVC_PATH, SVC).await?;
    let (open, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = svc.call("SearchItems", &(attrs(kind, name),)).await.map_err(e)?;
    for item in open.into_iter().chain(locked) {
        let ip = proxy(&c, item.as_str(), "org.freedesktop.Secret.Item").await?;
        let prompt: OwnedObjectPath = ip.call("Delete", &()).await.map_err(e)?;
        run_prompt(&c, &prompt).await?;
    }
    Ok(())
}
