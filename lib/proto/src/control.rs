use crate::ids::{Color, Label, SpaceId};
use crate::read_capped_line;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

pub const MAX_REQUEST_FRAME_BYTES: usize = 8 * 1024;
pub const REQUEST_READ_TIMEOUT_SECS: u64 = 5;

// список закрыт: то, чем read_request проверяет "op", прежде чем пробовать разобрать в Request
const KNOWN_OPS: [&str; 13] = [
    "list",
    "describe",
    "build",
    "create",
    "start",
    "stop",
    "open-window",
    "reset-system",
    "reset-all",
    "update-image",
    "destroy",
    "net-status",
    "profiles",
];

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    // {} обязателен: deny_unknown_fields у внутреннего тега не действует на unit-варианты
    List {},
    Describe {
        space: SpaceId,
    },
    // профиль — такой же безопасный слаг, что и id спейса, отдельный тип не заводим
    Build {
        profile: SpaceId,
    },
    Create {
        space: SpaceId,
        profile: SpaceId,
        label: Label,
        color: Color,
        // абсолютный путь на хосте; отсутствует у старых клиентов — канал живёт ровно один create, не дольше
        #[serde(default)]
        seed: Option<String>,
        // непустая ⇒ шифруются оба тома; демон не хранит её и не умеет открыть диск сам (ADR-9)
        #[serde(default)]
        passphrase: Option<String>,
    },
    Start {
        space: SpaceId,
        #[serde(default)]
        passphrase: Option<String>,
    },
    Stop {
        space: SpaceId,
    },
    // спейс уже Running: agent.rs::exec_app просто спавнит ещё один waypipe-сервер поверх той же аренды
    OpenWindow {
        space: SpaceId,
    },
    ResetSystem {
        space: SpaceId,
        #[serde(default)]
        passphrase: Option<String>,
    },
    ResetAll {
        space: SpaceId,
        #[serde(default)]
        passphrase: Option<String>,
    },
    // пересаживает системный слой на новый шаблон профиля, data.qcow2 не трогает (в отличие от обеих reset-операций)
    UpdateImage {
        space: SpaceId,
        #[serde(default)]
        passphrase: Option<String>,
    },
    Destroy {
        space: SpaceId,
    },
    NetStatus {},
    Profiles {},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    BadRequest,
    UnknownOp,
    SpaceNotFound,
    SpaceExists,
    ProfileNotFound,
    TemplateNotFound,
    SpaceRunning,
    SpaceStopped,
    NoBridge,
    AgentTimeout,
    // ровно на текст QEMU "Invalid password, cannot unlock any keyslot" (ADR-9)
    WrongPassphrase,
    Internal,
}

#[derive(Debug)]
pub struct RequestError {
    pub code: ErrorCode,
    pub message: String,
}

fn bad_request(message: impl Into<String>) -> RequestError {
    RequestError {
        code: ErrorCode::BadRequest,
        message: message.into(),
    }
}

pub fn read_request<R: BufRead>(r: &mut R) -> Result<Request, RequestError> {
    let line =
        read_capped_line(r, MAX_REQUEST_FRAME_BYTES).map_err(|err| bad_request(err.to_string()))?;
    let value: serde_json::Value = serde_json::from_str(&line)
        .map_err(|err| bad_request(format!("запрос не является корректным JSON: {err}")))?;

    // op проверяется по закрытому списку до разбора в Request: только так код unknown-op достижим
    let op = value.get("op").and_then(serde_json::Value::as_str);
    match op {
        Some(op) if KNOWN_OPS.contains(&op) => {}
        _ => {
            return Err(RequestError {
                code: ErrorCode::UnknownOp,
                message: format!("операция {op:?} неизвестна"),
            })
        }
    }

    // разбираем строку заново, а не Value: from_value молча берёт последний из дублирующихся ключей
    serde_json::from_str(&line).map_err(|err| bad_request(format!("некорректный запрос: {err}")))
}

// пишет один JSON-объект на строку; и progress, и терминальный кадр Responder-а идут через неё
fn write_line<W: Write>(w: &mut W, value: &serde_json::Value) -> Result<()> {
    let line = serde_json::to_string(value).context("не удалось сериализовать кадр")?;
    writeln!(w, "{line}").context("не удалось записать кадр")
}

pub struct Responder<W: Write> {
    writer: W,
}

