# nmbrs-adapter-http

The `http` adapter for [nmbrs](https://crates.io/crates/nmbrs). It runs each op
as an HTTP request, using [reqwest](https://crates.io/crates/reqwest). On each
cycle, bind points in the URL, body and headers are filled in. The response
becomes the op's result.

## Using it

This adapter is used through the `nmbrs` CLI. Select it with `adapter=http`.

### Adapter parameters

| Param | Default | Effect |
|-------|---------|--------|
| `base_url` (alias `host`) | none | Prefix for any op URL that does not start with `http://` or `https://`. |
| `timeout` | `30000` | Timeout for the whole request, in milliseconds. It applies to every request unless an op sets `request_timeout_ms`. |

Redirects are followed, up to 10.

### Op fields

This adapter accepts only the fields below. Any other op field is rejected when
the workload is initialized.

| Field | Default | Effect |
|-------|---------|--------|
| `uri` (alias `url`) | required | The request URL. Bind points (`{name}`) are filled in on each cycle. |
| `method` | `GET` | `GET`, `POST`, `PUT`, `DELETE`, `PATCH` or `HEAD`, in any letter case. Fixed per op. |
| `body` | none | The request body. Bind points are filled in on each cycle. |
| `content_type` | `application/json` | The `Content-Type` header, which is sent on every request. Fixed per op. |
| `headers` | none | Extra headers, one `Name: Value` per line. Bind points are filled in on each cycle. |
| `ok_status` | 2xx | The status codes that count as success, as codes and inclusive ranges: `"200-299,404"`. |
| `request_timeout_ms` | adapter `timeout` | Request timeout for this op, in milliseconds. |
| `connect_timeout` | OS default | Connect-phase timeout for this op, as a duration string such as `"15s"` or `"500ms"`. |
| `on_timeout` | none | `accept` turns a client-side request timeout into a success with no body, instead of an error. Other errors are not affected. |
| `expect_body` | `true` | Set `false` when an empty result is normal for this op. The log line for an accepted timeout then drops from Warn to Debug. |

### Results and errors

- If the response is a success and its `Content-Type` contains `json`, or its
  body starts with `{` or `[`, the body is parsed as JSON. Captures and
  `verify:` can then address fields inside it. Other bodies are kept as text.
- Any other status fails the op with an error named `HttpStatus<code>`, for
  example `HttpStatus404`. 5xx errors are marked retryable.
- Connection failures are named `ConnectionRefused` and timeouts are named
  `Timeout`. Both are retryable. `errors:` policies match on these names.

### Example

These op templates come from
[`openapi/petstore_ops.yaml`](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs/examples/workloads/openapi/petstore_ops.yaml),
which was generated from an OpenAPI spec by `openapi-ops` (see
[nmbrs-adapter-openapi](https://crates.io/crates/nmbrs-adapter-openapi)):

```yaml
params:
  base_url: "http://localhost:8080/v1"

op_templates:
  addPet:
    description: "Add a pet"
    abstract:
      needs:
        name: String
        owner_id: u64
    method: POST
    uri: "{base_url}/pets"
    content_type: "application/json"
    body: "{\"name\": \"{name}\", \"owner\": {\"id\": {owner_id}}}"

  getPetById:
    description: "Find a pet by id"
    abstract:
      needs:
        pet_id: u64
    method: GET
    uri: "{base_url}/pets/{pet_id}"
```

[`openapi/petstore_workload.yaml`](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs/examples/workloads/openapi/petstore_workload.yaml)
uses these templates and binds the values they need. Run it against a live
service with:

```bash
nmbrs run workload=crates/nmbrs/examples/workloads/openapi/petstore_workload.yaml adapter=http base_url=http://localhost:8080/v1
```

To print the requests without sending them, use `adapter=stdout`.

## Cargo features

None. The crate depends on `nmbrs-runtime` with `default-features = false`.

## Where it sits

- Implements `DriverAdapter` and `OpDispenser` from
  [nmbrs-runtime](https://crates.io/crates/nmbrs-runtime) and registers itself
  under the name `http` via `inventory`.
- Reads op templates from
  [nmbrs-workload](https://crates.io/crates/nmbrs-workload).
- For Rust callers, the crate exports `HttpAdapter` and `HttpConfig`
  (`base_url`, `timeout_ms`, `connect_timeout_ms`, `follow_redirects`).

## Links

- Repository: https://github.com/nosqlbench/nmbrs
- API docs: https://docs.rs/nmbrs-adapter-http
- nmbrs CLI: https://crates.io/crates/nmbrs

## License

Apache-2.0
