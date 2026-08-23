//! SPARQL value semantics (doc 05 §4, correctness-first reference
//! implementation): decoded values, effective boolean value (§17.2.2),
//! operator comparisons (§17.4.1), numeric promotion (XSD), and the
//! ORDER BY total order (§15.1). The vectorized engine will fast-path
//! inline TermIds; this module is the semantic oracle it must match.

use graphy_core::{concise, vocab, Dir, TermRef};

/// A decoded value for expression evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Iri(String),
    Blank(String),
    /// Simple or language-tagged string.
    Str {
        lex: String,
        lang: Option<(String, Option<Dir>)>,
    },
    Num(Num),
    Bool(bool),
    /// xsd:dateTime / xsd:date with its declared datatype preserved.
    DateTime {
        lex: String,
        dt: String,
    },
    /// Any other typed literal (unknown datatypes compare by identity).
    Typed {
        lex: String,
        dt: String,
    },
    /// A triple term, kept as concise bytes (identity comparison).
    Triple(Vec<u8>),
}

/// Numeric tower: integer / decimal / double (float folds into double
/// in v1; the datatype survives for result typing).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Num {
    Int(i64),
    /// An XML Schema type derived from xsd:integer. The static datatype is
    /// retained for DATATYPE()/serialization; arithmetic promotes it to
    /// xsd:integer.
    IntSub(i64, &'static str),
    Dec(Dec),
    Flt(f32),
    Dbl(f64),
}

/// Exact xsd:decimal: `unscaled × 10⁻ˢᶜᵃˡᵉ`. Decimal arithmetic must be
/// exact (XSD/F&O) — floats are never involved except when a double joins
/// the promotion. Overflow beyond i128 is an evaluation error.
#[derive(Debug, Clone, Copy)]
pub struct Dec {
    pub unscaled: i128,
    pub scale: u32,
}

/// Division carries at most this many fraction digits (≥ the XSD minimum
/// of 18 totalDigits implementations must support), then normalizes.
const DIV_SCALE: u32 = 18;

impl Dec {
    pub fn from_int(i: i64) -> Dec {
        Dec {
            unscaled: i as i128,
            scale: 0,
        }
    }

    /// Parse a decimal lexical form (optional sign, digits, optional
    /// fraction); `None` on malformed input or > i128 precision.
    pub fn parse(lex: &str) -> Option<Dec> {
        let (neg, rest) = match lex.as_bytes().first()? {
            b'-' => (true, &lex[1..]),
            b'+' => (false, &lex[1..]),
            _ => (false, lex),
        };
        let (int, frac) = match rest.find('.') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        if int.is_empty() && frac.is_empty() {
            return None;
        }
        let mut unscaled: i128 = 0;
        for c in int.bytes().chain(frac.bytes()) {
            if !c.is_ascii_digit() {
                return None;
            }
            unscaled = unscaled.checked_mul(10)?.checked_add((c - b'0') as i128)?;
        }
        Some(
            Dec {
                unscaled: if neg { -unscaled } else { unscaled },
                scale: frac.len() as u32,
            }
            .normal(),
        )
    }

    /// Strip trailing fraction zeros.
    fn normal(mut self) -> Dec {
        while self.scale > 0 && self.unscaled % 10 == 0 {
            self.unscaled /= 10;
            self.scale -= 1;
        }
        self
    }

    fn pow10(e: u32) -> Option<i128> {
        10i128.checked_pow(e)
    }

    /// Both unscaled values at the common (max) scale.
    fn aligned(a: Dec, b: Dec) -> Option<(i128, i128, u32)> {
        let scale = a.scale.max(b.scale);
        let ax = a.unscaled.checked_mul(Dec::pow10(scale - a.scale)?)?;
        let bx = b.unscaled.checked_mul(Dec::pow10(scale - b.scale)?)?;
        Some((ax, bx, scale))
    }

    pub fn checked_add(a: Dec, b: Dec) -> Option<Dec> {
        let (ax, bx, scale) = Dec::aligned(a, b)?;
        Some(
            Dec {
                unscaled: ax.checked_add(bx)?,
                scale,
            }
            .normal(),
        )
    }

