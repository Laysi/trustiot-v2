//! The email with a login code (services/identity/README.md, "登入"; Laysi/trustiot-v2#1142):
//! the same content as plain text and as HTML, Traditional Chinese first, then English.
//!
//! - **The code in the preview.** An inbox list and a phone's notification show the first text
//!   of a message. The HTML starts with a hidden preheader and the plain text with a first line
//!   that both begin with the code, so the code shows without opening the email (decided on
//!   2026-11-07; the preview, like the email, is visible to whoever sees the screen). The
//!   subject never holds the code.
//! - **Self-contained.** No image, font, style sheet, link, tracking pixel or script: nothing
//!   the client loads, so opening the email tells nobody anything, and there is nothing to
//!   click. The brand is the word "TrustIoT" in the brand colour.
//! - **Every inserted value is HTML-escaped** ([`push_escaped`]), even the code and the minutes,
//!   which are digits: the template stays safe if a value changes.
//! - **Mail-client layout.** Tables, inline styles, at most 600 px wide, readable on a narrow
//!   screen; a `prefers-color-scheme: dark` block for the clients that read it, with the design
//!   system's dark colours ([`PALETTE`]). The code is large monospace digits spaced with CSS
//!   `letter-spacing` only, so copying it gives exactly the digits.
//! - **Wiped.** Both bodies are built straight into [`Zeroizing`] buffers sized up front, so
//!   no reallocation leaves a copy of the code behind.

use zeroize::Zeroizing;

use crate::login::{CODE_LIFETIME, EmailAddress, LoginCode};
use crate::mail::Message;

/// The subject: the same for every code, and never the code itself.
pub const SUBJECT: &str = "TrustIoT 登入驗證碼 / Login code";

/// One colour of the email, from docs/design/system/tokens.css (the `oklch` tokens converted to
/// sRGB, which every mail client reads). Light is what the inline styles say; dark is what the
/// `prefers-color-scheme: dark` block says.
#[derive(Clone, Copy, Debug)]
pub struct Colour {
    /// The token.
    pub token: &'static str,
    /// Its light value.
    pub light: &'static str,
    /// Its dark value.
    pub dark: &'static str,
}

/// Every colour the HTML uses. A test checks that the template uses no other, and that the
/// pairs it draws meet the design system's contrast rules in both themes.
pub const PALETTE: [Colour; 8] = [
    // `muted`: the page around the card.
    Colour {
        token: "muted / background",
        light: "#f3f4f7",
        dark: "#0d0f15",
    },
    Colour {
        token: "card",
        light: "#ffffff",
        dark: "#14171e",
    },
    Colour {
        token: "border",
        light: "#dfe1e5",
        dark: "#2f333a",
    },
    Colour {
        token: "foreground",
        light: "#15181f",
        dark: "#f0f2f5",
    },
    Colour {
        token: "muted-foreground",
        light: "#5e636f",
        dark: "#a0a5ae",
    },
    Colour {
        token: "brand",
        light: "#0d9b76",
        dark: "#0d9b76",
    },
    Colour {
        token: "brand-strong",
        light: "#086650",
        dark: "#98e8c9",
    },
    Colour {
        token: "brand-soft",
        light: "#dcefe6",
        dark: "#0e2d25",
    },
];

/// After the preheader's text: characters a client shows as nothing, so the preview does not
/// go on with the text of the layout (the heading, the instructions).
const PREHEADER_FILLER: &str = "&#847;&zwnj;&nbsp;";

/// How many times [`PREHEADER_FILLER`] follows the preheader: more than any preview shows.
const PREHEADER_FILLER_COUNT: usize = 90;

