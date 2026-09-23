#![forbid(unsafe_code)]

mod daemon;
mod ui;
mod view;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::mpsc::sync_channel;

fn main() -> Result<()> {
    let mut socket = PathBuf::from(daemon::DEFAULT_SOCKET);
    let mut dry_run = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--socket" => socket = args.next().context("--socket без значения")?.into(),
            other => anyhow::bail!("неизвестный аргумент {other}"),
        }
    }

    if dry_run {
        let daemon = daemon::Daemon::new(socket);
        print_spaces(&daemon);
        print_net(&daemon);
        return Ok(());
    }

    std::process::exit(ui::run(socket));
}

fn ask(
    daemon: &daemon::Daemon,
    id: u64,
    request: serde_json::Value,
) -> Result<miyori_proto::client::Reply, String> {
    let (tx, rx) = sync_channel(128);
    daemon.send(id, request, tx);
    loop {
        match rx.recv() {
            Ok(daemon::Update::Progress { text, .. }) => eprintln!("… {text}"),
            // ответ на чужой запрос не должен выдаваться за ответ на этот
            Ok(daemon::Update::Done { request_id, reply }) if request_id == id => return reply,
            Ok(daemon::Update::Done { .. }) => continue,
            Err(err) => return Err(err.to_string()),
        }
    }
}

fn print_spaces(daemon: &daemon::Daemon) {
    match view::spaces_view(ask(daemon, 1, serde_json::json!({"op": "list"}))) {
        view::SpacesView::Unavailable { message } => println!("СПЕЙСЫ НЕДОСТУПНЫ: {message}"),
        view::SpacesView::Spaces(rows) => {
            for row in &rows {
                let reason = if row.level_reason.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", row.level_reason)
                };
                println!(
                    "СПЕЙС {} · {} · {} · профиль {} · {} · уровень {}{}",
                    row.id, row.label, row.color, row.profile, row.state, row.level, reason
                );
                print_detail(daemon, &row.id);
            }
        }
    }
}

fn print_detail(daemon: &daemon::Daemon, id: &str) {
    let reply = ask(
        daemon,
        2,
        serde_json::json!({"op": "describe", "space": id}),
    );
    match view::detail_view(reply) {
        view::DetailView::Unavailable { message } => println!("  ДЕТАЛИ НЕДОСТУПНЫ: {message}"),
        view::DetailView::Space { rows, log_tail } => {
            for row in rows {
                println!("  {}: {}", row.key, row.value);
            }
            for line in log_tail {
                println!("  журнал| {line}");
            }
        }
    }
}

fn print_net(daemon: &daemon::Daemon) {
    match view::net_view(ask(daemon, 3, serde_json::json!({"op": "net-status"}))) {
        Err(message) => println!("СЕТЬ НЕДОСТУПНА: {message}"),
        Ok(net) => {
            for row in net.rows {
                println!("СЕТЬ {}: {}", row.key, row.value);
            }
            if !net.ruleset_note.is_empty() {
                println!("СЕТЬ ruleset: {}", net.ruleset_note);
            }
            for line in net.ruleset_text.lines() {
                println!("  nft| {line}");
            }
        }
    }
}
