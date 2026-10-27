use m2k::mail_read::latest_reply;

#[test]
fn newest_reply_preserves_paragraphs_signature_and_discards_plain_original() {
    let text = "Approved.\n\nPlease ship tomorrow.\n\n-- \nAlex\n\nOn Wed, Oct 7, 2026 at 09:00 Sender <sender@example.test> wrote:\nPrevious message.\n\nEven older message.";
    assert_eq!(
        latest_reply(text),
        "Approved.\n\nPlease ship tomorrow.\n\n-- \nAlex"
    );
}

#[test]
fn quoted_lines_are_removed_but_inline_replies_survive() {
    let text = "> First old question\nFirst new answer.\n\n >> Nested quote\n> Second old question\nSecond new answer.\n\nRegards,\nAlex";
    assert_eq!(
        latest_reply(text),
        "First new answer.\n\nSecond new answer.\n\nRegards,\nAlex"
    );
}

#[test]
fn english_attribution_with_quoted_original_keeps_inline_and_bottom_reply() {
    let text = "On October 7, 2026, Sender wrote:\n> First old question\nNew inline answer.\n> Second old question\n\nNew bottom reply.";
    assert_eq!(
        latest_reply(text),
        "New inline answer.\n\nNew bottom reply."
    );
}

#[test]
fn short_english_attribution_is_clear_when_explicit_quote_lines_follow() {
    assert_eq!(
        latest_reply("On Tuesday, Alice wrote:\n> Previous message."),
        ""
    );
}

#[test]
fn weekday_attributions_cut_unquoted_original_without_email_or_numeric_date() {
    for attribution in [
        "On Monday, Alice wrote:",
        "On Tue, Alice wrote:",
        "On Sunday, Alice wrote:",
        "在星期一，小明寫道：",
        "於週日，小明寫道：",
    ] {
        assert_eq!(
            latest_reply(&format!("Newest reply.\n{attribution}\nOld unquoted text.")),
            "Newest reply."
        );
    }
}

#[test]
fn crlf_and_wrapped_attribution_do_not_expose_quoted_chain() {
    let text = "Newest paragraph.\r\n\r\nOn Wed, Oct 7, 2026 at 09:00,\r\nSender <sender@example.test>\r\nwrote:\r\n> Earlier text\r\n>> Older text\r\n";
    assert_eq!(latest_reply(text), "Newest paragraph.");
}

#[test]
fn wrapped_attribution_with_plain_original_cuts_before_the_first_header_line() {
    let text = "Newest reply.\n\nOn Wed, Oct 7, 2026 at 09:00,\nSender <sender@example.test> wrote:\nOld unquoted content.";
    assert_eq!(latest_reply(text), "Newest reply.");
}

#[test]
fn chinese_reply_attributions_with_fullwidth_colon_are_recognized() {
    for attribution in [
        "於 2026年10月7日 09:00，王小明 <sender@example.test> 寫道：",
        "王小明 <sender@example.test> 在 2026/10/7 09:00 寫道：",
        "在 2026-10-07 09:00 王小明 寫道：",
        "于 2026年10月7日，王小明 写道：",
    ] {
        assert_eq!(
            latest_reply(&format!("最新回覆。\n\n{attribution}\n原文。")),
            "最新回覆。"
        );
    }
}

#[test]
fn chinese_inline_reply_after_wrapped_attribution_survives() {
    let text = "於 2026年10月7日 09:00，\n王小明 <sender@example.test> 寫道：\n> 舊問題。\n新的回答。\n\n敬祝順心\n小李";
    assert_eq!(latest_reply(text), "新的回答。\n\n敬祝順心\n小李");
}

#[test]
fn standalone_original_and_forwarded_markers_cut_unquoted_history() {
    for marker in [
        "-----Original Message-----",
        "---------- Forwarded Message ----------",
        "-------- 原始信件 --------",
        "----- 原始郵件 -----",
        "----- 原信件 -----",
        "----- 转发邮件 -----",
        "Begin forwarded message:",
    ] {
        assert_eq!(
            latest_reply(&format!(
                "Newest reply.\n\n{marker}\nFrom: sender@example.test\nOld content."
            )),
            "Newest reply."
        );
    }
}

