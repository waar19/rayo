# Rayo

Motor de búsqueda de archivos ultrarrápido para Windows, escrito en Rust e inspirado en Everything.

[English](README.md) | Español

## Qué hace Rayo hoy (MVP)

- Enumera la MFT de NTFS usando `FSCTL_ENUM_USN_DATA`.
- Construye y persiste un índice de archivos por FRN.
- Reconstruye rutas completas subiendo por FRNs padre.
- Busca por subcadena con filtros:
  - `--ext`
  - `--under`
  - `--glob`
  - `--dirs-only`
  - `--files-only`
  - `--limit`
- Aplica cambios en vivo desde USN Journal (`FSCTL_READ_USN_JOURNAL`).

## Estructura del proyecto

- `crates/rayo-core`: indexado, búsqueda, integración NTFS/USN, persistencia.
- `crates/rayo-cli`: interfaz CLI (`index`, `search`, `watch`).
- `crates/rayo-service`: servicio de fondo elevado con índice en memoria y API por named pipe.
- `crates/rayo-gui`: GUI nativa (`Slint`, estilo Fluent) con búsqueda por servicio o fallback local.

## Requisitos

- Windows (volumen NTFS).
- Toolchain de Rust (`cargo`).
- Privilegios de Administrador para `index` y `watch` (necesarios para leer MFT/USN).

## Inicio rápido

```powershell
# Compilar
cargo build

# Crear índice (terminal como Administrador)
cargo run -p rayo-cli -- index --drive C --output .\c.rayo

# Buscar
cargo run -p rayo-cli -- search --index .\c.rayo --query report --ext pdf

# Mantener índice actualizado (terminal como Administrador)
cargo run -p rayo-cli -- watch --drive C --index .\c.rayo

# Levantar servicio de fondo (terminal como Administrador)
cargo run -p rayo-service -- --drive C --index .\c.rayo

# Abrir GUI (intenta servicio, si no fallback al índice local)
cargo run -p rayo-gui -- --index .\c.rayo

# Opcional: instalar menú contextual de Explorer para usuario actual
cargo run -p rayo-cli -- shell install --gui-path .\target\debug\rayo-gui.exe
```

### Acciones de GUI

- Selecciona una fila y usa botones: `Open`, `Open as admin`, `Open folder`, `Copy path`.
- Búsqueda con debounce (~80 ms por defecto) para no congelar mientras escribes.
- Consultas vacías o de 1 caracter no disparan búsqueda completa salvo que uses `--under`.

## Resultados de validación (Windows 11, C:, Jul 2026)

Validación real sobre `C:` NTFS en terminal elevada:

- Indexado inicial a `c-base.rayo`: `INDEX_WALL_MS=133246` (~2m13s).
- Tamaño del índice: `364427087` bytes (~347.5 MiB).
- Entradas cargadas al iniciar watch: `6192118`.

Muestras de latencia de búsqueda sobre índice real:

- `--query report --limit 20`: `20` resultados en `2.4751112s` (wall-clock `15315 ms`).
- `--query report --ext pdf --limit 20`: `20` resultados en `1.9989417s` (wall-clock `17261 ms`).
- `--query system --under C:\Windows --limit 20`: `20` resultados en `2.7214587s` (wall-clock `18455 ms`).
- `--query kernel --glob "**/*.dll" --limit 20`: `20` resultados en `2.2629864s` (wall-clock `16657 ms`).

La validación de watch cubrió creación, renombrado y borrado de archivos.

Validación de servicio + integración:

- `rayo-service` inició elevado con índice existente y expuso `\\.\pipe\rayo-query`.
- Consulta no elevada por named pipe devolvió resultados JSON correctamente.
- `rayo-cli shell install` y `shell uninstall` crearon y removieron entradas de menú contextual en `HKCU\Software\Classes`.

## Hoja de ruta

### Fase 2

- Búsqueda de contenido estilo ripgrep con `grep`/`ignore`.
- Consultas sintácticas con `tree-sitter`.

### Fase 3

- Seguir puliendo la GUI Fluent nativa (menú contextual, atajos de teclado, acciones shell).
- Arquitectura orientada a servicio:
  - servicio de fondo para índice/watch,
  - IPC para clientes de consulta (named pipes),
  - GUI e integraciones Windows como clientes livianos.
- Integraciones potenciales:
  - plugin de PowerToys Run,
  - acción de menú contextual en Explorer ("Search with Rayo here").

## Licencia

[MIT](LICENSE)