/// The HTML document. `{code}`, `{minutes}` and `{filler}` are filled in by [`fill`]; nothing
/// else in it is a placeholder.
const HTML_TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="zh-Hant">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark">
<meta name="supported-color-schemes" content="light dark">
<meta name="format-detection" content="telephone=no, date=no, address=no, email=no">
<title>TrustIoT 登入驗證碼 / Login code</title>
<style>
:root { color-scheme: light dark; supported-color-schemes: light dark; }
body { margin: 0; padding: 0; -webkit-text-size-adjust: 100%; }
@media (prefers-color-scheme: dark) {
  .ti-page { background-color: #0d0f15 !important; }
  .ti-card { background-color: #14171e !important; border-color: #2f333a !important; border-top-color: #0d9b76 !important; }
  .ti-text { color: #f0f2f5 !important; }
  .ti-muted { color: #a0a5ae !important; }
  .ti-brand { color: #98e8c9 !important; }
  .ti-code-box { background-color: #0e2d25 !important; border-color: #2f333a !important; }
  .ti-code { color: #98e8c9 !important; }
}
@media (max-width: 480px) {
  .ti-card-pad { padding: 28px 20px !important; }
  .ti-code { font-size: 32px !important; letter-spacing: 6px !important; padding-left: 6px !important; }
}
</style>
</head>
<body class="ti-page" style="margin:0;padding:0;background-color:#f3f4f7;">
<div style="display:none;font-size:1px;line-height:1px;max-height:0;max-width:0;opacity:0;overflow:hidden;mso-hide:all;">{code} 是您的 TrustIoT 登入驗證碼 · Your TrustIoT login code is {code}{filler}</div>
<table role="presentation" class="ti-page" width="100%" cellpadding="0" cellspacing="0" border="0" style="width:100%;background-color:#f3f4f7;">
<tr>
<td align="center" style="padding:32px 16px;">
<!--[if mso]><table role="presentation" width="600" cellpadding="0" cellspacing="0" border="0"><tr><td><![endif]-->
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" style="width:100%;max-width:600px;">
<tr>
<td class="ti-brand" style="padding:0 4px 16px 4px;font-family:system-ui,-apple-system,'Segoe UI','Noto Sans TC','PingFang TC','Microsoft JhengHei',sans-serif;font-size:20px;line-height:28px;font-weight:600;color:#086650;">TrustIoT</td>
</tr>
<tr>
<td class="ti-card" style="background-color:#ffffff;border:1px solid #dfe1e5;border-top:4px solid #0d9b76;border-radius:8px;">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" style="width:100%;">
<tr>
<td class="ti-card-pad" style="padding:36px 40px;font-family:system-ui,-apple-system,'Segoe UI','Noto Sans TC','PingFang TC','Microsoft JhengHei',sans-serif;">
<h1 class="ti-text" style="margin:0;font-size:18px;line-height:28px;font-weight:600;color:#15181f;">您的登入驗證碼</h1>
<p class="ti-muted" lang="en" style="margin:0 0 24px 0;font-size:14px;line-height:20px;color:#5e636f;">Your login code</p>
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" style="width:100%;">
<tr>
<td class="ti-code-box" align="center" style="background-color:#dcefe6;border:1px solid #dfe1e5;border-radius:8px;padding:20px 12px;">
<span class="ti-code" style="display:inline-block;font-family:ui-monospace,SFMono-Regular,Menlo,Monaco,Consolas,'Liberation Mono','Courier New',monospace;font-size:40px;line-height:48px;font-weight:600;letter-spacing:10px;padding-left:10px;color:#086650;">{code}</span>
</td>
</tr>
</table>
<p class="ti-text" style="margin:24px 0 0 0;font-size:16px;line-height:24px;color:#15181f;">驗證碼 {minutes} 分鐘內有效，只能使用一次。如果您沒有要求登入，請忽略這封信，不要把驗證碼告訴任何人。</p>
<p class="ti-text" lang="en" style="margin:16px 0 0 0;font-size:16px;line-height:24px;color:#15181f;">It is valid for {minutes} minutes and works once. If you did not ask to log in, ignore this email and do not share the code with anyone.</p>
</td>
</tr>
</table>
</td>
</tr>
<tr>
<td class="ti-muted" style="padding:16px 4px 0 4px;font-family:system-ui,-apple-system,'Segoe UI','Noto Sans TC','PingFang TC','Microsoft JhengHei',sans-serif;font-size:12px;line-height:18px;color:#5e636f;">這封信由 TrustIoT 自動寄出。<br><span lang="en">This email was sent automatically by TrustIoT.</span></td>
</tr>
</table>
<!--[if mso]></td></tr></table><![endif]-->
</td>
</tr>
</table>
</body>
</html>
"##;

/// The email with `code` for `email`.
#[must_use]
pub fn code_message(email: &EmailAddress, code: &LoginCode) -> Message {
    Message::new(
        email.as_str().to_owned(),
        SUBJECT.to_owned(),
        text(code),
        html(code),
    )
}

/// The minutes a code is valid, as text.
fn minutes() -> String {
    (CODE_LIFETIME.as_secs() / 60).to_string()
}

/// The plain-text body. Its first line starts with the code: some clients build the preview
/// from the plain-text part.
fn text(code: &LoginCode) -> Zeroizing<String> {
    let code = code.as_str();
    let minutes = minutes();
    let pieces = [
        code,
        " 是您的 TrustIoT 登入驗證碼 · Your TrustIoT login code is ",
        code,
        "\n\n驗證碼 ",
        &minutes,
        " 分鐘內有效，只能使用一次。如果您沒有要求登入，請忽略這封信，\n\
         不要把驗證碼告訴任何人。\n\
         \n\
         It is valid for ",
        &minutes,
        " minutes and works once. If you did not ask to log in,\n\
         ignore this email and do not share the code with anyone.\n",
    ];
    // Exactly sized, so the code is never in a reallocated (and unwiped) buffer.
    let mut out = Zeroizing::new(String::with_capacity(
        pieces.iter().map(|piece| piece.len()).sum(),
    ));
    let capacity = out.capacity();
    for piece in pieces {
        out.push_str(piece);
    }
    debug_assert_eq!(
        out.capacity(),
        capacity,
        "the plain-text buffer was reallocated"
    );
    out
}

/// The HTML body.
fn html(code: &LoginCode) -> Zeroizing<String> {
    let minutes = minutes();
    let filler = PREHEADER_FILLER.repeat(PREHEADER_FILLER_COUNT);
    fill(
        HTML_TEMPLATE,
        &[
            ("{code}", Value::Text(code.as_str())),
            ("{minutes}", Value::Text(&minutes)),
            ("{filler}", Value::Markup(&filler)),
        ],
    )
}

/// A value for a placeholder of the template.
enum Value<'a> {
    /// Text, escaped on the way in.
    Text(&'a str),
    /// Markup written by this module, inserted as it is.
    Markup(&'a str),
}

/// The longest escape of one byte ([`push_escaped`]): `&quot;`.
const MAX_ESCAPE: usize = 6;

/// `template` with each placeholder replaced by its value, in a buffer sized for the worst case
/// first, so the code is never in a reallocated (and unwiped) buffer.
fn fill(template: &str, values: &[(&str, Value<'_>)]) -> Zeroizing<String> {
    let extra: usize = values
        .iter()
        .map(|(name, value)| {
            let each = match value {
                Value::Text(text) => text.len() * MAX_ESCAPE,
                Value::Markup(markup) => markup.len(),
            };
            template.matches(name).count() * each
        })
        .sum();
    let mut out = Zeroizing::new(String::with_capacity(template.len() + extra));
    let capacity = out.capacity();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(rest.get(..start).unwrap_or_default());
        let at = rest.get(start..).unwrap_or_default();
        match values.iter().find(|(name, _)| at.starts_with(name)) {
            Some((name, value)) => {
                match value {
                    Value::Text(text) => push_escaped(&mut out, text),
                    Value::Markup(markup) => out.push_str(markup),
                }
                rest = at.get(name.len()..).unwrap_or_default();
            }
            None => {
                out.push('{');
                rest = at.get(1..).unwrap_or_default();
            }
        }
    }
    out.push_str(rest);
    debug_assert_eq!(out.capacity(), capacity, "the HTML buffer was reallocated");
    out
}

/// Appends `text` with the five characters that mean something in HTML escaped, so a value is
/// text in an element or an attribute, never markup.
fn push_escaped(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = "908172";

    fn message() -> Message {
        let email = EmailAddress::parse("user@example.com").expect("an address");
        let code = LoginCode::parse(CODE).expect("a code");
        code_message(&email, &code)
    }

    /// Writes the HTML and the plain text of a sample email (code 908172) next to the path that
    /// `LOGIN_EMAIL_SAMPLE` names (`<path>.html`, `<path>.txt`), for the render screenshots of
    /// docs/test-reports/2026-11-07-1142-login-email.md. Does nothing without it.
    #[test]
    fn write_a_sample_when_asked() {
        let Some(path) = std::env::var_os("LOGIN_EMAIL_SAMPLE") else {
            return;
        };
        let message = message();
        let path = std::path::PathBuf::from(path);
        std::fs::write(path.with_extension("html"), message.html()).expect("write the HTML");
        std::fs::write(path.with_extension("txt"), message.text()).expect("write the text");
    }

    #[test]
    fn both_bodies_hold_the_code_and_the_subject_does_not() {
        let message = message();
        assert_eq!(message.to(), "user@example.com");
        assert_eq!(message.subject(), SUBJECT);
        assert!(!message.subject().chars().any(|c| c.is_ascii_digit()));
        // The preview line and the large code (the preheader holds it twice).
        assert_eq!(
            message.text().matches(CODE).count(),
            2,
            "{}",
            message.text()
        );
        assert_eq!(
            message.html().matches(CODE).count(),
            3,
            "{}",
            message.html()
        );
        // As one run of digits: letter-spacing spreads them, no character is inserted.
        assert!(
            message
                .html()
                .contains(&format!("monospace;font-size:40px;line-height:48px;font-weight:600;letter-spacing:10px;padding-left:10px;color:#086650;\">{CODE}</span>")),
            "{}",
            message.html()
        );
        assert_eq!(format!("{message:?}"), "Message(<redacted>)");
    }

    #[test]
    fn the_preview_starts_with_the_code() {
        let message = message();
        let first_line = message.text().lines().next().expect("a first line");
        assert!(first_line.starts_with(CODE), "{first_line}");

        let html = message.html();
        let (_, body) = html.split_once("<body").expect("a body");
        let (_, after_body_tag) = body.split_once('>').expect("the body tag");
        let preheader = after_body_tag
            .trim_start()
            .strip_prefix("<div style=\"display:none;")
            .expect("a hidden preheader first in the body");
        let (style_rest, text) = preheader.split_once("\">").expect("the preheader's text");
        for hidden in [
            "max-height:0",
            "opacity:0",
            "overflow:hidden",
            "mso-hide:all",
        ] {
            assert!(style_rest.contains(hidden), "{hidden}");
        }
        assert!(
            text.starts_with(&format!(
                "{CODE} 是您的 TrustIoT 登入驗證碼 · Your TrustIoT login code is {CODE}{PREHEADER_FILLER}"
            )),
            "{text}"
        );
        // The preheader comes before anything visible.
        let (before_layout, _) = after_body_tag.split_once("<table").expect("the layout");
        assert!(before_layout.contains("display:none"));
        assert_eq!(before_layout.matches("<div").count(), 1);
    }

    #[test]
    fn the_html_loads_nothing_and_links_nowhere() {
        let html = message().html().to_ascii_lowercase();
        for forbidden in [
            "http",
            "src=",
            "<a ",
            "<script",
            "url(",
            "href",
            "@import",
            "<img",
            "<link",
            "<iframe",
            "<form",
            "<object",
            "<embed",
            "srcset",
            "javascript:",
        ] {
            assert!(!html.contains(forbidden), "{forbidden}");
        }
        assert!(html.starts_with("<!doctype html>\n"));
        assert!(html.trim_end().ends_with("</html>"));
        for placeholder in ["{code}", "{minutes}", "{filler}"] {
            assert!(!html.contains(placeholder), "{placeholder}");
        }
        assert!(html.contains("10 分鐘內有效"));
        assert!(html.contains("valid for 10 minutes"));
        assert!(html.contains("max-width:600px"));
    }

    #[test]
    fn inserted_values_are_escaped() {
        let mut out = String::new();
        push_escaped(&mut out, "<a href=\"x\" onclick='y'>&amp;</a>");
        assert_eq!(
            out,
            "&lt;a href=&quot;x&quot; onclick=&#39;y&#39;&gt;&amp;amp;&lt;/a&gt;"
        );
        let filled = fill(
            "<p title=\"{v}\">{v}</p>{m}{",
            &[
                ("{v}", Value::Text("\"><script>")),
                ("{m}", Value::Markup("<br>")),
            ],
        );
        assert_eq!(
            filled.as_str(),
            "<p title=\"&quot;&gt;&lt;script&gt;\">&quot;&gt;&lt;script&gt;</p><br>{"
        );
    }

    /// WCAG 2 relative luminance of `#rrggbb`.
    fn luminance(hex: &str) -> f64 {
        let channel = |range: std::ops::Range<usize>| {
            let value =
                u8::from_str_radix(hex.get(range).expect("two hex digits"), 16).expect("hex");
            let c = f64::from(value) / 255.0;
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(1..3) + 0.7152 * channel(3..5) + 0.0722 * channel(5..7)
    }

    fn contrast(a: &str, b: &str) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    fn colour(token: &str) -> Colour {
        *PALETTE
            .iter()
            .find(|colour| colour.token == token)
            .unwrap_or_else(|| panic!("{token} is in the palette"))
    }

    /// docs/design/system/README.md, "色彩": text 4.5:1 (the large code and the wordmark too,
    /// though 3:1 would do for them), graphics such as the brand bar 3:1; in both themes.
    #[test]
    fn the_colours_meet_the_design_systems_contrast_rules() {
        let pairs = [
            ("foreground", "card", 4.5),
            ("muted-foreground", "card", 4.5),
            ("muted-foreground", "muted / background", 4.5),
            ("brand-strong", "muted / background", 4.5),
            ("brand-strong", "brand-soft", 4.5),
            ("brand", "card", 3.0),
        ];
        for (front, back, minimum) in pairs {
            let (front, back) = (colour(front), colour(back));
            for (theme, a, b) in [
                ("light", front.light, back.light),
                ("dark", front.dark, back.dark),
            ] {
                let ratio = contrast(a, b);
                assert!(
                    ratio >= minimum,
                    "{} on {} in {theme}: {ratio:.2}:1",
                    front.token,
                    back.token
                );
            }
        }
    }

    #[test]
    fn the_html_uses_only_palette_colours_and_every_dark_colour_is_set() {
        let html = message().html().to_ascii_lowercase();
        let known: Vec<&str> = PALETTE
            .iter()
            .flat_map(|colour| [colour.light, colour.dark])
            .collect();
        let mut used = Vec::new();
        for (index, _) in html.match_indices('#') {
            let candidate = html.get(index..index + 7).unwrap_or_default();
            if candidate.len() == 7 && candidate.bytes().skip(1).all(|b| b.is_ascii_hexdigit()) {
                assert!(
                    known.contains(&candidate),
                    "{candidate} is not a palette colour"
                );
                used.push(candidate);
            }
        }
        for colour in PALETTE {
            assert!(used.contains(&colour.light), "{} light", colour.token);
            assert!(used.contains(&colour.dark), "{} dark", colour.token);
        }
    }
}