#[test]
fn mail2000_style_chinese_header_bundle_is_a_clear_original_message() {
    let text = "已確認，謝謝。\n\n寄件者：王小明 <sender@example.test>\n寄件日期：2026/10/7 09:00\n收件者：小李\n主旨：舊郵件\n\n原本的信件內容。";
    assert_eq!(latest_reply(text), "已確認，謝謝。");
}

#[test]
fn english_outlook_header_bundle_cuts_old_text_with_or_without_horizontal_rule() {
    for rule in ["", "____________________________\n\n", "--------\n"] {
        for date_label in ["Sent", "Date"] {
            let text = format!(
                "Newest reply.\n\n{rule}From: Alice <alice@example.test>\n\
                 {date_label}: Monday, October 7, 2026 09:00\n\
                 To: Bob <bob@example.test>\nSubject: Old subject\n\nOld unquoted text."
            );
            assert_eq!(latest_reply(&text), "Newest reply.");
        }
    }
}

#[test]
fn horizontal_rule_before_incomplete_headers_is_preserved() {
    let text =
        "Newest note.\n\n---\nFrom: alice@example.test\nSubject: A new note\nKeep this body.";
    assert_eq!(latest_reply(text), text);
}

#[test]
fn prose_mentions_wrote_from_or_original_message_are_preserved() {
    let text = "I wrote: use the Original Message button.\nFrom the notes, we should keep this.\nOn this checklist, I wrote:\nKeep the newest task.\n\nOn request 2, I may have wrote:\nKeep this new explanation too.\n\n----- Original Message ----- is a button label.\n原始郵件只是這段說明中的名詞。\n在說明文件裡我寫道：\n保留這個新的段落。";
    assert_eq!(latest_reply(text), text);
}

#[test]
fn ordinary_from_fields_and_incomplete_chinese_header_examples_are_preserved() {
    for text in [
        "From: sender@example.test\nSubject: A new note\nBody of this note.",
        "寄件者：這是欄位名稱\n日期：明天\n主旨：說明\n保留這些文字。",
        "寄件者：sender@example.test\n寄件日期：2026-10-07\n請保留這項新的指示。",
    ] {
        assert_eq!(latest_reply(text), text);
    }
}

#[test]
fn quoted_original_marker_does_not_cut_later_inline_reply() {
    assert_eq!(
        latest_reply("> -----Original Message-----\n> Old text\nNew inline answer."),
        "New inline answer."
    );
}

#[test]
fn quotes_only_and_attribution_only_return_empty_without_fallback() {
    for text in [
        "> Old text\n >> Nested old text\n\n> Final quote",
        "On Oct 7, 2026, Sender <sender@example.test> wrote:\n> Old text",
        "於 2026年10月7日，王小明 寫道：\n> 舊信件",
        "-----Original Message-----\nOld text",
        "On Oct 7, 2026, Sender wrote:",
        "\r\n\r\n",
        "",
    ] {
        assert_eq!(latest_reply(text), "");
    }
}

#[test]
fn paragraph_boundary_prevents_accidental_multiline_attribution() {
    let text = "On Oct 7, 2026 we planned this.\n\nA different paragraph ends with wrote:\nKeep this useful reply.";
    assert_eq!(latest_reply(text), text);
}

#[test]
fn new_prose_mentioning_at_does_not_merge_with_a_later_chinese_attribution() {
    let text = "我在這裡補充最新進度\n在 2026-10-07 王小明寫道：\n舊信件。";
    assert_eq!(latest_reply(text), "我在這裡補充最新進度");
    let text = "On Monday we completed this.\n在 2026-10-07 王小明寫道：\n舊信件。";
    assert_eq!(latest_reply(text), "On Monday we completed this.");
}

#[test]
fn indentation_and_signature_separator_are_preserved() {
    assert_eq!(
        latest_reply("\n  New indented text.\n\n-- \nAlex\n\n> old text\n"),
        "  New indented text.\n\n-- \nAlex"
    );
}
