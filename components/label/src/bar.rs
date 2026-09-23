use crate::strip::Strip;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarContent {
    pub text: String,
    pub background: u32,
    pub foreground: u32,
}

const HOST_BACKGROUND: u32 = 0xff20_2020;
// у аномалии свой цвет: осиротевшее окно, неотличимое взглядом от хоста, ничего не сообщает
const ANOMALY_BACKGROUND: u32 = 0xffd7_8f00;
// не чёрный: молчаливый чёрный неотличим от "цвет не разобрали", сигнал должен быть заметным
const FALLBACK_COLOR: u32 = 0xffff_00ff;

fn parse_color(s: &str) -> Option<u32> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let rgb = u32::from_str_radix(hex, 16).ok()?;
    Some(0xff00_0000 | rgb)
}

fn brightness(color: u32) -> u32 {
    let r = (color >> 16) & 0xff;
    let g = (color >> 8) & 0xff;
    let b = color & 0xff;
    (299 * r + 587 * g + 114 * b) / 1000
}

fn foreground_for_background(background: u32) -> u32 {
    if brightness(background) > 128 {
        0xff00_0000
    } else {
        0xffff_ffff
    }
}

fn on(background: u32, text: String) -> BarContent {
    BarContent {
        text,
        background,
        foreground: foreground_for_background(background),
    }
}

pub fn bar_content(strip: &Strip) -> BarContent {
    match strip {
        Strip::Host => on(HOST_BACKGROUND, "хост".to_string()),
        Strip::Space { id, color, level } => {
            let background = parse_color(color).unwrap_or(FALLBACK_COLOR);
            BarContent {
                text: format!("спейс {id}: {level}"),
                background,
                foreground: foreground_for_background(background),
            }
        }
        Strip::Orphan { id } => on(ANOMALY_BACKGROUND, format!("окно без спейса: {id}")),
        Strip::Unknown { id } => on(ANOMALY_BACKGROUND, format!("спейс {id} демону неизвестен")),
        Strip::Unverified { claimed } => on(
            ANOMALY_BACKGROUND,
            format!("окно заявляет спейс {claimed}, проверить нечем"),
        ),
        Strip::FocusUnknown => on(ANOMALY_BACKGROUND, "фокус ещё не определён".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // взгляд на полосу должен отличать аномалию от обычного хоста, не читая текста
    #[test]
    fn anomaly_states_do_not_look_like_host() {
        for strip in [
            Strip::Orphan { id: "spike".into() },
            Strip::Unknown { id: "ghost".into() },
            Strip::Unverified {
                claimed: "spike".into(),
            },
            Strip::FocusUnknown,
        ] {
            let content = bar_content(&strip);
            assert_ne!(
                content.background, HOST_BACKGROUND,
                "аномалия {strip:?} окрашена как хост"
            );
            assert_eq!(content.background, ANOMALY_BACKGROUND);
        }
    }

    #[test]
    fn host_is_neutral_dark_with_light_text() {
        let content = bar_content(&Strip::Host);
        assert_eq!(content.text, "хост");
        assert_eq!(content.background, HOST_BACKGROUND);
        assert_eq!(content.foreground, 0xffff_ffff);
    }

    #[test]
    fn space_uses_its_own_color_and_names_id_and_level() {
        let strip = Strip::Space {
            id: "spike".into(),
            color: "#ff0000".into(),
            level: "strict".into(),
        };
        let content = bar_content(&strip);
        assert_eq!(content.text, "спейс spike: strict");
        assert_eq!(content.background, 0xffff_0000);
    }

    #[test]
    fn unparseable_color_gets_visible_fallback_but_keeps_text() {
        let strip = Strip::Space {
            id: "spike".into(),
            color: "not-a-color".into(),
            level: "strict".into(),
        };
        let content = bar_content(&strip);
        assert_eq!(content.background, FALLBACK_COLOR);
        assert_eq!(content.text, "спейс spike: strict");
    }

    #[test]
    fn dark_background_gets_light_text() {
        let strip = Strip::Space {
            id: "spike".into(),
            color: "#101010".into(),
            level: "strict".into(),
        };
        assert_eq!(bar_content(&strip).foreground, 0xffff_ffff);
    }

    #[test]
    fn light_background_gets_dark_text() {
        let strip = Strip::Space {
            id: "spike".into(),
            color: "#eeeeee".into(),
            level: "strict".into(),
        };
        assert_eq!(bar_content(&strip).foreground, 0xff00_0000);
    }

    #[test]
    fn orphan_names_the_window_not_a_space() {
        let content = bar_content(&Strip::Orphan { id: "spike".into() });
        assert_eq!(content.text, "окно без спейса: spike");
    }

    #[test]
    fn unknown_space_is_named_as_unknown_to_daemon() {
        let content = bar_content(&Strip::Unknown { id: "ghost".into() });
        assert_eq!(content.text, "спейс ghost демону неизвестен");
    }

    #[test]
    fn unverified_claim_does_not_assert_a_space() {
        let content = bar_content(&Strip::Unverified {
            claimed: "banking".into(),
        });
        assert_eq!(content.text, "окно заявляет спейс banking, проверить нечем");
    }

    // шрифт полосы — 187 глифов, и всё, чего в нём нет, рисуется знаком вопроса:
    // индикатор доверия, показывающий "?", не сообщает ничего
    #[test]
    fn every_bar_text_is_renderable_by_the_font() {
        let strips = [
            Strip::Host,
            Strip::Space {
                id: "web".into(),
                color: "#1971c2".into(),
                level: "standard".into(),
            },
            Strip::Space {
                id: "spike-reduced".into(),
                color: "#9c36b5".into(),
                level: "reduced".into(),
            },
            Strip::Orphan { id: "web".into() },
            Strip::Unknown { id: "web".into() },
            Strip::Unverified {
                claimed: "web".into(),
            },
            Strip::FocusUnknown,
        ];
        for strip in strips {
            let text = bar_content(&strip).text;
            for ch in text.chars() {
                assert!(
                    crate::font::glyph(ch).is_some(),
                    "в шрифте нет {ch:?} (U+{:04X}) из строки {text:?}",
                    ch as u32
                );
            }
        }
    }

    #[test]
    fn focus_unknown_does_not_claim_host_or_a_space() {
        let content = bar_content(&Strip::FocusUnknown);
        assert_eq!(content.text, "фокус ещё не определён");
    }
}
