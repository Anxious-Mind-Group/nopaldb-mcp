# Security Policy

## Reportar vulnerabilidades

Por favor **no** abras issues públicos para vulnerabilidades. Usa los
[GitHub Security Advisories de este repositorio](https://github.com/Anxious-Mind-Group/nopaldb-mcp/security/advisories/new)
para reportar en privado. Intentamos responder en un plazo razonable y coordinar
la divulgación.

## Notas sobre el transporte SSE

El transporte HTTP/SSE incluye hardening por defecto (ver `src/auth.rs` y `src/main.rs`):

- Bind a loopback (`127.0.0.1`) por defecto.
- Fail-closed: se niega a hacer bind en una interfaz no-loopback sin un bearer
  token (`NOPALDB_MCP_TOKEN` o `--auth-token`), salvo opt-out explícito con
  `--insecure-no-auth`.
- Comparación del token en tiempo constante (`subtle::ConstantTimeEq`).
- CORS restringido a los orígenes configurados con `--cors-origin`
  (sin configuración = same-origin).

El transporte stdio no expone red.
