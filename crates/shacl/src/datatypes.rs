//! XSD value semantics: ordering, and lexical well-formedness.
//!
//! `sh:minInclusive` and friends compare by *value*, not by lexical form, and
//! follow SPARQL's operator table: numerics compare across the whole numeric
//! tower, strings compare lexically, and anything incomparable simply fails the
//! constraint rather than erroring.

use std::cmp::Ordering;

use oxsdatatypes::{
    Date, DateTime, DayTimeDuration, Decimal, Double, Duration, GDay, GMonth, GMonthDay, GYear,
    GYearMonth, Time, YearMonthDuration,
};

use crate::model::{TermId, TermStore, Vocab};

/// A literal reduced to something comparable.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    /// Exact where possible; `Float` only once precision has already been lost.
    Int(i128),
    /// An integer too large for `i128`. XSD's integer value space has unbounded
    /// precision, so these are legal literals; held as a normalised lexical
    /// form (sign, then digits, no leading zeros) and compared digit-wise.
    BigInt(bool, String),
    Dec(Decimal),
    Float(f64),
    Str(String),
    Bool(bool),
    DateTime(DateTime),
    Date(Date),
    Time(Time),
    /// Only *partially* ordered: `P1M` and `P30D` have no defined order,
    /// because the length of a month varies. `PartialOrd` returns `None`
    /// there, which is the answer the range constraints want.
    Duration(Duration),
}

/// Compares two terms by value, per SPARQL's ordering operators.
///
/// Returns `None` when the pair is not comparable — different value spaces, a
/// non-literal, or an ill-formed lexical form. Callers treat that as a failed
/// comparison, which is what the range constraints require.
pub fn compare(a: TermId, b: TermId, store: &TermStore, vocab: &Vocab) -> Option<Ordering> {
    let va = value_of(a, store, vocab)?;
    let vb = value_of(b, store, vocab)?;
    compare_values(&va, &vb)
}

fn compare_values(a: &Value, b: &Value) -> Option<Ordering> {
    use Value::*;
    match (a, b) {
        (Int(x), Int(y)) => Some(x.cmp(y)),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (Str(x), Str(y)) => Some(x.cmp(y)),
        (DateTime(x), DateTime(y)) => x.partial_cmp(y),
        (Date(x), Date(y)) => x.partial_cmp(y),
        (Time(x), Time(y)) => x.partial_cmp(y),

        // Mixed numerics promote to the wider type, as SPARQL requires.
        (Dec(x), Dec(y)) => x.partial_cmp(y),
        (Int(x), Dec(y)) => Decimal::try_from(*x).ok()?.partial_cmp(y),
        (Dec(x), Int(y)) => x.partial_cmp(&Decimal::try_from(*y).ok()?),
        (Float(x), Float(y)) => x.partial_cmp(y),
        (Int(x), Float(y)) => (*x as f64).partial_cmp(y),
        (Float(x), Int(y)) => x.partial_cmp(&(*y as f64)),
        (Dec(x), Float(y)) => f64::from(Double::from(*x)).partial_cmp(y),
        (Float(x), Dec(y)) => x.partial_cmp(&f64::from(Double::from(*y))),

        (Duration(x), Duration(y)) => x.partial_cmp(y),

        // A `BigInt` exists only because it did not fit `i128`, whose range
        // already covers `Decimal`'s — so against either of those its sign
        // settles it without any digits being looked at.
        (BigInt(sx, dx), BigInt(sy, dy)) => Some(cmp_bigint(*sx, dx, *sy, dy)),
        (BigInt(s, _), Int(_) | Dec(_)) => Some(if *s {
            Ordering::Less
        } else {
            Ordering::Greater
        }),
        (Int(_) | Dec(_), BigInt(s, _)) => Some(if *s {
            Ordering::Greater
        } else {
            Ordering::Less
        }),
        // Floats reach these magnitudes, so this one is worth converting for.
        (BigInt(s, d), Float(y)) => bigint_as_f64(*s, d).partial_cmp(y),
        (Float(x), BigInt(s, d)) => x.partial_cmp(&bigint_as_f64(*s, d)),

        // Different value spaces are incomparable, not equal.
        _ => None,
    }
}

