/**
 * Codex CLI <-> llama.cpp /v1/responses compatibility proxy
 *
 * Fixes ggml-org/llama.cpp#24295: llama.cpp's Responses shim keeps only tools whose
 * `type === "function"`, silently dropping Codex's `namespace` (MCP) and `web_search`
 * tool types. It also drops them *before the prompt is built*, so the model never sees
 * them and Codex silently gets plain text back.
 *
 * This proxy:
 *   1. Flattens `namespace` groups into top-level `function` tools (request in).
 *   2. Synthesises a Chat-Completions `web_search` function (request in).
 *   3. Restores the original {namespace, name} pair on `function_call` items (response
 *      out), because Codex resolves calls as ToolName::new(namespace, name) and rejects
 *      a bare flattened name with `unsupported call: <name>`.  <-- flattening alone fails
 *   4. Keeps `usage.output_tokens_details.reasoning_tokens` present (original behaviour).
 */

import { Context, Hono } from "@hono/hono";
import { bearerAuth } from "@hono/hono/bearer-auth";
import { logger } from "@hono/hono/logger";
import { MongoClient } from "mongodb";

const app = new Hono();

interface TokenDoc {
    token: string;
    input: number;
    output: number;
}

// ------------------------------------------------------------------ config ---
const mongo_uri = Deno.env.get("MONGODB_URI")!;
const client = new MongoClient(mongo_uri);
await client.connect();
const db = client.db("ollama");
const collection = db.collection<TokenDoc>("tokens");

const removeTrailingSlash = (s: string) => s.endsWith("/") ? s.slice(0, -1) : s;
const ollama_url = removeTrailingSlash(Deno.env.get("OLLAMA_URL")!);
const upstream_api_key = Deno.env.get("UPSTREAM_API_KEY") || "";
const testing = Boolean(Deno.env.get("TESTING"));

const responses_path_re = new RegExp(
    Deno.env.get("RESPONSES_PATH_RE") || "/v1/responses/?$",
);
const web_search_mode = (
    Deno.env.get("CODEX_WEB_SEARCH") || "stub"
).toLowerCase(); // stub | drop
const fill_tool_strict = (Deno.env.get("TOOL_STRICT") || "fill") !== "leave";
const compat_log = (Deno.env.get("TOOL_COMPAT_LOG") || "1") !== "0";

// --------------------------------------------------------- compat: helpers ---
type Json = Record<string, unknown>;
interface Alias {
    namespace: string;
    name: string;
}
interface Log {
    flattened: string[];
    stubbed: string[];
    restored: number;
    dropped: string[];
}

const isObj = (v: unknown): v is Json =>
    typeof v === "object" && v !== null && !Array.isArray(v);

/** Mirrors codex-rs join_tool_name(): trim the joint, join with "__". */
const joinToolName = (ns: string, name: string) =>
    ns ? `${ns.replace(/_+$/, "")}__${name.replace(/^_+/, "")}` : name;

// ADDED "function" to STRIP to prevent double-nesting bugs when cloning tool props
const STRIP = new Set(["type", "tools", "namespace", "defer_loading", "function"]);

/**
 * Returns the INNER function object only — emit() wraps it in {type:"function", function:…}.
 * (Returning the full wrapper here is the classic double-nest bug.)
 */
const webSearchStub = (): Json => ({
    name: "web_search",
    description: "Search the web",
    strict: true,
    parameters: {
        type: "object",
        properties: { query: { type: "string", description: "Search query" } },
        required: ["query"],
    },
});

