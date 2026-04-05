use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use telepy_parser::{
    build_lazy_hierarchy_index, build_workspace_index, ensure_telepy_typeshed_snapshot,
    evict_module_file, query_class_members, query_incoming_calls,
    query_incoming_calls_tree, query_outgoing_calls, query_outgoing_calls_tree,
    query_resolved_callable_reference, query_resolved_class_fields, query_subtypes,
    query_subtypes_tree, query_subtypes_tree_limited, query_supertypes_tree, query_type_hierarchy,
    refresh_import_roots, sync_telepy_typeshed_snapshot, HierarchyNode, QueryResult,
};

#[derive(Debug, Default)]
struct QueryArgs {
    root: Option<PathBuf>,
    file: Option<PathBuf>,
    class_name: Option<String>,
    pretty: bool,
    debug: bool,
}

#[derive(Debug)]
struct BenchmarkArgs {
    root: Option<PathBuf>,
    file: Option<PathBuf>,
    class_name: Option<String>,
    pretty: bool,
    iterations: usize,
    warmups: usize,
}

#[derive(Debug, Default)]
struct ServeArgs {
    root: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct ServeRequest {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    action: String,
    file: PathBuf,
    #[serde(default, alias = "class")]
    symbol_name: String,
    line: Option<usize>,
    col: Option<usize>,
    max_depth: Option<usize>,
    member_depth: Option<usize>,
}

#[derive(Debug, Serialize)]
struct ReadyMessage {
    event: &'static str,
    timings: telepy_parser::Timings,
}

#[derive(Debug, Serialize)]
struct SuccessMessage<T> {
    id: u64,
    result: T,
}

#[derive(Debug, Serialize)]
struct ErrorMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    error: String,
}

#[derive(Debug, Clone, Serialize)]
struct HierarchyShape {
    class_nodes: usize,
    method_count: usize,
    field_count: usize,
    max_depth: usize,
}

#[derive(Debug, Clone, Serialize)]
struct TimedHierarchy {
    hierarchy: HierarchyNode,
    build_ms: f64,
    query_ms: f64,
    json_ms: f64,
    total_ms: f64,
    output_bytes: usize,
    shape: HierarchyShape,
}

#[derive(Debug, Clone, Serialize)]
struct MetricSummary {
    min_ms: f64,
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
struct HierarchyBenchmarkResult {
    build_ms: f64,
    iterations: usize,
    warmups: usize,
    query_ms: MetricSummary,
    json_ms: MetricSummary,
    total_ms: MetricSummary,
    output_bytes: usize,
    shape: HierarchyShape,
}

fn encode_message<T: Serialize>(message: &T) -> String {
    serde_json::to_string(message)
        .unwrap_or_else(|_| r#"{"error":"internal serialization failure"}"#.to_string())
}

fn success_response<T: Serialize>(id: u64, result: T) -> String {
    encode_message(&SuccessMessage { id, result })
}

fn error_response(id: Option<u64>, error: impl ToString) -> String {
    encode_message(&ErrorMessage {
        id,
        error: error.to_string(),
    })
}

fn response_from_result<T: Serialize>(id: u64, result: Result<T>) -> String {
    match result {
        Ok(result) => success_response(id, result),
        Err(err) => error_response(Some(id), err),
    }
}

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        print_usage();
        return Ok(());
    };

    match command.as_str() {
        "query" => run_query(args.collect()),
        "hierarchy" => run_hierarchy(args.collect()),
        "benchmark" | "benchmark-hierarchy" => run_benchmark_hierarchy(args.collect()),
        "serve" => run_serve(args.collect()),
        "sync-typeshed" => run_sync_typeshed(),
        _ => {
            print_usage();
            bail!("unknown command: {command}");
        }
    }
}

