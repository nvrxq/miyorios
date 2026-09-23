#![forbid(unsafe_code)]

use anyhow::{bail, Context, Result};
use miyori_proto::control::{read_request, Responder, REQUEST_READ_TIMEOUT_SECS};
use miyorid::lookup_gid;
use miyorid::ops;
use miyorid::render::{self, RegistryEntry};
use miyorid::store::Store;
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_STATE_DIR: &str = "/var/lib/miyorios";
const DEFAULT_RUN_DIR: &str = "/run/miyorios";
const DEFAULT_GROUP: &str = "miyori";
const DEFAULT_REGISTRY: &str = "/etc/miyorios/spaces.toml";

enum Kind {
    Nft,
    Registry,
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1).peekable();
    // "render" — единственная подкоманда; всё остальное (включая пусто) — флаги сервера
    if args.peek().map(String::as_str) == Some("render") {
        args.next();
        return run_render(args);
    }
    run_server(args)
}

fn run_render(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut registry: Option<PathBuf> = None;
    let mut state_dir: Option<PathBuf> = None;
    let mut kind = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--registry" => {
                registry = Some(args.next().context("--registry без значения")?.into());
            }
            "--state-dir" => {
                state_dir = Some(args.next().context("--state-dir без значения")?.into());
            }
            "--kind" => {
                kind = Some(match args.next().context("--kind без значения")?.as_str() {
                    "nft" => Kind::Nft,
                    "registry" => Kind::Registry,
                    other => bail!("--kind ожидает nft или registry, получено {other:?}"),
                });
            }
            other => bail!("неизвестный аргумент {other}"),
        }
    }
    if registry.is_none() && state_dir.is_none() {
        bail!("нужен --registry <файл> либо --state-dir <каталог>");
    }
    let kind = kind.context("нужен --kind nft|registry")?;

    let from_registry = match &registry {
        Some(path) => render::load_registry_file(path)?,
        None => Vec::new(),
    };
    let entries = match &state_dir {
        Some(dir) => merge_entries(from_registry, entries_from_state_dir(dir)?)?,
        None => from_registry,
    };

    let text = match kind {
        Kind::Nft => render::render_nft(&entries)?,
        Kind::Registry => render::render_registry(&entries),
    };
    print!("{text}");
    Ok(())
}

fn entries_from_state_dir(dir: &Path) -> Result<Vec<RegistryEntry>> {
    let store = Store::load(dir)?;
    Ok(store
        .list()
        .iter()
        .map(|c| RegistryEntry {
            id: c.id.clone(),
            cid: c.cid,
            label: c.label.clone(),
            color: c.color.clone(),
        })
        .collect())
}

// cid — метка доверия от хоста (ADR 0002): тихая склейка двух разных спейсов на один cid недопустима
fn merge_entries(a: Vec<RegistryEntry>, b: Vec<RegistryEntry>) -> Result<Vec<RegistryEntry>> {
    let mut merged: Vec<RegistryEntry> = Vec::with_capacity(a.len() + b.len());
    'entries: for entry in a.into_iter().chain(b) {
        for existing in &merged {
            if existing.cid == entry.cid && existing.id == entry.id {
                continue 'entries;
            }
            if existing.cid == entry.cid {
                bail!(
                    "конфликт cid {}: id {:?} и {:?} указывают один и тот же cid",
                    entry.cid,
                    existing.id,
                    entry.id
                );
            }
            if existing.id == entry.id {
                bail!(
                    "конфликт id {:?}: cid {} и {} назначены одному id",
                    entry.id,
                    existing.cid,
                    entry.cid
                );
            }
        }
        merged.push(entry);
    }
    merged.sort_by_key(|e| e.cid);
    Ok(merged)
}

struct ServerConfig {
    state_dir: PathBuf,
    run_dir: PathBuf,
    group: String,
    registry: PathBuf,
    build_uid: Option<u32>,
}

