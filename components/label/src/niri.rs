use anyhow::{bail, Context, Result};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub id: u64,
    pub title: String,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    // второе поле — id сфокусированного окна из этого же снимка, если такое есть
    WindowsChanged(Vec<Window>, Option<u64>),
    // второе поле — is_focused из niri: снимок одного окна не отвечает за фокус остальных
    WindowOpenedOrChanged(Window, bool),
    WindowClosed(u64),
    WindowFocusChanged(Option<u64>),
    Other,
}

fn window_is_focused(value: &serde_json::Value) -> bool {
    value
        .get("is_focused")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn window_from_value(value: &serde_json::Value) -> Result<Window> {
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .context("окно без числового id")?;
    let title = value
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let pid = match value.get("pid") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .context("поле pid не является числом")?
                .try_into()
                .context("pid не помещается в u32")?,
        ),
    };
    Ok(Window { id, title, pid })
}

pub fn parse_event(line: &str) -> Result<Event> {
    let value: serde_json::Value =
        serde_json::from_str(line).context("строка события не является JSON")?;

    if let Some(payload) = value.get("WindowsChanged") {
        let raw_windows = payload
            .get("windows")
            .and_then(serde_json::Value::as_array)
            .context("WindowsChanged без массива windows")?;
        let windows = raw_windows
            .iter()
            .map(window_from_value)
            .collect::<Result<Vec<_>>>()?;
        let focused = raw_windows
            .iter()
            .zip(windows.iter())
            .find(|(raw, _)| window_is_focused(raw))
            .map(|(_, window)| window.id);
        return Ok(Event::WindowsChanged(windows, focused));
    }

    if let Some(payload) = value.get("WindowOpenedOrChanged") {
        let raw_window = payload
            .get("window")
            .context("WindowOpenedOrChanged без window")?;
        let is_focused = window_is_focused(raw_window);
        return Ok(Event::WindowOpenedOrChanged(
            window_from_value(raw_window)?,
            is_focused,
        ));
    }

    if let Some(payload) = value.get("WindowClosed") {
        let id = payload
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .context("WindowClosed без числового id")?;
        return Ok(Event::WindowClosed(id));
    }

    if let Some(payload) = value.get("WindowFocusChanged") {
        match payload.get("id") {
            None => bail!("WindowFocusChanged без поля id"),
            Some(serde_json::Value::Null) => return Ok(Event::WindowFocusChanged(None)),
            Some(v) => {
                let id = v
                    .as_u64()
                    .context("поле id в WindowFocusChanged не является числом")?;
                return Ok(Event::WindowFocusChanged(Some(id)));
            }
        }
    }

    Ok(Event::Other)
}

// "неизвестно" и "фокуса нет" — разные вещи: первое нельзя показывать полосой как хост
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus<'a> {
    Unknown,
    None,
    Window(&'a Window),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FocusId {
    #[default]
    Unknown,
    None,
    Window(u64),
}

#[derive(Default)]
pub struct WindowMap {
    windows: HashMap<u64, Window>,
    focus: FocusId,
}

impl WindowMap {
    pub fn apply(&mut self, event: Event) {
        match event {
            // снимок авторитетен целиком: раз в нём никто не сфокусирован — фокуса нет, а не "неизвестно"
            Event::WindowsChanged(windows, focused) => {
                self.windows = windows.into_iter().map(|w| (w.id, w)).collect();
                self.focus = match focused {
                    Some(id) => FocusId::Window(id),
                    None => FocusId::None,
                };
            }
            // is_focused=false здесь не значит "фокуса нет нигде" — событие отвечает только за это окно
            Event::WindowOpenedOrChanged(window, is_focused) => {
                let id = window.id;
                self.windows.insert(id, window);
                if is_focused {
                    self.focus = FocusId::Window(id);
                }
            }
            Event::WindowClosed(id) => {
                self.windows.remove(&id);
                if self.focus == FocusId::Window(id) {
                    self.focus = FocusId::None;
                }
            }
            Event::WindowFocusChanged(id) => {
                self.focus = match id {
                    Some(id) => FocusId::Window(id),
                    None => FocusId::None,
                };
            }
            Event::Other => {}
        }
    }