/// Orders two normalised big integers: sign first, then magnitude, which for
/// digits without leading zeros is length before lexicographic order.
fn cmp_bigint(neg_a: bool, a: &str, neg_b: bool, b: &str) -> Ordering {
    match (neg_a, neg_b) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        _ => {
            let magnitude = a.len().cmp(&b.len()).then_with(|| a.cmp(b));
            // Among negatives the larger magnitude is the smaller number.
            if neg_a {
                magnitude.reverse()
            } else {
                magnitude
            }
        }
    }
}

fn bigint_as_f64(neg: bool, digits: &str) -> f64 {
    // Beyond f64's range this saturates to infinity, which still orders
    // correctly against any finite float.
    let m = digits.parse::<f64>().unwrap_or(f64::INFINITY);
    if neg { -m } else { m }
}

/// Splits an XSD integer literal into sign and digits, rejecting anything that
/// is not `[+-]?[0-9]+` and dropping leading zeros so lengths are comparable.
///
/// XSD permits no whitespace, no decimal point and no exponent here.
fn normalise_integer(lex: &str) -> Option<(bool, String)> {
    let (neg, rest) = match lex.as_bytes().first()? {
        b'-' => (true, &lex[1..]),
        b'+' => (false, &lex[1..]),
        _ => (false, lex),
    };
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let trimmed = rest.trim_start_matches('0');
    if trimmed.is_empty() {
        // Every spelling of zero normalises to one, and is never negative.
        return Some((false, "0".to_string()));
    }
    Some((neg, trimmed.to_string()))
}

fn value_of(t: TermId, store: &TermStore, vocab: &Vocab) -> Option<Value> {
    let lex = store.lexical_form(t)?;
    let dt = store.datatype(t)?;

    // Language-tagged strings compare as strings.
    if store.language(t).is_some() {
        return Some(Value::Str(lex.to_string()));
    }

    Some(match dt {
        _ if dt == vocab.xsd_string || dt == vocab.xsd_anyURI => Value::Str(lex.to_string()),
        _ if dt == vocab.xsd_boolean => Value::Bool(match lex {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return None,
        }),
        _ if is_integer_type(dt, vocab) => {
            let (neg, digits) = normalise_integer(lex)?;
            // The common case stays exact and cheap; only literals past i128
            // fall back to digit-wise comparison.
            match lex.parse::<i128>() {
                Ok(n) => Value::Int(n),
                Err(_) => Value::BigInt(neg, digits),
            }
        }
        _ if dt == vocab.xsd_decimal => Value::Dec(lex.parse::<Decimal>().ok()?),
        _ if dt == vocab.xsd_float || dt == vocab.xsd_double => {
            Value::Float(parse_xsd_double(lex)?)
        }
        _ if dt == vocab.xsd_dateTime => Value::DateTime(lex.parse::<DateTime>().ok()?),
        _ if dt == vocab.xsd_date => Value::Date(lex.parse::<Date>().ok()?),
        _ if dt == vocab.xsd_time => Value::Time(lex.parse::<Time>().ok()?),
        _ if dt == vocab.xsd_duration => Value::Duration(lex.parse::<Duration>().ok()?),
        _ => return None,
    })
}

fn is_integer_type(dt: TermId, v: &Vocab) -> bool {
    dt == v.xsd_integer
        || dt == v.xsd_long
        || dt == v.xsd_int
        || dt == v.xsd_short
        || dt == v.xsd_byte
        || dt == v.xsd_nonNegativeInteger
        || dt == v.xsd_positiveInteger
        || dt == v.xsd_nonPositiveInteger
        || dt == v.xsd_negativeInteger
        || dt == v.xsd_unsignedLong
        || dt == v.xsd_unsignedInt
        || dt == v.xsd_unsignedShort
        || dt == v.xsd_unsignedByte
}

