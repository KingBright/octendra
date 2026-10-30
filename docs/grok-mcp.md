# Grok / Grok Bot

Use the Gateway's public `/mcp` URL. Grok and Grok Bot have separate connector
registries; authorize and test each client with the intended account.

## Grok web

Open `https://grok.com/connectors`, choose **New Connector → Custom**, enter
`Octendra` and the MCP URL, then complete the Gateway login in the OAuth popup.
The observed Grok registration uses public-client authentication (`none`),
PKCE S256 and this exact callback, including its trailing slash:

```text
https://grok.com/connectors-oauth-exchange-code/
```

Add that callback to `redirect_uris` and `https://grok.com` to `allowed_origins`
on the existing Gateway. Keep the existing entries and OAuth state.

## Grok Bot

Sign in to the official Grok Bot app with the intended account. Ask a Bot to add
the remote MCP URL, then complete its Authenticate/Authorize card. Cursor's
documented registration includes all three callbacks below; explicitly configure
them together on the Gateway. The installed-app and localhost callbacks are
accepted only when listed, and do not enable other custom schemes or localhost
ports. Do not substitute Grok web's callback or create duplicate entries.

```text
cursor://anysphere.cursor-mcp/oauth/callback
https://www.cursor.com/agents/mcp/oauth/callback
http://localhost:8787/callback
```

Add `https://cursor.com` and `https://www.cursor.com` to `allowed_origins`.
Inspect the actual registration if a newer client sends different metadata.

The HTTPS callback at `www.cursor.com` redirects with HTTP 308 to
`https://cursor.com/agents/mcp/oauth/callback`. Add that exact canonical URL to
`redirect_uris` too, so the authorization page's `form-action` policy permits
both steps. `allowed_origins` alone does not configure form redirects. After
changing this policy, start a fresh authorization page; an already open page
retains its original policy.

## Standard HTTPS alongside an existing public port

Some hosted client environments can reach port 443 but cannot complete TLS on
8443. After routing both origins to the same Gateway and upgrading its binary,
keep the existing `public_url` and explicitly add the standard HTTPS origin:

```json
{
  "public_url": "https://octendra.example.com:8443",
  "public_url_aliases": ["https://octendra.example.com"]
}
```

Only configured HTTPS origins select the OAuth issuer, resource and endpoints.
The Gateway uses the request's Host, ignores forwarded host headers and keeps
tokens bound to their original resource. Existing port-8443 clients retain their
endpoint and authorization; use the port-443 MCP URL for the hosted client.
The aliases must serve the same Gateway and database, not a separate instance.

An “added” badge or HTTP 200 from `/oauth/token` alone does not establish a
working connection. Start a fresh client message, invoke `devices_list`, then
open an explicit device workspace and execute a harmless command. Check the
client's tool execution record and actual output.

Official guides: [Grok connectors](https://docs.x.ai/grok/connectors) and
[Grok Bot apps](https://docs.x.ai/grok-bot/computer-and-apps).
