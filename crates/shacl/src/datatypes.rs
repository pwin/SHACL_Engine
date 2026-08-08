//! XSD value semantics: ordering, and lexical well-formedness.
//!
//! `sh:minInclusive` and friends compare by *value*, not by lexical form, and
//! follow SPARQL's operator table: numerics compare across the whole numeric
//! tower, strings compare lexically, and anything incomparable simply fails the
//! constraint rather than erroring.

use std::cmp::Ordering;

use oxsdatatypes::{Date, DateTime, Decimal, Double, Duration, Time};

use crate::model::{TermId, TermStore, Vocab};

/// A literal reduced to something comparable.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    /// Exact where possible; `Float` only once precision has already been lost.
    Int(i128),
    Dec(Decimal),
    Float(f64),
    Str(String),
    Bool(bool),
    DateTime(DateTime),
    Date(Date),
    Time(Time),
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

        // Different value spaces are incomparable, not equal.
        _ => None,
    }
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
        _ if is_integer_type(dt, vocab) => Value::Int(lex.trim().parse::<i128>().ok()?),
        _ if dt == vocab.xsd_decimal => Value::Dec(lex.parse::<Decimal>().ok()?),
        _ if dt == vocab.xsd_float || dt == vocab.xsd_double => {
            Value::Float(parse_xsd_double(lex)?)
        }
        _ if dt == vocab.xsd_dateTime => Value::DateTime(lex.parse::<DateTime>().ok()?),
        _ if dt == vocab.xsd_date => Value::Date(lex.parse::<Date>().ok()?),
        _ if dt == vocab.xsd_time => Value::Time(lex.parse::<Time>().ok()?),
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
    // An unknown datatype places no constraint on its lexical space.
    true
}

fn is_well_formed_integer(lex: &str, dt: TermId, v: &Vocab) -> bool {
    // XSD integers permit a leading sign but no whitespace or decimal point.
    let Ok(n) = lex.parse::<i128>() else {
        return false;
    };
    let in_range = |lo: i128, hi: i128| n >= lo && n <= hi;
    match dt {
        _ if dt == v.xsd_integer => true,
        _ if dt == v.xsd_long => in_range(i64::MIN as i128, i64::MAX as i128),
        _ if dt == v.xsd_int => in_range(i32::MIN as i128, i32::MAX as i128),
        _ if dt == v.xsd_short => in_range(i16::MIN as i128, i16::MAX as i128),
        _ if dt == v.xsd_byte => in_range(i8::MIN as i128, i8::MAX as i128),
        _ if dt == v.xsd_nonNegativeInteger => n >= 0,
        _ if dt == v.xsd_positiveInteger => n > 0,
        _ if dt == v.xsd_nonPositiveInteger => n <= 0,
        _ if dt == v.xsd_negativeInteger => n < 0,
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
}
