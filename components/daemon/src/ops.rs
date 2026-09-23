#![forbid(unsafe_code)]

use crate::net;
use crate::observe;
use crate::qemu::{self, SpaceRun, SpaceState};
use crate::store::Store;
use anyhow::Context;
use miyori_profile::Manifest;
use miyori_proto::control::{ErrorCode, Request, Responder};
use miyori_proto::ids::{Color, Label, SpaceId};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;

type OpResult = Result<serde_json::Value, (ErrorCode, String)>;

// нужен от setpriv для доступа к /dev/kvm и /dev/vhost-vsock; общий для всех спейсов (решение B)
const KVM_GROUP: &str = "kvm";

// демон живёт одним процессом на сокет: держит хэндлы QEMU и сериализует мутирующие операции
pub struct Ctx {
    state_dir: PathBuf,
    run_dir: PathBuf,
    registry_path: PathBuf,
    // None — брать владельца <state>/profiles на каждый build; так его переопределяет --build-uid
    build_uid: Option<u32>,
    // None — тот же режим тестов, что у --group "" в main.rs: группу сокета менять не у кого
    socket_gid: Option<u32>,
    // сериализует create/start/stop/destroy (иначе два create выдадут один CID); отравление игнорируем — данных замок не охраняет
    lock: Mutex<()>,
    // без хэндла stop не reap'нул бы QEMU без зомби; отравление игнорируем — insert/remove карты неделимы
    children: Mutex<HashMap<String, Child>>,
}

impl Ctx {
    pub fn new(
        state_dir: PathBuf,
        run_dir: PathBuf,
        registry_path: PathBuf,
        build_uid: Option<u32>,
        socket_gid: Option<u32>,
    ) -> Self {
        Self {
            state_dir,
            run_dir,
            registry_path,
            build_uid,
            socket_gid,
            lock: Mutex::new(()),
            children: Mutex::new(HashMap::new()),
        }
    }
}

// responder идёт в build() только ради progress; finish() Responder не видит и не может — терминальный кадр один
pub fn dispatch<W: Write>(ctx: &Ctx, req: Request, responder: &mut Responder<W>) -> OpResult {
    match req {
        Request::List {} => list(ctx),
        Request::Describe { space } => describe(ctx, &space),
        Request::Build { profile } => build(ctx, profile, responder),
        Request::Create {
            space,
            profile,
            label,
            color,
            seed,
            passphrase,
        } => create(ctx, space, profile, label, color, seed, passphrase),
        Request::Start { space, passphrase } => start(ctx, space, passphrase, responder),
        Request::Stop { space } => stop(ctx, space),
        Request::OpenWindow { space } => open_window(ctx, space),
        Request::ResetSystem { space, passphrase } => reset_system(ctx, space, passphrase),
        Request::ResetAll { space, passphrase } => reset_all(ctx, space, passphrase),
        Request::UpdateImage { space, passphrase } => update_image(ctx, space, passphrase),
        Request::Destroy { space } => destroy(ctx, space),
        Request::NetStatus {} => net_status(),
        Request::Profiles {} => profiles(ctx),
    }
}

// только наблюдение — мосты, vfio-pci и запуск miyori-net демону не принадлежат (решение C, задача 11)
fn net_status() -> OpResult {
    let taps: Vec<_> = net::list_space_taps()
        .into_iter()
        .map(|t| serde_json::json!({ "name": t.name, "isolated": t.isolated }))
        .collect();

    let pci = net::pci_status(&net::uplink_pci_addr());
    let miyori_net = net::miyori_net_status();
    let tunnel = miyori_net
        .tunnel
        .map(|t| serde_json::json!({ "ifc": t.ifc, "set": t.set, "route": t.route }));
    let ruleset = net::nft_ruleset();

    Ok(serde_json::json!({
        "bridges": {
            (net::BRIDGE): net::bridge_exists(),
            (net::CAPTIVE_BRIDGE): net::link_exists(net::CAPTIVE_BRIDGE),
        },
        "taps": taps,
        "uplink-pci": {
            "address": pci.address,
            "present": pci.present,
            "driver": pci.driver,
        },
        "miyori-net": {
            "running": miyori_net.running,
            "pid": miyori_net.pid,
            "uplink": miyori_net.uplink,
            "tunnel": tunnel,
        },
        "ruleset": {
            "available": ruleset.available,
            "text": ruleset.text,
            "truncated-from": ruleset.truncated_from,
        },
        // счётчики цепочек kill-switch живут внутри miyori-net; команд агента для них нет (спека §7)
        "killswitch_counters": "недоступны с хоста",
    }))
}

fn list(ctx: &Ctx) -> OpResult {
    let store = load_store(&ctx.state_dir)?;
    let mut spaces = Vec::new();
    for space in store.list() {
        // сломанный манифест одного профиля не должен прятать все спейсы разом — profiles устроен так же
        let (level, level_reason) = match load_manifest(&ctx.state_dir, space.profile.as_ref()) {
            Ok(manifest) => (manifest.isolation.level, manifest.isolation.reason),
            Err(_) => (
                "неизвестен".to_string(),
                "манифест профиля не читается".to_string(),
            ),
        };
        let state = observed_state(ctx, space.id.as_ref(), space.cid);
        spaces.push(serde_json::json!({
            "id": space.id.as_ref(),
            "profile": space.profile.as_ref(),
            "state": state_label(state),
            "cid": space.cid,
            "isolation-level": level,
            "isolation-reason": level_reason,
            "label": space.label.as_ref(),
            "color": space.color.as_ref(),
            "encrypted": space.encrypted,
        }));
    }
    Ok(serde_json::json!(spaces))
}

// только чтение, как list/describe/net-status — менеджер спрашивает это ДО create, ctx.lock тут не нужен
fn profiles(ctx: &Ctx) -> OpResult {
    let profiles_dir = ctx.state_dir.join("profiles");
    let mut names: Vec<String> = match std::fs::read_dir(&profiles_dir) {
        Ok(read) => read
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect(),
        // каталога нет — профилей просто нет; но нечитаемый каталог отдать пустым списком нельзя,
        // это соврало бы менеджеру "профилей не заведено" вместо "я их не вижу"
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => {
            log_internal(
                "profiles: каталог профилей не читается",
                &anyhow::Error::new(err),
            );
            return Err(internal_error());
        }
    };
    names.sort();

    let items: Vec<_> = names
        .iter()
        .map(|name| describe_profile(&ctx.state_dir, name))
        .collect();
    Ok(serde_json::json!(items))
}

// сломанный манифест не должен ронять всю операцию и не должен молча пропадать из списка
fn describe_profile(state_dir: &Path, profile: &str) -> serde_json::Value {
    let manifest_path = state_dir
        .join("profiles")
        .join(profile)
        .join("manifest.toml");
    let template = resolve_template_digest(state_dir, profile).is_ok();
    match Manifest::load(&manifest_path) {
        Ok(manifest) => serde_json::json!({
            "profile": profile,
            "template": template,
            "manifest-ok": true,
            "isolation-level": manifest.isolation.level,
            "isolation-reason": manifest.isolation.reason,
            "command": manifest.app.command,
            "manifest-error": null,
        }),
        Err(err) => {
            log_internal(
                &format!("profiles: манифест профиля \"{profile}\" не прочитан"),
                &err,
            );
            serde_json::json!({
                "profile": profile,
                "template": template,
                "manifest-ok": false,
                "isolation-level": null,
                "isolation-reason": null,
                "command": null,
                "manifest-error": format!("{err:#}"),
            })
        }
    }
}

fn describe(ctx: &Ctx, id: &SpaceId) -> OpResult {
    let store = load_store(&ctx.state_dir)?;
    let space = space_or_not_found(&store, id)?;
    let manifest = load_manifest(&ctx.state_dir, space.profile.as_ref())?;
    let manifest_path = ctx
        .state_dir
        .join("profiles")
        .join(space.profile.as_ref())
        .join("manifest.toml");
    let data_qcow2 = ctx
        .state_dir
        .join("spaces")
        .join(id.as_ref())
        .join("data.qcow2");
    let data_qcow2_bytes = std::fs::metadata(&data_qcow2).map(|m| m.len()).ok();
    let state = observed_state(ctx, space.id.as_ref(), space.cid);

    // rss/uptime — только для найденного живого pid; иначе показывались бы факты о мёртвом процессе
    let pid = qemu::pid_of(&ctx.run_dir, id.as_ref(), space.cid);
    let rss_kib = pid.and_then(observe::rss_kib);
    let uptime_secs = pid.and_then(|_| {
        let pid_path = ctx.run_dir.join(id.as_ref()).join("qemu.pid");
        observe::uptime_secs(&pid_path, std::time::SystemTime::now())
    });
    let log_path = ctx
        .state_dir
        .join("spaces")
        .join(id.as_ref())
        .join("last-run.log");
    let log_tail = observe::tail_sanitized(
        &log_path,
        observe::LOG_TAIL_MAX_LINES,
        observe::LOG_TAIL_MAX_LINE_CHARS,
    );

    Ok(serde_json::json!({
        "id": space.id.as_ref(),
        "profile": space.profile.as_ref(),
        "state": state_label(state),
        "cid": space.cid,
        "uid": space.uid,
        "digest": space.digest,
        "label": space.label.as_ref(),
        "color": space.color.as_ref(),
        "encrypted": space.encrypted,
        "created": space.created,
        "description": manifest.description,
        "app": {
            "mode": manifest.app.mode,
            "command": manifest.app.command,
        },
        "resources": {
            "memory-mb": manifest.resources.memory_mb,
            "cpus": manifest.resources.cpus,
            "disk-gb": manifest.resources.disk_gb,
            "data-mb": manifest.resources.data_mb,
        },
        "data-qcow2-bytes": data_qcow2_bytes,
        "manifest-path": manifest_path.display().to_string(),
        "isolation": {
            "level": manifest.isolation.level,
            "gpu": manifest.isolation.gpu,
            "reason": manifest.isolation.reason,
        },
        "network": {
            "via": manifest.network.via,
        },
        "clean-shutdown": space.clean_shutdown,
        "rss-kib": rss_kib,
        "uptime-secs": uptime_secs,
        "log-tail": log_tail,
    }))
}

fn create(
    ctx: &Ctx,
    id: SpaceId,
    profile: SpaceId,
    label: Label,
    color: Color,
    seed: Option<String>,
    passphrase: Option<String>,
) -> OpResult {
    let _guard = ctx.lock.lock().unwrap_or_else(|poison| poison.into_inner());

    // пустая строка — ошибка запроса, а не "без шифрования": тихого пропуска шифрования быть не должно
    if passphrase.as_deref() == Some("") {
        return Err((
            ErrorCode::BadRequest,
            "пароль спейса не может быть пустым".to_string(),
        ));
    }
    let encrypted = passphrase.is_some();

    if id.as_ref() == crate::render::MIYORI_NET_ID {
        return Err((
            ErrorCode::BadRequest,
            format!("id \"{id}\" зарезервирован за сетевой VM"),
        ));
    }

    let mut store = load_store(&ctx.state_dir)?;
    if store.get(&id).is_some() {
        return Err((
            ErrorCode::SpaceExists,
            format!("спейс \"{id}\" уже существует"),
        ));
    }

    let manifest = load_manifest(&ctx.state_dir, profile.as_ref())?;
    let digest = resolve_template_digest(&ctx.state_dir, profile.as_ref())?;

    // валидируем и берём build_uid/build_gid до create_space: без seed ни один из этих вызовов не происходит
    let seed_spec = match seed {
        Some(raw) => {
            let dir = validate_seed_dir(&raw, manifest.resources.data_mb)?;
            let build_uid = resolve_build_uid(ctx)?;
            let build_gid = resolve_build_gid(build_uid)?;
            Some(SeedSpec {
                dir,
                build_uid,
                build_gid,
            })
        }
        None => None,
    };

    let config = store
        .create_space(
            id.clone(),
            profile.clone(),
            digest.clone(),
            label,
            color,
            encrypted,
        )
        .map_err(|err| {
            log_internal("не удалось выделить cid/uid спейсу", &err);
            internal_error()
        })?
        .clone();

    let template_dir = template_dir_path(&ctx.state_dir, profile.as_ref(), &digest);
    let space_dir = ctx.state_dir.join("spaces").join(id.as_ref());

    // секрет qemu-img живёт только на время томов ниже — cleanup идёт до проверки результата, как в seed_data_volume
    let outcome: anyhow::Result<()> = (|| {
        let qemu_img_secret = match &passphrase {
            Some(pp) => Some(write_qemu_img_secret(&ctx.run_dir, pp)?),
            None => None,
        };
        let result = create_volumes(
            &template_dir,
            &space_dir,
            config.uid,
            manifest.resources.data_mb,
            seed_spec.as_ref(),
            qemu_img_secret.as_deref(),
        );
        if let Some(path) = &qemu_img_secret {
            cleanup_qemu_img_secret(path);
        }
        result?;
        net::refresh(&store, &ctx.registry_path)
    })();

    if let Err(err) = outcome {
        // конфиг без исправного тома хуже отсутствующего спейса — откатываем, а не оставляем половину
        let _ = store.remove_space(&id);
        log_internal(
            "create: откат после ошибки томов или применения правил",
            &err,
        );
        return Err(internal_error());
    }

    Ok(serde_json::json!({
        "id": config.id.as_ref(),
        "cid": config.cid,
        "uid": config.uid,
    }))
}

