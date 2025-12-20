import { Hono } from "@hono/hono";
import { bearerAuth } from "@hono/hono/bearer-auth";
import { logger } from "@hono/hono/logger";
import { MongoClient } from "@db/mongo";

const app = new Hono();

interface TokenDoc {
	token: string;
	input: number;
	output: number;
}

// Connect to MongoDB
const client = new MongoClient();
const mongo_uri = Deno.env.get("MONGODB_URI")!;
await client.connect(mongo_uri); // Make sure to connect before using

const db = client.database("ollama");
const collection = db.collection<TokenDoc>("tokens");

function removeTrailingSlash(str: string): string {
	// Check if the string ends with a slash and remove it if present
	return str.endsWith("/") ? str.slice(0, -1) : str;
}

const ollama_url_unedited = Deno.env.get("OLLAMA_URL")!;
const ollama_url = removeTrailingSlash(ollama_url_unedited);

app.use(logger());

app.use(
	"*",
	bearerAuth({
		verifyToken: async (token, _c) => {
			const tokenDoc = await collection.findOne({
				token,
			});

			if (!tokenDoc) {
				return false;
			}

			return true;
		},
	}),
	async (c) => {
		try {
			const response = await fetch(`${ollama_url}${c.req.path}`, {
				method: c.req.method,
				headers: c.req.header(),
				body: c.req.method === "GET" || c.req.method === "HEAD"
					? undefined
					: await c.req.arrayBuffer(),
			});

			// const jsonResponce = await response.json()

			// if (jsonResponce["prompt_eval_count"] && jsonResponce["eval_count"]) {
			// 	const token = c.req.header("Authorization")!.replace("Bearer ", "")

			// 	const tokenDoc = await db.collection<TokenDoc>("tokens").findOne({
			// 		token
			// 	});

			// }

			return response;
		} catch (error) {
			console.error("Error forwarding request:", error);
			return c.json({ message: "Internal Server Error" }, 500);
		}
	},
);

Deno.serve({
	port: 8000,
}, app.fetch);
