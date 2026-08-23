//! Tagged 64-bit term identifiers with inline values (doc 01 §4).
//!
//! Layout: bits 63-60 hold a [`Tag`]; bits 59-0 are tag-specific payload.
//! Numeric payloads use **offset-binary** signed encodings (`enc = v + 2^(w-1)`)
//! so that unsigned payload comparison equals value order within a tag.
//!
//! **Inlining criterion (universal):** a lexical form inlines iff parsing it
//! for its datatype and re-serializing with our canonical writer reproduces
//! the input byte-for-byte (canonical-form-only). Everything else — including
//! perfectly valid but non-canonical forms like `"042"` or `"4"^^xsd:decimal`
//! (canonical decimals require a decimal point) — takes a dictionary ordinal.
//! This keeps TermId assignment a pure function of the term while preserving
//! lexical-form identity: `"4.0"` and `"4.00"` are distinct RDF literals and
//! only the first is canonical.

use std::cmp::Ordering;

use crate::vocab;

const TAG_SHIFT: u32 = 60;
const PAYLOAD_MASK: u64 = (1 << TAG_SHIFT) - 1;

/// The four tag bits (63-60) of a [`TermId`]. Values 0x8-0xE are reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Tag {
    /// Dictionary reference: section in bits 59-56, ordinal in bits 55-0.
    DictRef = 0x0,
    /// xsd:integer, 60-bit offset-binary, range ±2⁵⁹.
    Integer = 0x1,
    /// xsd:decimal: 52-bit offset-binary unscaled (bits 59-8) + scale (bits 7-0).
    Decimal = 0x2,
    /// xsd:double/xsd:float: bit 59 = declared-float, bits 58-0 = f64 bits[63:5].
    Double = 0x3,
    /// xsd:boolean in bit 0.
    Boolean = 0x4,
    /// xsd:dateTime/xsd:date; see [`InlineDateTime`] for the field layout.
    DateTime = 0x5,
    /// xsd:string of ≤ 7 bytes (cargo feature `inline-short-strings`).
    ShortString = 0x6,
    /// Ordinal into the store's TRIPLE_TERMS section.
    TripleTerm = 0x7,
    /// Sentinels: payload 0 = DEFAULT_GRAPH, 1 = UNDEF.
    Sentinel = 0xF,
}

impl Tag {
    fn from_bits(bits: u8) -> Option<Tag> {
        Some(match bits {
            0x0 => Tag::DictRef,
            0x1 => Tag::Integer,
            0x2 => Tag::Decimal,
            0x3 => Tag::Double,
            0x4 => Tag::Boolean,
            0x5 => Tag::DateTime,
            0x6 => Tag::ShortString,
            0x7 => Tag::TripleTerm,
            0xF => Tag::Sentinel,
            _ => return None,
        })
    }
}

/// Dictionary section of a [`Tag::DictRef`] id (doc 02).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Section {
    Shared = 0,
    Subjects = 1,
    Predicates = 2,
    Objects = 3,
    Graphs = 4,
    TripleTerms = 5,
}

impl Section {
    fn from_bits(bits: u8) -> Option<Section> {
        Some(match bits {
            0 => Section::Shared,
            1 => Section::Subjects,
            2 => Section::Predicates,
            3 => Section::Objects,
            4 => Section::Graphs,
            5 => Section::TripleTerms,
            _ => return None,
        })
    }
}

/// A tagged 64-bit term identifier.
///
/// Derived `Ord` is raw-u64 order: ids group by tag, and within the numeric
/// tags the offset-binary payloads make that order the value order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TermId(u64);

impl TermId {
    /// Reserved null id. Note this aliases `(Shared, ordinal 0)`, so shared
    /// dictionary ordinals start at 1.
    pub const NULL: TermId = TermId(0);
    /// Sentinel naming the default graph in quad patterns.
    pub const DEFAULT_GRAPH: TermId = TermId(0xF << TAG_SHIFT);
    /// Sentinel for unbound values (SPARQL `UNDEF`).
    pub const UNDEF: TermId = TermId((0xF << TAG_SHIFT) | 1);

    /// Reinterpret raw bits (e.g. read back from a segment). Not validated.
    pub fn from_raw(raw: u64) -> TermId {
        TermId(raw)
    }

    pub fn raw(self) -> u64 {
        self.0
    }

    pub fn is_null(self) -> bool {
        self.0 == 0
    }

    /// The tag bits, or `None` for the reserved patterns 0x8-0xE.
    pub fn tag(self) -> Option<Tag> {
        Tag::from_bits((self.0 >> TAG_SHIFT) as u8)
    }

    /// A dictionary reference. `ordinal` must fit in 56 bits.
    pub fn dict(section: Section, ordinal: u64) -> TermId {
        debug_assert!(ordinal < 1 << 56, "dictionary ordinal exceeds 56 bits");
        TermId(((section as u64) << 56) | ordinal)
    }

    /// Section and ordinal, if this is a dictionary reference.
    pub fn dict_ref(self) -> Option<(Section, u64)> {
        if self.0 >> TAG_SHIFT != Tag::DictRef as u64 {
            return None;
        }
        let section = Section::from_bits((self.0 >> 56) as u8 & 0xF)?;
        Some((section, self.0 & ((1 << 56) - 1)))
    }

