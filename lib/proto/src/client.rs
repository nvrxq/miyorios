use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub const MAX_RESPONSE_FRAME_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Ok(serde_json::Value),
    Err { code: String, message: String },
}

pub fn exchange<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    request: &serde_json::Value,
    on_progress: &mut dyn FnMut(&str),
) -> Result<Reply> {
    let line = serde_json::to_string(request).context("не удалось сериализовать запрос")?;
    writeln!(writer, "{line}").context("не удалось отправить запрос")?;
    writer.flush().context("не удалось отправить запрос")?;

    loop {
        let line = crate::read_capped_line(reader, MAX_RESPONSE_FRAME_BYTES)
            .context("демон не прислал ответа")?;
        let value: serde_json::Value =
            serde_json::from_str(&line).context("кадр ответа не является корректным JSON")?;

        if let Some(progress) = value.get("progress") {
            let text = progress
                .as_str()
                .context("поле progress не является строкой")?;
            on_progress(text);
            continue;
        }

        if let Some(ok) = value.get("ok") {
            let ok = ok.as_bool().context("поле ok не является булевым")?;
            if ok {
                let data = value
                    .get("data")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                return Ok(Reply::Ok(data));
            }
            let code = value
                .get("code")
                .and_then(serde_json::Value::as_str)
                .context("отказ демона пришёл без code")?;
            let message = value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .context("отказ демона пришёл без message")?;
            return Ok(Reply::Err {
                code: code.to_string(),
                message: message.to_string(),
            });
        }

        bail!("кадр ответа не содержит ни progress, ни ok: {line}");
    }
}

