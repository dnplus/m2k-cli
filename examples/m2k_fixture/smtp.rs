//! Implicit-TLS SMTP submission with AUTH PLAIN/LOGIN. Every accepted
//! message is saved under `smtp/`; mail to the fixture user also lands in
//! INBOX so `sync` and `mail watch` can see it.

use super::net::LineConn;
use super::state::Fixture;
use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use serde_json::json;
use std::{collections::BTreeSet, net::TcpStream, sync::Arc};

type Conn = LineConn<native_tls::TlsStream<TcpStream>>;

#[derive(Default)]
struct Envelope {
    from: Option<String>,
    to: Vec<String>,
}

pub fn serve(fixture: Arc<Fixture>, stream: native_tls::TlsStream<TcpStream>) -> Result<()> {
    let mut conn = LineConn::new(stream);
    conn.write("220 localhost m2k fixture ESMTP\r\n")?;
    let mut authenticated = false;
    let mut envelope = Envelope::default();
    while let Some(line) = conn.read_line()? {
        let line = String::from_utf8_lossy(&line).into_owned();
        let upper = line.to_ascii_uppercase();
        let verb = upper.split_whitespace().next().unwrap_or("");
        match verb {
            "EHLO" => conn.write(
                "250-localhost\r\n250-AUTH PLAIN LOGIN\r\n250-8BITMIME\r\n250-SMTPUTF8\r\n250 ENHANCEDSTATUSCODES\r\n",
            )?,
            "HELO" => conn.write("250 localhost\r\n")?,
            "NOOP" => conn.write("250 2.0.0 ok\r\n")?,
            "RSET" => {
                envelope = Envelope::default();
                conn.write("250 2.0.0 reset\r\n")?;
            }
            "QUIT" => {
                conn.write("221 2.0.0 bye\r\n")?;
                return Ok(());
            }
            "AUTH" => {
                authenticated = auth(&fixture, &mut conn, &line)?;
                if authenticated {
                    conn.write("235 2.7.0 authenticated\r\n")?;
                } else {
                    conn.write("535 5.7.8 invalid credentials\r\n")?;
                }
            }
            "MAIL" | "RCPT" | "DATA" if !authenticated => {
                conn.write("530 5.7.0 authentication required\r\n")?
            }
            "MAIL" => {
                envelope = Envelope {
                    from: Some(address(&line)),
                    to: Vec::new(),
                };
                conn.write("250 2.1.0 sender ok\r\n")?;
            }
            "RCPT" => {
                envelope.to.push(address(&line));
                conn.write("250 2.1.5 recipient ok\r\n")?;
            }
            "DATA" => {
                if envelope.from.is_none() || envelope.to.is_empty() {
                    conn.write("503 5.5.1 need MAIL and RCPT\r\n")?;
                    continue;
                }
                conn.write("354 end with <CRLF>.<CRLF>\r\n")?;
                let mut message = Vec::new();
                while let Some(data) = conn.read_line()? {
                    if data == b"." {
                        break;
                    }
                    let data = data.strip_prefix(b".").unwrap_or(&data);
                    message.extend_from_slice(data);
                    message.extend_from_slice(b"\r\n");
                }
                deliver(&fixture, &envelope, message);
                envelope = Envelope::default();
                conn.write("250 2.0.0 queued\r\n")?;
            }
            _ => conn.write("502 5.5.2 command not implemented\r\n")?,
        }
    }
    Ok(())
}

fn address(line: &str) -> String {
    let start = line.find('<').map_or(0, |i| i + 1);
    let end = line[start..].find('>').map_or(line.len(), |i| start + i);
    line[start..end].trim().to_owned()
}

fn decode(value: &str) -> String {
    STANDARD
        .decode(value.trim())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

fn auth(fixture: &Fixture, conn: &mut Conn, line: &str) -> Result<bool> {
    let mut parts = line.split_whitespace().skip(1);
    let mechanism = parts.next().unwrap_or("").to_ascii_uppercase();
    let initial = parts.next().map(ToOwned::to_owned);
    let mut prompt = |text: &str| -> Result<String> {
        conn.write(format!("334 {text}\r\n"))?;
        Ok(conn
            .read_line()?
            .map(|line| String::from_utf8_lossy(&line).into_owned())
            .unwrap_or_default())
    };
    let (user, password) = match mechanism.as_str() {
        "PLAIN" => {
            let payload = match initial {
                Some(payload) => payload,
                None => prompt("")?,
            };
            let decoded = decode(&payload);
            let mut fields = decoded.split('\0').skip(1);
            (
                fields.next().unwrap_or("").to_owned(),
                fields.next().unwrap_or("").to_owned(),
            )
        }
        "LOGIN" => {
            let user = match initial {
                Some(user) => decode(&user),
                None => decode(&prompt("VXNlcm5hbWU6")?),
            };
            (user, decode(&prompt("UGFzc3dvcmQ6")?))
        }
        _ => return Ok(false),
    };
    Ok(user == fixture.user && password == fixture.password)
}

fn deliver(fixture: &Fixture, envelope: &Envelope, message: Vec<u8>) {
    let subject = mailparse::parse_headers(&message)
        .ok()
        .and_then(|(headers, _)| {
            use mailparse::MailHeaderMap;
            headers.get_first_value("Subject")
        })
        .unwrap_or_default();
    let to_self = envelope
        .to
        .iter()
        .any(|rcpt| rcpt.eq_ignore_ascii_case(&fixture.user));
    let mut inbox_uid = None;
    if to_self {
        let mut mail = fixture.mail.lock().expect("mail lock");
        if let Some(inbox) = mail.get_mut("INBOX") {
            inbox_uid = Some(inbox.append(message.clone(), BTreeSet::new(), Utc::now()));
        }
    }
    fixture.record(
        "smtp.message",
        json!({
            "from": envelope.from,
            "to": envelope.to,
            "subject": subject,
            "delivered_to_inbox_uid": inbox_uid,
        }),
        Some(("smtp", "message.eml", &message)),
    );
}
