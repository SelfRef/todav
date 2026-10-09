//! Minimal VCALENDAR/VTODO reader-writer. Keeps every line it does not touch verbatim.

/// One unfolded content line: `NAME;PARAMS:VALUE` (params and value stay raw/escaped).
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub name: String,
    pub params: String,
    pub value: String,
}

impl Line {
    fn parse(s: &str) -> Line {
        let name_end = s.find([';', ':']).unwrap_or(s.len());
        let mut in_quote = false;
        let mut colon = s.len();
        for (i, c) in s[name_end..].char_indices() {
            match c {
                '"' => in_quote = !in_quote,
                ':' if !in_quote => {
                    colon = name_end + i;
                    break;
                }
                _ => {}
            }
        }
        Line {
            name: s[..name_end].to_ascii_uppercase(),
            params: s[name_end..colon].to_string(),
            value: s.get(colon + 1..).unwrap_or("").to_string(),
        }
    }

    pub fn param(&self, key: &str) -> Option<&str> {
        self.params.split(';').skip(1).find_map(|p| {
            let (k, v) = p.split_once('=')?;
            k.eq_ignore_ascii_case(key).then(|| v.trim_matches('"'))
        })
    }

    fn render(&self) -> String {
        format!("{}{}:{}", self.name, self.params, self.value)
    }
}

/// A calendar object holding (at least) one VTODO.
#[derive(Debug, Clone)]
pub struct Calendar {
    lines: Vec<Line>,
}

impl Calendar {
    pub fn parse(text: &str) -> Option<Calendar> {
        let mut unfolded: Vec<String> = Vec::new();
        for raw in text.split('\n') {
            let raw = raw.strip_suffix('\r').unwrap_or(raw);
            if let Some(cont) = raw.strip_prefix([' ', '\t']) {
                unfolded.last_mut()?.push_str(cont);
            } else if !raw.is_empty() {
                unfolded.push(raw.to_string());
            }
        }
        let cal = Calendar {
            lines: unfolded.iter().map(|s| Line::parse(s)).collect(),
        };
        cal.todo_range()?;
        Some(cal)
    }

    /// Fresh VCALENDAR with an empty VTODO.
    pub fn new_todo(uid: &str, now: i64) -> Calendar {
        let text = format!(
            "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//Todav//EN\nBEGIN:VTODO\nUID:{uid}\nCREATED:{t}\nDTSTAMP:{t}\nEND:VTODO\nEND:VCALENDAR\n",
            t = fmt_utc(now)
        );
        Calendar::parse(&text).unwrap()
    }

    pub fn to_ics(&self) -> String {
        let mut out = String::new();
        for l in &self.lines {
            fold_into(&mut out, &l.render());
        }
        out
    }

