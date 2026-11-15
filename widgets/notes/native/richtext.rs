//! RTF rich-text parser, multi-run word wrapper, and card text renderer.
//! Ensures formatting (bold, italic, underline, strikeout, fonts, sizes, colors)
//! and lists (bullets/numbering) saved in Note::rtf persist when exiting input mode.
use super::*;
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub text: String,
    pub family: Option<String>,
    pub size_dip: Option<f32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub color: Option<u32>,
}

impl Run {
    fn same_style(&self, other: &Self) -> bool {
        self.family == other.family
            && self.size_dip == other.size_dip
            && self.bold == other.bold
            && self.italic == other.italic
            && self.underline == other.underline
            && self.strike == other.strike
            && self.color == other.color
    }

    pub fn style_bits(&self) -> u8 {
        u8::from(self.bold)
            | (u8::from(self.italic) << 1)
            | (u8::from(self.underline) << 2)
            | (u8::from(self.strike) << 3)
    }

    fn effective_size(&self, base_body: f32) -> f32 {
        self.size_dip.unwrap_or(base_body)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Paragraph {
    pub prefix: Option<String>,
    pub runs: Vec<Run>,
}

#[derive(Clone, Debug)]
pub struct RichLine {
    pub indent: f32,
    pub runs: Vec<Run>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dest {
    Normal,
    FontTbl,
    ColorTbl,
    Pn,
    PnText,
    Ignore,
}

#[derive(Clone)]
struct GroupState {
    dest: Dest,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    font_idx: i32,
    half_pts: i32,
    color_idx: usize,
    uc: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Bullet,
    Numbered,
}

fn cp1252_char(byte: u8) -> char {
    match byte {
        0x80 => '€',
        0x82 => '‚',
        0x83 => 'ƒ',
        0x84 => '„',
        0x85 => '…',
        0x86 => '†',
        0x87 => '‡',
        0x88 => 'ˆ',
        0x89 => '‰',
        0x8A => 'Š',
        0x8B => '‹',
        0x8C => 'Œ',
        0x8E => 'Ž',
        0x91 => '‘',
        0x92 => '’',
        0x93 => '“',
        0x94 => '”',
        0x95 => '•',
        0x96 => '–',
        0x97 => '—',
        0x98 => '˜',
        0x99 => '™',
        0x9A => 'š',
        0x9B => '›',
        0x9C => 'œ',
        0x9E => 'ž',
        0x9F => 'Ÿ',
        _ => byte as char,
    }
}

fn is_default_font_name(name: &str) -> bool {
    let t = name.trim();
    t.is_empty()
        || t.eq_ignore_ascii_case("Segoe UI Variable Text")
        || t.eq_ignore_ascii_case("Segoe UI")
}

fn is_symbol_font_name(name: &str) -> bool {
    let t = name.trim();
    t.eq_ignore_ascii_case("Symbol")
        || t.eq_ignore_ascii_case("Wingdings")
        || t.eq_ignore_ascii_case("Webdings")
}

fn normalize_pntext(raw: &str, list_kind: Option<ListKind>) -> String {
    let mut cleaned = String::with_capacity(raw.len() + 2);
    for ch in raw.chars() {
        match ch {
            '\t' | '\r' | '\n' => {
                if !cleaned.ends_with(' ') && !cleaned.is_empty() {
                    cleaned.push(' ');
                }
            }
            '\u{00B7}' if list_kind != Some(ListKind::Numbered) => cleaned.push('•'),
            _ => cleaned.push(ch),
        }
    }
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed == "·" || trimmed == "\u{F0B7}" {
        "• ".into()
    } else {
        format!("{trimmed} ")
    }
}

pub fn parse_rtf(rtf: &str, base_body_size: f32) -> Vec<Paragraph> {
    if !rtf.trim_start().starts_with('{') {
        return rtf
            .lines()
            .map(|line| Paragraph {
                prefix: None,
                runs: if line.is_empty() {
                    vec![]
                } else {
                    vec![Run {
                        text: line.into(),
                        family: None,
                        size_dip: None,
                        bold: false,
                        italic: false,
                        underline: false,
                        strike: false,
                        color: None,
                    }]
                },
            })
            .collect();
    }

    let bytes = rtf.as_bytes();
    let mut i = 0usize;
    let mut stack: Vec<GroupState> = Vec::new();
    let mut st = GroupState {
        dest: Dest::Normal,
        bold: false,
        italic: false,
        underline: false,
        strike: false,
        font_idx: 0,
        half_pts: 0,
        color_idx: 0,
        uc: 1,
    };

    let mut fonttbl: HashMap<i32, String> = HashMap::new();
    let mut cur_font_idx: Option<i32> = None;
    let mut cur_font_name = String::new();

    let mut colortbl: Vec<Option<u32>> = Vec::new();
    let mut ct_r: Option<u8> = None;
    let mut ct_g: Option<u8> = None;
    let mut ct_b: Option<u8> = None;
    let mut ct_has_rgb = false;

    let mut paragraphs: Vec<Paragraph> = Vec::new();
    let mut cur_runs: Vec<Run> = Vec::new();
    let mut cur_text = String::new();
    let mut cur_style_snap: Option<Run> = None;

    let mut cur_prefix: Option<String> = None;
    let mut list_kind: Option<ListKind> = None;
    let mut pn_start: usize = 1;
    let mut list_counter: usize = 0;
    let mut prev_list_kind: Option<ListKind> = None;
    let mut pntext_buf = String::new();
    let mut pntext_symbol_font = false;
    let mut utf16_buf: Vec<u16> = Vec::new();

    let make_run = |text: String,
                    s: &GroupState,
                    fonts: &HashMap<i32, String>,
                    colors: &[Option<u32>]|
     -> Run {
        let family = fonts.get(&s.font_idx).and_then(|name| {
            if is_default_font_name(name) || is_symbol_font_name(name) {
                None
            } else {
                Some(name.clone())
            }
        });
        let size_dip = if s.half_pts > 0 && (s.half_pts as f32 - base_body_size * 1.5).abs() > 1.25 {
            Some((s.half_pts as f32 * (2.0 / 3.0)).clamp(9.0, 36.0))
        } else {
            None
        };
        let color = colors.get(s.color_idx).copied().flatten();
        Run {
            text,
            family,
            size_dip,
            bold: s.bold,
            italic: s.italic,
            underline: s.underline,
            strike: s.strike,
            color,
        }
    };

    let flush_utf16 = |utf16: &mut Vec<u16>, dest: Dest, out_text: &mut String, out_pn: &mut String| {
        if utf16.is_empty() {
            return;
        }
        for decoded in char::decode_utf16(utf16.drain(..)) {
            let ch = decoded.unwrap_or('\u{FFFD}');
            match dest {
                Dest::Normal => out_text.push(ch),
                Dest::PnText => out_pn.push(ch),
                _ => {}
            }
        }
    };

    let flush_run = |text: &mut String,
                     snap: &mut Option<Run>,
                     runs: &mut Vec<Run>,
                     s: &GroupState,
                     fonts: &HashMap<i32, String>,
                     colors: &[Option<u32>]| {
        if text.is_empty() {
            return;
        }
        let t = std::mem::take(text);
        let run = snap.take().unwrap_or_else(|| make_run(String::new(), s, fonts, colors));
        let mut new_run = run;
        new_run.text = t;
        if let Some(last) = runs.last_mut() {
            if last.same_style(&new_run) {
                last.text.push_str(&new_run.text);
                return;
            }
        }
        runs.push(new_run);
    };

    let push_char = |ch: char,
                     s: &GroupState,
                     cur_text: &mut String,
                     snap: &mut Option<Run>,
                     runs: &mut Vec<Run>,
                     cur_font_name: &mut String,
                     pntext_buf: &mut String,
                     fonts: &mut HashMap<i32, String>,
                     cur_font_idx: &mut Option<i32>,
                     colortbl: &mut Vec<Option<u32>>,
                     ct_r: &mut Option<u8>,
                     ct_g: &mut Option<u8>,
                     ct_b: &mut Option<u8>,
                     ct_has_rgb: &mut bool| {
        match s.dest {
            Dest::Normal => {
                let desired = make_run(String::new(), s, fonts, colortbl);
                if snap.as_ref().is_some_and(|prev| !prev.same_style(&desired)) {
                    flush_run(cur_text, snap, runs, s, fonts, colortbl);
                }
                if snap.is_none() {
                    *snap = Some(desired);
                }
                cur_text.push(ch);
            }
            Dest::FontTbl => {
                if ch == ';' {
                    if let Some(idx) = cur_font_idx.take() {
                        let name = cur_font_name.trim().to_string();
                        if !name.is_empty() {
                            fonts.insert(idx, name);
                        }
                    }
                    cur_font_name.clear();
                } else {
                    cur_font_name.push(ch);
                }
            }
            Dest::ColorTbl => {
                if ch == ';' {
                    let color = if *ct_has_rgb {
                        let r = ct_r.unwrap_or(0);
                        let g = ct_g.unwrap_or(0);
                        let b = ct_b.unwrap_or(0);
                        // Auto/black in RichEdit RTF should adopt adaptive card ink.
                        if r < 24 && g < 24 && b < 24 {
                            None
                        } else {
                            Some(super::rgb(r, g, b))
                        }
                    } else {
                        None
                    };
                    colortbl.push(color);
                    *ct_r = None;
                    *ct_g = None;
                    *ct_b = None;
                    *ct_has_rgb = false;
                }
            }
            Dest::PnText => pntext_buf.push(ch),
            Dest::Pn | Dest::Ignore => {}
        }
    };

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'{' => {
                flush_utf16(&mut utf16_buf, st.dest, &mut cur_text, &mut pntext_buf);
                if st.dest == Dest::Normal {
                    flush_run(&mut cur_text, &mut cur_style_snap, &mut cur_runs, &st, &fonttbl, &colortbl);
                }
                stack.push(st.clone());
                if st.dest == Dest::PnText {
                    pntext_buf.clear();
                }
                i += 1;
            }
            b'}' => {
                flush_utf16(&mut utf16_buf, st.dest, &mut cur_text, &mut pntext_buf);
                match st.dest {
                    Dest::Normal => {
                        flush_run(&mut cur_text, &mut cur_style_snap, &mut cur_runs, &st, &fonttbl, &colortbl);
                    }
                    Dest::FontTbl => {
                        if let Some(idx) = cur_font_idx.take() {
                            let name = cur_font_name.trim().trim_end_matches(';').trim();
                            if !name.is_empty() {
                                fonttbl.insert(idx, name.to_string());
                            }
                        }
                        cur_font_name.clear();
                    }
                    Dest::PnText => {
                        let p = normalize_pntext(&pntext_buf, list_kind);
                        if !p.is_empty() {
                            if cur_prefix.is_none() {
                                cur_prefix = Some(p);
                            }
                        } else if pntext_symbol_font && !pntext_buf.is_empty() && cur_prefix.is_none() {
                            cur_prefix = Some("• ".into());
                        }
                        pntext_buf.clear();
                        pntext_symbol_font = false;
                    }
                    _ => {}
                }
                if let Some(prev) = stack.pop() {
                    st = prev;
                }
                i += 1;
            }
            b'\r' | b'\n' => {
                i += 1;
            }
            b'\\' => {
                i += 1;
                if i >= bytes.len() {
                    break;
                }
                let next = bytes[i];
                if !next.is_ascii_alphabetic() {
                    match next {
                        b'*' => {
                            flush_utf16(&mut utf16_buf, st.dest, &mut cur_text, &mut pntext_buf);
                            i += 1;
                            // Peek whether this ignorable group is {\*\pn ...}
                            let rest = &bytes[i..];
                            let mut k = 0;
                            while k < rest.len() && rest[k] == b' ' {
                                k += 1;
                            }
                            if rest[k..].starts_with(b"\\pn")
                                && rest.get(k + 3).is_some_and(|c| !c.is_ascii_alphabetic())
                            {
                                st.dest = Dest::Pn;
                            } else {
                                st.dest = Dest::Ignore;
                            }
                        }
                        b'\'' => {
                            flush_utf16(&mut utf16_buf, st.dest, &mut cur_text, &mut pntext_buf);
                            i += 1;
                            if i + 1 < bytes.len() {
                                if let Ok(hex_str) = std::str::from_utf8(&bytes[i..i + 2]) {
                                    if let Ok(byte_val) = u8::from_str_radix(hex_str, 16) {
                                        i += 2;
                                        let ch = if st.dest == Dest::PnText
                                            && (pntext_symbol_font || byte_val == 0xB7)
                                            && list_kind != Some(ListKind::Numbered)
                                        {
                                            '•'
                                        } else {
                                            cp1252_char(byte_val)
                                        };
                                        push_char(
                                            ch,
                                            &st,
                                            &mut cur_text,
                                            &mut cur_style_snap,
                                            &mut cur_runs,
                                            &mut cur_font_name,
                                            &mut pntext_buf,
                                            &mut fonttbl,
                                            &mut cur_font_idx,
                                            &mut colortbl,
                                            &mut ct_r,
                                            &mut ct_g,
                                            &mut ct_b,
                                            &mut ct_has_rgb,
                                        );
                                        continue;
                                    }
                                }
                            }
                            i += 1;
                        }
                        b'\\' | b'{' | b'}' => {
                            flush_utf16(&mut utf16_buf, st.dest, &mut cur_text, &mut pntext_buf);
                            i += 1;
                            push_char(
                                next as char,
                                &st,
                                &mut cur_text,
                                &mut cur_style_snap,
                                &mut cur_runs,
                                &mut cur_font_name,
                                &mut pntext_buf,
                                &mut fonttbl,
                                &mut cur_font_idx,
                                &mut colortbl,
                                &mut ct_r,
                                &mut ct_g,
                                &mut ct_b,
                                &mut ct_has_rgb,
                            );
                        }
                        b'~' => {
                            flush_utf16(&mut utf16_buf, st.dest, &mut cur_text, &mut pntext_buf);
                            i += 1;
                            push_char(
                                ' ',
                                &st,
                                &mut cur_text,
                                &mut cur_style_snap,
                                &mut cur_runs,
                                &mut cur_font_name,
                                &mut pntext_buf,
                                &mut fonttbl,
                                &mut cur_font_idx,
                                &mut colortbl,
                                &mut ct_r,
                                &mut ct_g,
                                &mut ct_b,
                                &mut ct_has_rgb,
                            );
                        }
                        b'_' => {
                            flush_utf16(&mut utf16_buf, st.dest, &mut cur_text, &mut pntext_buf);
                            i += 1;
                            push_char(
                                '-',
                                &st,
                                &mut cur_text,
                                &mut cur_style_snap,
                                &mut cur_runs,
                                &mut cur_font_name,
                                &mut pntext_buf,
                                &mut fonttbl,
                                &mut cur_font_idx,
                                &mut colortbl,
                                &mut ct_r,
                                &mut ct_g,
                                &mut ct_b,
                                &mut ct_has_rgb,
                            );
                        }
                        b'-' => {
                            flush_utf16(&mut utf16_buf, st.dest, &mut cur_text, &mut pntext_buf);
                            i += 1;
                        }
                        b'\r' | b'\n' => {
                            flush_utf16(&mut utf16_buf, st.dest, &mut cur_text, &mut pntext_buf);
                            i += 1;
                            if st.dest == Dest::Normal {
                                flush_run(&mut cur_text, &mut cur_style_snap, &mut cur_runs, &st, &fonttbl, &colortbl);
                                paragraphs.push(finalize_paragraph(
                                    &mut cur_prefix,
                                    &mut list_kind,
                                    &mut prev_list_kind,
                                    &mut list_counter,
                                    pn_start,
                                    &mut cur_runs,
                                ));
                                pn_start = 1;
                            }
                        }
                        _ => {
                            flush_utf16(&mut utf16_buf, st.dest, &mut cur_text, &mut pntext_buf);
                            i += 1;
                        }
                    }
                    continue;
                }

                let word_start = i;
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let word = &rtf[word_start..i];
                let mut param: Option<i32> = None;
                if i < bytes.len() && (bytes[i] == b'-' || bytes[i].is_ascii_digit()) {
                    let p_start = i;
                    if bytes[i] == b'-' {
                        i += 1;
                    }
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    if let Ok(v) = rtf[p_start..i].parse::<i32>() {
                        param = Some(v);
                    }
                }
                if i < bytes.len() && bytes[i] == b' ' {
                    i += 1;
                }

                if word == "u" {
                    if let Some(v) = param {
                        let unit = v as i16 as u16;
                        utf16_buf.push(unit);
                        // Skip \uc replacement ANSI characters (including \'hh hex escape as 1 char).
                        let mut skip = st.uc;
                        while skip > 0 && i < bytes.len() {
                            if bytes[i] == b'{' || bytes[i] == b'}' {
                                break;
                            }
                            if bytes[i] == b'\\' {
                                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                                    i = (i + 4).min(bytes.len());
                                    skip -= 1;
                                    continue;
                                }
                                break;
                            }
                            if bytes[i] == b'\r' || bytes[i] == b'\n' {
                                i += 1;
                                continue;
                            }
                            i += 1;
                            skip -= 1;
                        }
                        continue;
                    }
                }

                flush_utf16(&mut utf16_buf, st.dest, &mut cur_text, &mut pntext_buf);

                match word {
                    "fonttbl" => st.dest = Dest::FontTbl,
                    "colortbl" => st.dest = Dest::ColorTbl,
                    "stylesheet" | "info" | "pict" | "object" | "header" | "footer"
                    | "headerl" | "headerr" | "headerf" | "footerl" | "footerr" | "footerf"
                    | "listtable" | "listoverridetable" => st.dest = Dest::Ignore,
                    "pntext" | "listtext" if st.dest == Dest::Normal => {
                        st.dest = Dest::PnText;
                        pntext_buf.clear();
                        pntext_symbol_font = false;
                    }
                    "f" => {
                        if let Some(idx) = param {
                            match st.dest {
                                Dest::FontTbl => {
                                    cur_font_idx = Some(idx);
                                    cur_font_name.clear();
                                }
                                Dest::PnText => {
                                    if fonttbl.get(&idx).is_some_and(|n| is_symbol_font_name(n)) {
                                        pntext_symbol_font = true;
                                    }
                                }
                                Dest::Normal => {
                                    if cur_text.is_empty() {
                                        st.font_idx = idx;
                                    } else {
                                        flush_run(
                                            &mut cur_text,
                                            &mut cur_style_snap,
                                            &mut cur_runs,
                                            &st,
                                            &fonttbl,
                                            &colortbl,
                                        );
                                        st.font_idx = idx;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "red" if st.dest == Dest::ColorTbl => {
                        ct_r = param.map(|v| v.clamp(0, 255) as u8);
                        ct_has_rgb = true;
                    }
                    "green" if st.dest == Dest::ColorTbl => {
                        ct_g = param.map(|v| v.clamp(0, 255) as u8);
                        ct_has_rgb = true;
                    }
                    "blue" if st.dest == Dest::ColorTbl => {
                        ct_b = param.map(|v| v.clamp(0, 255) as u8);
                        ct_has_rgb = true;
                    }
                    "pnlvlblt" if st.dest == Dest::Pn => list_kind = Some(ListKind::Bullet),
                    "pnlvlbody" | "pnlvl1" | "pnlvl2" | "pnlvl3" | "pnlvl4"
                        if st.dest == Dest::Pn =>
                    {
                        list_kind = Some(ListKind::Numbered);
                    }
                    "pnstart" if st.dest == Dest::Pn => {
                        if let Some(v) = param.filter(|v| *v > 0) {
                            pn_start = v as usize;
                        }
                    }
                    _ if st.dest == Dest::FontTbl || st.dest == Dest::ColorTbl
                        || st.dest == Dest::Ignore || st.dest == Dest::Pn => {}
                    "uc" => {
                        if let Some(v) = param {
                            st.uc = v.max(0) as usize;
                        }
                    }
                    "pard" if st.dest == Dest::Normal => {
                        cur_prefix = None;
                        list_kind = None;
                    }
                    "plain" if st.dest == Dest::Normal => {
                        flush_run(
                            &mut cur_text,
                            &mut cur_style_snap,
                            &mut cur_runs,
                            &st,
                            &fonttbl,
                            &colortbl,
                        );
                        st.bold = false;
                        st.italic = false;
                        st.underline = false;
                        st.strike = false;
                        st.font_idx = 0;
                        st.half_pts = 0;
                        st.color_idx = 0;
                    }
                    "b" if st.dest == Dest::Normal => {
                        flush_run(
                            &mut cur_text,
                            &mut cur_style_snap,
                            &mut cur_runs,
                            &st,
                            &fonttbl,
                            &colortbl,
                        );
                        st.bold = param != Some(0);
                    }
                    "i" if st.dest == Dest::Normal => {
                        flush_run(
                            &mut cur_text,
                            &mut cur_style_snap,
                            &mut cur_runs,
                            &st,
                            &fonttbl,
                            &colortbl,
                        );
                        st.italic = param != Some(0);
                    }
                    "ul" | "uld" | "uldb" | "ulth" | "ulwave" if st.dest == Dest::Normal => {
                        flush_run(
                            &mut cur_text,
                            &mut cur_style_snap,
                            &mut cur_runs,
                            &st,
                            &fonttbl,
                            &colortbl,
                        );
                        st.underline = param != Some(0);
                    }
                    "ulnone" | "ul0" if st.dest == Dest::Normal => {
                        flush_run(
                            &mut cur_text,
                            &mut cur_style_snap,
                            &mut cur_runs,
                            &st,
                            &fonttbl,
                            &colortbl,
                        );
                        st.underline = false;
                    }
                    "strike" | "striked" if st.dest == Dest::Normal => {
                        flush_run(
                            &mut cur_text,
                            &mut cur_style_snap,
                            &mut cur_runs,
                            &st,
                            &fonttbl,
                            &colortbl,
                        );
                        st.strike = param != Some(0);
                    }
                    "fs" if st.dest == Dest::Normal => {
                        if let Some(v) = param {
                            flush_run(
                                &mut cur_text,
                                &mut cur_style_snap,
                                &mut cur_runs,
                                &st,
                                &fonttbl,
                                &colortbl,
                            );
                            st.half_pts = v;
                        }
                    }
                    "cf" if st.dest == Dest::Normal => {
                        if let Some(v) = param.filter(|v| *v >= 0) {
                            flush_run(
                                &mut cur_text,
                                &mut cur_style_snap,
                                &mut cur_runs,
                                &st,
                                &fonttbl,
                                &colortbl,
                            );
                            st.color_idx = v as usize;
                        }
                    }
                    "par" if st.dest == Dest::Normal => {
                        flush_run(
                            &mut cur_text,
                            &mut cur_style_snap,
                            &mut cur_runs,
                            &st,
                            &fonttbl,
                            &colortbl,
                        );
                        paragraphs.push(finalize_paragraph(
                            &mut cur_prefix,
                            &mut list_kind,
                            &mut prev_list_kind,
                            &mut list_counter,
                            pn_start,
                            &mut cur_runs,
                        ));
                        pn_start = 1;
                    }
                    "line" if st.dest == Dest::Normal => {
                        push_char(
                            '\n',
                            &st,
                            &mut cur_text,
                            &mut cur_style_snap,
                            &mut cur_runs,
                            &mut cur_font_name,
                            &mut pntext_buf,
                            &mut fonttbl,
                            &mut cur_font_idx,
                            &mut colortbl,
                            &mut ct_r,
                            &mut ct_g,
                            &mut ct_b,
                            &mut ct_has_rgb,
                        );
                    }
                    "tab" => push_char(
                        if st.dest == Dest::PnText { ' ' } else { '\t' },
                        &st,
                        &mut cur_text,
                        &mut cur_style_snap,
                        &mut cur_runs,
                        &mut cur_font_name,
                        &mut pntext_buf,
                        &mut fonttbl,
                        &mut cur_font_idx,
                        &mut colortbl,
                        &mut ct_r,
                        &mut ct_g,
                        &mut ct_b,
                        &mut ct_has_rgb,
                    ),
                    "bullet" => push_char(
                        '•',
                        &st,
                        &mut cur_text,
                        &mut cur_style_snap,
                        &mut cur_runs,
                        &mut cur_font_name,
                        &mut pntext_buf,
                        &mut fonttbl,
                        &mut cur_font_idx,
                        &mut colortbl,
                        &mut ct_r,
                        &mut ct_g,
                        &mut ct_b,
                        &mut ct_has_rgb,
                    ),
                    "emdash" => push_char(
                        '—',
                        &st,
                        &mut cur_text,
                        &mut cur_style_snap,
                        &mut cur_runs,
                        &mut cur_font_name,
                        &mut pntext_buf,
                        &mut fonttbl,
                        &mut cur_font_idx,
                        &mut colortbl,
                        &mut ct_r,
                        &mut ct_g,
                        &mut ct_b,
                        &mut ct_has_rgb,
                    ),
                    "endash" => push_char(
                        '–',
                        &st,
                        &mut cur_text,
                        &mut cur_style_snap,
                        &mut cur_runs,
                        &mut cur_font_name,
                        &mut pntext_buf,
                        &mut fonttbl,
                        &mut cur_font_idx,
                        &mut colortbl,
                        &mut ct_r,
                        &mut ct_g,
                        &mut ct_b,
                        &mut ct_has_rgb,
                    ),
                    "lquote" => push_char(
                        '‘',
                        &st,
                        &mut cur_text,
                        &mut cur_style_snap,
                        &mut cur_runs,
                        &mut cur_font_name,
                        &mut pntext_buf,
                        &mut fonttbl,
                        &mut cur_font_idx,
                        &mut colortbl,
                        &mut ct_r,
                        &mut ct_g,
                        &mut ct_b,
                        &mut ct_has_rgb,
                    ),
                    "rquote" => push_char(
                        '’',
                        &st,
                        &mut cur_text,
                        &mut cur_style_snap,
                        &mut cur_runs,
                        &mut cur_font_name,
                        &mut pntext_buf,
                        &mut fonttbl,
                        &mut cur_font_idx,
                        &mut colortbl,
                        &mut ct_r,
                        &mut ct_g,
                        &mut ct_b,
                        &mut ct_has_rgb,
                    ),
                    "ldblquote" => push_char(
                        '“',
                        &st,
                        &mut cur_text,
                        &mut cur_style_snap,
                        &mut cur_runs,
                        &mut cur_font_name,
                        &mut pntext_buf,
                        &mut fonttbl,
                        &mut cur_font_idx,
                        &mut colortbl,
                        &mut ct_r,
                        &mut ct_g,
                        &mut ct_b,
                        &mut ct_has_rgb,
                    ),
                    "rdblquote" => push_char(
                        '”',
                        &st,
                        &mut cur_text,
                        &mut cur_style_snap,
                        &mut cur_runs,
                        &mut cur_font_name,
                        &mut pntext_buf,
                        &mut fonttbl,
                        &mut cur_font_idx,
                        &mut colortbl,
                        &mut ct_r,
                        &mut ct_g,
                        &mut ct_b,
                        &mut ct_has_rgb,
                    ),
                    _ => {}
                }
            }
            _ => {
                flush_utf16(&mut utf16_buf, st.dest, &mut cur_text, &mut pntext_buf);
                let rest = &rtf[i..];
                let ch = rest.chars().next().unwrap();
                i += ch.len_utf8();
                if st.dest == Dest::FontTbl && ch == ';' {
                    if let Some(idx) = cur_font_idx.take() {
                        let name = cur_font_name.trim().to_string();
                        if !name.is_empty() {
                            fonttbl.insert(idx, name);
                        }
                    }
                    cur_font_name.clear();
                    continue;
                }
                if st.dest == Dest::FontTbl {
                    cur_font_name.push(ch);
                    continue;
                }
                push_char(
                    if ch == '\t' { ' ' } else { ch },
                    &st,
                    &mut cur_text,
                    &mut cur_style_snap,
                    &mut cur_runs,
                    &mut cur_font_name,
                    &mut pntext_buf,
                    &mut fonttbl,
                    &mut cur_font_idx,
                    &mut colortbl,
                    &mut ct_r,
                    &mut ct_g,
                    &mut ct_b,
                    &mut ct_has_rgb,
                );
            }
        }
    }

    flush_utf16(&mut utf16_buf, st.dest, &mut cur_text, &mut pntext_buf);
    flush_run(&mut cur_text, &mut cur_style_snap, &mut cur_runs, &st, &fonttbl, &colortbl);
    if !cur_runs.is_empty() || cur_prefix.is_some() || paragraphs.is_empty() {
        paragraphs.push(finalize_paragraph(
            &mut cur_prefix,
            &mut list_kind,
            &mut prev_list_kind,
            &mut list_counter,
            pn_start,
            &mut cur_runs,
        ));
    }
    paragraphs
}

fn finalize_paragraph(
    cur_prefix: &mut Option<String>,
    list_kind: &mut Option<ListKind>,
    prev_list_kind: &mut Option<ListKind>,
    list_counter: &mut usize,
    pn_start: usize,
    cur_runs: &mut Vec<Run>,
) -> Paragraph {
    let mut prefix = cur_prefix.take();
    let kind = *list_kind;
    if kind == Some(ListKind::Numbered) {
        if *prev_list_kind == Some(ListKind::Numbered) {
            *list_counter += 1;
        } else {
            *list_counter = pn_start.max(1);
        }
        if prefix.is_none() {
            prefix = Some(format!("{}. ", *list_counter));
        }
    } else {
        *list_counter = 0;
        if kind == Some(ListKind::Bullet) && prefix.is_none() {
            prefix = Some("• ".into());
        }
    }
    *prev_list_kind = kind;
    Paragraph {
        prefix,
        runs: std::mem::take(cur_runs),
    }
}

pub fn note_paragraphs(note: &model::Note, base_body_size: f32) -> (bool, Vec<Paragraph>) {
    let rtf = note.rich_text();
    let paragraphs = parse_rtf(&rtf, base_body_size);
    let has_content = paragraphs
        .iter()
        .any(|p| p.prefix.is_some() || p.runs.iter().any(|r| !r.text.trim().is_empty()));
    (!has_content, paragraphs)
}

fn push_slice(runs: &mut Vec<Run>, style: &Run, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = runs.last_mut() {
        if last.same_style(style) {
            last.text.push_str(text);
            return;
        }
    }
    let mut r = style.clone();
    r.text = text.to_string();
    runs.push(r);
}

#[derive(Clone)]
struct Slice {
    style: Run,
    text: String,
    width: f32,
}

fn split_soft_lines(runs: &[Run]) -> Vec<Vec<Run>> {
    let mut lines: Vec<Vec<Run>> = vec![vec![]];
    for run in runs {
        let mut first = true;
        for part in run.text.split('\n') {
            if !first {
                lines.push(vec![]);
            }
            first = false;
            if !part.is_empty() {
                push_slice(lines.last_mut().unwrap(), run, part);
            }
        }
    }
    lines
}

fn tokenize_line(runs: &[Run], measure: &mut impl FnMut(&Run, &str) -> f32) -> Vec<(bool, Vec<Slice>, f32)> {
    // Returns (is_space, slices, total_width) tokens across runs so word formatting splits don't break words.
    let mut tokens: Vec<(bool, Vec<Slice>, f32)> = Vec::new();
    for run in runs {
        let mut idx = 0usize;
        let s = &run.text;
        while idx < s.len() {
            let ws = s[idx..].chars().next().unwrap().is_whitespace();
            let mut end = idx;
            while end < s.len() {
                let ch = s[end..].chars().next().unwrap();
                if ch.is_whitespace() != ws {
                    break;
                }
                end += ch.len_utf8();
            }
            let chunk = if ws { " " } else { &s[idx..end] };
            let w = measure(run, chunk);
            let slice = Slice {
                style: run.clone(),
                text: chunk.to_string(),
                width: w,
            };
            if let Some((prev_ws, slices, total_w)) = tokens.last_mut() {
                if *prev_ws == ws {
                    if !ws {
                        slices.push(slice);
                        *total_w += w;
                    }
                    idx = end;
                    continue;
                }
            }
            tokens.push((ws, vec![slice], w));
            idx = end;
        }
    }
    tokens
}

pub fn wrap_paragraphs(
    paragraphs: &[Paragraph],
    width: f32,
    base_body: f32,
    measure: &mut impl FnMut(&Run, &str) -> f32,
) -> Vec<RichLine> {
    let avail = (width - 3.).max(base_body);
    let mut out = Vec::new();

    for para in paragraphs {
        let sub_lines = split_soft_lines(&para.runs);
        let pfx_style = para.runs.first().cloned().unwrap_or(Run {
            text: String::new(),
            family: None,
            size_dip: None,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            color: None,
        });
        let pfx_run_template = Run {
            underline: false,
            strike: false,
            ..pfx_style
        };
        let indent = if let Some(pfx) = &para.prefix {
            measure(&pfx_run_template, pfx).min(avail * 0.25)
        } else {
            0.
        };

        for (sub_idx, runs) in sub_lines.iter().enumerate() {
            let first_sub_with_prefix = sub_idx == 0 && para.prefix.is_some();
            let mut line_runs: Vec<Run> = Vec::new();
            let mut cur_indent = if first_sub_with_prefix { 0. } else { indent };
            let mut cur_w = if let (true, Some(pfx)) = (first_sub_with_prefix, &para.prefix) {
                push_slice(&mut line_runs, &pfx_run_template, pfx);
                measure(&pfx_run_template, pfx)
            } else {
                0.
            };
            let mut line_has_word = false;
            let tokens = tokenize_line(runs, measure);
            let mut pending_space: Option<Slice> = None;

            for (is_ws, slices, tok_w) in tokens {
                if is_ws {
                    if line_has_word {
                        pending_space = slices.into_iter().next();
                    }
                    continue;
                }
                let space_w = pending_space.as_ref().map_or(0., |s| s.width);
                let line_avail = (avail - cur_indent).max(base_body);
                if cur_w + space_w + tok_w <= line_avail {
                    if let Some(sp) = pending_space.take() {
                        push_slice(&mut line_runs, &sp.style, &sp.text);
                        cur_w += sp.width;
                    }
                    for sl in slices {
                        push_slice(&mut line_runs, &sl.style, &sl.text);
                        cur_w += sl.width;
                    }
                    line_has_word = true;
                    continue;
                }
                pending_space = None;
                if line_has_word {
                    out.push(RichLine {
                        indent: cur_indent,
                        runs: std::mem::take(&mut line_runs),
                    });
                    cur_indent = indent;
                    cur_w = 0.;
                    line_has_word = false;
                }
                let next_avail = (avail - cur_indent).max(base_body);
                if tok_w <= next_avail {
                    for sl in slices {
                        push_slice(&mut line_runs, &sl.style, &sl.text);
                        cur_w += sl.width;
                    }
                    line_has_word = true;
                } else {
                    // Split oversized token by character across its styled slices.
                    for sl in slices {
                        for ch in sl.text.chars() {
                            let mut ch_buf = [0u8; 4];
                            let ch_s = ch.encode_utf8(&mut ch_buf);
                            let cw = measure(&sl.style, ch_s);
                            if line_has_word && cur_w + cw > (avail - cur_indent).max(base_body) {
                                out.push(RichLine {
                                    indent: cur_indent,
                                    runs: std::mem::take(&mut line_runs),
                                });
                                cur_indent = indent;
                                cur_w = 0.;
                            }
                            push_slice(&mut line_runs, &sl.style, ch_s);
                            cur_w += cw;
                            line_has_word = true;
                        }
                    }
                }
            }
            out.push(RichLine {
                indent: cur_indent,
                runs: line_runs,
            });
        }
    }
    out
}

pub unsafe fn wrap_note(
    note: &model::Note,
    text_w: f32,
    dpi: f32,
    body_size: f32,
    typography: &mut typography::Typography,
) -> (bool, Vec<RichLine>) {
    let (empty, paragraphs) = note_paragraphs(note, body_size);
    if empty {
        if note.images.is_empty() {
            return (
                true,
                vec![RichLine {
                    indent: 0.,
                    runs: vec![Run {
                        text: "Write something…".into(),
                        family: None,
                        size_dip: None,
                        bold: false,
                        italic: false,
                        underline: false,
                        strike: false,
                        color: None,
                    }],
                }],
            );
        }
        return (true, vec![]);
    }
    let lines = wrap_paragraphs(&paragraphs, text_w, body_size, &mut |run, s| {
        typography.measure_styled(
            s,
            run.family.as_deref(),
            dpi,
            run.effective_size(body_size),
            run.bold,
            run.italic,
        )
    });
    (false, lines)
}

pub fn draw_lines(
    c: &Canvas,
    lines: &[RichLine],
    content_x: f32,
    right_limit: f32,
    top_y: f32,
    leading: f32,
    body_size: f32,
    default_color: u32,
) {
    for (i, line) in lines.iter().enumerate() {
        let top = top_y + i as f32 * leading;
        let mut cur_x = content_x + line.indent;
        for run in &line.runs {
            let size = run.effective_size(body_size);
            let style = run.style_bits();
            let color = run.color.unwrap_or(default_color);
            let run_w = c.measure_rich_run(&run.text, run.family.as_deref(), size, style);
            if cur_x < right_limit {
                c.text_rich_run(
                    &run.text,
                    [cur_x, top, (cur_x + run_w + 6.).min(right_limit + 4.), top + leading],
                    run.family.as_deref(),
                    size,
                    style,
                    color,
                    255,
                );
            }
            cur_x += run_w;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rtf_preserves_styles_unicode_fonts_and_lists() {
        let rtf = r#"{\rtf1\ansi\ansicpg1252\deff0{\fonttbl{\f0\fnil\fcharset0 Segoe UI Variable Text;}{\f1\fnil\fcharset0 Consolas;}{\f2\fcharset2 Symbol;}}
{\colortbl ;\red255\green80\blue40;}
\pard\f0\fs21 Excepteur moll\i it anim\i0 m.\par
\pard{\pntext\f2\'B7\tab}{\*\pn\pnlvlblt\pnf2\pnindent0{\pntxtb\'B7}}\b Bold\b0  and \ul under\ulnone  \strike cut\strike0  \f1\cf1 code\par
\pard{\pntext\f0 1.\tab}{\*\pn\pnlvlbody\pnf0\pnindent0\pnstart1\pndec}Ti\u7871?ng Vi\u7879?t \u-10179?\u-8704?\par
}"#;
        let paras = parse_rtf(rtf, 14.);
        assert_eq!(paras.len(), 3);
        assert_eq!(paras[0].prefix, None);
        assert_eq!(paras[0].runs.len(), 3);
        assert_eq!(paras[0].runs[0].text, "Excepteur moll");
        assert!(!paras[0].runs[0].italic);
        assert_eq!(paras[0].runs[1].text, "it anim");
        assert!(paras[0].runs[1].italic);
        assert_eq!(paras[0].runs[2].text, "m.");

        assert_eq!(paras[1].prefix.as_deref(), Some("• "));
        assert!(paras[1].runs[0].bold);
        assert!(paras[1].runs.iter().any(|r| r.underline && r.text == "under"));
        assert!(paras[1].runs.iter().any(|r| r.strike && r.text == "cut"));
        let code = paras[1].runs.last().unwrap();
        assert_eq!(code.family.as_deref(), Some("Consolas"));
        assert!(code.color.is_some());

        assert_eq!(paras[2].prefix.as_deref(), Some("1. "));
        let text: String = paras[2].runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(text, "Tiếng Việt 😀");
    }

    #[test]
    fn wrap_keeps_styled_midword_boundary_together() {
        let para = Paragraph {
            prefix: None,
            runs: vec![
                Run {
                    text: "deserunt moll".into(),
                    family: None,
                    size_dip: None,
                    bold: false,
                    italic: false,
                    underline: false,
                    strike: false,
                    color: None,
                },
                Run {
                    text: "it anim".into(),
                    family: None,
                    size_dip: None,
                    bold: false,
                    italic: true,
                    underline: false,
                    strike: false,
                    color: None,
                },
            ],
        };
        let lines = wrap_paragraphs(&[para], 15., 1., &mut |_r, s| s.chars().count() as f32);
        assert_eq!(lines.len(), 2);
        let first: String = lines[0].runs.iter().map(|r| r.text.as_str()).collect();
        let second: String = lines[1].runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(first, "deserunt");
        assert_eq!(second, "mollit anim");
        assert!(!lines[1].runs[0].italic);
        assert!(lines[1].runs[1].italic);
    }
}
