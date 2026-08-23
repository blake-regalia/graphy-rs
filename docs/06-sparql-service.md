# SPARQL HTTP service

`graphy-server` is an Axum transport over the transport-independent parsing and serialization logic in `graphy-protocol`.

## Routes

| Route | Methods | Purpose |
| --- | --- | --- |
| `/sparql` | GET, POST | SPARQL Query and Update protocol |
| `/sparql/service` | GET | Service description |
| `/graphs` | GET, PUT, POST, DELETE | Graph Store Protocol operations |
| `/health` | GET | Liveness response |

Graph Store requests select the default graph with `?default` or a named graph with `?graph=<iri>`.

## Protocol behavior

Queries support URL parameters, URL-encoded POST bodies, and direct `application/sparql-query` bodies. Updates are POST-only and also accept direct `application/sparql-update` bodies. Dataset parameters, content negotiation, request deadlines, and snapshot ETags are handled by `graphy-protocol`.

SELECT and ASK results are available as SPARQL Results JSON, XML, CSV, or TSV. Graph results and Graph Store bodies currently use N-Triples or Turtle.

Each request pins a snapshot, and native CPU work runs outside the async reactor. Request bodies and complete result sets are currently buffered in memory; large-result streaming and admission control remain roadmap work.

## Configuration and security

`Config` controls read-only mode, outbound-network permission, and default/maximum deadlines. Outbound HTTP for SPARQL `LOAD` must be compiled with the `outbound-http` feature and explicitly enabled at runtime; it is unavailable on WebAssembly. The loader caps response size and accepts supported RDF media types.

The service does not provide authentication, authorization, CORS policy, rate limiting, operational metrics, or a generic `/load` endpoint. Deployments must supply those controls at a trusted boundary.
