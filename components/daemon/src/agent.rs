#![forbid(unsafe_code)]

// хост звонит агенту сам (спека §6): агент только слушает, здесь — клиентская сторона на хосте
use crate::qemu::{self, SpaceState};
use miyori_proto::agent::{read_agent_reply, AgentReply, AgentRequest};
use std::fmt;
use std::io::{BufReader, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;
use vsock::{VsockAddr, VsockStream};

pub const AGENT_PORT: u32 = 1701;
pub const AGENT_HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
// sync+umount на живом диске укладываются в доли секунды; запас — под медленную microvm, не про подвисший umount
pub const AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
// exec-app — это spawn() на госте, а не ожидание старта самого приложения; как health, а не как shutdown
pub const AGENT_EXEC_APP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, PartialEq, Eq)]
pub enum HealthError {
    Timeout,
    NonceMismatch,
    Io(String),
}

impl fmt::Display for HealthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HealthError::Timeout => write!(f, "агент не ответил за отведённое время"),
            HealthError::NonceMismatch => write!(f, "агент ответил чужим nonce"),
            HealthError::Io(msg) => write!(f, "{msg}"),
        }
    }
}

// решение H: ответ гостя недоверен целиком — лимит и разбор несёт read_agent_reply, здесь транспорт и сравнение nonce
fn call_agent(
    cid: u32,
    expected_nonce: &str,
    timeout: Duration,
    request: &AgentRequest,
) -> Result<AgentReply, HealthError> {
    let stream = connect_with_timeout(cid, AGENT_PORT, timeout)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(to_health_error)?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(to_health_error)?;

    let line = serde_json::to_string(request).expect("AgentRequest всегда сериализуется");
    {
        let mut writer = &stream;
        writeln!(writer, "{line}").map_err(to_health_error)?;
    }

    let mut reader = BufReader::new(&stream);
    let reply = read_agent_reply(&mut reader).map_err(read_error_to_health_error)?;
    if reply.nonce != expected_nonce {
        return Err(HealthError::NonceMismatch);
    }
    Ok(reply)
}

pub fn health(cid: u32, expected_nonce: &str, timeout: Duration) -> Result<(), HealthError> {
    call_agent(cid, expected_nonce, timeout, &AgentRequest::Health {}).map(|_| ())
}

// §6.1: хост ждёт подтверждение размонтирования с тайм-аутом, а не самого факта отправки shutdown
pub fn shutdown(
    cid: u32,
    expected_nonce: &str,
    timeout: Duration,
) -> Result<AgentReply, HealthError> {
    call_agent(cid, expected_nonce, timeout, &AgentRequest::Shutdown {})
}

// демон зовёт после того, как health впервые ответил успехом (спека §6): агент уже поднят
pub fn exec_app(
    cid: u32,
    expected_nonce: &str,
    timeout: Duration,
) -> Result<AgentReply, HealthError> {
    call_agent(cid, expected_nonce, timeout, &AgentRequest::ExecApp {})
}

// живой QEMU — не то же самое, что живой спейс: describe/list спрашивают агента поверх процесса
pub fn observed_state(run_dir: &Path, id: &str, cid: u32, timeout: Duration) -> SpaceState {
    let process_state = qemu::state_of(run_dir, id, cid);
    if process_state != SpaceState::Running {
        return process_state;
    }
    // читаем nonce заново при каждой проверке, а не из памяти демона: та же логика, что у miyori-guid (решение E)
    let nonce = match std::fs::read_to_string(run_dir.join(id).join("nonce")) {
        Ok(raw) => raw.trim().to_string(),
        Err(_) => return SpaceState::Unresponsive,
    };
    match health(cid, &nonce, timeout) {
        Ok(()) => SpaceState::Running,
        Err(_) => SpaceState::Unresponsive,
    }
}

fn to_health_error(err: std::io::Error) -> HealthError {
    match err.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => HealthError::Timeout,
        _ => HealthError::Io(err.to_string()),
    }
}

// read_agent_reply оборачивает io::Error в anyhow::Error через .context(): kind виден только по цепочке причин
fn read_error_to_health_error(err: anyhow::Error) -> HealthError {
    let timed_out = err
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io_err| {
            matches!(
                io_err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
        });
    if timed_out {
        HealthError::Timeout
    } else {
        HealthError::Io(err.to_string())
    }
}