    /// A triple-term ordinal id. `ordinal` must fit in 60 bits.
    pub fn triple_term(ordinal: u64) -> TermId {
        debug_assert!(
            ordinal <= PAYLOAD_MASK,
            "triple-term ordinal exceeds 60 bits"
        );
        TermId(((Tag::TripleTerm as u64) << TAG_SHIFT) | ordinal)
    }

    /// The ordinal, if this is a triple-term id.
    pub fn triple_term_ordinal(self) -> Option<u64> {
        (self.0 >> TAG_SHIFT == Tag::TripleTerm as u64).then_some(self.0 & PAYLOAD_MASK)
    }

    /// Inline a lexical form for a recognized datatype, iff it is canonical
    /// and its value fits the payload (see module docs). `None` means "assign
    /// a dictionary ordinal instead" — never an error.
    pub fn try_inline(lexical: &str, datatype_iri: &str) -> Option<TermId> {
        let value = match datatype_iri {
            vocab::XSD_INTEGER => InlineValue::Integer(lexical.parse().ok()?),
            vocab::XSD_DECIMAL => parse_decimal(lexical)?,
            vocab::XSD_DOUBLE => InlineValue::Double {
                value: lexical.parse().ok()?,
                declared_float: false,
            },
            vocab::XSD_FLOAT => InlineValue::Double {
                // f32→f64 widening is exact and leaves the low 29 mantissa
                // bits zero, so declared floats always pass the low-5-bits
                // inlinability test.
                value: f64::from(lexical.parse::<f32>().ok()?),
                declared_float: true,
            },
            vocab::XSD_BOOLEAN => match lexical {
                "true" => InlineValue::Boolean(true),
                "false" => InlineValue::Boolean(false),
                // "1"/"0" are valid lexicals but non-canonical → dictionary.
                _ => return None,
            },
            vocab::XSD_DATE_TIME => InlineValue::DateTime(parse_date_time(lexical, false)?),
            vocab::XSD_DATE => InlineValue::DateTime(parse_date_time(lexical, true)?),
            #[cfg(feature = "inline-short-strings")]
            vocab::XSD_STRING => InlineValue::ShortString(ShortString::new(lexical)?),
            _ => return None,
        };
        // The round-trip test is the single canonicity gate: any accepted
        // non-canonical spelling above ("+42", "4.00", "+00:00", "infinity",
        // 5-digit fractional seconds, …) fails here and goes to the dictionary.
        if value.canonical_lexical() != lexical {
            return None;
        }
        TermId::inline(value)
    }

    /// Encode an [`InlineValue`], if it fits the payload ranges.
    pub fn inline(value: InlineValue) -> Option<TermId> {
        let (tag, payload) = match value {
            InlineValue::Integer(v) => {
                if !(-(1i64 << 59)..1 << 59).contains(&v) {
                    return None;
                }
                (Tag::Integer, (v + (1 << 59)) as u64)
            }
            InlineValue::Decimal { unscaled, scale } => {
                if !(-(1i64 << 51)..1 << 51).contains(&unscaled) {
                    return None;
                }
                (
                    Tag::Decimal,
                    (((unscaled + (1 << 51)) as u64) << 8) | u64::from(scale),
                )
            }
            InlineValue::Double {
                value,
                declared_float,
            } => {
                let bits = value.to_bits();
                if bits & 0x1F != 0 {
                    return None;
                }
                (Tag::Double, (u64::from(declared_float) << 59) | (bits >> 5))
            }
            InlineValue::Boolean(b) => (Tag::Boolean, u64::from(b)),
            InlineValue::DateTime(dt) => {
                let tz = dt.tz_quarters.unwrap_or(0);
                if !(-(1i64 << 40)..1 << 40).contains(&dt.seconds)
                    || dt.millis > 999
                    || !(-56..=56).contains(&tz)
                {
                    return None;
                }
                if dt.date_only {
                    // A date is a wall-clock midnight; anything finer cannot
                    // round-trip through the date lexical space.
                    let wall = dt.seconds + i64::from(tz) * 900;
                    if dt.millis != 0 || wall.rem_euclid(86400) != 0 {
                        return None;
                    }
                }
                let tz_field = match dt.tz_quarters {
                    Some(q) => (1 << 58) | ((i64::from(q) + 64) as u64) << 51,
                    None => 0,
                };
                (
                    Tag::DateTime,
                    (u64::from(dt.date_only) << 59)
                        | tz_field
                        | (u64::from(dt.millis) << 41)
                        | (dt.seconds + (1 << 40)) as u64,
                )
            }
            #[cfg(feature = "inline-short-strings")]
            InlineValue::ShortString(s) => (Tag::ShortString, s.to_payload()),
        };
        Some(TermId(((tag as u64) << TAG_SHIFT) | payload))
    }