    pub fn checked_sub(a: Dec, b: Dec) -> Option<Dec> {
        let (ax, bx, scale) = Dec::aligned(a, b)?;
        Some(
            Dec {
                unscaled: ax.checked_sub(bx)?,
                scale,
            }
            .normal(),
        )
    }

    pub fn checked_mul(a: Dec, b: Dec) -> Option<Dec> {
        Some(
            Dec {
                unscaled: a.unscaled.checked_mul(b.unscaled)?,
                scale: a.scale.checked_add(b.scale)?,
            }
            .normal(),
        )
    }

    /// Exact-as-possible division at [`DIV_SCALE`] fraction digits
    /// (truncating), then normalized. `None` on zero divisor/overflow.
    pub fn checked_div(a: Dec, b: Dec) -> Option<Dec> {
        if b.unscaled == 0 {
            return None;
        }
        // a/b at result scale s: unscaled = a.unscaled·10^(s + b.scale − a.scale) / b.unscaled.
        let s = DIV_SCALE.max(a.scale);
        let shift = s + b.scale - a.scale; // s ≥ a.scale ⇒ non-negative
        let num = a.unscaled.checked_mul(Dec::pow10(shift)?)?;
        Some(
            Dec {
                unscaled: num / b.unscaled,
                scale: s,
            }
            .normal(),
        )
    }

    pub fn compare(a: Dec, b: Dec) -> std::cmp::Ordering {
        match Dec::aligned(a, b) {
            Some((ax, bx, _)) => ax.cmp(&bx),
            // Alignment overflow: magnitudes differ wildly — compare
            // approximately (sign + f64 suffices at that distance).
            None => a
                .to_f64()
                .partial_cmp(&b.to_f64())
                .unwrap_or(std::cmp::Ordering::Equal),
        }
    }

    pub fn to_f64(self) -> f64 {
        self.unscaled as f64 / 10f64.powi(self.scale as i32)
    }

    pub fn is_zero(self) -> bool {
        self.unscaled == 0
    }

    /// Truncate toward zero.
    pub fn trunc(self) -> i128 {
        match Dec::pow10(self.scale) {
            Some(p) => self.unscaled / p,
            None => 0,
        }
    }

    pub fn abs(self) -> Option<Dec> {
        Some(Dec {
            unscaled: self.unscaled.checked_abs()?,
            scale: self.scale,
        })
    }

    pub fn ceil(self) -> Dec {
        let t = self.trunc();
        let exact =
            self.scale == 0 || Dec::pow10(self.scale).is_some_and(|p| self.unscaled % p == 0);
        Dec::from_i128(if !exact && self.unscaled > 0 {
            t + 1
        } else {
            t
        })
    }

    pub fn floor(self) -> Dec {
        let t = self.trunc();
        let exact =
            self.scale == 0 || Dec::pow10(self.scale).is_some_and(|p| self.unscaled % p == 0);
        Dec::from_i128(if !exact && self.unscaled < 0 {
            t - 1
        } else {
            t
        })
    }

    /// Round half up (F&O fn:round: half toward positive infinity).
    pub fn round(self) -> Dec {
        let Some(p) = Dec::pow10(self.scale) else {
            return self;
        };
        let t = self.unscaled / p;
        let rem = self.unscaled % p;
        let half = p / 2;
        let out = if rem.abs() > half || (rem.abs() == half && self.unscaled > 0) {
            if self.unscaled > 0 {
                t + 1
            } else {
                t - 1
            }
        } else {
            t
        };
        Dec::from_i128(out)
    }

    fn from_i128(i: i128) -> Dec {
        Dec {
            unscaled: i,
            scale: 0,
        }
    }

    /// XPath canonical form (casting §17.5): integer-valued decimals have
    /// no fraction part at all (`"0"`), unlike the term-lexical form.
    pub fn xpath_lexical(self) -> String {
        let d = self.normal();
        if d.scale == 0 {
            return d.unscaled.to_string();
        }
        d.lexical()
    }

