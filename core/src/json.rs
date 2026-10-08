//! Minimal JSON value: parse and serialize (config.json, ntfy stream lines).

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    pub fn parse(s: &str) -> Option<Json> {
        let mut p = Parser {
            s: s.as_bytes(),
            i: 0,
        };
        let v = p.value()?;
        p.ws();
        (p.i == p.s.len()).then_some(v)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.get(key),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn num(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(a) => a,
            _ => &[],
        }
    }
}

impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Json::Null => write!(f, "null"),
            Json::Bool(b) => write!(f, "{b}"),
            Json::Num(n) => write!(f, "{n}"),
            Json::Str(s) => {
                write!(f, "\"")?;
                for c in s.chars() {
                    match c {
                        '"' => write!(f, "\\\"")?,
                        '\\' => write!(f, "\\\\")?,
                        '\n' => write!(f, "\\n")?,
                        c if (c as u32) < 0x20 => write!(f, "\\u{:04x}", c as u32)?,
                        c => write!(f, "{c}")?,
                    }
                }
                write!(f, "\"")
            }
            Json::Arr(a) => {
                write!(f, "[")?;
                for (i, v) in a.iter().enumerate() {
                    write!(f, "{}{v}", if i > 0 { "," } else { "" })?;
                }
                write!(f, "]")
            }
            Json::Obj(m) => {
                write!(f, "{{")?;
                for (i, (k, v)) in m.iter().enumerate() {
                    write!(
                        f,
                        "{}{}:{v}",
                        if i > 0 { "," } else { "" },
                        Json::Str(k.clone())
                    )?;
                }
                write!(f, "}}")
            }
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Option<()> {
        self.ws();
        (self.s.get(self.i) == Some(&c)).then(|| self.i += 1)
    }

    fn lit(&mut self, word: &str, v: Json) -> Option<Json> {
        self.s[self.i..].starts_with(word.as_bytes()).then(|| {
            self.i += word.len();
            v
        })
    }

    fn value(&mut self) -> Option<Json> {
        self.ws();
        match *self.s.get(self.i)? {
            b'n' => self.lit("null", Json::Null),
            b't' => self.lit("true", Json::Bool(true)),
            b'f' => self.lit("false", Json::Bool(false)),
            b'"' => self.string().map(Json::Str),
            b'[' => {
                self.i += 1;
                let mut a = Vec::new();
                if self.eat(b']').is_some() {
                    return Some(Json::Arr(a));
                }
                loop {
                    a.push(self.value()?);
                    if self.eat(b']').is_some() {
                        return Some(Json::Arr(a));
                    }
                    self.eat(b',')?;
                }
            }
            b'{' => {
                self.i += 1;
                let mut m = BTreeMap::new();
                if self.eat(b'}').is_some() {
                    return Some(Json::Obj(m));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.eat(b':')?;
                    m.insert(k, self.value()?);
                    if self.eat(b'}').is_some() {
                        return Some(Json::Obj(m));
                    }
                    self.eat(b',')?;
                }
            }
            _ => {
                let start = self.i;
                while self
                    .s
                    .get(self.i)
                    .is_some_and(|c| b"+-.eE0123456789".contains(c))
                {
                    self.i += 1;
                }
                std::str::from_utf8(&self.s[start..self.i])
                    .ok()?
                    .parse()
                    .ok()
                    .map(Json::Num)
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        if self.s.get(self.i) != Some(&b'"') {
            return None;
        }
        self.i += 1;
        let mut out = Vec::new();
        loop {
            match *self.s.get(self.i)? {
                b'"' => {
                    self.i += 1;
                    return String::from_utf8(out).ok();
                }
                b'\\' => {
                    self.i += 1;
                    let c = *self.s.get(self.i)?;
                    self.i += 1;
                    let ch = match c {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xd800..0xdc00).contains(&cp)
                                && self.s[self.i..].starts_with(b"\\u")
                            {
                                self.i += 2;
                                cp = 0x10000 + ((cp - 0xd800) << 10) + (self.hex4()? - 0xdc00);
                            }
                            char::from_u32(cp).unwrap_or('\u{fffd}')
                        }
                        c => c as char,
                    };
                    out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                }
                c => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let h = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u32::from_str_radix(h, 16).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let src = r#" {"version": 1, "lists": {"shopping": {"categories": [
            {"name": "Spożywcze \"A\"", "icon": "basket", "order": 0}, {"name": "Å😀", "order": -1.5e0}]}},
            "x": [true, false, null, []], "e": {}} "#;
        let v = Json::parse(src).unwrap();
        let cats = v
            .get("lists")
            .unwrap()
            .get("shopping")
            .unwrap()
            .get("categories")
            .unwrap()
            .arr();
        assert_eq!(cats[0].get("name").unwrap().str(), Some("Spożywcze \"A\""));
        assert_eq!(cats[1].get("name").unwrap().str(), Some("Å😀"));
        assert_eq!(cats[1].get("order").unwrap().num(), Some(-1.5));
        assert_eq!(Json::parse(&v.to_string()), Some(v));
        assert_eq!(Json::parse("{\"a\":1} x"), None);
        assert_eq!(Json::parse("[1,"), None);
    }
}