fn run_query(args: Vec<String>) -> Result<()> {
    let parsed = parse_query_args(&args)?;
    let root = parsed.root.clone().context("missing --root")?;
    if !root.exists() {
        bail!("root path does not exist: {}", root.display());
    }
    let file = parsed.file.clone().context("missing --file")?;
    let class_name = parsed.class_name.clone().context("missing --class")?;

    let build = build_lazy_hierarchy_index(&root)?;
    let mut result = query_type_hierarchy(&build.index, &file, &class_name)?;
    result.timings.discover_ms = build.timings.discover_ms;
    result.timings.parse_ms = build.timings.parse_ms;
    result.timings.index_ms = build.timings.index_ms;
    result.timings.total_ms = build.timings.total_ms + result.timings.query_ms;

    if parsed.debug {
        let timed =
            timed_hierarchy_from_result(build.timings.total_ms, result.clone(), parsed.pretty)?;
        print_hierarchy_debug(&parsed, &timed);
    }

    if parsed.pretty {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("{}", serde_json::to_string(&result)?);
    }

    Ok(())
}

fn run_hierarchy(args: Vec<String>) -> Result<()> {
    let parsed = parse_query_args(&args)?;
    let timed = build_and_query_hierarchy(&parsed)?;
    let rendered = encode_json(&timed.hierarchy, parsed.pretty)?;

    if parsed.debug {
        print_hierarchy_debug(&parsed, &timed);
    }

    println!("{rendered}");
    Ok(())
}

fn run_benchmark_hierarchy(args: Vec<String>) -> Result<()> {
    let parsed = parse_benchmark_args(&args)?;
    let root = parsed.root.context("missing --root")?;
    if !root.exists() {
        bail!("root path does not exist: {}", root.display());
    }
    let file = parsed.file.context("missing --file")?;
    let class_name = parsed.class_name.context("missing --class")?;

    let build = build_lazy_hierarchy_index(&root)?;
    let mut query_samples = Vec::with_capacity(parsed.iterations);
    let mut json_samples = Vec::with_capacity(parsed.iterations);
    let mut total_samples = Vec::with_capacity(parsed.iterations);
    let mut output_bytes = 0usize;
    let mut shape = None;

    for iteration in 0..(parsed.warmups + parsed.iterations) {
        let start = Instant::now();
        let result = query_type_hierarchy(&build.index, &file, &class_name)?;
        let query_ms = result.timings.query_ms;
        let json_start = Instant::now();
        let encoded = encode_json(&result.hierarchy, parsed.pretty)?;
        let json_ms = elapsed_ms(json_start);
        let total_ms = elapsed_ms(start);

        if iteration >= parsed.warmups {
            query_samples.push(query_ms);
            json_samples.push(json_ms);
            total_samples.push(total_ms);
            output_bytes = encoded.len();
            if shape.is_none() {
                shape = Some(collect_hierarchy_shape(&result.hierarchy));
            }
        }
    }

    let output = HierarchyBenchmarkResult {
        build_ms: build.timings.total_ms,
        iterations: parsed.iterations,
        warmups: parsed.warmups,
        query_ms: summarize_samples(query_samples),
        json_ms: summarize_samples(json_samples),
        total_ms: summarize_samples(total_samples),
        output_bytes,
        shape: shape.unwrap_or(HierarchyShape {
            class_nodes: 0,
            method_count: 0,
            field_count: 0,
            max_depth: 0,
        }),
    };

    println!("{}", encode_json(&output, parsed.pretty)?);
    Ok(())
}