    /// Canonical lexical form (minimal scale, always a fraction digit —
    /// matching the store's canonical decimal convention).
    pub fn lexical(self) -> String {
        let d = self.normal();
        let neg = d.unscaled < 0;
        let digits = d.unscaled.unsigned_abs().to_string();
        let scale = d.scale as usize;
        let mut out = String::new();
        if neg {
            out.push('-');
        }
        if scale == 0 {
            out.push_str(&digits);
            out.push_str(".0");
        } else if digits.len() > scale {
            out.push_str(&digits[..digits.len() - scale]);
            out.push('.');
            out.push_str(&digits[digits.len() - scale..]);
        } else {
            out.push_str("0.");
            for _ in 0..scale - digits.len() {
                out.push('0');
            }
            out.push_str(&digits);
        }
        out
    }
}

impl PartialEq for Dec {
    fn eq(&self, other: &Dec) -> bool {
        Dec::compare(*self, *other) == std::cmp::Ordering::Equal
    }
}

impl Num {
    pub fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::IntSub(i, _) => i as f64,
            Num::Dec(d) => d.to_f64(),
            Num::Flt(d) => d as f64,
            Num::Dbl(d) => d,
        }
    }

    /// The decimal view of an exact (non-double) number.
    fn as_dec(self) -> Option<Dec> {
        match self {
            Num::Int(i) => Some(Dec::from_int(i)),
            Num::IntSub(i, _) => Some(Dec::from_int(i)),
            Num::Dec(d) => Some(d),
            Num::Flt(_) | Num::Dbl(_) => None,
        }
    }

    /// XSD promotion for a binary op result.
    fn promote(a: Num, b: Num) -> NumKind {
        use Num::*;
        match (a, b) {
            (Int(_) | IntSub(_, _), Int(_) | IntSub(_, _)) => NumKind::Int,
            (Dbl(_), _) | (_, Dbl(_)) => NumKind::Dbl,
            (Flt(_), _) | (_, Flt(_)) => NumKind::Flt,
            _ => NumKind::Dec,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumKind {
    Int,
    Dec,
    Flt,
    Dbl,
}

fn integer_subtype(dt: &str) -> Option<&'static str> {
    Some(
        match dt.strip_prefix("http://www.w3.org/2001/XMLSchema#")? {
            "long" => "http://www.w3.org/2001/XMLSchema#long",
            "int" => "http://www.w3.org/2001/XMLSchema#int",
            "short" => "http://www.w3.org/2001/XMLSchema#short",
            "byte" => "http://www.w3.org/2001/XMLSchema#byte",
            "nonNegativeInteger" => "http://www.w3.org/2001/XMLSchema#nonNegativeInteger",
            "positiveInteger" => "http://www.w3.org/2001/XMLSchema#positiveInteger",
            "negativeInteger" => "http://www.w3.org/2001/XMLSchema#negativeInteger",
            "nonPositiveInteger" => "http://www.w3.org/2001/XMLSchema#nonPositiveInteger",
            "unsignedLong" => "http://www.w3.org/2001/XMLSchema#unsignedLong",
            "unsignedInt" => "http://www.w3.org/2001/XMLSchema#unsignedInt",
            "unsignedShort" => "http://www.w3.org/2001/XMLSchema#unsignedShort",
            "unsignedByte" => "http://www.w3.org/2001/XMLSchema#unsignedByte",
            _ => return None,
        },
    )
}

fn integer_in_range(value: i64, dt: &str) -> bool {
    match dt.strip_prefix("http://www.w3.org/2001/XMLSchema#") {
        Some("byte") => i8::try_from(value).is_ok(),
        Some("short") => i16::try_from(value).is_ok(),
        Some("int") => i32::try_from(value).is_ok(),
        Some("long") => true,
        Some("nonNegativeInteger") => value >= 0,
        Some("positiveInteger") => value > 0,
        Some("negativeInteger") => value < 0,
        Some("nonPositiveInteger") => value <= 0,
        Some("unsignedLong") => value >= 0,
        Some("unsignedInt") => u32::try_from(value).is_ok(),
        Some("unsignedShort") => u16::try_from(value).is_ok(),
        Some("unsignedByte") => u8::try_from(value).is_ok(),
        _ => false,
    }
}

/// Decode concise term bytes into a [`Value`].
pub fn decode_value(bytes: &[u8]) -> Value {
    match concise::decode(bytes) {
        Ok(TermRef::Iri(iri)) => Value::Iri(iri.to_owned()),
        Ok(TermRef::BlankNode(l)) => Value::Blank(l.to_owned()),
        Ok(TermRef::TripleTerm(_)) => Value::Triple(bytes.to_vec()),
        Ok(TermRef::Literal(l)) => {
            let lex = l.lexical();
            if let Some((tag, dir)) = l.lang() {
                return Value::Str {
                    lex: lex.to_owned(),
                    lang: Some((tag.to_owned(), dir)),
                };
            }
            match l.datatype() {
                vocab::XSD_STRING => Value::Str {
                    lex: lex.to_owned(),
                    lang: None,
                },
                vocab::XSD_BOOLEAN => match lex {
                    "true" | "1" => Value::Bool(true),
                    "false" | "0" => Value::Bool(false),
                    _ => Value::Typed {
                        lex: lex.to_owned(),
                        dt: vocab::XSD_BOOLEAN.to_owned(),
                    },
                },
                vocab::XSD_INTEGER => match lex.parse::<i64>() {
                    Ok(v) => Value::Num(Num::Int(v)),
                    Err(_) => Value::Typed {
                        lex: lex.to_owned(),
                        dt: vocab::XSD_INTEGER.to_owned(),
                    },
                },
                vocab::XSD_DECIMAL => match Dec::parse(lex) {
                    Some(v) => Value::Num(Num::Dec(v)),
                    None => Value::Typed {
                        lex: lex.to_owned(),
                        dt: vocab::XSD_DECIMAL.to_owned(),
                    },
                },
                vocab::XSD_DOUBLE => match parse_fp(lex) {
                    Some(v) => Value::Num(Num::Dbl(v)),
                    None => Value::Typed {
                        lex: lex.to_owned(),
                        dt: vocab::XSD_DOUBLE.to_owned(),
                    },
                },
                vocab::XSD_FLOAT => match parse_fp(lex) {
                    Some(v) => Value::Num(Num::Flt(v as f32)),
                    None => Value::Typed {
                        lex: lex.to_owned(),
                        dt: vocab::XSD_FLOAT.to_owned(),
                    },
                },
                vocab::XSD_DATE_TIME | vocab::XSD_DATE => Value::DateTime {
                    lex: lex.to_owned(),
                    dt: l.datatype().to_owned(),
                },
                dt if integer_subtype(dt).is_some() => match lex.parse::<i64>() {
                    Ok(v) if integer_in_range(v, dt) => {
                        Value::Num(Num::IntSub(v, integer_subtype(dt).unwrap()))
                    }
                    _ => Value::Typed {
                        lex: lex.to_owned(),
                        dt: dt.to_owned(),
                    },
                },
                dt => Value::Typed {
                    lex: lex.to_owned(),
                    dt: dt.to_owned(),
                },
            }
        }
        Err(_) => Value::Typed {
            lex: String::new(),
            dt: String::new(),
        },
    }
}

fn parse_fp(lex: &str) -> Option<f64> {
    match lex {
        "INF" | "+INF" => Some(f64::INFINITY),
        "-INF" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        _ => lex.parse().ok(),
    }
}

#[derive(Clone, Copy)]
struct Temporal {
    day: i64,
    second: Dec,
    offset_minutes: Option<i64>,
}

fn temporal_cmp(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let a = parse_temporal(a)?;
    let b = parse_temporal(b)?;
    let normalize = |t: Temporal, assumed_offset: i64| -> Option<(i64, Dec)> {
        let offset = t.offset_minutes.unwrap_or(assumed_offset);
        let shifted = Dec::checked_sub(t.second, Dec::from_int(offset.checked_mul(60)?))?;
        let seconds_per_day = 86_400i128;
        let whole = shifted.trunc();
        let day_delta = whole.div_euclid(seconds_per_day);
        let within = Dec::checked_sub(
            shifted,
            Dec::from_i128(day_delta.checked_mul(seconds_per_day)?),
        )?;
        Some((t.day.checked_add(i64::try_from(day_delta).ok()?)?, within))
    };
    let cmp = |x: (i64, Dec), y: (i64, Dec)| x.0.cmp(&y.0).then_with(|| Dec::compare(x.1, y.1));
    match (a.offset_minutes, b.offset_minutes) {
        (Some(_), Some(_)) | (None, None) => Some(cmp(normalize(a, 0)?, normalize(b, 0)?)),
        (Some(_), None) => {
            // A missing timezone spans the XML Schema interval -14:00 to
            // +14:00. Only values outside the whole interval are ordered.
            let av = normalize(a, 0)?;
            let earliest = normalize(b, 14 * 60)?;
            let latest = normalize(b, -14 * 60)?;
            if cmp(av, earliest).is_lt() {
                Some(std::cmp::Ordering::Less)
            } else if cmp(av, latest).is_gt() {
                Some(std::cmp::Ordering::Greater)
            } else {
                None
            }
        }
        (None, Some(_)) => {
            let bv = normalize(b, 0)?;
            let earliest = normalize(a, 14 * 60)?;
            let latest = normalize(a, -14 * 60)?;
            if cmp(latest, bv).is_lt() {
                Some(std::cmp::Ordering::Less)
            } else if cmp(earliest, bv).is_gt() {
                Some(std::cmp::Ordering::Greater)
            } else {
                None
            }
        }
    }
}

fn parse_temporal(lex: &str) -> Option<Temporal> {
    let (body, offset_minutes) = if let Some(body) = lex.strip_suffix('Z') {
        (body, Some(0))
    } else if lex.len() >= 6 {
        let at = lex.len() - 6;
        let suffix = &lex[at..];
        if matches!(suffix.as_bytes()[0], b'+' | b'-') && suffix.as_bytes()[3] == b':' {
            let h: i64 = suffix[1..3].parse().ok()?;
            let m: i64 = suffix[4..6].parse().ok()?;
            if h > 14 || m > 59 || h == 14 && m != 0 {
                return None;
            }
            let sign = if suffix.starts_with('-') { -1 } else { 1 };
            (&lex[..at], Some(sign * (h * 60 + m)))
        } else {
            (lex, None)
        }
    } else {
        (lex, None)
    };

    let (date, time) = match body.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (body, None),
    };
    let (negative, date) = date
        .strip_prefix('-')
        .map_or((false, date), |date| (true, date));
    let mut parts = date.split('-');
    let mut year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || year == 0 || !(1..=12).contains(&month) {
        return None;
    }
    if negative {
        year = -year;
    }
    let leap = year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0);
    let max_day = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=max_day).contains(&day) {
        return None;
    }
    let mut second = Dec::from_int(0);
    if let Some(time) = time {
        let mut parts = time.split(':');
        let hour: i64 = parts.next()?.parse().ok()?;
        let minute: i64 = parts.next()?.parse().ok()?;
        let sec = Dec::parse(parts.next()?)?;
        if parts.next().is_some()
            || !(0..=24).contains(&hour)
            || !(0..=59).contains(&minute)
            || Dec::compare(sec, Dec::from_int(0)).is_lt()
            || !Dec::compare(sec, Dec::from_int(60)).is_lt()
            || hour == 24 && (minute != 0 || !sec.is_zero())
        {
            return None;
        }
        second = Dec::checked_add(
            Dec::from_int(
                hour.checked_mul(3600)?
                    .checked_add(minute.checked_mul(60)?)?,
            ),
            sec,
        )?;
    }
    Some(Temporal {
        day: days_from_civil(year, month, day),
        second,
        offset_minutes,
    })
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    era * 146_097 + (365 * yoe + yoe / 4 - yoe / 100) + doy
}

