/**
 * TEST-ONLY helpers for the config loader.
 *
 * The loader fetches `GET {api}/v1/config/{kind}` and validates the JSON.
 * `serveConfig` builds a fake `fetch` over a mutable payload map so a test
 * can drive the loader (and simulate the server publishing a new version)
 * without a real server. Exposed only on the `./testkit` subpath.
 */

type Payload = { version: number } & Record<string, unknown>;

function urlOf(input: RequestInfo | URL): string {
  if (typeof input === "string") return input;
  if (input instanceof URL) return input.href;
  return input.url;
}

/**
 * Build a fake `fetch` that serves `payloads[kind]` at `/v1/config/{kind}`
 * and 404s everything else. The map is captured by reference — mutate it
 * between calls to model a server-side publish.
 */
export function serveConfig(payloads: Record<string, Payload | undefined>): typeof fetch {
  return (async (input: RequestInfo | URL): Promise<Response> => {
    const m = urlOf(input).match(/\/v1\/config\/([a-z0-9_-]+)$/i);
    const payload = m?.[1] ? payloads[m[1]] : undefined;
    if (!payload) return new Response("not found", { status: 404 });
    return new Response(JSON.stringify(payload), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
  }) as typeof fetch;
}