// -------------------------------------------------- compat: request rewrite ---
function flattenTools(
    tools: unknown[],
    alias: Map<string, Alias>,
    log: Log,
): Json[] {
    const out: Json[] = [];
    const seen = new Set<string>();
    const emit = (wire: string, fn: Json, why: string) => {
        if (seen.has(wire)) {
            log.dropped.push(`${why} "${wire}" (name collision)`);
            return;
        }
        seen.add(wire);
        out.push({ type: "function", function: fn }); // wrapper lives here, once
    };

    const walk = (tool: unknown, ns: string | null, depth: number) => {
        if (!isObj(tool) || depth > 8) return;
        const type = typeof tool.type === "string" ? tool.type : "";

        // 1. namespace envelope -> recurse children with the prefix applied.
        if (type === "namespace") {
            const nsName = typeof tool.name === "string" && tool.name
                ? tool.name
                : (ns ?? "");
            const children = Array.isArray(tool.tools) ? tool.tools : [];
            for (const child of children) walk(child, nsName, depth + 1);
            log.flattened.push(
                `namespace "${nsName}" -> ${children.length} child tool(s)`,
            );
            return;
        }

        // 2. web_search -> synthesise a Chat-Completions equivalent (issue #24295).
        if (type === "web_search") {
            if (web_search_mode === "drop") {
                log.dropped.push("web_search (drop)");
                return;
            }
            if (!seen.has("web_search")) {
                emit("web_search", webSearchStub(), "web_search");
                log.stubbed.push("web_search");
            }
            return;
        }

        // 3. plain function tool (top-level, or a namespace child). Accepts both
        //    flat Responses shape and nested {type, function:{...}} shape.
        if (
            type === "function" ||
            (typeof tool.name === "string" &&
                (tool.parameters !== undefined || isObj(tool.function)))
        ) {
            const fn: Json = isObj(tool.function) ? { ...tool.function } : {};
            
            // "function" is now in STRIP, meaning it won't be copied back into fn
            for (const k of Object.keys(tool)) {
                if (!STRIP.has(k)) fn[k] = tool[k];
            }

            const raw = typeof fn.name === "string" ? fn.name : "";
            if (!raw) {
                log.dropped.push("function tool with no name");
                return;
            }

            const wire = ns ? joinToolName(ns, raw) : raw;
            fn.name = wire;
            if (fill_tool_strict && fn.strict === undefined) fn.strict = true; // matches upstream patch
            emit(wire, fn, "function");
            if (ns) alias.set(wire, { namespace: ns, name: raw });
            return;
        }

        // 4. everything else stays unsupported
        if (type) log.dropped.push(`type "${type}"`);
    };

    for (const tool of tools) walk(tool, null, 0);
    return out;
}

function reverseIndex(alias: Map<string, Alias>) {
    const byNsName = new Map<string, string>();
    const byName = new Map<string, string | null>();
    for (const [wire, e] of alias) {
        byNsName.set(`${e.namespace}\u0000${e.name}`, wire);
        byName.set(e.name, byName.has(e.name) ? null : wire);
    }
    return { byNsName, byName };
}

function rewriteInput(
    node: unknown,
    alias: Map<string, Alias>,
    depth = 0,
): void {
    if (depth > 12 || !alias.size) return;
    if (Array.isArray(node)) {
        for (const n of node) rewriteInput(n, alias, depth + 1);
        return;
    }
    if (!isObj(node)) return;

    if (node.type === "function_call" && typeof node.name === "string") {
        const { byNsName, byName } = reverseIndex(alias);
        const ns = typeof node.namespace === "string" &&
                node.namespace &&
                node.namespace !== "functions"
            ? node.namespace
            : null;
        const joined = ns ? joinToolName(ns, node.name) : node.name;
        const wire = ns
            ? (byNsName.get(`${ns}\u0000${node.name}`) ??
                (alias.has(joined) ? joined : undefined))
            : (byName.get(node.name) ?? undefined);
        if (wire) {
            node.name = wire;
            delete node.namespace;
        }
        return;
    }
    for (const k of Object.keys(node)) rewriteInput(node[k], alias, depth + 1);
}

function remapToolChoice(
    choice: unknown,
    alias: Map<string, Alias>,
    log: Log,
): unknown {
    if (!alias.size || choice == null) return choice;
    if (typeof choice === "string") {
        if (choice === "auto" || choice === "none" || choice === "required") {
            return choice;
        }
        const { byName } = reverseIndex(alias);
        const wire = byName.get(choice) ??
            (alias.has(choice) ? choice : undefined);
        if (wire) return wire;
        log.dropped.push(`tool_choice "${choice}" -> auto`);
        return "auto";
    }
    if (!isObj(choice)) return choice;
    const type = typeof choice.type === "string" ? choice.type : "";
    const inner = isObj(choice.function) ? choice.function : null;
    const raw = typeof choice.name === "string"
        ? choice.name
        : ((inner?.name as string | undefined) ?? "");
    if (type === "function" && raw) {
        const { byNsName, byName } = reverseIndex(alias);
        const ns = typeof choice.namespace === "string"
            ? choice.namespace
            : null;
        const wire = (ns ? byNsName.get(`${ns}\u0000${raw}`) : undefined) ??
            (alias.has(raw) ? raw : undefined) ??
            byName.get(raw) ??
            undefined;
        if (!wire) {
            log.dropped.push(`tool_choice function "${raw}" -> auto`);
            return "auto";
        }
        const out: Json = { ...choice, type: "function", name: wire };
        delete out.namespace;
        if (inner) out.function = { ...inner, name: wire };
        return out;
    }
    log.dropped.push(`tool_choice type "${type || "unknown"}" -> auto`);
    return "auto";
}