/// Re-encode a computed value as concise bytes.
pub fn encode_value(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    match v {
        Value::Iri(iri) => concise::encode_iri(&mut out, iri),
        Value::Blank(l) => concise::encode_blank(&mut out, l),
        Value::Str { lex, lang: None } => concise::encode_simple(&mut out, lex),
        Value::Str {
            lex,
            lang: Some((tag, dir)),
        } => concise::encode_lang(&mut out, lex, tag, *dir),
        Value::Num(Num::Int(i)) => {
            concise::encode_datatype(&mut out, &i.to_string(), vocab::XSD_INTEGER)
        }
        Value::Num(Num::IntSub(i, dt)) => concise::encode_datatype(&mut out, &i.to_string(), dt),
        Value::Num(Num::Dec(d)) => {
            concise::encode_datatype(&mut out, &d.lexical(), vocab::XSD_DECIMAL)
        }
        Value::Num(Num::Dbl(d)) => {
            // The store's canonical double writer (id inlining depends on
            // canonical forms — computed doubles must match store terms).
            let s = graphy_core::InlineValue::Double {
                value: *d,
                declared_float: false,
            }
            .canonical_lexical();
            concise::encode_datatype(&mut out, &s, vocab::XSD_DOUBLE)
        }
        Value::Num(Num::Flt(d)) => {
            let s = graphy_core::InlineValue::Double {
                value: f64::from(*d),
                declared_float: true,
            }
            .canonical_lexical();
            concise::encode_datatype(&mut out, &s, vocab::XSD_FLOAT)
        }
        Value::Bool(b) => concise::encode_datatype(
            &mut out,
            if *b { "true" } else { "false" },
            vocab::XSD_BOOLEAN,
        ),
        Value::DateTime { lex, dt } => concise::encode_datatype(&mut out, lex, dt),
        // xsd:string has a dedicated concise spelling (single-spelling
        // invariant) — STRDT(?x, xsd:string) must produce a simple literal.
        Value::Typed { lex, dt } if dt == vocab::XSD_STRING => {
            concise::encode_simple(&mut out, lex)
        }
        Value::Typed { lex, dt } => concise::encode_datatype(&mut out, lex, dt),
        Value::Triple(bytes) => return bytes.clone(),
    }
    out
}

