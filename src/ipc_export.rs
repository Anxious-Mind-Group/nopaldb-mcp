use nopaldb::Graph;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

/// Prefijo y extensión de los archivos que crea el export. La limpieza solo
/// toca nombres con esta forma: el directorio puede ser uno del usuario.
const PREFIX: &str = "nopaldb_export_";
const EXTENSION: &str = ".json";

/// Vida de un export: pasado este tiempo, la siguiente limpieza lo borra.
pub const EXPORT_TTL: Duration = Duration::from_secs(3600);

/// Límite por defecto del archivo exportado.
pub const DEFAULT_MAX_EXPORT_BYTES: u64 = 512 * 1024 * 1024;

/// Dónde y hasta cuánto escribe `export_context_arrow`.
#[derive(Debug, Clone)]
pub struct ExportConfig {
    pub dir: PathBuf,
    pub max_bytes: u64,
}

impl ExportConfig {
    /// Directorio por defecto: `<base>.exports`, hermano de la base y no
    /// dentro de ella, para no mezclarse con los archivos del motor. No es
    /// `/tmp`: ahí lo leería cualquier usuario y el nombre se podría ocupar
    /// antes que nosotros.
    pub fn default_dir(db: &str) -> PathBuf {
        let db = db.trim_end_matches('/');
        PathBuf::from(format!("{db}.exports"))
    }
}

pub struct ExportOutcome {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Ejecuta la consulta y escribe el resultado como JSON (arreglo de filas) en
/// un archivo nuevo del directorio de exports, con permisos 0600.
///
/// El JSON se escribe en streaming con tope: al pasar `max_bytes` se aborta y
/// se borra el archivo parcial. El tope protege el disco y evita la copia en
/// bytes del resultado; las filas que materializa el motor al consultar no
/// las limita.
///
/// Antes era un mmap con extensión `.arrow`: no aportaba nada (quien consume
/// lee el archivo igual) y el contenido nunca fue Arrow.
pub async fn export_to_file(graph: &Graph, query: &str, cfg: &ExportConfig) -> Result<ExportOutcome, String> {
    let result = graph.execute_nql(query).await.map_err(|e| e.to_string())?;
    let rows = crate::tools::query_result_to_value(&result);

    ensure_dir(&cfg.dir).map_err(|e| format!("cannot create export dir {}: {e}", cfg.dir.display()))?;
    sweep(&cfg.dir, EXPORT_TTL, SystemTime::now());

    let path = cfg.dir.join(format!("{PREFIX}{}{EXTENSION}", Uuid::new_v4()));
    let file = create_private(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    let mut out = CappedWriter { inner: BufWriter::new(file), written: 0, max: cfg.max_bytes };
    let written = serde_json::to_writer(&mut out, &rows)
        .map_err(io::Error::from)
        .and_then(|()| out.inner.flush())
        .map(|()| out.written);
    match written {
        Ok(bytes) => Ok(ExportOutcome { path, bytes }),
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            if e.kind() == io::ErrorKind::FileTooLarge {
                Err(format!(
                    "export exceeds the limit of {} bytes (--max-export-bytes); narrow the query \
                     or add a limit",
                    cfg.max_bytes
                ))
            } else {
                Err(format!("cannot write {}: {e}", path.display()))
            }
        }
    }
}

/// Borra los exports con más de `ttl`. Solo archivos con el nombre que crea
/// este módulo; los errores se ignoran (otro proceso pudo borrarlo antes).
pub fn sweep(dir: &Path, ttl: Duration, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with(PREFIX) && name.ends_with(EXTENSION)) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let expired = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > ttl);
        if meta.is_file() && expired {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn ensure_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// `create_new`: nunca reutiliza ni sigue un archivo existente.
fn create_private(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)
}

struct CappedWriter<W> {
    inner: W,
    written: u64,
    max: u64,
}

impl<W: Write> Write for CappedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written + buf.len() as u64 > self.max {
            return Err(io::Error::from(io::ErrorKind::FileTooLarge));
        }
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nopaldb::types::{Node, PropertyValue};

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("nopaldb-mcp-{tag}-{}", Uuid::new_v4()))
    }

    async fn graph_with_plants(dir: &Path, n: usize) -> Graph {
        let g = Graph::open(dir.join("g.db")).await.unwrap();
        for i in 0..n {
            let node = Node::new("Planta").with_property("name", PropertyValue::String(format!("planta-{i}")));
            g.add_node(node).await.unwrap();
        }
        g
    }

    fn exports_in(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
        entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()
    }

    #[tokio::test]
    async fn export_is_private_json_in_its_own_dir() {
        let root = temp_dir("export");
        let g = graph_with_plants(&root, 3).await;
        let cfg = ExportConfig { dir: root.join("g.db.exports"), max_bytes: DEFAULT_MAX_EXPORT_BYTES };

        let out = export_to_file(&g, "find p.name from (p:Planta) order by p.name", &cfg).await.unwrap();
        assert_eq!(out.path.parent(), Some(cfg.dir.as_path()));
        assert!(out.path.to_str().unwrap().ends_with(".json"));
        let rows: serde_json::Value = serde_json::from_slice(&std::fs::read(&out.path).unwrap()).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 3, "{rows}");
        assert_eq!(out.bytes, std::fs::metadata(&out.path).unwrap().len());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&out.path), 0o600);
            assert_eq!(mode(&cfg.dir), 0o700);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn export_over_the_limit_fails_and_leaves_nothing() {
        let root = temp_dir("limit");
        let g = graph_with_plants(&root, 50).await;
        let cfg = ExportConfig { dir: root.join("exports"), max_bytes: 200 };

        let err = export_to_file(&g, "find p.name from (p:Planta)", &cfg).await.err().unwrap();
        assert!(err.contains("--max-export-bytes"), "{err}");
        assert_eq!(exports_in(&cfg.dir), Vec::<String>::new(), "quedó un archivo parcial");

        // Bajo el límite, la misma consulta pasa.
        let cfg = ExportConfig { max_bytes: DEFAULT_MAX_EXPORT_BYTES, ..cfg };
        export_to_file(&g, "find p.name from (p:Planta)", &cfg).await.unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_removes_only_old_exports() {
        let dir = temp_dir("sweep");
        ensure_dir(&dir).unwrap();
        let old = SystemTime::now() - EXPORT_TTL - Duration::from_secs(60);
        for (name, mtime) in [
            ("nopaldb_export_viejo.json", Some(old)),
            ("nopaldb_export_nuevo.json", None),
            ("notas_del_usuario.json", Some(old)),
            ("nopaldb_export_viejo.txt", Some(old)),
        ] {
            let f = File::create(dir.join(name)).unwrap();
            if let Some(t) = mtime {
                f.set_modified(t).unwrap();
            }
        }

        sweep(&dir, EXPORT_TTL, SystemTime::now());
        let mut left = exports_in(&dir);
        left.sort();
        assert_eq!(left, ["nopaldb_export_nuevo.json", "nopaldb_export_viejo.txt", "notas_del_usuario.json"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
