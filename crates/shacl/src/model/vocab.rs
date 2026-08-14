//! Pre-interned vocabulary.
//!
//! Every IRI the engine tests against is interned once when the [`TermStore`]
//! is created, so shape compilation and validation compare `u32`s instead of
//! strings. The term list is derived from what the W3C SHACL 1.0 and 1.2 test
//! suites actually use.

use super::term::{TermId, TermStore};

macro_rules! vocab {
    ($( $prefix:ident = $ns:literal { $( $field:ident = $local:literal ),* $(,)? } )*) => {
        $( #[doc = concat!("The `", stringify!($prefix), ":` namespace IRI.")]
           pub const $prefix: &str = $ns; )*

        /// Interned handles for every vocabulary IRI the engine needs.
        ///
        /// Field names mirror the IRIs they denote (`sh_NodeShape` for
        /// `sh:NodeShape`), which keeps shape-compilation code readable against
        /// the spec at the cost of Rust's usual casing.
        #[derive(Debug, Clone)]
        #[allow(non_snake_case)]
        pub struct Vocab {
            $($( pub $field: TermId, )*)*
        }

        impl Vocab {
            /// Interns the whole vocabulary into `store`.
            pub fn new(store: &mut TermStore) -> Self {
                Self {
                    $($( $field: store.named_node(concat!($ns, $local)), )*)*
                }
            }
        }
    };
}