/// Effective boolean value (§17.2.2); `None` = type error.
pub fn ebv(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Num(n) => Some(match n {
            Num::Int(i) => *i != 0,
            Num::IntSub(i, _) => *i != 0,
            Num::Dec(d) => !d.is_zero(),
            Num::Flt(d) => *d != 0.0 && !d.is_nan(),
            Num::Dbl(d) => *d != 0.0 && !d.is_nan(),
        }),
        Value::Str { lex, lang: None } => Some(!lex.is_empty()),
        Value::Str { lang: Some(_), .. } => None,
        _ => None,
    }
}

/// Operator comparison (§17.4.1: `<`-family). `None` = type error.
pub fn cmp_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    use Value::*;
    match (a, b) {
        // Exact when both sides are exact (int/decimal); doubles compare
        // in f64 as XSD promotion dictates.
        (Num(x), Num(y)) => match (x.as_dec(), y.as_dec()) {
            (Some(dx), Some(dy)) => Some(Dec::compare(dx, dy)),
            _ => x.as_f64().partial_cmp(&y.as_f64()),
        },
        (Str { lex: x, lang: lx }, Str { lex: y, lang: ly }) if lang_eq(lx, ly) => {
            Some(x.as_str().cmp(y))
        }
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (DateTime { lex: x, dt: dx }, DateTime { lex: y, dt: dy }) if dx == dy => {
            temporal_cmp(x, y)
        }
        _ => None,
    }
}

