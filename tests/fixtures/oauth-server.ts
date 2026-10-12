// A fake OAuth-protected MCP server for the tests (#18): one Bun.serve that is both the resource server (the MCP
// fixture at /mcp, only with a valid Bearer token) and its authorization server (metadata, dynamic registration,
// an /authorize that redirects straight back with a code, a /token that checks PKCE (S256) and rotates refresh
// tokens, /revoke). Nothing leaves the machine. `requests` logs "METHOD /path" so tests can say what never happened.
import { WebStandardStreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/webStandardStreamableHttp.js";
import { createHash, randomBytes } from "node:crypto";
import { makeServer } from "./mcp-server.ts";

export interface OAuthServerOptions {
  /** Offer dynamic client registration (default true). */
  dcr?: boolean;
  /** Rotate refresh tokens on every refresh (default true). */
  rotate?: boolean;
  /** List a revocation endpoint (default true). */
  revocation?: boolean;
  /** A client registered by hand: its id and (optionally) secret and the redirect URI it was registered with. */
  preRegistered?: { clientId: string; secret?: string; redirectUri: string };
  /** Answer /mcp without asking for a token at all (a server that needs no login). */
  open?: boolean;
}

const random = () => randomBytes(12).toString("hex");

export function oauthServer(opts: OAuthServerOptions = {}) {
  const { dcr = true, rotate = true, revocation = true } = opts;
  const requests: string[] = [];
  const clients = new Map<string, { redirect_uris: string[]; secret?: string }>();
  if (opts.preRegistered) clients.set(opts.preRegistered.clientId, { redirect_uris: [opts.preRegistered.redirectUri], secret: opts.preRegistered.secret });
  const codes = new Map<string, { challenge: string; redirectUri: string; clientId: string; scope: string | null }>();
  const access = new Set<string>();
  const refresh = new Set<string>();
  const revoked: string[] = [];
  const scopes: (string | null)[] = [];
  const registrations: Record<string, unknown>[] = [];
  let refuseRefresh = false;

  const issue = () => {
    const tokens = { access_token: `at-${random()}`, refresh_token: `rt-${random()}`, token_type: "Bearer", expires_in: 3600 };
    access.add(tokens.access_token);
    refresh.add(tokens.refresh_token);
    return tokens;
  };
  const json = (body: unknown, status = 200) => Response.json(body, { status, headers: { "cache-control": "no-store" } });
  const oauthError = (error: string) => json({ error }, 400);

  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request): Promise<Response> {
      const url = new URL(request.url);
      requests.push(`${request.method} ${url.pathname}`);
      const base: string = `http://127.0.0.1:${server.port}`;
      switch (url.pathname) {
        case "/mcp": {
          const auth = request.headers.get("authorization");
          if (!opts.open && (!auth?.startsWith("Bearer ") || !access.has(auth.slice(7)))) {
            return new Response("unauthorized", {
              status: 401,
              headers: { "www-authenticate": `Bearer resource_metadata="${base}/.well-known/oauth-protected-resource/mcp"` },
            });
          }
          const transport = new WebStandardStreamableHTTPServerTransport({ sessionIdGenerator: undefined, enableJsonResponse: true });
          await makeServer().connect(transport);
          return transport.handleRequest(request);
        }
        case "/.well-known/oauth-protected-resource/mcp":
        case "/.well-known/oauth-protected-resource":
          if (opts.open) return new Response("not found", { status: 404 });
          return json({ resource: `${base}/mcp`, authorization_servers: [base], scopes_supported: ["mcp:read", "mcp:write"] });
        case "/.well-known/oauth-authorization-server":
          return json({
            issuer: base,
            authorization_endpoint: `${base}/authorize`,
            token_endpoint: `${base}/token`,
            ...(dcr ? { registration_endpoint: `${base}/register` } : {}),
            ...(revocation ? { revocation_endpoint: `${base}/revoke` } : {}),
            response_types_supported: ["code"],
            grant_types_supported: ["authorization_code", "refresh_token"],
            code_challenge_methods_supported: ["S256"],
            token_endpoint_auth_methods_supported: ["none", "client_secret_post"],
          });
        case "/register": {
          if (!dcr) return new Response("not found", { status: 404 });
          const body = (await request.json()) as { redirect_uris: string[] };
          registrations.push(body);
          const clientId = `client-${random()}`;
          clients.set(clientId, { redirect_uris: body.redirect_uris });
          return json({ ...body, client_id: clientId, client_id_issued_at: Math.floor(Date.now() / 1000) }, 201);
        }
        case "/authorize": {
          const q = url.searchParams;
          const client = clients.get(q.get("client_id") ?? "");
          const redirectUri = q.get("redirect_uri") ?? "";
          if (!client || !client.redirect_uris.includes(redirectUri)) return new Response("unknown client or redirect_uri", { status: 400 });
          scopes.push(q.get("scope"));
          const code = `code-${random()}`;
          codes.set(code, { challenge: q.get("code_challenge") ?? "", redirectUri, clientId: q.get("client_id")!, scope: q.get("scope") });
          const back = new URL(redirectUri);
          back.searchParams.set("code", code);
          back.searchParams.set("state", q.get("state") ?? "");
          return new Response(null, { status: 302, headers: { location: back.href } });
        }
        case "/token": {
          const form = new URLSearchParams(await request.text());
          const client = clients.get(form.get("client_id") ?? "");
          if (!client) return json({ error: "invalid_client" }, 401);
          if (client.secret && form.get("client_secret") !== client.secret) return json({ error: "invalid_client" }, 401);
          if (form.get("grant_type") === "authorization_code") {
            const grant = codes.get(form.get("code") ?? "");
            codes.delete(form.get("code") ?? "");
            const verifier = form.get("code_verifier") ?? "";
            const challenge = createHash("sha256").update(verifier).digest("base64url");
            if (!grant || grant.challenge !== challenge || grant.redirectUri !== form.get("redirect_uri")) return oauthError("invalid_grant");
            return json(issue());
          }
          if (form.get("grant_type") === "refresh_token") {
            const old = form.get("refresh_token") ?? "";
            if (refuseRefresh || !refresh.has(old)) return oauthError("invalid_grant");
            if (!rotate) {
              const tokens = issue();
              refresh.delete(tokens.refresh_token);
              return json({ ...tokens, refresh_token: old });
            }
            refresh.delete(old);
            return json(issue());
          }
          return oauthError("unsupported_grant_type");
        }
        case "/revoke": {
          if (!revocation) return new Response("not found", { status: 404 });
          const token = new URLSearchParams(await request.text()).get("token") ?? "";
          revoked.push(token);
          refresh.delete(token);
          access.delete(token);
          return new Response(null, { status: 200 });
        }
        default:
          return new Response("not found", { status: 404 });
      }
    },
  });

  return {
    url: `http://127.0.0.1:${server.port}/mcp`,
    base: `http://127.0.0.1:${server.port}`,
    requests,
    revoked,
    /** The scope each /authorize asked for. */
    scopes,
    registrations,
    /** Every access token stops working (as when it expires): the next call gets a 401 and refreshes. */
    expire: () => access.clear(),
    /** Refresh tokens are refused from now on (invalid_grant): the user has to log in again. */
    refuseRefresh: () => {
      refuseRefresh = true;
    },
    /** Paths requested, without the method. */
    paths: () => requests.map((r) => r.split(" ")[1]!),
    close: () => server.stop(true),
  };
}
