# Rayo

Motor de busqueda de archivos ultrarrapido para Windows, escrito en Rust.

## Objetivo del MVP

- Indexar nombres de archivos y carpetas en volumenes NTFS usando MFT/USN Journal.
- Buscar por nombre con filtros de extension, prefijo de ruta y glob.
- Mantener el indice actualizado usando el USN Journal.
- Operar desde CLI para validar rendimiento antes de crear GUI.

## Comandos iniciales

```powershell
cargo run -p rayo-cli -- index --drive C --output .\c.rayo
cargo run -p rayo-cli -- search --index .\c.rayo --query report --ext pdf
cargo run -p rayo-cli -- watch --drive C --index .\c.rayo
```

## Validacion en caliente (Windows 11, C:, Jul 2026)

Resultados de una corrida real en volumen NTFS C: con sesion elevada:

- Indexado inicial de `C:` a `c-base.rayo`: `INDEX_WALL_MS=133246` (~2m13s).
- Tamano del indice generado: `364427087` bytes (~347.5 MiB).
- Conteo de entradas cargadas al iniciar `watch`: `6192118 entradas`.

### Latencia de busqueda (indice real)

Pruebas ejecutadas sobre indice real con `rayo-cli search`:

- `--query report --limit 20`: `Resultados: 20 en 2.4751112s` (wall-clock total: `15315 ms`).
- `--query report --ext pdf --limit 20`: `Resultados: 20 en 1.9989417s` (wall-clock total: `17261 ms`).
- `--query system --under C:\Windows --limit 20`: `Resultados: 20 en 2.7214587s` (wall-clock total: `18455 ms`).
- `--query kernel --glob "**/*.dll" --limit 20`: `Resultados: 20 en 2.2629864s` (wall-clock total: `16657 ms`).

Nota: el tiempo "Resultados ... en ..." es el medido dentro de la app; el wall-clock incluye arranque del proceso e impresion de resultados.

### Validacion de watch (creacion, rename, borrado)

Se valido `watch` sobre indices separados (`c-watch*.rayo`) para no contaminar el indice base:

- Creacion detectada: `Resultados: 1` para `rayo_watch_token_20260708_1540.txt`.
- Rename detectado:
  - nombre anterior: `Resultados: 0`,
  - nombre nuevo: `Resultados: 1` para `rayo_watch_token_20260708_1545_renamed.txt`.
- Borrado detectado: en el log de `watch`, el total bajo de `6192087` a `6192086` tras eliminar el archivo de prueba.

Importante: en estas pruebas automatizadas, `watch` se detuvo por timeout (kill de proceso), no via Ctrl+C. Esa salida no graciosa puede dejar el archivo de indice temporal en estado inconsistente si se lee justo durante/escritura.
