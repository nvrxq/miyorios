use crate::font_data;

type Bitmap = [u8; font_data::GLYPH_HEIGHT * font_data::ROW_BYTES];

// '?' стоит в таблице специально как видимый заменитель неизвестного символа
const REPLACEMENT: char = '?';
// три точки, а не многоточие: U+2026 в наборе глифов нет, а рисовать заменитель вместо признака обрезки нельзя
const ELLIPSIS: &str = "...";

pub fn glyph(ch: char) -> Option<&'static Bitmap> {
    let code = ch as u32;
    font_data::GLYPHS
        .binary_search_by_key(&code, |&(c, _)| c)
        .ok()
        .map(|i| &font_data::GLYPHS[i].1)
}

pub fn text_width_px(text: &str) -> usize {
    font_data::GLYPH_WIDTH * text.chars().count()
}

// обрезанный текст обязан выглядеть обрезанным: молча укоротить сообщение об ошибке — соврать о нём
pub fn fit_text(text: &str, max_px: usize) -> String {
    if text_width_px(text) <= max_px {
        return text.to_string();
    }
    let fits = max_px / font_data::GLYPH_WIDTH;
    if fits <= ELLIPSIS.len() {
        return text.chars().take(fits).collect();
    }
    let mut out: String = text.chars().take(fits - ELLIPSIS.len()).collect();
    out.push_str(ELLIPSIS);
    out
}

// пропущенный символ сдвинул бы всю строку — рисуем заменитель, а не дырку
fn glyph_or_replacement(ch: char) -> &'static Bitmap {
    glyph(ch).unwrap_or_else(|| glyph(REPLACEMENT).expect("'?' обязан быть в таблице глифов"))
}

fn draw_glyph(
    buf: &mut [u32],
    buf_width: usize,
    buf_height: usize,
    x: usize,
    y: usize,
    bitmap: &Bitmap,
    fg: u32,
) {
    for row in 0..font_data::GLYPH_HEIGHT {
        let py = y + row;
        if py >= buf_height {
            break;
        }
        let row_bytes = &bitmap[row * font_data::ROW_BYTES..(row + 1) * font_data::ROW_BYTES];
        for col in 0..font_data::GLYPH_WIDTH {
            let px = x + col;
            if px >= buf_width {
                break; // дальше по строке будет только хуже — не заворачиваем на следующую
            }
            let bit = (row_bytes[col / 8] >> (7 - col % 8)) & 1;
            if bit == 1 {
                buf[py * buf_width + px] = fg;
            }
        }
    }
}

pub fn draw_text(
    buf: &mut [u32],
    buf_width: usize,
    buf_height: usize,
    x: usize,
    y: usize,
    text: &str,
    fg: u32,
) {
    let mut cursor_x = x;
    for ch in text.chars() {
        draw_glyph(
            buf,
            buf_width,
            buf_height,
            cursor_x,
            y,
            glyph_or_replacement(ch),
            fg,
        );
        cursor_x += font_data::GLYPH_WIDTH;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_a_found() {
        assert!(glyph('A').is_some());
    }

    #[test]
    fn glyph_zhe_found() {
        assert!(glyph('Ж').is_some());
    }

    #[test]
    fn unknown_char_uses_replacement_without_changing_width() {
        assert!(glyph('\u{1F642}').is_none());
        assert_eq!(text_width_px("A"), text_width_px("\u{1F642}"));
    }

    #[test]
    fn fit_text_keeps_short_text_as_is() {
        assert_eq!(fit_text("спейс spike", 1000), "спейс spike");
    }

    #[test]
    fn fit_text_marks_the_cut() {
        let long = "a".repeat(100);
        let cut = fit_text(&long, font_data::GLYPH_WIDTH * 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with("..."), "{cut}");
    }

    #[test]
    fn fit_text_on_a_hopeless_width_does_not_panic() {
        assert_eq!(
            fit_text("abcdef", font_data::GLYPH_WIDTH * 2)
                .chars()
                .count(),
            2
        );
        assert_eq!(fit_text("abcdef", 0), "");
    }

    #[test]
    fn draw_text_out_of_bounds_does_not_panic() {
        let width = 20;
        let height = 20;
        let mut buf = vec![0u32; width * height];
        draw_text(
            &mut buf,
            width,
            height,
            width - 2,
            height - 2,
            "AAAA",
            0xffff_ffff,
        );
    }

    #[test]
    fn drawn_glyph_lights_expected_rectangle_and_nothing_outside() {
        let width = font_data::GLYPH_WIDTH + 4;
        let height = font_data::GLYPH_HEIGHT + 4;
        let mut buf = vec![0u32; width * height];
        draw_text(&mut buf, width, height, 2, 2, "A", 0xffff_ffff);

        let bitmap = glyph('A').unwrap();
        for row in 0..font_data::GLYPH_HEIGHT {
            for col in 0..font_data::GLYPH_WIDTH {
                let byte = bitmap[row * font_data::ROW_BYTES + col / 8];
                let bit = (byte >> (7 - col % 8)) & 1;
                let pixel = buf[(2 + row) * width + (2 + col)];
                let expected = if bit == 1 { 0xffff_ffff } else { 0 };
                assert_eq!(pixel, expected, "row {row} col {col}");
            }
        }
        // угол буфера вне прямоугольника глифа не тронут
        assert_eq!(buf[0], 0);
        assert_eq!(buf[width * height - 1], 0);
    }

    #[test]
    fn text_beyond_buffer_width_is_clipped_not_wrapped() {
        let width = 10;
        let height = font_data::GLYPH_HEIGHT;
        let mut buf = vec![0u32; width * height];
        // x == width: глиф целиком за краем; если бы индекс завернулся на строку 0, буфер бы не остался пустым
        draw_text(&mut buf, width, height, width, 0, "A", 0xffff_ffff);
        assert!(buf.iter().all(|&p| p == 0));
    }

    #[test]
    fn overlong_text_is_clipped_without_panicking() {
        let width = font_data::GLYPH_WIDTH;
        let height = font_data::GLYPH_HEIGHT;
        let mut buf = vec![0u32; width * height];
        draw_text(&mut buf, width, height, 0, 0, "AAAAAA", 0xffff_ffff);
        assert!(buf.iter().any(|&p| p != 0));
    }
}
