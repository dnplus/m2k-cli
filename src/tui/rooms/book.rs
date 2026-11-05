use super::super::receive;
use crate::{
    meeting::{self, BookArgs, BookOutput, InviteArgs, InviteOutput},
    meeting_draft::Draft,
};
use anyhow::Result;
use crossterm::event::KeyCode;
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
};

enum Stage {
    Saved,
    Checking(Receiver<Result<BookOutput>>),
    Unavailable(String),
    ConfirmBook(Box<BookOutput>),
    Booking(Receiver<Result<BookOutput>>),
    BookFailed(String),
    Booked,
    ConfirmInvite(Vec<String>),
    Inviting(Receiver<Result<InviteOutput>>),
    InviteFailed(String),
    Invited(InviteOutput),
}

pub(super) struct Flow {
    pub draft: Draft,
    dir: PathBuf,
    stage: Stage,
    booked: Option<BookOutput>,
}

fn taipei(value: Option<&str>) -> String {
    value
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|date| {
            date.with_timezone(&chrono_tz::Asia::Taipei)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| value.unwrap_or("?").to_owned())
}

fn spawn<T: Send + 'static>(
    dir: &Path,
    work: impl FnOnce(&Path) -> Result<T> + Send + 'static,
) -> Receiver<Result<T>> {
    let dir = dir.to_owned();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work(&dir).map_err(|error| crate::mail_reply::masked_error(&dir, error)));
    });
    rx
}

impl Flow {
    pub fn new(dir: &Path, draft: Draft) -> Self {
        Self {
            draft,
            dir: dir.to_owned(),
            stage: Stage::Saved,
            booked: None,
        }
    }

    fn book_args(&self, dry_run: bool) -> BookArgs {
        BookArgs {
            file: self.draft.invitation.clone(),
            room: None,
            url: Some(self.draft.room_url.clone()),
            dry_run,
            target: "both".into(),
        }
    }

    pub fn writing(&self) -> bool {
        matches!(self.stage, Stage::Booking(_) | Stage::Inviting(_))
    }

    fn busy(&self) -> bool {
        self.writing() || matches!(self.stage, Stage::Checking(_))
    }

    pub fn key(&mut self, code: KeyCode) -> Option<String> {
        match (&self.stage, code) {
            (Stage::Saved | Stage::Unavailable(_), KeyCode::Char('b')) => {
                let args = self.book_args(true);
                self.stage = Stage::Checking(spawn(&self.dir, move |dir| {
                    meeting::book(dir, args).map(|(output, _)| output)
                }));
                Some("正在 live 檢查會議室與個人行事曆（dry-run，不寫入）...".into())
            }
            (Stage::ConfirmBook(_), KeyCode::Char('y')) => {
                let args = self.book_args(false);
                self.stage = Stage::Booking(spawn(&self.dir, move |dir| {
                    meeting::book(dir, args).map(|(output, _)| output)
                }));
                Some("正在預約：再次 live 檢查後寫入個人與會議室行事曆...".into())
            }
            (Stage::Booked, KeyCode::Char('i')) => {
                let parsed = meeting::invite(
                    &self.dir,
                    InviteArgs {
                        file: self.draft.invitation.clone(),
                        dry_run: true,
                        send: false,
                    },
                );
                match parsed {
                    Ok((output, _)) if output.attendees.is_empty() => {
                        Some("邀請沒有與會者，未寄送".into())
                    }
                    Ok((output, _)) => {
                        self.stage = Stage::ConfirmInvite(output.attendees);
                        Some("確認收件人：y 寄出 · Esc 取消".into())
                    }
                    Err(error) => Some(format!("無法讀取邀請：{error:#}")),
                }
            }
            (Stage::ConfirmInvite(_), KeyCode::Char('y')) => {
                let file = self.draft.invitation.clone();
                self.stage = Stage::Inviting(spawn(&self.dir, move |dir| {
                    meeting::invite(
                        dir,
                        InviteArgs {
                            file,
                            dry_run: false,
                            send: true,
                        },
                    )
                    .map(|(output, _)| output)
                }));
                Some("正在透過 SMTP 寄送會議邀請...".into())
            }
            _ => None,
        }
    }

    /// `None` leaves the draft screen; `Some` stays with a new status.
    pub fn escape(&mut self) -> Option<String> {
        match self.stage {
            Stage::ConfirmBook(_) => {
                self.stage = Stage::Saved;
                Some("已取消預約，未寫入任何行事曆；草稿保留".into())
            }
            Stage::ConfirmInvite(_) => {
                self.stage = Stage::Booked;
                Some("已取消寄送邀請，未寄出".into())
            }
            _ if self.busy() => Some("背景工作進行中，請等結果".into()),
            _ => None,
        }
    }