fn build<W: Write>(ctx: &Ctx, profile: SpaceId, responder: &mut Responder<W>) -> OpResult {
    // профиля нет — код profile-not-found, а не internal: клиент должен уметь отличить "нечего собирать"
    let _ = load_manifest(&ctx.state_dir, profile.as_ref())?;

    let build_uid = resolve_build_uid(ctx)?;
    let build_gid = resolve_build_gid(build_uid)?;
    let build_home = resolve_build_home(build_uid)?;
    let profile_dir = ctx.state_dir.join("profiles").join(profile.as_ref());
    // pid один на весь демон — уникальность на КОНКРЕТНЫЙ build даёт id потока: у каждого соединения свой
    let staging = ctx
        .state_dir
        .join("templates")
        .join(".staging")
        .join(format!(
            "{}-{}-{:?}",
            profile.as_ref(),
            std::process::id(),
            std::thread::current().id()
        ));

    if let Err(err) = prepare_staging_dir(&staging, build_uid) {
        log_internal("build: не удалось подготовить каталог сборки", &err);
        return Err(internal_error());
    }

    let argv = crate::build::build_argv(build_uid, build_gid, &profile_dir);
    let build_result = crate::build::run_streaming(&argv, &staging, &build_home, |line| {
        let _ = responder.progress(line);
    });
    if let Err(err) = build_result {
        log_internal("build: сборщик завершился неудачей", &err);
        cleanup_staging(&staging);
        return Err(internal_error());
    }

    let digest = match crate::build::digest_from_out_dir(&staging, profile.as_ref()) {
        Ok(digest) => digest,
        Err(err) => {
            log_internal(
                "build: не удалось определить digest собранного шаблона",
                &err,
            );
            cleanup_staging(&staging);
            return Err(internal_error());
        }
    };

    let built_dir = staging.join(profile.as_ref()).join(&digest);
    let final_dir = template_dir_path(&ctx.state_dir, profile.as_ref(), &digest);
    if let Err(err) = finalize_template(&built_dir, &final_dir) {
        log_internal("build: не удалось уложить шаблон под root", &err);
        cleanup_staging(&staging);
        return Err(internal_error());
    }

    // latest переезжает на новый digest; прежние каталоги не трогаем — GC шаблонов не входит в эту задачу
    let templates_dir = ctx.state_dir.join("templates").join(profile.as_ref());
    if let Err(err) = relink_latest(&templates_dir, &digest) {
        log_internal("build: не удалось обновить latest", &err);
    }

    cleanup_staging(&staging);

    Ok(serde_json::json!({
        "profile": profile.as_ref(),
        "digest": digest,
        "path": final_dir.display().to_string(),
    }))
}

// build-uid по умолчанию — владелец <state>/profiles: тот же субъект, у которого есть subuid-диапазон (ADR-5)
fn resolve_build_uid(ctx: &Ctx) -> Result<u32, (ErrorCode, String)> {
    if let Some(uid) = ctx.build_uid {
        return Ok(uid);
    }
    let profiles_dir = ctx.state_dir.join("profiles");
    std::fs::metadata(&profiles_dir)
        .map(|meta| meta.uid())
        .map_err(|err| {
            log_internal(
                &format!("не удалось определить владельца {}", profiles_dir.display()),
                &err.into(),
            );
            internal_error()
        })
}

// newuidmap сверяет gid процесса с ПЕРВИЧНЫМ gid пользователя из passwd, а не с владельцем каталога — другой источник рано или поздно разойдётся с ним и развалит unshare user namespace
fn resolve_build_gid(build_uid: u32) -> Result<u32, (ErrorCode, String)> {
    crate::lookup_passwd_entry(Path::new("/etc/passwd"), build_uid)
        .map(|entry| entry.gid)
        .map_err(|err| {
            log_internal(
                &format!("не удалось определить gid для uid {build_uid}"),
                &err,
            );
            internal_error()
        })
}

// без явного HOME сборщик наследует HOME демона (root); отказ здесь честнее, чем rustup,
// упирающийся в /root/.rustup чуть позже и куда невнятнее (задача 10)
fn resolve_build_home(build_uid: u32) -> Result<String, (ErrorCode, String)> {
    crate::lookup_passwd_entry(Path::new("/etc/passwd"), build_uid)
        .map(|entry| entry.home)
        .map_err(|err| {
            log_internal(
                &format!("не удалось определить HOME для uid {build_uid}"),
                &err,
            );
            internal_error()
        })
}

// сборщик пишет не от root — каталог должен принадлежать build_uid ещё до его запуска
fn prepare_staging_dir(staging: &Path, build_uid: u32) -> anyhow::Result<()> {
    std::fs::create_dir_all(staging)
        .with_context(|| format!("не удалось создать {}", staging.display()))?;
    std::os::unix::fs::chown(staging, Some(build_uid), None)
        .with_context(|| format!("не удалось сменить владельца {}", staging.display()))
}

fn cleanup_staging(staging: &Path) {
    let _ = std::fs::remove_dir_all(staging);
}

// build-profile.sh уже выставил 0444/0555 (ловушка "a-w ломает kernel" их не касается — режим он не трогает);
// здесь только перенос на постоянное место и владелец root, ровно то, что задача 10 разрешает делать от root
fn finalize_template(built_dir: &Path, final_dir: &Path) -> anyhow::Result<()> {
    if final_dir.is_dir() {
        // тот же digest уже уложен параллельной сборкой — переносить нечего, содержимое то же самое
        return Ok(());
    }
    let parent = final_dir
        .parent()
        .context("у каталога шаблона должен быть родитель")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("не удалось создать {}", parent.display()))?;
    std::fs::rename(built_dir, final_dir).with_context(|| {
        format!(
            "не удалось перенести {} в {}",
            built_dir.display(),
            final_dir.display()
        )
    })?;
    own_dir_and_its_entries_as_root(final_dir)
}

// lchown, не chown: запись каталога может оказаться симлинком, положенным туда build_uid'ом, и chown сменил
// бы владельца её цели, а не самой ссылки; каталог шаблона плоский (см. finalize_template) — рекурсия не нужна
fn own_dir_and_its_entries_as_root(dir: &Path) -> anyhow::Result<()> {
    std::os::unix::fs::lchown(dir, Some(0), Some(0))
        .with_context(|| format!("не удалось сменить владельца {}", dir.display()))?;
    for entry in std::fs::read_dir(dir).with_context(|| format!("не читается {}", dir.display()))?
    {
        let entry = entry.context("не читается запись каталога шаблона")?;
        std::os::unix::fs::lchown(entry.path(), Some(0), Some(0))
            .with_context(|| format!("не удалось сменить владельца {}", entry.path().display()))?;
    }
    Ok(())
}

fn relink_latest(templates_dir: &Path, digest: &str) -> anyhow::Result<()> {
    let latest = templates_dir.join("latest");
    let _ = std::fs::remove_file(&latest);
    std::os::unix::fs::symlink(digest, &latest)
        .with_context(|| format!("не удалось создать {}", latest.display()))
}

// столько QEMU хватает, чтобы упасть на неоткрытом диске или занятом CID
const SPAWN_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);
// на реальном железе гость успевает поднять агента; в юнит-тестах слушателя нет вовсе, и ждать нечего
const AGENT_START_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);
// быстрее реагируем на подъём агента, чем длится сам AGENT_HEALTH_TIMEOUT, но не долбим порт впустую
const AGENT_START_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(300);

// QEMU уже жив — его не откатываем; параметризованный дедлайн даёт юнит-тестам не ждать AGENT_START_DEADLINE целиком
fn start_app_via_agent(
    cid: u32,
    nonce: &str,
    deadline: std::time::Duration,
    alive: impl Fn() -> bool,
    mut on_progress: impl FnMut(&str),
) -> String {
    let started = std::time::Instant::now();
    loop {
        match crate::agent::health(cid, nonce, crate::agent::AGENT_HEALTH_TIMEOUT) {
            Ok(()) => break,
            Err(_) if started.elapsed() < deadline => {
                // мёртвый QEMU агентом уже не станет: без этой проверки неверный пароль ждал бы весь дедлайн
                if !alive() {
                    return "qemu не дожил до ответа агента".to_string();
                }
                on_progress("ждём агента гостя");
                std::thread::sleep(AGENT_START_POLL_INTERVAL);
            }
            Err(err) => return format!("агент не ответил за отведённое время: {err}"),
        }
    }

    match crate::agent::exec_app(cid, nonce, crate::agent::AGENT_EXEC_APP_TIMEOUT) {
        Ok(reply) if reply.ok => "started".to_string(),
        Ok(reply) => reply
            .detail
            .unwrap_or_else(|| "агент отказался запускать приложение".to_string()),
        Err(err) => format!("не удалось запустить приложение: {err}"),
    }
}

// группе сокета управления открыт только подкаталог gui: она равна uid менеджера, а он в п.21
// модели угроз недоверенная сторона, и qemu.pid с nonce из общего каталога он бы переписывал.
// Режим выставляется безусловно: каталог мог остаться от прежнего запуска с чужими правами
fn prepare_space_run_dir(dir: &Path, gid: Option<u32>) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    let gui = dir.join(miyori_config::GUI_SUBDIR);
    std::fs::create_dir_all(&gui)?;
    if let Some(gid) = gid {
        std::os::unix::fs::chown(&gui, None, Some(gid))?;
        std::fs::set_permissions(&gui, std::fs::Permissions::from_mode(0o770))?;
    }
    Ok(())
}

// пустой nonce совпал бы с пустым полем в ответе гостя, и подтверждение пришло бы ни от кого:
// отсутствие файла — это отказ, а не пустая строка
fn read_nonce(run_dir: &Path, id: &str) -> std::io::Result<String> {
    let nonce = std::fs::read_to_string(run_dir.join(id).join("nonce"))?
        .trim()
        .to_string();
    if nonce.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "nonce спейса пуст",
        ));
    }
    Ok(nonce)
}

fn start<W: Write>(
    ctx: &Ctx,
    id: SpaceId,
    passphrase: Option<String>,
    responder: &mut Responder<W>,
) -> OpResult {
    let _guard = ctx.lock.lock().unwrap_or_else(|poison| poison.into_inner());

    let store = load_store(&ctx.state_dir)?;
    let space = space_or_not_found(&store, &id)?.clone();
    require_passphrase_matches_encryption(space.encrypted, &passphrase)?;

    if qemu::state_of(&ctx.run_dir, id.as_ref(), space.cid) == SpaceState::Running {
        return Err((
            ErrorCode::SpaceRunning,
            format!("спейс \"{id}\" уже запущен"),
        ));
    }

    if !net::bridge_exists() {
        return Err((
            ErrorCode::NoBridge,
            format!(
                "нет моста \"{}\": сначала подними фикстуру — sudo bash components/net/net-fixture.sh up",
                net::BRIDGE
            ),
        ));
    }

    let manifest = load_manifest(&ctx.state_dir, space.profile.as_ref())?;
    let template_dir = template_dir_path(&ctx.state_dir, space.profile.as_ref(), &space.digest);
    if !template_dir.join("root.qcow2").is_file() {
        return Err((
            ErrorCode::TemplateNotFound,
            format!("шаблон {} отсутствует или неполон", template_dir.display()),
        ));
    }

    let kvm_gid = lookup_kvm_gid().map_err(|err| {
        log_internal("не найдена группа kvm", &err);
        internal_error()
    })?;
    let nonce = generate_nonce().map_err(|err| {
        log_internal("не удалось сгенерировать nonce", &err);
        internal_error()
    })?;

    let run_space_dir = ctx.run_dir.join(id.as_ref());
    let saved = prepare_space_run_dir(&run_space_dir, ctx.socket_gid)
        .with_context(|| format!("не удалось подготовить {}", run_space_dir.display()))
        .and_then(|()| write_nonce(&run_space_dir.join("nonce"), &nonce));
    if let Err(err) = saved {
        log_internal("не удалось сохранить nonce", &err);
        return Err(internal_error());
    }

    // выше уже подтверждено, что спейс не Running — значит owner'а у tap-space-<cid> нет и снять его безопасно
    if let Err(err) = net::reap_leftover_tap(space.cid) {
        log_internal("не удалось снять оставшийся tap перед запуском", &err);
        return Err(internal_error());
    }

    if let Err(err) = net::create_tap(space.cid, space.uid) {
        log_internal("не удалось поднять tap", &err);
        return Err(internal_error());
    }

    // живёт всё время работы QEMU (её читает uid спейса, не root) — убирается в stop и при любом отказе ниже
    let secret_file = match &passphrase {
        Some(pp) => match write_running_secret(&ctx.run_dir, id.as_ref(), space.uid, pp) {
            Ok(path) => Some(path),
            Err(err) => {
                let _ = net::delete_tap(space.cid);
                log_internal("не удалось подготовить файл секрета", &err);
                return Err(internal_error());
            }
        },
        None => None,
    };

    let space_dir = ctx.state_dir.join("spaces").join(id.as_ref());
    let run = SpaceRun {
        id: id.as_ref().to_string(),
        cid: space.cid,
        uid: space.uid,
        template_dir,
        overlay: space_dir.join("system-overlay.qcow2"),
        data: space_dir.join("data.qcow2"),
        data_mb: manifest.resources.data_mb,
        memory_mb: manifest.resources.memory_mb,
        cpus: manifest.resources.cpus,
        isolation_level: manifest.isolation.level.clone(),
        gpu: manifest.isolation.gpu.clone(),
        nonce,
        app_command: manifest.app.command.clone(),
        kvm_gid,
    };

    match qemu::spawn(&run, &ctx.run_dir, &ctx.state_dir, secret_file.as_deref()) {
        Ok(mut child) => {
            // QEMU падает на неоткрытом диске за миллисекунды; без этой паузы start отвечал бы
            // ok на мертворождённый процесс, а спейс числился бы running до первой же проверки
            std::thread::sleep(SPAWN_SETTLE);
            if let Ok(Some(status)) = child.try_wait() {
                let _ = net::delete_tap(space.cid);
                let _ = std::fs::remove_file(ctx.run_dir.join(id.as_ref()).join("qemu.pid"));
                remove_running_secret(&ctx.run_dir, id.as_ref());
                let log = space_dir.join("last-run.log");
                let tail = tail_of(&log);
                eprintln!(
                    "miyorid: qemu спейса \"{id}\" завершился сразу со статусом {status}; хвост {}:\n{}",
                    log.display(),
                    tail
                );
                // неверный пароль умирает так же быстро, как незашифрованный QEMU на битом диске — отличаем по тексту QEMU
                if qemu_died_of_wrong_passphrase(&tail) {
                    return Err((
                        ErrorCode::WrongPassphrase,
                        format!("неверный пароль спейса \"{id}\""),
                    ));
                }
                return Err((
                    ErrorCode::Internal,
                    format!("QEMU спейса \"{id}\" завершился сразу после запуска; причина в журнале последнего запуска"),
                ));
            }
            ctx.children
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(id.as_ref().to_string(), child);

            let children = &ctx.children;
            let watched = id.as_ref().to_string();
            let app = start_app_via_agent(
                space.cid,
                &run.nonce,
                AGENT_START_DEADLINE,
                || {
                    children
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .get_mut(&watched)
                        .map(|child| matches!(child.try_wait(), Ok(None)))
                        .unwrap_or(false)
                },
                |line| {
                    let _ = responder.progress(line);
                },
            );

            // LUKS-разбор пароля стоит секунды (iter-time), а не миллисекунды — SPAWN_SETTLE его не ловит;
            // агент не ответивший за весь AGENT_START_DEADLINE мог просто не ответить, потому что QEMU уже мёртв
            if app != "started" {
                let still_alive = ctx
                    .children
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .get_mut(id.as_ref())
                    .map(|child| matches!(child.try_wait(), Ok(None)))
                    .unwrap_or(false);

                if !still_alive {
                    kill_and_reap(ctx, id.as_ref());
                    let _ = net::delete_tap(space.cid);
                    remove_running_secret(&ctx.run_dir, id.as_ref());
                    let log = space_dir.join("last-run.log");
                    let tail = tail_of(&log);
                    eprintln!(
                        "miyorid: qemu спейса \"{id}\" умер, пока ждали агента (отчёт: {app}); хвост {}:\n{}",
                        log.display(),
                        tail
                    );
                    if qemu_died_of_wrong_passphrase(&tail) {
                        return Err((
                            ErrorCode::WrongPassphrase,
                            format!("неверный пароль спейса \"{id}\""),
                        ));
                    }
                    return Err((
                        ErrorCode::Internal,
                        format!("QEMU спейса \"{id}\" умер во время запуска; причина в журнале последнего запуска"),
                    ));
                }
            }

            Ok(serde_json::json!({
                "id": id.as_ref(),
                "state": "running",
                "cid": space.cid,
                "app": app,
            }))
        }
        Err(err) => {
            let _ = net::delete_tap(space.cid);
            remove_running_secret(&ctx.run_dir, id.as_ref());
            log_internal("не удалось запустить qemu", &err);
            Err(internal_error())
        }
    }
}

