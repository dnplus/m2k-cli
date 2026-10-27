use unicode_width::UnicodeWidthStr;

pub fn row(cells: &[(&str, usize)]) -> String {
    cells
        .iter()
        .map(|(text, width)| cell(text, *width))
        .collect::<Vec<_>>()
        .join(" ")
}

fn cell(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let text = single_line(text);
    let mut rendered = if text.width() <= width {
        text
    } else {
        let content_width = width - 1;
        let mut end = 0;
        for (index, character) in text.char_indices() {
            let candidate = index + character.len_utf8();
            if text[..candidate].width() > content_width {
                break;
            }
            end = candidate;
        }
        format!("{}…", &text[..end])
    };
    rendered.extend(std::iter::repeat_n(' ', width - rendered.width()));
    rendered
}

#[derive(Clone, Copy)]
enum Escape {
    None,
    Start,
    Intermediate,
    Csi,
    String,
    StringEnd,
}

fn single_line(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut escape = Escape::None;
    let mut pending_space = false;
    for character in text.chars() {
        match escape {
            Escape::Start | Escape::Intermediate => {
                escape = match character {
                    '[' if matches!(escape, Escape::Start) => Escape::Csi,
                    ']' | 'P' | 'X' | '^' | '_' if matches!(escape, Escape::Start) => {
                        Escape::String
                    }
                    '\u{20}'..='\u{2f}' => Escape::Intermediate,
                    '\u{1b}' => Escape::Start,
                    _ => Escape::None,
                };
                continue;
            }
            Escape::Csi => {
                if ('\u{40}'..='\u{7e}').contains(&character) {
                    escape = Escape::None;
                } else if character == '\u{1b}' {
                    escape = Escape::Start;
                }
                continue;
            }
            Escape::String => {
                if matches!(character, '\u{7}' | '\u{9c}') {
                    escape = Escape::None;
                } else if character == '\u{1b}' {
                    escape = Escape::StringEnd;
                }
                continue;
            }
            Escape::StringEnd => {
                escape = match character {
                    '\\' | '\u{7}' | '\u{9c}' => Escape::None,
                    '\u{1b}' => Escape::StringEnd,
                    _ => Escape::String,
                };
                continue;
            }
            Escape::None => {}
        }
        match character {
            '\u{1b}' => escape = Escape::Start,
            '\u{9b}' => escape = Escape::Csi,
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => escape = Escape::String,
            character
                if character.is_control()
                    || character.is_whitespace()
                    || matches!(
                        character,
                        '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
                    ) =>
            {
                pending_space = !result.is_empty();
            }
            character => {
                if pending_space {
                    result.push(' ');
                    pending_space = false;
                }
                result.push(character);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pads_and_truncates_using_unicode_display_width() {
        assert_eq!(row(&[("abc", 5), ("x", 3)]), "abc   x  ");
        assert_eq!(row(&[("甲乙", 5), ("X", 2)]), "甲乙  X ");
        assert_eq!(row(&[("甲乙丙", 4)]), "甲… ");
        assert_eq!(row(&[("Café", 3)]), "Ca…");
        assert_eq!(row(&[("Cafe\u{301}", 4)]), "Cafe\u{301}");
        assert_eq!(row(&[("👨‍👩‍👧‍👦", 2)]).width(), 2);
        assert_eq!(row(&[("🇹🇼", 2)]).width(), 2);
    }

    #[test]
    fn flattens_line_breaks_tabs_controls_and_unicode_separators() {
        let rendered = row(&[(" a\r\nb\tc\u{8}\u{2028}d\u{202e}e ", 12)]);
        assert_eq!(rendered, "a b c d e   ");
        assert!(rendered.chars().all(|character| !character.is_control()));
    }

    #[test]
    fn strips_terminal_colors_cursor_controls_and_hyperlinks() {
        let rendered = row(&[(
            "A\u{1b}[31m紅\u{1b}[0m\u{1b}[2K\u{1b}]8;;https://example.test\u{1b}\\link\u{1b}]8;;\u{1b}\\B\u{9b}0m",
            12,
        )]);
        assert_eq!(rendered, "A紅linkB    ");
        assert!(!rendered.contains("example.test"));
        assert!(rendered.chars().all(|character| !character.is_control()));
    }

    #[test]
    fn handles_empty_narrow_and_incomplete_escape_cells() {
        assert_eq!(row(&[]), "");
        assert_eq!(row(&[("anything", 0)]), "");
        assert_eq!(row(&[("甲乙", 1)]), "…");
        assert_eq!(row(&[("abc\u{1b}[31", 4)]), "abc ");
        assert_eq!(row(&[("abc\u{1b}]unfinished", 4)]), "abc ");
        assert_eq!(row(&[("\n\t", 3)]), "   ");
    }
}