    /// Decode the inline value, if this id carries one (dictionary refs,
    /// triple-term ordinals, sentinels, and reserved tags return `None`).
    pub fn decode(self) -> Option<InlineValue> {
        let payload = self.0 & PAYLOAD_MASK;
        Some(match self.tag()? {
            Tag::Integer => InlineValue::Integer(payload as i64 - (1 << 59)),
            Tag::Decimal => InlineValue::Decimal {
                unscaled: (payload >> 8) as i64 - (1 << 51),
                scale: (payload & 0xFF) as u8,
            },
            Tag::Double => InlineValue::Double {
                value: f64::from_bits((payload & ((1 << 59) - 1)) << 5),
                declared_float: payload >> 59 & 1 == 1,
            },
            Tag::Boolean => InlineValue::Boolean(payload & 1 == 1),
            Tag::DateTime => InlineValue::DateTime(InlineDateTime {
                date_only: payload >> 59 & 1 == 1,
                tz_quarters: (payload >> 58 & 1 == 1)
                    .then(|| ((payload >> 51 & 0x7F) as i64 - 64) as i8),
                millis: (payload >> 41 & 0x3FF) as u16,
                seconds: (payload & ((1 << 41) - 1)) as i64 - (1 << 40),
            }),
            #[cfg(feature = "inline-short-strings")]
            Tag::ShortString => InlineValue::ShortString(ShortString::from_payload(payload)?),
            _ => return None,
        })
    }
}

/// A value carried inline in a [`TermId`] payload.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InlineValue {
    Integer(i64),
    /// Value = `unscaled · 10⁻ˢᶜᵃˡᵉ`. Canonical values have `scale ≥ 1` and no
    /// strippable trailing zero (`scale == 1 || unscaled % 10 != 0`).
    Decimal {
        unscaled: i64,
        scale: u8,
    },
    /// A declared xsd:float is stored widened to f64; the flag preserves the
    /// datatype (and thus which canonical writer reproduces the lexical form).
    Double {
        value: f64,
        declared_float: bool,
    },
    Boolean(bool),
    DateTime(InlineDateTime),
    #[cfg(feature = "inline-short-strings")]
    ShortString(ShortString),
}

impl InlineValue {
    /// Serialize with the canonical writer for this value's datatype. By the
    /// inlining criterion this reproduces the original lexical form exactly.
    pub fn canonical_lexical(&self) -> String {
        match *self {
            InlineValue::Integer(v) => v.to_string(),
            InlineValue::Decimal { unscaled, scale } => render_decimal(unscaled, scale),
            InlineValue::Double {
                value,
                declared_float: false,
            } => render_double(value),
            InlineValue::Double {
                value,
                declared_float: true,
            } => render_float(value as f32),
            InlineValue::Boolean(b) => (if b { "true" } else { "false" }).to_owned(),
            InlineValue::DateTime(ref dt) => render_date_time(dt),
            #[cfg(feature = "inline-short-strings")]
            InlineValue::ShortString(ref s) => s.as_str().to_owned(),
        }
    }

    pub fn datatype_iri(&self) -> &'static str {
        match self {
            InlineValue::Integer(_) => vocab::XSD_INTEGER,
            InlineValue::Decimal { .. } => vocab::XSD_DECIMAL,
            InlineValue::Double {
                declared_float: false,
                ..
            } => vocab::XSD_DOUBLE,
            InlineValue::Double {
                declared_float: true,
                ..
            } => vocab::XSD_FLOAT,
            InlineValue::Boolean(_) => vocab::XSD_BOOLEAN,
            InlineValue::DateTime(dt) if dt.date_only => vocab::XSD_DATE,
            InlineValue::DateTime(_) => vocab::XSD_DATE_TIME,
            #[cfg(feature = "inline-short-strings")]
            InlineValue::ShortString(_) => vocab::XSD_STRING,
        }
    }
}

/// An inlined xsd:dateTime or xsd:date.
///
/// Payload layout (tag 0x5): bit 59 = kind (0 dateTime, 1 date), bit 58 =
/// has-tz, bits 57-51 = tz quarter-hours (offset-binary i7, ±56), bits 50-41 =
/// milliseconds, bits 40-0 = seconds from the epoch (offset-binary i41).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InlineDateTime {
    pub date_only: bool,
    /// Seconds from 1970-01-01T00:00:00 as an instant: the timezone offset is
    /// already subtracted; a value without timezone stores its wall clock
    /// as if UTC (which is also how [`partial_cmp_value`] compares it — a
    /// documented M0 deviation from XSD's partial order, to revisit).
    pub seconds: i64,
    /// Milliseconds 0-999. Finer fractional seconds never inline.
    pub millis: u16,
    /// Timezone offset in quarter-hours (±56 = ±14:00); `None` = no timezone.
    pub tz_quarters: Option<i8>,
}

/// An inlined xsd:string of at most 7 bytes, packed big-endian left-aligned
/// into payload bits 59-4 with the byte length in bits 3-0 — so unsigned
/// payload order is byte-lexicographic string order.
#[cfg(feature = "inline-short-strings")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShortString {
    bytes: [u8; 7],
    len: u8,
}

#[cfg(feature = "inline-short-strings")]
impl ShortString {
    pub fn new(s: &str) -> Option<ShortString> {
        let b = s.as_bytes();
        if b.len() > 7 {
            return None;
        }
        let mut bytes = [0u8; 7];
        bytes[..b.len()].copy_from_slice(b);
        Some(ShortString {
            bytes,
            len: b.len() as u8,
        })
    }