// в лог демона, не клиенту: строки QEMU содержат пути состояния
fn tail_of(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

// ровно текст, которым QEMU отличает неверный пароль от прочих причин ранней смерти (проверено на этой машине)
fn qemu_died_of_wrong_passphrase(log_tail: &str) -> bool {
    log_tail.contains("Invalid password, cannot unlock any keyslot")
}

// пароль обязателен ровно когда спейс зашифрован — общее правило для start/reset-system/reset-all/update-image
fn require_passphrase_matches_encryption(
    encrypted: bool,
    passphrase: &Option<String>,
) -> Result<(), (ErrorCode, String)> {
    match (encrypted, passphrase.is_some()) {
        (true, false) => Err((ErrorCode::BadRequest, "спейсу нужен пароль".to_string())),
        (false, true) => Err((ErrorCode::BadRequest, "спейс не зашифрован".to_string())),
        _ => Ok(()),
    }
}

fn stop(ctx: &Ctx, id: SpaceId) -> OpResult {
    let _guard = ctx.lock.lock().unwrap_or_else(|poison| poison.into_inner());

    let mut store = load_store(&ctx.state_dir)?;
    let space = space_or_not_found(&store, &id)?.clone();

    if qemu::state_of(&ctx.run_dir, id.as_ref(), space.cid) != SpaceState::Running {
        return Err((
            ErrorCode::SpaceStopped,
            format!("спейс \"{id}\" не запущен"),
        ));
    }

    // §6.1: сперва просим агента погасить приложение/sync/umount, и только потом убиваем QEMU
    let (graceful, detail) = shutdown_via_agent(&ctx.run_dir, id.as_ref(), space.cid);

    kill_and_reap(ctx, id.as_ref());

    if let Err(err) = net::delete_tap(space.cid) {
        log_internal("не удалось удалить tap при остановке", &err);
        return Err(internal_error());
    }

    remove_running_secret(&ctx.run_dir, id.as_ref());

    if let Err(err) = store.set_clean_shutdown(&id, graceful) {
        log_internal("не удалось записать признак аварийного останова", &err);
        return Err(internal_error());
    }

    Ok(serde_json::json!({
        "id": id.as_ref(),
        "state": "stopped",
        "clean-shutdown": graceful,
        "detail": detail,
    }))
}

// тайм-аут, молчание и неверный nonce неотличимы для клиента control.sock: любое сомнение — аварийный останов
fn shutdown_via_agent(run_dir: &Path, id: &str, cid: u32) -> (bool, String) {
    let nonce = match read_nonce(run_dir, id) {
        Ok(nonce) => nonce,
        Err(err) => return (false, format!("nonce спейса не читается: {err}")),
    };
    match crate::agent::shutdown(cid, &nonce, crate::agent::AGENT_SHUTDOWN_TIMEOUT) {
        Ok(reply) if reply.ok && reply.data_unmounted == Some(true) => (
            true,
            reply
                .detail
                .unwrap_or_else(|| "том отмонтирован".to_string()),
        ),
        Ok(reply) => (
            false,
            reply
                .detail
                .unwrap_or_else(|| "агент не подтвердил размонтирование тома".to_string()),
        ),
        Err(err) => (false, format!("агент недостижим: {err}")),
    }
}

// гость не умеет выключить себя без окна (sysrq под acpi=off не работает), а agent::exec_app
// не портит уже открытые окна — второе соединение просто берёт ещё одну аренду waypipe (broker.rs)
fn open_window(ctx: &Ctx, id: SpaceId) -> OpResult {
    let _guard = ctx.lock.lock().unwrap_or_else(|poison| poison.into_inner());

    let store = load_store(&ctx.state_dir)?;
    let space = space_or_not_found(&store, &id)?.clone();

    if qemu::state_of(&ctx.run_dir, id.as_ref(), space.cid) != SpaceState::Running {
        return Err((
            ErrorCode::SpaceStopped,
            format!("спейс \"{id}\" не запущен"),
        ));
    }

    let nonce = read_nonce(&ctx.run_dir, id.as_ref()).map_err(|err| {
        (
            ErrorCode::AgentTimeout,
            format!("nonce спейса не читается: {err}"),
        )
    })?;

    match crate::agent::exec_app(space.cid, &nonce, crate::agent::AGENT_EXEC_APP_TIMEOUT) {
        Ok(reply) if reply.ok => Ok(serde_json::json!({
            "id": id.as_ref(),
            "detail": reply.detail.unwrap_or_else(|| "окно открыто".to_string()),
        })),
        Ok(reply) => Err((
            ErrorCode::AgentTimeout,
            reply
                .detail
                .unwrap_or_else(|| "агент отказался открывать окно".to_string()),
        )),
        Err(err) => Err((ErrorCode::AgentTimeout, format!("агент недостижим: {err}"))),
    }
}

// подменять том под живым QEMU — порча данных, поэтому обе reset-операции требуют состояния Stopped
fn reject_if_running(ctx: &Ctx, id: &SpaceId, cid: u32) -> Result<(), (ErrorCode, String)> {
    if qemu::state_of(&ctx.run_dir, id.as_ref(), cid) == SpaceState::Running {
        return Err((
            ErrorCode::SpaceRunning,
            format!("спейс \"{id}\" запущен, сначала stop"),
        ));
    }
    Ok(())
}

fn reset_system(ctx: &Ctx, id: SpaceId, passphrase: Option<String>) -> OpResult {
    let _guard = ctx.lock.lock().unwrap_or_else(|poison| poison.into_inner());

    let store = load_store(&ctx.state_dir)?;
    let space = space_or_not_found(&store, &id)?.clone();
    require_passphrase_matches_encryption(space.encrypted, &passphrase)?;
    reject_if_running(ctx, &id, space.cid)?;

    let template_dir = template_dir_path(&ctx.state_dir, space.profile.as_ref(), &space.digest);
    let space_dir = ctx.state_dir.join("spaces").join(id.as_ref());
    let overlay = space_dir.join("system-overlay.qcow2");

    let qemu_img_secret = match &passphrase {
        Some(pp) => Some(write_qemu_img_secret(&ctx.run_dir, pp).map_err(|err| {
            log_internal("reset-system: не удалось подготовить файл секрета", &err);
            internal_error()
        })?),
        None => None,
    };
    let result = qemu_img_overlay(
        &template_dir.join("root.qcow2"),
        &overlay,
        qemu_img_secret.as_deref(),
    )
    .and_then(|()| own_and_lock_down(&overlay, space.uid));
    if let Some(path) = &qemu_img_secret {
        cleanup_qemu_img_secret(path);
    }

    if let Err(err) = result {
        log_internal(
            "reset-system: не удалось пересоздать system-overlay.qcow2",
            &err,
        );
        return Err(internal_error());
    }

    Ok(serde_json::json!({ "id": id.as_ref() }))
}

fn reset_all(ctx: &Ctx, id: SpaceId, passphrase: Option<String>) -> OpResult {
    let _guard = ctx.lock.lock().unwrap_or_else(|poison| poison.into_inner());

    let store = load_store(&ctx.state_dir)?;
    let space = space_or_not_found(&store, &id)?.clone();
    require_passphrase_matches_encryption(space.encrypted, &passphrase)?;
    reject_if_running(ctx, &id, space.cid)?;

    let manifest = load_manifest(&ctx.state_dir, space.profile.as_ref())?;
    let template_dir = template_dir_path(&ctx.state_dir, space.profile.as_ref(), &space.digest);
    let space_dir = ctx.state_dir.join("spaces").join(id.as_ref());

    let qemu_img_secret = match &passphrase {
        Some(pp) => Some(write_qemu_img_secret(&ctx.run_dir, pp).map_err(|err| {
            log_internal("reset-all: не удалось подготовить файл секрета", &err);
            internal_error()
        })?),
        None => None,
    };
    // seed нет и не будет: путь к файлам оператора нигде не хранится после create — переспросить их негде
    let result = create_volumes(
        &template_dir,
        &space_dir,
        space.uid,
        manifest.resources.data_mb,
        None,
        qemu_img_secret.as_deref(),
    );
    if let Some(path) = &qemu_img_secret {
        cleanup_qemu_img_secret(path);
    }

    if let Err(err) = result {
        log_internal("reset-all: не удалось пересоздать тома спейса", &err);
        return Err(internal_error());
    }

    Ok(serde_json::json!({ "id": id.as_ref() }))
}

// принимает свежий шаблон профиля, но не reset — /data остаётся тем же томом ни при каких условиях
fn update_image(ctx: &Ctx, id: SpaceId, passphrase: Option<String>) -> OpResult {
    let _guard = ctx.lock.lock().unwrap_or_else(|poison| poison.into_inner());

    let mut store = load_store(&ctx.state_dir)?;
    let space = space_or_not_found(&store, &id)?.clone();
    require_passphrase_matches_encryption(space.encrypted, &passphrase)?;
    reject_if_running(ctx, &id, space.cid)?;

    let new_digest = resolve_latest_digest(&ctx.state_dir, space.profile.as_ref())?;

    if new_digest == space.digest {
        return Ok(serde_json::json!({
            "id": id.as_ref(),
            "from": space.digest,
            "to": new_digest,
            "detail": "спейс уже на текущем образе",
        }));
    }

    let template_dir = template_dir_path(&ctx.state_dir, space.profile.as_ref(), &new_digest);
    if !template_dir_is_complete(&template_dir) {
        return Err((
            ErrorCode::TemplateNotFound,
            format!("шаблон {} отсутствует или неполон", template_dir.display()),
        ));
    }

    let qemu_img_secret = match &passphrase {
        Some(pp) => Some(write_qemu_img_secret(&ctx.run_dir, pp).map_err(|err| {
            log_internal("update-image: не удалось подготовить файл секрета", &err);
            internal_error()
        })?),
        None => None,
    };
    // оверлей меняется раньше digest: при сбое между шагами повтор увидит старый digest и доведёт дело сам, а обратный порядок соврал бы, что переезд уже случился
    let space_dir = ctx.state_dir.join("spaces").join(id.as_ref());
    let overlay = space_dir.join("system-overlay.qcow2");
    let result = qemu_img_overlay(
        &template_dir.join("root.qcow2"),
        &overlay,
        qemu_img_secret.as_deref(),
    )
    .and_then(|()| own_and_lock_down(&overlay, space.uid));
    if let Some(path) = &qemu_img_secret {
        cleanup_qemu_img_secret(path);
    }

    if let Err(err) = result {
        log_internal(
            "update-image: не удалось пересоздать system-overlay.qcow2",
            &err,
        );
        return Err(internal_error());
    }

    if let Err(err) = store.set_digest(&id, new_digest.clone()) {
        log_internal("update-image: не удалось записать новый digest", &err);
        return Err(internal_error());
    }

    Ok(serde_json::json!({
        "id": id.as_ref(),
        "from": space.digest,
        "to": new_digest,
        "detail": "образ обновлён, /data не тронут",
    }))
}

fn destroy(ctx: &Ctx, id: SpaceId) -> OpResult {
    let _guard = ctx.lock.lock().unwrap_or_else(|poison| poison.into_inner());

    let mut store = load_store(&ctx.state_dir)?;
    let space = space_or_not_found(&store, &id)?.clone();

    if qemu::state_of(&ctx.run_dir, id.as_ref(), space.cid) == SpaceState::Running {
        return Err((
            ErrorCode::SpaceRunning,
            format!("спейс \"{id}\" запущен, сначала stop"),
        ));
    }

    // stop уже снял tap штатно; если QEMU погиб мимо демона, tap остался бы висеть и навсегда сжёг CID (store::allocate_cid)
    let tap = crate::space_tap(space.cid);
    if net::link_exists(&tap) {
        if let Err(err) = net::delete_tap(space.cid) {
            log_internal("не удалось снять tap при destroy", &err);
            return Err(internal_error());
        }
    }

    store.remove_space(&id).map_err(|err| {
        log_internal("не удалось удалить каталог спейса", &err);
        internal_error()
    })?;

    if let Err(err) = net::refresh(&store, &ctx.registry_path) {
        log_internal(
            "не удалось перерендерить реестр/правила после destroy",
            &err,
        );
        return Err(internal_error());
    }

    Ok(serde_json::json!({ "id": id.as_ref() }))
}

fn state_label(state: SpaceState) -> &'static str {
    match state {
        SpaceState::Running => "running",
        SpaceState::Stopped => "stopped",
        SpaceState::Unresponsive => "unresponsive",
    }
}

// только для отчёта клиенту (list/describe); гварды start/stop/destroy остаются на qemu::state_of —
// им нужен факт живого процесса, а не живого агента, и без сетевого похода
fn observed_state(ctx: &Ctx, id: &str, cid: u32) -> SpaceState {
    crate::agent::observed_state(&ctx.run_dir, id, cid, crate::agent::AGENT_HEALTH_TIMEOUT)
}

fn space_or_not_found<'a>(
    store: &'a Store,
    id: &SpaceId,
) -> Result<&'a crate::store::SpaceConfig, (ErrorCode, String)> {
    store.get(id).ok_or_else(|| {
        (
            ErrorCode::SpaceNotFound,
            format!("спейс \"{id}\" не найден"),
        )
    })
}