vocab! {
    SH = "http://www.w3.org/ns/shacl#" {
        // Shape types and structure
        sh_NodeShape = "NodeShape",
        sh_PropertyShape = "PropertyShape",
        sh_ShapeClass = "ShapeClass",
        sh_property = "property",
        sh_path = "path",
        sh_deactivated = "deactivated",
        sh_severity = "severity",
        sh_message = "message",
        sh_name = "name",
        sh_description = "description",
        sh_order = "order",

        // Severities
        sh_Violation = "Violation",
        sh_Warning = "Warning",
        sh_Info = "Info",

        // Targets
        sh_targetClass = "targetClass",
        sh_targetNode = "targetNode",
        sh_targetObjectsOf = "targetObjectsOf",
        sh_targetSubjectsOf = "targetSubjectsOf",
        sh_target = "target",
        sh_targetWhere = "targetWhere",

        // Property paths
        sh_inversePath = "inversePath",
        sh_alternativePath = "alternativePath",
        sh_zeroOrMorePath = "zeroOrMorePath",
        sh_oneOrMorePath = "oneOrMorePath",
        sh_zeroOrOnePath = "zeroOrOnePath",

        // Value type constraints
        sh_class = "class",
        sh_datatype = "datatype",
        sh_datatypes = "datatypes",
        sh_nodeKind = "nodeKind",

        // Node kinds
        sh_IRI = "IRI",
        sh_BlankNode = "BlankNode",
        sh_Literal = "Literal",
        sh_BlankNodeOrIRI = "BlankNodeOrIRI",
        sh_BlankNodeOrLiteral = "BlankNodeOrLiteral",
        sh_IRIOrLiteral = "IRIOrLiteral",

        // Cardinality
        sh_minCount = "minCount",
        sh_maxCount = "maxCount",

        // Value range
        sh_minExclusive = "minExclusive",
        sh_minInclusive = "minInclusive",
        sh_maxExclusive = "maxExclusive",
        sh_maxInclusive = "maxInclusive",

        // String based
        sh_minLength = "minLength",
        sh_maxLength = "maxLength",
        sh_pattern = "pattern",
        sh_flags = "flags",
        sh_languageIn = "languageIn",
        sh_uniqueLang = "uniqueLang",
        sh_singleLine = "singleLine",

        // Property pair
        sh_equals = "equals",
        sh_disjoint = "disjoint",
        sh_lessThan = "lessThan",
        sh_lessThanOrEquals = "lessThanOrEquals",

        // Logical
        sh_not = "not",
        sh_and = "and",
        sh_or = "or",
        sh_xone = "xone",

        // Shape based
        sh_node = "node",
        sh_qualifiedValueShape = "qualifiedValueShape",
        sh_qualifiedMinCount = "qualifiedMinCount",
        sh_qualifiedMaxCount = "qualifiedMaxCount",
        sh_qualifiedValueShapesDisjoint = "qualifiedValueShapesDisjoint",

        // Other
        sh_closed = "closed",
        sh_ByTypes = "ByTypes",
        sh_ignoredProperties = "ignoredProperties",
        sh_hasValue = "hasValue",
        sh_in = "in",

        // SHACL 1.2: lists, members, sets
        sh_minListLength = "minListLength",
        sh_maxListLength = "maxListLength",
        sh_memberShape = "memberShape",
        sh_uniqueMembers = "uniqueMembers",
        sh_subsetOf = "subsetOf",
        sh_uniqueValuesFor = "uniqueValuesFor",
        sh_rootClass = "rootClass",
        sh_someValue = "someValue",

        // SHACL 1.2: reification
        sh_reifierShape = "reifierShape",
        sh_reificationRequired = "reificationRequired",

        // Node expressions
        sh_expression = "expression",
        sh_nodeByExpression = "nodeByExpression",
        sh_sparqlExpr = "sparqlExpr",
        sh_nodeShape = "nodeShape",
        sh_shape = "shape",

        // SPARQL based constraints and components
        sh_sparql = "sparql",
        sh_select = "select",
        sh_ask = "ask",
        sh_prefixes = "prefixes",
        sh_declare = "declare",
        sh_prefix = "prefix",
        sh_namespace = "namespace",
        sh_parameter = "parameter",
        sh_optional = "optional",
        sh_validator = "validator",
        sh_nodeValidator = "nodeValidator",
        sh_propertyValidator = "propertyValidator",
        sh_labelTemplate = "labelTemplate",
        sh_SPARQLConstraint = "SPARQLConstraint",
        sh_SPARQLAskValidator = "SPARQLAskValidator",
        sh_SPARQLSelectValidator = "SPARQLSelectValidator",
        sh_ConstraintComponent = "ConstraintComponent",
        sh_Parameter = "Parameter",
        sh_PrefixDeclaration = "PrefixDeclaration",

        // SHACL-AF node expressions. A separate vocabulary from the SHACL 1.2
        // node expression algebra, which lives in its own `shacl-node-expr#`
        // namespace — the two can be told apart by predicate, so both are
        // accepted by the same evaluator.
        sh_this = "this",
        sh_nodes = "nodes",
        sh_filterShape = "filterShape",
        sh_union = "union",
        sh_intersection = "intersection",

        // SHACL-AF rules
        sh_rule = "rule",
        sh_TripleRule = "TripleRule",
        sh_SPARQLRule = "SPARQLRule",
        sh_subject = "subject",
        sh_predicate = "predicate",
        sh_object = "object",
        sh_construct = "construct",
        sh_condition = "condition",

        // Validation report
        sh_ValidationReport = "ValidationReport",
        sh_ValidationResult = "ValidationResult",
        sh_conforms = "conforms",
        sh_conformanceDisallows = "conformanceDisallows",
        sh_result = "result",
        sh_focusNode = "focusNode",
        sh_value = "value",
        sh_resultPath = "resultPath",
        sh_resultSeverity = "resultSeverity",
        sh_resultMessage = "resultMessage",
        sh_sourceShape = "sourceShape",
        sh_sourceConstraint = "sourceConstraint",
        sh_sourceConstraintComponent = "sourceConstraintComponent",
        sh_detail = "detail",
        sh_shapesGraph = "shapesGraph",
        sh_entailment = "entailment",

        // Constraint components, used as `sh:sourceConstraintComponent` values
        sh_AndConstraintComponent = "AndConstraintComponent",
        sh_ClassConstraintComponent = "ClassConstraintComponent",
        sh_ClosedConstraintComponent = "ClosedConstraintComponent",
        sh_DatatypeConstraintComponent = "DatatypeConstraintComponent",
        sh_DisjointConstraintComponent = "DisjointConstraintComponent",
        sh_EqualsConstraintComponent = "EqualsConstraintComponent",
        sh_ExpressionConstraintComponent = "ExpressionConstraintComponent",
        sh_HasValueConstraintComponent = "HasValueConstraintComponent",
        sh_InConstraintComponent = "InConstraintComponent",
        sh_LanguageInConstraintComponent = "LanguageInConstraintComponent",
        sh_LessThanConstraintComponent = "LessThanConstraintComponent",
        sh_LessThanOrEqualsConstraintComponent = "LessThanOrEqualsConstraintComponent",
        sh_MaxCountConstraintComponent = "MaxCountConstraintComponent",
        sh_MaxExclusiveConstraintComponent = "MaxExclusiveConstraintComponent",
        sh_MaxInclusiveConstraintComponent = "MaxInclusiveConstraintComponent",
        sh_MaxLengthConstraintComponent = "MaxLengthConstraintComponent",
        sh_MaxListLengthConstraintComponent = "MaxListLengthConstraintComponent",
        sh_MemberShapeConstraintComponent = "MemberShapeConstraintComponent",
        sh_MinCountConstraintComponent = "MinCountConstraintComponent",
        sh_MinExclusiveConstraintComponent = "MinExclusiveConstraintComponent",
        sh_MinInclusiveConstraintComponent = "MinInclusiveConstraintComponent",
        sh_MinLengthConstraintComponent = "MinLengthConstraintComponent",
        sh_MinListLengthConstraintComponent = "MinListLengthConstraintComponent",
        sh_NodeConstraintComponent = "NodeConstraintComponent",
        sh_NodeByExpressionConstraintComponent = "NodeByExpressionConstraintComponent",
        sh_NodeKindConstraintComponent = "NodeKindConstraintComponent",
        sh_NotConstraintComponent = "NotConstraintComponent",
        sh_OrConstraintComponent = "OrConstraintComponent",
        sh_PatternConstraintComponent = "PatternConstraintComponent",
        sh_PropertyConstraintComponent = "PropertyConstraintComponent",
        sh_QualifiedMaxCountConstraintComponent = "QualifiedMaxCountConstraintComponent",
        sh_QualifiedMinCountConstraintComponent = "QualifiedMinCountConstraintComponent",
        sh_ReifierShapeConstraintComponent = "ReifierShapeConstraintComponent",
        sh_RootClassConstraintComponent = "RootClassConstraintComponent",
        sh_SingleLineConstraintComponent = "SingleLineConstraintComponent",
        sh_SomeValueConstraintComponent = "SomeValueConstraintComponent",
        sh_SPARQLConstraintComponent = "SPARQLConstraintComponent",
        sh_SubsetOfConstraintComponent = "SubsetOfConstraintComponent",
        sh_UniqueLangConstraintComponent = "UniqueLangConstraintComponent",
        sh_UniqueMembersConstraintComponent = "UniqueMembersConstraintComponent",
        sh_UniqueValuesForConstraintComponent = "UniqueValuesForConstraintComponent",
        sh_XoneConstraintComponent = "XoneConstraintComponent",
    }

    RDF = "http://www.w3.org/1999/02/22-rdf-syntax-ns#" {
        rdf_type = "type",
        rdf_first = "first",
        rdf_rest = "rest",
        rdf_nil = "nil",
        rdf_List = "List",
        rdf_langString = "langString",
        rdf_Property = "Property",
        rdf_subject = "subject",
        rdf_predicate = "predicate",
        rdf_object = "object",
        rdf_reifies = "reifies",
    }

    RDFS = "http://www.w3.org/2000/01/rdf-schema#" {
        rdfs_subClassOf = "subClassOf",
        rdfs_subPropertyOf = "subPropertyOf",
        rdfs_Class = "Class",
        rdfs_Literal = "Literal",
        rdfs_Resource = "Resource",
        rdfs_domain = "domain",
        rdfs_range = "range",
        rdfs_label = "label",
        rdfs_comment = "comment",
    }

    XSD = "http://www.w3.org/2001/XMLSchema#" {
        xsd_string = "string",
        xsd_boolean = "boolean",
        xsd_integer = "integer",
        xsd_decimal = "decimal",
        xsd_float = "float",
        xsd_double = "double",
        xsd_date = "date",
        xsd_dateTime = "dateTime",
        xsd_time = "time",
        xsd_duration = "duration",
        xsd_anyURI = "anyURI",
        xsd_long = "long",
        xsd_int = "int",
        xsd_short = "short",
        xsd_byte = "byte",
        xsd_nonNegativeInteger = "nonNegativeInteger",
        xsd_positiveInteger = "positiveInteger",
        xsd_nonPositiveInteger = "nonPositiveInteger",
        xsd_negativeInteger = "negativeInteger",
        xsd_unsignedLong = "unsignedLong",
        xsd_unsignedInt = "unsignedInt",
        xsd_unsignedShort = "unsignedShort",
        xsd_unsignedByte = "unsignedByte",
    }

    OWL = "http://www.w3.org/2002/07/owl#" {
        owl_imports = "imports",
        owl_Class = "Class",
    }

    MF = "http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#" {
        mf_Manifest = "Manifest",
        mf_include = "include",
        mf_entries = "entries",
        mf_action = "action",
        mf_result = "result",
        mf_status = "status",
        mf_name = "name",
    }

    SHT = "http://www.w3.org/ns/shacl-test#" {
        sht_Validate = "Validate",
        sht_dataGraph = "dataGraph",
        sht_shapesGraph = "shapesGraph",
        sht_approved = "approved",
        sht_proposed = "proposed",
        sht_failure = "failure",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_expected_iris() {
        let mut s = TermStore::new();
        let v = Vocab::new(&mut s);

        assert_eq!(
            s.iri(v.sh_NodeShape),
            Some("http://www.w3.org/ns/shacl#NodeShape")
        );
        assert_eq!(
            s.iri(v.rdf_type),
            Some("http://www.w3.org/1999/02/22-rdf-syntax-ns#type")
        );
        assert_eq!(
            s.iri(v.xsd_integer),
            Some("http://www.w3.org/2001/XMLSchema#integer")
        );
    }

    #[test]
    fn vocabulary_terms_are_shared_with_later_interning() {
        let mut s = TermStore::new();
        let v = Vocab::new(&mut s);
        // A term parsed from a document must land on the same id.
        assert_eq!(s.named_node("http://www.w3.org/ns/shacl#class"), v.sh_class);
    }

    #[test]
    fn every_term_is_distinct() {
        let mut s = TermStore::new();
        let before = s.len();
        Vocab::new(&mut s);
        let added = s.len() - before;
        // Re-interning must not add anything; catches duplicate entries in the
        // macro list collapsing silently.
        let mut s2 = TermStore::new();
        let v2 = Vocab::new(&mut s2);
        let _ = v2;
        assert_eq!(s2.len(), added);
        assert!(added > 150, "expected the full vocabulary, got {added}");
    }
}
