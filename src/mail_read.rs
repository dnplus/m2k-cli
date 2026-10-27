pub fn latest_reply(text: &str) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if quoted(line) {
            index += 1;
            continue;
        }
        if original_separator(line)
            || header_bundle(&lines, index)
            || rule_before_header_bundle(&lines, index)
        {
            break;
        }
        if let Some(end) = attribution_end(&lines, index) {
            let next = lines[end + 1..].iter().find(|line| !line.trim().is_empty());
            if next.is_some_and(|line| quoted(line)) {
                index = end + 1;
                continue;
            }
            break;
        }
        output.push(line);
        index += 1;
    }
    let Some(first) = output.iter().position(|line| !line.trim().is_empty()) else {
        return String::new();
    };
    let last = output
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .unwrap();
    output[first..=last].join("\n")
}

fn quoted(line: &str) -> bool {
    line.trim_start().starts_with('>')
}

fn original_separator(line: &str) -> bool {
    let line = line.trim();
    if line.eq_ignore_ascii_case("Begin forwarded message:") {
        return true;
    }
    let leading = line.bytes().take_while(|byte| *byte == b'-').count();
    let trailing = line.bytes().rev().take_while(|byte| *byte == b'-').count();
    if leading < 3 || trailing < 3 || leading + trailing >= line.len() {
        return false;
    }
    let label = line[leading..line.len() - trailing]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    matches!(
        label.as_str(),
        "original message"
            | "forwarded message"
            | "原信件"
            | "原始信件"
            | "原始郵件"
            | "原始邮件"
            | "轉寄信件"
            | "轉寄郵件"
            | "转寄邮件"
            | "轉發郵件"
            | "转发邮件"
    )
}

fn attribution_end(lines: &[&str], start: usize) -> Option<usize> {
    let first = lines[start].trim();
    if first.ends_with(['.', '。', '!', '！', '?', '？']) {
        return None;
    }
    let english_start = first.to_ascii_lowercase().starts_with("on ");
    let chinese_start =
        first.contains(['於', '于', '在']) && (first.contains('@') || date_hint(first));
    if !(english_start || chinese_start || first.contains('@')) {
        return None;
    }
    let mut joined = String::new();
    for (index, line) in lines.iter().enumerate().skip(start).take(6) {
        let line = line.trim();
        if line.is_empty() || quoted(line) || joined.len() + line.len() > 512 {
            break;
        }
        if !joined.is_empty() {
            joined.push(' ');
        }
        joined.push_str(line);
        let Some(without_colon) = joined.strip_suffix([':', '：']) else {
            continue;
        };
        let without_colon = without_colon.trim_end();
        let english = without_colon.to_ascii_lowercase();
        let english_attribution = english.starts_with("on ") && english.ends_with(" wrote");
        let chinese_attribution =
            !english_start && (without_colon.ends_with("寫道") || without_colon.ends_with("写道"));
        let followed_by_quote = lines[index + 1..]
            .iter()
            .find(|line| !line.trim().is_empty())
            .is_some_and(|line| quoted(line));
        if (english_attribution || chinese_attribution)
            && (joined.contains('@') || date_hint(&joined) || followed_by_quote)
        {
            return Some(index);
        }
    }
    None
}

