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
- `crates/rayo-cli`: interfaz CLI (`index`, `search`, `content`, `watch`).
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
# Una unidad:
cargo run -p rayo-cli -- index --drive C --output .\c.rayo
# Multi-unidad (genera c.rayo, d.rayo desde el path base):
cargo run -p rayo-cli -- index --drive C,D --output .\c.rayo

# Buscar
cargo run -p rayo-cli -- search --index .\c.rayo --query report --ext pdf

# Búsqueda de contenido (regex, estilo ripgrep)
cargo run -p rayo-cli -- content --query "Rayo GUI search client" --under . --limit 20

# Mantener índice actualizado (terminal como Administrador)
cargo run -p rayo-cli -- watch --drive C --index .\c.rayo

# Levantar servicio de fondo (terminal como Administrador)
# Una unidad:
cargo run -p rayo-service -- --drive C --index .\c.rayo
# Multi-unidad con merge:
cargo run -p rayo-service -- --drives C,D --index .\c.rayo

# Abrir GUI (intenta servicio, si no fallback al índice local)
cargo run -p rayo-gui -- --index .\c.rayo

# Opcional: instalar menús contextuales de Explorer (archivo/carpeta/fondo)
cargo run -p rayo-cli -- shell install --gui-path .\target\debug\rayo-gui.exe

# Diagnóstico de integración shell
cargo run -p rayo-cli -- shell doctor --gui-path .\target\debug\rayo-gui.exe
```

### Acciones de GUI

- Selecciona una fila y usa botones: `Open`, `Open as admin`, `Open folder`, `Copy path`.
- Panel de Settings integrado para ajustar alcance, extensión, modo, límite y debounce.
- Atajos de teclado: `Ctrl+,` abre Settings y `Esc` cierra Settings.
- Consultas vacías o de 1 caracter no disparan búsqueda completa salvo que uses `--under`.

### Flags contextuales de GUI

- `--under <ruta>`: abre la GUI acotada a una carpeta (útil desde Explorer).
- `--query <texto>`: precarga la caja de búsqueda.
- `--open <ruta>`: deriva el contexto desde un archivo/carpeta para flujo de click derecho.

### Modo trigrama opcional

Para queries largas, el modo trigrama puede reducir mucho la latencia de la primera búsqueda:

```powershell
# CLI puntual
cargo run --release -p rayo-cli -- search --index .\c.rayo --query tickettrack --trigram

