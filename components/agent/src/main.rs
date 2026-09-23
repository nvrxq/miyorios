#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use miyori_proto::agent::{AgentReply, AgentRequest};
use miyori_proto::read_capped_line;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::Duration;
use vsock::{VsockAddr, VsockListener, VsockStream, VMADDR_CID_ANY};

// закреплено спекой §6; гость только слушает, соединение всегда открывает хост
const AGENT_PORT: u32 = 1701;
// кадры агента крошечные ({"cmd":"health"} и подобные) — лимит с большим запасом, не 8 КиБ control.sock
const MAX_AGENT_REQUEST_BYTES: usize = 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
// §6.1: shutdown обязан погасить именно приложение, запущенное последним exec-app, а не любой процесс
static APP_CHILD: Mutex<Option<Child>> = Mutex::new(None);
const DATA_MOUNT_POINT: &str = "/data";

fn main() -> Result<()> {
    let listener = VsockListener::bind(&VsockAddr::new(VMADDR_CID_ANY, AGENT_PORT))
        .context("не удалось слушать vsock агента")?;
    eprintln!("miyori-agent слушает vsock:{AGENT_PORT}");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => handle(stream),
            Err(err) => eprintln!("miyori-agent: accept: {err}"),
        }
    }
    Ok(())
}

// один кадр на вход, один на выход, потом закрываем — соединение всегда инициирует хост (спека §6)
fn handle(stream: VsockStream) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    let request = {
        let mut reader = BufReader::new(&stream);
        read_request(&mut reader)
    };
    let is_shutdown = matches!(request, Ok(AgentRequest::Shutdown {}));

    let reply = match request {
        Ok(AgentRequest::Health {}) => health_reply(),
        Ok(AgentRequest::Shutdown {}) => shutdown_reply(),
        Ok(AgentRequest::ExecApp {}) => exec_app_reply(),
        Err(err) => error_reply(&err),
    };

    if let Ok(line) = serde_json::to_string(&reply) {
        let mut writer = &stream;
        let _ = writeln!(writer, "{line}");
    }

    // §6.1 п.3: агент отвечает и умолкает — отвечать после shutdown больше некому и незачем
    if is_shutdown {
        std::process::exit(0);
    }
}

// хост доверенный по конструкции, но кадр всё равно разбираем строго тем же приёмом, что и control.sock
fn read_request<R: BufRead>(r: &mut R) -> Result<AgentRequest> {
    let line = read_capped_line(r, MAX_AGENT_REQUEST_BYTES)?;
    serde_json::from_str(&line).context("нераспознанный запрос от хоста")
}

fn health_reply() -> AgentReply {
    AgentReply {
        nonce: current_nonce(),
        ok: true,
        detail: current_uptime_secs().map(|s| format!("uptime={s}s")),
        data_unmounted: None,
    }
}

// §6.1: приложение, sync, размонтирование — в этом порядке, иначе открытый файл в /data не даст умонтировать
fn shutdown_reply() -> AgentReply {
    let nonce = current_nonce();
    if let Some(mut child) = take_app_child() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // синхронный sync, а не отдельный поток: до ответа хосту кэш обязан быть на диске, а не "скоро будет"
    let _ = Command::new("sync").status();
    let unmounted = unmount_data();
    AgentReply {
        nonce,
        ok: unmounted,
        detail: Some(if unmounted {
            "том отмонтирован".to_string()
        } else {
            "не удалось отмонтировать /data".to_string()
        }),
        data_unmounted: Some(unmounted),
    }
}

fn take_app_child() -> Option<Child> {
    APP_CHILD.lock().unwrap_or_else(|p| p.into_inner()).take()
}

