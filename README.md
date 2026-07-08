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
