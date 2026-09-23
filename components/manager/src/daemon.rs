#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::mpsc::SyncSender;
use std::time::Duration;

pub const DEFAULT_SOCKET: &str = "/run/miyorios/control.sock";
pub const FRAME_TIMEOUT_SECS: u64 = 120;

pub enum Update {
    Progress {
        request_id: u64,
        text: String,
    },
    Done {
        request_id: u64,
        reply: Result<miyori_proto::client::Reply, String>,
    },
}

pub struct Daemon {
    socket: PathBuf,
}

impl Daemon {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    // build идёт минутами — вызов уходит в поток, иначе GTK перестал бы перерисовываться
    pub fn send(&self, request_id: u64, request: serde_json::Value, tx: SyncSender<Update>) {
        let socket = self.socket.clone();
        let timeout = request_timeout(request["op"].as_str().unwrap_or(""));
        std::thread::spawn(move || {
            let progress_tx = tx.clone();
            let mut on_progress = |text: &str| {
                let _ = progress_tx.try_send(Update::Progress {
                    request_id,
                    text: text.chars().take(1024).collect(),
                });
            };
            let reply = miyori_proto::client::call(&socket, &request, timeout, &mut on_progress)
                // {:#} разворачивает цепочку причин: без неё до пользователя доедет "демон не отвечает" без "Permission denied"
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(Update::Done { request_id, reply });
        });
    }
}

fn request_timeout(op: &str) -> Duration {
    Duration::from_secs(match op {
        "describe" => 8,
        "profiles" | "net-status" => 10,
        "list" => 60,
        _ => FRAME_TIMEOUT_SECS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    fn tmp_socket_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "miyori-manager-daemon-test-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reply_reaches_channel_with_matching_request_id() {
        let dir = tmp_socket_dir("ok");
        let socket_path = dir.join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            writeln!(writer, "{}", serde_json::json!({"ok": true, "data": "hi"})).unwrap();
        });

        let (tx, rx) = sync_channel(128);
        let daemon = Daemon::new(socket_path);
        daemon.send(7, serde_json::json!({"op": "list"}), tx);

        let update = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        match update {
            Update::Done { request_id, reply } => {
                assert_eq!(request_id, 7);
                assert_eq!(
                    reply.unwrap(),
                    miyori_proto::client::Reply::Ok(serde_json::json!("hi"))
                );
            }
            Update::Progress { text, .. } => panic!("не ждали progress, получили {text}"),
        }

        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_frames_arrive_in_order_before_done() {
        let dir = tmp_socket_dir("progress");
        let socket_path = dir.join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            writeln!(writer, "{}", serde_json::json!({"progress": "шаг1"})).unwrap();
            writeln!(writer, "{}", serde_json::json!({"progress": "шаг2"})).unwrap();
            writeln!(writer, "{}", serde_json::json!({"ok": true, "data": 42})).unwrap();
        });

        let (tx, rx) = sync_channel(128);
        let daemon = Daemon::new(socket_path);
        daemon.send(1, serde_json::json!({"op": "build"}), tx);

        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let third = rx.recv_timeout(Duration::from_secs(5)).unwrap();

        match first {
            Update::Progress { text, .. } => assert_eq!(text, "шаг1"),
            Update::Done { .. } => panic!("ждали progress первым"),
        }
        match second {
            Update::Progress { text, .. } => assert_eq!(text, "шаг2"),
            Update::Done { .. } => panic!("ждали progress вторым"),
        }
        match third {
            Update::Done { request_id, reply } => {
                assert_eq!(request_id, 1);
                assert_eq!(
                    reply.unwrap(),
                    miyori_proto::client::Reply::Ok(serde_json::json!(42))
                );
            }
            Update::Progress { text, .. } => panic!("ждали Done третьим, получили progress {text}"),
        }

        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_socket_sends_done_with_err_not_silence() {
        let dir = tmp_socket_dir("missing");
        let socket_path = dir.join("control.sock");

        let (tx, rx) = sync_channel(128);
        let daemon = Daemon::new(socket_path);
        daemon.send(3, serde_json::json!({"op": "list"}), tx);

        let update = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        match update {
            Update::Done { request_id, reply } => {
                assert_eq!(request_id, 3);
                assert!(reply.is_err());
            }
            Update::Progress { text, .. } => panic!("не ждали progress, получили {text}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_timeouts_do_not_inherit_long_mutation_timeout() {
        assert_eq!(request_timeout("describe"), Duration::from_secs(8));
        assert_eq!(request_timeout("profiles"), Duration::from_secs(10));
        assert_eq!(request_timeout("net-status"), Duration::from_secs(10));
        assert_eq!(request_timeout("list"), Duration::from_secs(60));
        assert_eq!(request_timeout("build"), Duration::from_secs(120));
    }

    #[test]
    fn progress_backlog_is_bounded_but_terminal_reply_is_not_dropped() {
        let dir = tmp_socket_dir("backlog");
        let socket_path = dir.join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            for _ in 0..300 {
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({"progress":"я".repeat(2048)})
                )
                .unwrap();
            }
            writeln!(stream, "{}", serde_json::json!({"ok":true,"data":null})).unwrap();
        });
        let (tx, rx) = sync_channel(2);
        Daemon::new(socket_path).send(91, serde_json::json!({"op":"build"}), tx);
        server.join().unwrap();
        let mut count = 0;
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Update::Progress { request_id, text } => {
                    count += 1;
                    assert_eq!(request_id, 91);
                    assert_eq!(text.chars().count(), 1024);
                }
                Update::Done { request_id, reply } => {
                    assert_eq!(request_id, 91);
                    assert!(reply.is_ok());
                    break;
                }
            }
        }
        assert!(
            count < 300,
            "saturated progress queue did not drop intermediate frames"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