    /// (index of BEGIN:VTODO, index of its END:VTODO)
    fn todo_range(&self) -> Option<(usize, usize)> {
        let begin = self
            .lines
            .iter()
            .position(|l| l.name == "BEGIN" && l.value.eq_ignore_ascii_case("VTODO"))?;
        let mut depth = 0;
        for (i, l) in self.lines.iter().enumerate().skip(begin) {
            match l.name.as_str() {
                "BEGIN" => depth += 1,
                "END" => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((begin, i));
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Top-level VTODO properties (nested VALARM etc. skipped), with their line index.
    fn todo_props(&self) -> impl Iterator<Item = (usize, &Line)> {
        let (b, e) = self.todo_range().unwrap();
        let mut depth = 0;
        self.lines[b + 1..e]
            .iter()
            .enumerate()
            .filter_map(move |(i, l)| {
                match l.name.as_str() {
                    "BEGIN" => depth += 1,
                    "END" => depth -= 1,
                    _ if depth == 0 => return Some((b + 1 + i, l)),
                    _ => {}
                }
                None
            })
    }

    pub fn prop(&self, name: &str) -> Option<&Line> {
        self.todo_props().map(|(_, l)| l).find(|l| l.name == name)
    }

    pub fn props<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Line> {
        self.todo_props()
            .map(|(_, l)| l)
            .filter(move |l| l.name == name)
    }

    /// Unescaped value; empty values (`DESCRIPTION:`) read as absent.
    pub fn text(&self, name: &str) -> Option<String> {
        self.prop(name)
            .map(|l| unescape(&l.value))
            .filter(|v| !v.is_empty())
    }

    /// Replace all occurrences of `name` with one line, or remove it when `value` is None.
    /// `value` is raw (already escaped where needed).
    pub fn set(&mut self, name: &str, params: &str, value: Option<&str>) {
        let idx: Vec<usize> = self
            .todo_props()
            .filter(|(_, l)| l.name == name)
            .map(|(i, _)| i)
            .collect();
        let line = value.map(|v| Line {
            name: name.to_string(),
            params: params.to_string(),
            value: v.to_string(),
        });
        let after_last = self.todo_props().last().map(|(i, _)| i + 1);
        let insert_at = idx.first().copied().or(after_last).unwrap();
        for i in idx.into_iter().rev() {
            self.lines.remove(i);
        }
        if let Some(line) = line {
            self.lines.insert(insert_at, line);
        }
    }

    pub fn set_text(&mut self, name: &str, value: Option<&str>) {
        self.set(name, "", value.map(escape).as_deref());
    }

    pub fn set_time(&mut self, name: &str, t: Option<i64>) {
        self.set(name, "", t.map(fmt_utc).as_deref());
    }

    /// Timestamp of a DATE / DATE-TIME property. TZID and floating times are read as UTC.
    pub fn time(&self, name: &str) -> Option<i64> {
        parse_time(&self.prop(name)?.value)
    }

    pub fn categories(&self) -> Vec<String> {
        self.props("CATEGORIES")
            .flat_map(|l| split_unescaped(&l.value, ','))
            .map(|s| unescape(&s))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Drop the parent link (RELATED-TO without RELTYPE or RELTYPE=PARENT); other relations stay.
    pub fn clear_parent(&mut self) {
        let idx: Vec<usize> = self
            .todo_props()
            .filter(|(_, l)| {
                l.name == "RELATED-TO"
                    && l.param("RELTYPE")
                        .is_none_or(|r| r.eq_ignore_ascii_case("PARENT"))
            })
            .map(|(i, _)| i)
            .collect();
        for i in idx.into_iter().rev() {
            self.lines.remove(i);
        }
    }

    pub fn parent_uid(&self) -> Option<String> {
        self.props("RELATED-TO")
            .find(|l| {
                l.param("RELTYPE")
                    .is_none_or(|r| r.eq_ignore_ascii_case("PARENT"))
            })
            .map(|l| unescape(&l.value))
            .filter(|v| !v.is_empty())
    }
}

fn fold_into(out: &mut String, line: &str) {
    let mut width = 0;
    for c in line.chars() {
        if width + c.len_utf8() > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(c);
        width += c.len_utf8();
    }
    out.push_str("\r\n");
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(c) => out.push(c),
            None => out.push('\\'),
        }
    }
    out
}

/// Split on `sep` not preceded by a backslash escape; parts stay escaped.
fn split_unescaped(s: &str, sep: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut esc = false;
    for c in s.chars() {
        if c == sep && !esc {
            parts.push(String::new());
        } else {
            parts.last_mut().unwrap().push(c);
        }
        esc = c == '\\' && !esc;
    }
    parts
}

// ponytail: TZID/floating times read as UTC (off by the zone offset); fine for sorting, revisit if DUE shows times.
pub fn parse_time(v: &str) -> Option<i64> {
    let d = v.get(..8)?;
    let (y, m, day) = (
        d[..4].parse().ok()?,
        d[4..6].parse().ok()?,
        d[6..8].parse().ok()?,
    );
    let mut secs = days_from_civil(y, m, day) * 86400;
    if let Some(t) = v.get(9..15) {
        secs += t[..2].parse::<i64>().ok()? * 3600
            + t[2..4].parse::<i64>().ok()? * 60
            + t[4..6].parse::<i64>().ok()?;
    }
    Some(secs)
}

pub fn fmt_utc(t: i64) -> String {
    let (y, m, d) = civil_from_days(t.div_euclid(86400));
    let s = t.rem_euclid(86400);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

// Howard Hinnant's civil date algorithms.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Nextcloud Tasks v0.16.1\r\nBEGIN:VTODO\r\nUID:abc-123\r\nCREATED:20261001T080000Z\r\nLAST-MODIFIED:20261002T090000Z\r\nDTSTAMP:20261002T090000Z\r\nSUMMARY:Mleko\\, jajka i żółty ser – bardzo długa nazwa zadania która się zawija\r\n  na pewno\r\nDESCRIPTION:line1\\nline2\r\nCATEGORIES:Groceries,Home\\,Garden\r\nRELATED-TO;RELTYPE=PARENT:parent-1\r\nDUE;VALUE=DATE:20261009\r\nX-UNKNOWN;FOO=\"a:b\":keep me\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nSUMMARY:alarm\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";

    #[test]
    fn reads_properties() {
        let c = Calendar::parse(SAMPLE).unwrap();
        assert_eq!(c.text("UID").unwrap(), "abc-123");
        assert_eq!(
            c.text("SUMMARY").unwrap(),
            "Mleko, jajka i żółty ser – bardzo długa nazwa zadania która się zawija na pewno"
        );
        assert_eq!(c.text("DESCRIPTION").unwrap(), "line1\nline2");
        assert_eq!(c.categories(), ["Groceries", "Home,Garden"]);
        assert_eq!(c.parent_uid().unwrap(), "parent-1");
        assert_eq!(c.time("DUE"), Some(parse_time("20261009T000000Z").unwrap()));
        assert_eq!(c.prop("X-UNKNOWN").unwrap().value, "keep me");
        assert_eq!(c.prop("TRIGGER"), None, "VALARM props are not VTODO props");
    }

    #[test]
    fn round_trip_is_stable_and_only_touches_patched_props() {
        let c = Calendar::parse(SAMPLE).unwrap();
        let once = c.to_ics();
        assert_eq!(Calendar::parse(&once).unwrap().to_ics(), once);
        assert!(once.lines().all(|l| l.len() <= 76)); // 75 + '\r'

        let mut p = c.clone();
        p.set_text("SUMMARY", Some("Chleb; masło"));
        p.set("STATUS", "", Some("COMPLETED"));
        p.set_text("DESCRIPTION", None);
        let out = p.to_ics();
        let p2 = Calendar::parse(&out).unwrap();
        assert_eq!(p2.text("SUMMARY").unwrap(), "Chleb; masło");
        assert_eq!(p2.text("STATUS").unwrap(), "COMPLETED");
        assert_eq!(p2.text("DESCRIPTION"), None);
        assert_eq!(p2.prop("TRIGGER"), None);
        assert!(out.contains("TRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VTODO"));
        assert!(out.contains("X-UNKNOWN;FOO=\"a:b\":keep me"));
    }

    #[test]
    fn time_round_trip() {
        for t in [0, 951782400, 1791460800, 4102444799] {
            assert_eq!(parse_time(&fmt_utc(t)), Some(t));
        }
        assert_eq!(fmt_utc(1791460800), "20261008T120000Z");
    }

    /// Every VTODO in exports dropped into `<repo>/tmp/*.ics` survives parse → write → parse,
    /// and patching SUMMARY changes nothing else. Skips when there are no exports.
    #[test]
    fn real_exports_round_trip() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../tmp");
        let files = std::fs::read_dir(dir).into_iter().flatten().flatten();
        for f in files.filter(|f| f.path().extension().is_some_and(|e| e == "ics")) {
            let text = std::fs::read_to_string(f.path()).unwrap();
            let unfold = |t: &str| {
                Calendar::parse(t)
                    .unwrap()
                    .lines
                    .iter()
                    .map(Line::render)
                    .collect::<Vec<_>>()
            };
            // Split a whole-calendar export into one object per VTODO, as the server stores them.
            for todo in text.split("BEGIN:VTODO").skip(1) {
                let body = todo.split("END:VTODO").next().unwrap();
                let one = format!("BEGIN:VCALENDAR\nBEGIN:VTODO{body}END:VTODO\nEND:VCALENDAR\n");
                let cal = Calendar::parse(&one).unwrap();
                assert_eq!(unfold(&cal.to_ics()), unfold(&one), "{one}");
                let mut p = cal.clone();
                p.set_text("SUMMARY", Some("x, y; z"));
                let (before, after) = (unfold(&one), unfold(&p.to_ics()));
                let diff: Vec<_> = before.iter().zip(&after).filter(|(a, b)| a != b).collect();
                assert_eq!(diff.len(), 1, "{diff:?}");
                assert!(after.iter().any(|l| l == "SUMMARY:x\\, y\\; z"));
            }
        }
    }

    #[test]
    fn empty_values_read_as_absent() {
        let c = Calendar::parse("BEGIN:VCALENDAR\nBEGIN:VTODO\nUID:u\nDESCRIPTION:\nRELATED-TO:\nEND:VTODO\nEND:VCALENDAR\n").unwrap();
        assert_eq!((c.text("DESCRIPTION"), c.parent_uid()), (None, None));
    }

    #[test]
    fn clear_parent_keeps_other_relations() {
        let mut c = Calendar::parse(
            "BEGIN:VCALENDAR\nBEGIN:VTODO\nUID:u\nRELATED-TO;RELTYPE=PARENT:p\nRELATED-TO;RELTYPE=SIBLING:s\nEND:VTODO\nEND:VCALENDAR\n",
        )
        .unwrap();
        c.clear_parent();
        assert_eq!(c.parent_uid(), None);
        assert!(c.to_ics().contains("RELATED-TO;RELTYPE=SIBLING:s"));
    }

    #[test]
    fn new_todo_parses() {
        let mut c = Calendar::new_todo("u1", 0);
        c.set_text("SUMMARY", Some("x"));
        let c = Calendar::parse(&c.to_ics()).unwrap();
        assert_eq!(c.text("UID").unwrap(), "u1");
        assert_eq!(c.text("SUMMARY").unwrap(), "x");
    }
}