// нет /dev/vdb у профиля вовсе — размонтировать нечего, и это не отказ, а норма (data_mb = 0)
fn unmount_data() -> bool {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    if !mounts_contain(&mounts, DATA_MOUNT_POINT) {
        return true;
    }
    Command::new("umount")
        .arg(DATA_MOUNT_POINT)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn mounts_contain(mounts: &str, target: &str) -> bool {
    mounts
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .any(|mount_point| mount_point == target)
}

// решение G: строка запуска из сокета не доходит до exec вообще — miyori-launch без аргументов
fn exec_app_reply() -> AgentReply {
    let nonce = current_nonce();
    match Command::new("/usr/local/bin/miyori-launch").spawn() {
        Ok(child) => {
            *APP_CHILD.lock().unwrap_or_else(|p| p.into_inner()) = Some(child);
            AgentReply {
                nonce,
                ok: true,
                detail: None,
                data_unmounted: None,
            }
        }
        Err(err) => AgentReply {
            nonce,
            ok: false,
            detail: Some(err.to_string()),
            data_unmounted: None,
        },
    }
}

fn error_reply(err: &anyhow::Error) -> AgentReply {
    AgentReply {
        nonce: current_nonce(),
        ok: false,
        detail: Some(err.to_string()),
        data_unmounted: None,
    }
}

fn current_nonce() -> String {
    std::fs::read_to_string("/proc/cmdline")
        .map(|cmdline| nonce_from_cmdline(&cmdline))
        .unwrap_or_default()
}

// per-boot nonce хост кладёт в -append; отдельного канала для него нет — только кадры этого протокола
fn nonce_from_cmdline(cmdline: &str) -> String {
    cmdline
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("MIYORI_NONCE="))
        .unwrap_or_default()
        .to_string()
}

fn current_uptime_secs() -> Option<u64> {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|raw| uptime_seconds(&raw))
}

fn uptime_seconds(raw: &str) -> Option<u64> {
    let secs: f64 = raw.split_whitespace().next()?.parse().ok()?;
    Some(secs as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn nonce_from_cmdline_extracts_value() {
        let cmdline = "console=hvc0 root=/dev/vda rw MIYORI_NONCE=3f9a7c2e1b6d4508 MIYORI_CID=3\n";
        assert_eq!(nonce_from_cmdline(cmdline), "3f9a7c2e1b6d4508");
    }

    #[test]
    fn nonce_from_cmdline_is_empty_when_missing() {
        assert_eq!(nonce_from_cmdline("console=hvc0 root=/dev/vda rw\n"), "");
    }

    #[test]
    fn uptime_seconds_parses_leading_float() {
        assert_eq!(uptime_seconds("123.45 678.90\n"), Some(123));
    }

    #[test]
    fn uptime_seconds_none_on_garbage() {
        assert_eq!(uptime_seconds("не число\n"), None);
    }

    #[test]
    fn read_request_parses_health() {
        let mut r = Cursor::new(b"{\"cmd\":\"health\"}\n".to_vec());
        assert_eq!(read_request(&mut r).unwrap(), AgentRequest::Health {});
    }

    #[test]
    fn read_request_rejects_unknown_field_on_unit_variant() {
        let mut r = Cursor::new(b"{\"cmd\":\"health\",\"evil\":1}\n".to_vec());
        assert!(read_request(&mut r).is_err());
    }

    #[test]
    fn read_request_rejects_unknown_command() {
        let mut r = Cursor::new(b"{\"cmd\":\"rm-rf\"}\n".to_vec());
        assert!(read_request(&mut r).is_err());
    }

    #[test]
    fn read_request_rejects_oversized_line() {
        let huge = vec![b'a'; 1024 * 1024];
        let mut r = Cursor::new(huge);
        assert!(read_request(&mut r).is_err());
    }

    #[test]
    fn read_request_rejects_truncated_frame() {
        let mut r = Cursor::new(b"{\"cmd\":\"health\"".to_vec());
        assert!(read_request(&mut r).is_err());
    }

    #[test]
    fn mounts_contain_finds_exact_mount_point() {
        let sample = "/dev/vda / ext4 rw 0 0\n/dev/vdb /data ext4 rw 0 0\n";
        assert!(mounts_contain(sample, "/data"));
    }

    #[test]
    fn mounts_contain_does_not_match_a_prefix() {
        // /data2 не должен считаться совпадением с /data — иначе умонтировали бы чужой том
        let sample = "/dev/vdc /data2 ext4 rw 0 0\n";
        assert!(!mounts_contain(sample, "/data"));
    }

    #[test]
    fn mounts_contain_false_when_absent() {
        assert!(!mounts_contain("/dev/vda / ext4 rw 0 0\n", "/data"));
    }

    // хост тестов никогда не монтирует /data — это и есть путь "размонтировать нечего", а не отказ
    #[test]
    fn shutdown_reply_succeeds_when_nothing_is_mounted_at_data() {
        let reply = shutdown_reply();
        assert!(reply.ok, "{reply:?}");
        assert_eq!(reply.data_unmounted, Some(true));
        assert_eq!(reply.detail.as_deref(), Some("том отмонтирован"));
    }
}
