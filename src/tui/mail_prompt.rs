use crate::meeting_rsvp::RsvpResponse;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;

pub const DEFAULT_SAVE_DIR: &str = "~/Downloads";

pub struct Target {
    pub id: String,
    pub subject: String,
    pub folder: String,
    pub collection: String,
}

pub enum Prompt {
    /// `confirm` 為假時在輸入目標資料夾；為真時等 y。
    Move {
        target: Target,
        input: String,
        folders: Vec<String>,
        confirm: bool,
        notice: Option<String>,
    },
    Delete {
        target: Target,
    },
    Attachment {
        id: String,
        names: Vec<String>,
        selected: usize,
        dir: String,
        notice: Option<String>,
    },
    /// `confirm` 為假時在選回覆與輸入 COMMENT；為真時等 y。
    Rsvp {
        id: String,
        summary: String,
        organizer: String,
        response: RsvpResponse,
        comment: String,
        confirm: bool,
    },
}

pub const RSVP_RESPONSES: [RsvpResponse; 3] = [
    RsvpResponse::Accept,
    RsvpResponse::Decline,
    RsvpResponse::Tentative,
];

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Cancel,
    Move {
        folder: String,
    },
    Delete,
    Save {
        index: usize,
        path: PathBuf,
    },
    Rsvp {
        response: RsvpResponse,
        comment: Option<String>,
    },
}

impl Prompt {
    pub fn attachment(id: String, names: Vec<String>) -> Self {
        Self::Attachment {
            id,
            names,
            selected: 0,
            dir: DEFAULT_SAVE_DIR.into(),
            notice: None,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc {
            return Action::Cancel;
        }
        match self {
            Self::Delete { .. } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Action::Delete,
                _ => Action::None,
            },
            Self::Rsvp {
                confirm: true,
                response,
                comment,
                ..
            } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Action::Rsvp {
                    response: *response,
                    comment: Some(comment.trim().to_owned()).filter(|c| !c.is_empty()),
                },
                _ => Action::None,
            },
            Self::Rsvp {
                response,
                comment,
                confirm,
                ..
            } => {
                let step = |by: usize| {
                    let at = RSVP_RESPONSES
                        .iter()
                        .position(|r| r == response)
                        .unwrap_or(0);
                    RSVP_RESPONSES[(at + by) % RSVP_RESPONSES.len()]
                };
                match key.code {
                    KeyCode::Tab | KeyCode::Right | KeyCode::Down => *response = step(1),
                    KeyCode::BackTab | KeyCode::Left | KeyCode::Up => {
                        *response = step(RSVP_RESPONSES.len() - 1)
                    }
                    KeyCode::Enter => *confirm = true,
                    KeyCode::Backspace => {
                        comment.pop();
                    }
                    KeyCode::Char(c) if !ctrl => comment.push(c),
                    _ => {}
                }
                Action::None
            }
            Self::Move {
                confirm: true,
                input,
                ..
            } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Action::Move {
                    folder: input.trim().to_owned(),
                },
                _ => Action::None,
            },
            Self::Move {
                target,
                input,
                folders,
                confirm,
                notice,
            } => {
                match key.code {
                    KeyCode::Enter => {
                        let folder = input.trim();
                        if folder.is_empty() {
                            *notice = Some("請輸入目標資料夾".into());
                        } else if folder == target.collection {
                            *notice = Some("郵件已在這個資料夾".into());
                        } else {
                            *notice = None;
                            *confirm = true;
                        }
                    }
                    KeyCode::Tab if !folders.is_empty() => {
                        let next = folders
                            .iter()
                            .position(|folder| folder == input.trim())
                            .map_or(0, |index| (index + 1) % folders.len());
                        *input = folders[next].clone();
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(c) if !ctrl => input.push(c),
                    _ => {}
                }
                Action::None
            }
            Self::Attachment {
                names,
                selected,
                dir,
                notice,
                ..
            } => {
                match key.code {
                    KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Down => *selected = (*selected + 1).min(names.len().saturating_sub(1)),
                    KeyCode::Backspace => {
                        dir.pop();
                    }
                    KeyCode::Char(c) if !ctrl => dir.push(c),
                    KeyCode::Enter => {
                        if dir.trim().is_empty() {
                            *notice = Some("請輸入存放目錄".into());
                        } else if let Some(name) = names.get(*selected) {
                            return Action::Save {
                                index: *selected,
                                path: expand_home(dir.trim()).join(safe_file_name(name, *selected)),
                            };
                        }
                    }
                    _ => {}
                }
                Action::None
            }
        }
    }
}

fn expand_home(dir: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match (dir, home) {
        ("~", Some(home)) => home,
        (dir, Some(home)) if dir.starts_with("~/") => home.join(&dir[2..]),
        (dir, _) => PathBuf::from(dir),
    }
}

