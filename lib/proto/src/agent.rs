use crate::read_capped_line;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::BufRead;

pub const MAX_AGENT_REPLY_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case", deny_unknown_fields)]
pub enum AgentRequest {
    // {} обязателен: deny_unknown_fields у внутреннего тега не действует на unit-варианты
    Health {},
    Shutdown {},
    // без аргументов: строка запуска из сокета не доходит до exec (решение G)
    ExecApp {},
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReply {
    pub nonce: String,
    pub ok: bool,
    #[serde(default)]
    pub detail: Option<String>,
    // Some(...) только в ответе на shutdown, для health/exec-app остаётся None
    #[serde(default)]
    pub data_unmounted: Option<bool>,
}

// ответ гостя — недоверенные данные (решение H): лимит и строгий разбор обязательны
pub fn read_agent_reply<R: BufRead>(r: &mut R) -> Result<AgentReply> {
    let line = read_capped_line(r, MAX_AGENT_REPLY_BYTES)?;
    serde_json::from_str(&line).context("некорректный ответ агента")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn agent_reply_requires_nonce() {
        let mut r = BufReader::new(b"{\"ok\":true}\n".as_slice());
        assert!(read_agent_reply(&mut r).is_err());
    }

    #[test]
    fn agent_reply_over_limit_is_refused() {
        let huge = vec![b'a'; 1024 * 1024];
        let mut r = BufReader::new(&huge[..]);
        assert!(read_agent_reply(&mut r).is_err());
    }

    #[test]
    fn agent_exec_app_has_no_arguments() {
        let json = serde_json::to_string(&AgentRequest::ExecApp {}).unwrap();
        assert_eq!(json, r#"{"cmd":"exec-app"}"#);
    }
}