function compatRequest(body: Json): {
    body: Json;
    alias: Map<string, Alias>;
    log: Log;
} {
    const alias = new Map<string, Alias>();
    const log: Log = { flattened: [], stubbed: [], restored: 0, dropped: [] };
    const out: Json = { ...body };
    if (Array.isArray(body.tools) && body.tools.length) {
        const flat = flattenTools(body.tools, alias, log);
        if (flat.length) out.tools = flat;
        else delete out.tools;
    }
    if (alias.size) rewriteInput(body.input, alias);
    if (out.tool_choice !== undefined) {
        out.tool_choice = remapToolChoice(out.tool_choice, alias, log);
    }
    return { body: out, alias, log };
}

// ------------------------------------------------- compat: response restore ---
function restoreFunctionCallNames(
    node: unknown,
    alias: Map<string, Alias>,
    depth = 0,
): number {
    if (!alias.size || depth > 12) return 0;
    if (Array.isArray(node)) {
        let n = 0;
        for (const x of node) {
            n += restoreFunctionCallNames(x, alias, depth + 1);
        }
        return n;
    }
    if (!isObj(node)) return 0;

    let n = 0;
    const type = typeof node.type === "string" ? node.type : "";
    const callLike = type === "function_call" ||
        type.startsWith("response.function_call") ||
        "arguments" in node ||
        "call_id" in node ||
        "item_id" in node;

    if (callLike && typeof node.name === "string") {
        const hit = alias.get(node.name);
        if (hit) {
            node.name = hit.name;
            node.namespace = hit.namespace;
            n++;
        }
    }
    if (isObj(node.function) && typeof node.function.name === "string") {
        const hit = alias.get(node.function.name);
        if (hit) {
            node.function = { ...node.function, name: hit.name };
            node.namespace = hit.namespace;
            n++;
        }
    }
    for (const k of Object.keys(node)) {
        n += restoreFunctionCallNames(node[k], alias, depth + 1);
    }
    return n;
}

function patchUsageDeep(node: unknown, depth = 0): void {
    if (depth > 12) return;
    if (Array.isArray(node)) {
        for (const x of node) patchUsageDeep(x, depth + 1);
        return;
    }
    if (!isObj(node)) return;
    const looksLikeUsage =
        ("input_tokens" in node || "prompt_tokens" in node) &&
        ("output_tokens" in node || "completion_tokens" in node);
    if (looksLikeUsage && node.output_tokens_details == null) {
        const legacy = isObj(node.output_token_details)
            ? node.output_token_details
            : null;
        const r = legacy && typeof legacy.reasoning_tokens === "number"
            ? legacy.reasoning_tokens
            : 0;
        node.output_tokens_details = { reasoning_tokens: r };
    }
    for (const k of Object.keys(node)) patchUsageDeep(node[k], depth + 1);
}

// ---------------------------------------------------------------- middleware ---
app.use(logger());

const HOP_BY_HOP = new Set([
    "host",
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "proxy-authorization",
    "accept-encoding",
]);

