#![forbid(unsafe_code)]

use crate::view::{DetailSection, SpaceDetailRow};
use gtk4::prelude::*;
use std::collections::HashMap;

pub fn label(text: &str, class: &str) -> gtk4::Label {
    let label = gtk4::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class(class);
    label
}

pub fn button(text: &str, icon: &str, primary: bool) -> gtk4::Button {
    let button = gtk4::Button::new();
    let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    content.set_halign(gtk4::Align::Center);
    if !icon.is_empty() {
        content.append(&gtk4::Image::from_icon_name(icon));
    }
    content.append(&gtk4::Label::new(Some(text)));
    button.set_child(Some(&content));
    button.add_css_class("action-button");
    if primary {
        button.add_css_class("suggested-action");
    }
    button
}

pub fn scroll(child: &impl IsA<gtk4::Widget>) -> gtk4::ScrolledWindow {
    let scroll = gtk4::ScrolledWindow::new();
    scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scroll.set_child(Some(child));
    scroll.set_vexpand(true);
    scroll
}

pub fn text_view() -> gtk4::TextView {
    let text = gtk4::TextView::builder()
        .monospace(true)
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk4::WrapMode::WordChar)
        .build();
    text.add_css_class("log-view");
    text
}

pub fn state_text(state: &str) -> &str {
    match state {
        "running" => "Работает",
        "stopped" => "Остановлен",
        "unresponsive" => "Не отвечает",
        other => other,
    }
}

pub fn space_icon(profile: &str) -> &'static str {
    match profile {
        "tg" | "telegram" => "mail-send-symbolic",
        "web" | "browser" => "web-browser-symbolic",
        "dev" | "kali" => "utilities-terminal-symbolic",
        _ => "application-x-executable-symbolic",
    }
}

pub struct Details {
    pub root: gtk4::Box,
    pub log: gtk4::TextView,
    pub stack: gtk4::Stack,
    pub title: gtk4::Label,
    pub subtitle: gtk4::Label,
    pub state: gtk4::Label,
    pub level: gtk4::Label,
    pub identity: gtk4::Label,
    pub icon: gtk4::Image,
    values: HashMap<String, gtk4::Label>,
    stats: [(gtk4::Label, gtk4::Label); 3],
}

impl Details {
    pub fn new(rows: &[SpaceDetailRow]) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 20);
        let stack = gtk4::Stack::new();
        stack.set_vhomogeneous(false);
        stack.set_hhomogeneous(false);
        let switcher = gtk4::StackSwitcher::new();
        switcher.set_stack(Some(&stack));
        switcher.set_halign(gtk4::Align::Start);
        switcher.add_css_class("detail-tabs");
        root.append(&switcher);
        root.append(&stack);
        let overview = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
        let cards = gtk4::FlowBox::new();
        cards.set_selection_mode(gtk4::SelectionMode::None);
        cards.set_homogeneous(true);
        cards.set_min_children_per_line(1);
        cards.set_max_children_per_line(3);
        cards.set_column_spacing(12);
        cards.set_row_spacing(12);
        let stats = [
            ("Память", "drive-harddisk-symbolic"),
            ("Том данных", "drive-harddisk-symbolic"),
            ("Время работы", "preferences-system-time-symbolic"),
        ]
        .map(|(caption, icon)| {
            let card = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
            card.add_css_class("metric-card");
            let heading = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            heading.append(&gtk4::Image::from_icon_name(icon));
            heading.append(&label(caption, "muted"));
            card.append(&heading);
            let value = label("—", "metric-value");
            let note = label("", "muted");
            note.set_wrap(true);
            card.append(&value);
            card.append(&note);
            cards.insert(&card, -1);
            (value, note)
        });
        overview.append(&cards);
        let mut values = HashMap::new();
        for (section, heading, parent) in [
            (DetailSection::Network, "Сеть и изоляция", &overview),
            (DetailSection::Space, "О спейсе", &overview),
        ] {
            let panel = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
            panel.add_css_class("info-card");
            panel.append(&label(heading, "section-title"));
            panel.append(&detail_grid(rows, section, &mut values));
            parent.append(&panel);
        }
        let technical = gtk4::Expander::new(Some("Технические сведения"));
        technical.add_css_class("technical");
        technical.set_child(Some(&detail_grid(
            rows,
            DetailSection::Identity,
            &mut values,
        )));
        overview.append(&technical);
        let resources = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
        resources.add_css_class("info-card");
        resources.append(&label("Ресурсы и хранилище", "section-title"));
        resources.append(&detail_grid(rows, DetailSection::Resources, &mut values));
        let log = text_view();
        let log_scroll = scroll(&log);
        log_scroll.set_min_content_height(280);
        stack.add_titled(&overview, Some("overview"), "Обзор");
        stack.add_titled(&resources, Some("resources"), "Ресурсы");
        stack.add_titled(&log_scroll, Some("log"), "Журнал");
        Self {
            root,
            log,
            stack,
            title: label("", "space-title"),
            subtitle: label("", "muted"),
            state: label("", "state-badge"),
            level: label("", "badge"),
            identity: label("", "badge"),
            icon: gtk4::Image::new(),
            values,
            stats,
        }
    }

    pub fn update(&self, rows: &[SpaceDetailRow]) {
        for row in rows {
            if let Some(value) = self.values.get(&row.key) {
                let text = if row.key == "Состояние" {
                    state_text(&row.value)
                } else {
                    &row.value
                };
                if value.text() != text {
                    value.set_text(text);
                }
                value.set_tooltip_text(Some(text));
                if row.key == "Уровень изоляции" {
                    if row.value.starts_with("reduced") {
                        value.add_css_class("warning-text");
                    } else {
                        value.remove_css_class("warning-text");
                    }
                }
            }
        }
        let get = |key: &str| {
            rows.iter()
                .find(|r| r.key == key)
                .map(|r| r.value.as_str())
                .unwrap_or("—")
        };
        for (i, (key, separator)) in [("Память", " · фактически "), ("Том данных", " · файл ")]
            .iter()
            .enumerate()
        {
            let raw = get(key);
            let (note, value) = raw.split_once(separator).unwrap_or(("", raw));
            self.stats[i].0.set_text(value);
            self.stats[i].1.set_text(note);
        }
        self.stats[2].0.set_text(get("Время работы"));
        self.stats[2].1.set_text("С момента запуска");
    }
}

fn detail_grid(
    rows: &[SpaceDetailRow],
    section: DetailSection,
    values: &mut HashMap<String, gtk4::Label>,
) -> gtk4::Grid {
    let grid = gtk4::Grid::new();
    grid.set_column_spacing(24);
    grid.set_row_spacing(12);
    for (index, row) in rows.iter().filter(|r| r.section == section).enumerate() {
        let key = label(&row.key, "detail-key");
        key.set_valign(gtk4::Align::Start);
        let value = label("—", "detail-value");
        value.set_selectable(true);
        value.set_hexpand(true);
        value.set_wrap(true);
        value.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        value.set_max_width_chars(50);
        if row.mono {
            value.add_css_class("monospace");
        }
        grid.attach(&key, 0, index as i32, 1, 1);
        grid.attach(&value, 1, index as i32, 1, 1);
        values.insert(row.key.clone(), value);
    }
    grid
}