    pub fn poll(&mut self) -> Option<String> {
        let status = match &self.stage {
            Stage::Checking(rx) => match receive(rx)? {
                Ok(output) => {
                    self.stage = Stage::ConfirmBook(Box::new(output));
                    "live 檢查通過：會議室仍空 · y 預約 · Esc 取消".to_owned()
                }
                Err(error) => {
                    self.stage = Stage::Unavailable(format!("{error:#}"));
                    "live 檢查未通過，未寫入；草稿保留".to_owned()
                }
            },
            Stage::Booking(rx) => match receive(rx)? {
                Ok(output) if output.state == "complete" => {
                    let status = format!(
                        "已預約 {} · 已寫入個人與會議室行事曆 · i 寄邀請給與會者",
                        output.room.as_deref().unwrap_or("會議室")
                    );
                    self.booked = Some(output);
                    self.stage = Stage::Booked;
                    status
                }
                Ok(output) => {
                    self.stage = Stage::BookFailed(partial_message(&output));
                    format!("預約未完成（{}）；草稿保留", output.state)
                }
                Err(error) => {
                    self.stage = Stage::BookFailed(format!(
                        "{error:#}\n\n若訊息表示結果不確定，請先在行事曆確認是否已寫入，再用下方指令處理。"
                    ));
                    "預約失敗；草稿保留".to_owned()
                }
            },
            Stage::Inviting(rx) => match receive(rx)? {
                Ok(output) => {
                    let status = format!("已寄送會議邀請給 {}", output.attendees.join(", "));
                    self.stage = Stage::Invited(output);
                    status
                }
                Err(error) => {
                    self.stage = Stage::InviteFailed(format!("{error:#}"));
                    "寄送邀請失敗；草稿與預約保留".to_owned()
                }
            },
            _ => return None,
        };
        Some(status)
    }

