// Cada tool y recurso del servidor, llamado por stdio contra una base creada
// con el motor actual (ADP-04). El binario corre como lo lanza un cliente:
// JSON-RPC por líneas sobre stdin/stdout. Dominio ficticio (botánica).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use nopaldb::Graph;
use nopaldb::index::IndexType;
use nopaldb::types::{Edge, Node, PropertyValue};
use serde_json::{Value, json};

const FLORA: &str = r#"
@prefix owl:   <http://www.w3.org/2002/07/owl#> .
@prefix rdfs:  <http://www.w3.org/2000/01/rdf-schema#> .
@prefix flora: <http://plantas.example/flora#> .
@prefix :      <http://plantas.example/> .

flora:Planta a owl:Class .
flora:Arbol  a owl:Class ; rdfs:subClassOf flora:Planta .
flora:Pino   a owl:Class ; rdfs:subClassOf flora:Arbol .

:roble a flora:Arbol ; :name "roble" .
:musgo a flora:Planta ; :name "musgo" .
"#;

fn s(v: &str) -> PropertyValue {
    PropertyValue::String(v.to_string())
}

/// Base de prueba: un camino ana → beto → carla, documentos con índice
/// full-text y embeddings, y una ontología pequeña.
async fn fixture(dir: &Path) {
    let graph = Graph::open(dir).await.unwrap();
    let mut ids = Vec::new();
    for (name, vec) in [("ana", [1.0f32, 0.0, 0.0]), ("beto", [0.9, 0.1, 0.0]), ("carla", [0.0, 0.0, 1.0])] {
        let id = graph.add_node(Node::new("Persona").with_property("name", s(name))).await.unwrap();
        graph.add_node_embedding(id, vec.to_vec(), "m").await.unwrap();
        ids.push(id);
    }
    graph.add_edge(Edge::new(ids[0], ids[1], "CONOCE")).await.unwrap();
    graph.add_edge(Edge::new(ids[1], ids[2], "CONOCE")).await.unwrap();
    for (name, body) in [("d0", "riego de cactus"), ("d1", "poda de rosales")] {
        graph
            .add_node(Node::new("Doc").with_property("name", s(name)).with_property("body", s(body)))
            .await
            .unwrap();
    }
    graph.create_index("Doc", "body", IndexType::FullText).await.unwrap();
    graph.import_turtle(FLORA).await.unwrap();
    graph.close().await.unwrap();
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Server {
    fn start(db: &Path, extra: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nopaldb-mcp"))
            .arg("--db")
            .arg(db)
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("arrancar nopaldb-mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut srv = Server { child, stdin, stdout, next_id: 0 };
        let init = srv.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "stdio-test", "version": "0" }
            }),
        );
        assert!(init["result"]["capabilities"]["tools"].is_object(), "initialize: {init}");
        assert_eq!(init["result"]["serverInfo"]["name"], "nopaldb-mcp", "initialize: {init}");
        assert_eq!(init["result"]["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        srv.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        srv
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).unwrap();
            assert!(n > 0, "el servidor cerró stdout esperando {method}");
            let msg: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
            if msg["id"] == json!(id) {
                return msg;
            }
        }
    }

    /// Llama una tool y devuelve `result`; falla si la llamada es un error.
    fn call(&mut self, tool: &str, args: Value) -> Value {
        let res = self.call_raw(tool, args);
        assert_ne!(res["isError"], json!(true), "{tool} devolvió error: {res}");
        // Un resultado JSON tiene que viajar también como `structuredContent`,
        // que MCP exige objeto: rmcp 3 descarta en silencio un arreglo.
        if serde_json::from_str::<Value>(&text_of(&res)).is_ok_and(|v| v.is_object() || v.is_array()) {
            assert!(res["structuredContent"].is_object(), "{tool} sin structuredContent: {res}");
        }
        res
    }

    fn call_raw(&mut self, tool: &str, args: Value) -> Value {
        let msg = self.request("tools/call", json!({ "name": tool, "arguments": args }));
        assert!(msg["error"].is_null(), "{tool}: error JSON-RPC {msg}");
        msg["result"].clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// El directorio de exports por defecto: `<db>.exports`, al lado de la base.
fn exports_dir(db: &Path) -> PathBuf {
    PathBuf::from(format!("{}.exports", db.display()))
}

fn temp_db() -> PathBuf {
    std::env::temp_dir().join(format!("nopaldb-mcp-test-{}", uuid::Uuid::new_v4()))
}

fn text_of(res: &Value) -> String {
    res["content"][0]["text"].as_str().unwrap_or_default().to_string()
}

fn names(rows: &Value, key: &str) -> Vec<String> {
    let mut v: Vec<String> = rows
        .as_array()
        .unwrap_or_else(|| panic!("se esperaba un arreglo: {rows}"))
        .iter()
        .filter_map(|r| r[key].as_str().map(str::to_string))
        .collect();
    v.sort();
    v
}

const ALL_TOOLS: [&str; 22] = [
    "classify_node",
    "cypher_query",
    "delete_node",
    "export_context_arrow",
    "find_path",
    "get_local_causal_context",
    "get_neighbors",
    "get_node",
    "graph_query",
    "index_project_structure",
    "list_instances",
    "list_subclasses",
    "nql_syntax",
    "record_episodic_event",
    "rename_project_entity",
    "run_pagerank",
    "schema_by_kind",
    "schema_info",
    "search_hybrid",
    "similar_nodes",
    "upsert_node",
    "validate_and_commit_pr_context",
];

/// Las tools de lectura sobre la base del fixture. Corre igual con y sin
/// `--readonly`: con la bandera la base se abre sellada.
fn check_read_tools(srv: &mut Server) {
    // Consulta y esquema.
    let q = srv.call("graph_query", json!({ "query": "find p.name from (p:Persona) order by p.name" }));
    assert_eq!(names(&q["structuredContent"]["rows"], "p.name"), ["ana", "beto", "carla"]);
    let ex = srv.call("graph_query", json!({ "query": "explain find p.name from (p:Persona)" }));
    assert!(!text_of(&ex).is_empty(), "explain: {ex}");
    // Cypher de solo lectura: se traduce a NQL y trae el NQL que corrió.
    let cy = srv.call(
        "cypher_query",
        json!({ "query": "MATCH (a:Persona)-[:CONOCE]->(b) RETURN b.name ORDER BY b.name" }),
    );
    assert_eq!(names(&cy["structuredContent"]["rows"], "b.name"), ["beto", "carla"]);
    assert_eq!(
        cy["structuredContent"]["nql"],
        json!("find b.name from (a:Persona)-[:CONOCE]->(b) order by b.name"),
        "{cy}"
    );
    let w = srv.call_raw("cypher_query", json!({ "query": "CREATE (p:Persona {name: 'eva'})" }));
    assert_eq!(w["isError"], json!(true), "{w}");
    assert!(text_of(&w).contains("read-only"), "{w}");
    let syn = srv.call("nql_syntax", json!({ "statement": "find" }));
    assert!(!text_of(&syn).is_empty() || !syn["structuredContent"].is_null(), "nql_syntax: {syn}");
    let si = srv.call("schema_info", json!({}));
    let labels = si["structuredContent"]["labels"].as_array().unwrap();
    assert!(labels.contains(&json!("Persona")) && labels.contains(&json!("Doc")), "{si}");
    assert!(si["structuredContent"]["edge_types"].as_array().unwrap().contains(&json!("CONOCE")), "{si}");
    assert!(si["structuredContent"]["node_count"].as_i64().unwrap() >= 5);
    let sk = srv.call("schema_by_kind", json!({}));
    let personas = sk["structuredContent"]["by_label"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["label"] == "Persona")
        .cloned()
        .unwrap();
    assert_eq!(personas["count"], 3);

    // Nodos y recorrido.
    let n = srv.call("get_node", json!({ "name": "beto", "label": "Persona" }));
    assert_eq!(n["structuredContent"]["name"], "beto");
    let id = n["structuredContent"]["id"].as_str().unwrap().to_string();
    let by_id = srv.call("get_node", json!({ "id": id }));
    assert_eq!(by_id["structuredContent"]["name"], "beto");
    let out = srv.call("get_neighbors", json!({ "id": "beto", "direction": "out" }));
    assert_eq!(names(&out["structuredContent"]["neighbors"], "name"), ["carla"], "{out}");
    let both = srv.call("get_neighbors", json!({ "id": "beto" }));
    assert_eq!(names(&both["structuredContent"]["neighbors"], "name"), ["ana", "carla"], "{both}");
    let path = srv.call("find_path", json!({ "from": "ana", "to": "carla" }));
    assert_eq!(path["structuredContent"]["found"], true, "{path}");
    assert_eq!(path["structuredContent"]["depth"], 2, "{path}");

    // Búsqueda y algoritmos.
    let pr = srv.call("run_pagerank", json!({ "label": "Persona", "top_n": 3 }));
    let ranked = pr["structuredContent"]["results"].as_array().unwrap();
    assert_eq!(ranked.len(), 3);
    assert_eq!(ranked[0]["name"], "carla", "el final del camino acumula rango: {pr}");
    let sim = srv.call("similar_nodes", json!({ "name": "ana", "model": "m", "k": 2 }));
    let sims = sim["structuredContent"]["results"].as_array().unwrap();
    assert_eq!(sims[0]["name"], "ana");
    assert_eq!(sims[1]["name"], "beto");
    let hy = srv.call("search_hybrid", json!({ "text": "cactus", "label": "Doc" }));
    let hits = hy["structuredContent"]["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{hy}");
    let hv = srv.call("search_hybrid", json!({ "vector": [0.0, 0.0, 1.0], "model": "m", "k": 1 }));
    assert_eq!(hv["structuredContent"]["hits"].as_array().unwrap().len(), 1, "{hv}");

    // Ontología.
    let c = srv.call("classify_node", json!({ "name": "roble", "class": "Planta" }));
    assert_eq!(c["structuredContent"]["is_instance"], true, "{c}");
    let c = srv.call("classify_node", json!({ "name": "musgo", "class": "Arbol" }));
    assert_eq!(c["structuredContent"]["is_instance"], false, "{c}");
    let li = srv.call("list_instances", json!({ "class": "Planta" }));
    assert_eq!(names(&li["structuredContent"]["instances"], "name"), ["musgo", "roble"], "{li}");
    let ls = srv.call("list_subclasses", json!({ "class": "Planta" }));
    assert_eq!(ls["structuredContent"]["direct_subclasses"], json!(["Arbol"]), "{ls}");
    assert_eq!(ls["structuredContent"]["indirect_subclasses"], json!(["Pino"]), "{ls}");
    // La clase va escapada también en la consulta transitiva: una comilla no
    // reescribe el filtro.
    let inj = srv.call("list_subclasses", json!({ "class": "x\" or ancestor.label = \"Planta" }));
    assert_eq!(inj["structuredContent"]["indirect_subclasses"], json!([]), "{inj}");
}

#[tokio::test]
async fn every_tool_answers_over_stdio() {
    let db = temp_db();
    fixture(&db).await;
    // Un export que dejó una corrida anterior: el arranque lo limpia.
    std::fs::create_dir_all(exports_dir(&db)).unwrap();
    let orphan = exports_dir(&db).join("nopaldb_export_viejo.json");
    let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    std::fs::File::create(&orphan).unwrap().set_modified(two_hours_ago).unwrap();
    let mut srv = Server::start(&db, &[]);
    assert!(!orphan.exists(), "el arranque no limpió el export viejo");

    // tools/list: las 21, cada una con su schema de entrada.
    let list = srv.request("tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let mut listed: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    listed.sort();
    assert_eq!(listed, ALL_TOOLS);
    assert!(tools.iter().all(|t| t["inputSchema"].is_object()));

    check_read_tools(&mut srv);

    // Escritura idempotente.
    let up = srv.call(
        "upsert_node",
        json!({ "label": "Persona", "key": "name", "props": { "name": "dora", "edad": 30 } }),
    );
    assert_eq!(up["structuredContent"]["outcome"], "created", "{up}");
    let again = srv.call(
        "upsert_node",
        json!({ "label": "Persona", "key": "name", "props": { "name": "dora", "edad": 30 } }),
    );
    assert_eq!(again["structuredContent"]["outcome"], "unchanged", "{again}");
    let del = srv.call("delete_node", json!({ "label": "Persona", "key": "name", "value": "dora" }));
    assert_eq!(del["structuredContent"]["deleted"], true, "{del}");
    let del = srv.call("delete_node", json!({ "label": "Persona", "key": "name", "value": "dora" }));
    assert_eq!(del["structuredContent"]["deleted"], false, "{del}");

    // Contexto de proyecto.
    let entities = json!([
        { "kind": "File", "name": "riego.rs", "path": "src/riego.rs" },
        { "kind": "Function", "name": "regar", "path": "src/riego.rs", "belongs_to": "riego.rs" }
    ]);
    let ix = srv.call("index_project_structure", json!({ "entities": entities }));
    assert_eq!(ix["structuredContent"]["entities_indexed"], 2, "{ix}");
    let files = srv.call("graph_query", json!({ "query": "find f.name from (f:File) where f.path = \"src/riego.rs\"" }));
    assert_eq!(names(&files["structuredContent"]["rows"], "f.name"), ["riego.rs"], "{files}");
    let ix = srv.call("index_project_structure", json!({ "entities": entities }));
    assert_eq!(ix["structuredContent"]["entities_indexed"], 2, "{ix}");
    let files = srv.call("graph_query", json!({ "query": "find count(*) as n from (f:File)" }));
    assert_eq!(files["structuredContent"]["rows"][0]["n"], 1, "reindexar no duplica: {files}");

    let ev = srv.call(
        "record_episodic_event",
        json!({
            "file_path": "src/riego.rs",
            "rationale": "regar al amanecer",
            "affected_entities": [{ "kind": "File", "name": "riego.rs", "path": "src/riego.rs" }],
            "applied_rules": ["SOLID"],
            "commit_hash": "abc1"
        }),
    );
    assert_eq!(ev["structuredContent"]["entities_linked"], 1, "{ev}");
    let amend = srv.call(
        "record_episodic_event",
        json!({
            "file_path": "src/riego.rs",
            "rationale": "regar al amanecer, sin encharcar",
            "affected_entities": [],
            "applied_rules": [],
            "commit_hash": "abc2",
            "previous_head_hash": "abc1"
        }),
    );
    assert_eq!(amend["structuredContent"]["status"], "amended", "{amend}");
    let ctx = srv.call("get_local_causal_context", json!({ "filepath": "src/riego.rs" }));
    let ctx_text = ctx["structuredContent"]["context"].as_str().unwrap();
    assert!(ctx_text.contains("sin encharcar") && ctx_text.contains("abc2"), "{ctx}");

    let rn = srv.call("rename_project_entity", json!({ "old_path": "src/riego.rs", "new_path": "src/agua.rs" }));
    assert_eq!(rn["structuredContent"]["status"], "success", "{rn}");
    let renamed = srv.call("graph_query", json!({ "query": "find f.name from (f:File) where f.path = \"src/agua.rs\"" }));
    assert_eq!(names(&renamed["structuredContent"]["rows"], "f.name"), ["agua.rs"], "{renamed}");
    let ctx = srv.call("get_local_causal_context", json!({ "filepath": "src/agua.rs" }));
    assert!(ctx["structuredContent"]["context"].as_str().unwrap().contains("sin encharcar"), "{ctx}");

    let pr_ctx = srv.call(
        "validate_and_commit_pr_context",
        json!({
            "consolidated_rationale": "riego por goteo",
            "final_main_commit_hash": "f00d",
            "changed_files": ["src/agua.rs", "src/poda.rs"]
        }),
    );
    assert_eq!(pr_ctx["structuredContent"]["status"], "sealed", "{pr_ctx}");
    assert_eq!(pr_ctx["structuredContent"]["files_linked"], 2, "{pr_ctx}");

    let exp = srv.call("export_context_arrow", json!({ "query": "find p.name from (p:Persona)" }));
    let file = PathBuf::from(exp["structuredContent"]["path"].as_str().unwrap());
    // Por defecto, junto a la base y no en /tmp.
    assert_eq!(file.parent(), Some(exports_dir(&db).as_path()), "{exp}");
    assert_eq!(exp["structuredContent"]["format"], "json", "{exp}");
    let exported: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(exported.as_array().unwrap().len(), 3, "{exported}");
    let _ = std::fs::remove_file(&file);

    // Recursos.
    let res = srv.request("resources/list", json!({}));
    let uris = names(&res["result"]["resources"], "uri");
    assert_eq!(uris, ["nopal://schema", "nopal://stats"]);
    let stats = srv.request("resources/read", json!({ "uri": "nopal://stats" }));
    let stats: Value = serde_json::from_str(stats["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert!(stats["edge_count"].as_i64().unwrap() >= 2, "{stats}");
    let schema = srv.request("resources/read", json!({ "uri": "nopal://schema" }));
    assert!(schema["result"]["contents"][0]["text"].as_str().unwrap().contains("Persona"));

    drop(srv);
    let _ = std::fs::remove_dir_all(&db);
    let _ = std::fs::remove_dir_all(exports_dir(&db));
}

#[tokio::test]
async fn readonly_rejects_every_write_tool() {
    let db = temp_db();
    fixture(&db).await;
    let mut srv = Server::start(&db, &["--readonly"]);

    let entity = json!([{ "kind": "File", "name": "a.rs", "path": "a.rs" }]);
    let writes = [
        ("graph_query", json!({ "query": "add (p:Persona {name: \"eva\"})" })),
        ("upsert_node", json!({ "label": "Persona", "key": "name", "props": { "name": "eva" } })),
        ("delete_node", json!({ "label": "Persona", "key": "name", "value": "ana" })),
        ("index_project_structure", json!({ "entities": entity })),
        (
            "record_episodic_event",
            json!({ "file_path": "a.rs", "rationale": "r", "affected_entities": [], "applied_rules": [] }),
        ),
        ("rename_project_entity", json!({ "old_path": "a.rs", "new_path": "b.rs" })),
        (
            "validate_and_commit_pr_context",
            json!({ "consolidated_rationale": "r", "final_main_commit_hash": "h", "changed_files": [] }),
        ),
    ];
    for (tool, args) in writes {
        let res = srv.call_raw(tool, args);
        assert_eq!(res["isError"], json!(true), "{tool} escribió en modo solo lectura: {res}");
        assert!(text_of(&res).contains("read-only"), "{tool}: {res}");
    }

    // NQL que escribe sin empezar con la palabra de escritura: comentarios
    // delante, índices, y exportaciones que escriben un archivo.
    let out = db.join("fuga.json");
    let out = out.to_str().unwrap();
    let sneaky = [
        "/* nota */ add (p:Persona {name: \"eva\"})".to_string(),
        "// nota\ndelete (p:Persona) where p.name = \"ana\"".to_string(),
        "  UPDATE (p:Persona) set p.name = \"eva\" where p.name = \"ana\"".to_string(),
        "create index on Persona(name) type hash".to_string(),
        "drop index persona_name".to_string(),
        "create unique index on Persona(name)".to_string(),
        "upsert (p:Persona {name: \"eva\"})".to_string(),
        "/* x */ upsert (p:Persona {name: \"ana\"})-[:CONOCE]->(q:Persona {name: \"eva\"})".to_string(),
        format!("find p.name from (p:Persona) export json with path=\"{out}\""),
        format!("/* x */ find p.name from (p:Persona) export csv with path=\"{out}\""),
    ];
    for tool in ["graph_query", "export_context_arrow"] {
        for q in &sneaky {
            let res = srv.call_raw(tool, json!({ "query": q }));
            assert_eq!(res["isError"], json!(true), "{tool} aceptó {q:?} en solo lectura: {res}");
            assert!(text_of(&res).contains("read-only"), "{tool} {q:?}: {res}");
        }
    }
    assert!(!Path::new(out).exists(), "una exportación escribió {out}");

    // `explain upsert` solo describe el plan: pasa con la base sellada.
    let ex = srv.call("graph_query", json!({ "query": "explain upsert (p:Persona {name: \"eva\"})" }));
    assert_ne!(ex["isError"], json!(true), "{ex}");

    // Nada cambió, y leer sigue funcionando con la base sellada.
    let q = srv.call("graph_query", json!({ "query": "find count(*) as n from (p:Persona)" }));
    assert_eq!(q["structuredContent"]["rows"][0]["n"], 3);
    let q = srv.call("graph_query", json!({ "query": "find p.name from (p:Persona) where p.name = \"ana\"" }));
    assert_eq!(names(&q["structuredContent"]["rows"], "p.name"), ["ana"]);
    let si = srv.call("schema_info", json!({}));
    assert_eq!(si["structuredContent"]["readonly"], true);
    check_read_tools(&mut srv);
    let prof = srv.call("graph_query", json!({ "query": "profile find p.name from (p:Persona)" }));
    assert!(!text_of(&prof).is_empty(), "{prof}");
    // Exportar sin `path` devuelve los datos: no escribe nada.
    let csv = srv.call("graph_query", json!({ "query": "find p.name from (p:Persona) order by p.name export csv" }));
    assert!(text_of(&csv).contains("carla"), "{csv}");
    let exp = srv.call("export_context_arrow", json!({ "query": "find p.name from (p:Persona)" }));
    let _ = std::fs::remove_file(exp["structuredContent"]["path"].as_str().unwrap());
    let stats = srv.request("resources/read", json!({ "uri": "nopal://stats" }));
    assert!(stats["result"]["contents"][0]["text"].is_string(), "{stats}");

    drop(srv);
    let _ = std::fs::remove_dir_all(&db);
    let _ = std::fs::remove_dir_all(exports_dir(&db));
}

/// Lo que `nql_syntax` le enseña al agente tiene que correr por `graph_query`:
/// antes ofrecía `sketch` y `commit`, que `execute_statement` rechaza.
#[tokio::test]
async fn every_syntax_example_runs_through_graph_query() {
    let db = temp_db();
    fixture(&db).await;
    let mut srv = Server::start(&db, &[]);

    let syn = srv.call("nql_syntax", json!({}));
    let reference = syn["structuredContent"]["syntax_reference"].as_str().unwrap().to_string();
    let examples: Vec<&str> = reference
        .split("```nql\n")
        .skip(1)
        .flat_map(|block| block.split("```").next().unwrap().lines())
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert!(examples.len() >= 15, "{reference}");
    for nql in examples {
        srv.call("graph_query", json!({ "query": nql }));
    }

    // Repetido, el upsert del ejemplo encuentra el nodo y no cambia nada.
    let up = srv.call(
        "graph_query",
        json!({ "query": "upsert (p:Person { email: \"alice@example.org\", name: \"Alice\" }) set p.visits = 1" }),
    );
    let w = &up["structuredContent"];
    assert_eq!((w["nodes_created"].as_u64(), w["nodes_unchanged"].as_u64()), (Some(0), Some(1)), "{up}");
    assert_eq!(w["matched_ids"].as_array().map(Vec::len), Some(1), "{up}");

    drop(srv);
    let _ = std::fs::remove_dir_all(&db);
    let _ = std::fs::remove_dir_all(exports_dir(&db));
}