fn run_server(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut config = ServerConfig {
        state_dir: PathBuf::from(DEFAULT_STATE_DIR),
        run_dir: PathBuf::from(DEFAULT_RUN_DIR),
        group: DEFAULT_GROUP.to_string(),
        registry: PathBuf::from(DEFAULT_REGISTRY),
        build_uid: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state-dir" => {
                config.state_dir = args.next().context("--state-dir без значения")?.into();
            }
            "--run-dir" => {
                config.run_dir = args.next().context("--run-dir без значения")?.into();
            }
            "--group" => {
                config.group = args.next().context("--group без значения")?;
            }
            // отдельный путь для теста: иначе он перезапишет боевой /etc/miyorios/spaces.toml оператора
            "--registry" => {
                config.registry = args.next().context("--registry без значения")?.into();
            }
            // по умолчанию build() берёт владельца <state>/profiles — это subuid-диапазон оператора, не root
            "--build-uid" => {
                let raw = args.next().context("--build-uid без значения")?;
                config.build_uid = Some(
                    raw.parse()
                        .with_context(|| format!("--build-uid {raw:?} не является числом"))?,
                );
            }
            other => bail!("неизвестный аргумент {other}"),
        }
    }

    // пустая строка — режим тестов: группу не меняем вовсе, а не ищем группу с пустым именем
    let gid = if config.group.is_empty() {
        None
    } else {
        Some(lookup_gid(Path::new("/etc/group"), &config.group)?)
    };

    std::fs::create_dir_all(&config.run_dir)
        .with_context(|| format!("не удалось создать {}", config.run_dir.display()))?;
    let socket_path = config.run_dir.join("control.sock");
    match std::fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("не удалось удалить старый {}", socket_path.display()))
        }
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("не удалось создать сокет {}", socket_path.display()))?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o660))
        .with_context(|| format!("не удалось выставить права на {}", socket_path.display()))?;

    if gid.is_none() {
        // молчаливый сокет с чужой группой — это менеджер, который не дозвонится, и никто не поймёт почему
        eprintln!(
            "miyorid: ПРЕДУПРЕЖДЕНИЕ: --group пуст, группа сокета {} осталась прежней; это режим тестов",
            socket_path.display()
        );
    }
    if let Some(gid) = gid {
        // не root — chown не сработает; демон обязан быть шумным об этом, а не тихо оставить старую группу
        if let Err(err) = std::os::unix::fs::chown(&socket_path, None, Some(gid)) {
            eprintln!(
                "miyorid: ПРЕДУПРЕЖДЕНИЕ: не удалось сменить группу сокета {} на {:?} ({err}); демон запущен не от root?",
                socket_path.display(),
                config.group
            );
        }
    }

    eprintln!("miyorid: слушает {}", socket_path.display());

    // один Ctx на весь процесс: мутирующие операции сериализуются его замком (гонка за CID недопустима)
    let ctx = std::sync::Arc::new(ops::Ctx::new(
        config.state_dir.clone(),
        config.run_dir.clone(),
        config.registry.clone(),
        config.build_uid,
        gid,
    ));

    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("miyorid: ошибка приёма соединения: {err}");
                continue;
            }
        };
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            // паника обработчика не должна утащить за собой процесс: поток и так изолирован,
            // но перехват здесь превращает её в строку в логе, а не в тихо оборванное соединение
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle_connection(stream, &ctx)
            }));
            if let Err(payload) = outcome {
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "неизвестная паника".to_string());
                eprintln!("miyorid: обработчик соединения запаниковал: {message}");
            }
        });
    }
    Ok(())
}

fn handle_connection(mut stream: UnixStream, ctx: &ops::Ctx) {
    if let Err(err) = stream.set_read_timeout(Some(Duration::from_secs(REQUEST_READ_TIMEOUT_SECS)))
    {
        eprintln!("miyorid: не удалось выставить тайм-аут чтения: {err}");
        return;
    }
    // чтение кадра — отдельное неизменяемое заимствование &stream; к моменту создания Responder-а оно уже кончилось
    let request = {
        let mut reader = BufReader::new(&stream);
        read_request(&mut reader)
    };
    // Responder создаётся ДО dispatch и живёт здесь: dispatch получает лишь &mut для progress,
    // а finish (self по значению) отсюда не позвать — терминальный кадр гарантирует система типов
    let mut responder = Responder::new(&mut stream);
    let outcome = match request {
        Ok(req) => ops::dispatch(ctx, req, &mut responder),
        Err(err) => Err((err.code, err.message)),
    };
    if let Err(err) = responder.finish(outcome) {
        eprintln!("miyorid: не удалось отправить ответ клиенту: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use miyori_proto::ids::{Color, Label, SpaceId};

    fn entry(id: &str, cid: u32) -> RegistryEntry {
        RegistryEntry {
            id: SpaceId::new(id).unwrap(),
            cid,
            label: Label::new("untrusted").unwrap(),
            color: Color::new("#e03131").unwrap(),
        }
    }

    #[test]
    fn merge_unions_two_sources_sorted_by_cid() {
        let a = vec![entry("web", 7), entry("spike", 3)];
        let b = vec![entry("dev", 5), entry("spike-reduced", 6)];
        let merged = merge_entries(a, b).unwrap();
        let cids: Vec<u32> = merged.iter().map(|e| e.cid).collect();
        assert_eq!(cids, vec![3, 5, 6, 7]);
    }

    #[test]
    fn merge_collapses_exact_duplicate() {
        let a = vec![entry("spike", 3)];
        let b = vec![entry("spike", 3)];
        let merged = merge_entries(a, b).unwrap();
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn merge_rejects_same_cid_different_id() {
        let a = vec![entry("spike", 3)];
        let b = vec![entry("spike2", 3)];
        // RegistryEntry без Debug: unwrap_err() того же результата не собрать
        let msg = match merge_entries(a, b) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("конфликт cid 3 (spike/spike2) обязан быть ошибкой"),
        };
        assert!(msg.contains("spike"), "{msg}");
        assert!(msg.contains("spike2"), "{msg}");
        assert!(msg.contains('3'), "{msg}");
    }

    #[test]
    fn merge_rejects_same_id_different_cid() {
        let a = vec![entry("spike", 3)];
        let b = vec![entry("spike", 4)];
        let msg = match merge_entries(a, b) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("конфликт id spike (cid 3/4) обязан быть ошибкой"),
        };
        assert!(msg.contains("spike"), "{msg}");
        assert!(msg.contains('3'), "{msg}");
        assert!(msg.contains('4'), "{msg}");
    }
}
