#![forbid(unsafe_code)]

use miyori_proto::client::Reply;
use serde_json::Value;

#[derive(Clone, PartialEq, Eq)]
pub struct SpaceRow {
    pub id: String,
    pub label: String,
    pub color: String,
    pub profile: String,
    pub state: String,
    pub level: String,
    pub level_reason: String,
    pub encrypted: bool,
}

pub enum SpacesView {
    Spaces(Vec<SpaceRow>),
    Unavailable { message: String },
}

pub fn spaces_view(reply: Result<Reply, String>) -> SpacesView {
    let value = match reply_to_value(reply) {
        Ok(value) => value,
        Err(message) => return SpacesView::Unavailable { message },
    };
    let items = match value.as_array() {
        Some(items) => items,
        None => {
            return SpacesView::Unavailable {
                message: "демон вернул неожиданный ответ вместо списка спейсов".to_string(),
            }
        }
    };

    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        match space_row(item) {
            Some(row) => rows.push(row),
            None => {
                return SpacesView::Unavailable {
                    message: "демон прислал спейс без обязательного поля".to_string(),
                }
            }
        }
    }
    SpacesView::Spaces(rows)
}

// требует ok/err в общем виде — spaces_view, detail_view и net_view отказываются одинаково
fn reply_to_value(reply: Result<Reply, String>) -> Result<Value, String> {
    match reply {
        Err(transport) => Err(transport),
        Ok(Reply::Err { code, message }) => Err(format!("{code}: {message}")),
        Ok(Reply::Ok(value)) => Ok(value),
    }
}

