#![forbid(unsafe_code)]

pub mod agent;
pub mod client;
pub mod control;
pub mod ids;

use anyhow::{bail, Context, Result};
use std::io::BufRead;

// read_line не даёт остановиться на лимите, не прочитав всю строку — оба протокола читают сами, побайтно
pub fn read_capped_line<R: BufRead>(r: &mut R, limit: usize) -> Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = r.read(&mut byte).context("ошибка чтения кадра")?;
        if n == 0 {
            bail!("соединение закрылось до завершения строки символом \\n");
        }
        if buf.len() + 1 > limit {
            bail!("кадр превышает лимит {limit} байт");
        }
        let b = byte[0];
        if b == b'\n' {
            return String::from_utf8(buf).context("кадр не является корректным UTF-8");
        }
        if b.is_ascii_control() {
            bail!("кадр содержит запрещённый управляющий байт 0x{b:02x}");
        }
        buf.push(b);
    }
}
