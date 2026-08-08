//! Breaks validation down by phase, to find what makes it scale superlinearly.
//!
//! Run as:
//! ```sh
//! cargo run --release --example profile -- bench/data-10000.ttl bench/shapes.ttl
//! ```

use std::path::PathBuf;
use std::time::Instant;

use shacl::model::{Graph, TermId, TermStore, Vocab, loader, scope};
use shacl::path::Path;
use shacl::shapes::{Constraint, Shapes, Target};

fn main() {
    let mut args = std::env::args().skip(1);
    let data_path = PathBuf::from(args.next().expect("usage: profile <data.ttl> <shapes.ttl>"));
    let shapes_path = PathBuf::from(args.next().expect("usage: profile <data.ttl> <shapes.ttl>"));

    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);

    let t = Instant::now();
    let data = loader::load_file(&data_path, scope::DATA, &mut store).unwrap();
    let parse = t.elapsed();

    let shapes_graph = loader::load_file(&shapes_path, scope::SHAPES, &mut store).unwrap();
    let compiled = Shapes::compile(&shapes_graph, &store, &vocab).unwrap();

    println!("triples      {}", data.len());
    println!("terms        {}", store.len());
    println!("parse        {:>9.4}s", parse.as_secs_f64());

    // --- phase 1: resolving targets into focus nodes
    let root = compiled.roots()[0];
    let shape = compiled.get(root);
    let t = Instant::now();
    let mut focus = Vec::new();
    for target in &shape.targets {
        if let Target::Class(c) | Target::ImplicitClass(c) = target {
            let subs = subclasses(&data, &vocab, *c);
            for sub in subs {
                focus.extend(data.subjects(vocab.rdf_type, sub));
            }
        }
    }
    let target_raw = t.elapsed();

    let t = Instant::now();
    focus.sort_unstable();
    focus.dedup();
    let target_sort = t.elapsed();

    println!("focus nodes  {}", focus.len());
    println!("  gather     {:>9.4}s", target_raw.as_secs_f64());
    println!("  sort+dedup {:>9.4}s", target_sort.as_secs_f64());

    // --- phase 2: path evaluation, the suspected hot spot
    let mut total_path = std::time::Duration::ZERO;
    for c in &shape.constraints {
        let Constraint::Property(id) = c else {
            continue;
        };
        let inner = compiled.get(*id);
        let Some(p) = &inner.path else { continue };

        let t = Instant::now();
        let sets = p.eval_sets(&focus, &data);
        let took = t.elapsed();
        total_path += took;
        println!(
            "  path {:<28} {:>9.4}s  ({} rows, {} values)",
            describe(p, &store),
            took.as_secs_f64(),
            sets.len(),
            sets.total_values()
        );
    }
    println!("  path total {:>9.4}s", total_path.as_secs_f64());

    // --- phase 2b: the per-value work each constraint does, replicated here
    // so its cost is visible without instrumenting the engine's hot loop.
    for c in &shape.constraints {
        let Constraint::Property(id) = c else {
            continue;
        };
        let inner = compiled.get(*id);
        let Some(p) = &inner.path else { continue };
        let sets = p.eval_sets(&focus, &data);

        for constraint in &inner.constraints {
            let label = match constraint {
                Constraint::Class(_) => "class",
                Constraint::Datatype(_) => "datatype",
                Constraint::NodeKind(_) => "nodeKind",
                Constraint::Pattern { .. } => "pattern",
                _ => continue,
            };
            let t = Instant::now();
            let mut hits = 0usize;
            match constraint {
                Constraint::Class(classes) => {
                    // What the engine does: a graph probe per value node.
                    for row in sets.rows() {
                        for &v in row.values {
                            if data
                                .objects(v, vocab.rdf_type)
                                .any(|ty| classes.contains(&ty))
                            {
                                hits += 1;
                            }
                        }
                    }
                }
                Constraint::Datatype(dts) => {
                    for row in sets.rows() {
                        for &v in row.values {
                            if dts.iter().any(|&d| store.datatype(v) == Some(d)) {
                                hits += 1;
                            }
                        }
                    }
                }
                Constraint::NodeKind(_) => {
                    for row in sets.rows() {
                        for &v in row.values {
                            let _ = store.kind(v);
                            hits += 1;
                        }
                    }
                }
                Constraint::Pattern { regex, .. } => {
                    for row in sets.rows() {
                        for &v in row.values {
                            if regex.is_match(store.lexical_form(v).unwrap_or_default()) {
                                hits += 1;
                            }
                        }
                    }
                }
                _ => {}
            }
            println!(
                "  check {label:<26} {:>9.4}s  ({} values, {hits} pass)",
                t.elapsed().as_secs_f64(),
                sets.total_values()
            );
        }
    }

    // --- phase 3: the whole thing
    let t = Instant::now();
    let report = shacl::validate::validate_with(&data, &compiled, &mut store, &vocab).unwrap();
    let full = t.elapsed();
    println!(
        "validate     {:>9.4}s  ({} results)",
        full.as_secs_f64(),
        report.results.len()
    );

    // --- phase 4: raw index probe rate, isolating cache behaviour from
    // everything else. One binary search per focus node, nothing more.
    let pred = shape
        .constraints
        .iter()
        .find_map(|c| match c {
            Constraint::Property(id) => compiled.get(*id).path.as_ref()?.as_predicate(),
            _ => None,
        })
        .expect("a predicate path to probe");
    let t = Instant::now();
    let mut hits = 0usize;
    for &f in &focus {
        hits += data.objects(f, pred).count();
    }
    let probe = t.elapsed();
    println!(
        "probe sorted {:>9.4}s  ({} lookups, {hits} hits, {:.0} ns/lookup)",
        probe.as_secs_f64(),
        focus.len(),
        probe.as_secs_f64() * 1e9 / focus.len() as f64
    );

    // The same lookups in scrambled order. A sequence path's second step probes
    // whatever its first step reached, which bears no relation to the index
    // order — so if locality is what makes probing expensive, this is where it
    // shows up.
    let mut shuffled = focus.clone();
    let mut state = 0x2545F4914F6CDD1Du64;
    for i in (1..shuffled.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        shuffled.swap(i, (state % (i as u64 + 1)) as usize);
    }
    let t = Instant::now();
    let mut hits = 0usize;
    for &f in &shuffled {
        hits += data.objects(f, pred).count();
    }
    let probe_rand = t.elapsed();
    println!(
        "probe random {:>9.4}s  ({} lookups, {hits} hits, {:.0} ns/lookup, {:.1}x sorted)",
        probe_rand.as_secs_f64(),
        shuffled.len(),
        probe_rand.as_secs_f64() * 1e9 / shuffled.len() as f64,
        probe_rand.as_secs_f64() / probe.as_secs_f64()
    );
}

fn subclasses(data: &Graph, vocab: &Vocab, class: TermId) -> Vec<TermId> {
    let mut seen = vec![class];
    let mut queue = vec![class];
    while let Some(c) = queue.pop() {
        for sub in data.subjects(vocab.rdfs_subClassOf, c) {
            if !seen.contains(&sub) {
                seen.push(sub);
                queue.push(sub);
            }
        }
    }
    seen
}

fn describe(p: &Path, store: &TermStore) -> String {
    match p.as_predicate() {
        Some(t) => store
            .iri(t)
            .and_then(|i| i.rsplit_once('#').map(|(_, l)| l.to_string()))
            .unwrap_or_else(|| "?".into()),
        None => format!("{p:?}").chars().take(28).collect(),
    }
}