    pub fn as_str(&self) -> &str {
        // Constructed only from &str prefixes of whole length, so valid UTF-8.
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).expect("ShortString is UTF-8")
    }

    fn to_payload(self) -> u64 {
        let mut w = [0u8; 8];
        w[..7].copy_from_slice(&self.bytes);
        (u64::from_be_bytes(w) >> 4) | u64::from(self.len)
    }

    fn from_payload(payload: u64) -> Option<ShortString> {
        let len = (payload & 0xF) as u8;
        if len > 7 {
            return None;
        }
        let w = ((payload & !0xF) << 4).to_be_bytes();
        let mut bytes = [0u8; 7];
        bytes.copy_from_slice(&w[..7]);
        std::str::from_utf8(&bytes[..usize::from(len)]).ok()?;
        Some(ShortString { bytes, len })
    }
}

// ------------------------------------------------------------ value compare

/// SPARQL-style value comparison across inline ids: exact for
/// integer↔integer, integer↔decimal, and decimal↔decimal; via f64 when a
/// double is involved (M0 pragmatism); instant order for same-kind dateTimes.
/// `None` when either id has no inline value or the value spaces don't compare
/// (boolean↔numeric, date↔dateTime, NaN, …).
pub fn partial_cmp_value(a: TermId, b: TermId) -> Option<Ordering> {
    use InlineValue as V;
    let (va, vb) = (a.decode()?, b.decode()?);
    match (va, vb) {
        (V::Integer(x), V::Integer(y)) => Some(x.cmp(&y)),
        (V::Integer(x), V::Decimal { unscaled, scale }) => {
            Some(cmp_int_dec(i128::from(x), i128::from(unscaled), scale))
        }
        (V::Decimal { unscaled, scale }, V::Integer(y)) => {
            Some(cmp_int_dec(i128::from(y), i128::from(unscaled), scale).reverse())
        }
        (
            V::Decimal {
                unscaled: u1,
                scale: s1,
            },
            V::Decimal {
                unscaled: u2,
                scale: s2,
            },
        ) => Some(cmp_dec_dec(i128::from(u1), s1, i128::from(u2), s2)),
        (V::Double { value: x, .. }, _) => x.partial_cmp(&to_f64(vb)?),
        (_, V::Double { value: y, .. }) => to_f64(va)?.partial_cmp(&y),
        (V::Boolean(x), V::Boolean(y)) => Some(x.cmp(&y)),
        (V::DateTime(x), V::DateTime(y)) if x.date_only == y.date_only => {
            Some((x.seconds, x.millis).cmp(&(y.seconds, y.millis)))
        }
        #[cfg(feature = "inline-short-strings")]
        (V::ShortString(x), V::ShortString(y)) => Some(x.as_str().cmp(y.as_str())),
        _ => None,
    }
}

fn to_f64(v: InlineValue) -> Option<f64> {
    match v {
        InlineValue::Integer(i) => Some(i as f64),
        InlineValue::Decimal { unscaled, scale } => {
            Some(unscaled as f64 / 10f64.powi(i32::from(scale)))
        }
        InlineValue::Double { value, .. } => Some(value),
        _ => None,
    }
}

/// Exact compare of integer `a` against decimal `u·10⁻ˢ`, overflow-free:
/// compare against the decimal's floor, then break ties on its fraction.
fn cmp_int_dec(a: i128, u: i128, s: u8) -> Ordering {
    let (floor, has_frac) = if s == 0 {
        (u, false)
    } else if u32::from(s) <= 38 {
        let p = 10i128.pow(u32::from(s));
        (u.div_euclid(p), u.rem_euclid(p) != 0)
    } else {
        // |u| < 2⁵² < 10³⁸ < 10ˢ ⇒ |u·10⁻ˢ| < 1.
        (if u >= 0 { 0 } else { -1 }, u != 0)
    };
    match a.cmp(&floor) {
        Ordering::Equal if has_frac => Ordering::Less,
        ord => ord,
    }
}

