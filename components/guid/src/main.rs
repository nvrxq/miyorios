#![forbid(unsafe_code)]

mod broker;
mod peer;

use anyhow::{Context, Result};
use broker::{Broker, BrokerConfig};
use std::path::PathBuf;
use std::sync::Arc;
use vsock::{VsockAddr, VsockListener, VMADDR_CID_ANY};

fn main() -> Result<()> {
    let mut registry_path = PathBuf::from("/etc/miyorios/spaces.toml");
    let mut port: u32 = 1700;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--registry" => registry_path = args.next().context("--registry без значения")?.into(),
            "--port" => {
                port = args
                    .next()
                    .context("--port без значения")?
                    .parse()
                    .context("--port ожидает число")?
            }
            other => anyhow::bail!("неизвестный аргумент {other}"),
        }
    }

    // брокер живёт в сессии оператора: сокет композитора берём из его окружения
    let wayland_socket =
        PathBuf::from(std::env::var("XDG_RUNTIME_DIR").context("нет XDG_RUNTIME_DIR")?)
            .join(std::env::var("WAYLAND_DISPLAY").context("нет WAYLAND_DISPLAY")?);
    let broker = Arc::new(Broker::new(
        registry_path,
        BrokerConfig {
            run_dir: PathBuf::from("/run/miyorios"),
            waypipe: PathBuf::from("/usr/local/lib/miyorios/waypipe"),
            wayland_socket,
            memory_max_mb: 512,
            tasks_max: 64,
            cpu_quota_pct: 50,
        },
    ));
    // без сторожа прокси мёртвого CID переживёт свою VM: io::copy на убитом vhost-vsock не возвращается
    broker.spawn_dead_proxy_watchdog();

    let listener = VsockListener::bind(&VsockAddr::new(VMADDR_CID_ANY, port))?;
    eprintln!("miyori-guid слушает vsock:{port}");

    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept: {e}");
                continue;
            }
        };
        let cid = match peer::peer_cid(&stream) {
            Ok(cid) => cid,
            Err(e) => {
                eprintln!("нет CID пира, соединение закрыто: {e}");
                continue;
            }
        };
        let broker = Arc::clone(&broker);
        std::thread::spawn(move || {
            if let Err(e) = broker.handle(stream, cid) {
                eprintln!("cid {cid}: {e}");
            }
        });
    }
    Ok(())
}