/// XSD doubles admit `INF`, `-INF` and `NaN`, which Rust spells differently.
fn parse_xsd_double(lex: &str) -> Option<f64> {
    match lex {
        "INF" | "+INF" => Some(f64::INFINITY),
        "-INF" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        // Rust accepts "inf"/"nan" spellings that XSD does not.
        s if s.eq_ignore_ascii_case("inf")
            || s.eq_ignore_ascii_case("infinity")
            || s.eq_ignore_ascii_case("nan") =>
        {
            None
        }
        s => s.parse::<f64>().ok(),
    }
}

/// Whether `lex` is a well-formed lexical form for datatype `dt`.
///
/// `sh:datatype` requires more than a matching datatype IRI: a literal whose
/// lexical form is invalid for its datatype — `"aldi"^^xsd:integer` — is
/// ill-formed and must fail.
pub fn is_well_formed(lex: &str, dt: TermId, vocab: &Vocab) -> bool {
    if dt == vocab.xsd_string || dt == vocab.rdf_langString || dt == vocab.xsd_anyURI {
        return true;
    }
    if dt == vocab.xsd_boolean {
        return matches!(lex, "true" | "false" | "1" | "0");
    }
    if is_integer_type(dt, vocab) {
        return is_well_formed_integer(lex, dt, vocab);
    }
    if dt == vocab.xsd_decimal {
        return lex.parse::<Decimal>().is_ok();
    }
    if dt == vocab.xsd_float || dt == vocab.xsd_double {
        return parse_xsd_double(lex).is_some();
    }
    if dt == vocab.xsd_dateTime {
        return lex.parse::<DateTime>().is_ok();
    }
    if dt == vocab.xsd_date {
        return lex.parse::<Date>().is_ok();
    }
    if dt == vocab.xsd_time {
        return lex.parse::<Time>().is_ok();
    }
    if dt == vocab.xsd_duration {
        return lex.parse::<Duration>().is_ok();
    }
    // The two duration subtypes are not covered by `xsd:duration` above. Each
    // is a restriction of its value space rather than only of its lexical one,
    // so `"P1Y"^^xsd:dayTimeDuration` parses as a duration and still is not one
    // of these: a year is not a day-time component. Parsing as the subtype is
    // what catches that; parsing as `Duration` would not.
    if dt == vocab.xsd_dayTimeDuration {
        return lex.parse::<DayTimeDuration>().is_ok();
    }
    if dt == vocab.xsd_yearMonthDuration {
        return lex.parse::<YearMonthDuration>().is_ok();
    }
    // The gregorian types. Their lexical forms are specific and easy to get
    // wrong by hand -- `gYearMonth` is `1066-10`, `gMonth` is `--10`, `gDay` is
    // `---14` -- which is the reason to check them rather than hope.
    if dt == vocab.xsd_gYear {
        return lex.parse::<GYear>().is_ok();
    }
    if dt == vocab.xsd_gYearMonth {
        return lex.parse::<GYearMonth>().is_ok();
    }
    if dt == vocab.xsd_gMonth {
        return lex.parse::<GMonth>().is_ok();
    }
    if dt == vocab.xsd_gDay {
        return lex.parse::<GDay>().is_ok();
    }
    if dt == vocab.xsd_gMonthDay {
        return is_well_formed_g_month_day(lex);
    }
    // An unknown datatype places no constraint on its lexical space.
    //
    // That default is right for a datatype this engine has never heard of, and
    // it was wrong for the seven above, which it had the parsers for all along:
    // `oxsdatatypes` is already a dependency and implements every one. Until
    // 0.3.9 a malformed `xsd:gYear` satisfied `sh:datatype`, so validation
    // passed data it was asked to reject. Reported from work on tarka.
    true
}