/// Exact compare of two decimals by aligning to the larger scale.
fn cmp_dec_dec(u1: i128, s1: u8, u2: i128, s2: u8) -> Ordering {
    if s1 > s2 {
        return cmp_dec_dec(u2, s2, u1, s1).reverse();
    }
    if u1 == 0 {
        return 0.cmp(&u2);
    }
    match 10i128
        .checked_pow(u32::from(s2 - s1))
        .and_then(|p| u1.checked_mul(p))
    {
        Some(lhs) => lhs.cmp(&u2),
        // Overflow ⇒ |u1·10^Δ| > i128::MAX/10 ≫ 2⁵² ≥ |u2|: sign decides.
        None => {
            if u1 > 0 {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
    }
}

// ----------------------------------------------------------------- decimal

fn parse_decimal(lex: &str) -> Option<InlineValue> {
    let b = lex.as_bytes();
    let (neg, rest) = match b.first()? {
        b'+' => (false, &b[1..]),
        b'-' => (true, &b[1..]),
        _ => (false, b),
    };
    let (int_part, frac_part) = match rest.iter().position(|&c| c == b'.') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, &rest[rest.len()..]),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.iter().chain(frac_part).all(u8::is_ascii_digit) {
        return None;
    }
    let mut unscaled: i128 = 0;
    for &d in int_part.iter().chain(frac_part) {
        unscaled = unscaled
            .checked_mul(10)?
            .checked_add(i128::from(d - b'0'))?;
    }
    if neg {
        unscaled = -unscaled;
    }
    // Normalize to the canonical scale: minimal, but at least one fraction
    // digit — the canonical writer never emits "4" or "4.00", only "4.0".
    let mut scale = frac_part.len() as u32;
    while scale > 1 && unscaled % 10 == 0 {
        unscaled /= 10;
        scale -= 1;
    }
    if scale == 0 {
        unscaled = unscaled.checked_mul(10)?;
        scale = 1;
    }
    if scale > 255 || !(-(1i128 << 51)..1 << 51).contains(&unscaled) {
        return None;
    }
    Some(InlineValue::Decimal {
        unscaled: unscaled as i64,
        scale: scale as u8,
    })
}

fn render_decimal(unscaled: i64, scale: u8) -> String {
    let mut digits = unscaled.unsigned_abs().to_string();
    let scale = usize::from(scale);
    let mut out = String::with_capacity(digits.len() + 3);
    if unscaled < 0 {
        out.push('-');
    }
    if scale == 0 {
        // Unreachable via canonical parsing; rendered plainly for
        // hand-constructed values.
        out.push_str(&digits);
        return out;
    }
    if digits.len() <= scale {
        digits.insert_str(0, &"0".repeat(scale + 1 - digits.len()));
    }
    let point = digits.len() - scale;
    out.push_str(&digits[..point]);
    out.push('.');
    out.push_str(&digits[point..]);
    out
}

// ------------------------------------------------------------------ double

fn render_double(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_owned();
    }
    if v.is_infinite() {
        return (if v.is_sign_positive() { "INF" } else { "-INF" }).to_owned();
    }
    with_point_mantissa(format!("{v:E}"))
}

fn render_float(v: f32) -> String {
    if v.is_nan() {
        return "NaN".to_owned();
    }
    if v.is_infinite() {
        return (if v.is_sign_positive() { "INF" } else { "-INF" }).to_owned();
    }
    with_point_mantissa(format!("{v:E}"))
}

/// `{:E}` emits `1E0`; XSD canonical mantissas always carry a point (`1.0E0`).
fn with_point_mantissa(mut s: String) -> String {
    let e = s.find('E').expect("{:E} output contains E");
    if !s[..e].contains('.') {
        s.insert_str(e, ".0");
    }
    s
}

// ---------------------------------------------------------------- dateTime

fn parse_date_time(lex: &str, date_only: bool) -> Option<InlineDateTime> {
    if !lex.is_ascii() {
        return None;
    }
    let (body, tz_quarters) = split_tz(lex)?;
    let (date_part, time_part) = if date_only {
        (body, None)
    } else {
        let t = body.find('T')?;
        (&body[..t], Some(&body[t + 1..]))
    };
    let (y, m, d) = parse_date_fields(date_part)?;
    let (sod, millis) = match time_part {
        Some(t) => parse_time_fields(t)?,
        None => (0, 0),
    };
    let wall = days_from_civil(y, m, d) * 86400 + sod;
    let seconds = wall - i64::from(tz_quarters.unwrap_or(0)) * 900;
    if !(-(1i64 << 40)..1 << 40).contains(&seconds) {
        return None;
    }
    Some(InlineDateTime {
        date_only,
        seconds,
        millis,
        tz_quarters,
    })
}

/// Split a trailing timezone (`Z` or `±hh:mm`); `None` when a timezone is
/// present but not representable in quarter-hours (or out of XSD's ±14:00).
fn split_tz(lex: &str) -> Option<(&str, Option<i8>)> {
    if let Some(body) = lex.strip_suffix('Z') {
        return Some((body, Some(0)));
    }
    let b = lex.as_bytes();
    if b.len() >= 6 {
        let t = &b[b.len() - 6..];
        if (t[0] == b'+' || t[0] == b'-')
            && t[1].is_ascii_digit()
            && t[2].is_ascii_digit()
            && t[3] == b':'
            && t[4].is_ascii_digit()
            && t[5].is_ascii_digit()
        {
            let hh = i16::from((t[1] - b'0') * 10 + (t[2] - b'0'));
            let mm = i16::from((t[4] - b'0') * 10 + (t[5] - b'0'));
            if hh > 14 || (hh == 14 && mm != 0) || mm > 59 || mm % 15 != 0 {
                return None;
            }
            let q = (hh * 4 + mm / 15) as i8;
            return Some((
                &lex[..lex.len() - 6],
                Some(if t[0] == b'-' { -q } else { q }),
            ));
        }
    }
    Some((lex, None))
}

/// `YYYY-MM-DD` with optional leading `-` and ≥ 4 year digits, month/day
/// validated against the proleptic Gregorian calendar.
fn parse_date_fields(s: &str) -> Option<(i64, u32, u32)> {
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    if s.len() < 10 {
        return None;
    }
    let (ystr, md) = s.split_at(s.len() - 6);
    let b = md.as_bytes();
    if b[0] != b'-'
        || b[3] != b'-'
        || !(b[1].is_ascii_digit()
            && b[2].is_ascii_digit()
            && b[4].is_ascii_digit()
            && b[5].is_ascii_digit())
    {
        return None;
    }
    if ystr.len() < 4 || !ystr.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let y: i64 = ystr.parse().ok()?;
    // The i41-seconds payload spans ±~34.8k years; bail before day arithmetic
    // can overflow.
    if y > 40_000 {
        return None;
    }
    let y = if neg { -y } else { y };
    let m = u32::from((b[1] - b'0') * 10 + (b[2] - b'0'));
    let d = u32::from((b[4] - b'0') * 10 + (b[5] - b'0'));
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    Some((y, m, d))
}

