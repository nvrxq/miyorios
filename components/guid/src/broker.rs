use anyhow::{Context, Result};
use miyori_config::{Registry, Space};
use miyori_sandbox::{spawn, SandboxSpec};
use std::collections::HashMap;
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vsock::VsockStream;

pub struct BrokerConfig {
    pub run_dir: PathBuf,
    pub waypipe: PathBuf,
    pub wayland_socket: PathBuf,
    pub memory_max_mb: u64,
    pub tasks_max: u64,
    pub cpu_quota_pct: u64,
}

impl BrokerConfig {
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self {
            run_dir: PathBuf::from("/run/miyorios"),
            waypipe: PathBuf::from("/usr/local/lib/miyorios/waypipe"),
            wayland_socket: PathBuf::from("/run/user/1000/wayland-1"),
            memory_max_mb: 512,
            tasks_max: 64,
            cpu_quota_pct: 50,
        }
    }
}

type ProxySpawner = dyn Fn(&SandboxSpec) -> Result<Child> + Send + Sync;
// io::Result, а не bool: код ошибки — единственное, чем ENODEV отличим от занятого порта
type LivenessProbe = dyn Fn(u32) -> std::io::Result<()> + Send + Sync;

// leases считает живые соединения спейса: на нуле прокси гасить некому, кроме нас
struct Proxy {
    child: Child,
    leases: usize,
    socket: PathBuf,
    // сторожу больше не за что проверять живость прокси, кроме как за CID его гостя
    cid: u32,
}

pub struct Broker {
    registry_path: PathBuf,
    cfg: BrokerConfig,
    // хэндл прокси держим: без него процесс нечем остановить и не за чем следить
    started: Mutex<HashMap<String, Proxy>>,
    spawn_proxy: Box<ProxySpawner>,
    probe: Box<LivenessProbe>,
}

// гарантирует release_proxy на любом выходе из handle(), включая ранний `?`
struct ProxyLease<'a> {
    broker: &'a Broker,
    space_id: String,
}

impl Drop for ProxyLease<'_> {
    fn drop(&mut self) {
        self.broker.release_proxy(&self.space_id);
    }
}

impl Broker {
    pub fn new(registry_path: PathBuf, cfg: BrokerConfig) -> Self {
        Self::with_spawner(registry_path, cfg, spawn)
    }

    // тестам нужен способ подменить запуск песочницы: настоящий spawn тянет systemd-run/bwrap/waypipe
    fn with_spawner(
        registry_path: PathBuf,
        cfg: BrokerConfig,
        spawn_proxy: impl Fn(&SandboxSpec) -> Result<Child> + Send + Sync + 'static,
    ) -> Self {
        Self::with_spawner_and_probe(registry_path, cfg, spawn_proxy, default_probe)
    }