fn run_serve(args: Vec<String>) -> Result<()> {
    let parsed = parse_serve_args(&args)?;
    let root = parsed.root.context("missing --root")?;
    if !root.exists() {
        bail!("root path does not exist: {}", root.display());
    }
    let lazy_build = build_lazy_hierarchy_index(&root)?;
    let mut full_build: Option<telepy_parser::IndexBuild> = None;

    // Attempt typeshed sync in the background so it does not block the
    // ready handshake or the first query.
    let refresh_index = lazy_build.index.clone();
    std::thread::spawn(move || match ensure_telepy_typeshed_snapshot() {
        Ok(_) => refresh_import_roots(&refresh_index),
        Err(err) => eprintln!("telepy: failed to sync vendored typeshed snapshot: {err}"),
    });

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    writeln!(
        out,
        "{}",
        serde_json::to_string(&ReadyMessage {
            event: "ready",
            timings: lazy_build.timings.clone(),
        })?
    )?;
    out.flush()?;

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<ServeRequest>(&line) {
            Ok(request) => {
                let request_id = request.id;
                match request.action.as_str() {
                    "" | "supertypes" => response_from_result(
                        request_id,
                        query_type_hierarchy(
                            &lazy_build.index,
                            &request.file,
                            &request.symbol_name,
                        ),
                    ),
                    "supertypes_tree" => response_from_result(
                        request_id,
                        query_supertypes_tree(
                            &lazy_build.index,
                            &request.file,
                            &request.symbol_name,
                        ),
                    ),
                    "subtypes" => response_from_result(
                        request_id,
                        ensure_full_build(&mut full_build, &root)
                            .and_then(|build| {
                                query_subtypes(&build.index, &request.file, &request.symbol_name)
                            })
                            .map(|items| serde_json::json!({ "items": items })),
                    ),
                    "subtypes_tree" => response_from_result(
                        request_id,
                        ensure_full_build(&mut full_build, &root).and_then(|build| {
                            match (request.max_depth, request.member_depth) {
                                (None, None) => query_subtypes_tree(
                                    &build.index,
                                    &request.file,
                                    &request.symbol_name,
                                ),
                                _ => query_subtypes_tree_limited(
                                    &build.index,
                                    &request.file,
                                    &request.symbol_name,
                                    request.max_depth,
                                    request.member_depth,
                                ),
                            }
                        }),
                    ),
                    "prewarm_full" => response_from_result(
                        request_id,
                        ensure_full_build(&mut full_build, &root)
                            .map(|_| serde_json::json!({ "ok": true })),
                    ),
                    "refresh" => {
                        evict_module_file(&lazy_build.index, &request.file);
                        full_build = None;
                        response_from_result(
                            request_id,
                            Ok(serde_json::json!({ "ok": true })),
                        )
                    }
                    "class_members" => response_from_result(
                        request_id,
                        query_class_members(&lazy_build.index, &request.file, &request.symbol_name),
                    ),
                    "resolve_class_fields" => response_from_result(
                        request_id,
                        query_resolved_class_fields(
                            &lazy_build.index,
                            &request.file,
                            &request.symbol_name,
                        ),
                    ),
                    "resolve_callable_reference" => {
                        let line = request
                            .line
                            .context("missing 'line' for callable reference query");
                        let col = request
                            .col
                            .context("missing 'col' for callable reference query");
                        match (line, col) {
                            (Ok(l), Ok(c)) => response_from_result(
                                request_id,
                                ensure_full_build(&mut full_build, &root).and_then(|build| {
                                    query_resolved_callable_reference(
                                        &build.index,
                                        &request.file,
                                        l,
                                        c,
                                    )
                                }),
                            ),
                            (Err(err), _) | (_, Err(err)) => error_response(Some(request_id), err),
                        }
                    }
                    "incoming_calls" => {
                        let line = request.line.context("missing 'line' for call query");
                        let col = request.col.context("missing 'col' for call query");
                        match (line, col) {
                            (Ok(l), Ok(c)) => response_from_result(
                                request_id,
                                ensure_full_build(&mut full_build, &root)
                                    .and_then(|build| {
                                        query_incoming_calls(
                                            &build.index,
                                            &request.file,
                                            &request.symbol_name,
                                            Some(l),
                                            Some(c),
                                        )
                                    })
                                    .map(|items| serde_json::json!({ "items": items })),
                            ),
                            (Err(err), _) | (_, Err(err)) => error_response(Some(request_id), err),
                        }
                    }
                    "incoming_calls_tree" => {
                        let line = request.line.context("missing 'line' for call query");
                        let col = request.col.context("missing 'col' for call query");
                        match (line, col) {
                            (Ok(l), Ok(c)) => response_from_result(
                                request_id,
                                ensure_full_build(&mut full_build, &root).and_then(|build| {
                                    query_incoming_calls_tree(
                                        &build.index,
                                        &request.file,
                                        &request.symbol_name,
                                        Some(l),
                                        Some(c),
                                    )
                                }),
                            ),
                            (Err(err), _) | (_, Err(err)) => error_response(Some(request_id), err),
                        }
                    }
                    "outgoing_calls" => {
                        let line = request.line.context("missing 'line' for call query");
                        let col = request.col.context("missing 'col' for call query");
                        match (line, col) {
                            (Ok(l), Ok(c)) => response_from_result(
                                request_id,
                                ensure_full_build(&mut full_build, &root)
                                    .and_then(|build| {
                                        query_outgoing_calls(
                                            &build.index,
                                            &request.file,
                                            &request.symbol_name,
                                            Some(l),
                                            Some(c),
                                        )
                                    })
                                    .map(|items| serde_json::json!({ "items": items })),
                            ),
                            (Err(err), _) | (_, Err(err)) => error_response(Some(request_id), err),
                        }
                    }
                    "outgoing_calls_tree" => {
                        let line = request.line.context("missing 'line' for call query");
                        let col = request.col.context("missing 'col' for call query");
                        match (line, col) {
                            (Ok(l), Ok(c)) => response_from_result(
                                request_id,
                                ensure_full_build(&mut full_build, &root).and_then(|build| {
                                    query_outgoing_calls_tree(
                                        &build.index,
                                        &request.file,
                                        &request.symbol_name,
                                        Some(l),
                                        Some(c),
                                    )
                                }),
                            ),
                            (Err(err), _) | (_, Err(err)) => error_response(Some(request_id), err),
                        }
                    }
                    other => error_response(Some(request_id), format!("unknown action: {other}")),
                }
            }
            Err(err) => error_response(None, err),
        };

        if writeln!(out, "{response}").is_err() {
            break; // client disconnected
        }
        out.flush()?;
    }

    Ok(())
}

