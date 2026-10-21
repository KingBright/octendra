# Octendra independent deployment

Keep the existing maintenance clients, Gateway, Gemini app, reverse proxy, data and credentials running. This template prepares a separate Octendra instance; it is deliberately incomplete until the new origin, free loopback port, state directory and authorization are confirmed. Do not point its state directory at the existing instance or copy its credentials.

Build from the primary main tree with the established release runner and cache. Install the verified Octendra binaries under their new product paths and application IDs; do not replace the maintenance binaries. Render `gateway.json.in` only with explicitly approved values. Keep the new Gateway on loopback behind the separately approved new-domain ingress; do not repoint the old ingress.

The new `/status`, `/manifest.webmanifest`, `/branding/octendra-192.png`, `/branding/octendra-512.png` and MCP/OAuth metadata share the Octendra presentation. Their absolute URLs derive from the configured origin. New Gemini setup uses the confirmed new origin plus `/mcp`; retain the old saved app as the maintenance connection. A saved Gemini app name is client-managed and is not proof of server metadata or connection health.

Before retiring anything, verify the new connection, explicitly selected device, read/edit/execute, file upload/download, screen observation and authorized input. macOS privacy grants, Windows task ACLs and device enrollment remain platform/user authorization steps; do not bypass them. Retire the old channel only after the complete new path passes and the user authorizes retirement.
