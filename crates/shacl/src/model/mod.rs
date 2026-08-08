//! The RDF substrate: interned terms and indexed graphs.

pub mod graph;
pub mod interner;
pub mod loader;
pub mod term;
pub mod vocab;

pub use graph::{Graph, GraphBuilder};
pub use interner::{Interner, StrId};
pub use term::{TermData, TermId, TermKind, TermStore};
pub use vocab::Vocab;

/// Blank node scopes. Documents loaded into different scopes never share blank
/// node identity, which is what keeps a data graph and a shapes graph parsed
/// from separate files from aliasing each other's `_:b0`.
pub mod scope {
    pub const DATA: u32 = 0;
    pub const SHAPES: u32 = 1;
    /// Terms minted by SPARQL evaluation, which belong to no source document.
    pub const SPARQL: u32 = 8;
    /// First scope handed out to additional documents (imports, test files).
    pub const FIRST_DYNAMIC: u32 = 16;
}

impl Graph {
    /// Reads an RDF collection starting at `head` into `out`.
    ///
    /// Returns `false` if the list is malformed — not `rdf:nil`-terminated, or
    /// cyclic — which several shape constraints must detect rather than hang on.
    pub fn collect_list(&self, head: TermId, vocab: &Vocab, out: &mut Vec<TermId>) -> bool {
        out.clear();
        let mut node = head;
        // The list cannot be longer than the graph; this bounds cyclic input.
        let limit = self.len() + 1;
        for _ in 0..limit {
            if node == vocab.rdf_nil {
                return true;
            }
            let Some(first) = self.object(node, vocab.rdf_first) else {
                return false;
            };
            let Some(rest) = self.object(node, vocab.rdf_rest) else {
                return false;
            };
            out.push(first);
            node = rest;
        }
        false
    }

    /// Convenience wrapper over [`Graph::collect_list`].
    pub fn list(&self, head: TermId, vocab: &Vocab) -> Option<Vec<TermId>> {
        let mut out = Vec::new();
        self.collect_list(head, vocab, &mut out).then_some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdfio::RdfFormat;

    fn graph_of(turtle: &str) -> (TermStore, Vocab, Graph) {
        let mut store = TermStore::new();
        let vocab = Vocab::new(&mut store);
        let mut b = GraphBuilder::new();
        loader::parse_str(
            turtle,
            RdfFormat::Turtle,
            "http://t/",
            0,
            &mut store,
            &mut b,
        )
        .unwrap();
        (store, vocab, b.build())
    }

    #[test]
    fn reads_a_well_formed_list() {
        let (mut store, vocab, g) =
            graph_of("@prefix ex: <http://ex/> . ex:s ex:p (ex:a ex:b ex:c) .");
        let s = store.named_node("http://ex/s");
        let p = store.named_node("http://ex/p");
        let head = g.object(s, p).unwrap();

        let items = g.list(head, &vocab).expect("well-formed");
        let names: Vec<_> = items.iter().map(|&t| store.iri(t).unwrap()).collect();
        assert_eq!(names, vec!["http://ex/a", "http://ex/b", "http://ex/c"]);
    }

    #[test]
    fn empty_list_is_nil() {
        let (_, vocab, g) = graph_of("@prefix ex: <http://ex/> . ex:s ex:p ex:o .");
        assert_eq!(g.list(vocab.rdf_nil, &vocab), Some(vec![]));
    }

    #[test]
    fn rejects_an_unterminated_list() {
        // `_:l` has rdf:first but no rdf:rest.
        let (mut store, vocab, g) = graph_of(
            "@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
             @prefix ex: <http://ex/> . ex:l rdf:first ex:a .",
        );
        let head = store.named_node("http://ex/l");
        assert_eq!(g.list(head, &vocab), None);
    }

    #[test]
    fn terminates_on_a_cyclic_list() {
        let (mut store, vocab, g) = graph_of(
            "@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
             @prefix ex: <http://ex/> .
             ex:l rdf:first ex:a ; rdf:rest ex:l .",
        );
        let head = store.named_node("http://ex/l");
        assert_eq!(g.list(head, &vocab), None, "must not loop forever");
    }
}