app.use(
    "*",
    bearerAuth({
        verifyToken: async (token: string, _c: Context) => {
            if (testing && token === "testing") return true;
            return Boolean(await collection.findOne({ token }));
        },
    }),
    async (c: Context) => {
        try {
            const isGetLike = c.req.method === "GET" || c.req.method === "HEAD";

            // ---- 1. Body (read once) + Codex tool compatibility rewrite ----
            let upstreamBody: BodyInit | null | undefined;
            let alias = new Map<string, Alias>();
            let log: Log | null = null;

            if (isGetLike) {
                upstreamBody = undefined;
            } else if (responses_path_re.test(c.req.path)) {
                const text = await c.req.text();
                try {
                    const parsed = JSON.parse(text) as Json;
                    if (parsed && Array.isArray(parsed.tools)) {
                        const compat = compatRequest(parsed);
                        alias = compat.alias;
                        log = compat.log;
                        upstreamBody = JSON.stringify(compat.body);
                        if (
                            compat_log &&
                            (log.flattened.length || log.stubbed.length ||
                                log.dropped.length)
                        ) {
                            console.log(
                                `[tool-compat] flattened=[${
                                    log.flattened.join("; ")
                                }] stubbed=[${
                                    log.stubbed.join(",")
                                }] dropped=[${log.dropped.join(", ")}]`,
                            );
                        }
                    } else {
                        upstreamBody = text;
                    }
                } catch {
                    upstreamBody = text;
                }
            } else {
                upstreamBody = c.req.raw.body; 
            }

            // ---- 2. Rebuild request headers (no host/content-length leaks, no gzip) ----
            const reqHeaders = new Headers();
            for (const [k, v] of c.req.raw.headers) {
                if (!HOP_BY_HOP.has(k.toLowerCase())) reqHeaders.set(k, v);
            }
            
            // This applies to ALL requests, not just /v1/responses
            if (upstream_api_key) {
                reqHeaders.set("Authorization", `Bearer ${upstream_api_key}`);
            }
            reqHeaders.set("Accept-Encoding", "identity"); 

            const url = new URL(c.req.url);
            const ollama_response = await fetch(`${ollama_url}${c.req.path}${url.search}`, {
                method: c.req.method,
                headers: reqHeaders,
                body: upstreamBody,
            });

            const contentType = ollama_response.headers.get("content-type") ||
                "";
            const newHeaders = new Headers(ollama_response.headers);
            newHeaders.delete("content-length");
            newHeaders.delete("content-encoding");
            newHeaders.delete("transfer-encoding");

            const finalize = (
                body: BodyInit | null | undefined,
                status: number,
                headers: Headers,
            ) => {
                if (log?.restored && compat_log) {
                    console.log(
                        `[tool-compat] restored ${log.restored} function_call name(s)`,
                    );
                }
                return new Response(body ?? null, { status, headers });
            };

            // ---- 3A. Streaming (text/event-stream) ----
            if (contentType.includes("text/event-stream")) {
                const decoder = new TextDecoder(),
                    encoder = new TextEncoder();
                const localAlias = alias,
                    localLog = log;
                let buffer = "";

                const ts = new TransformStream({
                    transform(chunk, controller) {
                        buffer += decoder.decode(chunk, { stream: true });
                        const lines = buffer.split(/\r?\n/);
                        buffer = lines.pop() ?? ""; 
                        for (const line of lines) {
                            const isData = line.startsWith("data:") &&
                                !line.startsWith("data: [DONE]");
                            if (!isData) {
                                controller.enqueue(encoder.encode(line + "\n"));
                                continue;
                            }
                            const payload = line.startsWith("data: ")
                                ? line.slice(6)
                                : line.slice(5);
                            try {
                                const data = JSON.parse(payload);
                                patchUsageDeep(data);
                                if (localLog) {
                                    localLog.restored +=
                                        restoreFunctionCallNames(
                                            data,
                                            localAlias,
                                        );
                                }
                                controller.enqueue(
                                    encoder.encode(
                                        `data: ${JSON.stringify(data)}\n`,
                                    ),
                                );
                            } catch {
                                controller.enqueue(encoder.encode(line + "\n"));
                            }
                        }
                    },
                    flush(controller) {
                        if (buffer) controller.enqueue(encoder.encode(buffer));
                    },
                });

                return finalize(
                    ollama_response.body?.pipeThrough(ts),
                    ollama_response.status,
                    newHeaders,
                );
            }

            // ---- 3B. Buffered JSON ----
            if (contentType.includes("application/json")) {
                const data = await ollama_response.json();
                if (isObj(data)) {
                    patchUsageDeep(data);
                    if (log) {
                        log.restored += restoreFunctionCallNames(data, alias);
                    }
                }
                newHeaders.set(
                    "content-type",
                    "application/json; charset=utf-8",
                );
                return finalize(
                    JSON.stringify(data),
                    ollama_response.status,
                    newHeaders,
                );
            }

            // ---- 3C. Everything else (health, /models, images) ----
            return finalize(
                ollama_response.body,
                ollama_response.status,
                newHeaders,
            );
        } catch (error) {
            console.error("Error forwarding request:", error);
            return c.json({ message: "Internal Server Error" }, 500);
        }
    },
);

Deno.serve({ port: 8000 }, app.fetch);