// хэндл есть — reap'аем сами; хэндла нет — даемон перезапускался, зомби уже не наша забота, а init'а
fn kill_and_reap(ctx: &Ctx, id: &str) {
    let handle = ctx
        .children
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(id);
    match handle {
        Some(mut child) => {
            let _ = child.kill();
            let _ = child.wait();
        }
        None => {
            if let Ok(raw) = std::fs::read_to_string(ctx.run_dir.join(id).join("qemu.pid")) {
                if let Ok(pid) = raw.trim().parse::<u32>() {
                    let _ = Command::new("kill")
                        .args(["-KILL", &pid.to_string()])
                        .status();
                }
            }
        }
    }
    let _ = std::fs::remove_file(ctx.run_dir.join(id).join("qemu.pid"));
}

fn template_dir_path(state_dir: &Path, profile: &str, digest: &str) -> PathBuf {
    state_dir.join("templates").join(profile).join(digest)
}

// digest — latest, если это символическая ссылка, иначе единственный каталог; расхождение — отказ
fn resolve_template_digest(state_dir: &Path, profile: &str) -> Result<String, (ErrorCode, String)> {
    let templates_dir = state_dir.join("templates").join(profile);
    let latest = templates_dir.join("latest");

    let via_latest = std::fs::symlink_metadata(&latest)
        .ok()
        .filter(|meta| meta.file_type().is_symlink())
        .and_then(|_| std::fs::read_link(&latest).ok())
        .and_then(|target| target.file_name().map(|n| n.to_string_lossy().into_owned()));
    if let Some(digest) = via_latest {
        return Ok(digest);
    }

    let mut candidates = Vec::new();
    if let Ok(read) = std::fs::read_dir(&templates_dir) {
        for entry in read.flatten() {
            if entry.file_name() == "latest" {
                continue;
            }
            if entry.path().is_dir() {
                candidates.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }

    match candidates.as_slice() {
        [only] => Ok(only.clone()),
        _ => Err((
            ErrorCode::TemplateNotFound,
            format!(
                "для профиля \"{profile}\" не найден ровно один шаблон в {}; собери: bash tools/build-profile.sh profiles/{profile}",
                templates_dir.display()
            ),
        )),
    }
}

// в отличие от resolve_template_digest (create/start), тут нет запасной эвристики "единственный каталог" — кнопка обязана везти строго на latest
fn resolve_latest_digest(state_dir: &Path, profile: &str) -> Result<String, (ErrorCode, String)> {
    let templates_dir = state_dir.join("templates").join(profile);
    let latest = templates_dir.join("latest");
    std::fs::symlink_metadata(&latest)
        .ok()
        .filter(|meta| meta.file_type().is_symlink())
        .and_then(|_| std::fs::read_link(&latest).ok())
        .and_then(|target| target.file_name().map(|n| n.to_string_lossy().into_owned()))
        .ok_or_else(|| {
            (
                ErrorCode::TemplateNotFound,
                format!(
                    "для профиля \"{profile}\" нет latest в {}; собери: bash tools/build-profile.sh profiles/{profile}",
                    templates_dir.display()
                ),
            )
        })
}

fn template_dir_is_complete(dir: &Path) -> bool {
    ["root.qcow2", "vmlinuz", "initrd.img", "MANIFEST"]
        .iter()
        .all(|name| dir.join(name).is_file())
}

// каталог оператора и учётка, под которой его читает mkfs.ext4 (не root, см. seed_data_volume)
struct SeedSpec {
    dir: PathBuf,
    build_uid: u32,
    build_gid: u32,
}

// data_mb == 0 проверен раньше (validate_seed_dir); путь обязан быть абсолютным существующим каталогом
fn validate_seed_dir(raw: &str, data_mb: u32) -> Result<PathBuf, (ErrorCode, String)> {
    if data_mb == 0 {
        return Err((
            ErrorCode::BadRequest,
            "у профиля data_mb = 0: класть присланные файлы некуда".to_string(),
        ));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err((
            ErrorCode::BadRequest,
            format!("seed \"{raw}\" должен быть абсолютным путём"),
        ));
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(path.to_path_buf()),
        Ok(_) => Err((
            ErrorCode::BadRequest,
            format!("seed \"{raw}\" не является каталогом"),
        )),
        Err(_) => Err((
            ErrorCode::BadRequest,
            format!("seed \"{raw}\" не существует"),
        )),
    }
}

fn create_volumes(
    template_dir: &Path,
    space_dir: &Path,
    uid: u32,
    data_mb: u32,
    seed: Option<&SeedSpec>,
    secret: Option<&Path>,
) -> anyhow::Result<()> {
    let overlay = space_dir.join("system-overlay.qcow2");
    qemu_img_overlay(&template_dir.join("root.qcow2"), &overlay, secret)?;
    own_and_lock_down(&overlay, uid)?;

    if data_mb > 0 {
        let data = space_dir.join("data.qcow2");
        match seed {
            Some(spec) => seed_data_volume(space_dir, &data, data_mb, spec, secret)?,
            None => qemu_img_blank(&data, data_mb, secret)?,
        }
        own_and_lock_down(&data, uid)?;
    }
    Ok(())
}

// setpriv тем же приёмом, что crate::build::build_argv: чтение файлов оператора не должно идти от root
fn mkfs_seed_argv(spec: &SeedSpec, raw: &Path) -> Vec<String> {
    vec![
        "setpriv".into(),
        "--reuid".into(),
        spec.build_uid.to_string(),
        "--regid".into(),
        spec.build_gid.to_string(),
        "--clear-groups".into(),
        "--".into(),
        "mkfs.ext4".into(),
        "-q".into(),
        "-d".into(),
        spec.dir.display().to_string(),
        raw.display().to_string(),
    ]
}

// staging — тот же приём, что build(): root готовит каталог и пустой raw, build_uid читает seed при mkfs, root доводит до qcow2
fn seed_data_volume(
    space_dir: &Path,
    data: &Path,
    data_mb: u32,
    spec: &SeedSpec,
    secret: Option<&Path>,
) -> anyhow::Result<()> {
    let staging = space_dir.join(".seed-staging");
    prepare_staging_dir(&staging, spec.build_uid)?;
    // каталог спейса читаем всем: без 0700 файлы оператора на время засева видны qemu чужих спейсов
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("не удалось выставить права {}", staging.display()))?;

    let result = (|| -> anyhow::Result<()> {
        let raw = staging.join("data.raw");
        truncate_raw(&raw, data_mb)?;
        // raw создан root'ом внутри каталога build_uid — без chown build_uid не сможет его открыть на запись
        std::os::unix::fs::chown(&raw, Some(spec.build_uid), Some(spec.build_gid))
            .with_context(|| format!("не удалось сменить владельца {}", raw.display()))?;

        let argv = mkfs_seed_argv(spec, &raw);
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .context("не удалось запустить mkfs.ext4 для засева /data")?;
        if !status.success() {
            anyhow::bail!("mkfs.ext4 -d {} завершился с {status}", spec.dir.display());
        }

        qemu_img_convert(&raw, data, secret)
    })();

    cleanup_staging(&staging);
    result
}

fn truncate_raw(path: &Path, size_mb: u32) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)
        .with_context(|| format!("не удалось создать {}", path.display()))?;
    file.set_len(u64::from(size_mb) * 1024 * 1024)
        .with_context(|| format!("не удалось выставить размер {}", path.display()))
}

// секрет — путь к файлу, читаемому qemu-img; аргументы отдельно от исполнения, как qemu::build_argv
fn qemu_img_convert_argv(raw: &Path, qcow2: &Path, secret: Option<&Path>) -> Vec<String> {
    let mut argv = vec!["qemu-img".to_string(), "convert".to_string()];
    if let Some(secret) = secret {
        argv.push("--object".to_string());
        argv.push(format!(
            "secret,id=sec0,file={},format=raw",
            secret.display()
        ));
    }
    argv.push("-O".to_string());
    argv.push("qcow2".to_string());
    if secret.is_some() {
        argv.push("-o".to_string());
        argv.push("encrypt.format=luks,encrypt.key-secret=sec0".to_string());
    }
    argv.push(raw.display().to_string());
    argv.push(qcow2.display().to_string());
    argv
}

fn qemu_img_convert(raw: &Path, qcow2: &Path, secret: Option<&Path>) -> anyhow::Result<()> {
    let argv = qemu_img_convert_argv(raw, qcow2, secret);
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .context("не удалось запустить qemu-img convert для засеянного data.qcow2")?;
    if !status.success() {
        anyhow::bail!("qemu-img convert {} завершился с {status}", qcow2.display());
    }
    Ok(())
}

fn qemu_img_overlay_argv(backing: &Path, overlay: &Path, secret: Option<&Path>) -> Vec<String> {
    let mut argv = vec!["qemu-img".to_string(), "create".to_string()];
    if let Some(secret) = secret {
        argv.push("--object".to_string());
        argv.push(format!(
            "secret,id=sec0,file={},format=raw",
            secret.display()
        ));
    }
    argv.push("-f".to_string());
    argv.push("qcow2".to_string());
    argv.push("-b".to_string());
    argv.push(backing.display().to_string());
    argv.push("-F".to_string());
    argv.push("qcow2".to_string());
    if secret.is_some() {
        argv.push("-o".to_string());
        argv.push("encrypt.format=luks,encrypt.key-secret=sec0".to_string());
    }
    argv.push(overlay.display().to_string());
    argv
}

fn qemu_img_overlay(backing: &Path, overlay: &Path, secret: Option<&Path>) -> anyhow::Result<()> {
    let argv = qemu_img_overlay_argv(backing, overlay, secret);
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .context("не удалось запустить qemu-img create")?;
    if !status.success() {
        anyhow::bail!(
            "qemu-img create {} завершился с {status}",
            overlay.display()
        );
    }
    Ok(())
}

fn qemu_img_blank_argv(path: &Path, size_mb: u32, secret: Option<&Path>) -> Vec<String> {
    let mut argv = vec!["qemu-img".to_string(), "create".to_string()];
    if let Some(secret) = secret {
        argv.push("--object".to_string());
        argv.push(format!(
            "secret,id=sec0,file={},format=raw",
            secret.display()
        ));
    }
    argv.push("-f".to_string());
    argv.push("qcow2".to_string());
    if secret.is_some() {
        argv.push("-o".to_string());
        argv.push("encrypt.format=luks,encrypt.key-secret=sec0".to_string());
    }
    argv.push(path.display().to_string());
    argv.push(format!("{size_mb}M"));
    argv
}

fn qemu_img_blank(path: &Path, size_mb: u32, secret: Option<&Path>) -> anyhow::Result<()> {
    let argv = qemu_img_blank_argv(path, size_mb, secret);
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .context("не удалось запустить qemu-img create для data.qcow2")?;
    if !status.success() {
        anyhow::bail!("qemu-img create {} завершился с {status}", path.display());
    }
    Ok(())
}