    // только для тестов: полосе (main.rs, strip.rs) довольно focus(), а мёртвого кода в бинаре быть не должно
    #[cfg(test)]
    pub fn get(&self, id: u64) -> Option<&Window> {
        self.windows.get(&id)
    }

    pub fn focus(&self) -> Focus<'_> {
        match self.focus {
            FocusId::Unknown => Focus::Unknown,
            FocusId::None => Focus::None,
            FocusId::Window(id) => match self.windows.get(&id) {
                Some(window) => Focus::Window(window),
                None => Focus::None,
            },
        }
    }

    #[cfg(test)]
    pub fn focused(&self) -> Option<&Window> {
        match self.focus() {
            Focus::Window(window) => Some(window),
            Focus::None | Focus::Unknown => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_windows_changed() {
        let line = r#"{"WindowsChanged":{"windows":[{"id":1,"title":"a","app_id":"x","pid":100,"workspace_id":1,"is_focused":false}]}}"#;
        let event = parse_event(line).unwrap();
        assert_eq!(
            event,
            Event::WindowsChanged(
                vec![Window {
                    id: 1,
                    title: "a".into(),
                    pid: Some(100)
                }],
                None
            )
        );
    }

    #[test]
    fn parses_windows_changed_reports_the_focused_window_id() {
        let line = r#"{"WindowsChanged":{"windows":[
            {"id":1,"title":"a","app_id":"x","pid":100,"workspace_id":1,"is_focused":false},
            {"id":2,"title":"b","app_id":"y","pid":200,"workspace_id":1,"is_focused":true}
        ]}}"#;
        let event = parse_event(line).unwrap();
        assert_eq!(
            event,
            Event::WindowsChanged(
                vec![
                    Window {
                        id: 1,
                        title: "a".into(),
                        pid: Some(100)
                    },
                    Window {
                        id: 2,
                        title: "b".into(),
                        pid: Some(200)
                    }
                ],
                Some(2)
            )
        );
    }

    #[test]
    fn parses_window_opened_or_changed() {
        let line = r#"{"WindowOpenedOrChanged":{"window":{"id":47,"title":"SomeF","app_id":"org.wezfurlong.wezterm","pid":187653,"workspace_id":9,"is_focused":true}}}"#;
        let event = parse_event(line).unwrap();
        assert_eq!(
            event,
            Event::WindowOpenedOrChanged(
                Window {
                    id: 47,
                    title: "SomeF".into(),
                    pid: Some(187653)
                },
                true
            )
        );
    }

    #[test]
    fn parses_window_closed() {
        let line = r#"{"WindowClosed":{"id":47}}"#;
        assert_eq!(parse_event(line).unwrap(), Event::WindowClosed(47));
    }

    #[test]
    fn parses_window_focus_changed_some() {
        let line = r#"{"WindowFocusChanged":{"id":47}}"#;
        assert_eq!(
            parse_event(line).unwrap(),
            Event::WindowFocusChanged(Some(47))
        );
    }

    #[test]
    fn parses_window_focus_changed_null() {
        let line = r#"{"WindowFocusChanged":{"id":null}}"#;
        assert_eq!(parse_event(line).unwrap(), Event::WindowFocusChanged(None));
    }

    #[test]
    fn unknown_event_is_other_not_error() {
        let line = r#"{"WorkspacesChanged":{"workspaces":[]}}"#;
        assert_eq!(parse_event(line).unwrap(), Event::Other);
    }

    #[test]
    fn window_with_null_pid() {
        let line = r#"{"WindowOpenedOrChanged":{"window":{"id":1,"title":"a","app_id":"x","pid":null,"workspace_id":1,"is_focused":false}}}"#;
        let event = parse_event(line).unwrap();
        assert_eq!(
            event,
            Event::WindowOpenedOrChanged(
                Window {
                    id: 1,
                    title: "a".into(),
                    pid: None
                },
                false
            )
        );
    }

    #[test]
    fn non_json_line_is_error() {
        assert!(parse_event("not json at all").is_err());
    }

    #[test]
    fn windows_changed_without_array_is_error() {
        let line = r#"{"WindowsChanged":{"windows":"nope"}}"#;
        assert!(parse_event(line).is_err());
    }

    #[test]
    fn window_closed_without_id_is_error() {
        let line = r#"{"WindowClosed":{}}"#;
        assert!(parse_event(line).is_err());
    }

    #[test]
    fn window_opened_without_id_is_error() {
        let line = r#"{"WindowOpenedOrChanged":{"window":{"title":"a"}}}"#;
        assert!(parse_event(line).is_err());
    }

    fn win(id: u64, title: &str, pid: Option<u32>) -> Window {
        Window {
            id,
            title: title.into(),
            pid,
        }
    }

    #[test]
    fn map_windows_changed_replaces_entirely() {
        let mut map = WindowMap::default();
        map.apply(Event::WindowsChanged(vec![win(1, "a", Some(10))], Some(1)));
        map.apply(Event::WindowsChanged(vec![win(2, "b", Some(20))], Some(2)));
        assert!(map.get(1).is_none());
        assert_eq!(map.get(2), Some(&win(2, "b", Some(20))));
    }

    #[test]
    fn map_window_closed_removes_and_clears_focus() {
        let mut map = WindowMap::default();
        map.apply(Event::WindowsChanged(vec![win(1, "a", Some(10))], None));
        map.apply(Event::WindowFocusChanged(Some(1)));
        assert_eq!(map.focused(), Some(&win(1, "a", Some(10))));

        map.apply(Event::WindowClosed(1));
        assert!(map.get(1).is_none());
        assert!(map.focused().is_none());
    }

    #[test]
    fn map_focus_changed_moves_focus() {
        let mut map = WindowMap::default();
        map.apply(Event::WindowsChanged(vec![win(1, "a", Some(10))], None));
        map.apply(Event::WindowFocusChanged(Some(1)));
        assert_eq!(map.focused().map(|w| w.id), Some(1));
    }

    // полоса, продолжающая называть закрытое окно, — ровно то враньё, ради которого её и пишут
    #[test]
    fn map_focus_lost_on_close_leaves_no_stale_window() {
        let mut map = WindowMap::default();
        map.apply(Event::WindowsChanged(vec![win(1, "a", Some(10))], None));
        map.apply(Event::WindowFocusChanged(Some(1)));
        map.apply(Event::WindowClosed(1));
        assert!(map.focused().is_none());
    }

    #[test]
    fn map_windows_changed_clears_focus_if_focused_window_gone() {
        let mut map = WindowMap::default();
        map.apply(Event::WindowsChanged(vec![win(1, "a", Some(10))], None));
        map.apply(Event::WindowFocusChanged(Some(1)));
        map.apply(Event::WindowsChanged(vec![win(2, "b", Some(20))], None));
        assert!(map.focused().is_none());
    }

    // воспроизведение дефекта: полоса запущена, когда окно спейса уже в фокусе,
    // и WindowFocusChanged больше не придёт — фокус узнаётся только из снимка
    #[test]
    fn snapshot_with_focused_window_sets_focus_without_focus_changed_event() {
        let mut map = WindowMap::default();
        let line = r#"{"WindowsChanged":{"windows":[{"id":1,"title":"a","app_id":"x","pid":100,"workspace_id":1,"is_focused":true}]}}"#;
        map.apply(parse_event(line).unwrap());
        assert_eq!(map.focused().map(|w| w.id), Some(1));
    }

    // до единого события фокус не наблюдался, а не "точно отсутствует"
    #[test]
    fn before_any_event_focus_is_unknown_not_absent() {
        let map = WindowMap::default();
        assert_eq!(map.focus(), Focus::Unknown);
    }

    // niri явно сообщает "фокуса нет" (id: null) — это отличается от того, что мы просто ещё не знаем
    #[test]
    fn focus_changed_to_null_means_no_focus_not_unknown() {
        let mut map = WindowMap::default();
        map.apply(Event::WindowFocusChanged(None));
        assert_eq!(map.focus(), Focus::None);
    }
}