fn field_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn space_row(item: &Value) -> Option<SpaceRow> {
    Some(SpaceRow {
        id: field_str(item, "id")?,
        label: field_str(item, "label")?,
        color: field_str(item, "color")?,
        profile: field_str(item, "profile")?,
        state: field_str(item, "state")?,
        level: field_str(item, "isolation-level")?,
        level_reason: field_str(item, "isolation-reason")?,
        // менеджер может оказаться новее демона: поле не приехало — шифрования нет, а не отказ разбирать строку
        encrypted: field_bool(item, "encrypted").unwrap_or(false),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRow {
    pub profile: String,
    pub template: bool,
    pub manifest_ok: bool,
    pub isolation_level: Option<String>,
    pub manifest_error: Option<String>,
}

pub enum ProfilesView {
    Profiles(Vec<ProfileRow>),
    Unavailable { message: String },
}

pub fn profiles_view(reply: Result<Reply, String>) -> ProfilesView {
    let value = match reply_to_value(reply) {
        Ok(value) => value,
        Err(message) => return ProfilesView::Unavailable { message },
    };
    let items = match value.as_array() {
        Some(items) => items,
        None => {
            return ProfilesView::Unavailable {
                message: "демон вернул неожиданный ответ вместо списка профилей".to_string(),
            }
        }
    };

    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        match profile_row(item) {
            Some(row) => rows.push(row),
            None => {
                return ProfilesView::Unavailable {
                    message: "демон прислал профиль без обязательного поля".to_string(),
                }
            }
        }
    }
    ProfilesView::Profiles(rows)
}

fn field_bool(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

// null — законное значение (манифест сломан, уровня нет); отсутствие ключа — испорченный ответ
fn nullable_field_str(value: &Value, key: &str) -> Option<Option<String>> {
    match value.get(key)? {
        Value::Null => Some(None),
        Value::String(s) => Some(Some(s.clone())),
        _ => None,
    }
}

fn profile_row(item: &Value) -> Option<ProfileRow> {
    Some(ProfileRow {
        profile: field_str(item, "profile")?,
        template: field_bool(item, "template")?,
        manifest_ok: field_bool(item, "manifest-ok")?,
        isolation_level: nullable_field_str(item, "isolation-level")?,
        manifest_error: nullable_field_str(item, "manifest-error")?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailRow {
    pub key: String,
    pub value: String,
}

// группа определяет, под каким заголовком карточки окажется строка — правая панель, документации проекта
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailSection {
    Space,
    Identity,
    Resources,
    Network,
}

// mono — машинное значение (дайджест, путь, cid/uid), которому нужен моноширинный шрифт и обрезка, а не перенос
#[derive(Debug)]
pub struct SpaceDetailRow {
    pub key: String,
    pub value: String,
    pub section: DetailSection,
    pub mono: bool,
}

pub enum DetailView {
    Space {
        rows: Vec<SpaceDetailRow>,
        log_tail: Vec<String>,
    },
    Unavailable {
        message: String,
    },
}

pub fn detail_view(reply: Result<Reply, String>) -> DetailView {
    let value = match reply_to_value(reply) {
        Ok(value) => value,
        Err(message) => return DetailView::Unavailable { message },
    };
    if !value.is_object() {
        return DetailView::Unavailable {
            message: "демон вернул неожиданный ответ вместо описания спейса".to_string(),
        };
    }

    let row = |key: &str, text: String, section: DetailSection, mono: bool| SpaceDetailRow {
        key: key.to_string(),
        value: text,
        section,
        mono,
    };
    let field_or_dash =
        |json_key: &str| field_str(&value, json_key).unwrap_or_else(|| "—".to_string());
    use DetailSection::{Identity, Network, Resources, Space};

    let rows = vec![
        row("Профиль", field_or_dash("profile"), Space, false),
        row("Описание", field_or_dash("description"), Space, false),
        row("Состояние", field_or_dash("state"), Space, false),
        row("CID", num_or_dash(&value, "cid"), Identity, true),
        row("uid", num_or_dash(&value, "uid"), Identity, true),
        row("Создан", field_or_dash("created"), Identity, false),
        row("Приложение", app_summary(&value), Identity, false),
        row("Шаблон", field_or_dash("digest"), Identity, true),
        row("Манифест", field_or_dash("manifest-path"), Identity, true),
        row(
            "Сеть",
            nested_field_str(&value, "network", "via").unwrap_or_else(|| "—".to_string()),
            Network,
            false,
        ),
        row(
            "Уровень изоляции",
            isolation_level_summary(&value),
            Network,
            false,
        ),
        row(
            "GPU",
            nested_field_str(&value, "isolation", "gpu").unwrap_or_else(|| "—".to_string()),
            Network,
            false,
        ),
        row("Память", memory_summary(&value), Resources, false),
        row("Том данных", data_volume_summary(&value), Resources, false),
        row(
            "CPU",
            nested_num_or_dash(&value, "resources", "cpus"),
            Resources,
            false,
        ),
        row("Диск", disk_summary(&value), Resources, false),
        row(
            "Шифрование диска",
            encrypted_summary(&value),
            Resources,
            false,
        ),
        row("Время работы", uptime_summary(&value), Space, false),
        row(
            "Чистый останов",
            clean_shutdown_summary(&value),
            Space,
            false,
        ),
    ];

    let log_tail = value
        .get("log-tail")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    DetailView::Space { rows, log_tail }
}

fn nested_field_str(value: &Value, obj_key: &str, field_key: &str) -> Option<String> {
    value
        .get(obj_key)
        .and_then(|obj| obj.get(field_key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn num_or_dash(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_i64)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "—".to_string())
}

fn nested_num_or_dash(value: &Value, obj_key: &str, field_key: &str) -> String {
    value
        .get(obj_key)
        .and_then(|obj| obj.get(field_key))
        .and_then(Value::as_i64)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "—".to_string())
}

fn app_summary(value: &Value) -> String {
    let mode = nested_field_str(value, "app", "mode");
    let command = nested_field_str(value, "app", "command");
    match (mode, command) {
        (Some(mode), Some(command)) => format!("{mode} · {command}"),
        (Some(mode), None) => mode,
        (None, Some(command)) => command,
        (None, None) => "—".to_string(),
    }
}

fn isolation_level_summary(value: &Value) -> String {
    let level = match nested_field_str(value, "isolation", "level") {
        Some(level) => level,
        None => return "—".to_string(),
    };
    if level == "standard" {
        return level;
    }
    if level == "reduced" {
        let reason = nested_field_str(value, "isolation", "reason").unwrap_or_default();
        return if reason.trim().is_empty() {
            "reduced — причина не указана в манифесте".to_string()
        } else {
            format!("reduced — {reason}")
        };
    }
    level
}

// заказано в МиБ из манифеста, фактически — RSS QEMU; демон меряет, менеджер только показывает (решение C)
fn memory_summary(value: &Value) -> String {
    let requested = mib_or_dash(nested_num(value, "resources", "memory-mb"));
    let actual = mib_or_dash(
        value
            .get("rss-kib")
            .and_then(Value::as_u64)
            .map(|kib| kib / 1024),
    );
    format!("заказано {requested} · фактически {actual}")
}

fn data_volume_summary(value: &Value) -> String {
    let requested = mib_or_dash(nested_num(value, "resources", "data-mb"));
    let actual = value
        .get("data-qcow2-bytes")
        .and_then(Value::as_u64)
        .map(|bytes| format!("{:.1} МиБ", bytes as f64 / (1024.0 * 1024.0)))
        .unwrap_or_else(|| "—".to_string());
    format!("заказано {requested} · файл {actual}")
}

fn mib_or_dash(n: Option<u64>) -> String {
    match n {
        Some(n) => format!("{n} МиБ"),
        None => "—".to_string(),
    }
}

fn nested_num(value: &Value, obj_key: &str, field_key: &str) -> Option<u64> {
    value
        .get(obj_key)
        .and_then(|obj| obj.get(field_key))
        .and_then(Value::as_u64)
}

fn encrypted_summary(value: &Value) -> String {
    if field_bool(value, "encrypted").unwrap_or(false) {
        "да".to_string()
    } else {
        "нет".to_string()
    }
}

fn disk_summary(value: &Value) -> String {
    match value
        .get("resources")
        .and_then(|r| r.get("disk-gb"))
        .and_then(Value::as_i64)
    {
        Some(gb) => format!("{gb} ГиБ"),
        None => "—".to_string(),
    }
}

fn uptime_summary(value: &Value) -> String {
    let secs = match value.get("uptime-secs").and_then(Value::as_u64) {
        Some(secs) => secs,
        None => return "—".to_string(),
    };
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours} ч {minutes} мин")
    } else if minutes > 0 {
        format!("{minutes} мин {seconds} с")
    } else {
        format!("{seconds} с")
    }
}

fn clean_shutdown_summary(value: &Value) -> String {
    match value.get("clean-shutdown").and_then(Value::as_bool) {
        Some(true) => "да".to_string(),
        Some(false) => "нет".to_string(),
        None => "ещё не останавливали".to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetView {
    pub rows: Vec<DetailRow>,
    pub ruleset_text: String,
    // пусто — показывать нечего сверх текста; иначе строка, которую обязан увидеть оператор
    pub ruleset_note: String,
}

pub fn net_view(reply: Result<Reply, String>) -> Result<NetView, String> {
    let value = reply_to_value(reply)?;
    if !value.is_object() {
        return Err("демон вернул неожиданный ответ вместо статуса сети".to_string());
    }

    let mut rows = Vec::new();
    let yes_no = |b: bool| if b { "да" } else { "нет" }.to_string();

    let bridge = |key: &str| {
        value
            .get("bridges")
            .and_then(|b| b.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    rows.push(DetailRow {
        key: "Мост br-spaces".to_string(),
        value: yes_no(bridge("br-spaces")),
    });
    rows.push(DetailRow {
        key: "Мост br-captive".to_string(),
        value: yes_no(bridge("br-captive")),
    });

    if let Some(taps) = value.get("taps").and_then(Value::as_array) {
        for tap in taps {
            let name = field_str(tap, "name").unwrap_or_else(|| "—".to_string());
            let isolated = tap
                .get("isolated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            rows.push(DetailRow {
                key: format!("Tap {name}"),
                value: if isolated {
                    "изолирован".to_string()
                } else {
                    "не изолирован".to_string()
                },
            });
        }
    }

    let pci_address =
        nested_field_str(&value, "uplink-pci", "address").unwrap_or_else(|| "—".to_string());
    let pci_present = value
        .get("uplink-pci")
        .and_then(|p| p.get("present"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let pci_driver =
        nested_field_str(&value, "uplink-pci", "driver").unwrap_or_else(|| "—".to_string());
    rows.push(DetailRow {
        key: "Аплинк PCI".to_string(),
        value: format!(
            "{pci_address} · {} · драйвер {pci_driver}",
            if pci_present {
                "подключен"
            } else {
                "не подключен"
            }
        ),
    });

    let net_running = value
        .get("miyori-net")
        .and_then(|n| n.get("running"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    rows.push(DetailRow {
        key: "miyori-net".to_string(),
        value: if net_running {
            "поднят".to_string()
        } else {
            "не поднят".to_string()
        },
    });
    rows.push(DetailRow {
        key: "miyori-net PID".to_string(),
        value: nested_num_or_dash(&value, "miyori-net", "pid"),
    });
    rows.push(DetailRow {
        key: "miyori-net аплинк".to_string(),
        value: nested_field_str(&value, "miyori-net", "uplink").unwrap_or_else(|| "—".to_string()),
    });
    let tunnel_value = value
        .get("miyori-net")
        .and_then(|n| n.get("tunnel"))
        .filter(|t| !t.is_null())
        .map(|t| {
            let ifc = field_str(t, "ifc").unwrap_or_else(|| "—".to_string());
            let set = t.get("set").and_then(Value::as_bool).unwrap_or(false);
            let route = t.get("route").and_then(Value::as_bool).unwrap_or(false);
            format!("ifc={ifc} set={} route={}", yes_no(set), yes_no(route))
        })
        .unwrap_or_else(|| "—".to_string());
    rows.push(DetailRow {
        key: "miyori-net туннель".to_string(),
        value: tunnel_value,
    });

    // дословно строка демона — это честное «недоступны с хоста», а не заглушка, которую можно заменить нулём (решение D)
    rows.push(DetailRow {
        key: "Счётчики kill-switch".to_string(),
        value: field_str(&value, "killswitch_counters").unwrap_or_else(|| "—".to_string()),
    });

    let ruleset_text = nested_field_str(&value, "ruleset", "text").unwrap_or_default();
    let ruleset_available = value
        .get("ruleset")
        .and_then(|r| r.get("available"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let truncated_from = value
        .get("ruleset")
        .and_then(|r| r.get("truncated-from"))
        .and_then(Value::as_u64);

    let ruleset_note = if !ruleset_available {
        "ruleset недоступен для чтения".to_string()
    } else if let Some(total) = truncated_from {
        format!(
            "показано {} из {total} символов — ruleset обрезан",
            ruleset_text.chars().count()
        )
    } else {
        String::new()
    };

    Ok(NetView {
        rows,
        ruleset_text,
        ruleset_note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_value<'a>(rows: &'a [DetailRow], key: &str) -> &'a str {
        rows.iter()
            .find(|r| r.key == key)
            .unwrap_or_else(|| panic!("нет строки с ключом {key}"))
            .value
            .as_str()
    }

    fn space_row<'a>(rows: &'a [SpaceDetailRow], key: &str) -> &'a SpaceDetailRow {
        rows.iter()
            .find(|r| r.key == key)
            .unwrap_or_else(|| panic!("нет строки с ключом {key}"))
    }

    fn space_row_value<'a>(rows: &'a [SpaceDetailRow], key: &str) -> &'a str {
        space_row(rows, key).value.as_str()
    }

    // --- spaces_view ---

    #[test]
    fn transport_error_becomes_unavailable_with_same_text() {
        let view = spaces_view(Err(
            "демон не отвечает на /run/miyorios/control.sock".to_string()
        ));
        match view {
            SpacesView::Unavailable { message } => {
                assert_eq!(message, "демон не отвечает на /run/miyorios/control.sock");
            }
            SpacesView::Spaces(_) => panic!("транспортная ошибка не должна давать список"),
        }
    }

    #[test]
    fn reply_err_becomes_unavailable_with_code_and_message_verbatim() {
        let view = spaces_view(Ok(Reply::Err {
            code: "internal".to_string(),
            message: "не удалось прочитать реестр".to_string(),
        }));
        match view {
            SpacesView::Unavailable { message } => {
                assert!(message.contains("internal"), "{message}");
                assert!(message.contains("не удалось прочитать реестр"), "{message}");
            }
            SpacesView::Spaces(_) => panic!("отказ демона не должен давать список"),
        }
    }

    #[test]
    fn non_array_reply_becomes_unavailable_with_clear_message() {
        let view = spaces_view(Ok(Reply::Ok(serde_json::json!({"not": "an array"}))));
        match view {
            SpacesView::Unavailable { message } => {
                assert!(!message.is_empty());
            }
            SpacesView::Spaces(_) => panic!("объект вместо массива не должен давать список"),
        }
    }

    #[test]
    fn normal_reply_preserves_daemon_order_and_fields() {
        let reply = Ok(Reply::Ok(serde_json::json!([
            {
                "id": "telegram", "profile": "telegram", "state": "running",
                "cid": 42, "isolation-level": "standard", "isolation-reason": "",
                "label": "Telegram", "color": "#3390ec"
            },
            {
                "id": "kali", "profile": "kali", "state": "stopped",
                "cid": 43, "isolation-level": "reduced", "isolation-reason": "полноэкранная графика",
                "label": "Kali", "color": "#557c94"
            }
        ])));
        match spaces_view(reply) {
            SpacesView::Spaces(rows) => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].id, "telegram");
                assert_eq!(rows[0].label, "Telegram");
                assert_eq!(rows[0].color, "#3390ec");
                assert_eq!(rows[0].profile, "telegram");
                assert_eq!(rows[0].state, "running");
                assert_eq!(rows[0].level, "standard");
                assert_eq!(rows[0].level_reason, "");
                assert_eq!(rows[1].id, "kali");
                assert_eq!(rows[1].level, "reduced");
                assert_eq!(rows[1].level_reason, "полноэкранная графика");
            }
            SpacesView::Unavailable { message } => panic!("ожидался список, получено {message}"),
        }
    }

    // демон может быть старее менеджера — список обязан собраться и без этого поля
    #[test]
    fn space_row_encrypted_field_missing_defaults_to_false() {
        let reply = Ok(Reply::Ok(serde_json::json!([
            {
                "id": "telegram", "profile": "telegram", "state": "running",
                "cid": 42, "isolation-level": "standard", "isolation-reason": "",
                "label": "Telegram", "color": "#3390ec"
            }
        ])));
        match spaces_view(reply) {
            SpacesView::Spaces(rows) => assert!(!rows[0].encrypted),
            SpacesView::Unavailable { message } => panic!("{message}"),
        }
    }

    #[test]
    fn space_row_encrypted_field_true_is_read() {
        let reply = Ok(Reply::Ok(serde_json::json!([
            {
                "id": "kali", "profile": "kali", "state": "stopped",
                "cid": 43, "isolation-level": "standard", "isolation-reason": "",
                "label": "Kali", "color": "#557c94", "encrypted": true
            }
        ])));
        match spaces_view(reply) {
            SpacesView::Spaces(rows) => assert!(rows[0].encrypted),
            SpacesView::Unavailable { message } => panic!("{message}"),
        }
    }

    #[test]
    fn item_missing_required_field_becomes_unavailable_not_a_blank_row() {
        let reply = Ok(Reply::Ok(serde_json::json!([
            {
                "id": "telegram", "profile": "telegram", "state": "running",
                "cid": 42, "isolation-level": "standard", "isolation-reason": ""
                // нет label и color
            }
        ])));
        match spaces_view(reply) {
            SpacesView::Unavailable { message } => assert!(!message.is_empty()),
            SpacesView::Spaces(rows) => panic!(
                "спейс без label не должен попадать в список: {:?}",
                rows.iter().map(|r| &r.id).collect::<Vec<_>>()
            ),
        }
    }

    // --- profiles_view ---

    #[test]
    fn profiles_transport_error_becomes_unavailable_with_same_text() {
        let view = profiles_view(Err("демон не отвечает".to_string()));
        match view {
            ProfilesView::Unavailable { message } => assert_eq!(message, "демон не отвечает"),
            ProfilesView::Profiles(_) => panic!("транспортная ошибка не должна давать список"),
        }
    }

    #[test]
    fn profiles_reply_err_becomes_unavailable_with_code_and_message_verbatim() {
        let view = profiles_view(Ok(Reply::Err {
            code: "internal".to_string(),
            message: "каталог профилей не читается".to_string(),
        }));
        match view {
            ProfilesView::Unavailable { message } => {
                assert!(message.contains("internal"), "{message}");
                assert!(
                    message.contains("каталог профилей не читается"),
                    "{message}"
                );
            }
            ProfilesView::Profiles(_) => panic!("отказ демона не должен давать список"),
        }
    }

    #[test]
    fn profiles_non_array_reply_becomes_unavailable() {
        let view = profiles_view(Ok(Reply::Ok(serde_json::json!({"not": "an array"}))));
        match view {
            ProfilesView::Unavailable { message } => assert!(!message.is_empty()),
            ProfilesView::Profiles(_) => panic!("объект вместо массива не должен давать список"),
        }
    }

    #[test]
    fn profiles_empty_list_is_not_unavailable() {
        let view = profiles_view(Ok(Reply::Ok(serde_json::json!([]))));
        match view {
            ProfilesView::Profiles(rows) => assert!(rows.is_empty()),
            ProfilesView::Unavailable { message } => {
                panic!("пустой список профилей — не ошибка: {message}")
            }
        }
    }

    #[test]
    fn profiles_normal_reply_preserves_order_and_fields_including_broken_manifest() {
        let reply = Ok(Reply::Ok(serde_json::json!([
            {
                "profile": "spike", "template": true, "manifest-ok": true,
                "isolation-level": "standard", "isolation-reason": "",
                "command": "/bin/true", "manifest-error": null
            },
            {
                "profile": "ghost", "template": false, "manifest-ok": false,
                "isolation-level": null, "isolation-reason": null, "command": null,
                "manifest-error": "некорректный TOML манифеста: expected `=`"
            }
        ])));
        match profiles_view(reply) {
            ProfilesView::Profiles(rows) => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].profile, "spike");
                assert!(rows[0].template);
                assert!(rows[0].manifest_ok);
                assert_eq!(rows[0].isolation_level.as_deref(), Some("standard"));
                assert_eq!(rows[0].manifest_error, None);

                assert_eq!(rows[1].profile, "ghost");
                assert!(!rows[1].template);
                assert!(!rows[1].manifest_ok);
                assert_eq!(rows[1].isolation_level, None);
                assert_eq!(
                    rows[1].manifest_error.as_deref(),
                    Some("некорректный TOML манифеста: expected `=`")
                );
            }
            ProfilesView::Unavailable { message } => panic!("ожидался список, получено {message}"),
        }
    }

    #[test]
    fn profiles_item_missing_required_field_becomes_unavailable() {
        let reply = Ok(Reply::Ok(serde_json::json!([
            {
                "profile": "spike", "manifest-ok": true,
                "isolation-level": "standard", "manifest-error": null
                // нет "template"
            }
        ])));
        match profiles_view(reply) {
            ProfilesView::Unavailable { message } => assert!(!message.is_empty()),
            ProfilesView::Profiles(rows) => panic!(
                "профиль без template не должен попадать в список: {:?}",
                rows.iter().map(|r| &r.profile).collect::<Vec<_>>()
            ),
        }
    }

    // --- detail_view ---

    #[test]
    fn detail_transport_error_becomes_unavailable() {
        let view = detail_view(Err("демон не отвечает".to_string()));
        match view {
            DetailView::Unavailable { message } => assert_eq!(message, "демон не отвечает"),
            DetailView::Space { .. } => panic!("транспортная ошибка не должна давать детали"),
        }
    }

    #[test]
    fn detail_reply_err_becomes_unavailable_verbatim() {
        let view = detail_view(Ok(Reply::Err {
            code: "space-not-found".to_string(),
            message: "нет спейса \"x\"".to_string(),
        }));
        match view {
            DetailView::Unavailable { message } => {
                assert!(message.contains("space-not-found"));
                assert!(message.contains("нет спейса \"x\""));
            }
            DetailView::Space { .. } => panic!("отказ демона не должен давать детали"),
        }
    }

    #[test]
    fn detail_non_object_reply_becomes_unavailable() {
        let view = detail_view(Ok(Reply::Ok(serde_json::json!([1, 2, 3]))));
        match view {
            DetailView::Unavailable { message } => assert!(!message.is_empty()),
            DetailView::Space { .. } => panic!("массив вместо объекта не должен давать детали"),
        }
    }

    fn full_describe_reply() -> Value {
        serde_json::json!({
            "id": "kali", "profile": "kali", "state": "running",
            "cid": 43, "uid": 100043, "digest": "abc123",
            "label": "Kali", "color": "#557c94", "created": "2026-08-24T10:00:00Z",
            "description": "Kali Linux — целый рабочий стол",
            "app": {"mode": "session", "command": "/usr/bin/startxfce4"},
            "resources": {"memory-mb": 512, "cpus": 2, "disk-gb": 32, "data-mb": 64},
            "data-qcow2-bytes": 1258291,
            "manifest-path": "/var/lib/miyorios/profiles/kali/manifest.toml",
            "isolation": {"level": "reduced", "gpu": "virtio-gpu-venus", "reason": "полноэкранная графика"},
            "network": {"via": "miyori-net"},
            "clean-shutdown": true,
            "rss-kib": 441344,
            "uptime-secs": 8000,
            "log-tail": ["строка1", "строка2"]
        })
    }

    #[test]
    fn detail_full_reply_renders_expected_rows_in_order() {
        let view = detail_view(Ok(Reply::Ok(full_describe_reply())));
        match view {
            DetailView::Space { rows, log_tail } => {
                let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
                assert_eq!(
                    keys,
                    vec![
                        "Профиль",
                        "Описание",
                        "Состояние",
                        "CID",
                        "uid",
                        "Создан",
                        "Приложение",
                        "Шаблон",
                        "Манифест",
                        "Сеть",
                        "Уровень изоляции",
                        "GPU",
                        "Память",
                        "Том данных",
                        "CPU",
                        "Диск",
                        "Шифрование диска",
                        "Время работы",
                        "Чистый останов",
                    ]
                );
                assert_eq!(space_row_value(&rows, "Профиль"), "kali");
                assert_eq!(space_row_value(&rows, "CID"), "43");
                assert_eq!(space_row_value(&rows, "uid"), "100043");
                assert_eq!(
                    space_row_value(&rows, "Приложение"),
                    "session · /usr/bin/startxfce4"
                );
                assert_eq!(space_row_value(&rows, "Сеть"), "miyori-net");
                assert_eq!(
                    space_row_value(&rows, "Уровень изоляции"),
                    "reduced — полноэкранная графика"
                );
                assert_eq!(space_row_value(&rows, "GPU"), "virtio-gpu-venus");
                assert_eq!(
                    space_row_value(&rows, "Память"),
                    "заказано 512 МиБ · фактически 431 МиБ"
                );
                assert_eq!(
                    space_row_value(&rows, "Том данных"),
                    "заказано 64 МиБ · файл 1.2 МиБ"
                );
                assert_eq!(space_row_value(&rows, "CPU"), "2");
                assert_eq!(space_row_value(&rows, "Диск"), "32 ГиБ");
                assert_eq!(space_row_value(&rows, "Время работы"), "2 ч 13 мин");
                assert_eq!(space_row_value(&rows, "Чистый останов"), "да");
                assert_eq!(log_tail, vec!["строка1".to_string(), "строка2".to_string()]);
            }
            DetailView::Unavailable { message } => panic!("ожидались детали, получено {message}"),
        }
    }

    // демон может быть старее менеджера — карточка обязана собраться и без этого поля
    #[test]
    fn detail_encrypted_field_missing_shows_no() {
        let reply = full_describe_reply();
        match detail_view(Ok(Reply::Ok(reply))) {
            DetailView::Space { rows, .. } => {
                assert_eq!(space_row_value(&rows, "Шифрование диска"), "нет");
            }
            DetailView::Unavailable { message } => panic!("{message}"),
        }
    }

    #[test]
    fn detail_encrypted_field_true_shows_yes() {
        let mut reply = full_describe_reply();
        reply["encrypted"] = serde_json::json!(true);
        match detail_view(Ok(Reply::Ok(reply))) {
            DetailView::Space { rows, .. } => {
                assert_eq!(space_row_value(&rows, "Шифрование диска"), "да");
            }
            DetailView::Unavailable { message } => panic!("{message}"),
        }
    }

    #[test]
    fn detail_rows_carry_section_and_monospace_metadata_for_the_card_layout() {
        let view = detail_view(Ok(Reply::Ok(full_describe_reply())));
        match view {
            DetailView::Space { rows, .. } => {
                assert_eq!(space_row(&rows, "Профиль").section, DetailSection::Space);
                assert_eq!(space_row(&rows, "Описание").section, DetailSection::Space);
                assert_eq!(space_row(&rows, "Состояние").section, DetailSection::Space);
                assert_eq!(
                    space_row(&rows, "Время работы").section,
                    DetailSection::Space
                );
                assert_eq!(
                    space_row(&rows, "Чистый останов").section,
                    DetailSection::Space
                );
                assert!(!space_row(&rows, "Профиль").mono);

                assert_eq!(space_row(&rows, "CID").section, DetailSection::Identity);
                assert!(space_row(&rows, "CID").mono);
                assert_eq!(space_row(&rows, "uid").section, DetailSection::Identity);
                assert!(space_row(&rows, "uid").mono);
                assert_eq!(space_row(&rows, "Создан").section, DetailSection::Identity);
                assert_eq!(
                    space_row(&rows, "Приложение").section,
                    DetailSection::Identity
                );
                assert_eq!(space_row(&rows, "Шаблон").section, DetailSection::Identity);
                assert!(space_row(&rows, "Шаблон").mono);
                assert_eq!(
                    space_row(&rows, "Манифест").section,
                    DetailSection::Identity
                );
                assert!(space_row(&rows, "Манифест").mono);

                assert_eq!(space_row(&rows, "Память").section, DetailSection::Resources);
                assert_eq!(space_row(&rows, "CPU").section, DetailSection::Resources);
                assert_eq!(space_row(&rows, "Диск").section, DetailSection::Resources);
                assert_eq!(
                    space_row(&rows, "Том данных").section,
                    DetailSection::Resources
                );

                assert_eq!(space_row(&rows, "Сеть").section, DetailSection::Network);
                assert_eq!(
                    space_row(&rows, "Уровень изоляции").section,
                    DetailSection::Network
                );
                assert_eq!(space_row(&rows, "GPU").section, DetailSection::Network);
            }
            DetailView::Unavailable { message } => panic!("{message}"),
        }
    }

    #[test]
    fn detail_null_rss_shows_dash_for_actual_memory() {
        let mut reply = full_describe_reply();
        reply["rss-kib"] = Value::Null;
        match detail_view(Ok(Reply::Ok(reply))) {
            DetailView::Space { rows, .. } => {
                assert_eq!(
                    space_row_value(&rows, "Память"),
                    "заказано 512 МиБ · фактически —"
                );
            }
            DetailView::Unavailable { message } => panic!("{message}"),
        }
    }

    #[test]
    fn detail_null_data_qcow2_bytes_shows_dash_for_file() {
        let mut reply = full_describe_reply();
        reply["data-qcow2-bytes"] = Value::Null;
        match detail_view(Ok(Reply::Ok(reply))) {
            DetailView::Space { rows, .. } => {
                assert_eq!(
                    space_row_value(&rows, "Том данных"),
                    "заказано 64 МиБ · файл —"
                );
            }
            DetailView::Unavailable { message } => panic!("{message}"),
        }
    }

    #[test]
    fn detail_uptime_under_a_minute_shows_seconds_only() {
        let mut reply = full_describe_reply();
        reply["uptime-secs"] = serde_json::json!(47);
        match detail_view(Ok(Reply::Ok(reply))) {
            DetailView::Space { rows, .. } => {
                assert_eq!(space_row_value(&rows, "Время работы"), "47 с");
            }
            DetailView::Unavailable { message } => panic!("{message}"),
        }
    }

    #[test]
    fn detail_uptime_null_shows_dash() {
        let mut reply = full_describe_reply();
        reply["uptime-secs"] = Value::Null;
        match detail_view(Ok(Reply::Ok(reply))) {
            DetailView::Space { rows, .. } => {
                assert_eq!(space_row_value(&rows, "Время работы"), "—");
            }
            DetailView::Unavailable { message } => panic!("{message}"),
        }
    }

    #[test]
    fn detail_isolation_standard_has_no_reason_suffix() {
        let mut reply = full_describe_reply();
        reply["isolation"] = serde_json::json!({"level": "standard", "gpu": "none", "reason": ""});
        match detail_view(Ok(Reply::Ok(reply))) {
            DetailView::Space { rows, .. } => {
                assert_eq!(space_row_value(&rows, "Уровень изоляции"), "standard");
            }
            DetailView::Unavailable { message } => panic!("{message}"),
        }
    }

    #[test]
    fn detail_isolation_reduced_with_empty_reason_says_so_instead_of_hiding_it() {
        let mut reply = full_describe_reply();
        reply["isolation"] =
            serde_json::json!({"level": "reduced", "gpu": "virtio-gpu-venus", "reason": ""});
        match detail_view(Ok(Reply::Ok(reply))) {
            DetailView::Space { rows, .. } => {
                assert_eq!(
                    space_row_value(&rows, "Уровень изоляции"),
                    "reduced — причина не указана в манифесте"
                );
            }
            DetailView::Unavailable { message } => panic!("{message}"),
        }
    }

    #[test]
    fn detail_clean_shutdown_false_and_null() {
        let mut reply = full_describe_reply();
        reply["clean-shutdown"] = serde_json::json!(false);
        match detail_view(Ok(Reply::Ok(reply))) {
            DetailView::Space { rows, .. } => {
                assert_eq!(space_row_value(&rows, "Чистый останов"), "нет")
            }
            DetailView::Unavailable { message } => panic!("{message}"),
        }

        let mut reply2 = full_describe_reply();
        reply2["clean-shutdown"] = Value::Null;
        match detail_view(Ok(Reply::Ok(reply2))) {
            DetailView::Space { rows, .. } => {
                assert_eq!(
                    space_row_value(&rows, "Чистый останов"),
                    "ещё не останавливали"
                )
            }
            DetailView::Unavailable { message } => panic!("{message}"),
        }
    }

    #[test]
    fn detail_missing_field_is_dash_not_invented() {
        let reply = serde_json::json!({"id": "kali"});
        match detail_view(Ok(Reply::Ok(reply))) {
            DetailView::Space { rows, log_tail } => {
                assert_eq!(space_row_value(&rows, "Профиль"), "—");
                assert_eq!(space_row_value(&rows, "CID"), "—");
                assert_eq!(space_row_value(&rows, "uid"), "—");
                assert_eq!(
                    space_row_value(&rows, "Память"),
                    "заказано — · фактически —"
                );
                assert!(log_tail.is_empty());
            }
            DetailView::Unavailable { message } => panic!("{message}"),
        }
    }

    #[test]
    fn detail_log_tail_is_passed_through_unchanged() {
        let mut reply = full_describe_reply();
        reply["log-tail"] = serde_json::json!(["  строка с пробелами  ", "вторая"]);
        match detail_view(Ok(Reply::Ok(reply))) {
            DetailView::Space { log_tail, .. } => {
                assert_eq!(
                    log_tail,
                    vec!["  строка с пробелами  ".to_string(), "вторая".to_string()]
                );
            }
            DetailView::Unavailable { message } => panic!("{message}"),
        }
    }

    // --- net_view ---

    #[test]
    fn net_transport_error_returns_err() {
        let err = net_view(Err("демон не отвечает".to_string())).unwrap_err();
        assert_eq!(err, "демон не отвечает");
    }

    #[test]
    fn net_reply_err_returns_err_verbatim() {
        let err = net_view(Ok(Reply::Err {
            code: "internal".to_string(),
            message: "не удалось прочитать nft".to_string(),
        }))
        .unwrap_err();
        assert!(err.contains("internal"));
        assert!(err.contains("не удалось прочитать nft"));
    }

    #[test]
    fn net_non_object_reply_returns_err() {
        let err = net_view(Ok(Reply::Ok(serde_json::json!([1, 2])))).unwrap_err();
        assert!(!err.is_empty());
    }

    fn full_net_status_reply() -> Value {
        serde_json::json!({
            "bridges": {"br-spaces": true, "br-captive": false},
            "taps": [{"name": "tap-telegram", "isolated": true}, {"name": "tap-kali", "isolated": false}],
            "uplink-pci": {"address": "0000:05:00.0", "present": true, "driver": "vfio-pci"},
            "miyori-net": {"running": true, "pid": 1234, "uplink": "vfio", "tunnel": {"ifc": "tun0", "set": true, "route": true}},
            "ruleset": {"available": true, "text": "table inet filter { ... }", "truncated-from": null},
            "killswitch_counters": "недоступны с хоста"
        })
    }

    #[test]
    fn net_full_reply_carries_data_verbatim() {
        let view = net_view(Ok(Reply::Ok(full_net_status_reply()))).unwrap();
        assert_eq!(
            row_value(&view.rows, "Счётчики kill-switch"),
            "недоступны с хоста"
        );
        assert_eq!(view.ruleset_text, "table inet filter { ... }");
        assert_eq!(view.ruleset_note, "");
    }

    #[test]
    fn net_ruleset_unavailable_note_says_so_and_text_carries_reason() {
        let mut reply = full_net_status_reply();
        reply["ruleset"] = serde_json::json!({"available": false, "text": "nft недоступен для чтения", "truncated-from": null});
        let view = net_view(Ok(Reply::Ok(reply))).unwrap();
        assert!(!view.ruleset_note.is_empty());
        assert_eq!(view.ruleset_text, "nft недоступен для чтения");
    }

    #[test]
    fn net_ruleset_truncated_note_reports_shown_and_total() {
        let mut reply = full_net_status_reply();
        reply["ruleset"] =
            serde_json::json!({"available": true, "text": "abcde", "truncated-from": 500});
        let view = net_view(Ok(Reply::Ok(reply))).unwrap();
        assert!(view.ruleset_note.contains('5'), "{}", view.ruleset_note);
        assert!(view.ruleset_note.contains("500"), "{}", view.ruleset_note);
    }
}
