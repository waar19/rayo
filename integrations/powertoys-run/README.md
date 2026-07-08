# PowerToys Run Plugin Scaffold

This folder contains a minimal .NET scaffold for `Community.PowerToys.Run.Plugin.Rayo`.

Current scope:
- Named pipe client that queries `\\.\pipe\rayo-query`.
- Response DTOs and async query method.

Next step:
- Wire this client to actual PowerToys Run plugin interfaces and result actions.