fn date_hint(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let words = lower
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    if words.iter().any(|word| {
        matches!(
            *word,
            "mon"
                | "monday"
                | "tue"
                | "tues"
                | "tuesday"
                | "wed"
                | "wednesday"
                | "thu"
                | "thur"
                | "thurs"
                | "thursday"
                | "fri"
                | "friday"
                | "sat"
                | "saturday"
                | "sun"
                | "sunday"
        )
    }) || ["星期", "週", "周", "禮拜", "礼拜"].iter().any(|prefix| {
        text.match_indices(*prefix).any(|(index, _)| {
            text[index + prefix.len()..]
                .chars()
                .next()
                .is_some_and(|day| {
                    matches!(day, '一' | '二' | '三' | '四' | '五' | '六' | '日' | '天')
                })
        })
    }) {
        return true;
    }
    let has_digit = text.bytes().any(|byte| byte.is_ascii_digit());
    if !has_digit {
        return false;
    }
    if text.contains('月') && text.contains(['年', '日']) {
        return true;
    }
    if words.iter().enumerate().any(|(index, word)| {
        matches!(
            *word,
            "jan"
                | "january"
                | "feb"
                | "february"
                | "mar"
                | "march"
                | "apr"
                | "april"
                | "may"
                | "jun"
                | "june"
                | "jul"
                | "july"
                | "aug"
                | "august"
                | "sep"
                | "sept"
                | "september"
                | "oct"
                | "october"
                | "nov"
                | "november"
                | "dec"
                | "december"
        ) && [index.checked_sub(1), Some(index + 1)]
            .into_iter()
            .flatten()
            .filter_map(|neighbor| words.get(neighbor))
            .filter_map(|word| word.parse::<u16>().ok())
            .any(|number| (1..=31).contains(&number) || (1000..=9999).contains(&number))
    }) {
        return true;
    }
    let bytes = text.as_bytes();
    for start in 0..bytes.len().saturating_sub(7) {
        let rest = &bytes[start..];
        if start > 0 && bytes[start - 1].is_ascii_digit() {
            continue;
        }
        if rest.len() < 8 || !rest[..4].iter().all(u8::is_ascii_digit) {
            continue;
        }
        let separator = rest[4];
        if !matches!(separator, b'-' | b'/') {
            continue;
        }
        let month_len = rest[5..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if !(1..=2).contains(&month_len) || rest.get(5 + month_len) != Some(&separator) {
            continue;
        }
        let day_start = 6 + month_len;
        let day_len = rest[day_start..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if !(1..=2).contains(&day_len) {
            continue;
        }
        let month = std::str::from_utf8(&rest[5..5 + month_len])
            .unwrap()
            .parse::<u8>()
            .unwrap();
        let day = std::str::from_utf8(&rest[day_start..day_start + day_len])
            .unwrap()
            .parse::<u8>()
            .unwrap();
        if (1..=12).contains(&month) && (1..=31).contains(&day) {
            return true;
        }
    }
    false
}

fn rule_before_header_bundle(lines: &[&str], start: usize) -> bool {
    let line = lines[start].trim();
    if line.len() < 3 || !line.bytes().all(|byte| matches!(byte, b'-' | b'_')) {
        return false;
    }
    lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| !line.trim().is_empty())
        .is_some_and(|(index, _)| header_bundle(lines, index))
}

fn header_bundle(lines: &[&str], start: usize) -> bool {
    let Some((first, _)) = header(lines[start]) else {
        return false;
    };
    if !matches!(
        first.to_ascii_lowercase().as_str(),
        "from" | "寄件者" | "寄件人" | "發件人" | "发件人" | "發件者" | "发件者"
    ) {
        return false;
    }
    let mut fields = 0_u8;
    for line in lines.iter().skip(start).take(8) {
        let Some((name, value)) = header(line) else {
            return false;
        };
        if value.is_empty() {
            return false;
        }
        fields |= match name.to_ascii_lowercase().as_str() {
            "from" | "寄件者" | "寄件人" | "發件人" | "发件人" | "發件者" | "发件者"
                if value.contains('@') =>
            {
                1
            }
            "sent" | "date" | "寄件日期" | "傳送日期" | "发送时间" | "發送時間" | "日期"
                if date_hint(value) =>
            {
                2
            }
            "to" | "收件者" | "收件人" => 4,
            "subject" | "主旨" | "標題" | "标题" => 8,
            "cc" | "bcc" | "副本" | "密件副本" => 0,
            _ => return false,
        };
        if fields == 15 {
            return true;
        }
    }
    false
}

fn header(line: &str) -> Option<(&str, &str)> {
    let (name, value) = line.trim().split_once([':', '：'])?;
    Some((name.trim(), value.trim()))
}