impl<W: Write> Responder<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    pub fn progress(&mut self, message: &str) -> Result<()> {
        write_line(
            &mut self.writer,
            &serde_json::json!({ "progress": message }),
        )
    }

    // self по значению — после терминального кадра писать больше нечем, второй такой же не скомпилируется
    pub fn finish(mut self, result: Result<serde_json::Value, (ErrorCode, String)>) -> Result<()> {
        let value = match result {
            Ok(data) => serde_json::json!({ "ok": true, "data": data }),
            Err((code, message)) => {
                serde_json::json!({ "ok": false, "code": code, "message": message })
            }
        };
        write_line(&mut self.writer, &value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::{BufReader, Cursor, Read};
    use std::rc::Rc;

    // считает байты, реально отданные из источника, а не то, сколько попросил вызывающий код
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
    fn parses_minimal_request() {
        let mut r = Cursor::new(b"{\"op\":\"list\"}\n".to_vec());
        assert_eq!(read_request(&mut r).unwrap(), Request::List {});
    }

    #[test]
    fn space_id_is_validated_at_parse_time() {
        let mut r = Cursor::new(b"{\"op\":\"start\",\"space\":\"../etc\"}\n".to_vec());
        assert!(read_request(&mut r).is_err());
    }

    #[test]
    fn space_id_length_is_capped() {
        let long_id = "a".repeat(33);
        let line = format!("{{\"op\":\"start\",\"space\":\"{long_id}\"}}\n");
        let mut r = Cursor::new(line.into_bytes());
        assert!(read_request(&mut r).is_err());
    }

    #[test]
    fn unknown_field_is_rejected_not_ignored() {
        // struct-вариант с полями
        let mut r = Cursor::new(b"{\"op\":\"start\",\"space\":\"a\",\"gpu\":\"venus\"}\n".to_vec());
        assert!(
            read_request(&mut r).is_err(),
            "start с лишним полем должен быть отвергнут"
        );

        // вариант без своих полей — та самая дыра: deny_unknown_fields на unit-вариантах не работал
        let mut r = Cursor::new(b"{\"op\":\"list\",\"evil\":1}\n".to_vec());
        assert!(
            read_request(&mut r).is_err(),
            "list с лишним полем должен быть отвергнут"
        );

        let mut r = Cursor::new(b"{\"op\":\"net-status\",\"evil\":\"x\"}\n".to_vec());
        assert!(
            read_request(&mut r).is_err(),
            "net-status с лишним полем должен быть отвергнут"
        );
    }

    #[test]
    fn parses_profiles_request() {
        let mut r = Cursor::new(b"{\"op\":\"profiles\"}\n".to_vec());
        assert_eq!(read_request(&mut r).unwrap(), Request::Profiles {});
    }

    #[test]
    fn parses_update_image_request() {
        let mut r = Cursor::new(b"{\"op\":\"update-image\",\"space\":\"a\"}\n".to_vec());
        assert_eq!(
            read_request(&mut r).unwrap(),
            Request::UpdateImage {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            }
        );
    }

    #[test]
    fn update_image_with_extra_field_is_bad_request() {
        let mut r =
            Cursor::new(b"{\"op\":\"update-image\",\"space\":\"a\",\"gpu\":\"venus\"}\n".to_vec());
        let err = read_request(&mut r).unwrap_err();
        assert_eq!(err.code, ErrorCode::BadRequest, "{}", err.message);
    }

    #[test]
    fn profiles_with_extra_field_is_bad_request_not_unknown_op() {
        // "profiles" узнан по KNOWN_OPS — отказ должен идти за лишнее поле, а не за "операция неизвестна"
        let mut r = Cursor::new(
            b"{\"op\":\"profiles\",\"\xd0\xbb\xd0\xb8\xd1\x88\xd0\xbd\xd0\xb5\xd0\xb5\":1}\n"
                .to_vec(),
        );
        let err = read_request(&mut r).unwrap_err();
        assert_eq!(err.code, ErrorCode::BadRequest, "{}", err.message);
    }

    #[test]
    fn unknown_op_is_rejected() {
        let mut r = Cursor::new(b"{\"op\":\"rm -rf\"}\n".to_vec());
        assert!(read_request(&mut r).is_err());
    }

    #[test]
    fn duplicate_field_is_rejected() {
        let mut r =
            Cursor::new(b"{\"op\":\"describe\",\"space\":\"a\",\"space\":\"b\"}\n".to_vec());
        let err = read_request(&mut r).unwrap_err();
        assert_eq!(err.code, ErrorCode::BadRequest, "{}", err.message);
    }

    #[test]
    fn unknown_op_gets_unknown_op_code() {
        let mut r = Cursor::new(b"{\"op\":\"rm -rf\"}\n".to_vec());
        let err = read_request(&mut r).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnknownOp);
    }

    #[test]
    fn malformed_json_gets_bad_request_code() {
        let mut r = Cursor::new(b"{not json at all\n".to_vec());
        let err = read_request(&mut r).unwrap_err();
        assert_eq!(err.code, ErrorCode::BadRequest);
    }

    #[test]
    fn deeply_nested_json_is_refused() {
        let depth = 4000;
        let mut line: Vec<u8> = "[".repeat(depth).into_bytes();
        line.extend(std::iter::repeat_n(b']', depth));
        line.push(b'\n');
        assert!(
            line.len() <= MAX_REQUEST_FRAME_BYTES,
            "тест сам должен уложиться в лимит"
        );
        let mut r = Cursor::new(line);
        // важно само отсутствие паники (переполнение стека рекурсивным разбором), а не конкретный код
        assert!(read_request(&mut r).is_err());
    }

    #[test]
    fn create_without_seed_still_parses() {
        let mut r = Cursor::new(
            b"{\"op\":\"create\",\"space\":\"a\",\"profile\":\"p\",\"label\":\"l\",\"color\":\"#e03131\"}\n"
                .to_vec(),
        );
        let req = read_request(&mut r).unwrap();
        assert_eq!(
            req,
            Request::Create {
                space: SpaceId::new("a").unwrap(),
                profile: SpaceId::new("p").unwrap(),
                label: Label::new("l").unwrap(),
                color: Color::new("#e03131").unwrap(),
                seed: None,
                passphrase: None,
            }
        );
    }

    #[test]
    fn create_with_seed_parses() {
        let mut r = Cursor::new(
            b"{\"op\":\"create\",\"space\":\"a\",\"profile\":\"p\",\"label\":\"l\",\"color\":\"#e03131\",\"seed\":\"/home/op/files\"}\n"
                .to_vec(),
        );
        let req = read_request(&mut r).unwrap();
        assert_eq!(
            req,
            Request::Create {
                space: SpaceId::new("a").unwrap(),
                profile: SpaceId::new("p").unwrap(),
                label: Label::new("l").unwrap(),
                color: Color::new("#e03131").unwrap(),
                seed: Some("/home/op/files".to_string()),
                passphrase: None,
            }
        );
    }

    #[test]
    fn create_with_passphrase_parses() {
        let mut r = Cursor::new(
            b"{\"op\":\"create\",\"space\":\"a\",\"profile\":\"p\",\"label\":\"l\",\"color\":\"#e03131\",\"passphrase\":\"hunter2\"}\n"
                .to_vec(),
        );
        let req = read_request(&mut r).unwrap();
        assert_eq!(
            req,
            Request::Create {
                space: SpaceId::new("a").unwrap(),
                profile: SpaceId::new("p").unwrap(),
                label: Label::new("l").unwrap(),
                color: Color::new("#e03131").unwrap(),
                seed: None,
                passphrase: Some("hunter2".to_string()),
            }
        );
    }

    #[test]
    fn start_without_passphrase_still_parses() {
        let mut r = Cursor::new(b"{\"op\":\"start\",\"space\":\"a\"}\n".to_vec());
        assert_eq!(
            read_request(&mut r).unwrap(),
            Request::Start {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            }
        );
    }

    #[test]
    fn start_with_passphrase_parses() {
        let mut r = Cursor::new(
            b"{\"op\":\"start\",\"space\":\"a\",\"passphrase\":\"hunter2\"}\n".to_vec(),
        );
        assert_eq!(
            read_request(&mut r).unwrap(),
            Request::Start {
                space: SpaceId::new("a").unwrap(),
                passphrase: Some("hunter2".to_string()),
            }
        );
    }

    #[test]
    fn reset_system_and_reset_all_without_passphrase_still_parse() {
        let mut r = Cursor::new(b"{\"op\":\"reset-system\",\"space\":\"a\"}\n".to_vec());
        assert_eq!(
            read_request(&mut r).unwrap(),
            Request::ResetSystem {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            }
        );

        let mut r = Cursor::new(b"{\"op\":\"reset-all\",\"space\":\"a\"}\n".to_vec());
        assert_eq!(
            read_request(&mut r).unwrap(),
            Request::ResetAll {
                space: SpaceId::new("a").unwrap(),
                passphrase: None,
            }
        );
    }

    #[test]
    fn wrong_passphrase_error_code_serializes_to_kebab_case() {
        let mut buf = Vec::new();
        Responder::new(&mut buf)
            .finish(Err((
                ErrorCode::WrongPassphrase,
                "неверный пароль".to_string(),
            )))
            .unwrap();
        let line = String::from_utf8(buf).unwrap();
        let value: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(value["code"], "wrong-passphrase");
    }

    #[test]
    fn create_with_extra_field_is_bad_request() {
        let mut r = Cursor::new(
            b"{\"op\":\"create\",\"space\":\"a\",\"profile\":\"p\",\"label\":\"l\",\"color\":\"#e03131\",\"gpu\":\"venus\"}\n"
                .to_vec(),
        );
        let err = read_request(&mut r).unwrap_err();
        assert_eq!(err.code, ErrorCode::BadRequest, "{}", err.message);
    }

    #[test]
    fn color_must_be_rrggbb() {
        for bad in ["#xyz", "red", "#1234567"] {
            let line = format!(
                "{{\"op\":\"create\",\"space\":\"a\",\"profile\":\"p\",\"label\":\"l\",\"color\":\"{bad}\"}}\n"
            );
            let mut r = Cursor::new(line.into_bytes());
            assert!(read_request(&mut r).is_err(), "{bad} должен быть отвергнут");
        }
    }

    #[test]
    fn oversized_line_is_refused_without_buffering_it() {
        let huge = vec![b'a'; 1024 * 1024];
        let total = Rc::new(Cell::new(0));
        let counting = CountingReader {
            data: &huge,
            pos: 0,
            total: total.clone(),
        };
        let mut r = BufReader::new(counting);
        assert!(read_request(&mut r).is_err());
        assert!(
            total.get() <= MAX_REQUEST_FRAME_BYTES * 2,
            "прочитано {} байт при лимите {}",
            total.get(),
            MAX_REQUEST_FRAME_BYTES
        );
    }

    #[test]
    fn truncated_frame_is_refused() {
        let mut r = Cursor::new(b"{\"op\":\"list\"}".to_vec());
        assert!(read_request(&mut r).is_err());
    }

    #[test]
    fn nul_and_control_bytes_are_refused() {
        for byte in [0x00u8, 0x1b, 0x0d] {
            let mut raw = b"{\"op\":\"list\"".to_vec();
            raw.push(byte);
            raw.extend_from_slice(b"}\n");
            let mut r = Cursor::new(raw);
            assert!(
                read_request(&mut r).is_err(),
                "байт 0x{byte:02x} должен быть отвергнут"
            );
        }
    }

    #[test]
    fn second_request_on_same_connection_is_ignored() {
        let mut r = Cursor::new(b"{\"op\":\"list\"}\n{\"op\":\"net-status\"}\n".to_vec());
        assert_eq!(read_request(&mut r).unwrap(), Request::List {});
        let mut rest = String::new();
        r.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "{\"op\":\"net-status\"}\n");
    }

    #[test]
    fn error_frame_carries_code_and_message() {
        let mut buf = Vec::new();
        Responder::new(&mut buf)
            .finish(Err((
                ErrorCode::SpaceNotFound,
                "нет спейса \"x\"".to_string(),
            )))
            .unwrap();
        let line = String::from_utf8(buf).unwrap();
        let value: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["code"], "space-not-found");
        assert_eq!(value["message"], "нет спейса \"x\"");
    }

    #[test]
    fn terminal_frame_is_exactly_one() {
        let mut buf = Vec::new();
        let mut responder = Responder::new(&mut buf);
        responder.progress("качаем шаблон").unwrap();
        responder.progress("распаковываем").unwrap();
        // finish забирает self по значению — второй терминальный кадр писать больше некуда, это гарантия типа
        responder
            .finish(Ok(serde_json::json!({"done": true})))
            .unwrap();

        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        let terminal_count = lines
            .iter()
            .filter(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .unwrap()
                    .get("ok")
                    .is_some()
            })
            .count();
        assert_eq!(terminal_count, 1);
    }
}