/// 附件檔名來自寄件者，只取最後一段，避免 `../` 或絕對路徑寫出目錄外。
pub fn safe_file_name(name: &str, index: usize) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default().trim();
    if base.is_empty() || base == "." || base == ".." {
        format!("attachment-{}", index + 1)
    } else {
        base.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(prompt: &mut Prompt, code: KeyCode) -> Action {
        prompt.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn target() -> Target {
        Target {
            id: "m1".into(),
            subject: "週會".into(),
            folder: "收件匣".into(),
            collection: "INBOX".into(),
        }
    }

    #[test]
    fn move_needs_a_different_folder_then_an_explicit_y() {
        let mut prompt = Prompt::Move {
            target: target(),
            input: String::new(),
            folders: vec!["Archive".into(), "Trash".into()],
            confirm: false,
            notice: None,
        };
        assert_eq!(press(&mut prompt, KeyCode::Enter), Action::None);
        for c in "INBOX".chars() {
            press(&mut prompt, KeyCode::Char(c));
        }
        press(&mut prompt, KeyCode::Enter);
        assert!(
            matches!(&prompt, Prompt::Move { confirm: false, notice: Some(n), .. } if n.contains("已在"))
        );
        press(&mut prompt, KeyCode::Tab);
        press(&mut prompt, KeyCode::Tab);
        assert!(matches!(&prompt, Prompt::Move { input, .. } if input == "Trash"));
        press(&mut prompt, KeyCode::Enter);
        assert_eq!(
            press(&mut prompt, KeyCode::Char('n')),
            Action::None,
            "y 以外的鍵不會移動"
        );
        assert_eq!(
            press(&mut prompt, KeyCode::Char('y')),
            Action::Move {
                folder: "Trash".into()
            }
        );
        assert_eq!(press(&mut prompt, KeyCode::Esc), Action::Cancel);
    }

    #[test]
    fn delete_only_acts_on_y() {
        let mut prompt = Prompt::Delete { target: target() };
        for code in [KeyCode::Enter, KeyCode::Char('d'), KeyCode::Char(' ')] {
            assert_eq!(press(&mut prompt, code), Action::None);
        }
        assert_eq!(press(&mut prompt, KeyCode::Char('y')), Action::Delete);
    }

    #[test]
    fn rsvp_cycles_the_answer_keeps_the_comment_and_sends_only_on_y() {
        let mut prompt = Prompt::Rsvp {
            id: "m1".into(),
            summary: "季度回顧".into(),
            organizer: "carol@example.test".into(),
            response: RsvpResponse::Accept,
            comment: String::new(),
            confirm: false,
        };
        press(&mut prompt, KeyCode::Left);
        assert!(matches!(
            &prompt,
            Prompt::Rsvp {
                response: RsvpResponse::Tentative,
                ..
            }
        ));
        press(&mut prompt, KeyCode::Tab);
        press(&mut prompt, KeyCode::Tab);
        for c in " 出差 ".chars() {
            press(&mut prompt, KeyCode::Char(c));
        }
        assert_eq!(
            press(&mut prompt, KeyCode::Char('y')),
            Action::None,
            "輸入階段的 y 是 COMMENT 文字"
        );
        assert_eq!(press(&mut prompt, KeyCode::Enter), Action::None);
        assert_eq!(press(&mut prompt, KeyCode::Char('n')), Action::None);
        assert_eq!(
            press(&mut prompt, KeyCode::Char('y')),
            Action::Rsvp {
                response: RsvpResponse::Decline,
                comment: Some("出差 y".into())
            }
        );
        assert_eq!(press(&mut prompt, KeyCode::Esc), Action::Cancel);
    }

    #[test]
    fn attachment_path_stays_inside_the_chosen_directory() {
        assert_eq!(safe_file_name("../../etc/passwd", 0), "passwd");
        assert_eq!(safe_file_name("C:\\x\\報告.pdf", 0), "報告.pdf");
        assert_eq!(safe_file_name("..", 2), "attachment-3");
        let mut prompt = Prompt::attachment("m1".into(), vec!["a.txt".into(), "/tmp/b.txt".into()]);
        for _ in DEFAULT_SAVE_DIR.chars() {
            press(&mut prompt, KeyCode::Backspace);
        }
        for c in "/srv/out".chars() {
            press(&mut prompt, KeyCode::Char(c));
        }
        press(&mut prompt, KeyCode::Down);
        press(&mut prompt, KeyCode::Down);
        assert_eq!(
            press(&mut prompt, KeyCode::Enter),
            Action::Save {
                index: 1,
                path: PathBuf::from("/srv/out/b.txt")
            }
        );
    }
}
