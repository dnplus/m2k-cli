//! TUI 回覆草稿的狀態機：不碰終端或檔案，副作用由 `App` 依 `Action` 執行。

use crate::mail_reply::{self, ReplyHeaders};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 內建多行輸入（沒有 `$EDITOR`）。
    Editing,
    /// 外部編輯器執行中；此時終端已交給編輯器，不會收到按鍵。
    External,
    /// 編輯完成，等使用者確認寄出／存草稿／繼續編輯。
    Confirm,
    /// SMTP 寄送中，忽略按鍵。
    Sending,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// 存草稿檔；`close` 為真時結束回覆流程。
    Save {
        close: bool,
    },
    /// 先存草稿，再走 CLI 相同的寄送路徑。
    Send,
    /// 重新開啟外部編輯器。
    Edit,
}

pub struct ReplyFlow {
    pub mail_id: String,
    pub text: String,
    pub phase: Phase,
    pub external: bool,
    pub demo: bool,
    pub draft_path: Option<PathBuf>,
    pub notice: Option<String>,
}

impl ReplyFlow {
    pub fn new(mail_id: String, text: String, external: bool, demo: bool) -> Self {
        Self {
            mail_id,
            text,
            phase: if external {
                Phase::External
            } else {
                Phase::Editing
            },
            external,
            demo,
            draft_path: None,
            notice: None,
        }
    }