/// `=` per RDFterm-equal (§17.4.1.7): value equality where a comparison
/// exists, else term identity; incomparable distinct literals = error
/// (`None`).
pub fn eq_values(a: &Value, b: &Value) -> Option<bool> {
    use Value::*;
    match (a, b) {
        (Num(_), Num(_)) | (Bool(_), Bool(_)) => {
            cmp_values(a, b).map(|o| o == std::cmp::Ordering::Equal)
        }
        (DateTime { dt: dx, .. }, DateTime { dt: dy, .. }) if dx != dy => Some(false),
        (DateTime { .. }, DateTime { .. }) => {
            cmp_values(a, b).map(|o| o == std::cmp::Ordering::Equal)
        }
        (Str { lex: x, lang: lx }, Str { lex: y, lang: ly }) => Some(x == y && lang_eq(lx, ly)),
        (Iri(x), Iri(y)) => Some(x == y),
        (Blank(x), Blank(y)) => Some(x == y),
        (Triple(x), Triple(y)) => {
            let xs = triple_values(x)?;
            let ys = triple_values(y)?;
            let mut errored = false;
            for (left, right) in xs.iter().zip(ys.iter()) {
                match eq_values(left, right) {
                    Some(false) => return Some(false),
                    Some(true) => {}
                    None => errored = true,
                }
            }
            if errored {
                None
            } else {
                Some(true)
            }
        }
        (Typed { lex: x, dt: dx }, Typed { lex: y, dt: dy }) => {
            if dx == dy && x == y {
                Some(true)
            } else {
                // An ill-formed built-in literal or an extension datatype
                // may have an unknown/overlapping value mapping.
                None
            }
        }
        // Language-tagged strings have a distinct RDF value space; unlike
        // simple strings they cannot overlap extension datatype values.
        (Typed { .. }, Str { lang: Some(_), .. }) | (Str { lang: Some(_), .. }, Typed { .. }) => {
            Some(false)
        }
        // Unsupported or incompatible literal value spaces are an
        // evaluation error. Literal-vs-nonliteral and distinct nonliteral
        // kinds are simply unequal RDF terms.
        (x, y) if is_literal(x) && is_literal(y) => None,
        _ => Some(false),
    }
}