    pub fn hint(&self) -> &'static str {
        match self.stage {
            Stage::Saved | Stage::Unavailable(_) => {
                "b 檢查並預約 · PageUp/Down 捲動 · Esc 返回 · ? 說明"
            }
            Stage::ConfirmBook(_) | Stage::ConfirmInvite(_) => "y 確認 · Esc 取消（不寫入）",
            Stage::Booked => "i 寄邀請給與會者 · Esc 返回 · ? 說明",
            Stage::Checking(_) | Stage::Booking(_) | Stage::Inviting(_) => {
                "背景執行中 · 寫入時 Ctrl-C 會等結果"
            }
            _ => "PageUp/Down 捲動 · Esc 返回 · ? 說明",
        }
    }

    pub fn title(&self) -> &'static str {
        match self.stage {
            Stage::Saved => " 草稿已保存 · b 檢查並預約 · Esc 返回 ",
            Stage::Checking(_) => " live 檢查中 ",
            Stage::Unavailable(_) => " 無法預約 · 未寫入 · b 重新檢查 · Esc 返回 ",
            Stage::ConfirmBook(_) => " 確認預約 · y 寫入 · Esc 取消 ",
            Stage::Booking(_) => " 預約中 ",
            Stage::BookFailed(_) => " 預約未完成 · Esc 返回 ",
            Stage::Booked => " 已預約 · i 寄邀請 · Esc 返回 ",
            Stage::ConfirmInvite(_) => " 確認寄送邀請 · y 寄出 · Esc 取消 ",
            Stage::Inviting(_) => " 寄送邀請中 ",
            Stage::InviteFailed(_) => " 寄送邀請失敗 · Esc 返回 ",
            Stage::Invited(_) => " 已寄送邀請 · Esc 返回 ",
        }
    }

    pub fn text(&self) -> String {
        let draft = &self.draft;
        let head = format!(
            "{}\n{} → {}（Asia/Taipei）\n與會者：{}\nUID：{}\nICS：{}\nJSON：{}\n\n",
            draft.title,
            taipei(Some(&draft.start)),
            taipei(Some(&draft.end)),
            draft.attendees.join(", "),
            draft.uid,
            draft.invitation.display(),
            draft.manifest.display()
        );
        let body = match &self.stage {
            Stage::Saved => "按 b 先 live 檢查會議室是否仍空（同 meeting book --dry-run，不寫入）；通過後才出現確認畫面。".to_owned(),
            Stage::Checking(_) => "正在 live 查詢會議室與個人行事曆，尚未寫入...".to_owned(),
            Stage::Unavailable(reason) => format!(
                "live 檢查未通過，未寫入任何行事曆：\n{reason}\n\n若是時段衝突，請 Esc 回結果改選其他時段；也可按 b 重新檢查。"
            ),
            Stage::ConfirmBook(output) => self.confirm_book_text(output),
            Stage::Booking(_) => "正在預約：寫入前會再 live 檢查一次...".to_owned(),
            Stage::BookFailed(message) => format!("預約未完成，草稿保留。\n{message}"),
            Stage::Booked => {
                let room = self.booked.as_ref().and_then(|output| output.room.as_deref());
                format!(
                    "已預約 {}，已寫入個人與會議室行事曆。\n邀請尚未寄出；按 i 確認收件人後寄送。",
                    room.unwrap_or("會議室")
                )
            }
            Stage::ConfirmInvite(attendees) => format!(
                "將透過 SMTP 寄送 METHOD:REQUEST 會議邀請「{}」給：\n{}\n\ny 寄出 · Esc 取消（不寄出）",
                draft.title,
                attendees
                    .iter()
                    .map(|address| format!("  {address}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            Stage::Inviting(_) => "正在寄送會議邀請...".to_owned(),
            Stage::InviteFailed(error) => format!(
                "寄送邀請失敗：{error}\n預約保留。請先確認寄件備份是否已有這封邀請，再決定是否用下方 send_invitation 指令重寄。"
            ),
            Stage::Invited(output) => format!(
                "已寄送會議邀請給 {}\nMessage-ID：{}",
                output.attendees.join(", "),
                output.message_id.as_deref().unwrap_or("-")
            ),
        };
        let mut text = if matches!(self.stage, Stage::ConfirmBook(_) | Stage::ConfirmInvite(_)) {
            format!("{body}\n\n")
        } else {
            format!("{head}{body}\n\n")
        };
        if matches!(
            self.stage,
            Stage::Saved | Stage::Unavailable(_) | Stage::BookFailed(_) | Stage::InviteFailed(_)
        ) {
            text.push_str("CLI 指令：\n");
            for (name, command) in &draft.actions {
                text.push_str(&format!("{name}\n{command}\n\n"));
            }
        }
        text
    }

    fn confirm_book_text(&self, output: &BookOutput) -> String {
        let email = crate::config::load(&self.dir)
            .map(|config| config.email)
            .unwrap_or_else(|_| "自己".into());
        let mut text = format!(
            "live 檢查通過：會議室在這段時間仍空。\n\n會議室：{}\n開始：{}（Asia/Taipei）\n結束：{}（Asia/Taipei）\n主旨：{}\n將寫入：\n  個人行事曆（{email}）\n  會議室行事曆（{}）\n",
            output.room.as_deref().unwrap_or("?"),
            taipei(output.start.as_deref()),
            taipei(output.end.as_deref()),
            self.draft.title,
            self.draft.room_url
        );
        for warning in &output.warnings {
            text.push_str(&format!("注意：{warning}\n"));
        }
        text.push_str("\ny 預約（寫入前再 live 檢查一次）· Esc 取消（不寫入）");
        text
    }
}

fn partial_message(output: &BookOutput) -> String {
    let mut lines = vec![format!("狀態：{}", output.state)];
    for done in &output.completed {
        lines.push(format!("已寫入：{}", done.target));
    }
    if let Some(failed) = &output.failed {
        lines.push(format!("未完成：{}（{}）", failed.target, failed.error));
    }
    if let Some(remediation) = &output.remediation {
        lines.push(format!("下一步：{remediation}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::{BookFailure, BookTargetResult};

    #[test]
    fn partial_booking_names_what_was_written_what_failed_and_the_next_step() {
        let output = BookOutput {
            state: "partial".into(),
            uid: Some("u@m2k".into()),
            room: Some("第一會議室".into()),
            target: "both".into(),
            start: None,
            end: None,
            dry_run: false,
            warnings: Vec::new(),
            completed: vec![BookTargetResult {
                target: "me@example.test".into(),
                calendar_url: "https://mail.example.test/me/".into(),
                item_id: None,
                cache_error: None,
            }],
            failed: Some(BookFailure {
                target: "第一會議室".into(),
                calendar_url: "https://mail.example.test/room/".into(),
                error: "HTTP 403 Forbidden".into(),
            }),
            remediation: Some("m2k meeting book --file x --target room".into()),
            receipt: None,
        };
        let message = partial_message(&output);
        assert!(message.contains("已寫入：me@example.test"), "{message}");
        assert!(
            message.contains("未完成：第一會議室（HTTP 403 Forbidden）"),
            "{message}"
        );
        assert!(
            message.contains("下一步：m2k meeting book --file x --target room"),
            "{message}"
        );
    }
}