    /// 目前文字能否寄出；不能時回傳原因。
    pub fn check(&self) -> Result<(ReplyHeaders, String), String> {
        mail_reply::parse_draft(&self.text).map_err(|error| format!("{error:#}"))
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match self.phase {
            Phase::External | Phase::Sending => Action::None,
            Phase::Editing => match key.code {
                KeyCode::Char('d') if ctrl => {
                    self.phase = Phase::Confirm;
                    self.notice = None;
                    Action::None
                }
                KeyCode::Esc => Action::Save { close: true },
                KeyCode::Enter => {
                    self.text.push('\n');
                    Action::None
                }
                KeyCode::Tab => {
                    self.text.push('\t');
                    Action::None
                }
                KeyCode::Backspace => {
                    self.text.pop();
                    Action::None
                }
                KeyCode::Char(c) if !ctrl => {
                    self.text.push(c);
                    Action::None
                }
                _ => Action::None,
            },
            Phase::Confirm => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if self.demo {
                        self.notice = Some("展示資料只能存草稿，不會寄出".into());
                        return Action::None;
                    }
                    if let Err(reason) = self.check() {
                        self.notice = Some(format!("無法寄出：{reason}"));
                        return Action::None;
                    }
                    self.phase = Phase::Sending;
                    self.notice = None;
                    Action::Send
                }
                KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Esc => {
                    Action::Save { close: true }
                }
                KeyCode::Char('e') | KeyCode::Char('E') => {
                    self.notice = None;
                    if self.external {
                        self.phase = Phase::External;
                        Action::Edit
                    } else {
                        self.phase = Phase::Editing;
                        Action::None
                    }
                }
                _ => Action::None,
            },
        }
    }

    /// 外部編輯器結束並讀回文字後呼叫。
    pub fn editor_finished(&mut self, text: String) {
        self.text = text;
        self.phase = Phase::Confirm;
        self.notice = None;
    }

    /// 寄送失敗：回到確認畫面，草稿保留，不自動重試。
    pub fn send_failed(&mut self, reason: &str) {
        self.phase = Phase::Confirm;
        let kept = self
            .draft_path
            .as_ref()
            .map(|path| format!("；草稿保留於 {}", path.display()))
            .unwrap_or_default();
        self.notice = Some(format!(
            "寄送失敗：{reason}{kept}（不會自動重試，按 y 再寄）"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(flow: &mut ReplyFlow, code: KeyCode) -> Action {
        flow.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ready(external: bool, demo: bool) -> ReplyFlow {
        let mut flow = ReplyFlow::new(
            "m1".into(),
            "To: a@example.test\nSubject: Re: x\n\n好的\n\n> 原文\n".into(),
            external,
            demo,
        );
        flow.phase = Phase::Confirm;
        flow
    }

    #[test]
    fn builtin_editor_collects_text_and_ctrl_d_moves_to_confirm_not_send() {
        let mut flow = ReplyFlow::new("m1".into(), String::new(), false, false);
        assert_eq!(flow.phase, Phase::Editing);
        for c in "hi".chars() {
            press(&mut flow, KeyCode::Char(c));
        }
        press(&mut flow, KeyCode::Enter);
        press(&mut flow, KeyCode::Char('x'));
        press(&mut flow, KeyCode::Backspace);
        assert_eq!(flow.text, "hi\n");
        let action = flow.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!((action, flow.phase), (Action::None, Phase::Confirm));
    }

    #[test]
    fn only_explicit_y_in_confirm_sends_and_other_keys_never_do() {
        let mut flow = ready(true, false);
        for code in [KeyCode::Enter, KeyCode::Char(' '), KeyCode::Char('q')] {
            assert_eq!(press(&mut flow, code), Action::None);
            assert_eq!(flow.phase, Phase::Confirm);
        }
        assert_eq!(press(&mut flow, KeyCode::Char('y')), Action::Send);
        assert_eq!(flow.phase, Phase::Sending);
        assert_eq!(press(&mut flow, KeyCode::Char('y')), Action::None);
        assert_eq!(press(&mut flow, KeyCode::Esc), Action::None);
    }

    #[test]
    fn demo_refuses_send_but_can_save() {
        let mut flow = ready(true, true);
        assert_eq!(press(&mut flow, KeyCode::Char('y')), Action::None);
        assert_eq!(flow.phase, Phase::Confirm);
        assert!(flow.notice.as_deref().unwrap().contains("只能存草稿"));
        assert_eq!(
            press(&mut flow, KeyCode::Char('d')),
            Action::Save { close: true }
        );
    }

    #[test]
    fn invalid_draft_cannot_be_sent_and_can_be_edited_again() {
        let mut flow = ready(true, false);
        flow.text = "To: a@example.test\nSubject: s\n\n> 只有引用\n".into();
        assert_eq!(press(&mut flow, KeyCode::Char('y')), Action::None);
        assert!(flow.notice.as_deref().unwrap().contains("正文是空的"));
        assert_eq!(press(&mut flow, KeyCode::Char('e')), Action::Edit);
        assert_eq!(flow.phase, Phase::External);
        flow.editor_finished("To: a@example.test\nSubject: s\n\n收到\n".into());
        assert_eq!(flow.phase, Phase::Confirm);
        assert_eq!(press(&mut flow, KeyCode::Char('y')), Action::Send);
    }

    #[test]
    fn send_failure_returns_to_confirm_keeps_draft_and_needs_new_y() {
        let mut flow = ready(false, false);
        flow.draft_path = Some("/d/reply-1.txt".into());
        assert_eq!(press(&mut flow, KeyCode::Char('y')), Action::Send);
        flow.send_failed("SMTP 拒絕");
        assert_eq!(flow.phase, Phase::Confirm);
        let notice = flow.notice.clone().unwrap();
        assert!(notice.contains("SMTP 拒絕") && notice.contains("/d/reply-1.txt"));
        assert_eq!(press(&mut flow, KeyCode::Char('e')), Action::None);
        assert_eq!(flow.phase, Phase::Editing);
    }

    #[test]
    fn esc_in_builtin_editor_and_confirm_saves_instead_of_discarding() {
        let mut flow = ReplyFlow::new("m1".into(), "x".into(), false, false);
        assert_eq!(press(&mut flow, KeyCode::Esc), Action::Save { close: true });
        let mut flow = ready(false, false);
        assert_eq!(press(&mut flow, KeyCode::Esc), Action::Save { close: true });
    }
}
