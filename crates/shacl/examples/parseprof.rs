//! Breaks the load phase down: raw parsing, interning, and index building.
//!
//! ```sh
//! cargo run --release --example parseprof -- bench/data-100000.ttl
//! ```

use std::time::Instant;

// The parser allocates a String per term; on a multi-threaded load the system
// allocator becomes the contention point, so measure with a better one.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use shacl::model::{GraphBuilder, TermStore, Vocab};

fn main() {
    let path = std::env::args().nth(1).expect("usage: parseprof <data.ttl>");
    let text = std::fs::read_to_string(&path).expect("readable file");
    println!("input        {:.1} MB", text.len() as f64 / 1e6);

    // --- raw parse: what oxttl costs on its own, with the terms thrown away.
    let t = Instant::now();
    let mut n = 0usize;
    for quad in oxttl::TurtleParser::new()
        .with_base_iri("http://bench/")
        .unwrap()
        .for_slice(text.as_bytes())
    {
        let _ = quad.unwrap();
        n += 1;
    }
    let raw = t.elapsed();
    println!(
        "parse only   {:>9.4}s  ({n} triples, {:.0}k triples/s)",
        raw.as_secs_f64(),
        n as f64 / raw.as_secs_f64() / 1e3
    );

    // --- parse + intern, without building any index.
    let mut store = TermStore::new();
    let _ = Vocab::new(&mut store);
    let t = Instant::now();
    let mut rows = Vec::with_capacity(n);
    for quad in oxttl::TurtleParser::new()
        .with_base_iri("http://bench/")
        .unwrap()
        .for_slice(text.as_bytes())
    {
        let q = quad.unwrap();
        let s = store.intern_oxrdf(oxrdf::TermRef::from(q.subject.as_ref()), 0);
        let p = store.named_node(q.predicate.as_str());
        let o = store.intern_oxrdf(q.object.as_ref(), 0);
        rows.push((s, p, o));
    }
    let interned = t.elapsed();
    println!(
        "  + intern   {:>9.4}s  (+{:.4}s, {} distinct terms)",
        interned.as_secs_f64(),
        (interned - raw).as_secs_f64(),
        store.len()
    );

    // --- index building: the three sorts.
    let t = Instant::now();
    let mut b = GraphBuilder::new();
    for (s, p, o) in rows {
        b.push(s, p, o);
    }
    let push = t.elapsed();
    let t = Instant::now();
    let graph = b.build();
    let build = t.elapsed();
    println!("  push rows  {:>9.4}s", push.as_secs_f64());
    println!("  build idx  {:>9.4}s  ({} triples)", build.as_secs_f64(), graph.len());

    println!(
        "total        {:>9.4}s",
        (interned + push + build).as_secs_f64()
    );

    // --- A/B of the two load paths, in one process and back to back, since
    // comparing across runs on a busy machine has already misled once.
    let base = "http://bench/";
    println!("\n  path            best of 3");
    for (label, parallel) in [("sequential", false), ("parallel", true)] {
        let mut best = f64::INFINITY;
        let mut triples = 0;
        for _ in 0..3 {
            let mut store = TermStore::new();
            let _ = Vocab::new(&mut store);
            let mut b = GraphBuilder::new();
            let t = Instant::now();
            if parallel {
                shacl::model::loader::parse_turtle_parallel(&text, base, 0, &mut store, &mut b)
                    .unwrap();
            } else {
                shacl::model::loader::parse_str(
                    &text,
                    oxrdfio::RdfFormat::Turtle,
                    base,
                    0,
                    &mut store,
                    &mut b,
                )
                .unwrap();
            }
            best = best.min(t.elapsed().as_secs_f64());
            triples = b.len();
        }
        println!("  {label:<12} {best:>9.4}s  ({triples} triples)");
    }
}