/// `xsd:gMonthDay`, the leap day included.
///
/// `oxsdatatypes` rejects `--02-29`: with no year to go on it measures February
/// at 28 days. XSD 1.1 requires the opposite. gMonthDay has no year in its value
/// space, so S3.3.12.1 caps February at 29 -- "The day value must be no more
/// than 30 if month is one of 4, 6, 9, or 11, and no more than 29 if month is
/// 2" -- and S3.3.12 says as much in passing: "days beyond 28 cannot occur in
/// all Februaries; 29 is nonetheless permitted". The 28-or-29 leap-year split
/// belongs to `xsd:date`, which has a year to apply it to.
///
/// The parse is therefore retried with the single day the library miscounts
/// swapped for the one before it. Everything else about the form is still
/// checked by the library, the timezone included; only February's upper bound
/// comes from the specification rather than from the dependency.
fn is_well_formed_g_month_day(lex: &str) -> bool {
    if lex.parse::<GMonthDay>().is_ok() {
        return true;
    }
    match lex.strip_prefix("--02-29") {
        Some(timezone) => format!("--02-28{timezone}").parse::<GMonthDay>().is_ok(),
        None => false,
    }
}

fn is_well_formed_integer(lex: &str, dt: TermId, v: &Vocab) -> bool {
    // XSD integers permit a leading sign but no whitespace or decimal point.
    let Some((neg, digits)) = normalise_integer(lex) else {
        return false;
    };

    // The unbounded types are decided on the lexical form alone. Their value
    // space has arbitrary precision, so a 40-digit literal is perfectly valid
    // and must not be called ill-formed just because it overflows `i128`.
    let is_zero = digits == "0";
    match dt {
        _ if dt == v.xsd_integer => return true,
        _ if dt == v.xsd_nonNegativeInteger => return !neg,
        _ if dt == v.xsd_positiveInteger => return !neg && !is_zero,
        _ if dt == v.xsd_nonPositiveInteger => return neg || is_zero,
        _ if dt == v.xsd_negativeInteger => return neg && !is_zero,
        _ => {}
    }

    // What is left is the fixed-width types, every one of which fits `i128`,
    // so failing to parse here means out of range rather than ill-formed.
    let Ok(n) = lex.parse::<i128>() else {
        return false;
    };
    let in_range = |lo: i128, hi: i128| n >= lo && n <= hi;
    match dt {
        _ if dt == v.xsd_long => in_range(i64::MIN as i128, i64::MAX as i128),
        _ if dt == v.xsd_int => in_range(i32::MIN as i128, i32::MAX as i128),
        _ if dt == v.xsd_short => in_range(i16::MIN as i128, i16::MAX as i128),
        _ if dt == v.xsd_byte => in_range(i8::MIN as i128, i8::MAX as i128),
        _ if dt == v.xsd_unsignedLong => in_range(0, u64::MAX as i128),
        _ if dt == v.xsd_unsignedInt => in_range(0, u32::MAX as i128),
        _ if dt == v.xsd_unsignedShort => in_range(0, u16::MAX as i128),
        _ if dt == v.xsd_unsignedByte => in_range(0, u8::MAX as i128),
        _ => true,
    }
}