fn own_and_lock_down(path: &Path, uid: u32) -> anyhow::Result<()> {
    std::os::unix::fs::chown(path, Some(uid), None)
        .with_context(|| format!("не удалось сменить владельца {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("не удалось выставить права {}", path.display()))?;
    Ok(())
}

// секрет отдаётся во владение uid спейса, а /run/miyorios/<спейс> — состояние демона, и владеть им гость не должен
fn secrets_dir(run_dir: &Path) -> PathBuf {
    run_dir.join("secrets")
}

fn space_secret_dir(run_dir: &Path, id: &str) -> PathBuf {
    secrets_dir(run_dir).join(id)
}

// файл секрета для qemu-system: живёт всё время работы VM, убирается в stop() и при любом неудачном start()
fn write_running_secret(
    run_dir: &Path,
    id: &str,
    uid: u32,
    passphrase: &str,
) -> anyhow::Result<PathBuf> {
    let root = secrets_dir(run_dir);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("не удалось создать {}", root.display()))?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o711))
        .with_context(|| format!("не удалось выставить права {}", root.display()))?;

    let dir = space_secret_dir(run_dir, id);
    // от прошлого неудачного start могла остаться грязь — начинаем с чистого каталога
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("не удалось создать {}", dir.display()))?;
    // запираем до записи: между fs::write и chmod файла каталог не должен быть проходим никому лишнему
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("не удалось выставить права {}", dir.display()))?;

    let file = dir.join("pass");
    // без завершающего перевода строки: QEMU берёт содержимое файла секрета байт в байт
    std::fs::write(&file, passphrase)
        .with_context(|| format!("не удалось записать {}", file.display()))?;
    std::os::unix::fs::chown(&file, Some(uid), None)
        .with_context(|| format!("не удалось сменить владельца {}", file.display()))?;
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400))
        .with_context(|| format!("не удалось выставить права {}", file.display()))?;

    // каталог запираем последним: до этого момента демон (root) ещё пишет в него файл секрета
    std::os::unix::fs::chown(&dir, Some(uid), None)
        .with_context(|| format!("не удалось сменить владельца {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500))
        .with_context(|| format!("не удалось выставить права {}", dir.display()))?;

    Ok(file)
}

fn remove_running_secret(run_dir: &Path, id: &str) {
    let dir = space_secret_dir(run_dir, id);
    // 0500 не даёт удалить "pass" изнутри — возвращаем себе право на запись перед remove_dir_all
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    let _ = std::fs::remove_dir_all(&dir);
}

// временный секрет для qemu-img (работает от root): tmpfs, не диск; удаляется сразу после команды
fn write_qemu_img_secret(run_dir: &Path, passphrase: &str) -> anyhow::Result<PathBuf> {
    let dir = run_dir.join(".qemu-img-secrets");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("не удалось создать {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("не удалось выставить права {}", dir.display()))?;

    let path = dir.join(format!(
        "sec-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&path, passphrase)
        .with_context(|| format!("не удалось записать {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("не удалось выставить права {}", path.display()))?;
    Ok(path)
}

fn cleanup_qemu_img_secret(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn generate_nonce() -> anyhow::Result<String> {
    let mut buf = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .context("не открылся /dev/urandom")?
        .read_exact(&mut buf)
        .context("не удалось прочитать /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn write_nonce(path: &Path, nonce: &str) -> anyhow::Result<()> {
    std::fs::write(path, nonce)
        .with_context(|| format!("не удалось записать {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("не удалось выставить права {}", path.display()))
}

fn lookup_kvm_gid() -> anyhow::Result<u32> {
    crate::lookup_gid(Path::new("/etc/group"), KVM_GROUP)
}

fn load_store(state_dir: &Path) -> Result<Store, (ErrorCode, String)> {
    Store::load(state_dir).map_err(|err| {
        log_internal("не удалось загрузить хранилище спейсов", &err);
        internal_error()
    })
}

fn load_manifest(state_dir: &Path, profile: &str) -> Result<Manifest, (ErrorCode, String)> {
    let path = state_dir
        .join("profiles")
        .join(profile)
        .join("manifest.toml");
    if !path.is_file() {
        return Err((
            ErrorCode::ProfileNotFound,
            format!("профиль \"{profile}\" не найден"),
        ));
    }
    Manifest::load(&path).map_err(|err| {
        log_internal(
            &format!("не удалось разобрать манифест {}", path.display()),
            &err,
        );
        internal_error()
    })
}

// текст ошибки в лог, не клиенту: он не должен рассказывать недоверенному клиенту про пути и состояние
fn log_internal(context: &str, err: &anyhow::Error) {
    eprintln!("miyorid: {context}: {err:#}");
}

fn internal_error() -> (ErrorCode, String) {
    (ErrorCode::Internal, "внутренняя ошибка демона".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SpaceConfig;
    use std::path::PathBuf;
    use std::process::Child;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("miyorid-ops-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx(state_dir: &Path) -> Ctx {
        let run_dir = state_dir.join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        Ctx::new(
            state_dir.to_path_buf(),
            run_dir,
            state_dir.join("registry.toml"),
            None,
            None,
        )
    }

    // тестам ниже кадры progress не нужны — заворачиваем dispatch() в одноразовый Responder, а не
    // переписываем два десятка вызовов; имя намеренно затеняет super::dispatch (glob-импорт слабее)
    fn dispatch(ctx: &Ctx, req: Request) -> OpResult {
        let mut sink = Vec::new();
        let mut responder = Responder::new(&mut sink);
        super::dispatch(ctx, req, &mut responder)
    }

    fn write_manifest(state_dir: &Path, profile: &str) {
        let dir = state_dir.join("profiles").join(profile);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = r#"
id = "spike"
description = "проба"

[base]
builder = "mmdebstrap"
suite = "noble"
sources = ["deb http://archive.ubuntu.com/ubuntu noble main"]
packages = ["ca-certificates"]

[app]
mode = "app"
command = "/bin/true"

[resources]
memory_mb = 1024
cpus = 2
disk_gb = 8
data_mb = 4096

[isolation]
level = "standard"
gpu = "none"

[network]
via = "miyori-net"
"#;
        std::fs::write(dir.join("manifest.toml"), manifest).unwrap();
    }

    // data_mb = 0 — ровно та ситуация, в которой класть seed некуда
    fn write_manifest_no_data(state_dir: &Path, profile: &str) {
        let dir = state_dir.join("profiles").join(profile);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = r#"
id = "spike"
description = "проба"

[base]
builder = "mmdebstrap"
suite = "noble"
sources = ["deb http://archive.ubuntu.com/ubuntu noble main"]
packages = ["ca-certificates"]

[app]
mode = "app"
command = "/bin/true"

[resources]
memory_mb = 1024
cpus = 2
disk_gb = 8
data_mb = 0

[isolation]
level = "standard"
gpu = "none"

[network]
via = "miyori-net"
"#;
        std::fs::write(dir.join("manifest.toml"), manifest).unwrap();
    }

    fn write_manifest_reduced(state_dir: &Path, profile: &str, reason: &str) {
        let dir = state_dir.join("profiles").join(profile);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = format!(
            r#"
id = "spike"
description = "проба"

[base]
builder = "mmdebstrap"
suite = "noble"
sources = ["deb http://archive.ubuntu.com/ubuntu noble main"]
packages = ["ca-certificates"]

[app]
mode = "session"
command = "/usr/bin/startxfce4"

[resources]
memory_mb = 1024
cpus = 2
disk_gb = 8
data_mb = 4096

[isolation]
level = "reduced"
gpu = "virtio-gpu-venus"
reason = "{reason}"

[network]
via = "miyori-net"
"#
        );
        std::fs::write(dir.join("manifest.toml"), manifest).unwrap();
    }

    // манифест, который не является корректным TOML — profiles должен пережить это, а не молча выкинуть профиль
    fn write_broken_manifest(state_dir: &Path, profile: &str) {
        let dir = state_dir.join("profiles").join(profile);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.toml"), b"this is not [ valid toml").unwrap();
    }

    fn write_space(state_dir: &Path, id: &str, profile: &str, cid: u32, uid: u32) {
        write_space_with_encryption(state_dir, id, profile, cid, uid, false);
    }

    fn write_encrypted_space(state_dir: &Path, id: &str, profile: &str, cid: u32, uid: u32) {
        write_space_with_encryption(state_dir, id, profile, cid, uid, true);
    }

    fn write_space_with_encryption(
        state_dir: &Path,
        id: &str,
        profile: &str,
        cid: u32,
        uid: u32,
        encrypted: bool,
    ) {
        let dir = state_dir.join("spaces").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let config = SpaceConfig {
            id: SpaceId::new(id).unwrap(),
            profile: SpaceId::new(profile).unwrap(),
            digest: "d".repeat(64),
            cid,
            uid,
            label: Label::new("untrusted").unwrap(),
            color: Color::new("#e03131").unwrap(),
            created: "2026-08-26T00:00:00Z".to_string(),
            clean_shutdown: None,
            encrypted,
        };
        let toml = basic_toml::to_string(&config).unwrap();
        std::fs::write(dir.join("config.toml"), toml).unwrap();
    }

    fn write_template(state_dir: &Path, profile: &str, digest: &str) {
        let dir = state_dir.join("templates").join(profile).join(digest);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("root.qcow2"), b"fake").unwrap();
        std::fs::write(dir.join("vmlinuz"), b"fake").unwrap();
        std::fs::write(dir.join("initrd.img"), b"fake").unwrap();
    }

    fn link_latest(state_dir: &Path, profile: &str, digest: &str) {
        let templates_dir = state_dir.join("templates").join(profile);
        std::os::unix::fs::symlink(digest, templates_dir.join("latest")).unwrap();
    }

    // update-image требует ещё и MANIFEST — отдельная функция, чтобы не менять фикстуру write_template под другими тестами
    fn write_full_template(state_dir: &Path, profile: &str, digest: &str) {
        write_template(state_dir, profile, digest);
        let dir = state_dir.join("templates").join(profile).join(digest);
        std::fs::write(dir.join("MANIFEST"), b"fake").unwrap();
    }

    fn create_request(space: &str, profile: &str) -> Request {
        Request::Create {
            space: SpaceId::new(space).unwrap(),
            profile: SpaceId::new(profile).unwrap(),
            label: Label::new("untrusted").unwrap(),
            color: Color::new("#e03131").unwrap(),
            seed: None,
            passphrase: None,
        }
    }

    fn create_request_with_seed(space: &str, profile: &str, seed: &str) -> Request {
        Request::Create {
            space: SpaceId::new(space).unwrap(),
            profile: SpaceId::new(profile).unwrap(),
            label: Label::new("untrusted").unwrap(),
            color: Color::new("#e03131").unwrap(),
            seed: Some(seed.to_string()),
            passphrase: None,
        }
    }

    fn create_request_with_passphrase(space: &str, profile: &str, passphrase: &str) -> Request {
        Request::Create {
            space: SpaceId::new(space).unwrap(),
            profile: SpaceId::new(profile).unwrap(),
            label: Label::new("untrusted").unwrap(),
            color: Color::new("#e03131").unwrap(),
            seed: None,
            passphrase: Some(passphrase.to_string()),
        }
    }

    // подделывает "живой QEMU" без root: sleep под именем/аргументом, где qemu::state_of ищет свои подстроки
    fn spawn_fake_qemu(dir: &Path, cid: u32) -> Child {
        let path = dir.join(format!("qemu-system-x86_64-guest-cid={cid}"));
        std::os::unix::fs::symlink("/bin/sleep", &path).unwrap();
        Command::new(&path).arg("300").spawn().unwrap()
    }

    fn mark_running(c: &Ctx, id: &str, cid: u32, dir: &Path) -> Child {
        std::fs::create_dir_all(c.run_dir.join(id)).unwrap();
        let child = spawn_fake_qemu(dir, cid);
        std::fs::write(c.run_dir.join(id).join("qemu.pid"), child.id().to_string()).unwrap();
        // между fork и execve cmdline на мгновение принадлежит тесту, а не sleep — ждём настоящий exec
        for _ in 0..200 {
            if qemu::state_of(&c.run_dir, id, cid) == SpaceState::Running {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        child
    }

    fn reap(mut child: Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn list_is_empty_without_spaces() {
        let dir = temp_dir("list-empty");
        let data = dispatch(&ctx(&dir), Request::List {}).unwrap();
        assert_eq!(data, serde_json::json!([]));
    }

    // находка живой проверки: правка манифеста руками делала весь список спейсов пустым
    #[test]
    fn list_survives_a_broken_manifest_of_one_profile() {
        let dir = temp_dir("list-broken-manifest");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        std::fs::write(
            dir.join("profiles").join("spike").join("manifest.toml"),
            "это не toml =",
        )
        .unwrap();

        let data = dispatch(&ctx(&dir), Request::List {}).unwrap();
        assert_eq!(data[0]["id"], "a");
        assert_eq!(data[0]["isolation-level"], "неизвестен");
    }

    #[test]
    fn profiles_is_empty_without_profiles_dir() {
        let dir = temp_dir("profiles-empty");
        let data = dispatch(&ctx(&dir), Request::Profiles {}).unwrap();
        assert_eq!(data, serde_json::json!([]));
    }

    #[test]
    fn profiles_reports_template_readiness_and_sorts_by_name() {
        let dir = temp_dir("profiles-readiness");
        write_manifest(&dir, "spike");
        write_template(&dir, "spike", &"d".repeat(64));
        link_latest(&dir, "spike", &"d".repeat(64));
        write_manifest_reduced(&dir, "unbuilt", "полноэкранная графика");

        let data = dispatch(&ctx(&dir), Request::Profiles {}).unwrap();
        let items = data.as_array().unwrap();
        assert_eq!(items.len(), 2);

        // отсортировано по имени: "spike" перед "unbuilt"
        assert_eq!(items[0]["profile"], "spike");
        assert_eq!(items[0]["template"], true);
        assert_eq!(items[0]["manifest-ok"], true);
        assert_eq!(items[0]["isolation-level"], "standard");
        assert_eq!(items[0]["isolation-reason"], "");
        assert_eq!(items[0]["command"], "/bin/true");
        assert!(items[0]["manifest-error"].is_null());

        assert_eq!(items[1]["profile"], "unbuilt");
        assert_eq!(items[1]["template"], false);
        assert_eq!(items[1]["manifest-ok"], true);
        assert_eq!(items[1]["isolation-level"], "reduced");
        assert_eq!(items[1]["isolation-reason"], "полноэкранная графика");
        assert_eq!(items[1]["command"], "/usr/bin/startxfce4");
    }

    #[test]
    fn profiles_reports_broken_manifest_without_dropping_it_or_failing_the_op() {
        let dir = temp_dir("profiles-broken-manifest");
        write_manifest(&dir, "spike");
        write_broken_manifest(&dir, "ghost");

        let data = dispatch(&ctx(&dir), Request::Profiles {}).unwrap();
        let items = data.as_array().unwrap();
        assert_eq!(
            items.len(),
            2,
            "сломанный манифест не должен исчезать из списка"
        );

        let ghost = items.iter().find(|it| it["profile"] == "ghost").unwrap();
        assert_eq!(ghost["manifest-ok"], false);
        assert!(
            ghost["manifest-error"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "должна быть непустая причина отказа"
        );
        assert!(ghost["isolation-level"].is_null());
        assert!(ghost["command"].is_null());

        let spike = items.iter().find(|it| it["profile"] == "spike").unwrap();
        assert_eq!(spike["manifest-ok"], true);
    }

    #[test]
    fn describe_reports_space_not_found() {
        let dir = temp_dir("describe-missing-space");
        let err = dispatch(
            &ctx(&dir),
            Request::Describe {
                space: SpaceId::new("nope").unwrap(),
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::SpaceNotFound);
    }

    #[test]
    fn describe_reports_profile_not_found() {
        let dir = temp_dir("describe-missing-profile");
        write_space(&dir, "a", "ghost", 3, 70003);
        let err = dispatch(
            &ctx(&dir),
            Request::Describe {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::ProfileNotFound);
    }

    #[test]
    fn describe_returns_manifest_sections_verbatim() {
        let dir = temp_dir("describe-happy");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let data = dispatch(
            &ctx(&dir),
            Request::Describe {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap();
        assert_eq!(data["state"], "stopped");
        assert_eq!(data["cid"], 3);
        assert_eq!(data["uid"], 70003);
        assert_eq!(data["digest"], "d".repeat(64));
        assert_eq!(data["isolation"]["level"], "standard");
        assert_eq!(data["isolation"]["gpu"], "none");
        assert_eq!(data["network"]["via"], "miyori-net");
        assert_eq!(data["resources"]["memory-mb"], 1024);
        assert!(data["data-qcow2-bytes"].is_null());
        assert!(data["manifest-path"]
            .as_str()
            .unwrap()
            .ends_with("profiles/spike/manifest.toml"));
    }

    // менеджер называет и красит спейс только этими полями; description и app.command — из манифеста
    #[test]
    fn describe_reports_label_color_description_and_app_command() {
        let dir = temp_dir("describe-label-color");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let data = dispatch(
            &ctx(&dir),
            Request::Describe {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap();
        assert_eq!(data["label"], "untrusted");
        assert_eq!(data["color"], "#e03131");
        assert_eq!(data["description"], "проба");
        assert_eq!(data["app"]["command"], "/bin/true");
    }

    #[test]
    fn describe_reports_encrypted_field() {
        let dir = temp_dir("describe-encrypted");
        write_manifest(&dir, "spike");
        write_encrypted_space(&dir, "a", "spike", 3, 70003);
        let data = dispatch(
            &ctx(&dir),
            Request::Describe {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap();
        assert_eq!(data["encrypted"], true);
    }

    // мёртвый спейс не должен врать: ни памяти, ни аптайма у него нет
    #[test]
    fn describe_reports_null_rss_and_uptime_for_stopped_space() {
        let dir = temp_dir("describe-stopped-rss-uptime");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let data = dispatch(
            &ctx(&dir),
            Request::Describe {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap();
        assert!(data["rss-kib"].is_null());
        assert!(data["uptime-secs"].is_null());
    }

    // без него реализация, всегда отдающая null, прошла бы проверку выше и никто бы не заметил
    #[test]
    fn describe_reports_real_rss_and_uptime_for_running_space() {
        let dir = temp_dir("describe-running-rss-uptime");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let c = ctx(&dir);
        let child = mark_running(&c, "a", 3, &dir);

        let data = dispatch(
            &c,
            Request::Describe {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap();
        reap(child);

        let rss = data["rss-kib"]
            .as_u64()
            .expect("у живого процесса обязан быть RSS");
        assert!(rss > 0, "RSS живого процесса не бывает нулевым: {rss}");
        assert!(
            data["uptime-secs"].as_u64().is_some(),
            "у живого запуска обязан быть аптайм: {}",
            data["uptime-secs"]
        );
    }

    #[test]
    fn describe_reports_log_tail_as_empty_array_without_log() {
        let dir = temp_dir("describe-no-log");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let data = dispatch(
            &ctx(&dir),
            Request::Describe {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap();
        assert_eq!(data["log-tail"], serde_json::json!([]));
    }

    #[test]
    fn describe_reports_actual_data_qcow2_size() {
        let dir = temp_dir("describe-data-size");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        std::fs::write(dir.join("spaces").join("a").join("data.qcow2"), [0u8; 42]).unwrap();
        let data = dispatch(
            &ctx(&dir),
            Request::Describe {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap();
        assert_eq!(data["data-qcow2-bytes"], 42);
    }

    // живой процесс без живого агента — это unresponsive, а не running (задача 7, решение H)
    #[test]
    fn describe_reports_unresponsive_when_process_alive_but_agent_unreachable() {
        let dir = temp_dir("describe-unresponsive");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let c = ctx(&dir);
        let child = mark_running(&c, "a", 3, &dir);
        std::fs::write(c.run_dir.join("a").join("nonce"), "abc123").unwrap();

        let data = dispatch(
            &c,
            Request::Describe {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap();
        reap(child);

        assert_eq!(data["state"], "unresponsive");
    }

    // сквозь весь dispatch(): живой процесс + агент, реально подтвердивший тот же nonce -> running
    #[test]
    fn describe_reports_running_when_agent_confirms_nonce() {
        use std::io::{BufRead, BufReader, Write};
        use vsock::VMADDR_CID_LOCAL;

        // health() всегда звонит на agent::AGENT_PORT — лок и биндинг общие с agent.rs::tests
        let _guard = crate::agent::lock_agent_port_for_test();

        let cid = VMADDR_CID_LOCAL;
        let dir = temp_dir("describe-running-agent");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", cid, 70000 + cid);
        let c = ctx(&dir);
        let child = mark_running(&c, "a", cid, &dir);
        std::fs::write(c.run_dir.join("a").join("nonce"), "abc123").unwrap();

        let listener = crate::agent::bind_agent_port_for_test();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut writer = &stream;
            let _ = writeln!(
                writer,
                r#"{{"nonce":"abc123","ok":true,"detail":null,"data_unmounted":null}}"#
            );
        });

        let data = dispatch(
            &c,
            Request::Describe {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap();
        server.join().unwrap();
        reap(child);

        assert_eq!(data["state"], "running");
    }

    #[test]
    fn list_reports_isolation_level_and_reason() {
        let dir = temp_dir("list-happy");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let data = dispatch(&ctx(&dir), Request::List {}).unwrap();
        assert_eq!(data[0]["id"], "a");
        assert_eq!(data[0]["isolation-level"], "standard");
        assert_eq!(data[0]["isolation-reason"], "");
    }

    #[test]
    fn list_reports_label_and_color() {
        let dir = temp_dir("list-label-color");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let data = dispatch(&ctx(&dir), Request::List {}).unwrap();
        assert_eq!(data[0]["label"], "untrusted");
        assert_eq!(data[0]["color"], "#e03131");
    }

    #[test]
    fn list_reports_encrypted_field() {
        let dir = temp_dir("list-encrypted");
        write_manifest(&dir, "spike");
        write_encrypted_space(&dir, "a", "spike", 3, 70003);
        write_space(&dir, "b", "spike", 4, 70004);
        let data = dispatch(&ctx(&dir), Request::List {}).unwrap();
        let items = data.as_array().unwrap();
        let a = items.iter().find(|s| s["id"] == "a").unwrap();
        let b = items.iter().find(|s| s["id"] == "b").unwrap();
        assert_eq!(a["encrypted"], true);
        assert_eq!(b["encrypted"], false);
    }

    #[test]
    fn net_status_is_ok_and_names_absence_explicitly() {
        // без root/стенда мостов и tap'ов на машине нет — и это не ошибка операции
        let dir = temp_dir("net-status");
        let data = dispatch(&ctx(&dir), Request::NetStatus {}).unwrap();

        assert_eq!(data["killswitch_counters"], "недоступны с хоста");
        assert!(data["bridges"]["br-spaces"].is_boolean());
        assert!(data["bridges"]["br-captive"].is_boolean());
        assert!(data["taps"].is_array());
        assert!(data["uplink-pci"]["address"].is_string());
        assert!(data["uplink-pci"]["present"].is_boolean());
        // на машине разработчика сетевая машина может быть запущена по-настоящему — проверяем
        // согласованность полей, а не её отсутствие: иначе тест зелен только при выключенном продукте
        let running = data["miyori-net"]["running"]
            .as_bool()
            .expect("running — булев");
        if running {
            assert!(data["miyori-net"]["pid"].is_number());
        } else {
            assert_eq!(data["miyori-net"]["pid"], serde_json::Value::Null);
            assert_eq!(data["miyori-net"]["tunnel"], serde_json::Value::Null);
        }
    }

    #[test]
    fn net_status_reports_ruleset_field_shape() {
        // nft может не запуститься без root — проверяем форму поля, а не успех вызова
        let dir = temp_dir("net-status-ruleset");
        let data = dispatch(&ctx(&dir), Request::NetStatus {}).unwrap();

        assert!(data["ruleset"]["available"].is_boolean());
        assert!(data["ruleset"]["text"].is_string());
        assert!(data["ruleset"].get("truncated-from").is_some());
    }

    #[test]
    fn create_rejects_reserved_space_id() {
        let dir = temp_dir("create-reserved-id");
        let err = dispatch(&ctx(&dir), create_request("miyori-net", "spike")).unwrap_err();
        assert_eq!(err.0, ErrorCode::BadRequest);
    }

    #[test]
    fn create_rejects_duplicate_space_id() {
        let dir = temp_dir("create-dup-id");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let err = dispatch(&ctx(&dir), create_request("a", "spike")).unwrap_err();
        assert_eq!(err.0, ErrorCode::SpaceExists);
    }

    #[test]
    fn create_reports_profile_not_found() {
        let dir = temp_dir("create-missing-profile");
        let err = dispatch(&ctx(&dir), create_request("a", "ghost")).unwrap_err();
        assert_eq!(err.0, ErrorCode::ProfileNotFound);
    }

    #[test]
    fn create_reports_template_not_found() {
        let dir = temp_dir("create-missing-template");
        write_manifest(&dir, "spike");
        let err = dispatch(&ctx(&dir), create_request("a", "spike")).unwrap_err();
        assert_eq!(err.0, ErrorCode::TemplateNotFound);
    }

    #[test]
    fn create_rejects_seed_when_profile_has_no_data_volume() {
        let dir = temp_dir("create-seed-no-data-mb");
        write_manifest_no_data(&dir, "nodata");
        write_template(&dir, "nodata", &"d".repeat(64));
        let seed_dir = dir.join("seed-files");
        std::fs::create_dir_all(&seed_dir).unwrap();

        let err = dispatch(
            &ctx(&dir),
            create_request_with_seed("a", "nodata", seed_dir.to_str().unwrap()),
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::BadRequest);
    }

    #[test]
    fn create_rejects_relative_seed_path() {
        let dir = temp_dir("create-seed-relative");
        write_manifest(&dir, "spike");
        write_template(&dir, "spike", &"d".repeat(64));

        let err = dispatch(
            &ctx(&dir),
            create_request_with_seed("a", "spike", "relative/seed"),
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::BadRequest);
    }

    #[test]
    fn create_rejects_seed_dir_that_does_not_exist() {
        let dir = temp_dir("create-seed-missing-dir");
        write_manifest(&dir, "spike");
        write_template(&dir, "spike", &"d".repeat(64));
        let missing = dir.join("no-such-seed-dir");

        let err = dispatch(
            &ctx(&dir),
            create_request_with_seed("a", "spike", missing.to_str().unwrap()),
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::BadRequest);
    }

    // пустая строка — не "без шифрования", а ошибка запроса; проверяется раньше любого обращения к диску
    #[test]
    fn create_rejects_empty_passphrase() {
        let dir = temp_dir("create-empty-passphrase");
        let err =
            dispatch(&ctx(&dir), create_request_with_passphrase("a", "spike", "")).unwrap_err();
        assert_eq!(err.0, ErrorCode::BadRequest);
    }

    // без seed create_volumes обязана пойти прежним путём qemu_img_blank — ни каталога стейджинга, ни mkfs
    #[test]
    fn create_volumes_without_seed_takes_the_old_path() {
        let dir = temp_dir("create-volumes-no-seed");
        let template_dir = dir.join("templates").join("spike").join("d".repeat(64));
        std::fs::create_dir_all(&template_dir).unwrap();
        // qemu-img create -b читает backing-файл — фиктивных байт write_template тут не хватит
        qemu_img_blank(&template_dir.join("root.qcow2"), 1, None).unwrap();
        let space_dir = dir.join("spaces").join("a");
        std::fs::create_dir_all(&space_dir).unwrap();
        let uid = current_uid(&dir);

        create_volumes(&template_dir, &space_dir, uid, 1, None, None).unwrap();

        assert!(space_dir.join("data.qcow2").exists());
        assert!(
            !space_dir.join(".seed-staging").exists(),
            "без seed засев не должен создавать staging вообще"
        );
    }

    // setpriv тем же приёмом, что build_argv: reuid/regid/clear-groups перед mkfs.ext4 -d <seed> <raw>
    #[test]
    fn mkfs_seed_argv_drops_privileges_before_reading_seed() {
        let spec = SeedSpec {
            dir: PathBuf::from("/home/op/files"),
            build_uid: 70042,
            build_gid: 70100,
        };
        let raw = PathBuf::from("/var/lib/miyorios/spaces/a/.seed-staging/data.raw");
        let argv = mkfs_seed_argv(&spec, &raw);

        assert_eq!(argv[0], "setpriv");
        assert_eq!(argv[1], "--reuid");
        assert_eq!(argv[2], "70042");
        assert_eq!(argv[3], "--regid");
        assert_eq!(argv[4], "70100");
        assert!(argv.contains(&"--clear-groups".to_string()));
        let sep = argv
            .iter()
            .position(|s| s == "--")
            .expect("нет разделителя --");
        assert_eq!(argv[sep + 1], "mkfs.ext4");
        assert_eq!(argv[sep + 2], "-q");
        assert_eq!(argv[sep + 3], "-d");
        assert_eq!(argv[sep + 4], "/home/op/files");
        assert_eq!(
            argv[sep + 5],
            "/var/lib/miyorios/spaces/a/.seed-staging/data.raw"
        );
    }

    #[test]
    fn qemu_img_overlay_argv_without_secret() {
        let backing = Path::new("/var/lib/miyorios/templates/spike/dead/root.qcow2");
        let overlay = Path::new("/var/lib/miyorios/spaces/a/system-overlay.qcow2");
        let argv = qemu_img_overlay_argv(backing, overlay, None);
        assert_eq!(
            argv,
            vec![
                "qemu-img",
                "create",
                "-f",
                "qcow2",
                "-b",
                "/var/lib/miyorios/templates/spike/dead/root.qcow2",
                "-F",
                "qcow2",
                "/var/lib/miyorios/spaces/a/system-overlay.qcow2",
            ]
        );
    }

    // синтаксис проверен вживую на этой машине: --object перед -f, -o encrypt.* перед путём оверлея
    #[test]
    fn qemu_img_overlay_argv_with_secret_matches_proven_syntax() {
        let backing = Path::new("/var/lib/miyorios/templates/spike/dead/root.qcow2");
        let overlay = Path::new("/var/lib/miyorios/spaces/a/system-overlay.qcow2");
        let secret = Path::new("/run/miyorios/.qemu-img-secrets/sec-1");
        let argv = qemu_img_overlay_argv(backing, overlay, Some(secret));
        assert_eq!(
            argv,
            vec![
                "qemu-img",
                "create",
                "--object",
                "secret,id=sec0,file=/run/miyorios/.qemu-img-secrets/sec-1,format=raw",
                "-f",
                "qcow2",
                "-b",
                "/var/lib/miyorios/templates/spike/dead/root.qcow2",
                "-F",
                "qcow2",
                "-o",
                "encrypt.format=luks,encrypt.key-secret=sec0",
                "/var/lib/miyorios/spaces/a/system-overlay.qcow2",
            ]
        );
    }

    #[test]
    fn qemu_img_blank_argv_without_secret() {
        let path = Path::new("/var/lib/miyorios/spaces/a/data.qcow2");
        let argv = qemu_img_blank_argv(path, 4096, None);
        assert_eq!(
            argv,
            vec![
                "qemu-img",
                "create",
                "-f",
                "qcow2",
                "/var/lib/miyorios/spaces/a/data.qcow2",
                "4096M",
            ]
        );
    }

    #[test]
    fn qemu_img_blank_argv_with_secret_matches_proven_syntax() {
        let path = Path::new("/var/lib/miyorios/spaces/a/data.qcow2");
        let secret = Path::new("/run/miyorios/.qemu-img-secrets/sec-1");
        let argv = qemu_img_blank_argv(path, 4096, Some(secret));
        assert_eq!(
            argv,
            vec![
                "qemu-img",
                "create",
                "--object",
                "secret,id=sec0,file=/run/miyorios/.qemu-img-secrets/sec-1,format=raw",
                "-f",
                "qcow2",
                "-o",
                "encrypt.format=luks,encrypt.key-secret=sec0",
                "/var/lib/miyorios/spaces/a/data.qcow2",
                "4096M",
            ]
        );
    }

    #[test]
    fn qemu_img_convert_argv_without_secret() {
        let raw = Path::new("/var/lib/miyorios/spaces/a/.seed-staging/data.raw");
        let qcow2 = Path::new("/var/lib/miyorios/spaces/a/data.qcow2");
        let argv = qemu_img_convert_argv(raw, qcow2, None);
        assert_eq!(
            argv,
            vec![
                "qemu-img",
                "convert",
                "-O",
                "qcow2",
                "/var/lib/miyorios/spaces/a/.seed-staging/data.raw",
                "/var/lib/miyorios/spaces/a/data.qcow2",
            ]
        );
    }

    #[test]
    fn qemu_img_convert_argv_with_secret_matches_proven_syntax() {
        let raw = Path::new("/var/lib/miyorios/spaces/a/.seed-staging/data.raw");
        let qcow2 = Path::new("/var/lib/miyorios/spaces/a/data.qcow2");
        let secret = Path::new("/run/miyorios/.qemu-img-secrets/sec-1");
        let argv = qemu_img_convert_argv(raw, qcow2, Some(secret));
        assert_eq!(
            argv,
            vec![
                "qemu-img",
                "convert",
                "--object",
                "secret,id=sec0,file=/run/miyorios/.qemu-img-secrets/sec-1,format=raw",
                "-O",
                "qcow2",
                "-o",
                "encrypt.format=luks,encrypt.key-secret=sec0",
                "/var/lib/miyorios/spaces/a/.seed-staging/data.raw",
                "/var/lib/miyorios/spaces/a/data.qcow2",
            ]
        );
    }

    #[test]
    fn build_reports_profile_not_found_not_internal() {
        let dir = temp_dir("build-missing-profile");
        let err = dispatch(
            &ctx(&dir),
            Request::Build {
                profile: SpaceId::new("ghost").unwrap(),
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::ProfileNotFound);
    }

    #[test]
    fn resolve_build_uid_defaults_to_profiles_dir_owner() {
        let dir = temp_dir("build-uid-default");
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        let owner = std::fs::metadata(dir.join("profiles")).unwrap().uid();
        let c = ctx(&dir);
        assert_eq!(resolve_build_uid(&c).unwrap(), owner);
    }

    #[test]
    fn resolve_build_uid_prefers_explicit_flag() {
        let dir = temp_dir("build-uid-explicit");
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        let run_dir = dir.join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let c = Ctx::new(
            dir.to_path_buf(),
            run_dir,
            dir.join("registry.toml"),
            Some(12345),
            None,
        );
        assert_eq!(resolve_build_uid(&c).unwrap(), 12345);
    }

    #[test]
    fn prepare_staging_dir_creates_and_owns_it() {
        let dir = temp_dir("staging-owner");
        let staging = dir.join("staging").join("spike-1");
        let uid = std::fs::metadata(&dir).unwrap().uid();
        prepare_staging_dir(&staging, uid).unwrap();
        assert_eq!(std::fs::metadata(&staging).unwrap().uid(), uid);
    }

    // каталог демона группе не отдаётся: в нём лежат qemu.pid и nonce, а группа сокета —
    // это uid менеджера, то есть сторона, от которой демон и защищается (п.21 модели угроз)
    #[test]
    fn prepare_space_run_dir_keeps_daemon_dir_closed_to_group() {
        let dir = temp_dir("space-run-dir-new");
        let space_dir = dir.join("space-a");
        let gid = std::fs::metadata(&dir).unwrap().gid();

        prepare_space_run_dir(&space_dir, Some(gid)).unwrap();

        let meta = std::fs::metadata(&space_dir).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o755);
        assert_eq!(meta.permissions().mode() & 0o020, 0);
    }

    #[test]
    fn prepare_space_run_dir_opens_only_gui_subdir_to_group() {
        let dir = temp_dir("space-run-dir-gui");
        let space_dir = dir.join("space-a");
        let gid = std::fs::metadata(&dir).unwrap().gid();

        prepare_space_run_dir(&space_dir, Some(gid)).unwrap();

        let meta = std::fs::metadata(space_dir.join(miyori_config::GUI_SUBDIR)).unwrap();
        assert_eq!(meta.gid(), gid);
        assert_eq!(meta.permissions().mode() & 0o777, 0o770);
    }

    #[test]
    fn prepare_space_run_dir_fixes_existing_dir_with_wrong_mode() {
        let dir = temp_dir("space-run-dir-existing");
        let space_dir = dir.join("space-a");
        std::fs::create_dir_all(space_dir.join(miyori_config::GUI_SUBDIR)).unwrap();
        std::fs::set_permissions(&space_dir, std::fs::Permissions::from_mode(0o770)).unwrap();
        let gid = std::fs::metadata(&dir).unwrap().gid();

        prepare_space_run_dir(&space_dir, Some(gid)).unwrap();

        let meta = std::fs::metadata(&space_dir).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o755);
    }

    #[test]
    fn prepare_space_run_dir_without_gid_creates_both() {
        let dir = temp_dir("space-run-dir-no-gid");
        let space_dir = dir.join("space-a");

        prepare_space_run_dir(&space_dir, None).unwrap();

        assert!(space_dir.is_dir());
        assert!(space_dir.join(miyori_config::GUI_SUBDIR).is_dir());
    }

    // пустой nonce совпадал бы с пустым ответом гостя: отсутствие файла — это отказ, а не ""
    #[test]
    fn read_nonce_rejects_missing_file() {
        let dir = temp_dir("nonce-missing");
        assert!(read_nonce(&dir, "a").is_err());
    }

    #[test]
    fn read_nonce_rejects_empty_file() {
        let dir = temp_dir("nonce-empty");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a").join("nonce"), "   \n").unwrap();
        assert!(read_nonce(&dir, "a").is_err());
    }

    #[test]
    fn read_nonce_returns_trimmed_value() {
        let dir = temp_dir("nonce-ok");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a").join("nonce"), "abc123\n").unwrap();
        assert_eq!(read_nonce(&dir, "a").unwrap(), "abc123");
    }

    #[test]
    fn finalize_template_is_noop_when_digest_already_exists() {
        let dir = temp_dir("finalize-existing-digest");
        let built = dir.join("built");
        std::fs::create_dir_all(&built).unwrap();
        std::fs::write(built.join("marker"), b"built").unwrap();
        let final_dir = dir.join("final");
        std::fs::create_dir_all(&final_dir).unwrap();

        finalize_template(&built, &final_dir).unwrap();

        // digest уже занят — перенос не случился, содержимое built-каталога осталось на месте
        assert!(built.join("marker").is_file());
    }

    #[test]
    fn relink_latest_replaces_previous_symlink() {
        let dir = temp_dir("relink-latest");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("old-digest", dir.join("latest")).unwrap();

        relink_latest(&dir, "new-digest").unwrap();

        assert_eq!(
            std::fs::read_link(dir.join("latest")).unwrap(),
            Path::new("new-digest")
        );
    }

    #[test]
    fn resolve_template_digest_prefers_latest_symlink() {
        let dir = temp_dir("digest-latest");
        write_template(&dir, "spike", "digest-a");
        write_template(&dir, "spike", "digest-b");
        link_latest(&dir, "spike", "digest-b");
        assert_eq!(resolve_template_digest(&dir, "spike").unwrap(), "digest-b");
    }

    #[test]
    fn resolve_template_digest_uses_single_directory_without_latest() {
        let dir = temp_dir("digest-single");
        write_template(&dir, "spike", "onlydigest");
        assert_eq!(
            resolve_template_digest(&dir, "spike").unwrap(),
            "onlydigest"
        );
    }

    #[test]
    fn resolve_template_digest_errors_on_ambiguous_directories() {
        let dir = temp_dir("digest-ambiguous");
        write_template(&dir, "spike", "digest-a");
        write_template(&dir, "spike", "digest-b");
        let err = resolve_template_digest(&dir, "spike").unwrap_err();
        assert_eq!(err.0, ErrorCode::TemplateNotFound);
    }

    #[test]
    fn resolve_template_digest_errors_when_missing() {
        let dir = temp_dir("digest-missing");
        let err = resolve_template_digest(&dir, "spike").unwrap_err();
        assert_eq!(err.0, ErrorCode::TemplateNotFound);
    }

    #[test]
    fn require_passphrase_matches_encryption_rules() {
        assert!(require_passphrase_matches_encryption(true, &Some("x".to_string())).is_ok());
        assert!(require_passphrase_matches_encryption(false, &None).is_ok());
        assert_eq!(
            require_passphrase_matches_encryption(true, &None)
                .unwrap_err()
                .0,
            ErrorCode::BadRequest
        );
        assert_eq!(
            require_passphrase_matches_encryption(false, &Some("x".to_string()))
                .unwrap_err()
                .0,
            ErrorCode::BadRequest
        );
    }

    #[test]
    fn qemu_died_of_wrong_passphrase_matches_qemu_diagnostic() {
        assert!(qemu_died_of_wrong_passphrase(
            "qemu-system-x86_64: -drive id=root,...: Invalid password, cannot unlock any keyslot"
        ));
    }

    // отличаем от отсутствия секрета: то же самое "QEMU умер сразу", но другая причина и другой код клиенту
    #[test]
    fn qemu_died_of_wrong_passphrase_does_not_match_missing_secret_diagnostic() {
        assert!(!qemu_died_of_wrong_passphrase(
            "qemu-system-x86_64: -drive id=root,...: Parameter 'encrypt.key-secret' is required for cipher"
        ));
    }

    #[test]
    fn write_running_secret_has_no_trailing_newline_and_owner_only_mode() {
        let dir = temp_dir("running-secret-write");
        let uid = current_uid(&dir);
        let path = write_running_secret(&dir, "a", uid, "hunter2").unwrap();

        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"hunter2",
            "QEMU берёт файл байт в байт — перевод строки испортил бы пароль"
        );
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o400);
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o500);
    }

    #[test]
    fn remove_running_secret_clears_the_directory() {
        let dir = temp_dir("running-secret-remove");
        let uid = current_uid(&dir);
        write_running_secret(&dir, "a", uid, "hunter2").unwrap();

        remove_running_secret(&dir, "a");

        assert!(!space_secret_dir(&dir, "a").exists());
    }

    #[test]
    fn write_qemu_img_secret_has_exact_bytes_and_owner_only_mode() {
        let dir = temp_dir("qemu-img-secret-write");
        let path = write_qemu_img_secret(&dir, "hunter2").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"hunter2");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        cleanup_qemu_img_secret(&path);
        assert!(!path.exists());
    }

    #[test]
    fn start_reports_space_not_found() {
        let dir = temp_dir("start-missing-space");
        let err = dispatch(
            &ctx(&dir),
            Request::Start {
                space: SpaceId::new("nope").unwrap(),
                passphrase: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::SpaceNotFound);
    }

    #[test]
    fn start_reports_space_running_when_already_running() {
        let dir = temp_dir("start-running");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let c = ctx(&dir);
        let child = mark_running(&c, "a", 3, &dir);

        let err = dispatch(
            &c,
            Request::Start {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            },
        )
        .unwrap_err();
        reap(child);

        assert_eq!(err.0, ErrorCode::SpaceRunning);
    }

    // ловится раньше state_of/net::bridge_exists — не требует root, чтобы дойти до отказа
    #[test]
    fn start_rejects_missing_passphrase_for_encrypted_space() {
        let dir = temp_dir("start-encrypted-no-passphrase");
        write_manifest(&dir, "spike");
        write_encrypted_space(&dir, "a", "spike", 3, 70003);
        let err = dispatch(
            &ctx(&dir),
            Request::Start {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::BadRequest);
    }

    #[test]
    fn start_rejects_passphrase_for_unencrypted_space() {
        let dir = temp_dir("start-unencrypted-with-passphrase");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let err = dispatch(
            &ctx(&dir),
            Request::Start {
                space: SpaceId::new("a").unwrap(),
                passphrase: Some("hunter2".to_string()),
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::BadRequest);
    }

    #[test]
    fn stop_reports_space_not_found() {
        let dir = temp_dir("stop-missing-space");
        let err = dispatch(
            &ctx(&dir),
            Request::Stop {
                space: SpaceId::new("nope").unwrap(),
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::SpaceNotFound);
    }

    #[test]
    fn stop_reports_space_stopped_when_not_running() {
        let dir = temp_dir("stop-not-running");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let err = dispatch(
            &ctx(&dir),
            Request::Stop {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::SpaceStopped);
    }

    // окна без запущенного гостя не открыть — тот же отказ, что у stop-not-running
    #[test]
    fn open_window_reports_space_stopped_when_not_running() {
        let dir = temp_dir("open-window-not-running");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let err = dispatch(
            &ctx(&dir),
            Request::OpenWindow {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::SpaceStopped);
    }

    // агент подтвердил exec-app своим nonce -> операция успешна, а не молча "наверное получилось"
    #[test]
    fn open_window_succeeds_when_agent_confirms_exec_app() {
        use std::io::{BufRead, BufReader, Write};
        use vsock::VMADDR_CID_LOCAL;

        let _guard = crate::agent::lock_agent_port_for_test();
        let cid = VMADDR_CID_LOCAL;
        let dir = temp_dir("open-window-agent-confirms");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", cid, 70000 + cid);
        let c = ctx(&dir);
        let child = mark_running(&c, "a", cid, &dir);
        std::fs::write(c.run_dir.join("a").join("nonce"), "abc123").unwrap();

        let listener = crate::agent::bind_agent_port_for_test();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut writer = &stream;
            let _ = writeln!(
                writer,
                r#"{{"nonce":"abc123","ok":true,"detail":"окно открыто","data_unmounted":null}}"#
            );
        });

        let data = dispatch(
            &c,
            Request::OpenWindow {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap();
        server.join().unwrap();
        reap(child);

        assert_eq!(data["id"], "a");
        assert_eq!(data["detail"], "окно открыто");
    }

    // отказ агента — это отказ операции, а не "наверное получилось": ok:false должно вернуть Err с detail
    #[test]
    fn open_window_fails_when_agent_declines_exec_app() {
        use std::io::{BufRead, BufReader, Write};
        use vsock::VMADDR_CID_LOCAL;

        let _guard = crate::agent::lock_agent_port_for_test();
        let cid = VMADDR_CID_LOCAL;
        let dir = temp_dir("open-window-agent-declines");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", cid, 70000 + cid);
        let c = ctx(&dir);
        let child = mark_running(&c, "a", cid, &dir);
        std::fs::write(c.run_dir.join("a").join("nonce"), "abc123").unwrap();

        let listener = crate::agent::bind_agent_port_for_test();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut writer = &stream;
            let _ = writeln!(
                writer,
                r#"{{"nonce":"abc123","ok":false,"detail":"нет свободных леасов","data_unmounted":null}}"#
            );
        });

        let err = dispatch(
            &c,
            Request::OpenWindow {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap_err();
        server.join().unwrap();
        reap(child);

        assert_eq!(err.1, "нет свободных леасов");
    }

    // shutdown_via_agent — не через dispatch(Stop): тот трогает net::delete_tap, а это требует root
    #[test]
    fn shutdown_via_agent_is_graceful_when_agent_confirms_unmount() {
        use std::io::{BufRead, BufReader, Write};
        use vsock::VMADDR_CID_LOCAL;

        let _guard = crate::agent::lock_agent_port_for_test();
        let dir = temp_dir("shutdown-agent-graceful");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a").join("nonce"), "abc123").unwrap();

        let listener = crate::agent::bind_agent_port_for_test();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut writer = &stream;
            let _ = writeln!(
                writer,
                r#"{{"nonce":"abc123","ok":true,"detail":"том отмонтирован","data_unmounted":true}}"#
            );
        });

        let (graceful, detail) = shutdown_via_agent(&dir, "a", VMADDR_CID_LOCAL);
        server.join().unwrap();

        assert!(graceful);
        assert_eq!(detail, "том отмонтирован");
    }

    // start_app_via_agent — не через dispatch(Start): реальный success-путь требует root (net::create_tap)
    #[test]
    fn start_app_via_agent_reports_reason_when_agent_never_answers() {
        use vsock::VMADDR_CID_LOCAL;

        let _guard = crate::agent::lock_agent_port_for_test();
        let started = std::time::Instant::now();
        let app = start_app_via_agent(
            VMADDR_CID_LOCAL,
            "abc123",
            std::time::Duration::from_millis(200),
            || true,
            |_| {},
        );
        let elapsed = started.elapsed();

        assert_ne!(app, "started", "{app}");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "молчащий агент не должен держать start дольше отведённого дедлайна: {elapsed:?}"
        );
    }

    // неверный пароль убивает QEMU за секунды, а дедлайн агента — двадцать: без раннего выхода оператор ждал бы весь
    #[test]
    fn start_app_via_agent_gives_up_as_soon_as_qemu_is_gone() {
        use vsock::VMADDR_CID_LOCAL;

        let _guard = crate::agent::lock_agent_port_for_test();
        let started = std::time::Instant::now();
        let app = start_app_via_agent(
            VMADDR_CID_LOCAL,
            "abc123",
            std::time::Duration::from_secs(20),
            || false,
            |_| {},
        );
        let elapsed = started.elapsed();

        assert_ne!(app, "started", "{app}");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "мёртвый QEMU обязан прервать ожидание сразу, а не через дедлайн: {elapsed:?}"
        );
    }

    #[test]
    fn shutdown_via_agent_is_forced_when_agent_is_unreachable() {
        use vsock::VMADDR_CID_LOCAL;

        let _guard = crate::agent::lock_agent_port_for_test();
        let dir = temp_dir("shutdown-agent-unreachable");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a").join("nonce"), "abc123").unwrap();

        // порт нарочно никто не слушает — молчание не должно притвориться штатным остановом
        let (graceful, detail) = shutdown_via_agent(&dir, "a", VMADDR_CID_LOCAL);

        assert!(!graceful, "{detail}");
    }

    #[test]
    fn shutdown_via_agent_is_forced_on_nonce_mismatch() {
        use std::io::{BufRead, BufReader, Write};
        use vsock::VMADDR_CID_LOCAL;

        let _guard = crate::agent::lock_agent_port_for_test();
        let dir = temp_dir("shutdown-agent-mismatch");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a").join("nonce"), "abc123").unwrap();

        let listener = crate::agent::bind_agent_port_for_test();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut writer = &stream;
            // тот же приём, что и в agent.rs::health_rejects_wrong_nonce: агент настоящий, но пережил перезапуск
            let _ = writeln!(
                writer,
                r#"{{"nonce":"старый","ok":true,"detail":"том отмонтирован","data_unmounted":true}}"#
            );
        });

        let (graceful, _) = shutdown_via_agent(&dir, "a", VMADDR_CID_LOCAL);
        server.join().unwrap();

        assert!(!graceful);
    }

    #[test]
    fn describe_reports_clean_shutdown_after_forced_stop() {
        let dir = temp_dir("describe-clean-shutdown-false");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let mut store = load_store(&dir).unwrap();
        store
            .set_clean_shutdown(&SpaceId::new("a").unwrap(), false)
            .unwrap();

        let data = dispatch(
            &ctx(&dir),
            Request::Describe {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap();
        assert_eq!(data["clean-shutdown"], false);
    }

    #[test]
    fn describe_reports_null_clean_shutdown_before_first_stop() {
        let dir = temp_dir("describe-clean-shutdown-null");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);

        let data = dispatch(
            &ctx(&dir),
            Request::Describe {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap();
        assert!(data["clean-shutdown"].is_null());
    }

    #[test]
    fn reset_system_reports_space_not_found() {
        let dir = temp_dir("reset-system-missing-space");
        let err = dispatch(
            &ctx(&dir),
            Request::ResetSystem {
                space: SpaceId::new("nope").unwrap(),
                passphrase: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::SpaceNotFound);
    }

    #[test]
    fn reset_all_reports_space_not_found() {
        let dir = temp_dir("reset-all-missing-space");
        let err = dispatch(
            &ctx(&dir),
            Request::ResetAll {
                space: SpaceId::new("nope").unwrap(),
                passphrase: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::SpaceNotFound);
    }

    // подмена тома под живым QEMU — порча данных; проверяем отказ ДО того, как reset тронет файлы
    // (сам файл том в этой фикстуре не заводим — happy path трогает qemu-img/chown и требует root,
    // что этот юнит-тест сознательно не покрывает, см. отчёт задачи 9)
    #[test]
    fn reset_system_refuses_running_space() {
        let dir = temp_dir("reset-system-running");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let c = ctx(&dir);
        let child = mark_running(&c, "a", 3, &dir);

        let err = dispatch(
            &c,
            Request::ResetSystem {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            },
        )
        .unwrap_err();
        reap(child);

        assert_eq!(err.0, ErrorCode::SpaceRunning);
    }

    #[test]
    fn reset_all_refuses_running_space() {
        let dir = temp_dir("reset-all-running");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let c = ctx(&dir);
        let child = mark_running(&c, "a", 3, &dir);

        let err = dispatch(
            &c,
            Request::ResetAll {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            },
        )
        .unwrap_err();
        reap(child);

        assert_eq!(err.0, ErrorCode::SpaceRunning);
    }

    // пароль обязателен ⇔ спейс зашифрован — то же правило, что у start; ловится раньше reject_if_running
    #[test]
    fn reset_system_rejects_missing_passphrase_for_encrypted_space() {
        let dir = temp_dir("reset-system-encrypted-no-passphrase");
        write_manifest(&dir, "spike");
        write_encrypted_space(&dir, "a", "spike", 3, 70003);
        let err = dispatch(
            &ctx(&dir),
            Request::ResetSystem {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::BadRequest);
    }

    #[test]
    fn reset_all_rejects_passphrase_for_unencrypted_space() {
        let dir = temp_dir("reset-all-unencrypted-with-passphrase");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let err = dispatch(
            &ctx(&dir),
            Request::ResetAll {
                space: SpaceId::new("a").unwrap(),
                passphrase: Some("hunter2".to_string()),
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::BadRequest);
    }

    #[test]
    fn update_image_reports_space_not_found() {
        let dir = temp_dir("update-image-missing-space");
        let err = dispatch(
            &ctx(&dir),
            Request::UpdateImage {
                space: SpaceId::new("nope").unwrap(),
                passphrase: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::SpaceNotFound);
    }

    // подмена оверлея под живым QEMU — порча данных, тот же гвард, что у reset_system/reset_all
    #[test]
    fn update_image_refuses_running_space() {
        let dir = temp_dir("update-image-running");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let c = ctx(&dir);
        let child = mark_running(&c, "a", 3, &dir);

        let err = dispatch(
            &c,
            Request::UpdateImage {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            },
        )
        .unwrap_err();
        reap(child);

        assert_eq!(err.0, ErrorCode::SpaceRunning);
    }

    #[test]
    fn update_image_rejects_missing_passphrase_for_encrypted_space() {
        let dir = temp_dir("update-image-encrypted-no-passphrase");
        write_manifest(&dir, "spike");
        write_encrypted_space(&dir, "a", "spike", 3, 70003);
        let err = dispatch(
            &ctx(&dir),
            Request::UpdateImage {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::BadRequest);
    }

    // без latest update-image не знает, куда переезжать — это не тот же случай, что у create,
    // где годится единственный каталог без символической ссылки
    #[test]
    fn update_image_reports_missing_latest_symlink() {
        let dir = temp_dir("update-image-no-latest");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        write_full_template(&dir, "spike", &"e".repeat(64));
        // латест намеренно не заводим

        let err = dispatch(
            &ctx(&dir),
            Request::UpdateImage {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::TemplateNotFound);
    }

    #[test]
    fn update_image_reports_incomplete_template() {
        let dir = temp_dir("update-image-incomplete");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        // write_template (не write_full_template) не кладёт MANIFEST — каталог неполон
        write_template(&dir, "spike", &"e".repeat(64));
        link_latest(&dir, "spike", &"e".repeat(64));

        let err = dispatch(
            &ctx(&dir),
            Request::UpdateImage {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::TemplateNotFound);
    }

    // digest спейса уже совпадает с latest — это не работа, а сообщение; оверлей трогать не за чем
    #[test]
    fn update_image_reports_already_current_without_touching_overlay() {
        let dir = temp_dir("update-image-already-current");
        write_manifest(&dir, "spike");
        // write_space кладёт digest "d"*64
        write_space(&dir, "a", "spike", 3, 70003);
        write_full_template(&dir, "spike", &"d".repeat(64));
        link_latest(&dir, "spike", &"d".repeat(64));

        let data = dispatch(
            &ctx(&dir),
            Request::UpdateImage {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            },
        )
        .unwrap();

        assert_eq!(data["from"], "d".repeat(64));
        assert_eq!(data["to"], "d".repeat(64));
        assert_eq!(data["detail"], "спейс уже на текущем образе");
        assert!(
            !dir.join("spaces")
                .join("a")
                .join("system-overlay.qcow2")
                .exists(),
            "образ и так текущий — оверлей пересоздавать не за чем"
        );
    }

    // chown на собственный euid, в отличие от chown на 70003, root не требует — own_and_lock_down проходит в тесте
    fn current_uid(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().uid()
    }

    // роняем именно запись digest: space_dir становится 0500 уже после config.toml, а write_config_atomically не создаст config.toml.tmp без права записи в каталог
    #[test]
    fn update_image_leaves_digest_unchanged_when_persisting_new_digest_fails() {
        let dir = temp_dir("update-image-digest-write-fails");
        write_manifest(&dir, "spike");
        let uid = current_uid(&dir);
        write_space(&dir, "a", "spike", 3, uid);
        let new_digest = "e".repeat(64);
        // qemu-img create -b открывает backing, чтобы прочесть его размер — фиктивные байты
        // write_template тут не годятся, нужен настоящий qcow2 и для старого, и для нового root
        let new_template = dir.join("templates").join("spike").join(&new_digest);
        std::fs::create_dir_all(&new_template).unwrap();
        qemu_img_blank(&new_template.join("root.qcow2"), 1, None).unwrap();
        std::fs::write(new_template.join("vmlinuz"), b"fake").unwrap();
        std::fs::write(new_template.join("initrd.img"), b"fake").unwrap();
        std::fs::write(new_template.join("MANIFEST"), b"fake").unwrap();
        link_latest(&dir, "spike", &new_digest);

        // старый оверлей уже существует — qemu-img create перезапишет его на месте
        let space_dir = dir.join("spaces").join("a");
        let old_template = dir.join("templates").join("spike").join("d".repeat(64));
        std::fs::create_dir_all(&old_template).unwrap();
        qemu_img_blank(&old_template.join("root.qcow2"), 1, None).unwrap();
        qemu_img_overlay(
            &old_template.join("root.qcow2"),
            &space_dir.join("system-overlay.qcow2"),
            None,
        )
        .unwrap();

        let mut perms = std::fs::metadata(&space_dir).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(&space_dir, perms).unwrap();

        let err = dispatch(
            &ctx(&dir),
            Request::UpdateImage {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            },
        )
        .unwrap_err();

        // возвращаем права до чтения диска и до конца теста — иначе assert ниже не прочитает файл
        let mut perms = std::fs::metadata(&space_dir).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&space_dir, perms).unwrap();

        assert_eq!(err.0, ErrorCode::Internal);

        let on_disk = std::fs::read_to_string(space_dir.join("config.toml")).unwrap();
        assert!(
            on_disk.contains(&"d".repeat(64)),
            "digest на диске обязан остаться старым: {on_disk}"
        );
        assert!(
            !on_disk.contains(&new_digest),
            "digest на диске не должен был перейти на новый: {on_disk}"
        );

        let overlay_bytes = std::fs::read(space_dir.join("system-overlay.qcow2")).unwrap();
        let overlay_text = String::from_utf8_lossy(&overlay_bytes);
        assert!(
            overlay_text.contains(&new_digest),
            "оверлей обязан был успеть переехать на новый backing до сбоя записи digest"
        );
    }

    #[test]
    fn destroy_reports_space_not_found() {
        let dir = temp_dir("destroy-missing-space");
        let err = dispatch(
            &ctx(&dir),
            Request::Destroy {
                space: SpaceId::new("nope").unwrap(),
            },
        )
        .unwrap_err();
        assert_eq!(err.0, ErrorCode::SpaceNotFound);
    }

    #[test]
    fn destroy_refuses_running_space_and_keeps_its_directory() {
        let dir = temp_dir("destroy-running");
        write_manifest(&dir, "spike");
        write_space(&dir, "a", "spike", 3, 70003);
        let c = ctx(&dir);
        let child = mark_running(&c, "a", 3, &dir);

        let err = dispatch(
            &c,
            Request::Destroy {
                space: SpaceId::new("a").unwrap(),
            },
        )
        .unwrap_err();
        reap(child);

        assert_eq!(err.0, ErrorCode::SpaceRunning);
        assert!(
            dir.join("spaces").join("a").exists(),
            "запущенный спейс не должен быть удалён"
        );
    }
}