fn is_literal(v: &Value) -> bool {
    matches!(
        v,
        Value::Str { .. }
            | Value::Num(_)
            | Value::Bool(_)
            | Value::DateTime { .. }
            | Value::Typed { .. }
    )
}

fn lang_eq(a: &Option<(String, Option<Dir>)>, b: &Option<(String, Option<Dir>)>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some((ta, da)), Some((tb, db))) => ta.eq_ignore_ascii_case(tb) && da == db,
        _ => false,
    }
}

/// ORDER BY total order (§15.1): unbound < blank < IRI < literal, with
/// comparable literal groups by value and everything else deterministic
/// by (datatype, lexical).
pub fn order_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let rank = |v: &Value| match v {
        Value::Blank(_) => 0,
        Value::Iri(_) => 1,
        Value::Triple(_) => 3,
        _ => 2,
    };
    match rank(a).cmp(&rank(b)) {
        Ordering::Equal => {}
        o => return o,
    }
    match (a, b) {
        (Value::Blank(x), Value::Blank(y)) => x.cmp(y),
        (Value::Iri(x), Value::Iri(y)) => x.cmp(y),
        // SPARQL leaves this relative order implementation-defined. Use a
        // deterministic recursive SPO order, which also makes storage scan
        // order irrelevant to OFFSET/LIMIT over triple terms.
        (Value::Triple(x), Value::Triple(y)) => {
            if let (Some(xs), Some(ys)) = (triple_values(x), triple_values(y)) {
                for (left, right) in xs.iter().zip(ys.iter()) {
                    let o = order_cmp(left, right);
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                Ordering::Equal
            } else {
                x.cmp(y)
            }
        }
        _ => {
            if let Some(o) = cmp_values(a, b) {
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
            }
            // Deterministic fallback across incomparable literals.
            sort_key(a).cmp(&sort_key(b))
        }
    }
}

fn triple_values(bytes: &[u8]) -> Option<[Value; 3]> {
    let TermRef::TripleTerm(tt) = concise::decode(bytes).ok()? else {
        return None;
    };
    fn value(term: TermRef<'_>) -> Value {
        let mut out = Vec::new();
        fn write(out: &mut Vec<u8>, term: TermRef<'_>) {
            match term {
                TermRef::Iri(i) => concise::encode_iri(out, i),
                TermRef::BlankNode(b) => concise::encode_blank(out, b),
                TermRef::Literal(l) => {
                    if let Some((lang, dir)) = l.lang() {
                        concise::encode_lang(out, l.lexical(), lang, dir);
                    } else if l.datatype() == vocab::XSD_STRING {
                        concise::encode_simple(out, l.lexical());
                    } else {
                        concise::encode_datatype(out, l.lexical(), l.datatype());
                    }
                }
                TermRef::TripleTerm(t) => {
                    let mut s = Vec::new();
                    let mut p = Vec::new();
                    let mut o = Vec::new();
                    write(&mut s, t.subject());
                    write(&mut p, t.predicate());
                    write(&mut o, t.object());
                    concise::encode_triple_term(out, &s, &p, &o);
                }
            }
        }
        write(&mut out, term);
        decode_value(&out)
    }
    Some([
        value(tt.subject()),
        value(tt.predicate()),
        value(tt.object()),
    ])
}