pub fn call(
    socket: &Path,
    request: &serde_json::Value,
    read_timeout: Duration,
    on_progress: &mut dyn FnMut(&str),
) -> Result<Reply> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("демон не отвечает на {}", socket.display()))?;
    // таймаут действует на одно чтение кадра, а не на весь обмен: build идёт минутами, и каждый progress начинает отсчёт заново
    stream
        .set_read_timeout(Some(read_timeout))
        .context("не удалось установить таймаут чтения")?;
    stream
        .set_write_timeout(Some(read_timeout))
        .context("не удалось установить таймаут записи")?;
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .context("не удалось продублировать сокет для чтения")?,
    );
    let mut writer = stream;
    exchange(&mut reader, &mut writer, request, on_progress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::{BufReader, Cursor, Read};
    use std::os::unix::net::UnixListener;
    use std::rc::Rc;

    // считает байты, реально отданные из источника — образец в control.rs
    struct CountingReader<'a> {
        data: &'a [u8],
        pos: usize,
        total: Rc<Cell<usize>>,
    }

    impl<'a> Read for CountingReader<'a> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let remaining = &self.data[self.pos..];
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            self.pos += n;
            self.total.set(self.total.get() + n);
            Ok(n)
        }
    }

    #[test]
    fn terminal_ok_becomes_reply_ok_with_same_data() {
        let mut reader = Cursor::new(b"{\"ok\":true,\"data\":{\"n\":1}}\n".to_vec());
        let mut writer = Vec::new();
        let reply = exchange(
            &mut reader,
            &mut writer,
            &serde_json::json!({"op":"list"}),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(reply, Reply::Ok(serde_json::json!({"n": 1})));
    }

    #[test]
    fn terminal_err_becomes_reply_err_with_verbatim_code_and_message() {
        let mut reader = Cursor::new(
            b"{\"ok\":false,\"code\":\"space-not-found\",\"message\":\"\xd0\xbd\xd0\xb5\xd1\x82 \xd1\x81\xd0\xbf\xd0\xb5\xd0\xb9\xd1\x81\xd0\xb0 \\\"x\\\"\"}\n"
                .to_vec(),
        );
        let mut writer = Vec::new();
        let reply = exchange(
            &mut reader,
            &mut writer,
            &serde_json::json!({"op":"start","space":"x"}),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            reply,
            Reply::Err {
                code: "space-not-found".to_string(),
                message: "нет спейса \"x\"".to_string(),
            }
        );
    }

    #[test]
    fn progress_frames_are_delivered_in_order_before_terminal() {
        let mut reader = Cursor::new(
            b"{\"progress\":\"\xd1\x88\xd0\xb0\xd0\xb31\"}\n{\"progress\":\"\xd1\x88\xd0\xb0\xd0\xb32\"}\n{\"ok\":true,\"data\":42}\n"
                .to_vec(),
        );
        let mut writer = Vec::new();
        let mut calls: Vec<String> = Vec::new();
        let mut on_progress = |s: &str| calls.push(s.to_string());
        let reply = exchange(
            &mut reader,
            &mut writer,
            &serde_json::json!({"op":"build","profile":"p"}),
            &mut on_progress,
        )
        .unwrap();
        assert_eq!(calls, vec!["шаг1".to_string(), "шаг2".to_string()]);
        assert_eq!(reply, Reply::Ok(serde_json::json!(42)));
    }

    #[test]
    fn request_is_written_as_a_single_line_with_trailing_newline() {
        let mut reader = Cursor::new(b"{\"ok\":true,\"data\":null}\n".to_vec());
        let mut writer = Vec::new();
        let request = serde_json::json!({"op":"list"});
        exchange(&mut reader, &mut writer, &request, &mut |_| {}).unwrap();
        let sent = String::from_utf8(writer).unwrap();
        assert_eq!(
            sent,
            format!("{}\n", serde_json::to_string(&request).unwrap())
        );
    }

    #[test]
    fn stream_closing_without_terminal_frame_is_an_error() {
        let mut reader = Cursor::new(b"{\"progress\":\"x\"}\n".to_vec());
        let mut writer = Vec::new();
        assert!(exchange(
            &mut reader,
            &mut writer,
            &serde_json::json!({"op":"list"}),
            &mut |_| {}
        )
        .is_err());
    }

    #[test]
    fn non_json_frame_is_an_error() {
        let mut reader = Cursor::new(b"not json\n".to_vec());
        let mut writer = Vec::new();
        assert!(exchange(
            &mut reader,
            &mut writer,
            &serde_json::json!({"op":"list"}),
            &mut |_| {}
        )
        .is_err());
    }

    #[test]
    fn frame_without_progress_or_ok_is_an_error() {
        let mut reader = Cursor::new(b"{\"something\":1}\n".to_vec());
        let mut writer = Vec::new();
        assert!(exchange(
            &mut reader,
            &mut writer,
            &serde_json::json!({"op":"list"}),
            &mut |_| {}
        )
        .is_err());
    }

    #[test]
    fn progress_value_that_is_not_a_string_is_an_error() {
        let mut reader = Cursor::new(b"{\"progress\":7}\n".to_vec());
        let mut writer = Vec::new();
        assert!(exchange(
            &mut reader,
            &mut writer,
            &serde_json::json!({"op":"list"}),
            &mut |_| {}
        )
        .is_err());
    }

    #[test]
    fn ok_false_without_message_is_an_error() {
        let mut reader = Cursor::new(b"{\"ok\":false,\"code\":\"internal\"}\n".to_vec());
        let mut writer = Vec::new();
        assert!(exchange(
            &mut reader,
            &mut writer,
            &serde_json::json!({"op":"list"}),
            &mut |_| {}
        )
        .is_err());
    }

    #[test]
    fn ok_false_without_code_is_an_error() {
        let mut reader = Cursor::new(b"{\"ok\":false,\"message\":\"x\"}\n".to_vec());
        let mut writer = Vec::new();
        assert!(exchange(
            &mut reader,
            &mut writer,
            &serde_json::json!({"op":"list"}),
            &mut |_| {}
        )
        .is_err());
    }

    #[test]
    fn oversized_frame_is_refused_without_buffering_it() {
        let huge = vec![b'a'; 1024 * 1024];
        let total = Rc::new(Cell::new(0));
        let counting = CountingReader {
            data: &huge,
            pos: 0,
            total: total.clone(),
        };
        let mut reader = BufReader::new(counting);
        let mut writer = Vec::new();
        assert!(exchange(
            &mut reader,
            &mut writer,
            &serde_json::json!({"op":"list"}),
            &mut |_| {}
        )
        .is_err());
        assert!(
            total.get() <= MAX_RESPONSE_FRAME_BYTES * 2,
            "прочитано {} байт при лимите {}",
            total.get(),
            MAX_RESPONSE_FRAME_BYTES
        );
    }

    #[test]
    fn frames_after_terminal_frame_are_left_unread() {
        let mut reader = Cursor::new(b"{\"ok\":true,\"data\":1}\n{\"op\":\"trailing\"}\n".to_vec());
        let mut writer = Vec::new();
        let reply = exchange(
            &mut reader,
            &mut writer,
            &serde_json::json!({"op":"list"}),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(reply, Reply::Ok(serde_json::json!(1)));
        let mut rest = String::new();
        reader.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "{\"op\":\"trailing\"}\n");
    }

    #[test]
    fn call_reaches_socket_and_returns_reply() {
        let dir = std::env::temp_dir().join(format!(
            "miyori-proto-client-test-{}-ok",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket_path = dir.join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            writeln!(
                writer,
                "{}",
                serde_json::json!({"ok": true, "data": "готово"})
            )
            .unwrap();
        });

        let reply = call(
            &socket_path,
            &serde_json::json!({"op":"list"}),
            Duration::from_secs(5),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(reply, Reply::Ok(serde_json::json!("готово")));

        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn call_to_missing_socket_reports_the_path() {
        let path = std::env::temp_dir().join(format!(
            "miyori-proto-client-test-{}-missing.sock",
            std::process::id()
        ));
        let err = call(
            &path,
            &serde_json::json!({"op":"list"}),
            Duration::from_secs(1),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
    }
}