fn ensure_full_build<'a>(
    full_build: &'a mut Option<telepy_parser::IndexBuild>,
    root: &PathBuf,
) -> Result<&'a telepy_parser::IndexBuild> {
    if full_build.is_none() {
        *full_build = Some(build_workspace_index(root)?);
    }
    Ok(full_build.as_ref().expect("full build initialized"))
}

fn run_sync_typeshed() -> Result<()> {
    match sync_telepy_typeshed_snapshot()? {
        Some(snapshot) => {
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
            Ok(())
        }
        None => bail!("unable to determine Telepy cache directory"),
    }
}

fn parse_query_args(args: &[String]) -> Result<QueryArgs> {
    let mut parsed = QueryArgs::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--root" => {
                parsed.root = Some(PathBuf::from(
                    iter.next().context("missing value after --root")?,
                ));
            }
            "--file" => {
                parsed.file = Some(PathBuf::from(
                    iter.next().context("missing value after --file")?,
                ));
            }
            "--class" | "--symbol_name" => {
                parsed.class_name =
                    Some(iter.next().context("missing value after --class")?.clone());
            }
            "--pretty" => {
                parsed.pretty = true;
            }
            "--debug" => {
                parsed.debug = true;
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    Ok(parsed)
}

fn parse_benchmark_args(args: &[String]) -> Result<BenchmarkArgs> {
    let mut parsed = BenchmarkArgs {
        root: None,
        file: None,
        class_name: None,
        pretty: false,
        iterations: 100,
        warmups: 10,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--root" => {
                parsed.root = Some(PathBuf::from(
                    iter.next().context("missing value after --root")?,
                ));
            }
            "--file" => {
                parsed.file = Some(PathBuf::from(
                    iter.next().context("missing value after --file")?,
                ));
            }
            "--class" | "--symbol_name" => {
                parsed.class_name =
                    Some(iter.next().context("missing value after --class")?.clone());
            }
            "--pretty" => {
                parsed.pretty = true;
            }
            "--iterations" => {
                parsed.iterations = iter
                    .next()
                    .context("missing value after --iterations")?
                    .parse()
                    .context("invalid integer for --iterations")?;
            }
            "--warmups" => {
                parsed.warmups = iter
                    .next()
                    .context("missing value after --warmups")?
                    .parse()
                    .context("invalid integer for --warmups")?;
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    Ok(parsed)
}

fn parse_serve_args(args: &[String]) -> Result<ServeArgs> {
    let mut parsed = ServeArgs::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--root" => {
                parsed.root = Some(PathBuf::from(
                    iter.next().context("missing value after --root")?,
                ));
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    Ok(parsed)
}

fn print_usage() {
    eprintln!(
        "usage:\n  telepy-parser hierarchy --root <root> --file <file> --class <name> [--pretty] [--debug]\n  telepy-parser benchmark-hierarchy --root <root> --file <file> --class <name> [--iterations <n>] [--warmups <n>] [--pretty]\n  telepy-parser query --root <root> --file <file> --class <name> [--pretty] [--debug]\n  telepy-parser serve --root <root>\n  telepy-parser sync-typeshed"
    );
}

fn build_and_query_hierarchy(parsed: &QueryArgs) -> Result<TimedHierarchy> {
    let root = parsed.root.clone().context("missing --root")?;
    if !root.exists() {
        bail!("root path does not exist: {}", root.display());
    }
    let file = parsed.file.clone().context("missing --file")?;
    let class_name = parsed.class_name.clone().context("missing --class")?;

    let build = build_lazy_hierarchy_index(&root)?;
    let result = query_type_hierarchy(&build.index, &file, &class_name)?;
    timed_hierarchy_from_result(build.timings.total_ms, result, parsed.pretty)
}

fn timed_hierarchy_from_result(
    build_ms: f64,
    result: QueryResult,
    pretty: bool,
) -> Result<TimedHierarchy> {
    let shape = collect_hierarchy_shape(&result.hierarchy);
    let json_start = Instant::now();
    let encoded = encode_json(&result.hierarchy, pretty)?;
    let json_ms = elapsed_ms(json_start);
    Ok(TimedHierarchy {
        hierarchy: result.hierarchy,
        build_ms,
        query_ms: result.timings.query_ms,
        json_ms,
        total_ms: build_ms + result.timings.query_ms + json_ms,
        output_bytes: encoded.len(),
        shape,
    })
}

fn encode_json<T: Serialize>(value: &T, pretty: bool) -> Result<String> {
    if pretty {
        Ok(serde_json::to_string_pretty(value)?)
    } else {
        Ok(serde_json::to_string(value)?)
    }
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn collect_hierarchy_shape(root: &HierarchyNode) -> HierarchyShape {
    fn walk(node: &HierarchyNode, depth: usize, shape: &mut HierarchyShape) {
        shape.class_nodes += 1;
        shape.method_count += node.methods.len();
        shape.field_count += node.fields.len();
        shape.max_depth = shape.max_depth.max(depth);
        for child in &node.ancestors {
            walk(child, depth + 1, shape);
        }
    }

    let mut shape = HierarchyShape {
        class_nodes: 0,
        method_count: 0,
        field_count: 0,
        max_depth: 0,
    };
    walk(root, 1, &mut shape);
    shape
}

fn print_hierarchy_debug(parsed: &QueryArgs, timed: &TimedHierarchy) {
    eprintln!("telepy hierarchy debug");
    if let Some(root) = &parsed.root {
        eprintln!("  root: {}", root.display());
    }
    if let Some(file) = &parsed.file {
        eprintln!("  file: {}", file.display());
    }
    if let Some(class_name) = &parsed.class_name {
        eprintln!("  class: {class_name}");
    }
    eprintln!("  build_ms: {:.3}", timed.build_ms);
    eprintln!("  query_ms: {:.3}", timed.query_ms);
    eprintln!("  json_ms: {:.3}", timed.json_ms);
    eprintln!("  total_ms: {:.3}", timed.total_ms);
    eprintln!("  output_bytes: {}", timed.output_bytes);
    eprintln!(
        "  shape: class_nodes={} methods={} fields={} max_depth={}",
        timed.shape.class_nodes,
        timed.shape.method_count,
        timed.shape.field_count,
        timed.shape.max_depth
    );
}

fn summarize_samples(mut samples: Vec<f64>) -> MetricSummary {
    if samples.is_empty() {
        return MetricSummary {
            min_ms: 0.0,
            mean_ms: 0.0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            max_ms: 0.0,
        };
    }

    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let len = samples.len();
    let sum: f64 = samples.iter().sum();

    MetricSummary {
        min_ms: samples[0],
        mean_ms: sum / len as f64,
        p50_ms: percentile(&samples, 0.50),
        p95_ms: percentile(&samples, 0.95),
        max_ms: samples[len - 1],
    }
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let last = sorted.len() - 1;
    let idx = ((last as f64) * fraction).round() as usize;
    sorted[idx.min(last)]
}