fn sort_key(v: &Value) -> (u8, String, String) {
    match v {
        Value::Num(n) => (0, String::new(), format!("{:020.6}", n.as_f64())),
        Value::Str { lex, lang: None } => (1, String::new(), lex.clone()),
        Value::Str {
            lex,
            lang: Some((t, _)),
        } => (2, t.clone(), lex.clone()),
        Value::Bool(b) => (3, String::new(), b.to_string()),
        Value::DateTime { lex, dt } => (4, dt.clone(), lex.clone()),
        Value::Typed { lex, dt } => (5, dt.clone(), lex.clone()),
        Value::Triple(b) => (6, String::new(), format!("{b:?}")),
        Value::Iri(x) => (7, String::new(), x.clone()),
        Value::Blank(x) => (8, String::new(), x.clone()),
    }
}

/// Arithmetic with XSD promotion; `None` = type error (non-numeric,
/// or integer overflow).
pub fn arith(op: ArithOp, a: &Value, b: &Value) -> Option<Value> {
    let (Value::Num(x), Value::Num(y)) = (a, b) else {
        return None;
    };
    let kind = match op {
        // op:numeric-divide on two integers yields decimal.
        ArithOp::Div if matches!(Num::promote(*x, *y), NumKind::Int) => NumKind::Dec,
        _ => Num::promote(*x, *y),
    };
    Some(Value::Num(match kind {
        NumKind::Int => {
            let (i, j) = (int_of(*x), int_of(*y));
            let v = match op {
                ArithOp::Add => i.checked_add(j)?,
                ArithOp::Sub => i.checked_sub(j)?,
                ArithOp::Mul => i.checked_mul(j)?,
                ArithOp::Div => unreachable!("integer division promotes"),
            };
            Num::Int(v)
        }
        NumKind::Dec => {
            let (i, j) = (x.as_dec()?, y.as_dec()?);
            Num::Dec(match op {
                ArithOp::Add => Dec::checked_add(i, j)?,
                ArithOp::Sub => Dec::checked_sub(i, j)?,
                ArithOp::Mul => Dec::checked_mul(i, j)?,
                // Zero divisor / overflow error via None.
                ArithOp::Div => Dec::checked_div(i, j)?,
            })
        }
        NumKind::Flt => {
            let (i, j) = (x.as_f64() as f32, y.as_f64() as f32);
            Num::Flt(match op {
                ArithOp::Add => i + j,
                ArithOp::Sub => i - j,
                ArithOp::Mul => i * j,
                ArithOp::Div => i / j,
            })
        }
        NumKind::Dbl => {
            let (i, j) = (x.as_f64(), y.as_f64());
            Num::Dbl(match op {
                ArithOp::Add => i + j,
                ArithOp::Sub => i - j,
                ArithOp::Mul => i * j,
                ArithOp::Div => i / j,
            })
        }
    }))
}

fn int_of(n: Num) -> i64 {
    match n {
        Num::Int(i) => i,
        Num::IntSub(i, _) => i,
        Num::Dec(d) => d.trunc() as i64,
        Num::Flt(d) => d as i64,
        Num::Dbl(d) => d as i64,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

/// STR(value) per §17.4.2.5.
pub fn str_of(v: &Value) -> Option<String> {
    Some(match v {
        Value::Iri(i) => i.clone(),
        Value::Str { lex, .. } => lex.clone(),
        Value::Num(Num::Int(i)) => i.to_string(),
        Value::Num(Num::IntSub(i, _)) => i.to_string(),
        Value::Num(Num::Dec(d)) => d.lexical(),
        Value::Num(n) => format!("{}", n.as_f64()),
        Value::Bool(b) => b.to_string(),
        Value::DateTime { lex, .. } => lex.clone(),
        Value::Typed { lex, .. } => lex.clone(),
        Value::Blank(_) | Value::Triple(_) => return None,
    })
}

#[cfg(test)]
mod oracle_regression_tests {
    use super::{cmp_values, Value};

    #[test]
    fn same_language_strings_are_ordered_lexically() {
        let value = |lex: &str, lang: &str| Value::Str {
            lex: lex.to_owned(),
            lang: Some((lang.to_owned(), None)),
        };
        assert!(cmp_values(&value("a", "fr"), &value("b", "FR")).is_some_and(|o| o.is_lt()));
        assert!(cmp_values(&value("a", "en"), &value("b", "fr")).is_none());
    }
}