    // тестам сторожа нужен способ подменить пробу живости так же, как уже подменяется spawn_proxy
    fn with_spawner_and_probe(
        registry_path: PathBuf,
        cfg: BrokerConfig,
        spawn_proxy: impl Fn(&SandboxSpec) -> Result<Child> + Send + Sync + 'static,
        probe: impl Fn(u32) -> std::io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            registry_path,
            cfg,
            started: Mutex::new(HashMap::new()),
            spawn_proxy: Box::new(spawn_proxy),
            probe: Box::new(probe),
        }
    }

    // реестр — сотни байт, перечитать на каждый вызов дешевле, чем держать протухшую копию (решение E)
    pub fn resolve(&self, cid: u32) -> Result<Option<Space>> {
        let registry = Registry::load(&self.registry_path)?;
        Ok(registry.by_cid(cid).map(|s| Space {
            id: s.id.clone(),
            cid: s.cid,
            label: s.label.clone(),
            color: s.color.clone(),
        }))
    }

    // каталог на спейс, а не общий: песочница монтирует его целиком, см. Task 5
    pub fn socket_path(&self, space: &Space) -> PathBuf {
        self.cfg
            .run_dir
            .join(&space.id)
            .join(miyori_config::GUI_SUBDIR)
            .join("gui.sock")
    }

    pub fn handle(&self, mut vsock: VsockStream, cid: u32) -> Result<()> {
        // битый реестр отказывает всем CID, а не работает по последней хорошей копии (решение E)
        let space = self
            .resolve(cid)
            .context("реестр спейсов не читается или повреждён")?
            .ok_or_else(|| {
                anyhow::anyhow!("отклонено соединение с незарегистрированного CID {cid}")
            })?;
        let socket = self.socket_path(&space);
        self.ensure_proxy(&space, &socket)?;
        // если прокси поднять не удалось, гасить нечего — аренда берётся только после успеха
        let _lease = ProxyLease {
            broker: self,
            space_id: space.id.clone(),
        };

        let mut unix = UnixStream::connect(&socket)
            .with_context(|| format!("не подключиться к прокси {}", socket.display()))?;
        let mut vsock_read = vsock.try_clone().context("клонирование vsock")?;
        let mut unix_write = unix.try_clone().context("клонирование unix")?;

        let up = std::thread::spawn(move || std::io::copy(&mut vsock_read, &mut unix_write));
        std::io::copy(&mut unix, &mut vsock).ok();

        // встречный поток спит в read; без shutdown join не вернётся никогда
        let _ = vsock.shutdown(Shutdown::Both);
        let _ = unix.shutdown(Shutdown::Both);
        let _ = up.join();
        Ok(())
    }

    fn ensure_proxy(&self, space: &Space, socket: &Path) -> Result<()> {
        let mut started = self
            .started
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // счётчик протухшей записи переживает перезапуск: чужие аренды уже на неё рассчитывают
        let mut carried_leases = 0;
        if let Some(proxy) = started.get_mut(&space.id) {
            // try_wait() не блокирует: Ok(None) значит процесс ещё жив
            let alive = matches!(proxy.child.try_wait(), Ok(None));
            if alive && socket.exists() {
                proxy.leases += 1;
                return Ok(());
            }
            // запись протухла — реального прокси за ней больше нет, иначе останется зомби
            let mut dead = started.remove(&space.id).unwrap();
            carried_leases = dead.leases;
            if alive {
                let _ = dead.child.kill();
            }
            let _ = dead.child.wait();
        }
        let dir = socket.parent().context("сокет спейса без каталога")?;
        std::fs::create_dir_all(dir).with_context(|| format!("не создать {}", dir.display()))?;
        let mode = std::fs::metadata(dir)
            .with_context(|| format!("не прочитать права {}", dir.display()))?
            .permissions()
            .mode();
        // каталог демона (0770 root:группа) закрыт от «прочих» не хуже нашего 0700, а чужим владением нам и не распорядиться
        if !dir_mode_is_private(mode) {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).with_context(
                || {
                    format!(
                        "каталог {} открыт посторонним (режим {:o}) и закрыть его не удалось",
                        dir.display(),
                        mode & 0o777
                    )
                },
            )?;
        }
        let _ = std::fs::remove_file(socket);
        let child = (self.spawn_proxy)(&SandboxSpec {
            space_id: space.id.clone(),
            socket: socket.to_path_buf(),
            secctx: space.secctx_id(),
            title_prefix: space.title_prefix(),
            waypipe: self.cfg.waypipe.clone(),
            wayland_socket: self.cfg.wayland_socket.clone(),
            memory_max_mb: self.cfg.memory_max_mb,
            tasks_max: self.cfg.tasks_max,
            cpu_quota_pct: self.cfg.cpu_quota_pct,
        })?;
        wait_for_socket(socket)?;
        started.insert(
            space.id.clone(),
            Proxy {
                child,
                leases: carried_leases + 1,
                socket: socket.to_path_buf(),
                cid: space.cid,
            },
        );
        Ok(())
    }

    fn release_proxy(&self, space_id: &str) {
        let mut started = self
            .started
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(proxy) = started.get_mut(space_id) else {
            return;
        };
        proxy.leases = proxy.leases.saturating_sub(1);
        if proxy.leases > 0 {
            return;
        }
        // wait обязателен вместе с kill: иначе на месте прокси останется зомби до конца жизни guid
        if let Some(mut dead) = started.remove(space_id) {
            let _ = dead.child.kill();
            let _ = dead.child.wait();
            let _ = std::fs::remove_file(&dead.socket);
        }
    }

    // запускает фоновый опрос: аренда может не отпуститься никогда, сторож — единственная страховка
    pub fn spawn_dead_proxy_watchdog(self: &Arc<Self>) {
        let broker = Arc::clone(self);
        std::thread::spawn(move || loop {
            std::thread::sleep(DEAD_PROXY_POLL_INTERVAL);
            broker.reap_dead_proxies();
        });
    }

    // список, а не remove в цикле: нельзя мутировать map, пока по нему идёт iter()
    fn reap_dead_proxies(&self) {
        let mut started = self
            .started
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dead_ids: Vec<String> = started
            .iter()
            .filter(|(_, proxy)| match (self.probe)(proxy.cid) {
                Ok(()) => false,
                Err(e) => cid_is_gone(&e),
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in dead_ids {
            // повторяет release_proxy: kill+wait без зомби, сокет и запись уходят вместе
            if let Some(mut dead) = started.remove(&id) {
                let _ = dead.child.kill();
                let _ = dead.child.wait();
                let _ = std::fs::remove_file(&dead.socket);
            }
        }
    }
}

// проба стоит 0 мс на живом vhost-vsock — секунда с большим запасом, лишних будильников не нужно
const DEAD_PROXY_POLL_INTERVAL: Duration = Duration::from_secs(1);

// порт заведомо без слушателя: важен только факт жизни CID, гостевого агента трогать незачем
const LIVENESS_PROBE_PORT: u32 = 0xffff;

fn default_probe(cid: u32) -> std::io::Result<()> {
    VsockStream::connect_with_cid_port(cid, LIVENESS_PROBE_PORT).map(|_| ())
}

// ENODEV у ErrorKind в stable не определён (Uncategorized), поэтому сравнение по номеру — замер 2026-08-30 на vhost-vsock
const ENODEV: i32 = 19;

fn cid_is_gone(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(ENODEV)
}

// приватность каталога — это закрытость от «прочих», а не конкретно 0700: 0770 root:группа не хуже
fn dir_mode_is_private(mode: u32) -> bool {
    mode & 0o007 == 0
}

fn wait_for_socket(path: &Path) -> Result<()> {
    for _ in 0..100 {
        if path.exists() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    anyhow::bail!("waypipe не создал сокет {} за 5 c", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // общий временный каталог теста: под ним и registry, и (при надобности) run_dir
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "miyori-broker-test-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_registry(path: &Path, src: &str) {
        std::fs::write(path, src).unwrap();
    }

    // CID 1 здесь недопустим: это VMADDR_CID_LOCAL, и miyori-config его отвергает
    fn test_registry_path(name: &str) -> PathBuf {
        let path = test_dir(name).join("spaces.toml");
        write_registry(
            &path,
            "[[space]]\nid = \"spike\"\ncid = 3\nlabel = \"untrusted\"\ncolor = \"#e03131\"\n",
        );
        path
    }

    // реального waypipe в тестовом окружении нет — суррогат только создаёт сокет-файл и спит,
    // ensure_proxy проверяет лишь его наличие и живость процесса
    fn socket_touching_spawner(spec: &SandboxSpec) -> Result<Child> {
        Command::new("sh")
            .arg("-c")
            .arg(format!("touch '{}' && exec sleep 5", spec.socket.display()))
            .spawn()
            .context("тестовый суррогат прокси не запустился")
    }

    #[test]
    fn known_cid_resolves_to_its_own_space() {
        let b = Broker::new(test_registry_path("known-cid"), BrokerConfig::for_tests());
        assert_eq!(b.resolve(3).unwrap().unwrap().id, "spike");
    }

    #[test]
    fn unknown_cid_is_refused() {
        let b = Broker::new(test_registry_path("unknown-cid"), BrokerConfig::for_tests());
        assert!(b.resolve(42).unwrap().is_none());
    }

    #[test]
    fn loopback_cid_never_resolves() {
        let b = Broker::new(
            test_registry_path("loopback-cid"),
            BrokerConfig::for_tests(),
        );
        assert!(
            b.resolve(1).unwrap().is_none(),
            "локальный процесс не должен получать label спейса"
        );
    }

    #[test]
    fn socket_path_is_derived_from_registry_not_from_peer() {
        let b = Broker::new(test_registry_path("socket-path"), BrokerConfig::for_tests());
        let space = b.resolve(3).unwrap().unwrap();
        assert_eq!(
            b.socket_path(&space),
            PathBuf::from("/run/miyorios/spike/gui/gui.sock")
        );
    }

    #[test]
    fn space_added_after_start_is_resolved() {
        let registry_path = test_registry_path("hot-reload");
        let b = Broker::new(registry_path.clone(), BrokerConfig::for_tests());
        assert!(b.resolve(4).unwrap().is_none());

        write_registry(
            &registry_path,
            "[[space]]\nid = \"spike\"\ncid = 3\nlabel = \"untrusted\"\ncolor = \"#e03131\"\n\n[[space]]\nid = \"ctf\"\ncid = 4\nlabel = \"untrusted\"\ncolor = \"#e03131\"\n",
        );

        assert_eq!(
            b.resolve(4).unwrap().unwrap().id,
            "ctf",
            "спейс, дописанный после старта, обязан резолвиться без перезапуска демона"
        );
    }

    #[test]
    fn broken_registry_refuses_every_cid() {
        let registry_path = test_registry_path("broken-registry");
        let b = Broker::new(registry_path.clone(), BrokerConfig::for_tests());
        assert!(b.resolve(3).unwrap().is_some());

        write_registry(&registry_path, "это не toml {{{");
        assert!(
            b.resolve(3).is_err(),
            "битый реестр обязан отказывать всем CID, а не работать по последней хорошей копии"
        );
    }

    // VERIFY M2, тест 12 на живом хосте: miyori-net гасили и поднимали заново, ensure_proxy
    // отдавал Ok на мёртвую запись, UnixStream::connect падал ECONNRESET
    #[test]
    fn dead_proxy_is_restarted_on_next_connect() {
        let run_dir = test_dir("dead-proxy-run");
        let registry_path = test_registry_path("dead-proxy-registry");

        let spawn_count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&spawn_count);
        let cfg = BrokerConfig {
            run_dir: run_dir.clone(),
            ..BrokerConfig::for_tests()
        };
        let b = Broker::with_spawner(registry_path, cfg, move |spec: &SandboxSpec| {
            counter.fetch_add(1, Ordering::SeqCst);
            socket_touching_spawner(spec)
        });
        let space = b.resolve(3).unwrap().unwrap();
        let socket = b.socket_path(&space);

        b.ensure_proxy(&space, &socket).unwrap();
        assert_eq!(spawn_count.load(Ordering::SeqCst), 1);
        assert!(socket.exists());

        // симулируем смерть waypipe-клиента: процесс убит, его сокет пропал, запись в started осталась
        {
            let mut started = b.started.lock().unwrap();
            let proxy = started.get_mut(&space.id).unwrap();
            proxy.child.kill().unwrap();
            proxy.child.wait().unwrap();
        }
        std::fs::remove_file(&socket).unwrap();

        b.ensure_proxy(&space, &socket).unwrap();
        assert!(
            socket.exists(),
            "после смерти прокси повторное подключение должно получить рабочий сокет"
        );
        assert_eq!(
            spawn_count.load(Ordering::SeqCst),
            2,
            "мёртвая запись в started не должна маскировать необходимость перезапуска"
        );

        let _ = std::fs::remove_dir_all(&run_dir);
    }

    // проверяем смерть процесса наблюдаемо: не тем, что тест сам его убил
    fn process_is_dead(pid: u32) -> bool {
        !Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn proxy_dies_with_its_last_connection() {
        let run_dir = test_dir("release-last-run");
        let registry_path = test_registry_path("release-last-registry");
        let cfg = BrokerConfig {
            run_dir: run_dir.clone(),
            ..BrokerConfig::for_tests()
        };
        let b = Broker::with_spawner(registry_path, cfg, socket_touching_spawner);
        let space = b.resolve(3).unwrap().unwrap();
        let socket = b.socket_path(&space);

        b.ensure_proxy(&space, &socket).unwrap();
        let pid = {
            let mut started = b.started.lock().unwrap();
            started.get_mut(&space.id).unwrap().child.id()
        };

        b.release_proxy(&space.id);

        assert!(
            process_is_dead(pid),
            "процесс прокси обязан быть убит после ухода последней аренды"
        );
        assert!(
            !b.started.lock().unwrap().contains_key(&space.id),
            "запись прокси обязана исчезнуть из карты"
        );
        assert!(!socket.exists(), "файл сокета обязан быть удалён");

        let _ = std::fs::remove_dir_all(&run_dir);
    }

    #[test]
    fn proxy_survives_until_the_last_connection_goes() {
        let run_dir = test_dir("release-survive-run");
        let registry_path = test_registry_path("release-survive-registry");
        let cfg = BrokerConfig {
            run_dir: run_dir.clone(),
            ..BrokerConfig::for_tests()
        };
        let b = Broker::with_spawner(registry_path, cfg, socket_touching_spawner);
        let space = b.resolve(3).unwrap().unwrap();
        let socket = b.socket_path(&space);

        b.ensure_proxy(&space, &socket).unwrap();
        b.ensure_proxy(&space, &socket).unwrap();
        let pid = {
            let mut started = b.started.lock().unwrap();
            started.get_mut(&space.id).unwrap().child.id()
        };

        b.release_proxy(&space.id);
        assert!(
            !process_is_dead(pid),
            "прокси не должен гаситься, пока держит аренду хотя бы одно соединение"
        );
        assert!(socket.exists(), "сокет обязан остаться, пока прокси жив");

        b.release_proxy(&space.id);
        assert!(
            process_is_dead(pid),
            "после ухода последней аренды прокси обязан быть убит"
        );

        let _ = std::fs::remove_dir_all(&run_dir);
    }

    // сброс счётчика в 1 при перезапуске убил бы живой прокси чужой, уже отработавшей арендой
    #[test]
    fn restarted_proxy_keeps_the_count_of_live_connections() {
        let run_dir = test_dir("release-restart-run");
        let registry_path = test_registry_path("release-restart-registry");
        let cfg = BrokerConfig {
            run_dir: run_dir.clone(),
            ..BrokerConfig::for_tests()
        };
        let b = Broker::with_spawner(registry_path, cfg, socket_touching_spawner);
        let space = b.resolve(3).unwrap().unwrap();
        let socket = b.socket_path(&space);

        b.ensure_proxy(&space, &socket).unwrap();
        b.ensure_proxy(&space, &socket).unwrap();

        // симулируем смерть прокси, как в dead_proxy_is_restarted_on_next_connect
        {
            let mut started = b.started.lock().unwrap();
            let proxy = started.get_mut(&space.id).unwrap();
            proxy.child.kill().unwrap();
            proxy.child.wait().unwrap();
        }
        std::fs::remove_file(&socket).unwrap();

        b.ensure_proxy(&space, &socket).unwrap();
        let pid = {
            let mut started = b.started.lock().unwrap();
            started.get_mut(&space.id).unwrap().child.id()
        };

        b.release_proxy(&space.id);
        assert!(
            !process_is_dead(pid),
            "новый прокси не должен гаситься чужой арендой: счётчик протухшей записи обязан перейти новой"
        );

        let _ = std::fs::remove_dir_all(&run_dir);
    }

    #[test]
    fn release_of_unknown_space_is_harmless() {
        let b = Broker::new(
            test_registry_path("release-unknown-registry"),
            BrokerConfig::for_tests(),
        );
        b.release_proxy("нет-такого");
    }

    #[test]
    fn dir_mode_is_private_matches_the_others_bits() {
        assert!(dir_mode_is_private(0o700));
        assert!(dir_mode_is_private(0o770));
        assert!(!dir_mode_is_private(0o755));
        assert!(!dir_mode_is_private(0o777));
        assert!(!dir_mode_is_private(0o701));
    }

    // мод демона (0770 root:группа) закрыт от «прочих» не хуже нашего 0700 — трогать его нельзя,
    // иначе непривилегированный брокер упадёт с EPERM ровно там, где раньше не падал
    #[test]
    fn ensure_proxy_leaves_an_already_private_dir_untouched() {
        let run_dir = test_dir("private-dir-untouched-run");
        let registry_path = test_registry_path("private-dir-untouched-registry");
        let dir = run_dir.join("spike");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();

        let cfg = BrokerConfig {
            run_dir: run_dir.clone(),
            ..BrokerConfig::for_tests()
        };
        let b = Broker::with_spawner(registry_path, cfg, socket_touching_spawner);
        let space = b.resolve(3).unwrap().unwrap();
        let socket = b.socket_path(&space);

        b.ensure_proxy(&space, &socket).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o770,
            "уже приватный чужой режим не должен подменяться на 0700"
        );

        let _ = std::fs::remove_dir_all(&run_dir);
    }

    // коды измерены пробой connect() на vhost-vsock с хоста 2026-08-30, см. описание задачи
    const ENODEV: i32 = 19;
    const ECONNRESET: i32 = 104;
    const ETIMEDOUT: i32 = 110;
    const ECONNREFUSED: i32 = 111;
    const EACCES: i32 = 13;

    #[test]
    fn cid_is_gone_reads_only_enodev() {
        assert!(cid_is_gone(&std::io::Error::from_raw_os_error(ENODEV)));
        for code in [ECONNRESET, ETIMEDOUT, ECONNREFUSED, EACCES] {
            assert!(
                !cid_is_gone(&std::io::Error::from_raw_os_error(code)),
                "errno {code} означает, что CID жив, а не мёртв"
            );
        }
    }

    #[test]
    fn watchdog_kills_proxy_of_a_dead_cid() {
        let run_dir = test_dir("watchdog-dead-run");
        let registry_path = test_registry_path("watchdog-dead-registry");
        let cfg = BrokerConfig {
            run_dir: run_dir.clone(),
            ..BrokerConfig::for_tests()
        };
        let b =
            Broker::with_spawner_and_probe(registry_path, cfg, socket_touching_spawner, |_cid| {
                Err(std::io::Error::from_raw_os_error(ENODEV))
            });
        let space = b.resolve(3).unwrap().unwrap();
        let socket = b.socket_path(&space);
        b.ensure_proxy(&space, &socket).unwrap();
        let pid = {
            let mut started = b.started.lock().unwrap();
            started.get_mut(&space.id).unwrap().child.id()
        };

        b.reap_dead_proxies();

        assert!(
            process_is_dead(pid),
            "сторож обязан убить прокси, чей CID пропал"
        );
        assert!(
            !b.started.lock().unwrap().contains_key(&space.id),
            "запись мёртвого прокси обязана исчезнуть из карты"
        );
        assert!(!socket.exists(), "файл сокета обязан быть удалён");

        let _ = std::fs::remove_dir_all(&run_dir);
    }

    #[test]
    fn watchdog_leaves_proxy_of_a_live_cid_alone() {
        let run_dir = test_dir("watchdog-live-run");
        let registry_path = test_registry_path("watchdog-live-registry");
        let cfg = BrokerConfig {
            run_dir: run_dir.clone(),
            ..BrokerConfig::for_tests()
        };
        let b =
            Broker::with_spawner_and_probe(registry_path, cfg, socket_touching_spawner, |_cid| {
                Ok(())
            });
        let space = b.resolve(3).unwrap().unwrap();
        let socket = b.socket_path(&space);
        b.ensure_proxy(&space, &socket).unwrap();
        let pid = {
            let mut started = b.started.lock().unwrap();
            started.get_mut(&space.id).unwrap().child.id()
        };

        b.reap_dead_proxies();

        assert!(
            !process_is_dead(pid),
            "сторож, гасящий всё подряд, убил бы и живой CID — это и должно провалить тест"
        );
        assert!(b.started.lock().unwrap().contains_key(&space.id));
        assert!(socket.exists());

        b.release_proxy(&space.id);
        let _ = std::fs::remove_dir_all(&run_dir);
    }
}
