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
	// Check if the string ends with a slash and remove it if present
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
				return true
			}

			if (!tokenDoc) {
				return false;
			}

			return true;
		},
	}),
	async (c) => {
		try {
			const ollama_response = await fetch(`${ollama_url}${c.req.path}`, {
				method: c.req.method,
				headers: c.req.header(),
				body: c.req.method === "GET" || c.req.method === "HEAD"
					? undefined
					: await c.req.arrayBuffer(),
			});

            /*
			const json_responce = await ollama_response.json()

			if (c.req.path === "/v1/responses") {
				if (!json_responce["usage"]["output_tokens_details"]) {
					json_responce["usage"]["output_tokens_details"] = {"reasoning_tokens": 0}
				}
			}

			return c.json(json_responce);
            */

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
