//! Test harness: run a local proxy (the device binary, runs on Linux too),
//! then mount Adbfs against it. This lets us test the FUSE plumbing without
//! needing a real device.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use adb_device::DeviceId;
use adb_proxy::ProxyClient;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mountpoint = std::env::args().nth(1).expect("usage: test-adbfs <mountpoint>");
    std::fs::create_dir_all(&mountpoint)?;

    // Find and start the local proxy.
    let proxy = locate_proxy()?;
    eprintln!("starting local proxy at {} port 31399", proxy.display());
    let mut child = Command::new(&proxy)
        .arg("31399")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Wait for it.
    for _ in 0..20 {
        if tokio::net::TcpStream::connect("127.0.0.1:31399").await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let client = ProxyClient::connect("127.0.0.1:31399", 1).await?;
    let client = Arc::new(client);
    let mp = PathBuf::from(mountpoint);

    let client2 = client.clone();
    std::thread::Builder::new()
        .name("test-adbfs".into())
        .spawn(move || {
            let _ = adbfs::run(DeviceId("test".into()), (*client2).clone(), mp);
        })?;

    eprintln!("FUSE mounted. Press Ctrl-C to stop.");
    tokio::signal::ctrl_c().await?;
    child.kill().ok();
    Ok(())
}

fn locate_proxy() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let candidate = exe.parent().unwrap().join("adbshare-proxy");
    if candidate.exists() { return Ok(candidate); }
    if let Ok(p) = which("adbshare-proxy") { return Ok(p); }
    anyhow::bail!("adbshare-proxy not found; build with `cargo build --bin adbshare-proxy --release`")
}

fn which(name: &str) -> anyhow::Result<PathBuf> {
    let path = std::env::var_os("PATH").ok_or_else(|| anyhow::anyhow!("no PATH"))?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.is_file() { return Ok(p); }
    }
    Err(anyhow::anyhow!("not found"))
}
