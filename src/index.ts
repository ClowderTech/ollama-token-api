import { Hono } from "@hono/hono";
import { bearerAuth } from "@hono/hono/bearer-auth";
import { logger } from "@hono/hono/logger";
import { MongoClient } from "mongodb";

const app = new Hono();

interface TokenDoc {
	token: string;
	input: number;
	output: number;
}

// Connect to MongoDB
const mongo_uri = Deno.env.get("MONGODB_URI")!;
const client = new MongoClient(mongo_uri);
await client.connect();

const db = client.db("ollama");
const collection = db.collection<TokenDoc>("tokens");

function removeTrailingSlash(str: string): string {
	return str.endsWith("/") ? str.slice(0, -1) : str;
}

const ollama_url_unedited = Deno.env.get("OLLAMA_URL")!;
const ollama_url = removeTrailingSlash(ollama_url_unedited);

const testing = Boolean(Deno.env.get("TESTING")) || false;

app.use(logger());

app.use(
	"*",
	bearerAuth({
		verifyToken: async (token, _c) => {
			const tokenDoc = await collection.findOne({
				token,
			});

			if (testing && token === "testing") {
				return true;
			}

			if (!tokenDoc) {
				return false;
			}

			return true;
		},
	}),
	async (c) => {
		try {
			// 1. Forward the initial request payload
			const ollama_response = await fetch(`${ollama_url}${c.req.path}`, {
				method: c.req.method,
				headers: c.req.header(),
				body: c.req.method === "GET" || c.req.method === "HEAD"
					? undefined
					: await c.req.arrayBuffer(),
			});

			const contentType = ollama_response.headers.get("content-type") || "";
			
			// 2. Clone headers and remove content-length since mutating JSON changes the byte size
			const newHeaders = new Headers(ollama_response.headers);
			newHeaders.delete("content-length");

			// ================================================================
			// BRANCH A: Handle Streaming Connections (text/event-stream)
			// ================================================================
			if (contentType.includes("text/event-stream")) {
				const decoder = new TextDecoder();
				const encoder = new TextEncoder();
				let buffer = "";

				const transformStream = new TransformStream({
					transform(chunk, controller) {
						buffer += decoder.decode(chunk, { stream: true });
						const lines = buffer.split("\n");
						
						// Keep partial line in buffer if chunk split mid-way
						buffer = lines.pop() || "";

						for (const line of lines) {
							// Filter for line data, ignoring the final text/stream close signal
							if (line.startsWith("data: ") && line !== "data: [DONE]") {
								try {
									const jsonStr = line.slice(6);
									const data = JSON.parse(jsonStr);

									// Safely catch the final chunk that holds the usage stats
									if (data && data.usage && !data.usage.output_token_details) {
										data.usage.output_token_details = { reasoning_tokens: 0 };
									}

									controller.enqueue(encoder.encode(`data: ${JSON.stringify(data)}\n`));
								} catch {
									// In case line parsing errors out, pass through original string cleanly
									controller.enqueue(encoder.encode(line + "\n"));
								}
							} else {
								controller.enqueue(encoder.encode(line + "\n"));
							}
						}
					},
					flush(controller) {
						if (buffer) {
							controller.enqueue(encoder.encode(buffer));
						}
					}
				});

				return new Response(ollama_response.body?.pipeThrough(transformStream), {
					status: ollama_response.status,
					headers: newHeaders,
				});
			}

			// ================================================================
			// BRANCH B: Handle Standard Block Responses (application/json)
			// ================================================================
			if (contentType.includes("application/json")) {
				const data = await ollama_response.json();

				if (data && data.usage && !data.usage.output_token_details) {
					data.usage.output_token_details = { reasoning_tokens: 0 };
				}

				return new Response(JSON.stringify(data), {
					status: ollama_response.status,
					headers: newHeaders,
				});
			}

			// Branch C: Static configurations, health checks, or images pass straight through
			return ollama_response;

		} catch (error) {
			console.error("Error forwarding request:", error);
			return c.json({ message: "Internal Server Error" }, 500);
		}
	},
);

Deno.serve({
	port: 8000,
}, app.fetch);