/// Whether `tag` matches the basic language range `range`, per RFC 4647.
///
/// `sh:languageIn` compares language *ranges*: `en` matches `en-GB`, but `en-GB`
/// does not match `en`.
pub fn language_matches(tag: &str, range: &str) -> bool {
    if range == "*" {
        return !tag.is_empty();
    }
    if tag.eq_ignore_ascii_case(range) {
        return true;
    }
    tag.len() > range.len()
        && tag.as_bytes()[range.len()] == b'-'
        && tag[..range.len()].eq_ignore_ascii_case(range)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TermStore;

    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

    struct F {
        store: TermStore,
        vocab: Vocab,
    }

    impl F {
        fn new() -> Self {
            let mut store = TermStore::new();
            let vocab = Vocab::new(&mut store);
            Self { store, vocab }
        }
        fn lit(&mut self, lex: &str, dt: &str) -> TermId {
            self.store.literal(lex, &format!("{XSD}{dt}"), None)
        }
        fn cmp(&mut self, a: (&str, &str), b: (&str, &str)) -> Option<Ordering> {
            let x = self.lit(a.0, a.1);
            let y = self.lit(b.0, b.1);
            compare(x, y, &self.store, &self.vocab)
        }
        fn wf(&mut self, lex: &str, dt: &str) -> bool {
            let t = self.lit(lex, dt);
            is_well_formed(lex, self.store.datatype(t).unwrap(), &self.vocab)
        }
    }

    /// Each of these seven was accepted whatever its lexical form until 0.3.9,
    /// because `is_well_formed` had no branch for it and fell through to "an
    /// unknown datatype places no constraint". They are not unknown: every one
    /// has a parser in `oxsdatatypes`, which this crate already depends on.
    ///
    /// The good and bad cases are paired on purpose. A checker that rejects
    /// everything passes a test that only feeds it malformed input, and would
    /// be worse than the gap it replaced.
    #[test]
    fn checks_the_gregorian_types() {
        let mut f = F::new();

        assert!(f.wf("1066", "gYear"));
        assert!(f.wf("-0044", "gYear"));
        assert!(f.wf("1066Z", "gYear"));
        assert!(f.wf("1066+01:00", "gYear"));
        assert!(!f.wf("not-a-year", "gYear"));
        assert!(!f.wf("66", "gYear"), "a year takes at least four digits");
        assert!(!f.wf("1066-10", "gYear"), "that is a gYearMonth");

        assert!(f.wf("1066-10", "gYearMonth"));
        assert!(f.wf("-0044-03", "gYearMonth"));
        assert!(!f.wf("1066", "gYearMonth"));
        assert!(!f.wf("1066-13", "gYearMonth"), "there is no month 13");

        assert!(f.wf("--10", "gMonth"));
        assert!(f.wf("--02Z", "gMonth"));
        assert!(!f.wf("10", "gMonth"), "gMonth is written --MM");
        assert!(!f.wf("--13", "gMonth"));

        assert!(f.wf("---14", "gDay"));
        assert!(f.wf("---31", "gDay"), "no month, so 31 is always allowed");
        assert!(!f.wf("14", "gDay"), "gDay is written ---DD");
        assert!(!f.wf("---32", "gDay"));

        assert!(f.wf("--10-14", "gMonthDay"));
        assert!(!f.wf("February", "gMonthDay"));
        assert!(!f.wf("--04-31", "gMonthDay"), "April has thirty days");
    }

    /// `--02-29` is a valid `xsd:gMonthDay` and `oxsdatatypes` rejects it,
    /// having measured February at 28 days with no year to go on. XSD 1.1
    /// S3.3.12.1 caps February at 29 for this type precisely because the year
    /// is absent, and S3.3.12 spells it out: "days beyond 28 cannot occur in
    /// all Februaries; 29 is nonetheless permitted".
    ///
    /// Pinned separately from the rest because the compensation for it is ours
    /// rather than the library's, so a future version of the dependency fixing
    /// this must not quietly change the answer, and the surrounding bounds must
    /// stay the library's.
    #[test]
    fn allows_the_leap_day_as_a_g_month_day() {
        let mut f = F::new();
        assert!(f.wf("--02-29", "gMonthDay"));
        assert!(f.wf("--02-29Z", "gMonthDay"));
        assert!(f.wf("--02-29+01:00", "gMonthDay"));
        assert!(f.wf("--02-28", "gMonthDay"));
        // The exception is one day wide, and does not loosen anything round it.
        assert!(!f.wf("--02-30", "gMonthDay"));
        assert!(!f.wf("--02-29!", "gMonthDay"));
        assert!(!f.wf("--02-29+99:00", "gMonthDay"));
    }

    /// Both duration subtypes restrict the *value* space, not only the lexical
    /// one, so `"P1Y"` parses perfectly as a duration and is still not a
    /// dayTimeDuration. Checking them against `Duration` would have let both
    /// through, which is why each parses as its own type.
    #[test]
    fn checks_the_duration_subtypes() {
        let mut f = F::new();

        assert!(f.wf("P1DT2H", "dayTimeDuration"));
        assert!(f.wf("PT1H", "dayTimeDuration"));
        assert!(f.wf("-P1DT2H3M4S", "dayTimeDuration"));
        assert!(!f.wf("P1Y", "dayTimeDuration"), "a year is not day-time");
        assert!(!f.wf("P1M", "dayTimeDuration"), "nor is a month");
        assert!(!f.wf("one hour", "dayTimeDuration"));

        assert!(f.wf("P1Y2M", "yearMonthDuration"));
        assert!(f.wf("P1Y", "yearMonthDuration"));
        assert!(f.wf("-P2Y6M", "yearMonthDuration"));
        assert!(
            !f.wf("PT1H", "yearMonthDuration"),
            "an hour is not year-month"
        );
        assert!(!f.wf("P1D", "yearMonthDuration"), "nor is a day");

        // `xsd:duration` itself still takes either, as it always did.
        assert!(f.wf("P1Y", "duration"));
        assert!(f.wf("PT1H", "duration"));
        assert!(!f.wf("P", "duration"));
    }

    /// The fall-through is still the right answer for a datatype the engine
    /// genuinely does not know: SHACL asks whether the literal is ill-formed,
    /// and about someone's own datatype this engine has no opinion.
    #[test]
    fn an_unknown_datatype_still_constrains_nothing() {
        let mut f = F::new();
        let t = f
            .store
            .literal("whatever", "http://example.org/ns#Custom", None);
        assert!(is_well_formed(
            "whatever",
            f.store.datatype(t).unwrap(),
            &f.vocab
        ));
    }

    #[test]
    fn compares_integers_exactly() {
        let mut f = F::new();
        assert_eq!(
            f.cmp(("2", "integer"), ("10", "integer")),
            Some(Ordering::Less)
        );
        assert_eq!(
            f.cmp(("10", "integer"), ("10", "integer")),
            Some(Ordering::Equal)
        );
        // Beyond f64's exact range: must not collapse to equal.
        assert_eq!(
            f.cmp(
                ("9007199254740993", "integer"),
                ("9007199254740992", "integer")
            ),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn compares_across_the_numeric_tower() {
        let mut f = F::new();
        assert_eq!(
            f.cmp(("2", "integer"), ("2.5", "decimal")),
            Some(Ordering::Less)
        );
        assert_eq!(
            f.cmp(("2.0", "decimal"), ("2", "integer")),
            Some(Ordering::Equal)
        );
        assert_eq!(
            f.cmp(("1", "integer"), ("1.5e0", "double")),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn compares_strings_booleans_and_dates() {
        let mut f = F::new();
        assert_eq!(
            f.cmp(("a", "string"), ("b", "string")),
            Some(Ordering::Less)
        );
        assert_eq!(
            f.cmp(("false", "boolean"), ("true", "boolean")),
            Some(Ordering::Less)
        );
        assert_eq!(
            f.cmp(("2020-01-01", "date"), ("2021-01-01", "date")),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn different_value_spaces_are_incomparable() {
        let mut f = F::new();
        assert_eq!(f.cmp(("1", "integer"), ("a", "string")), None);
        assert_eq!(f.cmp(("true", "boolean"), ("1", "integer")), None);
        // An ill-formed literal has no value, so nothing compares to it.
        assert_eq!(f.cmp(("aldi", "integer"), ("1", "integer")), None);
    }

    #[test]
    fn non_literals_are_incomparable() {
        let mut f = F::new();
        let iri = f.store.named_node("http://ex/a");
        let one = f.lit("1", "integer");
        assert_eq!(compare(iri, one, &f.store, &f.vocab), None);
    }

    #[test]
    fn detects_ill_formed_literals() {
        let f = F::new();
        let v = &f.vocab;
        assert!(is_well_formed("42", v.xsd_integer, v));
        assert!(!is_well_formed("aldi", v.xsd_integer, v));
        assert!(!is_well_formed("4.2", v.xsd_integer, v));
        assert!(is_well_formed("4.2", v.xsd_decimal, v));
        assert!(is_well_formed("anything at all", v.xsd_string, v));
        assert!(!is_well_formed("yes", v.xsd_boolean, v));
        assert!(is_well_formed("2020-01-01", v.xsd_date, v));
        assert!(!is_well_formed("2020-13-01", v.xsd_date, v));
    }

    #[test]
    fn enforces_derived_integer_ranges() {
        let f = F::new();
        let v = &f.vocab;
        assert!(is_well_formed("-1", v.xsd_integer, v));
        assert!(!is_well_formed("-1", v.xsd_nonNegativeInteger, v));
        assert!(!is_well_formed("0", v.xsd_positiveInteger, v));
        assert!(is_well_formed("127", v.xsd_byte, v));
        assert!(!is_well_formed("128", v.xsd_byte, v));
    }

    #[test]
    fn xsd_doubles_use_xsd_spellings() {
        let f = F::new();
        let v = &f.vocab;
        assert!(is_well_formed("INF", v.xsd_double, v));
        assert!(is_well_formed("NaN", v.xsd_double, v));
        assert!(is_well_formed("1.5e3", v.xsd_double, v));
        assert!(
            !is_well_formed("Infinity", v.xsd_double, v),
            "not an XSD spelling"
        );
    }

    #[test]
    fn language_ranges_match_by_prefix() {
        assert!(language_matches("en", "en"));
        assert!(language_matches("en-GB", "en"));
        assert!(language_matches("EN-gb", "en"));
        assert!(!language_matches("en", "en-GB"));
        assert!(
            !language_matches("english", "en"),
            "must break on a subtag boundary"
        );
        assert!(language_matches("de", "*"));
        assert!(!language_matches("", "*"));
    }

    // -------------------------------------------------- unbounded integers

    /// XSD's integer value space has arbitrary precision, so a literal past
    /// `i128` is valid and has to compare — not merely avoid crashing.
    #[test]
    fn compares_integers_beyond_i128() {
        let mut f = F::new();
        let big = "123456789012345678901234567890123456789012";
        let bigger = "123456789012345678901234567890123456789013";
        assert_eq!(
            f.cmp((big, "integer"), (bigger, "integer")),
            Some(Ordering::Less)
        );
        assert_eq!(
            f.cmp((big, "integer"), (big, "integer")),
            Some(Ordering::Equal)
        );
        // More digits is a larger magnitude, so length has to be checked
        // before lexicographic order — "9" must not sort above "10…0".
        assert_eq!(
            f.cmp(("9", "integer"), (big, "integer")),
            Some(Ordering::Less)
        );
        // Among negatives the larger magnitude is the smaller number.
        let (neg_big, neg_bigger) = (format!("-{big}"), format!("-{bigger}"));
        assert_eq!(
            f.cmp((&neg_bigger, "integer"), (&neg_big, "integer")),
            Some(Ordering::Less)
        );
        assert_eq!(
            f.cmp((&neg_big, "integer"), ("0", "integer")),
            Some(Ordering::Less)
        );
    }

    /// A big integer against an ordinary one is settled by sign alone, being
    /// out of `i128` range by construction. Worth pinning in both directions.
    #[test]
    fn compares_big_integers_against_the_numeric_tower() {
        let mut f = F::new();
        let big = "99999999999999999999999999999999999999999";
        let neg_big = format!("-{big}");
        assert_eq!(
            f.cmp(
                (big, "integer"),
                ("170141183460469231731687303715884105727", "integer")
            ),
            Some(Ordering::Greater),
            "larger than i128::MAX"
        );
        assert_eq!(
            f.cmp(("5", "integer"), (big, "integer")),
            Some(Ordering::Less)
        );
        assert_eq!(
            f.cmp((&neg_big, "integer"), ("5", "integer")),
            Some(Ordering::Less)
        );
        assert_eq!(
            f.cmp((big, "integer"), ("1.5", "decimal")),
            Some(Ordering::Greater)
        );
        assert_eq!(
            f.cmp((big, "integer"), ("1e300", "double")),
            Some(Ordering::Less)
        );
        assert_eq!(
            f.cmp((big, "integer"), ("INF", "double")),
            Some(Ordering::Less)
        );
    }

    /// Leading zeros and an explicit plus are legal spellings that must not
    /// change the value, nor make `-0` differ from `+0`.
    #[test]
    fn integer_spellings_normalise() {
        let mut f = F::new();
        assert_eq!(
            f.cmp(("+007", "integer"), ("7", "integer")),
            Some(Ordering::Equal)
        );
        assert_eq!(
            f.cmp(("-0", "integer"), ("+0", "integer")),
            Some(Ordering::Equal)
        );
        let padded = format!("{}1", "0".repeat(50));
        assert_eq!(
            f.cmp((&padded, "integer"), ("1", "integer")),
            Some(Ordering::Equal),
            "50 leading zeros is still one"
        );
    }

    /// A 42-digit integer is well-formed; overflowing `i128` is not a reason
    /// to call it ill-formed. The fixed-width types keep their real bounds.
    #[test]
    fn unbounded_integer_types_admit_huge_literals() {
        let f = F::new();
        let v = &f.vocab;
        let big = "123456789012345678901234567890123456789012";
        let neg_big = format!("-{big}");

        assert!(is_well_formed(big, v.xsd_integer, v));
        assert!(is_well_formed(big, v.xsd_nonNegativeInteger, v));
        assert!(is_well_formed(big, v.xsd_positiveInteger, v));
        assert!(is_well_formed(&neg_big, v.xsd_negativeInteger, v));
        assert!(is_well_formed(&neg_big, v.xsd_nonPositiveInteger, v));
        assert!(!is_well_formed(big, v.xsd_nonPositiveInteger, v));
        assert!(!is_well_formed(&neg_big, v.xsd_nonNegativeInteger, v));

        // Fixed-width types still have their widths.
        assert!(!is_well_formed(big, v.xsd_long, v));
        assert!(!is_well_formed("300", v.xsd_byte, v));
        assert!(is_well_formed("127", v.xsd_byte, v));

        // Zero falls on the right side of each boundary.
        assert!(is_well_formed("0", v.xsd_nonNegativeInteger, v));
        assert!(is_well_formed("0", v.xsd_nonPositiveInteger, v));
        assert!(!is_well_formed("0", v.xsd_positiveInteger, v));
        assert!(!is_well_formed("0", v.xsd_negativeInteger, v));

        // And what was never an integer stays rejected.
        for bad in ["aldi", "1.0", " 1", "1e3", "", "-", "+", "1 "] {
            assert!(!is_well_formed(bad, v.xsd_integer, v), "{bad:?}");
        }
    }

    // ------------------------------------------------------------ durations

    /// Durations were well-formedness-checked but had no comparable value, so
    /// every range constraint against one reported a violation regardless of
    /// what the answer should have been.
    #[test]
    fn compares_durations() {
        let mut f = F::new();
        assert_eq!(
            f.cmp(("P1D", "duration"), ("P2D", "duration")),
            Some(Ordering::Less)
        );
        assert_eq!(
            f.cmp(("PT1H", "duration"), ("PT60M", "duration")),
            Some(Ordering::Equal)
        );
        assert_eq!(
            f.cmp(("P1Y", "duration"), ("P11M", "duration")),
            Some(Ordering::Greater)
        );
    }

    /// Durations are only *partially* ordered: a month has no fixed length, so
    /// `P1M` and `P30D` have no defined order and must stay incomparable
    /// rather than being forced into one.
    #[test]
    fn incommensurable_durations_do_not_compare() {
        let mut f = F::new();
        assert_eq!(f.cmp(("P1M", "duration"), ("P30D", "duration")), None);
        // Nor does a duration compare against a different value space.
        assert_eq!(f.cmp(("P1D", "duration"), ("1", "integer")), None);
    }
}