// у крейта vsock нет connect с таймаутом; без отдельного потока молчащий CID подвесил бы health() навсегда
fn connect_with_timeout(
    cid: u32,
    port: u32,
    timeout: Duration,
) -> Result<VsockStream, HealthError> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(VsockStream::connect(&VsockAddr::new(cid, port)));
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(err)) => Err(to_health_error(err)),
        Err(_) => Err(HealthError::Timeout),
    }
}

// health() всегда звонит на AGENT_PORT — этот лок делят тесты agent.rs и ops.rs, гонять их надо по одному
#[cfg(test)]
pub(crate) static AGENT_PORT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// паника одного теста с локом не должна валить все остальные — им нужен порт, а не чистое состояние
#[cfg(test)]
pub(crate) fn lock_agent_port_for_test() -> std::sync::MutexGuard<'static, ()> {
    AGENT_PORT_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

// лок не гарантирует, что ядро уже освободило порт предыдущего listener — короткий повтор дешевле гонки
#[cfg(test)]
pub(crate) fn bind_agent_port_for_test() -> vsock::VsockListener {
    use vsock::{VsockListener, VMADDR_CID_LOCAL};
    for _ in 0..50 {
        match VsockListener::bind(&VsockAddr::new(VMADDR_CID_LOCAL, AGENT_PORT)) {
            Ok(listener) => return listener,
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("bind agent port для теста: {err}"),
        }
    }
    panic!("agent port занят слишком долго после предыдущего теста");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use vsock::VMADDR_CID_LOCAL;

    // требует загруженного модуля vsock_loopback, как и components/guid/src/peer.rs
    fn fake_agent(reply_line: String) -> std::thread::JoinHandle<()> {
        let listener = bind_agent_port_for_test();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut writer = &stream;
            let _ = writeln!(writer, "{reply_line}");
        })
    }

    #[test]
    fn health_succeeds_when_nonce_matches() {
        let _guard = lock_agent_port_for_test();
        let server = fake_agent(
            r#"{"nonce":"abc123","ok":true,"detail":null,"data_unmounted":null}"#.to_string(),
        );
        let result = health(VMADDR_CID_LOCAL, "abc123", Duration::from_secs(2));
        server.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn health_rejects_wrong_nonce() {
        let _guard = lock_agent_port_for_test();
        let server = fake_agent(
            r#"{"nonce":"чужой","ok":true,"detail":null,"data_unmounted":null}"#.to_string(),
        );
        let result = health(VMADDR_CID_LOCAL, "abc123", Duration::from_secs(2));
        server.join().unwrap();
        assert_eq!(result, Err(HealthError::NonceMismatch));
    }

    #[test]
    fn health_rejects_oversized_reply_without_hanging() {
        let _guard = lock_agent_port_for_test();
        // подделка отвечает мусором длиннее MAX_AGENT_REPLY_BYTES — тот же приём, что и предложен в плане
        let huge = "a".repeat(64 * 1024);
        let server = fake_agent(huge);
        let started = std::time::Instant::now();
        let result = health(VMADDR_CID_LOCAL, "abc123", Duration::from_secs(2));
        server.join().unwrap();
        assert!(result.is_err(), "{result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "оверсайз-ответ не должен ждать полного тайм-аута: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn health_times_out_when_agent_is_silent() {
        let _guard = lock_agent_port_for_test();
        let listener = bind_agent_port_for_test();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            // держим соединение открытым дольше клиентского тайм-аута и ничего не отвечаем
            std::thread::sleep(Duration::from_millis(700));
            drop(stream);
        });

        let started = std::time::Instant::now();
        let result = health(VMADDR_CID_LOCAL, "abc123", Duration::from_millis(200));
        let elapsed = started.elapsed();
        server.join().unwrap();

        assert_eq!(result, Err(HealthError::Timeout));
        assert!(
            elapsed < Duration::from_millis(600),
            "агент молчит не навсегда — ждать нужно было ~200 мс, а не 700: {elapsed:?}"
        );
    }

    #[test]
    fn shutdown_succeeds_when_agent_confirms_unmount() {
        let _guard = lock_agent_port_for_test();
        let server = fake_agent(
            r#"{"nonce":"abc123","ok":true,"detail":"том отмонтирован","data_unmounted":true}"#
                .to_string(),
        );
        let reply = shutdown(VMADDR_CID_LOCAL, "abc123", Duration::from_secs(2)).unwrap();
        server.join().unwrap();
        assert!(reply.ok);
        assert_eq!(reply.data_unmounted, Some(true));
        assert_eq!(reply.detail.as_deref(), Some("том отмонтирован"));
    }

    #[test]
    fn exec_app_succeeds_when_agent_confirms() {
        let _guard = lock_agent_port_for_test();
        let server = fake_agent(
            r#"{"nonce":"abc123","ok":true,"detail":null,"data_unmounted":null}"#.to_string(),
        );
        let reply = exec_app(VMADDR_CID_LOCAL, "abc123", Duration::from_secs(2)).unwrap();
        server.join().unwrap();
        assert!(reply.ok);
    }

    #[test]
    fn exec_app_rejects_wrong_nonce() {
        let _guard = lock_agent_port_for_test();
        let server = fake_agent(
            r#"{"nonce":"чужой","ok":true,"detail":null,"data_unmounted":null}"#.to_string(),
        );
        let result = exec_app(VMADDR_CID_LOCAL, "abc123", Duration::from_secs(2));
        server.join().unwrap();
        assert_eq!(result.unwrap_err(), HealthError::NonceMismatch);
    }

    #[test]
    fn shutdown_rejects_wrong_nonce() {
        let _guard = lock_agent_port_for_test();
        let server = fake_agent(
            r#"{"nonce":"чужой","ok":true,"detail":"том отмонтирован","data_unmounted":true}"#
                .to_string(),
        );
        let result = shutdown(VMADDR_CID_LOCAL, "abc123", Duration::from_secs(2));
        server.join().unwrap();
        assert_eq!(result.unwrap_err(), HealthError::NonceMismatch);
    }

    #[test]
    fn shutdown_fails_fast_when_nothing_listens() {
        let _guard = lock_agent_port_for_test();
        let started = std::time::Instant::now();
        let result = shutdown(VMADDR_CID_LOCAL, "abc123", Duration::from_secs(2));
        assert!(result.is_err(), "{result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "отказ без слушателя обязан быть быстрым, а не по тайм-ауту: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn shutdown_times_out_when_agent_is_silent() {
        let _guard = lock_agent_port_for_test();
        let listener = bind_agent_port_for_test();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(700));
            drop(stream);
        });

        let started = std::time::Instant::now();
        let result = shutdown(VMADDR_CID_LOCAL, "abc123", Duration::from_millis(200));
        let elapsed = started.elapsed();
        server.join().unwrap();

        assert_eq!(result.unwrap_err(), HealthError::Timeout);
        assert!(
            elapsed < Duration::from_millis(600),
            "молчащий агент не должен держать stop дольше отведённого тайм-аута: {elapsed:?}"
        );
    }

    #[test]
    fn health_fails_fast_when_nothing_listens() {
        let _guard = lock_agent_port_for_test();
        let started = std::time::Instant::now();
        let result = health(VMADDR_CID_LOCAL, "abc123", Duration::from_secs(2));
        assert!(result.is_err(), "{result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "отказ без слушателя обязан быть быстрым, а не по тайм-ауту: {:?}",
            started.elapsed()
        );
    }

    fn temp_run_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("miyorid-agent-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn observed_state_is_stopped_without_pid_file() {
        let dir = temp_run_dir("stopped");
        assert_eq!(
            observed_state(&dir, "a", 3, Duration::from_millis(200)),
            SpaceState::Stopped
        );
    }

    #[test]
    fn observed_state_is_unresponsive_when_process_alive_but_agent_unreachable() {
        let _guard = lock_agent_port_for_test();
        let dir = temp_run_dir("unresponsive");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a").join("nonce"), "abc123").unwrap();

        // тот же приём, что в components/daemon/src/ops.rs::tests::spawn_fake_qemu — живой процесс, но не QEMU
        let fake_path = dir.join("qemu-system-x86_64-guest-cid=3");
        std::os::unix::fs::symlink("/bin/sleep", &fake_path).unwrap();
        let mut child = std::process::Command::new(&fake_path)
            .arg("300")
            .spawn()
            .unwrap();
        std::fs::write(dir.join("a").join("qemu.pid"), child.id().to_string()).unwrap();
        for _ in 0..200 {
            if qemu::state_of(&dir, "a", 3) == SpaceState::Running {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        let state = observed_state(&dir, "a", 3, Duration::from_millis(200));
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(state, SpaceState::Unresponsive);
    }
}
