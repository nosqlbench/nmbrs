# nmbrs-adapter-openapi

Turns an OpenAPI 3.x spec into op templates for
[nmbrs](https://crates.io/crates/nmbrs). This crate does not execute requests
and has no `adapter=` name. It generates http requests from a spec, and the
[`http` adapter](https://crates.io/crates/nmbrs-adapter-http) runs them. The
crate ships an `openapi-ops` binary and a small Rust library.

## `openapi-ops`

```bash
cargo install nmbrs-adapter-openapi
```

```text
openapi-ops <spec.yaml|json> [<out.yaml>] [base_url=<url>]
```

- Reads an OpenAPI 3.x spec in JSON or YAML.
- With no output file, writes the library to stdout. If `<out.yaml>` already
  exists, the command stops and does not overwrite it.
- `base_url=<url>` sets the default for the library's `base_url` param. If it
  is not given, the spec's first `servers` entry is used, or
  `http://localhost:8080` if the spec lists none.
- `-h` / `--help` prints usage. An unknown `key=` option is an error, as is a
  spec with no operations.

### What it generates

The output is a workload file with one `op_templates:` entry per operation, in
`operationId` order. On its own it runs nothing. A workload `extends:` it and
uses operations by name with `uses: <operationId>`.

For each `GET`, `POST`, `PUT`, `DELETE`, `PATCH` and `HEAD` operation, the
template contains:

- `description`, taken from the operation's summary (or its description).
- `abstract.needs`, listing the values the request must carry: path
  parameters, required query parameters, and required request-body fields.
  Spec types map to Polydat types: `integer` to `u64`, `number` to `f64`,
  `boolean` to `bool`, anything else to `String`.
- `method`, and `uri` as `{base_url}` + the path + the required query
  parameters, each written as a bind point.
- `content_type` and `body` when the operation has a request body. A JSON
  content type is chosen if one is listed. The body is a JSON template with
  the required fields, and dotted field names become nested objects. Numbers
  and booleans are inserted bare, and strings are quoted.
- `tags: { api_tag: ... }` holding the operation's tags.

Optional query parameters and body fields are not put in the template. They are
listed in a comment above it. If an operation has no `operationId`, one is
made from the method and path. Bind-point names are the spec names, with any
character that is not a letter, digit or `_` replaced by `_`.

Parameters written as `$ref` are skipped. A request-body schema that is a
`$ref` to `#/components/schemas/...` is resolved.

### Example

[`crates/nmbrs/examples/workloads/openapi/`](https://github.com/nosqlbench/nmbrs/tree/main/crates/nmbrs/examples/workloads/openapi)
has three files: a spec (`petstore.openapi.json`), the library generated from
it by `openapi-ops` (`petstore_ops.yaml`), and a workload built on that
library. A library like this one is produced with:

```bash
openapi-ops petstore.openapi.json petstore_ops.yaml
```

The spec's first server is `http://localhost:8080/v1`, so that becomes the
default `base_url`. One of the generated templates:

```yaml
  # GET /pets
  # optional, not templated: tag (String)
  listPets:
    description: "List pets, a page at a time"
    abstract:
      needs:
        limit: u64
    method: GET
    uri: "{base_url}/pets?limit={limit}"
    tags:
      api_tag: "pets"
```

The workload,
[`petstore_workload.yaml`](https://github.com/nosqlbench/nmbrs/blob/main/crates/nmbrs/examples/workloads/openapi/petstore_workload.yaml),
provides the values each operation needs:

```yaml
extends: ./petstore_ops.yaml

phases:
  seed:
    cycles: 2
    ops:
      add:
        uses: addPet
        bindings: |
          name := format_u64(add(mod(hash(cycle), 9000), 1000), 10)
          owner_id := add(mod(hash(cycle), 100), 1)
  read:
    cycles: 2
    ops:
      fetch:
        uses: getPetById
        bindings: |
          pet_id := mod(hash(cycle), 1000)
      page:
        uses: listPets
        bindings: |
          limit := 25
```

Run it with the `nmbrs` CLI. Use `adapter=http` to send the requests, or
`adapter=stdout` to print them. `base_url=` points it at another service:

```bash
nmbrs run workload=crates/nmbrs/examples/workloads/openapi/petstore_workload.yaml adapter=stdout base_url=http://pets.test:9000
```

## Library

- `parse_spec(source)` parses JSON or YAML into the `openapiv3::OpenAPI` document
  and a list of `ApiOperation` values (method, path, operation id, summary,
  path and query parameters, request body, tags).
- `render_op_library(api, ops, source_name, base_url)` returns the text that
  `openapi-ops` writes.
- `generate_ops(ops, base_url)` builds `ParsedOp`s directly. Each has `method`,
  a `uri` with every path and query parameter as a bind point, and for
  operations with a body, `content_type` and a `body` template. It also returns
  Polydat binding source that generates a value for every bind point.
- `describe_operations(ops)` prints a readable list of the operations to stdout.

## Cargo features

None.

## Where it sits

- Depends on [nmbrs-workload](https://crates.io/crates/nmbrs-workload) for the
  `ParsedOp` model, and on [openapiv3](https://crates.io/crates/openapiv3) for
  spec parsing. It does not depend on nmbrs-runtime and does not register an
  adapter.
- The templates it produces are run by
  [nmbrs-adapter-http](https://crates.io/crates/nmbrs-adapter-http).

## Links

- Repository: https://github.com/nosqlbench/nmbrs
- API docs: https://docs.rs/nmbrs-adapter-openapi
- nmbrs CLI: https://crates.io/crates/nmbrs

## License

Apache-2.0
