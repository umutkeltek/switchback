# Manage a running gateway from an agent

Start the MCP server with a running gateway's base URL. Give the MCP server process
an operator or admin key through the `SWITCHBACK_AI_KEY` environment variable.
Use an HTTPS URL for a remote gateway.

```json
{
  "mcpServers": {
    "switchback": {
      "command": "switchback",
      "args": ["mcp", "--gateway", "https://gateway.example"]
    }
  }
}
```

Configure your MCP client to pass the variable from its environment or secret
store. The gateway mode exposes the tools below and connects only to the URL
configured at startup. Running `switchback mcp` by itself serves the local config
tools.

## Change a setting and undo it

1. **Export.** Call `switchback_setup_export` and save its result. Find a concrete
   target with `switchback_models_list` or `switchback_engines_list`. Read its
   current revision and saved document with `switchback_settings_get`.
2. **Plan.** Copy the saved document for the target into a setup document and edit
   the relevant field. Include the observed revision in `if_match`. Send the
   document to `switchback_setup_plan` as `{"document": ...}`. Review every item
   before proceeding. The gateway leaves omitted items alone. It can apply valid
   items even when other items are refused.
3. **Apply.** Send the same document to `switchback_setup_apply` with
   `"confirm": true`. Save the returned `undo` call. An equal document returns
   `unchanged` and creates no revision.
4. **Check.** Read the settings or name again. For routing changes, call
   `switchback_route_preview` with the public name. The preview makes no inference
   request. Use `switchback_status` for gateway health.
5. **Undo.** Call the tool named in `undo.tool` with `undo.arguments`, then check
   the result and read current state again. The undo uses the revision written
   by the original change. If another writer has edited it, read the new state
   and plan again. `undo: null` means that call saved no change.

For example, this setup document sets the temperature for one target whose
current revision is 3 and whose saved document has no other fields. In a target
with other saved fields, copy them into the document first. Setup replaces each
included document in full.

```json
{
  "schema": "switchback/setup@1",
  "models": {
    "local-text/small": {"defaults": {"temperature": 0.4}}
  },
  "if_match": {
    "models": {"local-text/small": 3}
  }
}
```

For a single settings edit, `switchback_settings_set` accepts
`{"target":"local-text/small","revision":3,"patch":{"defaults":{"temperature":0.4}}}`.
The merge patch preserves fields you omit. A null value removes a field.
`switchback_settings_history` lists revisions and `switchback_settings_revert`
takes the target, current `revision`, and prior revision as `to`. Revision 0
clears overrides. A write's returned undo includes the revision guard.

Use `switchback_names_list` and `switchback_names_get` to inspect public names.
To update one, call `switchback_names_set` with `name`, `revision`, and ordered
`targets`. `switchback_names_revert` takes `name`, `revision`, and `to`. Revision
0 hands the name back to its configured route. The MCP setup tools accept a
document object and leave checks, immediate reloads, and wake off.

## Use HTTP directly

Send the same key as `Authorization: Bearer <key>` and use
`Content-Type: application/json` for JSON bodies. Keep the key in your client
environment or secret store. `GET /v1/setup/schema` serves the JSON Schema for
the setup document, also available as `switchback_setup_schema`. The
[example setup file](config/setup.example.yaml) shows the YAML form.

| Task | HTTP endpoint |
| --- | --- |
| Export setup | `GET /v1/setup` (add `?effective=true` to include names from config) |
| Plan and apply | `POST /v1/setup/plan` then `POST /v1/setup/apply` with the reviewed document |
| Read settings or history | `GET /v1/models/{target}/settings` or `GET /v1/models/{target}/settings/history` |
| Merge patch settings | `PATCH /v1/models/{target}/settings` with `If-Match` set to the current revision |
| Restore settings | `POST /v1/models/{target}/settings/revert` with `{"revision": N}` and the current revision in `If-Match` |
| Read names or history | `GET /v1/roles` or `GET /v1/roles/{name}` |
| Set or revert a name | `PUT /v1/roles/{name}` with `{"targets": [...]}`, or `POST /v1/roles/{name}/revert` with `{"to": N}`, both with the current revision in `If-Match` |
| List models or engines | `GET /v1/models` or `GET /v1/engines` |
| Preview routing | `POST /cp/v1/route-preview` with a chat request containing the model and one message |
| Check gateway health | `GET /v1/health` |

HTTP setup apply returns an `undo` document for each changed item. A settings
write returns an `undo` document for its target. Send either document to
`POST /v1/setup/apply` to restore that item. Undo can leave an empty stored
document where no override existed previously. The gateway then uses the
original effective settings or route. For HTTP name writes, save the pre-write
revision and use the name revert endpoint with the revision returned by the
write. Read `error.next` in a 4xx response for the suggested correction.