/// `hh:mm:ss` with optional `.f{1,3}` fraction → (seconds of day, millis).
/// XSD's `24:00:00` is rejected: it never appears in canonical forms, so it
/// could never inline anyway.
fn parse_time_fields(s: &str) -> Option<(i64, u16)> {
    let b = s.as_bytes();
    if b.len() < 8 || b[2] != b':' || b[5] != b':' {
        return None;
    }
    for i in [0, 1, 3, 4, 6, 7] {
        if !b[i].is_ascii_digit() {
            return None;
        }
    }
    let h = i64::from((b[0] - b'0') * 10 + (b[1] - b'0'));
    let mi = i64::from((b[3] - b'0') * 10 + (b[4] - b'0'));
    let sec = i64::from((b[6] - b'0') * 10 + (b[7] - b'0'));
    if h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let millis = if b.len() > 8 {
        if b[8] != b'.' {
            return None;
        }
        let frac = &b[9..];
        // > 3 digits can never equal the canonical rendering (≤ 3 digits, no
        // trailing zeros), so reject outright.
        if frac.is_empty() || frac.len() > 3 || !frac.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let mut v: u16 = 0;
        for &d in frac {
            v = v * 10 + u16::from(d - b'0');
        }
        v * 10u16.pow(3 - frac.len() as u32)
    } else {
        0
    };
    Some((h * 3600 + mi * 60 + sec, millis))
}

fn render_date_time(dt: &InlineDateTime) -> String {
    use std::fmt::Write as _;
    let wall = dt.seconds + i64::from(dt.tz_quarters.unwrap_or(0)) * 900;
    let (y, m, d) = civil_from_days(wall.div_euclid(86400));
    let sod = wall.rem_euclid(86400);
    let mut out = String::with_capacity(32);
    if y < 0 {
        out.push('-');
    }
    let ya = y.unsigned_abs();
    if ya < 10_000 {
        let _ = write!(out, "{ya:04}");
    } else {
        let _ = write!(out, "{ya}");
    }
    let _ = write!(out, "-{m:02}-{d:02}");
    if !dt.date_only {
        let _ = write!(
            out,
            "T{:02}:{:02}:{:02}",
            sod / 3600,
            sod / 60 % 60,
            sod % 60
        );
        if dt.millis > 0 {
            let mut frac = format!("{:03}", dt.millis);
            while frac.ends_with('0') {
                frac.pop();
            }
            out.push('.');
            out.push_str(&frac);
        }
    }
    match dt.tz_quarters {
        None => {}
        Some(0) => out.push('Z'),
        Some(q) => {
            let sign = if q < 0 { '-' } else { '+' };
            let qa = i64::from(q).unsigned_abs();
            let _ = write!(out, "{sign}{:02}:{:02}", qa / 4, qa % 4 * 15);
        }
    }
    out
}

fn is_leap_year(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(y) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 in the proleptic Gregorian calendar
/// (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400); // [0, 399]
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 }); // [0, 11]
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719_468
}