# Modo de servicio (clientes por named pipe, también multi-unidad)
cargo run -p rayo-service -- --drives C,D --index .\c.rayo --trigram --metrics-interval-secs 30
```

Tradeoff: el índice por trigrama usa más RAM, pero acelera consultas largas/poco frecuentes.

## Resultados de validación (Windows 11, C:, Jul 2026)

Validación real sobre `C:` NTFS en terminal elevada:

- Tamaño del índice: ~`365 MB`.
- Entradas cargadas: ~`6.2M`.

Muestras de latencia de búsqueda sobre índice real (release):

- `--query report --limit 20`: `20` resultados en `6.673 ms`.
- `--query report --limit 20 --trigram`: `20` resultados en `6.644 ms`.
- `--query tickettrack --limit 20`: `1` resultado en `7.685 ms`.
- `--query tickettrack --limit 20 --trigram`: `1` resultado en `0.502 ms`.
- `--query zzzqqxxnotfound --limit 20`: `0` resultados en `7.321 ms`.
- `--query zzzqqxxnotfound --limit 20 --trigram`: `0` resultados en `0.026 ms`.

La validación de watch cubrió creación, renombrado y borrado de archivos.

Validación de servicio + integración:

- `rayo-service` inició elevado con índice existente y expuso `\\.\pipe\rayo-query`.
- Consulta no elevada por named pipe devolvió resultados JSON correctamente.
- `rayo-cli shell install`, `shell doctor` y `shell uninstall` validaron la integración de Explorer para archivo/carpeta/fondo en `HKCU\Software\Classes`.

## Hoja de ruta

### Siguiente

- Consultas sintácticas con `tree-sitter`.
- Llevar búsqueda de contenido al servicio y a la GUI.

### Fase 3

- Seguir puliendo la GUI Fluent nativa (menú contextual, atajos de teclado, acciones shell).
- Arquitectura orientada a servicio:
  - servicio de fondo para índice/watch,
  - IPC para clientes de consulta (named pipes),
  - GUI e integraciones Windows como clientes livianos.
- Integraciones potenciales:
  - plugin de PowerToys Run,
  - acción de menú contextual en Explorer ("Search with Rayo here").

## CI y empaquetado de release

- Pipeline de CI: [`.github/workflows/ci.yml`](.github/workflows/ci.yml) ejecuta `fmt`, `test`, build release en Windows y build .NET no bloqueante para el scaffold del plugin PowerToys.
- Helper de empaquetado Windows: [`scripts/release-windows.ps1`](scripts/release-windows.ps1)

```powershell
pwsh .\scripts\release-windows.ps1
```

Esto genera `dist/rayo-windows.zip` con `rayo-cli.exe`, `rayo-service.exe`, `rayo-gui.exe` y documentación.

## Plugin de PowerToys Run

- Proyecto del plugin: [`integrations/powertoys-run`](integrations/powertoys-run)
- Keyword de activación: `ry`
- Dependencia en runtime: `rayo-service` corriendo como Administrador (`\\.\pipe\rayo-query`)

### Build e instalación manual

```powershell
dotnet build .\integrations\powertoys-run\Community.PowerToys.Run.Plugin.Rayo.csproj -c Release
dotnet publish .\integrations\powertoys-run\Community.PowerToys.Run.Plugin.Rayo.csproj -c Release -o .\dist\powertoys-run\RayoPlugin
```

Copia salida del plugin a:

`%LOCALAPPDATA%\Microsoft\PowerToys\PowerToys Run\Plugins\Rayo\`

Luego reinicia PowerToys y busca con:

`ry <consulta>`

### Ejecutar como servicio en segundo plano (recomendado)

Usa el modo de tarea programada para que Rayo corra sin ventana de consola visible:

```powershell
rayo-cli service install --service-exe "$env:LOCALAPPDATA\Rayo\rayo-service.exe" --drives C
rayo-cli service status
rayo-cli service uninstall
```

Defaults del modo en segundo plano:

- Índices: `%ProgramData%\Rayo\<drive>.rayo`
- Log del servicio: `%ProgramData%\Rayo\service.log`

### Instalador con detección de dependencias

Instalación en un comando desde último GitHub Release:

```powershell
irm https://raw.githubusercontent.com/waar19/rayo/main/scripts/install-powertoys-plugin.ps1 | iex
```

Instalación local con zip explícito:

```powershell
pwsh .\scripts\install-powertoys-plugin.ps1 -PluginZipPath .\dist\powertoys-run\RayoPlugin.zip -AutoInstallDependencies -RestartPowerToys
```

Qué hace:
- Detecta PowerToys.
- Instala plugin en `%LOCALAPPDATA%\Microsoft\PowerToys\PowerToys Run\Plugins\Rayo\`.
- Instala `rayo-service.exe` y `rayo-cli.exe` en `%LOCALAPPDATA%\Rayo\` para que Enter pueda iniciar el servicio desde PowerToys.
- Registra/inicia la tarea programada `Rayo Service` para arranque real en segundo plano.
- Soporta `RAYO_SERVICE_PATH` como override para ubicación personalizada del servicio.

### Assets de release

- Workflow por tag publica:
  - `rayo-windows.zip`
  - `RayoPlugin.zip`
- Instalador descarga `RayoPlugin.zip` del último release automáticamente cuando no recibe `-PluginZipPath`.

### Solución de errores de inicialización en PowerToys

Si PowerToys muestra error al iniciar plugin Rayo:

1. Verifica que usas último release (`v0.1.5` o superior).
2. Reinstala plugin:
   ```powershell
   irm https://raw.githubusercontent.com/waar19/rayo/main/scripts/install-powertoys-plugin.ps1 | iex
   ```
3. Si persiste, revisa logs:
   `%LOCALAPPDATA%\Microsoft\PowerToys\PowerToys Run\Logs\<version>\<fecha>.txt`
4. Busca:
   `Can't find class implement IPlugin` o errores de carga `System.Runtime`.

5. Si logs muestran mismatch de `IPlugin`, tu paquete incluyó DLLs del host (`Wox.Plugin.dll` / `PowerToys.*.dll`). Reinstala desde último release.

## Licencia

[MIT](LICENSE)