/// Inverse of [`days_from_civil`] (Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vocab;

    fn inline(lex: &str, dt: &str) -> Option<TermId> {
        TermId::try_inline(lex, dt)
    }

    fn round_trip(lex: &str, dt: &str) -> TermId {
        let id = inline(lex, dt).unwrap_or_else(|| panic!("{lex:?}^^{dt} should inline"));
        let v = id.decode().unwrap();
        assert_eq!(v.canonical_lexical(), lex, "lexical round-trip");
        assert_eq!(v.datatype_iri(), dt, "datatype round-trip");
        assert_eq!(TermId::inline(v), Some(id), "bit round-trip");
        id
    }

    #[test]
    fn dict_and_sentinels() {
        let id = TermId::dict(Section::Objects, 12345);
        assert_eq!(id.tag(), Some(Tag::DictRef));
        assert_eq!(id.dict_ref(), Some((Section::Objects, 12345)));
        assert_eq!(id.decode(), None);

        assert!(TermId::NULL.is_null());
        assert_eq!(TermId::dict(Section::Shared, 0), TermId::NULL);
        assert_eq!(TermId::DEFAULT_GRAPH.tag(), Some(Tag::Sentinel));
        assert_ne!(TermId::DEFAULT_GRAPH, TermId::UNDEF);

        let tt = TermId::triple_term(7);
        assert_eq!(tt.triple_term_ordinal(), Some(7));
        assert_eq!(tt.dict_ref(), None);
    }

    #[test]
    fn integer_canonical_only() {
        round_trip("42", vocab::XSD_INTEGER);
        round_trip("-42", vocab::XSD_INTEGER);
        round_trip("0", vocab::XSD_INTEGER);
        for bad in ["042", "+42", "-0", " 42", "4.0", "", "abc"] {
            assert_eq!(inline(bad, vocab::XSD_INTEGER), None, "{bad:?}");
        }
        // Range edges: ±2⁵⁹.
        let max = (1i64 << 59) - 1;
        let min = -(1i64 << 59);
        round_trip(&max.to_string(), vocab::XSD_INTEGER);
        round_trip(&min.to_string(), vocab::XSD_INTEGER);
        assert_eq!(inline(&(max + 1).to_string(), vocab::XSD_INTEGER), None);
        assert_eq!(inline(&(min - 1).to_string(), vocab::XSD_INTEGER), None);
    }

    #[test]
    fn integer_payload_order_is_value_order() {
        let values = [-(1i64 << 59), -100, -1, 0, 1, 7, 1 << 40, (1 << 59) - 1];
        let ids: Vec<TermId> = values
            .iter()
            .map(|v| round_trip(&v.to_string(), vocab::XSD_INTEGER))
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn decimal_requires_point_and_canonical_form() {
        round_trip("4.0", vocab::XSD_DECIMAL);
        round_trip("-0.5", vocab::XSD_DECIMAL);
        round_trip("0.0", vocab::XSD_DECIMAL);
        round_trip("123.456", vocab::XSD_DECIMAL);
        // 251 zeros then a 1: unscaled 1, scale 252 — legal and tiny.
        let tiny = format!("0.{}1", "0".repeat(251));
        round_trip(&tiny, vocab::XSD_DECIMAL);
        for bad in ["4", "4.", ".5", "4.00", "+4.0", "-0.0", "04.0", "4.0.0", ""] {
            assert_eq!(inline(bad, vocab::XSD_DECIMAL), None, "{bad:?}");
        }
    }

    #[test]
    fn double_and_float() {
        round_trip("4.2E9", vocab::XSD_DOUBLE);
        round_trip("1.0E0", vocab::XSD_DOUBLE);
        round_trip("-0.0E0", vocab::XSD_DOUBLE);
        round_trip("INF", vocab::XSD_DOUBLE);
        round_trip("-INF", vocab::XSD_DOUBLE);
        round_trip("NaN", vocab::XSD_DOUBLE);
        // Non-canonical spellings.
        for bad in ["0.1", "1E0", "42", "inf", "Infinity", "+INF", "1.0e0"] {
            assert_eq!(inline(bad, vocab::XSD_DOUBLE), None, "{bad:?}");
        }
        // Canonical but needs all 52 mantissa bits → low-5 test fails →
        // dictionary.
        assert_eq!(inline("1.0E-1", vocab::XSD_DOUBLE), None);

        // The same mantissa as f32 widens exactly and inlines.
        let f = round_trip("4.2E0", vocab::XSD_FLOAT);
        match f.decode().unwrap() {
            InlineValue::Double {
                declared_float,
                value,
            } => {
                assert!(declared_float);
                assert_eq!(value, f64::from(4.2f32));
            }
            _ => panic!(),
        }
        // Same lexical as double: 4.2 needs the full f64 mantissa → None.
        assert_eq!(inline("4.2E0", vocab::XSD_DOUBLE), None);
    }

    #[test]
    fn boolean() {
        let t = round_trip("true", vocab::XSD_BOOLEAN);
        let f = round_trip("false", vocab::XSD_BOOLEAN);
        assert!(f < t);
        assert_eq!(inline("1", vocab::XSD_BOOLEAN), None);
        assert_eq!(inline("0", vocab::XSD_BOOLEAN), None);
        assert_eq!(inline("True", vocab::XSD_BOOLEAN), None);
    }

    #[test]
    fn date_time_round_trips() {
        let epoch = round_trip("1970-01-01T00:00:00Z", vocab::XSD_DATE_TIME);
        match epoch.decode().unwrap() {
            InlineValue::DateTime(dt) => {
                assert_eq!((dt.seconds, dt.millis, dt.tz_quarters), (0, 0, Some(0)));
                assert!(!dt.date_only);
            }
            _ => panic!(),
        }
        round_trip("2020-02-29T12:34:56.789+05:30", vocab::XSD_DATE_TIME);
        round_trip("2026-07-11T08:15:00-08:00", vocab::XSD_DATE_TIME);
        round_trip("1969-12-31T23:59:59.001Z", vocab::XSD_DATE_TIME);
        round_trip("0000-01-01T00:00:00", vocab::XSD_DATE_TIME); // no tz, year 0
        round_trip("-0055-03-15T12:00:00Z", vocab::XSD_DATE_TIME);
        round_trip("12026-01-01T00:00:00Z", vocab::XSD_DATE_TIME); // 5-digit year

        for bad in [
            "2021-02-29T00:00:00Z",      // not a leap year
            "2020-13-01T00:00:00Z",      // month 13
            "2020-01-32T00:00:00Z",      // day 32
            "2020-01-01T24:00:00Z",      // 24:00 never canonical
            "2020-01-01T00:00:00+00:00", // canonical tz is Z
            "2020-01-01T00:00:00-00:00", // ditto
            "2020-01-01T00:00:00.120Z",  // trailing zero in fraction
            "2020-01-01T00:00:00.1234Z", // sub-millisecond
            "2020-01-01T00:00:00+05:17", // not quarter-hour
            "2020-01-01T00:00:00+15:00", // beyond ±14:00
            "2020-01-01",                // date lexical, wrong datatype
            "2020-1-01T00:00:00Z",       // narrow month
            "02020-01-01T00:00:00Z",     // padded year
        ] {
            assert_eq!(inline(bad, vocab::XSD_DATE_TIME), None, "{bad:?}");
        }
    }

    #[test]
    fn date_round_trips() {
        round_trip("2026-07-11", vocab::XSD_DATE);
        round_trip("2026-07-11Z", vocab::XSD_DATE);
        round_trip("2026-07-11+05:30", vocab::XSD_DATE);
        round_trip("-0001-12-31", vocab::XSD_DATE);
        assert_eq!(inline("2026-07-11T00:00:00Z", vocab::XSD_DATE), None);
        assert_eq!(inline("2026-7-11", vocab::XSD_DATE), None);
    }

    #[test]
    fn date_time_instant_order() {
        // Same instant, different tz → equal value, distinct ids.
        let a = round_trip("2020-01-01T05:00:00Z", vocab::XSD_DATE_TIME);
        let b = round_trip("2020-01-01T07:00:00+02:00", vocab::XSD_DATE_TIME);
        assert_ne!(a, b);
        assert_eq!(partial_cmp_value(a, b), Some(Ordering::Equal));

        let c = round_trip("2020-01-01T00:00:00Z", vocab::XSD_DATE_TIME);
        assert_eq!(partial_cmp_value(c, a), Some(Ordering::Less));

        // date vs dateTime: incomparable.
        let d = round_trip("2020-01-01", vocab::XSD_DATE);
        assert_eq!(partial_cmp_value(d, a), None);
    }

    #[test]
    fn cross_numeric_value_compare() {
        let i42 = round_trip("42", vocab::XSD_INTEGER);
        let d42 = round_trip("42.0", vocab::XSD_DECIMAL);
        let d425 = round_trip("42.5", vocab::XSD_DECIMAL);
        let f42 = round_trip("4.2E1", vocab::XSD_DOUBLE);
        assert_eq!(partial_cmp_value(i42, d42), Some(Ordering::Equal));
        assert_eq!(partial_cmp_value(i42, d425), Some(Ordering::Less));
        assert_eq!(partial_cmp_value(d425, i42), Some(Ordering::Greater));
        assert_eq!(partial_cmp_value(i42, f42), Some(Ordering::Equal));

        // Tiny decimal vs integers, exact despite huge scale.
        let tiny = round_trip(&format!("0.{}1", "0".repeat(251)), vocab::XSD_DECIMAL);
        let zero = round_trip("0", vocab::XSD_INTEGER);
        let one = round_trip("1", vocab::XSD_INTEGER);
        assert_eq!(partial_cmp_value(zero, tiny), Some(Ordering::Less));
        assert_eq!(partial_cmp_value(one, tiny), Some(Ordering::Greater));
        let neg_tiny = round_trip(&format!("-0.{}1", "0".repeat(251)), vocab::XSD_DECIMAL);
        assert_eq!(partial_cmp_value(zero, neg_tiny), Some(Ordering::Greater));
        assert_eq!(partial_cmp_value(tiny, neg_tiny), Some(Ordering::Greater));

        // NaN and cross-kind comparisons are undefined.
        let nan = round_trip("NaN", vocab::XSD_DOUBLE);
        assert_eq!(partial_cmp_value(nan, i42), None);
        let t = round_trip("true", vocab::XSD_BOOLEAN);
        assert_eq!(partial_cmp_value(t, i42), None);
        assert_eq!(
            partial_cmp_value(TermId::dict(Section::Shared, 1), i42),
            None
        );
    }

    #[test]
    fn unknown_datatypes_never_inline() {
        assert_eq!(inline("42", "http://ex.example/dt"), None);
        assert_eq!(inline("42", vocab::XSD_DATE), None);
        #[cfg(not(feature = "inline-short-strings"))]
        assert_eq!(inline("hi", vocab::XSD_STRING), None);
    }

    #[test]
    fn civil_calendar_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        for days in (-1_000_000..1_000_000).step_by(7919) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
            assert!((1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m));
        }
    }

    #[cfg(feature = "inline-short-strings")]
    #[test]
    fn short_strings() {
        let id = round_trip("hi", vocab::XSD_STRING);
        match id.decode().unwrap() {
            InlineValue::ShortString(s) => assert_eq!(s.as_str(), "hi"),
            _ => panic!(),
        }
        round_trip("", vocab::XSD_STRING);
        round_trip("1234567", vocab::XSD_STRING);
        round_trip("héllo", vocab::XSD_STRING); // 6 bytes UTF-8
        assert_eq!(inline("12345678", vocab::XSD_STRING), None);

        // Payload order = byte-lexicographic order, including prefixes.
        let words = ["", "a", "ab", "abc", "b", "zzzzzzz"];
        let ids: Vec<TermId> = words
            .iter()
            .map(|w| round_trip(w, vocab::XSD_STRING))
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        assert_eq!(partial_cmp_value(ids[1], ids[4]), Some(Ordering::Less));
    }
}
